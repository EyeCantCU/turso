//! Writing the full-text index (`fts3_write.c`): pending terms, segment
//! writer, merges, incremental merge, and the special INSERT commands.

use super::segment::{
    seg_reader_cursor, MultiSegReader, PendingList, SegFilter, SegReader, CURSOR_ALL,
    CURSOR_PENDING, IGNORE_EMPTY, REQUIRE_POS, SCAN,
};
use super::table::Ctx;
use super::{
    corrupt, get_varint, get_varint32, put_varint, quote_id, sql_error, value_int, value_text,
    varint_len, Sql, MAX_PENDING_DATA, MERGE_COUNT, SEGDIR_MAXLEVEL, VARINT_MAX,
};
use crate::sync::atomic::Ordering;
use crate::sync::Arc;
use crate::{Connection, LimboError, Result, Value};
use std::collections::BTreeMap;

const STAT_DOCTOTAL: i64 = 0;
const STAT_INCRMERGEHINT: i64 = 1;
const STAT_AUTOINCRMERGE: i64 = 2;
const MAX_APPENDABLE_HEIGHT: usize = 16;
/// `sizeof(Fts3HashElem)` on 64-bit targets, counted per pending term.
const HASH_ELEM_SIZE: usize = 40;

/// Per-connection write state of one table (the mutable part of `Fts3Table`).
pub(crate) struct Pending {
    pub index: Vec<BTreeMap<Vec<u8>, PendingList>>,
    pub n_data: usize,
    pub prev_docid: i64,
    pub prev_langid: i32,
    pub prev_delete: bool,
    pub n_leaf_add: u32,
    pub autoincrmerge: i32,
    pub savepoint: i32,
    pub ignore_savepoint: bool,
}

impl Pending {
    pub(crate) fn new(nindex: usize) -> Self {
        Self {
            index: (0..nindex).map(|_| BTreeMap::new()).collect(),
            n_data: 0,
            prev_docid: 0,
            prev_langid: 0,
            prev_delete: false,
            n_leaf_add: 0,
            autoincrmerge: 0xff,
            savepoint: 0,
            ignore_savepoint: false,
        }
    }
}

pub(crate) fn abs_level(
    tab: &super::table::Fts3Table,
    langid: i32,
    index: usize,
    level: i64,
) -> i64 {
    ((langid as i64) * tab.nindex() as i64 + index as i64) * SEGDIR_MAXLEVEL + level
}

/// Creates `%_stat` for an FTS3 table from inside a write. In SQLite the
/// nested CREATE TABLE opens a statement savepoint, which flushes pending
/// terms first.
fn create_stat_table_nested(ctx: &Ctx) -> Result<()> {
    let level = ctx.with_pending(|p| p.savepoint);
    savepoint(ctx, level)?;
    create_stat_table(ctx.conn, ctx.tab)
}

pub(crate) fn create_stat_table(
    conn: &Arc<Connection>,
    tab: &super::table::Fts3Table,
) -> Result<()> {
    Sql::new(conn).exec(
        &format!(
            "CREATE TABLE IF NOT EXISTS {}(id INTEGER PRIMARY KEY, value BLOB)",
            tab.shadow("stat")
        ),
        &[],
    )?;
    tab.has_stat.store(1, Ordering::Relaxed);
    Ok(())
}

fn has_stat(ctx: &Ctx) -> Result<bool> {
    // fts3SetHasStat
    let tab = ctx.tab;
    if tab.has_stat.load(Ordering::Relaxed) == 2 {
        let sql = "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?";
        let tbl = format!("{}_stat", tab.name());
        let found = ctx.sql().row(sql, &[Value::build_text(tbl)])?.is_some();
        tab.has_stat.store(found as u8, Ordering::Relaxed);
    }
    Ok(tab.has_stat.load(Ordering::Relaxed) == 1)
}

pub(crate) fn begin(ctx: &Ctx) -> Result<()> {
    ctx.with_pending(|p| p.n_leaf_add = 0);
    has_stat(ctx)?;
    Ok(())
}

pub(crate) fn pending_clear(ctx: &Ctx) {
    ctx.with_pending(|p| {
        for h in p.index.iter_mut() {
            h.clear();
        }
        p.n_data = 0;
    });
}

/// `fts3SyncMethod`: flushes pending terms and runs the automatic merge.
pub(crate) fn sync(ctx: &Ctx) -> Result<()> {
    const MIN_MERGE: i64 = 64;
    let last = ctx.conn.last_insert_rowid();
    let changes = ctx.conn.changes();
    let r = (|| {
        pending_flush(ctx)?;
        let (leaf_add, auto) = ctx.with_pending(|p| (p.n_leaf_add, p.autoincrmerge));
        if leaf_add as i64 > MIN_MERGE / 16 && auto != 0 && auto != 0xff {
            let mx = max_level(ctx)?;
            let mut a = leaf_add as i64 * mx;
            a += a / 2;
            if a > MIN_MERGE {
                incrmerge(ctx, a, auto as i64)?;
            }
        }
        Ok(())
    })();
    ctx.conn.update_last_rowid(last);
    ctx.conn.set_changes(changes);
    r
}

pub(crate) fn savepoint(ctx: &Ctx, level: i32) -> Result<()> {
    let (ignore, any) = ctx.with_pending(|p| (p.ignore_savepoint, !p.index[0].is_empty()));
    if !ignore {
        if any {
            ctx.with_pending(|p| p.ignore_savepoint = true);
            let r = pending_flush(ctx);
            ctx.with_pending(|p| p.ignore_savepoint = false);
            r?;
        }
        ctx.with_pending(|p| p.savepoint = level + 1);
    }
    Ok(())
}

/// `fts3RenameMethod`.
pub(crate) fn rename(ctx: &Ctx, new: &str) -> Result<()> {
    has_stat(ctx)?;
    pending_flush(ctx)?;
    let tab = ctx.tab;
    let sql = ctx.sql();
    let mut suffixes = Vec::new();
    if tab.content.is_none() {
        suffixes.push("content");
    }
    if tab.has_docsize {
        suffixes.push("docsize");
    }
    if tab.has_stat.load(Ordering::Relaxed) == 1 {
        suffixes.push("stat");
    }
    suffixes.push("segments");
    suffixes.push("segdir");
    ctx.with_pending(|p| p.ignore_savepoint = true);
    let mut r = Ok(());
    for s in suffixes {
        r = sql.exec(
            &format!(
                "ALTER TABLE {} RENAME TO {}",
                tab.shadow(s),
                quote_id(&format!("{new}_{s}"))
            ),
            &[],
        );
        if r.is_err() {
            break;
        }
    }
    ctx.with_pending(|p| p.ignore_savepoint = false);
    let key = tab.pending_key();
    *tab.name.write() = new.to_string();
    let mut inner = ctx.conn.fts3.inner.lock();
    if let Some(p) = inner.pending.remove(&key) {
        inner.pending.insert(tab.pending_key(), p);
    }
    r
}

/// `fts3DestroyMethod`.
pub(crate) fn destroy(ctx: &Ctx) -> Result<()> {
    let tab = ctx.tab;
    let sql = ctx.sql();
    for s in ["segments", "segdir", "docsize", "stat"] {
        sql.exec(&format!("DROP TABLE IF EXISTS {}", tab.shadow(s)), &[])?;
    }
    if tab.content.is_none() {
        sql.exec(
            &format!("DROP TABLE IF EXISTS {}", tab.shadow("content")),
            &[],
        )?;
    }
    ctx.conn
        .fts3
        .inner
        .lock()
        .pending
        .remove(&tab.pending_key());
    Ok(())
}

/// Tokenizes `text` into the pending terms (`fts3PendingTermsAdd`);
/// returns the number of token positions.
fn pending_add(ctx: &Ctx, langid: i32, text: Option<&[u8]>, icol: i32) -> Result<u32> {
    let Some(text) = text else { return Ok(0) };
    let tok = ctx.tab.tokenizer(Some(ctx.conn))?;
    let prefixes = ctx.tab.prefixes.clone();
    let mut nword: i32 = 0;
    let mut toks: Vec<(Vec<u8>, i32)> = Vec::new();
    tok.tokenize(langid, text, &mut |t, _s, _e, pos| {
        if pos >= nword {
            nword = pos + 1;
        }
        if pos < 0 || t.is_empty() {
            return Err(sql_error("SQL logic error"));
        }
        toks.push((t.to_vec(), pos));
        Ok(true)
    })?;
    ctx.with_pending(|p| {
        let docid = p.prev_docid;
        for (t, pos) in &toks {
            add_one(p, 0, t, docid, icol, *pos);
            for (i, &np) in prefixes.iter().enumerate().skip(1) {
                if (t.len() as i32) < np {
                    continue;
                }
                add_one(p, i, &t[..np as usize], docid, icol, *pos);
            }
        }
    });
    Ok(nword as u32)
}

