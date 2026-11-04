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
use super::table::{Ctx, Fts3Table};
use crate::Connection;
use crate::Result;
use std::collections::BTreeMap;
use std::sync::Arc;

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
    /// Highest token index in the loaded doclist (`iDoclistToken`), or -1.
    doclist_token: i32,
    /// Tokens tested against the row content instead (deferred tokens).
    deferred: Vec<usize>,
}

#[derive(Default, Clone)]
pub(crate) struct NodeState {
    pub docid: i64,
    pub eof: bool,
    pub start: bool,
    pub ami: Option<Vec<u32>>,
    /// Every token below this node is deferred (`bDeferred`).
    pub deferred: bool,
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
    /// Whether any token is deferred (`pCsr->pDeferred`).
    any_deferred: bool,
    /// Documents in the table (`pCsr->nDoc`), known once deferral ran.
    n_doc: i64,
    tab: Arc<Fts3Table>,
    conn: Arc<Connection>,
    langid: i32,
    /// The row content the deferred tokens were matched against.
    row_cache: Option<(i64, Vec<Option<Vec<u8>>>)>,
    /// First error raised while testing deferred tokens.
    err: Option<crate::LimboError>,
}

/// One token's place while deferral is decided (`Fts3TokenAndCost`).
struct TokenCost {
    phrase: usize,
    token: usize,
    root: Option<usize>,
    ovfl: i64,
    icol: i32,
}

