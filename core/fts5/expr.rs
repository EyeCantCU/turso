//! FTS5 full-text query expressions: parsing (fts5parse.y, fts5_expr.c)
//! and evaluation over materialized doclists.

use super::config::{is_bareword, Detail, Fts5Config};
use super::index::{char_len, Index, Pending, Structure, MAIN_PREFIX};
use super::tokenize::{TOKENIZE_PREFIX, TOKENIZE_QUERY, TOKEN_COLOCATED};
use super::varint::*;
use crate::Result;

const MAX_TOKEN_SIZE: usize = 32768;
const DEFAULT_NEARDIST: i32 = 10;
const MAX_EXPR_DEPTH: usize = 256;

/// One query term and its colocated alternatives (synonyms).
#[derive(Debug, Clone)]
pub(crate) struct Term {
    /// Main term first, then synonyms.
    pub alts: Vec<Vec<u8>>,
    pub prefix: bool,
    pub first: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Phrase {
    pub terms: Vec<Term>,
}

#[derive(Debug, Clone)]
pub(crate) struct Nearset {
    pub near: i32,
    pub colset: Option<Vec<usize>>,
    /// Indexes into `Expr::phrases` (filled in once parsing is done).
    pub phrases: Vec<usize>,
    /// The phrases while parsing.
    parsed: Vec<Phrase>,
}

#[derive(Debug, Clone)]
pub(crate) enum Node {
    /// Matches nothing (an empty phrase or an empty column filter).
    Eof(Option<Nearset>),
    String(Nearset),
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>, Box<Node>),
}