fn add_one(p: &mut Pending, index: usize, term: &[u8], docid: i64, icol: i32, pos: i32) {
    let list = p.index[index].entry(term.to_vec()).or_default();
    let before = list.n_data();
    let existed = before > 0;
    if existed {
        p.n_data = p
            .n_data
            .saturating_sub(before + term.len() + HASH_ELEM_SIZE);
    }
    list.append(docid, icol as i64, pos as i64);
    p.n_data += list.n_data() + term.len() + HASH_ELEM_SIZE;
}

/// `fts3PendingTermsDocid`: flushes when docids stop increasing.
fn pending_docid(ctx: &Ctx, delete: bool, langid: i32, docid: i64) -> Result<()> {
    let need = ctx.with_pending(|p| {
        docid < p.prev_docid
            || (docid == p.prev_docid && !p.prev_delete)
            || p.prev_langid != langid
            || p.n_data > MAX_PENDING_DATA
    });
    if need {
        pending_flush(ctx)?;
    }
    ctx.with_pending(|p| {
        p.prev_docid = docid;
        p.prev_langid = langid;
        p.prev_delete = delete;
    });
    Ok(())
}

/// `sqlite3Fts3PendingTermsFlush`.
pub(crate) fn pending_flush(ctx: &Ctx) -> Result<()> {
    let langid = ctx.prev_langid();
    for i in 0..ctx.tab.nindex() {
        match segment_merge(ctx, langid, i, CURSOR_PENDING) {
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    let (auto, leaf_add) = ctx.with_pending(|p| (p.autoincrmerge, p.n_leaf_add));
    if ctx.tab.has_stat.load(Ordering::Relaxed) == 1 && auto == 0xff && leaf_add > 0 {
        let v = select_stat(ctx, STAT_AUTOINCRMERGE)?;
        let n = match v {
            Some(v) => {
                let n = value_int(&v) as i32;
                if n == 1 {
                    8
                } else {
                    n
                }
            }
            None => 0,
        };
        ctx.with_pending(|p| p.autoincrmerge = n);
    }
    pending_clear(ctx);
    Ok(())
}

fn select_stat(ctx: &Ctx, id: i64) -> Result<Option<Value>> {
    let sql = format!("SELECT value FROM {} WHERE id=?", ctx.tab.shadow("stat"));
    Ok(ctx
        .sql()
        .row(&sql, &[Value::from_i64(id)])?
        .map(|mut r| r.swap_remove(0)))
}

fn replace_stat(ctx: &Ctx, id: i64, v: Value) -> Result<()> {
    let sql = format!("REPLACE INTO {} VALUES(?,?)", ctx.tab.shadow("stat"));
    ctx.sql().exec(&sql, &[Value::from_i64(id), v])
}

fn blob(b: &[u8]) -> Value {
    Value::from_slice(b).unwrap_or(Value::Null)
}

fn max_level(ctx: &Ctx) -> Result<i64> {
    let sql = format!(
        "SELECT max( level % 1024 ) FROM {}",
        ctx.tab.shadow("segdir")
    );
    Ok(ctx
        .sql()
        .row(&sql, &[])?
        .map(|r| value_int(&r[0]))
        .unwrap_or(0))
}

fn write_segment(ctx: &Ctx, block: i64, data: Option<&[u8]>) -> Result<()> {
    let sql = format!(
        "REPLACE INTO {}(blockid, block) VALUES(?, ?)",
        ctx.tab.shadow("segments")
    );
    let v = match data {
        Some(d) => blob(d),
        None => Value::Null,
    };
    ctx.sql().exec(&sql, &[Value::from_i64(block), v])
}

#[allow(clippy::too_many_arguments)]
fn write_segdir(
    ctx: &Ctx,
    level: i64,
    idx: i64,
    start: i64,
    leaf_end: i64,
    end: i64,
    nleaf_data: i64,
    root: &[u8],
) -> Result<()> {
    let sql = format!(
        "REPLACE INTO {} VALUES(?,?,?,?,?,?)",
        ctx.tab.shadow("segdir")
    );
    let endv = if nleaf_data == 0 {
        Value::from_i64(end)
    } else {
        Value::build_text(format!("{end} {nleaf_data}"))
    };
    ctx.sql().exec(
        &sql,
        &[
            Value::from_i64(level),
            Value::from_i64(idx),
            Value::from_i64(start),
            Value::from_i64(leaf_end),
            endv,
            blob(root),
        ],
    )
}

fn prefix_compress(prev: &[u8], next: &[u8]) -> usize {
    prev.iter().zip(next).take_while(|(a, b)| a == b).count()
}

struct TreeNode {
    parent: Option<usize>,
    right: Option<usize>,
    leftmost: usize,
    nentry: i64,
    term: Option<Vec<u8>>,
    data: Vec<u8>,
}

/// `SegmentWriter`.
struct SegWriter {
    nodes: Vec<TreeNode>,
    tree: Option<usize>,
    first: i64,
    free: i64,
    term: Vec<u8>,
    data: Vec<u8>,
    nleaf_data: i64,
}

impl SegWriter {
    fn new(ctx: &Ctx) -> Result<Self> {
        let sql = format!(
            "SELECT coalesce((SELECT max(blockid) FROM {}) + 1, 1)",
            ctx.tab.shadow("segments")
        );
        let free = ctx
            .sql()
            .row(&sql, &[])?
            .map(|r| value_int(&r[0]))
            .unwrap_or(1);
        Ok(Self {
            nodes: Vec::new(),
            tree: None,
            first: free,
            free,
            term: Vec::new(),
            data: Vec::new(),
            nleaf_data: 0,
        })
    }

    fn new_node(&mut self) -> usize {
        self.nodes.push(TreeNode {
            parent: None,
            right: None,
            leftmost: 0,
            nentry: 0,
            term: None,
            data: vec![0; 1 + VARINT_MAX],
        });
        self.nodes.len() - 1
    }

    /// `fts3NodeAddTerm`.
    fn node_add_term(
        &mut self,
        node_size: i32,
        tree: &mut Option<usize>,
        term: &[u8],
    ) -> Result<()> {
        if let Some(t) = *tree {
            let n = &self.nodes[t];
            let prev: &[u8] = n.term.as_deref().unwrap_or(&[]);
            let nprefix = prefix_compress(prev, term);
            let nsuffix = term.len() - nprefix;
            if nsuffix == 0 {
                return Err(corrupt());
            }
            let nreq = (n.data.len() - (1 + VARINT_MAX)) as i64
                + 1
                + VARINT_MAX as i64
                + varint_len(nprefix as u64) as i64
                + varint_len(nsuffix as u64) as i64
                + nsuffix as i64;
            if nreq <= node_size as i64 || n.term.is_none() {
                let has_term = n.term.is_some();
                let n = &mut self.nodes[t];
                if has_term {
                    put_varint(&mut n.data, nprefix as i64);
                }
                put_varint(&mut n.data, nsuffix as i64);
                n.data.extend_from_slice(&term[nprefix..]);
                n.nentry += 1;
                n.term = Some(term.to_vec());
                return Ok(());
            }
        }
        let new = self.new_node();
        if let Some(t) = *tree {
            let mut parent = self.nodes[t].parent;
            self.node_add_term(node_size, &mut parent, term)?;
            if self.nodes[t].parent.is_none() {
                self.nodes[t].parent = parent;
            }
            self.nodes[t].right = Some(new);
            self.nodes[new].leftmost = self.nodes[t].leftmost;
            self.nodes[new].parent = parent;
        } else {
            self.nodes[new].leftmost = new;
            let mut nt = Some(new);
            self.node_add_term(node_size, &mut nt, term)?;
        }
        *tree = Some(new);
        Ok(())
    }

    /// `fts3TreeFinishNode`: the node bytes with height and left child.
    fn finish_node(&mut self, n: usize, height: i64, left_child: i64) -> Vec<u8> {
        let node = &mut self.nodes[n];
        let start = VARINT_MAX - varint_len(left_child as u64);
        let mut head = vec![height as u8];
        put_varint(&mut head, left_child);
        node.data[start..start + head.len()].copy_from_slice(&head);
        node.data[start..].to_vec()
    }

    /// `fts3NodeWrite`: writes interior nodes; returns (last block, root).
    fn node_write(
        &mut self,
        ctx: &Ctx,
        tree: usize,
        height: i64,
        leaf: i64,
        free: i64,
    ) -> Result<(i64, Vec<u8>)> {
        match self.nodes[tree].parent {
            None => {
                let root = self.finish_node(tree, height, leaf);
                Ok((free - 1, root))
            }
            Some(parent) => {
                let mut next_free = free;
                let mut next_leaf = leaf;
                let mut it = Some(self.nodes[tree].leftmost);
                while let Some(n) = it {
                    let bytes = self.finish_node(n, height, next_leaf);
                    write_segment(ctx, next_free, Some(&bytes))?;
                    next_free += 1;
                    next_leaf += self.nodes[n].nentry + 1;
                    it = self.nodes[n].right;
                }
                self.node_write(ctx, parent, height + 1, free, next_free)
            }
        }
    }

    /// `fts3SegWriterAdd`.
    fn add(&mut self, ctx: &Ctx, node_size: i32, term: &[u8], doclist: &[u8]) -> Result<()> {
        let mut nprefix = prefix_compress(&self.term, term);
        let mut nsuffix = term.len() - nprefix;
        if nsuffix == 0 {
            return Err(corrupt());
        }
        let mut nreq = varint_len(nprefix as u64)
            + varint_len(nsuffix as u64)
            + nsuffix
            + varint_len(doclist.len() as u64)
            + doclist.len();
        if !self.data.is_empty() && self.data.len() + nreq > node_size as usize {
            if self.free == i64::MAX {
                return Err(corrupt());
            }
            let data = std::mem::take(&mut self.data);
            write_segment(ctx, self.free, Some(&data))?;
            self.free += 1;
            ctx.with_pending(|p| p.n_leaf_add += 1);
            let mut tree = self.tree;
            self.node_add_term(node_size, &mut tree, &term[..nprefix + 1])?;
            self.tree = tree;
            self.term.clear();
            nprefix = 0;
            nsuffix = term.len();
            nreq = 1
                + varint_len(term.len() as u64)
                + term.len()
                + varint_len(doclist.len() as u64)
                + doclist.len();
        }
        self.nleaf_data += nreq as i64;
        put_varint(&mut self.data, nprefix as i64);
        put_varint(&mut self.data, nsuffix as i64);
        self.data.extend_from_slice(&term[nprefix..]);
        put_varint(&mut self.data, doclist.len() as i64);
        self.data.extend_from_slice(doclist);
        self.term = term.to_vec();
        Ok(())
    }

    /// `fts3SegWriterFlush`.
    fn flush(&mut self, ctx: &Ctx, level: i64, idx: i64) -> Result<()> {
        if let Some(tree) = self.tree {
            let last_leaf = self.free;
            let data = std::mem::take(&mut self.data);
            write_segment(ctx, self.free, Some(&data))?;
            self.free += 1;
            let (last, root) = self.node_write(ctx, tree, 1, self.first, self.free)?;
            write_segdir(
                ctx,
                level,
                idx,
                self.first,
                last_leaf,
                last,
                self.nleaf_data,
                &root,
            )?;
        } else {
            write_segdir(ctx, level, idx, 0, 0, 0, self.nleaf_data, &self.data)?;
        }
        ctx.with_pending(|p| p.n_leaf_add += 1);
        Ok(())
    }
}

fn segment_max_level(ctx: &Ctx, langid: i32, index: usize) -> Result<i64> {
    let sql = format!(
        "SELECT max(level) FROM {} WHERE level BETWEEN ? AND ?",
        ctx.tab.shadow("segdir")
    );
    Ok(ctx
        .sql()
        .row(
            &sql,
            &[
                Value::from_i64(abs_level(ctx.tab, langid, index, 0)),
                Value::from_i64(abs_level(ctx.tab, langid, index, SEGDIR_MAXLEVEL - 1)),
            ],
        )?
        .map(|r| value_int(&r[0]))
        .unwrap_or(0))
}

fn segment_is_max_level(ctx: &Ctx, abs: i64) -> Result<bool> {
    let sql = format!(
        "SELECT max(level) FROM {} WHERE level BETWEEN ? AND ?",
        ctx.tab.shadow("segdir")
    );
    let hi = ((abs as u64 / SEGDIR_MAXLEVEL as u64) + 1) as i64 * SEGDIR_MAXLEVEL;
    Ok(ctx
        .sql()
        .row(&sql, &[Value::from_i64(abs + 1), Value::from_i64(hi)])?
        .is_some_and(|r| matches!(r[0], Value::Null)))
}

fn delete_segment(ctx: &Ctx, seg: &SegReader) -> Result<()> {
    if seg.start_block != 0 {
        let sql = format!(
            "DELETE FROM {} WHERE blockid BETWEEN ? AND ?",
            ctx.tab.shadow("segments")
        );
        ctx.sql().exec(
            &sql,
            &[
                Value::from_i64(seg.start_block),
                Value::from_i64(seg.end_block),
            ],
        )?;
    }
    Ok(())
}

fn delete_segdir(
    ctx: &Ctx,
    langid: i32,
    index: usize,
    level: i32,
    segs: &[SegReader],
) -> Result<()> {
    for s in segs {
        delete_segment(ctx, s)?;
    }
    let tab = ctx.tab;
    if level == CURSOR_ALL {
        let sql = format!(
            "DELETE FROM {} WHERE level BETWEEN ? AND ?",
            tab.shadow("segdir")
        );
        ctx.sql().exec(
            &sql,
            &[
                Value::from_i64(abs_level(tab, langid, index, 0)),
                Value::from_i64(abs_level(tab, langid, index, SEGDIR_MAXLEVEL - 1)),
            ],
        )
    } else {
        let sql = format!("DELETE FROM {} WHERE level = ?", tab.shadow("segdir"));
        ctx.sql().exec(
            &sql,
            &[Value::from_i64(abs_level(tab, langid, index, level as i64))],
        )
    }
}

/// `fts3ReadEndBlockField`: (end block, leaf data size).
fn read_end_block(v: &Value) -> (i64, i64) {
    let Some(t) = value_text(v) else {
        return (0, 0);
    };
    let mut i = 0;
    let mut val: u64 = 0;
    while i < t.len() && t[i].is_ascii_digit() {
        val = val.wrapping_mul(10).wrapping_add((t[i] - b'0') as u64);
        i += 1;
    }
    let end = val as i64;
    while i < t.len() && t[i] == b' ' {
        i += 1;
    }
    let mut mul: i64 = 1;
    if i < t.len() && t[i] == b'-' {
        i += 1;
        mul = -1;
    }
    let mut val: u64 = 0;
    while i < t.len() && t[i].is_ascii_digit() {
        val = val.wrapping_mul(10).wrapping_add((t[i] - b'0') as u64);
        i += 1;
    }
    if val as i64 == i64::MIN {
        mul = 1;
    }
    (end, (val as i64).wrapping_mul(mul))
}

/// `fts3PromoteSegments`.
fn promote_segments(ctx: &Ctx, abs: i64, nbyte: i64) -> Result<()> {
    let tab = ctx.tab;
    let last = (abs / SEGDIR_MAXLEVEL + 1) * SEGDIR_MAXLEVEL - 1;
    let limit = (nbyte * 3) / 2;
    let sql = format!(
        "SELECT level, idx, end_block FROM {} WHERE level BETWEEN ? AND ? \
         ORDER BY level DESC, idx ASC",
        tab.shadow("segdir")
    );
    let rows = ctx
        .sql()
        .rows(&sql, &[Value::from_i64(abs + 1), Value::from_i64(last)])?;
    let mut ok = false;
    for r in &rows {
        let (_, size) = read_end_block(&r[2]);
        if size <= 0 || size > limit {
            ok = false;
            break;
        }
        ok = true;
    }
    if ok {
        let rows = ctx
            .sql()
            .rows(&sql, &[Value::from_i64(abs), Value::from_i64(last)])?;
        let upd1 = format!(
            "UPDATE OR FAIL {} SET level=-1,idx=? WHERE level=? AND idx=?",
            tab.shadow("segdir")
        );
        for (i, r) in rows.iter().enumerate() {
            ctx.sql().exec(
                &upd1,
                &[
                    Value::from_i64(i as i64),
                    Value::from_i64(value_int(&r[0]) as i32 as i64),
                    Value::from_i64(value_int(&r[1]) as i32 as i64),
                ],
            )?;
        }
        let upd2 = format!(
            "UPDATE OR FAIL {} SET level=? WHERE level=-1",
            tab.shadow("segdir")
        );
        ctx.sql().exec(&upd2, &[Value::from_i64(abs)])?;
    }
    Ok(())
}

/// `fts3AllocateSegdirIdx`.
fn allocate_segdir_idx(ctx: &Ctx, langid: i32, index: usize, level: i64) -> Result<i64> {
    let sql = format!(
        "SELECT (SELECT max(idx) FROM {} WHERE level = ?) + 1",
        ctx.tab.shadow("segdir")
    );
    let next = ctx
        .sql()
        .row(
            &sql,
            &[Value::from_i64(abs_level(ctx.tab, langid, index, level))],
        )?
        .map(|r| value_int(&r[0]))
        .unwrap_or(0);
    if next >= MERGE_COUNT as i64 {
        segment_merge(ctx, langid, index, level as i32)?;
        Ok(0)
    } else {
        Ok(next)
    }
}

/// `fts3SegmentMerge`; returns false (SQLITE_DONE) when there was nothing
/// to do for an "all levels" merge.
fn segment_merge(ctx: &Ctx, langid: i32, index: usize, level: i32) -> Result<bool> {
    let tab = ctx.tab;
    let mut csr = MultiSegReader::default();
    seg_reader_cursor(ctx, langid, index, level, None, true, false, &mut csr)?;
    if csr.segs.is_empty() {
        return Ok(true);
    }
    let mut max_lvl = 0;
    if level != CURSOR_PENDING {
        max_lvl = segment_max_level(ctx, langid, index)?;
    }
    let new_level;
    let mut idx = 0;
    let ignore_empty;
    if level == CURSOR_ALL {
        if csr.segs.len() == 1 && !csr.segs[0].is_pending() {
            return Ok(false);
        }
        new_level = max_lvl;
        ignore_empty = true;
    } else {
        new_level = abs_level(tab, langid, index, level as i64 + 1);
        idx = allocate_segdir_idx(ctx, langid, index, level as i64 + 1)?;
        ignore_empty = level != CURSOR_PENDING && new_level > max_lvl;
    }
    let node_size = tab.node_size(ctx.conn)?;
    let flags = REQUIRE_POS | if ignore_empty { IGNORE_EMPTY } else { 0 };
    csr.start(
        ctx,
        SegFilter {
            term: None,
            icol: 0,
            flags,
        },
    )?;
    let mut writer: Option<SegWriter> = None;
    while csr.step(ctx)? {
        if writer.is_none() {
            writer = Some(SegWriter::new(ctx)?);
        }
        let w = writer.as_mut().unwrap();
        w.add(ctx, node_size, &csr.term, &csr.doclist)?;
    }
    if level != CURSOR_PENDING {
        delete_segdir(ctx, langid, index, level, &csr.segs)?;
    }
    if let Some(mut w) = writer {
        w.flush(ctx, new_level, idx)?;
        if level == CURSOR_PENDING || new_level < max_lvl {
            promote_segments(ctx, new_level, w.nleaf_data)?;
        }
    }
    Ok(true)
}

fn encode_ints(a: &[u32]) -> Vec<u8> {
    let mut out = Vec::new();
    for &x in a {
        put_varint(&mut out, x as i64);
    }
    out
}

fn decode_ints(n: usize, buf: &[u8]) -> Vec<u32> {
    let mut a = vec![0u32; n];
    if !buf.is_empty() && buf[buf.len() - 1] & 0x80 == 0 {
        let mut j = 0;
        let mut i = 0;
        while i < n && j < buf.len() {
            let (x, k) = get_varint(buf, j);
            j += k;
            a[i] = (x as u64 & 0xffff_ffff) as u32;
            i += 1;
        }
    }
    a
}

fn insert_docsize(ctx: &Ctx, sz: &[u32]) -> Result<()> {
    let docid = ctx.with_pending(|p| p.prev_docid);
    let b = encode_ints(&sz[..ctx.tab.ncol()]);
    let sql = format!("REPLACE INTO {} VALUES(?,?)", ctx.tab.shadow("docsize"));
    ctx.sql().exec(&sql, &[Value::from_i64(docid), blob(&b)])
}

/// `fts3UpdateDocTotals`.
fn update_doc_totals(ctx: &Ctx, ins: &[u32], del: &[u32], nchng: i64) -> Result<()> {
    let ncol = ctx.tab.ncol();
    let nstat = ncol + 2;
    let mut a = match select_stat(ctx, STAT_DOCTOTAL)? {
        Some(v) => decode_ints(nstat, &value_text(&v).unwrap_or_default()),
        None => vec![0; nstat],
    };
    if nchng < 0 && (a[0] as i64) < -nchng {
        a[0] = 0;
    } else {
        a[0] = (a[0] as i64 + nchng) as u32;
    }
    for i in 0..ncol + 1 {
        let x = a[i + 1];
        a[i + 1] = if x.wrapping_add(ins[i]) < del[i] {
            0
        } else {
            x.wrapping_add(ins[i]).wrapping_sub(del[i])
        };
    }
    replace_stat(ctx, STAT_DOCTOTAL, blob(&encode_ints(&a)))
}

fn langid_of(tab: &super::table::Fts3Table, row: &[Value]) -> i32 {
    if tab.langid.is_some() {
        row.get(tab.ncol() + 1).map(value_int).unwrap_or(0) as i32
    } else {
        0
    }
}

/// `fts3DeleteTerms`: queues delete entries for a row; false if not found.
fn delete_terms(ctx: &Ctx, rowid: &Value, sz: &mut [u32]) -> Result<bool> {
    let tab = ctx.tab;
    let sql = format!("{} WHERE rowid=?", tab.read_select()?);
    let Some(row) = ctx.sql().row(&sql, std::slice::from_ref(rowid))? else {
        return Ok(false);
    };
    let langid = langid_of(tab, &row);
    let docid = value_int(&row[0]);
    pending_docid(ctx, true, langid, docid)?;
    for (icol, v) in row.iter().enumerate().skip(1).take(tab.ncol()) {
        let icol = icol - 1;
        if !tab.notindexed[icol] {
            let text = value_text(v);
            sz[icol] += pending_add(ctx, langid, text.as_deref(), -1)?;
            sz[tab.ncol()] += text.map(|t| t.len()).unwrap_or(0) as u32;
        }
    }
    Ok(true)
}

fn delete_all(ctx: &Ctx, content: bool) -> Result<()> {
    pending_clear(ctx);
    let tab = ctx.tab;
    let sql = ctx.sql();
    if content {
        sql.exec(&format!("DELETE FROM {}", tab.shadow("content")), &[])?;
    }
    sql.exec(&format!("DELETE FROM {}", tab.shadow("segments")), &[])?;
    sql.exec(&format!("DELETE FROM {}", tab.shadow("segdir")), &[])?;
    if tab.has_docsize {
        sql.exec(&format!("DELETE FROM {}", tab.shadow("docsize")), &[])?;
    }
    if tab.has_stat.load(Ordering::Relaxed) == 1 {
        sql.exec(&format!("DELETE FROM {}", tab.shadow("stat")), &[])?;
    }
    Ok(())
}

/// `fts3DeleteByRowid`. Returns true when deleting the last row reset the
/// whole index, which also zeroes the insert sizes (SQLite's memset over
/// both arrays).
fn delete_by_rowid(ctx: &Ctx, rowid: &Value, nchng: &mut i64, del: &mut [u32]) -> Result<bool> {
    let tab = ctx.tab;
    if !delete_terms(ctx, rowid, del)? {
        return Ok(false);
    }
    let empty = if tab.content.is_some() {
        false
    } else {
        let sql = format!(
            "SELECT NOT EXISTS(SELECT docid FROM {} WHERE rowid!=?)",
            tab.shadow("content")
        );
        ctx.sql()
            .row(&sql, std::slice::from_ref(rowid))?
            .is_some_and(|r| value_int(&r[0]) != 0)
    };
    if empty {
        delete_all(ctx, true)?;
        *nchng = 0;
        for x in del.iter_mut() {
            *x = 0;
        }
        return Ok(true);
    }
    *nchng -= 1;
    if tab.content.is_none() {
        let sql = format!("DELETE FROM {} WHERE rowid = ?", tab.shadow("content"));
        ctx.sql().exec(&sql, std::slice::from_ref(rowid))?;
    }
    if tab.has_docsize {
        let sql = format!("DELETE FROM {} WHERE docid = ?", tab.shadow("docsize"));
        ctx.sql().exec(&sql, std::slice::from_ref(rowid))?;
    }
    Ok(false)
}

/// `fts3InsertData`: inserts the content row; returns the docid.
fn insert_data(ctx: &Ctx, args: &[Value]) -> Result<i64> {
    let tab = ctx.tab;
    let ncol = tab.ncol();
    if tab.content.is_some() {
        let mut rowid = &args[ncol + 3];
        if matches!(rowid, Value::Null) {
            rowid = &args[1];
        }
        return match rowid.as_int() {
            Some(i) => Ok(i),
            None => Err(LimboError::Constraint("constraint failed".into())),
        };
    }
    let mut params: Vec<Value> = Vec::with_capacity(ncol + 2);
    params.push(args[1].clone());
    for i in 0..ncol {
        params.push(args[2 + i].clone());
    }
    if tab.langid.is_some() {
        params.push(Value::from_i64(value_int(&args[ncol + 4]) as i32 as i64));
    }
    if !matches!(args[3 + ncol], Value::Null) {
        if matches!(args[0], Value::Null) && !matches!(args[1], Value::Null) {
            return Err(sql_error("SQL logic error"));
        }
        params[0] = args[3 + ncol].clone();
    }
    let mut vals = String::from("?");
    for _ in 0..ncol {
        match &tab.compress {
            Some(f) => vals.push_str(&format!(",{}(?)", quote_id(f))),
            None => vals.push_str(",?"),
        }
    }
    if tab.langid.is_some() {
        vals.push_str(", ?");
    }
    let sql = format!("INSERT INTO {} VALUES({vals})", tab.shadow("content"));
    match ctx.sql().exec(&sql, &params) {
        Err(LimboError::Constraint(_) | LimboError::ConstraintViolation(_)) => {
            return Err(LimboError::Constraint("constraint failed".into()))
        }
        r => r?,
    }
    Ok(ctx.conn.last_insert_rowid())
}

/// `sqlite3Fts3UpdateMethod`. `args` is [old rowid, new rowid, columns...,
/// table, docid, langid], or [rowid, NULL] for a DELETE.
pub(crate) fn update(ctx: &Ctx, args: &[Value], on_conflict: u8) -> Result<Option<i64>> {
    let tab = ctx.tab;
    let ncol = tab.ncol();
    let full = args.len() >= ncol + 5;
    if full && matches!(args[0], Value::Null) && !matches!(args[ncol + 2], Value::Null) {
        special_insert(ctx, &args[ncol + 2])?;
        return Ok(None);
    }
    if full && value_int(&args[ncol + 4]) < 0 {
        return Err(LimboError::Constraint("constraint failed".into()));
    }
    let mut del = vec![0u32; ncol + 1];
    let mut ins = vec![0u32; ncol + 1];
    let mut nchng: i64 = 0;
    let mut rowid: Option<i64> = None;
    let mut insert_done = false;
    let mut reset = false;
    if full && tab.content.is_none() {
        let mut new_rowid = &args[3 + ncol];
        if matches!(new_rowid, Value::Null) {
            new_rowid = &args[1];
        }
        if !matches!(new_rowid, Value::Null)
            && (matches!(args[0], Value::Null) || value_int(&args[0]) != value_int(new_rowid))
        {
            // ResolveType::Replace
            if on_conflict == 5 {
                reset |= delete_by_rowid(ctx, new_rowid, &mut nchng, &mut del)?;
            } else {
                rowid = Some(insert_data(ctx, args)?);
                insert_done = true;
            }
        }
    }
    if !matches!(args[0], Value::Null) {
        reset |= delete_by_rowid(ctx, &args[0], &mut nchng, &mut del)?;
    }
    if reset {
        ins.iter_mut().for_each(|x| *x = 0);
    }
    if full {
        let langid = value_int(&args[ncol + 4]) as i32;
        if !insert_done {
            match insert_data(ctx, args) {
                Ok(r) => rowid = Some(r),
                Err(LimboError::Constraint(_)) if tab.content.is_none() => return Err(corrupt()),
                Err(e) => return Err(e),
            }
        }
        let docid = rowid.unwrap_or(0);
        pending_docid(ctx, false, langid, docid)?;
        for i in 0..ncol {
            if !tab.notindexed[i] {
                let text = value_text(&args[2 + i]);
                ins[i] += pending_add(ctx, langid, text.as_deref(), i as i32)?;
                ins[ncol] += text.map(|t| t.len()).unwrap_or(0) as u32;
            }
        }
        if tab.has_docsize {
            insert_docsize(ctx, &ins)?;
        }
        nchng += 1;
    }
    if tab.fts4 {
        update_doc_totals(ctx, &ins, &del, nchng)?;
    }
    Ok(rowid)
}

/// `fts3SpecialInsert`.
fn special_insert(ctx: &Ctx, v: &Value) -> Result<()> {
    let z = value_text(v).unwrap_or_default();
    let lower = z.to_ascii_lowercase();
    let err = || sql_error("SQL logic error");
    if lower == b"optimize" {
        do_optimize(ctx)?;
    } else if lower == b"rebuild" {
        do_rebuild(ctx)?;
    } else if lower == b"integrity-check" {
        if !integrity_check(ctx)? {
            return Err(corrupt());
        }
    } else if z.len() > 6 && lower.starts_with(b"merge=") {
        do_incrmerge(ctx, &z[6..])?;
    } else if z.len() > 10 && lower.starts_with(b"automerge=") {
        do_autoincrmerge(ctx, &z[10..])?;
    } else if lower == b"flush" {
        pending_flush(ctx)?;
    } else {
        return Err(err());
    }
    Ok(())
}

/// `fts3DoOptimize`; returns false when every index was already optimal.
fn do_optimize(ctx: &Ctx) -> Result<bool> {
    pending_flush(ctx)?;
    let mut seen_done = false;
    for langid in all_langids(ctx)? {
        for i in 0..ctx.tab.nindex() {
            if !segment_merge(ctx, langid, i, CURSOR_ALL)? {
                seen_done = true;
            }
        }
    }
    Ok(!seen_done)
}

/// optimize(): true for "Index optimized".
pub(crate) fn optimize_fn(ctx: &Ctx) -> Result<bool> {
    do_optimize(ctx)
}

fn all_langids(ctx: &Ctx) -> Result<Vec<i32>> {
    let sql = format!(
        "SELECT ? UNION SELECT level / (1024 * ?) FROM {}",
        ctx.tab.shadow("segdir")
    );
    let rows = ctx.sql().rows(
        &sql,
        &[
            Value::from_i64(ctx.prev_langid() as i64),
            Value::from_i64(ctx.tab.nindex() as i64),
        ],
    )?;
    Ok(rows.iter().map(|r| value_int(&r[0]) as i32).collect())
}

/// `fts3DoRebuild`.
fn do_rebuild(ctx: &Ctx) -> Result<()> {
    let tab = ctx.tab;
    delete_all(ctx, false)?;
    let ncol = tab.ncol();
    let mut ins_tot = vec![0u32; ncol + 1];
    let del = vec![0u32; ncol + 1];
    let mut nentry: i64 = 0;
    let rows = ctx.sql().rows(&tab.read_select()?, &[])?;
    for row in rows {
        let langid = langid_of(tab, &row);
        pending_docid(ctx, false, langid, value_int(&row[0]))?;
        let mut sz = vec![0u32; ncol + 1];
        for icol in 0..ncol {
            if !tab.notindexed[icol] {
                let text = value_text(&row[icol + 1]);
                sz[icol] += pending_add(ctx, langid, text.as_deref(), icol as i32)?;
                sz[ncol] += text.map(|t| t.len()).unwrap_or(0) as u32;
            }
        }
        if tab.has_docsize {
            insert_docsize(ctx, &sz)?;
        }
        nentry += 1;
        for i in 0..=ncol {
            ins_tot[i] += sz[i];
        }
    }
    if tab.fts4 {
        update_doc_totals(ctx, &ins_tot, &del, nentry)?;
    }
    Ok(())
}

fn checksum_entry(term: &[u8], langid: i32, index: usize, docid: i64, col: i32, pos: i32) -> u64 {
    let mut ret = docid as u64;
    ret = ret.wrapping_add((ret << 3).wrapping_add(langid as i64 as u64));
    ret = ret.wrapping_add((ret << 3).wrapping_add(index as u64));
    ret = ret.wrapping_add((ret << 3).wrapping_add(col as i64 as u64));
    ret = ret.wrapping_add((ret << 3).wrapping_add(pos as i64 as u64));
    for &c in term {
        // zTerm is `const char *`: bytes are sign-extended.
        ret = ret.wrapping_add((ret << 3).wrapping_add(c as i8 as i64 as u64));
    }
    ret
}

fn checksum_index(ctx: &Ctx, langid: i32, index: usize) -> Result<u64> {
    let mut csr = MultiSegReader::default();
    seg_reader_cursor(ctx, langid, index, CURSOR_ALL, None, false, true, &mut csr)?;
    csr.start(
        ctx,
        SegFilter {
            term: None,
            icol: 0,
            flags: REQUIRE_POS | IGNORE_EMPTY | SCAN,
        },
    )?;
    let mut cksum = 0u64;
    while csr.step(ctx)? {
        let d = &csr.doclist;
        let (mut docid, mut p) = get_varint(d, 0);
        let mut col = 0i32;
        let mut pos: u64 = 0;
        while p < d.len() {
            let (v, n) = get_varint(d, p);
            p += n;
            let v = v as u64;
            if p < d.len() {
                if v == 0 || v == 1 {
                    col = 0;
                    pos = 0;
                    if v == 1 {
                        let (c, n) = get_varint32(d, p);
                        p += n;
                        col = c;
                    } else {
                        let (dv, n) = get_varint(d, p);
                        p += n;
                        if ctx.tab.desc {
                            docid = (docid as u64).wrapping_sub(dv as u64) as i64;
                        } else {
                            docid = (docid as u64).wrapping_add(dv as u64) as i64;
                        }
                    }
                } else {
                    pos = pos.wrapping_add(v - 2);
                    if pos > 0x7fff_ffff {
                        return Err(corrupt());
                    }
                    cksum ^= checksum_entry(&csr.term, langid, index, docid, col, pos as i32);
                }
            }
        }
    }
    Ok(cksum)
}

/// `sqlite3Fts3IntegrityCheck`: whether the index matches the content.
pub(crate) fn integrity_check(ctx: &Ctx) -> Result<bool> {
    let tab = ctx.tab;
    let mut c1 = 0u64;
    let mut c2 = 0u64;
    for langid in all_langids(ctx)? {
        for i in 0..tab.nindex() {
            match checksum_index(ctx, langid, i) {
                Ok(c) => c1 ^= c,
                Err(LimboError::Corrupt(_)) => return Ok(false),
                Err(e) => return Err(e),
            }
        }
    }
    if tab.contentless() {
        return Ok(true);
    }
    let tok = tab.tokenizer(Some(ctx.conn))?;
    let rows = ctx.sql().rows(&tab.read_select()?, &[])?;
    for row in rows {
        let docid = value_int(&row[0]);
        let lang = langid_of(tab, &row);
        for icol in 0..tab.ncol() {
            if tab.notindexed[icol] {
                continue;
            }
            let Some(text) = value_text(&row[icol + 1]) else {
                continue;
            };
            tok.tokenize(lang, &text, &mut |t, _, _, pos| {
                c2 ^= checksum_entry(t, lang, 0, docid, icol as i32, pos);
                for (i, &np) in tab.prefixes.iter().enumerate().skip(1) {
                    if np as usize <= t.len() {
                        c2 ^= checksum_entry(&t[..np as usize], lang, i, docid, icol as i32, pos);
                    }
                }
                Ok(true)
            })?;
        }
    }
    Ok(c1 == c2)
}

fn getint(z: &[u8], i: &mut usize) -> i64 {
    let mut v: i64 = 0;
    while *i < z.len() && z[*i].is_ascii_digit() && v < 214_748_363 {
        v = 10 * v + (z[*i] - b'0') as i64;
        *i += 1;
    }
    v
}

/// `fts3DoIncrmerge`: "merge=X,Y".
fn do_incrmerge(ctx: &Ctx, z: &[u8]) -> Result<()> {
    let mut i = 0;
    let mut nmin = (MERGE_COUNT / 2) as i64;
    let nmerge = getint(z, &mut i);
    if i < z.len() && z[i] == b',' && i + 1 < z.len() {
        i += 1;
        nmin = getint(z, &mut i);
    }
    if i < z.len() || nmin < 2 {
        return Err(sql_error("SQL logic error"));
    }
    if ctx.tab.has_stat.load(Ordering::Relaxed) != 1 {
        create_stat_table_nested(ctx)?;
    }
    incrmerge(ctx, nmerge, nmin)
}

/// `fts3DoAutoincrmerge`: "automerge=N".
fn do_autoincrmerge(ctx: &Ctx, z: &[u8]) -> Result<()> {
    let mut i = 0;
    let mut n = getint(z, &mut i) as i32;
    if n == 1 || n > MERGE_COUNT {
        n = 8;
    }
    ctx.with_pending(|p| p.autoincrmerge = n);
    if ctx.tab.has_stat.load(Ordering::Relaxed) != 1 {
        create_stat_table_nested(ctx)?;
    }
    replace_stat(ctx, STAT_AUTOINCRMERGE, Value::from_i64(n as i64))
}

// ---- Incremental merge (sqlite3Fts3Incrmerge) ----

#[derive(Default)]
struct NodeWriter {
    block: i64,
    key: Vec<u8>,
    data: Vec<u8>,
}

#[derive(Default)]
struct IncrWriter {
    nleaf_est: i64,
    nwork: i64,
    abs_level: i64,
    idx: i64,
    start: i64,
    end: i64,
    nleaf_data: i64,
    no_leaf_data: bool,
    nodes: Vec<NodeWriter>,
}

/// `NodeReader`.
struct NodeReader<'a> {
    node: &'a [u8],
    off: usize,
    child: i64,
    term: Vec<u8>,
    doclist: Option<(usize, usize)>,
    done: bool,
}

impl<'a> NodeReader<'a> {
    fn new(node: &'a [u8]) -> Result<Self> {
        let mut r = NodeReader {
            node,
            off: 1,
            child: 0,
            term: Vec::new(),
            doclist: None,
            done: false,
        };
        if !node.is_empty() && node[0] != 0 {
            let (c, n) = get_varint(node, 1);
            r.child = c;
            r.off = 1 + n;
        }
        r.next()?;
        Ok(r)
    }

    fn next(&mut self) -> Result<()> {
        let first = self.term.is_empty();
        if self.child != 0 && !first {
            self.child += 1;
        }
        if self.off >= self.node.len() {
            self.done = true;
            return Ok(());
        }
        let mut nprefix = 0usize;
        if !first {
            let (v, n) = get_varint32(self.node, self.off);
            self.off += n;
            nprefix = v as usize;
        }
        let (v, n) = get_varint32(self.node, self.off);
        self.off += n;
        let nsuffix = v as usize;
        if nprefix > self.term.len() || nsuffix > self.node.len() - self.off || nsuffix == 0 {
            return Err(corrupt());
        }
        self.term.truncate(nprefix);
        self.term
            .extend_from_slice(&self.node[self.off..self.off + nsuffix]);
        self.off += nsuffix;
        if self.child == 0 {
            let (nd, n) = get_varint32(self.node, self.off);
            self.off += n;
            let nd = nd as usize;
            if self.node.len() - self.off < nd {
                return Err(corrupt());
            }
            self.doclist = Some((self.off, nd));
            self.off += nd;
        }
        Ok(())
    }

    fn doclist(&self) -> Option<&'a [u8]> {
        self.doclist.map(|(o, n)| &self.node[o..o + n])
    }
}

