//! MATCH expression parser (`fts3_expr.c`), with the enhanced query
//! syntax (`SQLITE_ENABLE_FTS3_PARENTHESIS`) always on.

use super::sql_error;
use super::tokenizer::Tokenizer;
use crate::Result;

pub(crate) const NEAR: u8 = 1;
pub(crate) const NOT: u8 = 2;
pub(crate) const AND: u8 = 3;
pub(crate) const OR: u8 = 4;
pub(crate) const PHRASE: u8 = 5;

const DEFAULT_NEAR: i32 = 10;
const MAX_EXPR_DEPTH: i32 = 12;
const SQLITE_MAX_EXPR_DEPTH: i32 = 1000;

#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub z: Vec<u8>,
    pub is_prefix: bool,
    pub first: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Phrase {
    pub tokens: Vec<Token>,
    /// Column to match, or the column count for any column.
    pub icol: i32,
}

#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub etype: u8,
    pub nnear: i32,
    pub parent: Option<usize>,
    pub left: Option<usize>,
    pub right: Option<usize>,
    pub phrase: Option<Phrase>,
}

/// A parsed expression; nodes live in an arena and link by index.
#[derive(Debug, Clone, Default)]
pub(crate) struct Expr {
    pub nodes: Vec<Node>,
    pub root: Option<usize>,
}

#[derive(Debug)]
enum PErr {
    Error,
    TooBig,
    Other(crate::LimboError),
}

impl From<crate::LimboError> for PErr {
    fn from(e: crate::LimboError) -> Self {
        PErr::Other(e)
    }
}

type PResult<T> = std::result::Result<T, PErr>;

enum Step {
    Node(Option<usize>, usize),
    Done(usize),
}

struct Parser<'a> {
    tok: &'a dyn Tokenizer,
    langid: i32,
    cols: &'a [String],
    fts4: bool,
    default_col: i32,
    nest: i32,
    e: Expr,
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn at(z: &[u8], i: usize) -> u8 {
    z.get(i).copied().unwrap_or(0)
}

/// `sqlite3Fts3ReadInt`: digits as an int; `None` on overflow.
pub(crate) fn read_int(z: &[u8]) -> (usize, Option<i32>) {
    let mut v: u64 = 0;
    let mut i = 0;
    while i < z.len() && z[i].is_ascii_digit() {
        v = v * 10 + (z[i] - b'0') as u64;
        if v > 0x7fffffff {
            return (i, None);
        }
        i += 1;
    }
    (i, Some(v as i32))
}

fn find_barred(z: &[u8]) -> Option<usize> {
    z.iter().position(|&c| c == b'"' || c == b'(' || c == b')')
}

