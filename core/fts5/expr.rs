//! FTS5 full-text query expressions: parsing (fts5parse.y, fts5_expr.c)
//! and evaluation over materialized doclists.

use super::config::{is_bareword, Detail, Fts5Config};
use super::index::{char_len, Index, Pending, Structure, MAIN_PREFIX};
use super::tokenize::{TOKENIZE_PREFIX, TOKENIZE_QUERY, TOKEN_COLOCATED};
use super::varint::*;
use crate::Result;
use std::collections::HashMap;

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
        let rc = self
            .cfg
            .tokenizer
            .tokenize(flags, &z, self.locale, &mut |tflags, tok, _, _| {
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

/// A term's doclist: rowid -> position list (or column list).
type Doclist = HashMap<i64, Vec<u8>>;

/// Evaluates an expression against the index.
pub(crate) struct Evaluator {
    pub detail: Detail,
    /// Per phrase, per term: the doclist (synonyms and prefixes merged).
    terms: Vec<Vec<Doclist>>,
    /// Matching rowids, ascending.
    pub rows: Vec<i64>,
}

/// Merges position lists, removing duplicates.
fn merge_poslists(lists: &[&[u8]]) -> Vec<u8> {
    let mut all: Vec<i64> = lists.iter().flat_map(|l| poslist_positions(l)).collect();
    all.sort_unstable();
    all.dedup();
    let mut out = Vec::new();
    let mut w = PoslistWriter::default();
    for p in all {
        w.append(&mut out, p);
    }
    out
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
    let mut out = Vec::new();
    let mut w = PoslistWriter::default();
    for p in poslist_positions(pl) {
        if cols.contains(&(pos_col(p) as usize)) {
            w.append(&mut out, p);
        }
    }
    out
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
            if out.last() != Some(&ipos) {
                out.push(ipos);
            }
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
    let inputs: Vec<Vec<i64>> = lists.iter().map(|l| l.clone()).collect();
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

fn positions_to_poslist(pos: &[i64]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut w = PoslistWriter::default();
    for &p in pos {
        w.append(&mut out, p);
    }
    out
}

impl Evaluator {
    /// Loads every term's doclist and computes the matching rows.
    pub(crate) fn new(
        expr: &Expr,
        index: &Index,
        s: &Structure,
        pending: Option<&Pending>,
        rowid_range: (i64, i64),
    ) -> Result<Self> {
        let cfg = index.cfg;
        let detail = cfg.detail;
        let mut terms = Vec::with_capacity(expr.phrases.len());
        for (pi, phrase) in expr.phrases.iter().enumerate() {
            let colset = expr.phrase_nearset(pi).and_then(|n| n.colset.clone());
            let mut tl = Vec::with_capacity(phrase.terms.len());
            for term in &phrase.terms {
                let mut merged: HashMap<i64, Vec<Vec<u8>>> = HashMap::new();
                for alt in &term.alts {
                    for (rowid, pl) in query_token(index, s, pending, alt, term.prefix)? {
                        if rowid < rowid_range.0 || rowid > rowid_range.1 {
                            continue;
                        }
                        merged.entry(rowid).or_default().push(pl);
                    }
                }
                let mut dl = Doclist::with_capacity(merged.len());
                for (rowid, lists) in merged {
                    let refs: Vec<&[u8]> = lists.iter().map(|v| v.as_slice()).collect();
                    let mut pl = match detail {
                        Detail::Full if refs.len() > 1 => merge_poslists(&refs),
                        Detail::Columns if refs.len() > 1 => merge_collists(&refs),
                        _ => lists.into_iter().next().unwrap(),
                    };
                    if let Some(cols) = &colset {
                        pl = match detail {
                            Detail::Full => filter_poslist(&pl, cols),
                            Detail::Columns => filter_collist(&pl, cols),
                            Detail::None => pl,
                        };
                    }
                    dl.insert(rowid, pl);
                }
                tl.push(dl);
            }
            terms.push(tl);
        }
        let mut ev = Evaluator {
            detail,
            terms,
            rows: Vec::new(),
        };
        let mut candidates = ev.candidates(&expr.root);
        candidates.sort_unstable();
        candidates.dedup();
        let mut scratch = vec![None; expr.phrases.len()];
        ev.rows = candidates
            .into_iter()
            .filter(|&r| ev.eval(&expr.root, r, &mut scratch, expr))
            .collect();
        Ok(ev)
    }

    /// Rows that could match: every term of a leaf present.
    fn candidates(&self, node: &Node) -> Vec<i64> {
        match node {
            Node::Eof(_) => Vec::new(),
            Node::String(n) => {
                let mut acc: Option<Vec<i64>> = None;
                for &p in &n.phrases {
                    for dl in &self.terms[p] {
                        let keys: Vec<i64> = match &acc {
                            None => dl.keys().copied().collect(),
                            Some(a) => a.iter().copied().filter(|r| dl.contains_key(r)).collect(),
                        };
                        acc = Some(keys);
                    }
                }
                acc.unwrap_or_default()
            }
            Node::And(c) => {
                let mut acc: Option<Vec<i64>> = None;
                for n in c {
                    let mut k = self.candidates(n);
                    if let Some(a) = &acc {
                        let set: std::collections::HashSet<i64> = a.iter().copied().collect();
                        k.retain(|r| set.contains(r));
                    }
                    acc = Some(k);
                }
                acc.unwrap_or_default()
            }
            Node::Or(c) => c.iter().flat_map(|n| self.candidates(n)).collect(),
            Node::Not(a, _) => self.candidates(a),
        }
    }

    /// Tests `node` at `row`; on a match fills the phrase poslists of the
    /// leaves that contribute (others are left as None).
    fn eval(&self, node: &Node, row: i64, out: &mut [Option<Vec<u8>>], expr: &Expr) -> bool {
        match node {
            Node::Eof(_) => false,
            Node::String(n) => self.eval_string(n, row, out, expr),
            Node::And(c) => {
                let mut tmp = out.to_vec();
                for n in c {
                    if !self.eval(n, row, &mut tmp, expr) {
                        return false;
                    }
                }
                out.clone_from_slice(&tmp);
                true
            }
            Node::Or(c) => {
                let mut any = false;
                for n in c {
                    let mut tmp = out.to_vec();
                    if self.eval(n, row, &mut tmp, expr) {
                        out.clone_from_slice(&tmp);
                        any = true;
                    }
                }
                any
            }
            Node::Not(a, b) => {
                let mut tmp = out.to_vec();
                if !self.eval(a, row, &mut tmp, expr) {
                    return false;
                }
                let mut scratch = out.to_vec();
                if self.eval(b, row, &mut scratch, expr) {
                    return false;
                }
                out.clone_from_slice(&tmp);
                true
            }
        }
    }

    fn eval_string(&self, n: &Nearset, row: i64, out: &mut [Option<Vec<u8>>], expr: &Expr) -> bool {
        if self.detail != Detail::Full {
            let p = n.phrases[0];
            let Some(pl) = self.terms[p].first().and_then(|dl| dl.get(&row)) else {
                return false;
            };
            if self.detail == Detail::Columns && pl.is_empty() {
                return false;
            }
            out[p] = Some(pl.clone());
            return true;
        }
        let mut lists: Vec<Vec<i64>> = Vec::with_capacity(n.phrases.len());
        for &p in &n.phrases {
            let phrase = &expr.phrases[p];
            let mut tl = Vec::with_capacity(phrase.terms.len());
            for dl in &self.terms[p] {
                let Some(pl) = dl.get(&row) else {
                    return false;
                };
                tl.push(poslist_positions(pl));
            }
            let first = phrase.terms.first().is_some_and(|t| t.first);
            let pos = if tl.len() == 1 && !first {
                std::mem::take(&mut tl[0])
            } else {
                phrase_match(&tl, first)
            };
            if pos.is_empty() {
                return false;
            }
            lists.push(pos);
        }
        if lists.len() > 1 {
            let sizes: Vec<usize> = n
                .phrases
                .iter()
                .map(|&p| expr.phrases[p].terms.len())
                .collect();
            if !near_match(&mut lists, &sizes, n.near) {
                return false;
            }
        }
        for (&p, l) in n.phrases.iter().zip(lists) {
            out[p] = Some(positions_to_poslist(&l));
        }
        true
    }

    /// The per-phrase position lists (or column lists) of a matching row;
    /// None for phrases that do not contribute to the match.
    pub(crate) fn row_poslists(&self, expr: &Expr, row: i64) -> Vec<Option<Vec<u8>>> {
        let mut out = vec![None; expr.phrases.len()];
        self.eval(&expr.root, row, &mut out, expr);
        out
    }
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
