//! Full-text query evaluation (the `fts3Eval*` functions in `fts3.c`).
//!
//! Doclists are loaded whole, but the iteration state SQLite keeps per
//! expression node (current docid, EOF, position lists trimmed by NEAR,
//! incremental versus loaded phrases) is reproduced, because offsets(),
//! snippet() and matchinfo() observe it.

use super::expr::{Expr, AND, NEAR, NOT, OR, PHRASE};
use super::segment::{
    decode_doclist, decode_poslist, seg_reader_cursor, MultiSegReader, SegFilter, COLUMN_FILTER,
    FIRST, IGNORE_EMPTY, PREFIX, REQUIRE_POS,
};
use super::table::Ctx;
use crate::Result;
use std::collections::BTreeMap;

/// A position list: (column, positions), columns and positions ascending.
pub(crate) type Pl = Vec<(i32, Vec<i64>)>;
type Doclist = Vec<(i64, Pl)>;

#[derive(Default)]
pub(crate) struct PhraseState {
    pub ntoken: usize,
    pub icol: i32,
    /// The phrase doclist as read from the index. SQLite reads a one-token
    /// incremental phrase straight out of the segment buffers, so NEAR
    /// trims of such a phrase are written back here and seen on reload.
    orig: Doclist,
    pub incr: bool,
    /// `aAll` of a loaded phrase, in storage order; trims write through.
    all: Vec<(i64, Pl)>,
    next: Option<isize>,
    pub docid: i64,
    pub list: Option<Pl>,
    list_idx: Option<usize>,
    or_iter: Option<isize>,
    or_docid: i64,
}

#[derive(Default, Clone)]
pub(crate) struct NodeState {
    pub docid: i64,
    pub eof: bool,
    pub start: bool,
    pub ami: Option<Vec<u32>>,
}

pub(crate) struct Eval {
    pub expr: Expr,
    pub ns: Vec<NodeState>,
    pub ph: Vec<Option<PhraseState>>,
    desc_idx: bool,
    pub bdesc: bool,
    pub prev_id: i64,
    pub is_eof: bool,
    pub ncol: usize,
    min_docid: i64,
    max_docid: i64,
}

/// Union of position lists (`fts3PoslistMerge`).
fn pl_union(a: &Pl, b: &Pl) -> Pl {
    let mut m: BTreeMap<i32, Vec<i64>> = BTreeMap::new();
    for (c, p) in a.iter().chain(b.iter()) {
        m.entry(*c).or_default().extend_from_slice(p);
    }
    m.into_iter()
        .map(|(c, mut p)| {
            p.sort_unstable();
            p.dedup();
            (c, p)
        })
        .collect()
}

/// `fts3PoslistPhraseMerge` with isExact: positions of `b` that follow a
/// position of `a` by exactly `dist`.
fn pl_phrase(a: &Pl, b: &Pl, dist: i64) -> Pl {
    let mut out = Pl::new();
    for (cb, pb) in b {
        if let Some((_, pa)) = a.iter().find(|(ca, _)| ca == cb) {
            let keep: Vec<i64> = pb
                .iter()
                .copied()
                .filter(|&p2| pa.binary_search(&(p2 - dist)).is_ok())
                .collect();
            if !keep.is_empty() {
                out.push((*cb, keep));
            }
        }
    }
    out
}

/// `fts3PoslistNearMerge`: positions of `target` within range of `src`.
fn pl_near(src: &Pl, target: &Pl, n_right: i64, n_left: i64) -> Pl {
    let mut out = Pl::new();
    for (ct, pt) in target {
        if let Some((_, ps)) = src.iter().find(|(cs, _)| cs == ct) {
            let keep: Vec<i64> = pt
                .iter()
                .copied()
                .filter(|&p2| {
                    ps.iter().any(|&p1| {
                        (p1 < p2 && p2 <= p1 + n_right) || (p2 < p1 && p1 <= p2 + n_left)
                    })
                })
                .collect();
            if !keep.is_empty() {
                out.push((*ct, keep));
            }
        }
    }
    out
}