impl Parser<'_> {
    fn new_node(&mut self, etype: u8) -> usize {
        self.e.nodes.push(Node {
            etype,
            nnear: 0,
            parent: None,
            left: None,
            right: None,
            phrase: None,
        });
        self.e.nodes.len() - 1
    }

    fn n(&self, i: usize) -> &Node {
        &self.e.nodes[i]
    }

    fn nm(&mut self, i: usize) -> &mut Node {
        &mut self.e.nodes[i]
    }

    /// First token of `z` as a one-token phrase; consumed byte count.
    /// `None` when `z` is empty, which ends the parse as in SQLite.
    fn next_token(&mut self, icol: i32, z: &[u8]) -> PResult<Option<(Option<usize>, usize)>> {
        let mut first: Option<(Vec<u8>, usize, usize)> = None;
        self.tok.tokenize(self.langid, z, &mut |t, s, e, _| {
            first = Some((t.to_vec(), s as usize, e as usize));
            Ok(false)
        })?;
        match first {
            Some((token, istart, mut iend)) => {
                if let Some(b) = find_barred(&z[..iend.min(z.len())]) {
                    return self.next_token(icol, &z[..b]);
                }
                let mut tk = Token {
                    z: token,
                    is_prefix: false,
                    first: false,
                };
                if iend < z.len() && z[iend] == b'*' {
                    tk.is_prefix = true;
                    iend += 1;
                }
                let mut s = istart;
                while self.fts4 && s > 0 && z[s - 1] == b'^' {
                    tk.first = true;
                    s -= 1;
                }
                let idx = self.new_node(PHRASE);
                self.nm(idx).phrase = Some(Phrase {
                    tokens: vec![tk],
                    icol,
                });
                Ok(Some((Some(idx), iend)))
            }
            None if z.is_empty() => Ok(None),
            None => Ok(Some((None, find_barred(z).unwrap_or(z.len())))),
        }
    }

    /// A quoted phrase.
    fn next_string(&mut self, z: &[u8]) -> PResult<usize> {
        let mut tokens = Vec::new();
        self.tok.tokenize(self.langid, z, &mut |t, s, e, _| {
            let (s, e) = (s as usize, e as usize);
            tokens.push(Token {
                z: t.to_vec(),
                is_prefix: e < z.len() && z[e] == b'*',
                first: s > 0 && z[s - 1] == b'^',
            });
            Ok(true)
        })?;
        let idx = self.new_node(PHRASE);
        let icol = self.default_col;
        self.nm(idx).phrase = Some(Phrase { tokens, icol });
        Ok(idx)
    }

    fn next_node(&mut self, z: &[u8]) -> PResult<Step> {
        let mut off = 0;
        while off < z.len() && is_space(z[off]) {
            off += 1;
        }
        if off == z.len() {
            return Ok(Step::Done(z.len()));
        }
        let input = &z[off..];
        const KEYWORDS: [(&[u8], u8); 4] =
            [(b"OR", OR), (b"AND", AND), (b"NOT", NOT), (b"NEAR", NEAR)];
        for (kw, etype) in KEYWORDS {
            if input.starts_with(kw) {
                let mut nnear = DEFAULT_NEAR;
                let mut nkey = kw.len();
                if etype == NEAR && at(input, 4) == b'/' && at(input, 5).is_ascii_digit() {
                    // On overflow SQLite's ReadInt returns -1, leaving "NEAR/".
                    if let (n, Some(v)) = read_int(&input[5..]) {
                        nkey += 1 + n;
                        nnear = v.min(1_000_000_000);
                    }
                }
                let c = at(input, nkey);
                if is_space(c) || c == b'"' || c == b'(' || c == b')' || c == 0 {
                    let idx = self.new_node(etype);
                    self.nm(idx).nnear = nnear;
                    return Ok(Step::Node(Some(idx), off + nkey));
                }
            }
        }
        if input[0] == b'"' {
            let mut ii = 1;
            while ii < input.len() && input[ii] != b'"' {
                ii += 1;
            }
            if ii == input.len() {
                return Err(PErr::Error);
            }
            let idx = self.next_string(&input[1..ii])?;
            return Ok(Step::Node(Some(idx), off + ii + 1));
        }
        if input[0] == b'(' {
            self.nest += 1;
            if self.nest > SQLITE_MAX_EXPR_DEPTH {
                return Err(PErr::Error);
            }
            let (e, consumed) = self.parse(&input[1..])?;
            return Ok(Step::Node(e, off + 1 + consumed));
        } else if input[0] == b')' {
            self.nest -= 1;
            return Ok(Step::Done(off + 1));
        }
        let mut icol = self.default_col;
        let mut col_len = 0;
        for (ii, name) in self.cols.iter().enumerate() {
            let s = name.as_bytes();
            if input.len() > s.len()
                && input[s.len()] == b':'
                && input[..s.len()].eq_ignore_ascii_case(s)
            {
                icol = ii as i32;
                col_len = off + s.len() + 1;
                break;
            }
        }
        match self.next_token(icol, &z[col_len..])? {
            Some((e, consumed)) => Ok(Step::Node(e, consumed + col_len)),
            None => Ok(Step::Done(col_len)),
        }
    }

    fn precedence(&self, i: usize) -> u8 {
        self.n(i).etype
    }

    fn insert_binary(&mut self, head: &mut Option<usize>, prev: usize, new: usize) {
        let mut split = prev;
        while let Some(p) = self.n(split).parent {
            if self.precedence(p) <= self.precedence(new) {
                split = p;
            } else {
                break;
            }
        }
        if let Some(p) = self.n(split).parent {
            self.nm(p).right = Some(new);
            self.nm(new).parent = Some(p);
        } else {
            *head = Some(new);
        }
        self.nm(new).left = Some(split);
        self.nm(split).parent = Some(new);
    }

    /// `fts3ExprParse`: returns the expression and the bytes consumed.
    fn parse(&mut self, z: &[u8]) -> PResult<(Option<usize>, usize)> {
        let mut ret: Option<usize> = None;
        let mut prev: Option<usize> = None;
        let mut off = 0;
        let mut require_phrase = true;
        loop {
            match self.next_node(&z[off..])? {
                Step::Done(n) => {
                    off += n;
                    break;
                }
                Step::Node(p, n) => {
                    if let Some(p) = p {
                        let etype = self.n(p).etype;
                        let is_phrase = etype == PHRASE || self.n(p).left.is_some();
                        if !is_phrase && require_phrase {
                            return Err(PErr::Error);
                        }
                        if is_phrase && !require_phrase {
                            let and = self.new_node(AND);
                            let pv = prev.expect("previous node");
                            self.insert_binary(&mut ret, pv, and);
                            prev = Some(and);
                        }
                        if let Some(pv) = prev {
                            let pe = self.n(pv).etype;
                            if (etype == NEAR && !is_phrase && pe != PHRASE)
                                || (etype != PHRASE && is_phrase && pe == NEAR)
                            {
                                return Err(PErr::Error);
                            }
                        }
                        if is_phrase {
                            if ret.is_some() {
                                let pv = prev.expect("previous node");
                                self.nm(pv).right = Some(p);
                                self.nm(p).parent = Some(pv);
                            } else {
                                ret = Some(p);
                            }
                        } else {
                            let pv = prev.expect("previous node");
                            self.insert_binary(&mut ret, pv, p);
                        }
                        require_phrase = !is_phrase;
                        prev = Some(p);
                    }
                    off += n;
                }
            }
        }
        if ret.is_some() && require_phrase {
            return Err(PErr::Error);
        }
        Ok((ret, off))
    }

    fn check_depth(&self, p: Option<usize>, max: i32) -> PResult<()> {
        if let Some(p) = p {
            if max < 0 {
                return Err(PErr::TooBig);
            }
            self.check_depth(self.n(p).left, max - 1)?;
            self.check_depth(self.n(p).right, max - 1)?;
        }
        Ok(())
    }

    /// `fts3ExprBalance`: rebuilds AND and OR chains as balanced trees.
    fn balance(&mut self, root: usize, max: i32) -> PResult<usize> {
        if max == 0 {
            return Err(PErr::Error);
        }
        let etype = self.n(root).etype;
        if etype == AND || etype == OR {
            let mut leaves: Vec<Option<usize>> = vec![None; max as usize];
            let mut free: Option<usize> = None;
            let mut p = root;
            while self.n(p).etype == etype {
                p = self.n(p).left.expect("operator has left child");
            }
            loop {
                let parent = self.n(p).parent;
                self.nm(p).parent = None;
                if let Some(pp) = parent {
                    self.nm(pp).left = None;
                }
                let mut q = Some(self.balance(p, max - 1)?);
                for lvl in 0..max as usize {
                    let Some(qv) = q else { break };
                    match leaves[lvl] {
                        None => {
                            leaves[lvl] = Some(qv);
                            q = None;
                        }
                        Some(leaf) => {
                            let f = free.expect("free node");
                            self.nm(f).left = Some(leaf);
                            self.nm(f).right = Some(qv);
                            self.nm(leaf).parent = Some(f);
                            self.nm(qv).parent = Some(f);
                            free = self.n(f).parent;
                            self.nm(f).parent = None;
                            leaves[lvl] = None;
                            q = Some(f);
                        }
                    }
                }
                if q.is_some() {
                    return Err(PErr::TooBig);
                }
                let Some(pp) = parent else { break };
                let mut np = self.n(pp).right.expect("operator has right child");
                while self.n(np).etype == etype {
                    np = self.n(np).left.expect("operator has left child");
                }
                p = np;
                let pr = self.n(pp).right.expect("operator has right child");
                let gp = self.n(pp).parent;
                self.nm(pr).parent = gp;
                if let Some(gp) = gp {
                    self.nm(gp).left = Some(pr);
                }
                self.nm(pp).parent = free;
                free = Some(pp);
            }
            let mut q: Option<usize> = None;
            for leaf in leaves.into_iter().flatten() {
                match q {
                    None => {
                        self.nm(leaf).parent = None;
                        q = Some(leaf);
                    }
                    Some(qv) => {
                        let f = free.expect("free node");
                        self.nm(f).right = Some(qv);
                        self.nm(f).left = Some(leaf);
                        self.nm(leaf).parent = Some(f);
                        self.nm(qv).parent = Some(f);
                        free = self.n(f).parent;
                        self.nm(f).parent = None;
                        q = Some(f);
                    }
                }
            }
            Ok(q.expect("balanced tree"))
        } else if etype == NOT {
            let l = self.n(root).left.expect("NOT has left child");
            let r = self.n(root).right.expect("NOT has right child");
            self.nm(l).parent = None;
            self.nm(r).parent = None;
            let l = self.balance(l, max - 1)?;
            let r = self.balance(r, max - 1)?;
            self.nm(root).left = Some(l);
            self.nm(l).parent = Some(root);
            self.nm(root).right = Some(r);
            self.nm(r).parent = Some(root);
            Ok(root)
        } else {
            Ok(root)
        }
    }
}