/// `fts3IncrmergePush`.
fn incr_push(ctx: &Ctx, w: &mut IncrWriter, node_size: i32, term: &[u8]) -> Result<()> {
    let mut ptr = w.nodes[0].block;
    for layer in 1..MAX_APPENDABLE_HEIGHT {
        let node = &mut w.nodes[layer];
        let nprefix = prefix_compress(&node.key, term);
        let nsuffix = term.len() - nprefix;
        if nsuffix == 0 {
            return Err(corrupt());
        }
        let nspace = varint_len(nprefix as u64) + varint_len(nsuffix as u64) + nsuffix;
        let mut next_ptr = 0;
        if node.key.is_empty() || node.data.len() + nspace <= node_size as usize {
            if node.data.is_empty() {
                node.data.push(layer as u8);
                put_varint(&mut node.data, ptr);
            }
            if !node.key.is_empty() {
                put_varint(&mut node.data, nprefix as i64);
            }
            put_varint(&mut node.data, nsuffix as i64);
            node.data.extend_from_slice(&term[nprefix..]);
            node.key = term.to_vec();
        } else {
            write_segment(ctx, node.block, Some(&node.data))?;
            node.data.clear();
            node.data.push(layer as u8);
            put_varint(&mut node.data, ptr + 1);
            next_ptr = node.block;
            node.block += 1;
            node.key.clear();
        }
        if next_ptr == 0 {
            return Ok(());
        }
        ptr = next_ptr;
    }
    Err(corrupt())
}