impl Node {
    fn height(&self) -> usize {
        match self {
            Node::Eof(_) | Node::String(_) => 0,
            Node::And(c) | Node::Or(c) => 1 + c.iter().map(|n| n.height()).max().unwrap_or(0),
            Node::Not(a, b) => 1 + a.height().max(b.height()),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Expr {
    pub root: Node,
    pub phrases: Vec<Phrase>,
}

// ------------------------------------------------------------------ lexer

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Lp,
    Rp,
    Lcp,
    Rcp,
    Colon,
    Comma,
    Plus,
    Star,
    Minus,
    Caret,
    Eof,
    Str,
    And,
    Or,
    Not,
}

struct Lexer<'a> {
    z: &'a [u8],
    pos: usize,
    tok: Tok,
    start: usize,
    end: usize,
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

impl<'a> Lexer<'a> {
    fn text(&self) -> &'a [u8] {
        &self.z[self.start..self.end]
    }

    fn advance(&mut self) -> std::result::Result<(), String> {
        let z = self.z;
        let mut i = self.pos;
        while i < z.len() && is_space(z[i]) {
            i += 1;
        }
        self.start = i;
        let c = z.get(i).copied().unwrap_or(0);
        let mut n = 1;
        let tok = match c {
            b'(' => Tok::Lp,
            b')' => Tok::Rp,
            b'{' => Tok::Lcp,
            b'}' => Tok::Rcp,
            b':' => Tok::Colon,
            b',' => Tok::Comma,
            b'+' => Tok::Plus,
            b'*' => Tok::Star,
            b'-' => Tok::Minus,
            b'^' => Tok::Caret,
            0 => {
                n = 0;
                Tok::Eof
            }
            b'"' => {
                let mut j = i + 1;
                loop {
                    if j >= z.len() || z[j] == 0 {
                        return Err("unterminated string".into());
                    }
                    if z[j] == b'"' {
                        j += 1;
                        if z.get(j) != Some(&b'"') {
                            break;
                        }
                    }
                    j += 1;
                }
                n = j - i;
                Tok::Str
            }
            _ => {
                if !is_bareword(c) {
                    return Err(format!(
                        "fts5: syntax error near \"{}\"",
                        lossy(&z[i..i + 1])
                    ));
                }
                let mut j = i + 1;
                while j < z.len() && is_bareword(z[j]) {
                    j += 1;
                }
                n = j - i;
                match &z[i..j] {
                    b"OR" => Tok::Or,
                    b"NOT" => Tok::Not,
                    b"AND" => Tok::And,
                    _ => Tok::Str,
                }
            }
        };
        self.tok = tok;
        self.end = i + n;
        self.pos = self.end;
        Ok(())
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

// ------------------------------------------------------------------ parser

struct Parser<'a> {
    cfg: &'a Fts5Config,
    lex: Lexer<'a>,
    phrase_to_and: bool,
    locale: Option<&'a [u8]>,
}

type PResult<T> = std::result::Result<T, String>;

fn dequote_query(z: &[u8]) -> Vec<u8> {
    if z.first() == Some(&b'"') {
        super::config::dequote(z).0
    } else {
        z.to_vec()
    }
}

fn nearset(phrases: Vec<Phrase>) -> Nearset {
    Nearset {
        near: 0,
        colset: None,
        phrases: Vec::new(),
        parsed: phrases,
    }
}

impl<'a> Parser<'a> {
    fn syntax_error(&self) -> String {
        format!("fts5: syntax error near \"{}\"", lossy(self.lex.text()))
    }

    fn next(&mut self) -> PResult<()> {
        self.lex.advance()
    }

    fn expect(&mut self, t: Tok) -> PResult<()> {
        if self.lex.tok != t {
            return Err(self.syntax_error());
        }
        self.next()
    }

    /// sqlite3Fts5ParseTerm: tokenizes a string and appends it to `phrase`.
    fn parse_term(&mut self, phrase: &mut Phrase, text: &[u8], prefix: bool) -> PResult<()> {
        let z = dequote_query(text);
        let flags = TOKENIZE_QUERY | if prefix { TOKENIZE_PREFIX } else { 0 };
        let terms = &mut phrase.terms;
        let tokenizer = self.cfg.tokenizer()?;
        let rc = tokenizer.tokenize(flags, &z, self.locale, &mut |tflags, tok, _, _| {
            let tok = &tok[..tok.len().min(MAX_TOKEN_SIZE)];
            if !terms.is_empty() && (tflags & TOKEN_COLOCATED) != 0 {
                // Synonyms are kept newest first after the main term, as in SQLite.
                terms.last_mut().unwrap().alts.insert(1, tok.to_vec());
            } else {
                terms.push(Term {
                    alts: vec![tok.to_vec()],
                    prefix: false,
                    first: false,
                });
            }
            0
        });
        if rc != 0 {
            return Err(format!("tokenizer error {rc}"));
        }
        if let Some(t) = terms.last_mut() {
            t.prefix = prefix;
        }
        Ok(())
    }

    /// phrase ::= STRING star_opt (PLUS STRING star_opt)*
    fn parse_phrase(&mut self) -> PResult<Phrase> {
        let mut phrase = Phrase::default();
        if self.lex.tok != Tok::Str {
            return Err(self.syntax_error());
        }
        loop {
            let text = self.lex.text().to_vec();
            self.next()?;
            let star = self.lex.tok == Tok::Star;
            if star {
                self.next()?;
            }
            self.parse_term(&mut phrase, &text, star)?;
            if self.lex.tok != Tok::Plus {
                break;
            }
            self.next()?;
            if self.lex.tok != Tok::Str {
                return Err(self.syntax_error());
            }
        }
        Ok(phrase)
    }

    /// sqlite3Fts5ParseNearset: appends a phrase, dropping empty ones.
    fn near_add(near: &mut Vec<Phrase>, phrase: Phrase) {
        match near.last_mut() {
            None => near.push(phrase),
            Some(_) if phrase.terms.is_empty() => {}
            Some(last) if last.terms.is_empty() => *last = phrase,
            Some(_) => near.push(phrase),
        }
    }

    /// colset ::= [MINUS] STRING | [MINUS] LCP STRING+ RCP
    fn parse_colset(&mut self) -> PResult<Vec<usize>> {
        let invert = self.lex.tok == Tok::Minus;
        if invert {
            self.next()?;
        }
        let mut cols: Vec<usize> = Vec::new();
        if self.lex.tok == Tok::Lcp {
            self.next()?;
            if self.lex.tok != Tok::Str {
                return Err(self.syntax_error());
            }
            while self.lex.tok == Tok::Str {
                self.add_col(&mut cols)?;
                self.next()?;
            }
            self.expect(Tok::Rcp)?;
        } else if self.lex.tok == Tok::Str {
            self.add_col(&mut cols)?;
            self.next()?;
        } else {
            return Err(self.syntax_error());
        }
        if invert {
            let n = self.cfg.ncol();
            cols = (0..n).filter(|c| !cols.contains(c)).collect();
        }
        Ok(cols)
    }

    fn add_col(&mut self, cols: &mut Vec<usize>) -> PResult<()> {
        let name = lossy(&dequote_query(self.lex.text()));
        let Some(i) = self
            .cfg
            .cols
            .iter()
            .position(|c| c.eq_ignore_ascii_case(&name))
        else {
            return Err(format!("no such column: {name}"));
        };
        if let Err(pos) = cols.binary_search(&i) {
            cols.insert(pos, i);
        }
        Ok(())
    }

    /// sqlite3Fts5ParseNode for FTS5_STRING.
    fn make_string(&mut self, near: Nearset) -> PResult<Node> {
        if near.parsed.iter().any(|p| p.terms.is_empty()) {
            return Ok(Node::Eof(Some(near)));
        }
        if self.phrase_to_and && near.parsed[0].terms.len() > 1 {
            // "a+b+c" becomes "a AND b AND c" (fts5ParsePhraseToAnd).
            let terms = near.parsed.into_iter().next().unwrap().terms;
            let kids = terms
                .into_iter()
                .map(|t| {
                    Node::String(nearset(vec![Phrase {
                        terms: vec![Term {
                            alts: vec![t.alts[0].clone()],
                            prefix: false,
                            first: false,
                        }],
                    }]))
                })
                .collect();
            return Ok(Node::And(kids));
        }
        if self.cfg.detail != Detail::Full {
            let p = &near.parsed[0];
            if near.parsed.len() != 1
                || p.terms.len() > 1
                || (!p.terms.is_empty() && p.terms[0].first)
            {
                return Err(format!(
                    "fts5: {} queries are not supported (detail!=full)",
                    if near.parsed.len() == 1 {
                        "phrase"
                    } else {
                        "NEAR"
                    }
                ));
            }
        }
        Ok(Node::String(near))
    }

    /// nearset ::= phrase | CARET phrase | NEAR LP phrase+ [COMMA STRING] RP
    fn parse_nearset(&mut self) -> PResult<Nearset> {
        let mut near = Vec::new();
        if self.lex.tok == Tok::Caret {
            self.next()?;
            let mut p = self.parse_phrase()?;
            if let Some(t) = p.terms.first_mut() {
                t.first = true;
            }
            Self::near_add(&mut near, p);
            return Ok(nearset(near));
        }
        if self.lex.tok == Tok::Str {
            // STRING LP starts a NEAR group.
            let save = (self.lex.pos, self.lex.tok, self.lex.start, self.lex.end);
            let name = self.lex.text().to_vec();
            self.next()?;
            if self.lex.tok == Tok::Lp {
                self.next()?;
                let p = self.parse_phrase()?;
                Self::near_add(&mut near, p);
                while self.lex.tok == Tok::Str {
                    let p = self.parse_phrase()?;
                    Self::near_add(&mut near, p);
                }
                let mut dist = None;
                if self.lex.tok == Tok::Comma {
                    self.next()?;
                    if self.lex.tok != Tok::Str {
                        return Err(self.syntax_error());
                    }
                    dist = Some(self.lex.text().to_vec());
                    self.next()?;
                }
                if self.lex.tok != Tok::Rp {
                    return Err(self.syntax_error());
                }
                if name != b"NEAR" {
                    return Err(format!("fts5: syntax error near \"{}\"", lossy(&name)));
                }
                let mut n = nearset(near);
                n.near = match dist {
                    None => DEFAULT_NEARDIST,
                    Some(d) => {
                        let mut v: i32 = 0;
                        for &c in &d {
                            if !c.is_ascii_digit() {
                                return Err(format!("expected integer, got \"{}\"", lossy(&d)));
                            }
                            if v < 214748363 {
                                v = v * 10 + (c - b'0') as i32;
                            }
                        }
                        v
                    }
                };
                self.next()?;
                return Ok(n);
            }
            (self.lex.pos, self.lex.tok, self.lex.start, self.lex.end) = save;
        }
        let p = self.parse_phrase()?;
        Self::near_add(&mut near, p);
        Ok(nearset(near))
    }

    fn starts_colset_colon(&self) -> bool {
        match self.lex.tok {
            Tok::Minus | Tok::Lcp => true,
            Tok::Str => {
                let mut l = Lexer {
                    z: self.lex.z,
                    pos: self.lex.pos,
                    tok: self.lex.tok,
                    start: self.lex.start,
                    end: self.lex.end,
                };
                l.advance().is_ok() && l.tok == Tok::Colon
            }
            _ => false,
        }
    }

    /// cnearset ::= nearset | colset COLON nearset
    /// Also handles `colset COLON LP expr RP` (returned with `true`).
    fn parse_cnearset(&mut self) -> PResult<(Node, bool)> {
        if self.starts_colset_colon() {
            let cols = self.parse_colset()?;
            self.expect(Tok::Colon)?;
            if self.lex.tok == Tok::Lp {
                self.next()?;
                let mut e = self.parse_expr(0)?;
                self.expect(Tok::Rp)?;
                self.set_colset(&mut e, &cols)?;
                return Ok((e, true));
            }
            let near = self.parse_nearset()?;
            let mut node = self.make_string(near)?;
            self.set_colset(&mut node, &cols)?;
            return Ok((node, false));
        }
        let near = self.parse_nearset()?;
        Ok((self.make_string(near)?, false))
    }

    fn set_colset(&self, node: &mut Node, cols: &[usize]) -> PResult<()> {
        if self.cfg.detail == Detail::None {
            return Err("fts5: column queries are not supported (detail=none)".into());
        }
        apply_colset(node, cols);
        Ok(())
    }

    fn primary(&mut self) -> PResult<Node> {
        if self.lex.tok == Tok::Lp {
            self.next()?;
            let e = self.parse_expr(0)?;
            self.expect(Tok::Rp)?;
            return Ok(e);
        }
        let (first, is_expr) = self.parse_cnearset()?;
        if is_expr {
            return Ok(first);
        }
        // exprlist: juxtaposed cnearsets are an implicit AND.
        let mut left = first;
        while matches!(self.lex.tok, Tok::Str | Tok::Caret | Tok::Minus | Tok::Lcp) {
            let (right, is_expr) = self.parse_cnearset()?;
            if is_expr {
                return Err(self.syntax_error());
            }
            left = self.implicit_and(left, right)?;
        }
        Ok(left)
    }

    /// sqlite3Fts5ParseImplicitAnd: empty phrases vanish.
    fn implicit_and(&mut self, left: Node, right: Node) -> PResult<Node> {
        if matches!(right, Node::Eof(_)) {
            return Ok(left);
        }
        let mut left = left;
        match &mut left {
            Node::And(c) if matches!(c.last(), Some(Node::Eof(_))) => {
                *c.last_mut().unwrap() = right;
                return Ok(left);
            }
            Node::Eof(_) => return Ok(right),
            _ => {}
        }
        self.binary(BinOp::And, left, right)
    }

    fn binary(&self, op: BinOp, left: Node, right: Node) -> PResult<Node> {
        let node = match op {
            BinOp::Not => Node::Not(Box::new(left), Box::new(right)),
            BinOp::And | BinOp::Or => {
                let mut kids = Vec::new();
                for n in [left, right] {
                    match (op, n) {
                        (BinOp::And, Node::And(c)) | (BinOp::Or, Node::Or(c)) => kids.extend(c),
                        (_, n) => kids.push(n),
                    }
                }
                if op == BinOp::And {
                    Node::And(kids)
                } else {
                    Node::Or(kids)
                }
            }
        };
        if node.height() > MAX_EXPR_DEPTH {
            return Err(format!(
                "fts5 expression tree is too large (maximum depth {MAX_EXPR_DEPTH})"
            ));
        }
        Ok(node)
    }

    /// Precedence climbing: OR < AND < NOT, all left-associative.
    fn parse_expr(&mut self, min_prec: u8) -> PResult<Node> {
        let mut left = self.primary()?;
        loop {
            let (op, prec) = match self.lex.tok {
                Tok::Or => (BinOp::Or, 1),
                Tok::And => (BinOp::And, 2),
                Tok::Not => (BinOp::Not, 3),
                _ => break,
            };
            if prec < min_prec {
                break;
            }
            self.next()?;
            let right = self.parse_expr(prec + 1)?;
            left = self.binary(op, left, right)?;
        }
        Ok(left)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinOp {
    And,
    Or,
    Not,
}

/// fts5ParseSetColset: intersects or sets the column filter on all leaves.
fn apply_colset(node: &mut Node, cols: &[usize]) {
    match node {
        Node::String(n) => {
            let empty = match &mut n.colset {
                Some(c) => {
                    c.retain(|x| cols.contains(x));
                    c.is_empty()
                }
                None => {
                    n.colset = Some(cols.to_vec());
                    false
                }
            };
            if empty {
                let n = n.clone();
                *node = Node::Eof(Some(n));
            }
        }
        Node::Eof(Some(n)) => {
            match &mut n.colset {
                Some(c) => c.retain(|x| cols.contains(x)),
                None => n.colset = Some(cols.to_vec()),
            };
        }
        Node::Eof(None) => {}
        Node::And(c) | Node::Or(c) => c.iter_mut().for_each(|n| apply_colset(n, cols)),
        Node::Not(a, b) => {
            apply_colset(a, cols);
            apply_colset(b, cols);
        }
    }
}

/// Visits the phrase indexes of every leaf, in order.
fn for_each_leaf_mut(node: &mut Node, f: &mut dyn FnMut(&mut Nearset)) {
    match node {
        Node::String(n) | Node::Eof(Some(n)) => f(n),
        Node::Eof(None) => {}
        Node::And(c) | Node::Or(c) => c.iter_mut().for_each(|n| for_each_leaf_mut(n, f)),
        Node::Not(a, b) => {
            for_each_leaf_mut(a, f);
            for_each_leaf_mut(b, f);
        }
    }
}

impl Expr {
    /// sqlite3Fts5ExprNew. `icol` < ncol applies an implicit column filter.
    pub(crate) fn parse(
        cfg: &Fts5Config,
        phrase_to_and: bool,
        icol: usize,
        text: &[u8],
        locale: Option<&[u8]>,
    ) -> std::result::Result<Expr, String> {
        let mut p = Parser {
            cfg,
            lex: Lexer {
                z: text,
                pos: 0,
                tok: Tok::Eof,
                start: 0,
                end: 0,
            },
            phrase_to_and,
            locale,
        };
        p.next()?;
        let mut root = p.parse_expr(0)?;
        if p.lex.tok != Tok::Eof {
            return Err(p.syntax_error());
        }
        if icol < cfg.ncol() {
            p.set_colset(&mut root, &[icol])?;
        }
        // Phrases are numbered in the order they appear.
        let mut phrases = Vec::new();
        for_each_leaf_mut(&mut root, &mut |near| {
            for ph in near.parsed.drain(..) {
                near.phrases.push(phrases.len());
                phrases.push(ph);
            }
        });
        Ok(Expr { root, phrases })
    }

    /// sqlite3Fts5ExprAnd: `other`'s phrases come first.
    pub(crate) fn and(self, other: Expr) -> Expr {
        let n = other.phrases.len();
        let mut root1 = self.root;
        for_each_leaf_mut(&mut root1, &mut |near| {
            for p in near.phrases.iter_mut() {
                *p += n;
            }
        });
        let mut phrases = other.phrases;
        phrases.extend(self.phrases);
        let mut kids = Vec::new();
        for node in [root1, other.root] {
            match node {
                Node::And(c) => kids.extend(c),
                n => kids.push(n),
            }
        }
        Expr {
            root: Node::And(kids),
            phrases,
        }
    }

    /// An expression of phrase `i` alone, with its column filter
    /// (sqlite3Fts5ExprClonePhrase).
    pub(crate) fn single_phrase(&self, i: usize) -> Expr {
        let phrase = self.phrases[i].clone();
        let colset = self.phrase_nearset(i).and_then(|n| n.colset.clone());
        let near = Nearset {
            near: 0,
            colset,
            phrases: vec![0],
            parsed: Vec::new(),
        };
        let root = if phrase.terms.is_empty() {
            Node::Eof(Some(near))
        } else {
            Node::String(near)
        };
        Expr {
            root,
            phrases: vec![phrase],
        }
    }

    pub(crate) fn phrase_count(&self) -> usize {
        self.phrases.len()
    }

    pub(crate) fn phrase_size(&self, i: usize) -> usize {
        self.phrases.get(i).map_or(0, |p| p.terms.len())
    }

    /// The nearset holding phrase `i`.
    pub(crate) fn phrase_nearset(&self, i: usize) -> Option<&Nearset> {
        fn find(node: &Node, i: usize) -> Option<&Nearset> {
            match node {
                Node::String(n) | Node::Eof(Some(n)) => n.phrases.contains(&i).then_some(n),
                Node::Eof(None) => None,
                Node::And(c) | Node::Or(c) => c.iter().find_map(|n| find(n, i)),
                Node::Not(a, b) => find(a, i).or_else(|| find(b, i)),
            }
        }
        find(&self.root, i)
    }
}

/// Builds the MATCH expression for a LIKE/GLOB pattern on a trigram table
/// (sqlite3Fts5ExprPattern). `None` when the pattern has no 3-char run.
pub(crate) fn pattern_expr(
    cfg: &Fts5Config,
    glob: bool,
    icol: usize,
    text: &[u8],
) -> std::result::Result<Option<Expr>, String> {
    let spec: [u8; 3] = if glob {
        [b'*', b'?', b'[']
    } else {
        [b'_', b'%', 0]
    };
    let mut out = Vec::new();
    let n = text.len();
    let mut i = 0usize;
    let mut first = 0usize;
    while i <= n {
        let c = text.get(i).copied().unwrap_or(0);
        if i == n || c == spec[0] || c == spec[1] || (c == spec[2] && spec[2] != 0) {
            let chunk = &text[first.min(n)..i.min(n)];
            if chunk.iter().filter(|&&b| (b & 0xc0) != 0x80).count() >= 3 {
                out.push(b'"');
                for &b in chunk {
                    out.push(b);
                    if b == b'"' {
                        out.push(b'"');
                    }
                }
                out.push(b'"');
                out.push(b' ');
            }
            if i < n && c == spec[2] && spec[2] != 0 {
                i += 2;
                if text.get(i - 1) == Some(&b'^') {
                    i += 1;
                }
                while i < n && text[i] != b']' {
                    i += 1;
                }
            }
            first = i + 1;
        }
        i += 1;
    }
    if out.is_empty() {
        return Ok(None);
    }
    let mut icol = icol;
    let mut and = false;
    if cfg.detail != Detail::Full {
        and = true;
        if cfg.detail == Detail::None {
            icol = cfg.ncol();
        }
    }
    Expr::parse(cfg, and, icol, &out, None).map(Some)
}

// ------------------------------------------------------------------ evaluation
//
// A port of fts5_expr.c's node iterators (Next/Test per node type) over
// materialized term doclists, so that matching rows and the per-phrase
// position lists seen by auxiliary functions are exactly SQLite's.

/// Merges position lists, removing duplicates.
fn merge_poslists(lists: &[&[u8]]) -> Vec<u8> {
    let mut all: Vec<i64> = lists.iter().flat_map(|l| poslist_positions(l)).collect();
    all.sort_unstable();
    all.dedup();
    positions_to_poslist(&all)
}

/// Merges column lists (detail=columns), removing duplicates.
fn merge_collists(lists: &[&[u8]]) -> Vec<u8> {
    let mut all: Vec<i64> = Vec::new();
    for l in lists {
        let mut off = 0;
        let mut col = 0i64;
        while off < l.len() {
            col += get_varint32(l, &mut off) as i64 - 2;
            all.push(col);
        }
    }
    all.sort_unstable();
    all.dedup();
    let mut out = Vec::new();
    let mut prev = 0;
    for c in all {
        put_varint(&mut out, (c - prev + 2) as u64);
        prev = c;
    }
    out
}

/// Keeps only positions in the given columns.
fn filter_poslist(pl: &[u8], cols: &[usize]) -> Vec<u8> {
    let kept: Vec<i64> = poslist_positions(pl)
        .into_iter()
        .filter(|p| cols.contains(&(pos_col(*p) as usize)))
        .collect();
    positions_to_poslist(&kept)
}

fn filter_collist(cl: &[u8], cols: &[usize]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut off = 0;
    let mut col = 0i64;
    let mut prev = 0i64;
    while off < cl.len() {
        col += get_varint32(cl, &mut off) as i64 - 2;
        if cols.contains(&(col as usize)) {
            put_varint(&mut out, (col - prev + 2) as u64);
            prev = col;
        }
    }
    out
}

fn positions_to_poslist(pos: &[i64]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut w = PoslistWriter::default();
    for &p in pos {
        w.append(&mut out, p);
    }
    out
}

/// An index iterator over one term's doclist.
#[derive(Clone)]
struct TermIter {
    entries: std::sync::Arc<Vec<(i64, Vec<u8>)>>,
    idx: usize,
}

impl TermIter {
    fn eof(&self) -> bool {
        self.idx >= self.entries.len()
    }
    fn rowid(&self) -> i64 {
        self.entries.get(self.idx).map_or(0, |e| e.0)
    }
    fn data(&self) -> &[u8] {
        self.entries.get(self.idx).map_or(&[], |e| e.1.as_slice())
    }
    fn next(&mut self) {
        self.idx += 1;
    }
    /// sqlite3Fts5IterNextFrom (ascending): the first rowid >= `from`.
    fn next_from(&mut self, from: i64) {
        let rest = &self.entries[self.idx.min(self.entries.len())..];
        self.idx += rest.partition_point(|e| e.0 < from);
    }
}

#[derive(Clone)]
struct ETerm {
    /// The term's iterator, then one per synonym.
    iters: Vec<TermIter>,
    first: bool,
}

#[derive(Clone)]
struct EPhrase {
    terms: Vec<ETerm>,
    poslist: Vec<u8>,
    /// The node holding this phrase.
    node: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Eof,
    Term,
    String,
    And,
    Or,
    Not,
}

#[derive(Clone)]
struct ENode {
    kind: Kind,
    eof: bool,
    nomatch: bool,
    rowid: i64,
    children: Vec<usize>,
    phrases: Vec<usize>,
    near: i32,
    colset: bool,
}

/// The state of one query (Fts5Expr with its iterators).
#[derive(Clone)]
pub(crate) struct Evaluator {
    pub detail: Detail,
    nodes: Vec<ENode>,
    phrases: Vec<EPhrase>,
    root: usize,
    last: i64,
    /// Iterator positions (first iterator of each term) before the last
    /// next(); None until next() is first called.
    snapshot: Option<Vec<Vec<usize>>>,
}

fn rowid_cmp(a: i64, b: i64) -> std::cmp::Ordering {
    a.cmp(&b)
}

impl Evaluator {
    /// Builds the iterators for `expr` and positions on the first match in
    /// [first, last] (sqlite3Fts5ExprFirst).
    pub(crate) fn new(
        expr: &Expr,
        index: &Index,
        s: &Structure,
        pending: Option<&Pending>,
        rowid_range: (i64, i64),
    ) -> Result<Self> {
        let cfg = index.cfg;
        let detail = cfg.detail;
        let mut ev = Evaluator {
            detail,
            nodes: Vec::new(),
            phrases: Vec::new(),
            root: 0,
            last: rowid_range.1,
            snapshot: None,
        };
        // Phrases first, with their doclists (colset-filtered).
        for (pi, phrase) in expr.phrases.iter().enumerate() {
            let colset = expr.phrase_nearset(pi).and_then(|n| n.colset.clone());
            let mut terms = Vec::with_capacity(phrase.terms.len());
            for term in &phrase.terms {
                let mut iters = Vec::with_capacity(term.alts.len());
                for alt in &term.alts {
                    let q = query_part(cfg.tokendata, alt);
                    let mut dl = query_token(index, s, pending, q, term.prefix)?;
                    if let Some(cols) = &colset {
                        for (_, pl) in dl.iter_mut() {
                            *pl = match detail {
                                Detail::Full => filter_poslist(pl, cols),
                                Detail::Columns => filter_collist(pl, cols),
                                Detail::None => std::mem::take(pl),
                            };
                        }
                        // A merged prefix scan (no prefix index) skips rows
                        // whose filtered list is empty, as SQLite's
                        // SKIPEMPTY prefix iterator does.
                        if term.prefix && !has_prefix_index(index, alt) {
                            dl.retain(|(_, pl)| !pl.is_empty());
                        }
                    }
                    iters.push(TermIter {
                        entries: std::sync::Arc::new(dl),
                        idx: 0,
                    });
                }
                terms.push(ETerm {
                    iters,
                    first: term.first,
                });
            }
            ev.phrases.push(EPhrase {
                terms,
                poslist: Vec::new(),
                node: 0,
            });
        }
        ev.root = ev.build(&expr.root, expr);
        let root = ev.root;
        ev.node_first(root);
        if !ev.nodes[root].eof && rowid_cmp(ev.nodes[root].rowid, rowid_range.0).is_lt() {
            ev.node_next(root, Some(rowid_range.0));
        }
        while ev.nodes[root].nomatch {
            ev.node_next(root, None);
        }
        if rowid_cmp(ev.nodes[root].rowid, ev.last).is_gt() {
            ev.nodes[root].eof = true;
        }
        Ok(ev)
    }

    fn build(&mut self, node: &Node, expr: &Expr) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(ENode {
            kind: Kind::Eof,
            eof: false,
            nomatch: false,
            rowid: 0,
            children: Vec::new(),
            phrases: Vec::new(),
            near: 0,
            colset: false,
        });
        let (kind, children, phrases, near, colset) = match node {
            Node::Eof(n) => (
                Kind::Eof,
                Vec::new(),
                n.as_ref().map(|n| n.phrases.clone()).unwrap_or_default(),
                0,
                false,
            ),
            Node::String(n) => {
                let p = &expr.phrases[n.phrases[0]];
                let term = n.phrases.len() == 1
                    && p.terms.len() == 1
                    && p.terms[0].alts.len() == 1
                    && !p.terms[0].first;
                (
                    if term { Kind::Term } else { Kind::String },
                    Vec::new(),
                    n.phrases.clone(),
                    n.near,
                    n.colset.is_some(),
                )
            }
            Node::And(c) | Node::Or(c) => {
                let kids = c.iter().map(|n| self.build(n, expr)).collect();
                (
                    if matches!(node, Node::And(_)) {
                        Kind::And
                    } else {
                        Kind::Or
                    },
                    kids,
                    Vec::new(),
                    0,
                    false,
                )
            }
            Node::Not(a, b) => {
                let kids = vec![self.build(a, expr), self.build(b, expr)];
                (Kind::Not, kids, Vec::new(), 0, false)
            }
        };
        for &p in &phrases {
            if p < self.phrases.len() {
                self.phrases[p].node = idx;
            }
        }
        let n = &mut self.nodes[idx];
        n.kind = kind;
        n.children = children;
        n.phrases = phrases;
        n.near = near;
        n.colset = colset;
        idx
    }

    pub(crate) fn eof(&self) -> bool {
        self.nodes[self.root].eof
    }

    pub(crate) fn rowid(&self) -> i64 {
        self.nodes[self.root].rowid
    }

    /// sqlite3Fts5ExprNext
    pub(crate) fn next(&mut self) {
        self.snapshot = Some(
            self.phrases
                .iter()
                .map(|p| p.terms.iter().map(|t| t.iters[0].idx).collect())
                .collect(),
        );
        let root = self.root;
        loop {
            self.node_next(root, None);
            if !self.nodes[root].nomatch {
                break;
            }
        }
        if rowid_cmp(self.nodes[root].rowid, self.last).is_gt() {
            self.nodes[root].eof = true;
        }
    }

    /// Whether the main iterator of term `t` of phrase `p` moved during the
    /// last step (or the first positioning). SQLite's tokendata map only
    /// holds rows reached since the last xNext.
    pub(crate) fn term_arrived(&self, p: usize, t: usize) -> bool {
        let Some(snap) = &self.snapshot else {
            return true;
        };
        let cur = self
            .phrases
            .get(p)
            .and_then(|ph| ph.terms.get(t))
            .map(|term| term.iters[0].idx);
        cur != snap.get(p).and_then(|v| v.get(t)).copied()
    }

    /// sqlite3Fts5ExprPoslist: phrase `i`'s position list at the current row.
    pub(crate) fn phrase_poslist(&self, i: usize) -> Option<&[u8]> {
        let p = self.phrases.get(i)?;
        let node = &self.nodes[p.node];
        (!node.eof && node.rowid == self.nodes[self.root].rowid).then_some(p.poslist.as_slice())
    }

    /// The colset-filtered column list of phrase `i` (detail=columns).
    pub(crate) fn phrase_collist(&self, i: usize) -> Option<Vec<u8>> {
        let p = self.phrases.get(i)?;
        let node = &self.nodes[p.node];
        if node.eof || node.rowid != self.nodes[self.root].rowid || p.poslist.is_empty() {
            return None;
        }
        let term = p.terms.first()?;
        let row = node.rowid;
        let lists: Vec<&[u8]> = term
            .iters
            .iter()
            .filter(|it| !it.eof() && it.rowid() == row && !it.data().is_empty())
            .map(|it| it.data())
            .collect();
        Some(if lists.len() == 1 {
            lists[0].to_vec()
        } else {
            merge_collists(&lists)
        })
    }

    fn set_eof(&mut self, n: usize) {
        self.nodes[n].eof = true;
        self.nodes[n].nomatch = false;
        for c in self.nodes[n].children.clone() {
            self.set_eof(c);
        }
    }

    fn zero_poslist(&mut self, n: usize) {
        match self.nodes[n].kind {
            Kind::String | Kind::Term => {
                for p in self.nodes[n].phrases.clone() {
                    self.phrases[p].poslist.clear();
                }
            }
            _ => {
                for c in self.nodes[n].children.clone() {
                    self.zero_poslist(c);
                }
            }
        }
    }

    /// fts5NodeCompare: EOF nodes sort last.
    fn node_cmp(&self, a: usize, b: usize) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        if self.nodes[b].eof {
            return Less;
        }
        if self.nodes[a].eof {
            return Greater;
        }
        rowid_cmp(self.nodes[a].rowid, self.nodes[b].rowid)
    }

    /// fts5ExprNodeFirst
    fn node_first(&mut self, n: usize) {
        self.nodes[n].eof = false;
        self.nodes[n].nomatch = false;
        match self.nodes[n].kind {
            Kind::String | Kind::Term => self.near_init_all(n),
            Kind::Eof => {
                self.nodes[n].eof = true;
            }
            kind => {
                let kids = self.nodes[n].children.clone();
                let mut neof = 0;
                for &c in &kids {
                    self.node_first(c);
                    neof += self.nodes[c].eof as usize;
                }
                self.nodes[n].rowid = self.nodes[kids[0]].rowid;
                match kind {
                    Kind::And if neof > 0 => self.set_eof(n),
                    Kind::Or if neof == kids.len() => self.set_eof(n),
                    Kind::Not => self.nodes[n].eof = self.nodes[kids[0]].eof,
                    _ => {}
                }
            }
        }
        self.node_test(n);
    }

    /// fts5ExprNearInitAll: EOF if some term matches nothing.
    fn near_init_all(&mut self, n: usize) {
        for p in self.nodes[n].phrases.clone() {
            let ph = &mut self.phrases[p];
            if ph.terms.is_empty() {
                self.nodes[n].eof = true;
                return;
            }
            for t in ph.terms.iter_mut() {
                let mut hit = false;
                for it in t.iters.iter_mut() {
                    it.idx = 0;
                    if !it.eof() {
                        hit = true;
                    }
                }
                if !hit {
                    self.nodes[n].eof = true;
                    return;
                }
            }
        }
        self.nodes[n].eof = false;
    }

    fn node_test(&mut self, n: usize) {
        if self.nodes[n].eof {
            return;
        }
        match self.nodes[n].kind {
            Kind::String => self.test_string(n),
            Kind::Term => self.test_term(n),
            Kind::And => self.test_and(n),
            Kind::Or => self.test_or(n),
            Kind::Not => self.test_not(n),
            Kind::Eof => {}
        }
    }

    fn node_next(&mut self, n: usize, from: Option<i64>) {
        match self.nodes[n].kind {
            Kind::String => self.next_string(n, from),
            Kind::Term => self.next_term(n, from),
            Kind::And => {
                let c0 = self.nodes[n].children[0];
                self.node_next(c0, from);
                self.test_and(n);
            }
            Kind::Or => self.next_or(n, from),
            Kind::Not => {
                let c0 = self.nodes[n].children[0];
                self.node_next(c0, from);
                self.test_not(n);
            }
            Kind::Eof => {
                self.nodes[n].eof = true;
            }
        }
    }

    // ---- TERM

    fn test_term(&mut self, n: usize) {
        let p = self.nodes[n].phrases[0];
        let it = &self.phrases[p].terms[0].iters[0];
        let (rowid, data) = (it.rowid(), it.data().to_vec());
        let empty = data.is_empty() && self.detail != Detail::None;
        // detail=none iterators report one byte of "data" per entry.
        self.phrases[p].poslist = if self.detail == Detail::None {
            vec![1]
        } else {
            data
        };
        self.nodes[n].rowid = rowid;
        self.nodes[n].nomatch = empty;
    }

    fn next_term(&mut self, n: usize, from: Option<i64>) {
        let p = self.nodes[n].phrases[0];
        let it = &mut self.phrases[p].terms[0].iters[0];
        match from {
            Some(f) => it.next_from(f),
            None => it.next(),
        }
        if !it.eof() {
            self.test_term(n);
        } else {
            self.nodes[n].eof = true;
            self.nodes[n].nomatch = false;
        }
    }

    // ---- STRING

    /// fts5ExprSynonymRowid (ascending): the smallest rowid of any synonym.
    fn synonym_rowid(t: &ETerm) -> Option<i64> {
        t.iters.iter().filter(|i| !i.eof()).map(|i| i.rowid()).min()
    }

    fn test_string(&mut self, n: usize) {
        let phrases = self.nodes[n].phrases.clone();
        let first = &self.phrases[phrases[0]].terms[0];
        let mut last = if first.iters.len() > 1 {
            Self::synonym_rowid(first).unwrap_or(0)
        } else {
            first.iters[0].rowid()
        };
        loop {
            let mut matched = true;
            for &p in &phrases {
                for t in self.phrases[p].terms.iter_mut() {
                    if t.iters.len() > 1 {
                        if Self::synonym_rowid(t) == Some(last) {
                            continue;
                        }
                        matched = false;
                        // fts5ExprSynonymAdvanceto
                        for it in t.iters.iter_mut() {
                            if !it.eof() && last > it.rowid() {
                                it.next_from(last);
                            }
                        }
                        match Self::synonym_rowid(t) {
                            Some(r) => last = r,
                            None => {
                                self.nodes[n].nomatch = false;
                                self.nodes[n].eof = true;
                                return;
                            }
                        }
                    } else {
                        let it = &mut t.iters[0];
                        if it.rowid() == last && !it.eof() {
                            continue;
                        }
                        matched = false;
                        // fts5ExprAdvanceto
                        if last > it.rowid() {
                            it.next_from(last);
                            if it.eof() {
                                self.nodes[n].eof = true;
                                return;
                            }
                        }
                        last = it.rowid();
                    }
                }
            }
            if matched {
                break;
            }
        }
        self.nodes[n].rowid = last;
        self.nodes[n].nomatch = !self.near_test(n);
    }

    fn next_string(&mut self, n: usize, from: Option<i64>) {
        self.nodes[n].nomatch = false;
        let p0 = self.nodes[n].phrases[0];
        let t = &mut self.phrases[p0].terms[0];
        let eof;
        if t.iters.len() > 1 {
            let r = Self::synonym_rowid(t);
            let mut all_eof = true;
            for it in t.iters.iter_mut() {
                if !it.eof() {
                    let ii = it.rowid();
                    if Some(ii) == r || from.is_some_and(|f| ii != f && ii < f) {
                        match from {
                            Some(f) => it.next_from(f),
                            None => it.next(),
                        }
                        if !it.eof() {
                            all_eof = false;
                        }
                    } else {
                        all_eof = false;
                    }
                }
            }
            eof = all_eof;
        } else {
            let it = &mut t.iters[0];
            match from {
                Some(f) => it.next_from(f),
                None => it.next(),
            }
            eof = it.eof();
        }
        self.nodes[n].eof = eof;
        if !eof {
            self.test_string(n);
        }
    }

    /// The merged position list of a term at `row` (fts5ExprSynonymList).
    fn term_poslist(t: &ETerm, row: i64) -> Vec<u8> {
        if t.iters.len() == 1 {
            return t.iters[0].data().to_vec();
        }
        let lists: Vec<&[u8]> = t
            .iters
            .iter()
            .filter(|it| !it.eof() && it.rowid() == row && !it.data().is_empty())
            .map(|it| it.data())
            .collect();
        if lists.len() == 1 {
            lists[0].to_vec()
        } else {
            merge_poslists(&lists)
        }
    }

    /// fts5ExprNearTest
    fn near_test(&mut self, n: usize) -> bool {
        let row = self.nodes[n].rowid;
        let phrases = self.nodes[n].phrases.clone();
        if self.detail != Detail::Full {
            let p = phrases[0];
            let hit = self.phrases[p].terms[0].iters.iter().any(|it| {
                !it.eof()
                    && it.rowid() == row
                    && (self.detail == Detail::None || !it.data().is_empty())
            });
            self.phrases[p].poslist = if hit { vec![1] } else { Vec::new() };
            return hit;
        }
        let colset = self.nodes[n].colset;
        let mut i = 0;
        while i < phrases.len() {
            let p = phrases[i];
            let ph = &self.phrases[p];
            if ph.terms.len() > 1 || ph.terms[0].iters.len() > 1 || colset || ph.terms[0].first {
                let lists: Vec<Vec<i64>> = ph
                    .terms
                    .iter()
                    .map(|t| poslist_positions(&Self::term_poslist(t, row)))
                    .collect();
                let pos = phrase_match(&lists, ph.terms[0].first);
                self.phrases[p].poslist = positions_to_poslist(&pos);
                if pos.is_empty() {
                    break;
                }
            } else {
                self.phrases[p].poslist = ph.terms[0].iters[0].data().to_vec();
            }
            i += 1;
        }
        if i != phrases.len() {
            return false;
        }
        if phrases.len() == 1 {
            return true;
        }
        let mut lists: Vec<Vec<i64>> = phrases
            .iter()
            .map(|&p| poslist_positions(&self.phrases[p].poslist))
            .collect();
        let sizes: Vec<usize> = phrases
            .iter()
            .map(|&p| self.phrases[p].terms.len())
            .collect();
        let ok = near_match(&mut lists, &sizes, self.nodes[n].near);
        for (&p, l) in phrases.iter().zip(lists) {
            self.phrases[p].poslist = positions_to_poslist(&l);
        }
        ok
    }

    // ---- AND / OR / NOT

    fn test_and(&mut self, n: usize) {
        let kids = self.nodes[n].children.clone();
        let mut last = self.nodes[n].rowid;
        loop {
            self.nodes[n].nomatch = false;
            let mut matched = true;
            let mut eof = false;
            for &c in &kids {
                if rowid_cmp(last, self.nodes[c].rowid).is_gt() {
                    self.node_next(c, Some(last));
                }
                if self.nodes[c].eof {
                    self.set_eof(n);
                    eof = true;
                    break;
                } else if last != self.nodes[c].rowid {
                    matched = false;
                    last = self.nodes[c].rowid;
                }
                if self.nodes[c].nomatch {
                    self.nodes[n].nomatch = true;
                }
            }
            if eof || matched {
                break;
            }
        }
        if self.nodes[n].nomatch && n != self.root {
            self.zero_poslist(n);
        }
        self.nodes[n].rowid = last;
    }

    fn test_or(&mut self, n: usize) {
        let kids = self.nodes[n].children.clone();
        let mut next = kids[0];
        for &c in &kids[1..] {
            let cmp = self.node_cmp(next, c);
            if cmp.is_gt() || (cmp.is_eq() && !self.nodes[c].nomatch) {
                next = c;
            }
        }
        self.nodes[n].rowid = self.nodes[next].rowid;
        self.nodes[n].eof = self.nodes[next].eof;
        self.nodes[n].nomatch = self.nodes[next].nomatch;
    }

    fn next_or(&mut self, n: usize, from: Option<i64>) {
        let last = self.nodes[n].rowid;
        for c in self.nodes[n].children.clone() {
            if !self.nodes[c].eof {
                let r = self.nodes[c].rowid;
                if r == last || from.is_some_and(|f| rowid_cmp(r, f).is_lt()) {
                    self.node_next(c, from);
                }
            }
        }
        self.test_or(n);
    }

    fn test_not(&mut self, n: usize) {
        let (p1, p2) = (self.nodes[n].children[0], self.nodes[n].children[1]);
        while !self.nodes[p1].eof {
            let mut cmp = self.node_cmp(p1, p2);
            if cmp.is_gt() {
                let r = self.nodes[p1].rowid;
                self.node_next(p2, Some(r));
                cmp = self.node_cmp(p1, p2);
            }
            if !cmp.is_eq() || self.nodes[p2].nomatch {
                break;
            }
            self.node_next(p1, None);
        }
        self.nodes[n].eof = self.nodes[p1].eof;
        self.nodes[n].nomatch = self.nodes[p1].nomatch;
        self.nodes[n].rowid = self.nodes[p1].rowid;
        if self.nodes[p1].eof {
            self.zero_poslist(p2);
        }
    }
}

/// fts5ExprPhraseIsMatch: positions where the terms are adjacent.
fn phrase_match(lists: &[Vec<i64>], first: bool) -> Vec<i64> {
    let mut out = Vec::new();
    if lists.iter().any(|l| l.is_empty()) {
        return out;
    }
    let mut idx = vec![0usize; lists.len()];
    'outer: loop {
        let mut ipos = lists[0][idx[0]];
        loop {
            let mut matched = true;
            for (i, l) in lists.iter().enumerate() {
                let adj = ipos + i as i64;
                if l[idx[i]] != adj {
                    matched = false;
                    while l[idx[i]] < adj {
                        idx[i] += 1;
                        if idx[i] >= l.len() {
                            break 'outer;
                        }
                    }
                    if l[idx[i]] > adj {
                        ipos = l[idx[i]] - i as i64;
                    }
                }
            }
            if matched {
                break;
            }
        }
        if !first || pos_off(ipos) == 0 {
            out.push(ipos);
        }
        for (i, l) in lists.iter().enumerate() {
            idx[i] += 1;
            if idx[i] >= l.len() {
                break 'outer;
            }
        }
    }
    out
}

const LOOKAHEAD_EOF: i64 = 1 << 62;

/// fts5ExprNearIsMatch: trims each phrase's positions to those within
/// `near` tokens of the others. Returns false if nothing is left.
fn near_match(lists: &mut [Vec<i64>], sizes: &[usize], near: i32) -> bool {
    struct Reader<'a> {
        a: &'a [i64],
        i: usize,
        pos: i64,
        look: i64,
    }
    impl Reader<'_> {
        fn next(&mut self) -> bool {
            self.pos = self.look;
            self.look = self.a.get(self.i).copied().unwrap_or(LOOKAHEAD_EOF);
            self.i += 1;
            self.pos == LOOKAHEAD_EOF
        }
    }
    let inputs: Vec<Vec<i64>> = lists.to_vec();
    let mut readers: Vec<Reader> = inputs
        .iter()
        .map(|a| {
            let mut r = Reader {
                a,
                i: 0,
                pos: 0,
                look: 0,
            };
            r.next();
            r.next();
            r
        })
        .collect();
    let mut outs: Vec<Vec<i64>> = vec![Vec::new(); lists.len()];
    'outer: loop {
        let mut imax = readers[0].pos;
        loop {
            let mut matched = true;
            for (i, r) in readers.iter_mut().enumerate() {
                let imin = imax - sizes[i] as i64 - near as i64;
                if r.pos < imin || r.pos > imax {
                    matched = false;
                    while r.pos < imin {
                        if r.next() {
                            break 'outer;
                        }
                    }
                    if r.pos > imax {
                        imax = r.pos;
                    }
                }
            }
            if matched {
                break;
            }
        }
        for (i, r) in readers.iter().enumerate() {
            if outs[i].last().is_none_or(|&l| r.pos > l) {
                outs[i].push(r.pos);
            }
        }
        let mut iadv = 0;
        let mut imin = readers[0].look;
        for (i, r) in readers.iter().enumerate() {
            if r.look < imin {
                imin = r.look;
                iadv = i;
            }
        }
        if readers[iadv].next() {
            break;
        }
    }
    let ok = !outs[0].is_empty();
    for (l, o) in lists.iter_mut().zip(outs) {
        *l = o;
    }
    ok
}