fn pl_nonempty(pl: &Option<Pl>) -> bool {
    matches!(pl, Some(p) if p.iter().any(|(_, v)| !v.is_empty()))
}

/// Loads one token's doclist (`fts3TermSelect`), merged over all segments
/// and, for a prefix token, all matching terms.
fn token_doclist(ctx: &Ctx, langid: i32, tok: &super::expr::Token, icol: i32) -> Result<Doclist> {
    let tab = ctx.tab;
    let nterm = tok.z.len() as i32;
    let mut csr = MultiSegReader::default();
    let mut found = false;
    if tok.is_prefix {
        for i in 1..tab.prefixes.len() {
            if tab.prefixes[i] == nterm {
                found = true;
                seg_reader_cursor(
                    ctx,
                    langid,
                    i,
                    super::segment::CURSOR_ALL,
                    Some(&tok.z),
                    false,
                    false,
                    &mut csr,
                )?;
                break;
            }
        }
        if !found {
            for i in 1..tab.prefixes.len() {
                if tab.prefixes[i] == nterm + 1 {
                    found = true;
                    seg_reader_cursor(
                        ctx,
                        langid,
                        i,
                        super::segment::CURSOR_ALL,
                        Some(&tok.z),
                        true,
                        false,
                        &mut csr,
                    )?;
                    seg_reader_cursor(
                        ctx,
                        langid,
                        0,
                        super::segment::CURSOR_ALL,
                        Some(&tok.z),
                        false,
                        false,
                        &mut csr,
                    )?;
                    break;
                }
            }
        }
    }
    if !found {
        seg_reader_cursor(
            ctx,
            langid,
            0,
            super::segment::CURSOR_ALL,
            Some(&tok.z),
            tok.is_prefix,
            false,
            &mut csr,
        )?;
    }
    let ncol = tab.columns.len() as i32;
    let flags = IGNORE_EMPTY
        | REQUIRE_POS
        | if tok.is_prefix { PREFIX } else { 0 }
        | if tok.first { FIRST } else { 0 }
        | if icol < ncol { COLUMN_FILTER } else { 0 };
    csr.start(
        ctx,
        SegFilter {
            term: Some(tok.z.clone()),
            icol,
            flags,
        },
    )?;
    let mut merged: BTreeMap<i64, Pl> = BTreeMap::new();
    let mut first = true;
    let mut single: Doclist = Vec::new();
    while csr.step(ctx)? {
        let dl: Doclist = decode_doclist(&csr.doclist, tab.desc, true)
            .into_iter()
            .map(|(d, l)| (d, decode_poslist(&l)))
            .collect();
        if first {
            single = dl;
            first = false;
            continue;
        }
        if !single.is_empty() {
            for (d, pl) in single.drain(..) {
                merged.insert(d, pl);
            }
        }
        for (d, pl) in dl {
            match merged.get_mut(&d) {
                Some(e) => *e = pl_union(e, &pl),
                None => {
                    merged.insert(d, pl);
                }
            }
        }
    }
    let mut out: Doclist = if merged.is_empty() {
        single
    } else {
        merged.into_iter().collect()
    };
    if tab.desc {
        out.sort_by(|a, b| b.0.cmp(&a.0));
    } else {
        out.sort_by(|a, b| a.0.cmp(&b.0));
    }
    Ok(out)
}