/// `fts3AppendToNode`.
fn append_to_node(
    node: &mut Vec<u8>,
    prev: &mut Vec<u8>,
    term: &[u8],
    doclist: Option<&[u8]>,
) -> Result<()> {
    let first = prev.is_empty();
    let nprefix = prefix_compress(prev, term);
    let nsuffix = term.len() - nprefix;
    if nsuffix == 0 {
        return Err(corrupt());
    }
    *prev = term.to_vec();
    if !first {
        put_varint(node, nprefix as i64);
    }
    put_varint(node, nsuffix as i64);
    node.extend_from_slice(&term[nprefix..]);
    if let Some(d) = doclist {
        put_varint(node, d.len() as i64);
        node.extend_from_slice(d);
    }
    Ok(())
}

/// `fts3IncrmergeAppend`.
fn incr_append(
    ctx: &Ctx,
    w: &mut IncrWriter,
    node_size: i32,
    term: &[u8],
    doclist: &[u8],
) -> Result<()> {
    let leaf = &w.nodes[0];
    let nprefix = prefix_compress(&leaf.key, term);
    let mut nsuffix = term.len() - nprefix;
    if nsuffix == 0 {
        return Err(corrupt());
    }
    let mut nspace = varint_len(nprefix as u64)
        + varint_len(nsuffix as u64)
        + nsuffix
        + varint_len(doclist.len() as u64)
        + doclist.len();
    if !leaf.data.is_empty()
        && leaf.data.len() + nspace > node_size as usize
        && leaf.block < w.start + w.nleaf_est
    {
        let data = std::mem::take(&mut w.nodes[0].data);
        write_segment(ctx, w.nodes[0].block, Some(&data))?;
        w.nwork += 1;
        incr_push(ctx, w, node_size, &term[..nprefix + 1])?;
        let leaf = &mut w.nodes[0];
        leaf.block += 1;
        leaf.key.clear();
        leaf.data.clear();
        nsuffix = term.len();
        nspace = 1
            + varint_len(nsuffix as u64)
            + nsuffix
            + varint_len(doclist.len() as u64)
            + doclist.len();
    }
    let _ = nsuffix;
    w.nleaf_data += nspace as i64;
    let leaf = &mut w.nodes[0];
    if leaf.data.is_empty() {
        leaf.data.push(0);
    }
    append_to_node(&mut leaf.data, &mut leaf.key, term, Some(doclist))
}