/// `sqlite3Fts3ExprParse`. `None` query text gives an empty expression.
pub(crate) fn parse(
    tok: &dyn Tokenizer,
    langid: i32,
    cols: &[String],
    fts4: bool,
    default_col: i32,
    query: Option<&[u8]>,
) -> Result<Expr> {
    let Some(z) = query else {
        return Ok(Expr::default());
    };
    let mut p = Parser {
        tok,
        langid,
        cols,
        fts4,
        default_col,
        nest: 0,
        e: Expr::default(),
    };
    let result = (|| -> PResult<Option<usize>> {
        let (root, _) = p.parse(z)?;
        if p.nest != 0 {
            return Err(PErr::Error);
        }
        let Some(root) = root else { return Ok(None) };
        let root = p.balance(root, MAX_EXPR_DEPTH)?;
        p.check_depth(Some(root), MAX_EXPR_DEPTH)?;
        Ok(Some(root))
    })();
    match result {
        Ok(root) => {
            let mut e = p.e;
            e.root = root;
            Ok(e)
        }
        Err(PErr::TooBig) => Err(sql_error(format!(
            "FTS expression tree is too large (maximum depth {MAX_EXPR_DEPTH})"
        ))),
        Err(PErr::Error) => Err(sql_error(format!(
            "malformed MATCH expression: [{}]",
            String::from_utf8_lossy(z)
        ))),
        Err(PErr::Other(e)) => Err(e),
    }
}

impl Expr {
    /// Phrase nodes in matchinfo order: left to right, skipping the right
    /// side of each NOT (`sqlite3Fts3ExprIterate`).
    pub(crate) fn phrases(&self) -> Vec<usize> {
        fn walk(e: &Expr, i: usize, out: &mut Vec<usize>) {
            let n = &e.nodes[i];
            if n.etype == PHRASE {
                out.push(i);
            } else {
                walk(e, n.left.expect("left child"), out);
                if n.etype != NOT {
                    walk(e, n.right.expect("right child"), out);
                }
            }
        }
        let mut out = Vec::new();
        if let Some(r) = self.root {
            walk(self, r, &mut out);
        }
        out
    }

    /// Every phrase node, including those on the right of a NOT.
    pub(crate) fn all_phrases(&self) -> Vec<usize> {
        fn walk(e: &Expr, i: usize, out: &mut Vec<usize>) {
            let n = &e.nodes[i];
            if n.etype == PHRASE {
                out.push(i);
            } else {
                walk(e, n.left.expect("left child"), out);
                walk(e, n.right.expect("right child"), out);
            }
        }
        let mut out = Vec::new();
        if let Some(r) = self.root {
            walk(self, r, &mut out);
        }
        out
    }
}