/// The part of a query token used for lookups: with tokendata=1, the bytes
/// before the first NUL (fts5QueryTerm).
pub(crate) fn query_part(tokendata: bool, token: &[u8]) -> &[u8] {
    if tokendata {
        let n = token.iter().position(|&b| b == 0).unwrap_or(token.len());
        &token[..n]
    } else {
        token
    }
}

/// Whether index term `term` (without its index byte) is `q` itself or `q`
/// followed by token data (fts5IsTokendataPrefix).
pub(crate) fn is_tokendata_match(term: &[u8], q: &[u8]) -> bool {
    term.starts_with(q) && (term.len() == q.len() || term[q.len()] == 0)
}

/// The index terms a tokendata=1 query for `q` covers, with their doclists.
pub(crate) fn tokendata_terms(
    index: &Index,
    s: &Structure,
    pending: Option<&Pending>,
    q: &[u8],
) -> Result<Vec<super::index::TermDoclist>> {
    let mut key = vec![MAIN_PREFIX];
    key.extend_from_slice(q);
    let mut terms = index.scan_prefix(s, pending, &key)?;
    terms.retain(|(t, _)| is_tokendata_match(&t[1..], q));
    Ok(terms)
}

/// A tokendata=1 query: the union of the doclists of `q` and every `q\0...`
/// term. Rows hit by several terms get their lists merged keeping duplicate
/// positions, as fts5IterSetOutputsTokendata does.
fn tokendata_doclist(
    index: &Index,
    s: &Structure,
    pending: Option<&Pending>,
    q: &[u8],
) -> Result<Vec<(i64, Vec<u8>)>> {
    let terms = tokendata_terms(index, s, pending, q)?;
    let detail = index.cfg.detail;
    let mut merged: std::collections::BTreeMap<i64, Vec<Vec<u8>>> = Default::default();
    for (_, dl) in terms {
        for (rowid, pl) in dl {
            merged.entry(rowid).or_default().push(pl);
        }
    }
    Ok(merged
        .into_iter()
        .map(|(rowid, mut lists)| {
            let pl = if lists.len() == 1 || detail == Detail::None {
                lists.swap_remove(0)
            } else {
                let mut all: Vec<i64> = Vec::new();
                for l in &lists {
                    if detail == Detail::Full {
                        all.extend(poslist_positions(l));
                    } else {
                        let (mut off, mut col) = (0, 0i64);
                        while off < l.len() {
                            col += get_varint32(l, &mut off) as i64 - 2;
                            all.push(col);
                        }
                    }
                }
                all.sort();
                positions_to_poslist(&all)
            };
            (rowid, pl)
        })
        .collect())
}