impl Eval {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        ctx: &Ctx,
        expr: Expr,
        langid: i32,
        bdesc: bool,
        min_docid: i64,
        max_docid: i64,
    ) -> Result<Self> {
        let n = expr.nodes.len();
        let tab = ctx.tab;
        let mut ph: Vec<Option<PhraseState>> = (0..n).map(|_| None).collect();
        for i in expr.all_phrases() {
            let p = expr.nodes[i].phrase.as_ref().expect("phrase");
            // fts3EvalPhraseLoad: merge the token doclists left to right.
            let mut dl: Option<Doclist> = None;
            for t in &p.tokens {
                let tdl = token_doclist(ctx, langid, t, p.icol)?;
                dl = Some(match dl {
                    None => tdl,
                    Some(prev) => {
                        let mut right: BTreeMap<i64, Pl> = tdl.into_iter().collect();
                        let mut out = Doclist::new();
                        for (d, pl) in prev {
                            if let Some(r) = right.remove(&d) {
                                let m = pl_phrase(&pl, &r, 1);
                                if !m.is_empty() {
                                    out.push((d, m));
                                }
                            }
                        }
                        out
                    }
                });
            }
            let orig = dl.unwrap_or_default();
            let incr_ok = bdesc == tab.desc
                && !p.tokens.is_empty()
                && p.tokens.len() <= 4
                && p.tokens.iter().all(|t| {
                    !t.first && (!t.is_prefix || tab.prefixes[1..].contains(&(t.z.len() as i32)))
                });
            let mut st = PhraseState {
                ntoken: p.tokens.len(),
                icol: p.icol,
                orig,
                incr: incr_ok,
                ..Default::default()
            };
            if !st.incr {
                st.all.clone_from(&st.orig);
            }
            ph[i] = Some(st);
        }
        Ok(Self {
            expr,
            ns: vec![NodeState::default(); n],
            ph,
            desc_idx: tab.desc,
            bdesc,
            prev_id: 0,
            is_eof: false,
            ncol: tab.columns.len(),
            min_docid,
            max_docid,
        })
    }

    fn docid_cmp(&self, a: i64, b: i64) -> std::cmp::Ordering {
        if self.bdesc {
            b.cmp(&a)
        } else {
            a.cmp(&b)
        }
    }

    fn node(&self, i: usize) -> &super::expr::Node {
        &self.expr.nodes[i]
    }

    fn phrase(&mut self, i: usize) -> &mut PhraseState {
        self.ph[i].as_mut().expect("phrase state")
    }

    fn invalidate(&mut self, i: usize) {
        let p = self.phrase(i);
        p.list = None;
        p.list_idx = None;
    }

    /// `fts3EvalPhraseNext`; returns EOF.
    fn phrase_next(&mut self, i: usize) -> bool {
        let reverse = self.bdesc != self.desc_idx;
        let p = self.ph[i].as_mut().expect("phrase state");
        if p.incr {
            let k = p.next.unwrap_or_default();
            if k as usize >= p.orig.len() {
                return true;
            }
            p.docid = p.orig[k as usize].0;
            p.list = Some(p.orig[k as usize].1.clone());
            p.list_idx = Some(k as usize);
            p.next = Some(k + 1);
            return false;
        }
        let n = p.all.len() as isize;
        if reverse && n > 0 {
            let k = match p.next {
                None => n - 1,
                Some(k) => k - 1,
            };
            if k < 0 {
                p.next = Some(-1);
                return true;
            }
            p.next = Some(k);
            p.docid = p.all[k as usize].0;
            p.list = Some(p.all[k as usize].1.clone());
            p.list_idx = Some(k as usize);
            false
        } else {
            let k = p.next.unwrap_or(0);
            if k >= n {
                return true;
            }
            p.docid = p.all[k as usize].0;
            p.list = Some(p.all[k as usize].1.clone());
            p.list_idx = Some(k as usize);
            p.next = Some(k + 1);
            false
        }
    }

    /// Replaces the current position list of a phrase, writing through to
    /// `aAll` when the list lives there, as SQLite's in-place edits do.
    fn set_list(&mut self, i: usize, pl: Pl) {
        let p = self.phrase(i);
        if let Some(k) = p.list_idx {
            if !p.incr {
                p.all[k].1.clone_from(&pl);
            } else if p.ntoken == 1 {
                p.orig[k].1.clone_from(&pl);
            }
        }
        p.list = Some(pl);
    }

    /// `fts3EvalNextRow`.
    pub(crate) fn next_row(&mut self, n: usize) {
        if self.ns[n].eof {
            return;
        }
        self.ns[n].start = true;
        let node = self.node(n).clone();
        match node.etype {
            NEAR | AND => {
                let (l, r) = (node.left.unwrap(), node.right.unwrap());
                self.next_row(l);
                self.next_row(r);
                while !self.ns[l].eof && !self.ns[r].eof {
                    match self.docid_cmp(self.ns[l].docid, self.ns[r].docid) {
                        std::cmp::Ordering::Equal => break,
                        std::cmp::Ordering::Less => self.next_row(l),
                        std::cmp::Ordering::Greater => self.next_row(r),
                    }
                }
                self.ns[n].docid = self.ns[l].docid;
                self.ns[n].eof = self.ns[l].eof || self.ns[r].eof;
                if node.etype == NEAR && self.ns[n].eof {
                    // Drain both sides, zeroing loaded position lists.
                    for side in [r, l] {
                        if self.node(side).etype == PHRASE && !self.phrase(side).incr {
                            while !self.ns[side].eof {
                                self.set_list(side, Pl::new());
                                self.next_row(side);
                            }
                        }
                    }
                    self.ns[r].eof = true;
                    self.ns[l].eof = true;
                }
            }
            OR => {
                let (l, r) = (node.left.unwrap(), node.right.unwrap());
                let cmp = self.docid_cmp(self.ns[l].docid, self.ns[r].docid);
                if self.ns[r].eof || (!self.ns[l].eof && cmp == std::cmp::Ordering::Less) {
                    self.next_row(l);
                } else if self.ns[l].eof || cmp == std::cmp::Ordering::Greater {
                    self.next_row(r);
                } else {
                    self.next_row(l);
                    self.next_row(r);
                }
                self.ns[n].eof = self.ns[l].eof && self.ns[r].eof;
                let cmp = self.docid_cmp(self.ns[l].docid, self.ns[r].docid);
                self.ns[n].docid =
                    if self.ns[r].eof || (!self.ns[l].eof && cmp == std::cmp::Ordering::Less) {
                        self.ns[l].docid
                    } else {
                        self.ns[r].docid
                    };
            }
            NOT => {
                let (l, r) = (node.left.unwrap(), node.right.unwrap());
                if !self.ns[r].start {
                    self.next_row(r);
                }
                self.next_row(l);
                if !self.ns[l].eof {
                    while !self.ns[r].eof
                        && self.docid_cmp(self.ns[l].docid, self.ns[r].docid)
                            == std::cmp::Ordering::Greater
                    {
                        self.next_row(r);
                    }
                }
                self.ns[n].docid = self.ns[l].docid;
                self.ns[n].eof = self.ns[l].eof;
            }
            _ => {
                self.invalidate(n);
                let eof = self.phrase_next(n);
                self.ns[n].eof = eof;
                self.ns[n].docid = self.phrase(n).docid;
            }
        }
    }

    /// `fts3EvalNearTrim`.
    fn near_trim(
        &mut self,
        nnear: i32,
        src: &mut Pl,
        src_ntoken: &mut usize,
        target: usize,
    ) -> bool {
        let tnt = self.phrase(target).ntoken;
        let n1 = nnear as i64 + tnt as i64;
        let n2 = nnear as i64 + *src_ntoken as i64;
        let Some(tl) = self.phrase(target).list.clone() else {
            return false;
        };
        let out = pl_near(src, &tl, n1, n2);
        if out.is_empty() {
            return false;
        }
        self.set_list(target, out.clone());
        *src = out;
        *src_ntoken = tnt;
        true
    }

    /// `fts3EvalNearTest`.
    fn near_test(&mut self, n: usize) -> bool {
        let node = self.node(n).clone();
        if node.etype != NEAR {
            return true;
        }
        if let Some(p) = node.parent {
            if self.node(p).etype == NEAR {
                return true;
            }
        }
        let mut p = n;
        while let Some(l) = self.node(p).left {
            p = l;
        }
        let mut src = self.phrase(p).list.clone().unwrap_or_default();
        let mut ntok = self.phrase(p).ntoken;
        let mut res = true;
        let mut q = self.node(p).parent;
        while res {
            let Some(qq) = q else { break };
            if self.node(qq).etype != NEAR {
                break;
            }
            let target = self.node(qq).right.unwrap();
            let nnear = self.node(qq).nnear;
            res = self.near_trim(nnear, &mut src, &mut ntok, target);
            q = self.node(qq).parent;
        }
        let r = node.right.unwrap();
        let mut src = self.phrase(r).list.clone().unwrap_or_default();
        let mut ntok = self.phrase(r).ntoken;
        let mut q = node.left;
        while res {
            let Some(qq) = q else { break };
            let nnear = self.node(self.node(qq).parent.unwrap()).nnear;
            let target = if self.node(qq).etype == NEAR {
                self.node(qq).right.unwrap()
            } else {
                qq
            };
            res = self.near_trim(nnear, &mut src, &mut ntok, target);
            q = self.node(qq).left;
        }
        res
    }

    /// `fts3EvalTestExpr`.
    fn test_expr(&mut self, n: usize) -> bool {
        let node = self.node(n).clone();
        match node.etype {
            NEAR | AND => {
                let hit = self.test_expr(node.left.unwrap())
                    && self.test_expr(node.right.unwrap())
                    && self.near_test(n);
                let top_near =
                    node.etype == NEAR && node.parent.is_none_or(|p| self.node(p).etype != NEAR);
                if !hit && top_near {
                    let mut p = n;
                    while self.node(p).etype != PHRASE {
                        let r = self.node(p).right.unwrap();
                        if self.ns[r].docid == self.prev_id {
                            self.invalidate(r);
                        }
                        p = self.node(p).left.unwrap();
                    }
                    if self.ns[p].docid == self.prev_id {
                        self.invalidate(p);
                    }
                }
                hit
            }
            OR => {
                let a = self.test_expr(node.left.unwrap());
                let b = self.test_expr(node.right.unwrap());
                a || b
            }
            NOT => self.test_expr(node.left.unwrap()) && !self.test_expr(node.right.unwrap()),
            _ => {
                !self.ns[n].eof
                    && self.ns[n].docid == self.prev_id
                    && pl_nonempty(&self.phrase(n).list)
            }
        }
    }

    /// `fts3EvalNext`: moves to the next matching row.
    pub(crate) fn next(&mut self) {
        let Some(root) = self.expr.root else {
            self.is_eof = true;
            return;
        };
        loop {
            self.next_row(root);
            self.is_eof = self.ns[root].eof;
            self.prev_id = self.ns[root].docid;
            if self.is_eof || self.test_expr(root) {
                break;
            }
        }
        if (!self.bdesc && self.prev_id > self.max_docid)
            || (self.bdesc && self.prev_id < self.min_docid)
        {
            self.is_eof = true;
        }
    }

    /// `sqlite3Fts3EvalTestDeferred` on the current row, for its side effects.
    pub(crate) fn retest(&mut self) {
        if let Some(root) = self.expr.root {
            self.test_expr(root);
        }
    }

    /// `fts3EvalRestart`.
    fn restart(&mut self, n: usize) {
        let node = self.node(n).clone();
        if node.etype == PHRASE {
            self.invalidate(n);
            let p = self.phrase(n);
            if p.incr {
                p.incr = false;
                p.all = p.orig.clone();
            }
            p.next = None;
            p.docid = 0;
            p.or_iter = None;
        }
        self.ns[n].docid = 0;
        self.ns[n].eof = false;
        self.ns[n].start = false;
        if let Some(l) = node.left {
            self.restart(l);
        }
        if let Some(r) = node.right {
            self.restart(r);
        }
    }

    /// `sqlite3Fts3MsrCancel` as used by offsets(): an incremental phrase
    /// becomes a loaded one, positioned on the same row.
    pub(crate) fn cancel_incr(&mut self) {
        for i in self.expr.phrases() {
            if self.phrase(i).incr && !self.ns[i].eof {
                let docid = self.ns[i].docid;
                self.restart(i);
                loop {
                    self.next_row(i);
                    if self.ns[i].eof || self.ns[i].docid == docid {
                        break;
                    }
                }
            }
            self.phrase(i).incr = false;
        }
    }

    fn update_counts(&mut self, n: usize) {
        let node = self.node(n).clone();
        if node.etype == PHRASE {
            if let Some(list) = self.phrase(n).list.clone() {
                let ncol = self.ncol;
                if let Some(ami) = self.ns[n].ami.as_mut() {
                    for (col, poss) in &list {
                        let c = *col as usize;
                        if c >= ncol {
                            break;
                        }
                        ami[c * 3 + 1] += poss.len() as u32;
                        ami[c * 3 + 2] += (!poss.is_empty()) as u32;
                    }
                }
            }
        }
        if let Some(l) = node.left {
            self.update_counts(l);
        }
        if let Some(r) = node.right {
            self.update_counts(r);
        }
    }

    fn subtree_phrases(&self, root: usize) -> Vec<usize> {
        let sub = Expr {
            nodes: self.expr.nodes.clone(),
            root: Some(root),
        };
        sub.phrases()
    }

    /// `fts3EvalGatherStats`.
    fn gather_stats(&mut self, e: usize) {
        if self.ns[e].ami.is_some() {
            return;
        }
        let mut root = e;
        while let Some(p) = self.node(root).parent {
            if self.node(p).etype == NEAR {
                root = p;
            } else {
                break;
            }
        }
        let saved_prev = self.prev_id;
        let docid = self.ns[root].docid;
        let beof = self.ns[root].eof;
        for p in self.subtree_phrases(root) {
            self.ns[p].ami = Some(vec![0; self.ncol * 3]);
        }
        self.restart(root);
        let mut eof = false;
        while !eof {
            loop {
                self.next_row(root);
                eof = self.ns[root].eof;
                self.prev_id = self.ns[root].docid;
                if eof || self.node(root).etype != NEAR {
                    break;
                }
                let full = self.expr.root.unwrap();
                if self.test_expr(full) {
                    break;
                }
            }
            if !eof {
                self.update_counts(root);
            }
        }
        self.prev_id = saved_prev;
        if beof {
            self.ns[root].eof = true;
        } else {
            self.restart(root);
            loop {
                self.next_row(root);
                if self.ns[root].eof || self.ns[root].docid == docid {
                    break;
                }
            }
        }
    }

    /// `sqlite3Fts3EvalPhraseStats`: hits and documents per column.
    pub(crate) fn phrase_stats(&mut self, e: usize, out: &mut [u32]) {
        self.gather_stats(e);
        let ami = self.ns[e].ami.clone().unwrap_or_default();
        for c in 0..self.ncol {
            out[c * 3 + 1] = ami.get(c * 3 + 1).copied().unwrap_or(0);
            out[c * 3 + 2] = ami.get(c * 3 + 2).copied().unwrap_or(0);
        }
    }

    /// `sqlite3Fts3EvalPhrasePoslist`: positions of phrase `e` in column
    /// `icol` of the current row.
    pub(crate) fn phrase_poslist(&mut self, e: usize, icol: i32) -> Option<Vec<i64>> {
        let ncol = self.ncol as i32;
        let picol = self.phrase(e).icol;
        if picol < ncol && picol != icol {
            return None;
        }
        let mut list = self.phrase(e).list.clone();
        if self.ns[e].docid != self.prev_id || self.ns[e].eof {
            let mut bor = false;
            let mut near = e;
            let mut tree_eof = false;
            let mut p = self.node(e).parent;
            while let Some(pp) = p {
                if self.node(pp).etype == OR {
                    bor = true;
                }
                if self.node(pp).etype == NEAR {
                    near = pp;
                }
                if self.ns[pp].eof {
                    tree_eof = true;
                }
                p = self.node(pp).parent;
            }
            if !bor {
                return None;
            }
            let run = near;
            if self.phrase(e).incr {
                let eof_save = self.ns[run].eof;
                let docid = self.ns[e].docid;
                self.restart(run);
                while !self.ns[run].eof {
                    self.next_row(run);
                    if !eof_save && self.ns[run].docid == docid {
                        break;
                    }
                }
            }
            if tree_eof {
                while !self.ns[run].eof {
                    self.next_row(run);
                }
            }
            let mut matched = true;
            let mut q = Some(near);
            while let Some(qq) = q {
                let test = if self.node(qq).etype == NEAR {
                    self.node(qq).right.unwrap()
                } else {
                    qq
                };
                let prev = self.prev_id;
                let forward = self.bdesc == self.desc_idx;
                // Here SQLite's DOCID_CMP follows the index order, not the cursor's.
                let idx_cmp = |me: &Self, a: i64| {
                    if me.desc_idx {
                        prev.cmp(&a)
                    } else {
                        a.cmp(&prev)
                    }
                };
                let cmp_less = |me: &Self, a: i64| idx_cmp(me, a) == std::cmp::Ordering::Less;
                let cmp_greater = |me: &Self, a: i64| idx_cmp(me, a) == std::cmp::Ordering::Greater;
                let n = self.phrase(test).all.len() as isize;
                let mut it = self.phrase(test).or_iter;
                let mut docid = self.phrase(test).or_docid;
                let mut eof;
                if forward {
                    eof = n == 0 || it.is_some_and(|k| k >= n);
                    while (it.is_none() || cmp_less(self, docid)) && !eof {
                        let k = match it {
                            None => 0,
                            Some(k) => k + 1,
                        };
                        if k >= n {
                            eof = true;
                            it = Some(n);
                        } else {
                            it = Some(k);
                            docid = self.phrase(test).all[k as usize].0;
                        }
                    }
                } else {
                    eof = n == 0 || it.is_some_and(|k| k < 0);
                    while (it.is_none() || cmp_greater(self, docid)) && !eof {
                        let k = match it {
                            None => n - 1,
                            Some(k) => k - 1,
                        };
                        if k < 0 {
                            eof = true;
                            it = Some(-1);
                        } else {
                            it = Some(k);
                            docid = self.phrase(test).all[k as usize].0;
                        }
                    }
                }
                let ps = self.phrase(test);
                ps.or_iter = it;
                ps.or_docid = docid;
                if eof || docid != prev {
                    matched = false;
                }
                q = self.node(qq).left;
                if self.node(qq).etype == PHRASE {
                    break;
                }
            }
            list = if matched {
                let ps = self.phrase(e);
                match ps.or_iter {
                    Some(k) if k >= 0 && (k as usize) < ps.all.len() => {
                        Some(ps.all[k as usize].1.clone())
                    }
                    _ => None,
                }
            } else {
                None
            };
        }
        let list = list?;
        list.into_iter()
            .find(|(c, _)| *c == icol)
            .map(|(_, p)| p)
            .filter(|p| !p.is_empty())
    }

    /// The current position list of a phrase (`doclist.pList`).
    pub(crate) fn current_list(&self, e: usize) -> Option<&Pl> {
        self.ph[e].as_ref().and_then(|p| p.list.as_ref())
    }
}

pub(crate) fn _ops() -> [u8; 2] {
    [AND, OR]
}