/// `fts3IncrmergeRelease`.
fn incr_release(ctx: &Ctx, w: &mut IncrWriter) -> Result<()> {
    let mut iroot: isize = MAX_APPENDABLE_HEIGHT as isize - 1;
    while iroot >= 0 && w.nodes[iroot as usize].data.is_empty() {
        iroot -= 1;
    }
    if iroot < 0 {
        return Ok(());
    }
    if iroot == 0 {
        let b = w.nodes[0].block;
        let n1 = &mut w.nodes[1];
        n1.data.clear();
        n1.data.push(1);
        put_varint(&mut n1.data, b);
        iroot = 1;
    }
    let iroot = iroot as usize;
    for i in 0..iroot {
        if !w.nodes[i].data.is_empty() {
            write_segment(ctx, w.nodes[i].block, Some(&w.nodes[i].data))?;
        }
    }
    let nld = if w.no_leaf_data { 0 } else { w.nleaf_data };
    write_segdir(
        ctx,
        w.abs_level + 1,
        w.idx,
        w.start,
        w.nodes[0].block,
        w.end,
        nld,
        &w.nodes[iroot].data,
    )
}

fn term_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.cmp(b)
}

/// `fts3IncrmergeLoad`: reopens an appendable output segment.
fn incr_load(ctx: &Ctx, abs: i64, idx: i64, key: &[u8], w: &mut IncrWriter) -> Result<()> {
    let sql = format!(
        "SELECT idx, start_block, leaves_end_block, end_block, root FROM {} \
         WHERE level = ? AND idx = ?",
        ctx.tab.shadow("segdir")
    );
    let Some(row) = ctx
        .sql()
        .row(&sql, &[Value::from_i64(abs + 1), Value::from_i64(idx)])?
    else {
        return Ok(());
    };
    let start = value_int(&row[1]);
    let leaf_end = value_int(&row[2]);
    let (end, mut nld) = read_end_block(&row[3]);
    if nld < 0 {
        nld = -nld;
    }
    w.nleaf_data = nld;
    w.no_leaf_data = nld == 0;
    let root = match &row[4] {
        Value::Null => return Err(corrupt()),
        v => value_text(v).unwrap_or_default(),
    };
    let check = format!(
        "SELECT 1 FROM {} WHERE blockid=? AND block IS NULL",
        ctx.tab.shadow("segments")
    );
    let mut appendable = ctx.sql().row(&check, &[Value::from_i64(end)])?.is_some();
    if appendable {
        let leaf = ctx.read_block(leaf_end)?;
        let mut r = NodeReader::new(&leaf)?;
        while !r.done {
            r.next()?;
        }
        if term_cmp(key, &r.term) != std::cmp::Ordering::Greater {
            appendable = false;
        }
    }
    if appendable {
        let height = root.first().copied().unwrap_or(0) as usize;
        if !(1..MAX_APPENDABLE_HEIGHT).contains(&height) {
            return Err(corrupt());
        }
        w.nleaf_est = ((end - start) + 1) / MAX_APPENDABLE_HEIGHT as i64;
        w.start = start;
        w.end = end;
        w.abs_level = abs;
        w.idx = idx;
        for i in height + 1..MAX_APPENDABLE_HEIGHT {
            w.nodes[i].block = w.start + i as i64 * w.nleaf_est;
        }
        w.nodes[height].block = w.start + w.nleaf_est * height as i64;
        w.nodes[height].data.clone_from(&root);
        let mut i = height as isize;
        while i >= 0 {
            let iu = i as usize;
            if !w.nodes[iu].data.is_empty() {
                let data = w.nodes[iu].data.clone();
                let mut r = NodeReader::new(&data)?;
                while !r.done {
                    r.next()?;
                }
                w.nodes[iu].key.clone_from(&r.term);
                if iu > 0 {
                    let child = r.child;
                    let block = ctx.read_block(child)?;
                    w.nodes[iu - 1].block = child;
                    w.nodes[iu - 1].data = block;
                }
            }
            i -= 1;
        }
    }
    Ok(())
}