/// Whether a prefix query for `token` is served by a prefix index.
fn has_prefix_index(index: &Index, token: &[u8]) -> bool {
    let nchar = char_len(token);
    index.cfg.prefixes.contains(&nchar)
}

/// The doclist of a query token (prefix-aware), using a prefix index when
/// one has the right length.
fn query_token(
    index: &Index,
    s: &Structure,
    pending: Option<&Pending>,
    token: &[u8],
    prefix: bool,
) -> Result<Vec<(i64, Vec<u8>)>> {
    let cfg = index.cfg;
    if !prefix && cfg.tokendata {
        return tokendata_doclist(index, s, pending, token);
    }
    if !prefix {
        let mut key = vec![MAIN_PREFIX];
        key.extend_from_slice(token);
        return index.query_term(s, pending, &key);
    }
    let nchar = char_len(token);
    if let Some(i) = cfg.prefixes.iter().position(|&n| n == nchar) {
        let mut key = vec![MAIN_PREFIX + 1 + i as u8];
        key.extend_from_slice(token);
        return index.query_term(s, pending, &key);
    }
    let mut key = vec![MAIN_PREFIX];
    key.extend_from_slice(token);
    let terms = index.scan_prefix(s, pending, &key)?;
    let mut merged: std::collections::BTreeMap<i64, Vec<Vec<u8>>> = Default::default();
    for (_, dl) in terms {
        for (rowid, pl) in dl {
            merged.entry(rowid).or_default().push(pl);
        }
    }
    let detail = cfg.detail;
    Ok(merged
        .into_iter()
        .map(|(rowid, lists)| {
            let refs: Vec<&[u8]> = lists.iter().map(|v| v.as_slice()).collect();
            let pl = match detail {
                Detail::Full => merge_poslists(&refs),
                Detail::Columns => merge_collists(&refs),
                Detail::None => Vec::new(),
            };
            (rowid, pl)
        })
        .collect())
}