/// Merges a token doclist into a phrase's partial doclist
/// (`fts3EvalPhraseMergeToken`); positions are those of the later token.
fn merge_token(partial: &mut Option<(i32, Doclist)>, itoken: i32, dl: Doclist) {
    *partial = Some(match partial.take() {
        None => (itoken, dl),
        Some((max, prev)) => {
            let (left, right, dist) = if max < itoken {
                (prev, dl, itoken - max)
            } else {
                (dl, prev, max - itoken)
            };
            let mut rmap: BTreeMap<i64, Pl> = right.into_iter().collect();
            let mut out = Doclist::new();
            for (d, pl) in left {
                if let Some(r) = rmap.remove(&d) {
                    let m = pl_phrase(&pl, &r, dist as i64);
                    if !m.is_empty() {
                        out.push((d, m));
                    }
                }
            }
            (max.max(itoken), out)
        }
    });
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
/// The segment readers of one token (`fts3TermSegReaderCursor`).
fn token_cursor(ctx: &Ctx, langid: i32, tok: &super::expr::Token) -> Result<MultiSegReader> {
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
    Ok(csr)
}

/// Pages of a token's doclist beyond one per leaf (`sqlite3Fts3MsrOvfl`).
fn token_ovfl(ctx: &Ctx, csr: &MultiSegReader) -> Result<i64> {
    let pgsz = ctx.tab.page_size(ctx.conn)? as i64;
    let sql = format!(
        "SELECT length(block) FROM {} WHERE blockid = ?",
        ctx.tab.shadow("segments")
    );
    let mut novfl = 0;
    for seg in &csr.segs {
        if seg.is_pending() || seg.is_root_only() {
            continue;
        }
        for b in seg.start_block..=seg.leaf_end_block {
            let n = match ctx.sql().row(&sql, &[crate::Value::from_i64(b)])? {
                Some(r) => super::value_int(&r[0]),
                None => return Err(super::corrupt()),
            };
            if n + 35 > pgsz {
                novfl += (n + 34) / pgsz;
            }
        }
    }
    Ok(novfl)
}

/// `fts3TermSelect` over a token's readers.
fn cursor_doclist(
    ctx: &Ctx,
    mut csr: MultiSegReader,
    tok: &super::expr::Token,
    icol: i32,
) -> Result<Doclist> {
    let tab = ctx.tab;
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
        tab: Arc<Fts3Table>,
        conn: Arc<Connection>,
        expr: Expr,
        langid: i32,
        bdesc: bool,
        min_docid: i64,
        max_docid: i64,
    ) -> Result<Self> {
        let n = expr.nodes.len();
        let ctx = Ctx {
            conn: &conn,
            tab: &tab,
        };
        let phrases = expr.all_phrases();
        // Each token's readers (fts3EvalAllocateReaders).
        let mut csrs: BTreeMap<(usize, usize), MultiSegReader> = BTreeMap::new();
        let mut ntoken = 0;
        for &e in &phrases {
            let p = expr.nodes[e].phrase.as_ref().expect("phrase");
            for (i, t) in p.tokens.iter().enumerate() {
                csrs.insert((e, i), token_cursor(&ctx, langid, t)?);
                ntoken += 1;
            }
        }
        let mut partial: BTreeMap<usize, Option<(i32, Doclist)>> = BTreeMap::new();
        let mut deferred: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        let mut n_doc = 0;
        if ntoken > 1 && tab.fts4 {
            // fts3EvalTokenCosts: AND/NEAR clusters, NOT subtrees skipped.
            let mut tcs: Vec<TokenCost> = Vec::new();
            let mut ors: Vec<usize> = Vec::new();
            fn costs(
                expr: &Expr,
                n: usize,
                root: Option<usize>,
                tcs: &mut Vec<TokenCost>,
                ors: &mut Vec<usize>,
            ) {
                let node = &expr.nodes[n];
                if node.etype == PHRASE {
                    let p = node.phrase.as_ref().expect("phrase");
                    for i in 0..p.tokens.len() {
                        tcs.push(TokenCost {
                            phrase: n,
                            token: i,
                            root,
                            ovfl: 0,
                            icol: p.icol,
                        });
                    }
                } else if node.etype != NOT {
                    let (l, r) = (node.left.unwrap(), node.right.unwrap());
                    let mut lroot = root;
                    let mut rroot = root;
                    if node.etype == OR {
                        lroot = Some(l);
                        ors.push(l);
                    }
                    costs(expr, l, lroot, tcs, ors);
                    if node.etype == OR {
                        rroot = Some(r);
                        ors.push(r);
                    }
                    costs(expr, r, rroot, tcs, ors);
                }
            }
            if let Some(root) = expr.root {
                costs(&expr, root, None, &mut tcs, &mut ors);
            }
            for tc in tcs.iter_mut() {
                tc.ovfl = token_ovfl(&ctx, &csrs[&(tc.phrase, tc.token)])?;
            }
            let mut row_avg = 0i64;
            let roots: Vec<Option<usize>> = std::iter::once(None)
                .chain(ors.into_iter().map(Some))
                .collect();
            for root in roots {
                // fts3EvalSelectDeferred
                if tab.content.is_some() {
                    break;
                }
                let idx: Vec<usize> = (0..tcs.len()).filter(|&i| tcs[i].root == root).collect();
                let novfl: i64 = idx.iter().map(|&i| tcs[i].ovfl).sum();
                if novfl == 0 || idx.len() < 2 {
                    continue;
                }
                if row_avg == 0 {
                    // fts3EvalAverageDocsize
                    let sql = format!("SELECT value FROM {} WHERE id=?", tab.shadow("stat"));
                    let a = match ctx.sql().row(&sql, &[crate::Value::from_i64(0)])? {
                        Some(r) => match &r[0] {
                            crate::Value::Blob(b) => b.to_vec(),
                            _ => return Err(super::corrupt()),
                        },
                        None => return Err(super::corrupt()),
                    };
                    let mut p = 0;
                    let (nd, k) = super::get_varint(&a, p);
                    p += k;
                    let mut nbyte = 0;
                    while p < a.len() {
                        let (v, k) = super::get_varint(&a, p);
                        p += k;
                        nbyte = v;
                    }
                    if nd == 0 || nbyte == 0 {
                        return Err(super::corrupt());
                    }
                    n_doc = nd;
                    let pgsz = tab.page_size(&conn)? as i64;
                    row_avg = ((nbyte / nd) + pgsz) / pgsz;
                }
                let mut done = vec![false; idx.len()];
                let mut min_est: i64 = 0;
                let mut load4: i64 = 1;
                for ii in 0..idx.len() {
                    let mut pick: Option<usize> = None;
                    for (k, &i) in idx.iter().enumerate() {
                        if !done[k] && pick.is_none_or(|p| tcs[i].ovfl < tcs[idx[p]].ovfl) {
                            pick = Some(k);
                        }
                    }
                    let k = pick.expect("token left");
                    done[k] = true;
                    let tc = &tcs[idx[k]];
                    let key = (tc.phrase, tc.token);
                    if ii > 0 && tc.ovfl >= ((min_est + (load4 / 4) - 1) / (load4 / 4)) * row_avg {
                        csrs.remove(&key);
                        deferred.entry(tc.phrase).or_default().push(tc.token);
                    } else {
                        if ii < 12 {
                            load4 *= 4;
                        }
                        let pntok = expr.nodes[tc.phrase]
                            .phrase
                            .as_ref()
                            .map(|p| p.tokens.len())
                            .unwrap_or(0);
                        if ii == 0 || (pntok > 1 && ii != idx.len() - 1) {
                            let tok = expr.nodes[tc.phrase].phrase.as_ref().unwrap().tokens
                                [tc.token]
                                .clone();
                            let csr = csrs.remove(&key).expect("token readers");
                            let dl = cursor_doclist(&ctx, csr, &tok, tc.icol)?;
                            let part = partial.entry(tc.phrase).or_default();
                            merge_token(part, tc.token as i32, dl);
                            let count = part.as_ref().map(|p| p.1.len()).unwrap_or(0) as i64;
                            if ii == 0 || count < min_est {
                                min_est = count;
                            }
                        }
                    }
                }
            }
        }
        let mut ph: Vec<Option<PhraseState>> = (0..n).map(|_| None).collect();
        let mut ns = vec![NodeState::default(); n];
        let any_deferred = deferred.values().any(|v| !v.is_empty());
        for &e in &phrases {
            let p = expr.nodes[e].phrase.as_ref().expect("phrase");
            let def = deferred.remove(&e).unwrap_or_default();
            let mut part = partial.remove(&e).unwrap_or_default();
            let mut remaining = Vec::new();
            for (i, t) in p.tokens.iter().enumerate() {
                if let Some(csr) = csrs.remove(&(e, i)) {
                    remaining.push(i);
                    let dl = cursor_doclist(&ctx, csr, t, p.icol)?;
                    merge_token(&mut part, i as i32, dl);
                }
            }
            let (doclist_token, orig) = match part {
                Some((m, dl)) => (m, dl),
                None => (-1, Doclist::new()),
            };
            // fts3EvalPhraseStart: only tokens still read from the index count.
            let incr_ok = bdesc == tab.desc
                && !p.tokens.is_empty()
                && p.tokens.len() <= 4
                && p.tokens.iter().all(|t| !t.first)
                && !remaining.is_empty()
                && remaining.iter().all(|&i| {
                    let t = &p.tokens[i];
                    !t.is_prefix || tab.prefixes[1..].contains(&(t.z.len() as i32))
                });
            ns[e].deferred = !p.tokens.is_empty() && def.len() == p.tokens.len();
            let mut st = PhraseState {
                ntoken: p.tokens.len(),
                icol: p.icol,
                orig,
                incr: incr_ok,
                doclist_token,
                deferred: def,
                ..Default::default()
            };
            if !st.incr {
                st.all.clone_from(&st.orig);
            }
            ph[e] = Some(st);
        }
        // bDeferred of operators: both sides deferred.
        fn mark(expr: &Expr, n: usize, ns: &mut [NodeState]) -> bool {
            let node = &expr.nodes[n];
            if node.etype == PHRASE {
                return ns[n].deferred;
            }
            let l = mark(expr, node.left.unwrap(), ns);
            let r = mark(expr, node.right.unwrap(), ns);
            ns[n].deferred = l && r;
            ns[n].deferred
        }
        if let Some(root) = expr.root {
            mark(&expr, root, &mut ns);
        }
        let ncol = tab.columns.len();
        let desc_idx = tab.desc;
        Ok(Self {
            expr,
            ns,
            ph,
            desc_idx,
            bdesc,
            prev_id: 0,
            is_eof: false,
            ncol,
            min_docid,
            max_docid,
            any_deferred,
            n_doc,
            tab,
            conn,
            langid,
            row_cache: None,
            err: None,
        })
    }

    /// Positions of deferred token `tok` of phrase `e` in the current row
    /// (`sqlite3Fts3CacheDeferredDoclists`).
    fn deferred_positions(&mut self, e: usize, itok: usize) -> Result<Pl> {
        if self
            .row_cache
            .as_ref()
            .is_none_or(|(d, _)| *d != self.prev_id)
        {
            let sql = format!("{} WHERE rowid = ?", self.tab.read_select()?);
            let row = super::Sql::new(&self.conn)
                .row(&sql, &[crate::Value::from_i64(self.prev_id)])?
                .ok_or_else(super::corrupt)?;
            let texts = (0..self.ncol)
                .map(|i| row.get(i + 1).and_then(super::value_text))
                .collect();
            self.row_cache = Some((self.prev_id, texts));
        }
        let p = self.expr.nodes[e].phrase.as_ref().expect("phrase");
        let t = &p.tokens[itok];
        let icol = p.icol;
        let tok = self.tab.tokenizer(Some(&self.conn))?;
        let texts = &self.row_cache.as_ref().unwrap().1;
        let mut out = Pl::new();
        for (i, text) in texts.iter().enumerate() {
            if self.tab.notindexed[i] {
                continue;
            }
            if icol < self.ncol as i32 && icol != i as i32 {
                continue;
            }
            let Some(text) = text else { continue };
            let mut poss = Vec::new();
            tok.tokenize(self.langid, text, &mut |z, _, _, pos| {
                if (!t.first || pos == 0)
                    && (z.len() == t.z.len() || (t.is_prefix && t.z.len() < z.len()))
                    && z.starts_with(&t.z)
                {
                    poss.push(pos as i64);
                }
                Ok(true)
            })?;
            if !poss.is_empty() {
                out.push((i as i32, poss));
            }
        }
        Ok(out)
    }

    /// `fts3EvalDeferredPhrase`: combines the deferred tokens' positions
    /// with the phrase's loaded position list.
    fn deferred_phrase(&mut self, e: usize) -> Result<()> {
        let mut defs = self.phrase(e).deferred.clone();
        defs.sort_unstable();
        let mut acc: Option<Pl> = None;
        let mut iprev: i64 = -1;
        for itok in defs {
            let pl = self.deferred_positions(e, itok)?;
            if pl.is_empty() {
                self.invalidate(e);
                return Ok(());
            }
            acc = Some(match acc {
                None => pl,
                Some(a) => {
                    let m = pl_phrase(&a, &pl, itok as i64 - iprev);
                    if m.is_empty() {
                        self.invalidate(e);
                        return Ok(());
                    }
                    m
                }
            });
            iprev = itok as i64;
        }
        let Some(acc) = acc else { return Ok(()) };
        let undeferred = self.phrase(e).doclist_token as i64;
        if undeferred < 0 {
            let prev = self.prev_id;
            let p = self.phrase(e);
            p.list = Some(acc);
            p.list_idx = None;
            p.docid = prev;
        } else {
            let cur = self.phrase(e).list.clone().unwrap_or_default();
            let m = if undeferred > iprev {
                pl_phrase(&acc, &cur, undeferred - iprev)
            } else {
                pl_phrase(&cur, &acc, iprev - undeferred)
            };
            let p = self.phrase(e);
            p.list_idx = None;
            p.list = if m.is_empty() { None } else { Some(m) };
        }
        Ok(())
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
            NEAR | AND if self.ns[node.left.unwrap()].deferred => {
                let r = node.right.unwrap();
                self.next_row(r);
                self.ns[n].docid = self.ns[r].docid;
                self.ns[n].eof = self.ns[r].eof;
            }
            NEAR | AND if self.ns[node.right.unwrap()].deferred => {
                let l = node.left.unwrap();
                self.next_row(l);
                self.ns[n].docid = self.ns[l].docid;
                self.ns[n].eof = self.ns[l].eof;
            }
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
            _ if self.any_deferred
                && (self.ns[n].deferred
                    || (self.ns[n].docid == self.prev_id && self.phrase(n).list.is_some())) =>
            {
                if self.ns[n].deferred {
                    self.invalidate(n);
                }
                if let Err(e) = self.deferred_phrase(n) {
                    self.err.get_or_insert(e);
                }
                self.ns[n].docid = self.prev_id;
                self.phrase(n).list.is_some()
            }
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
            self.row_cache = None;
            self.next_row(root);
            self.is_eof = self.ns[root].eof;
            self.prev_id = self.ns[root].docid;
            if self.is_eof || self.test_expr(root) || self.err.is_some() {
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
            if self.node(p).etype == NEAR || self.ns[root].deferred {
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
                if self.test_expr(full) || self.err.is_some() {
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
        let parent_near = self
            .node(e)
            .parent
            .is_some_and(|p| self.node(p).etype == NEAR);
        if self.ns[e].deferred && !parent_near {
            for c in 0..self.ncol {
                out[c * 3 + 1] = self.n_doc as u32;
                out[c * 3 + 2] = self.n_doc as u32;
            }
            return;
        }
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
            let mut run = near;
            while self.ns[run].deferred {
                run = self.node(run).parent.expect("deferred node has a parent");
            }
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

    /// Takes the error, if any, raised while testing deferred tokens.
    pub(crate) fn take_err(&mut self) -> Result<()> {
        match self.err.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// The current position list of a phrase (`doclist.pList`).
    pub(crate) fn current_list(&self, e: usize) -> Option<&Pl> {
        self.ph[e].as_ref().and_then(|p| p.list.as_ref())
    }
}

pub(crate) fn _ops() -> [u8; 2] {
    [AND, OR]
}