/// `fts3IncrmergeWriter`: starts a new output segment.
fn incr_writer(ctx: &Ctx, abs: i64, idx: i64, nseg: usize, w: &mut IncrWriter) -> Result<()> {
    let sql = format!(
        "SELECT 2 * total(1 + leaves_end_block - start_block) FROM \
         (SELECT * FROM {} WHERE level = ? ORDER BY idx ASC LIMIT ?)",
        ctx.tab.shadow("segdir")
    );
    let est = ctx
        .sql()
        .row(&sql, &[Value::from_i64(abs), Value::from_i64(nseg as i64)])?
        .map(|r| value_int(&r[0]))
        .unwrap_or(0);
    let sql = format!(
        "SELECT coalesce((SELECT max(blockid) FROM {}) + 1, 1)",
        ctx.tab.shadow("segments")
    );
    if let Some(r) = ctx.sql().row(&sql, &[])? {
        w.start = value_int(&r[0]);
        w.end = w.start - 1 + est * MAX_APPENDABLE_HEIGHT as i64;
    }
    write_segment(ctx, w.end, None)?;
    w.abs_level = abs;
    w.nleaf_est = est;
    w.idx = idx;
    for i in 0..MAX_APPENDABLE_HEIGHT {
        w.nodes[i].block = w.start + i as i64 * w.nleaf_est;
    }
    Ok(())
}

/// `fts3TruncateNode`: drops the terms of a node that sort before `term`.
fn truncate_node(node: &[u8], term: &[u8]) -> Result<(Vec<u8>, i64)> {
    if node.is_empty() {
        return Err(corrupt());
    }
    let leaf = node[0] == 0;
    let mut out = Vec::new();
    let mut prev = Vec::new();
    let mut block = 0;
    let mut r = NodeReader::new(node)?;
    while !r.done {
        if out.is_empty() {
            let res = term_cmp(&r.term, term);
            if res == std::cmp::Ordering::Less || (!leaf && res == std::cmp::Ordering::Equal) {
                r.next()?;
                continue;
            }
            start_node(&mut out, node[0], r.child);
            block = r.child;
        }
        append_to_node(&mut out, &mut prev, &r.term.clone(), r.doclist())?;
        r.next()?;
    }
    if out.is_empty() {
        start_node(&mut out, node[0], r.child);
        block = r.child;
    }
    Ok((out, block))
}

fn start_node(out: &mut Vec<u8>, height: u8, child: i64) {
    out.clear();
    out.push(height);
    if child != 0 {
        put_varint(out, child);
    }
}

/// `fts3TruncateSegment`.
fn truncate_segment(ctx: &Ctx, abs: i64, idx: i64, term: &[u8]) -> Result<()> {
    let tab = ctx.tab;
    let sql = format!(
        "SELECT idx, start_block, leaves_end_block, end_block, root FROM {} \
         WHERE level = ? AND idx = ?",
        tab.shadow("segdir")
    );
    let mut root = Vec::new();
    let mut block = 0;
    let mut old_start = 0;
    if let Some(row) = ctx
        .sql()
        .row(&sql, &[Value::from_i64(abs), Value::from_i64(idx)])?
    {
        old_start = value_int(&row[1]);
        let r = value_text(&row[4]).unwrap_or_default();
        (root, block) = truncate_node(&r, term)?;
    }
    let mut new_start = 0;
    while block != 0 {
        new_start = block;
        let b = ctx.read_block(block)?;
        let (nb, next) = truncate_node(&b, term)?;
        block = next;
        write_segment(ctx, new_start, Some(&nb))?;
    }
    if new_start != 0 {
        let sql = format!(
            "DELETE FROM {} WHERE blockid BETWEEN ? AND ?",
            tab.shadow("segments")
        );
        ctx.sql().exec(
            &sql,
            &[Value::from_i64(old_start), Value::from_i64(new_start - 1)],
        )?;
    }
    let sql = format!(
        "UPDATE {} SET start_block = ?, root = ? WHERE level = ? AND idx = ?",
        tab.shadow("segdir")
    );
    ctx.sql().exec(
        &sql,
        &[
            Value::from_i64(new_start),
            blob(&root),
            Value::from_i64(abs),
            Value::from_i64(idx),
        ],
    )
}

/// `fts3IncrmergeChomp`: removes merged input; returns segments remaining.
fn incr_chomp(ctx: &Ctx, abs: i64, csr: &MultiSegReader) -> Result<usize> {
    let tab = ctx.tab;
    let mut nrem = 0;
    let n = csr.segs.len();
    for i in (0..n).rev() {
        let seg = csr
            .segs
            .iter()
            .find(|s| s.idx == i as i32)
            .ok_or_else(corrupt)?;
        if seg.at_eof() {
            delete_segment(ctx, seg)?;
            let sql = format!(
                "DELETE FROM {} WHERE level = ? AND idx = ?",
                tab.shadow("segdir")
            );
            ctx.sql().exec(
                &sql,
                &[Value::from_i64(abs), Value::from_i64(seg.idx as i64)],
            )?;
        } else {
            truncate_segment(ctx, abs, seg.idx as i64, &seg.term)?;
            nrem += 1;
        }
    }
    if nrem != n {
        // fts3RepackSegdirLevel
        let sql = format!(
            "SELECT idx FROM {} WHERE level=? ORDER BY 1 ASC",
            tab.shadow("segdir")
        );
        let idxs: Vec<i64> = ctx
            .sql()
            .rows(&sql, &[Value::from_i64(abs)])?
            .iter()
            .map(|r| value_int(&r[0]))
            .collect();
        let upd = format!(
            "UPDATE {} SET idx = ? WHERE level=? AND idx=?",
            tab.shadow("segdir")
        );
        ctx.with_pending(|p| p.ignore_savepoint = true);
        let mut r = Ok(());
        for (i, &ix) in idxs.iter().enumerate() {
            if ix != i as i64 {
                r = ctx.sql().exec(
                    &upd,
                    &[
                        Value::from_i64(i as i64),
                        Value::from_i64(abs),
                        Value::from_i64(ix),
                    ],
                );
                if r.is_err() {
                    break;
                }
            }
        }
        ctx.with_pending(|p| p.ignore_savepoint = false);
        r?;
    }
    Ok(nrem)
}

fn hint_pop(hint: &mut Vec<u8>) -> Result<(i64, i64)> {
    let n = hint.len();
    if n == 0 || hint[n - 1] & 0x80 != 0 {
        return Err(corrupt());
    }
    let mut i = n - 1;
    while i > 0 && hint[i - 1] & 0x80 != 0 {
        i -= 1;
    }
    if i == 0 {
        return Err(corrupt());
    }
    i -= 1;
    while i > 0 && hint[i - 1] & 0x80 != 0 {
        i -= 1;
    }
    let start = i;
    let (abs, k) = get_varint(hint, i);
    i += k;
    let (ninput, k) = get_varint32(hint, i);
    i += k;
    if i != n {
        return Err(corrupt());
    }
    hint.truncate(start);
    Ok((abs, ninput as i64))
}

/// `sqlite3Fts3Incrmerge`.
pub(crate) fn incrmerge(ctx: &Ctx, nmerge: i64, nmin: i64) -> Result<()> {
    let tab = ctx.tab;
    let node_size = tab.node_size(ctx.conn)?;
    let mut nrem = nmerge;
    let mut hint: Vec<u8> = match select_stat(ctx, STAT_INCRMERGEHINT)? {
        Some(Value::Null) | None => Vec::new(),
        Some(v) => value_text(&v).unwrap_or_default(),
    };
    let mut dirty_hint = false;
    while nrem > 0 {
        let nmod = SEGDIR_MAXLEVEL * tab.nindex() as i64;
        let sql = format!(
            "SELECT level, count(*) AS cnt FROM {} GROUP BY level HAVING cnt>=? \
             ORDER BY (level % 1024) ASC, 2 DESC LIMIT 1",
            tab.shadow("segdir")
        );
        let (mut abs, mut nseg) = match ctx.sql().row(&sql, &[Value::from_i64(nmin.max(2))])? {
            Some(r) => (value_int(&r[0]), value_int(&r[1])),
            None => (0, -1),
        };
        let mut use_hint = false;
        if !hint.is_empty() {
            let saved = hint.clone();
            let (habs, hseg) = hint_pop(&mut hint)?;
            if nseg < 0 || (abs % nmod) >= (habs % nmod) {
                abs = habs;
                nseg = nmin.max(nseg).min(hseg);
                use_hint = true;
                dirty_hint = true;
            } else {
                hint = saved;
            }
        }
        if nseg <= 0 {
            break;
        }
        if abs < 0 || abs > (nmod << 32) {
            return Err(corrupt());
        }
        let mut w = IncrWriter {
            nodes: (0..MAX_APPENDABLE_HEIGHT)
                .map(|_| NodeWriter::default())
                .collect(),
            ..Default::default()
        };
        let mut flags = REQUIRE_POS;
        let sql = format!(
            "SELECT (SELECT max(idx) FROM {} WHERE level = ?) + 1",
            tab.shadow("segdir")
        );
        let idx = ctx
            .sql()
            .row(&sql, &[Value::from_i64(abs + 1)])?
            .map(|r| value_int(&r[0]))
            .unwrap_or(0);
        if (idx == 0 || (use_hint && idx == 1)) && segment_is_max_level(ctx, abs + 1)? {
            flags |= IGNORE_EMPTY;
        }
        // fts3IncrmergeCsr
        let mut csr = MultiSegReader::default();
        let sql = format!(
            "SELECT idx, start_block, leaves_end_block, end_block, root FROM {} \
             WHERE level = ? ORDER BY idx ASC",
            tab.shadow("segdir")
        );
        let rows = ctx.sql().rows(&sql, &[Value::from_i64(abs)])?;
        for (i, r) in rows.iter().enumerate() {
            if i as i64 >= nseg {
                break;
            }
            csr.segs.push(SegReader::new(
                i as i32,
                false,
                value_int(&r[1]),
                value_int(&r[2]),
                value_int(&r[3]),
                &value_text(&r[4]).unwrap_or_default(),
            )?);
        }
        if csr.segs.len() as i64 != nseg {
            continue;
        }
        csr.start(
            ctx,
            SegFilter {
                term: None,
                icol: 0,
                flags,
            },
        )?;
        let mut has_row = csr.step(ctx)?;
        let empty = !has_row;
        if use_hint && idx > 0 {
            let key = csr.term.clone();
            incr_load(ctx, abs, idx - 1, &key, &mut w)?;
        } else {
            incr_writer(ctx, abs, idx, csr.segs.len(), &mut w)?;
        }
        let mut nseg_left = nseg as usize;
        if w.nleaf_est != 0 {
            if !empty {
                loop {
                    incr_append(
                        ctx,
                        &mut w,
                        node_size,
                        &csr.term.clone(),
                        &csr.doclist.clone(),
                    )?;
                    has_row = csr.step(ctx)?;
                    if w.nwork >= nrem && has_row {
                        break;
                    }
                    if !has_row {
                        break;
                    }
                }
            }
            nrem -= 1 + w.nwork;
            nseg_left = incr_chomp(ctx, abs, &csr)?;
            if nseg_left != 0 {
                dirty_hint = true;
                put_varint(&mut hint, abs);
                put_varint(&mut hint, nseg_left as i64);
            }
        }
        if nseg_left != 0 {
            w.nleaf_data = -w.nleaf_data;
        }
        incr_release(ctx, &mut w)?;
        if nseg_left == 0 && !w.no_leaf_data {
            promote_segments(ctx, abs + 1, w.nleaf_data)?;
        }
    }
    if dirty_hint {
        replace_stat(ctx, STAT_INCRMERGEHINT, blob(&hint))?;
    }
    Ok(())
}
