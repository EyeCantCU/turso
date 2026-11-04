//! The FTS5 index stored in the %_data and %_idx tables (fts5_index.c):
//! the structure record, segment leaves, doclist indexes, tombstone hashes,
//! segment writing, merging and the integrity check.

use super::config::{ConfigValues, Detail, Fts5Config, CURRENT_VERSION_SECUREDELETE, MAX_SEGMENT};
use super::hash::Hash;
use super::sql::{self, corrupt, corrupt_msg};
use super::varint::*;
use crate::sync::Arc;
use crate::{Connection, Result, Value};

pub(crate) const AVERAGES_ROWID: i64 = 1;
pub(crate) const STRUCTURE_ROWID: i64 = 10;
const DATA_DLI_B: i64 = 1;
const DATA_HEIGHT_B: i64 = 5;
const DATA_PAGE_B: i64 = 31;
const MAX_PGNO: i64 = 0x7FFF_FF00;
const MIN_DLIDX_SIZE: i32 = 4;
const WORK_UNIT: i64 = 64;
const OPT_WORK_UNIT: i32 = 1000;
const MAX_LEVEL: usize = 64;
const STRUCTURE_V2: [u8; 4] = [0xff, 0x00, 0x00, 0x01];
pub(crate) const MAIN_PREFIX: u8 = b'0';

fn dri(segid: i64, dlidx: i64, height: i64, pgno: i64) -> i64 {
    (segid << (DATA_PAGE_B + DATA_HEIGHT_B + DATA_DLI_B))
        + (dlidx << (DATA_PAGE_B + DATA_HEIGHT_B))
        + (height << DATA_PAGE_B)
        + pgno
}

pub(crate) fn segment_rowid(segid: i32, pgno: i32) -> i64 {
    dri(segid as i64, 0, 0, pgno as i64)
}

fn dlidx_rowid(segid: i32, height: i32, pgno: i32) -> i64 {
    dri(segid as i64, 1, height as i64, pgno as i64)
}

fn tombstone_rowid(segid: i32, ipg: i32) -> i64 {
    dri(segid as i64 + (1 << 16), 0, 0, ipg as i64)
}

// ------------------------------------------------------------------ structure

#[derive(Debug, Clone, Default)]
pub(crate) struct Segment {
    pub segid: i32,
    pub pgno_first: i32,
    pub pgno_last: i32,
    pub origin1: u64,
    pub origin2: u64,
    pub npg_tombstone: i32,
    pub nentry_tombstone: u64,
    pub nentry: u64,
}

impl Segment {
    fn size(&self) -> i32 {
        1 + self.pgno_last - self.pgno_first
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Level {
    pub nmerge: usize,
    pub segs: Vec<Segment>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Structure {
    pub write_counter: u64,
    pub origin_cntr: u64,
    pub levels: Vec<Level>,
}

impl Structure {
    pub(crate) fn nsegment(&self) -> usize {
        self.levels.iter().map(|l| l.segs.len()).sum()
    }

    /// Decodes a structure record; returns it with its cookie.
    pub(crate) fn decode(data: &[u8]) -> Option<(Structure, i32)> {
        let cookie = get_u32(data, 0) as i32;
        let mut i = 4;
        let v2 = data.len() >= 8 && data[4..8] == STRUCTURE_V2;
        if v2 {
            i += 4;
        }
        let nlevel = get_varint32(data, &mut i) as i64;
        let mut nsegment = get_varint32(data, &mut i) as i64;
        if !(0..=MAX_SEGMENT as i64).contains(&nlevel)
            || !(0..=MAX_SEGMENT as i64).contains(&nsegment)
        {
            return None;
        }
        let mut s = Structure {
            write_counter: get_varint(data, &mut i),
            ..Default::default()
        };
        let mut origin_max = 0u64;
        for ilvl in 0..nlevel {
            if i >= data.len() {
                return None;
            }
            let nmerge = get_varint32(data, &mut i) as usize;
            let ntotal = get_varint32(data, &mut i) as usize;
            if ntotal < nmerge {
                return None;
            }
            nsegment -= ntotal as i64;
            let mut segs = Vec::with_capacity(ntotal.min(MAX_SEGMENT));
            for _ in 0..ntotal {
                if i >= data.len() {
                    return None;
                }
                let mut seg = Segment {
                    segid: get_varint32(data, &mut i) as i32,
                    pgno_first: get_varint32(data, &mut i) as i32,
                    pgno_last: get_varint32(data, &mut i) as i32,
                    ..Default::default()
                };
                if v2 {
                    seg.origin1 = get_varint(data, &mut i);
                    seg.origin2 = get_varint(data, &mut i);
                    seg.npg_tombstone = get_varint32(data, &mut i) as i32;
                    seg.nentry_tombstone = get_varint(data, &mut i);
                    seg.nentry = get_varint(data, &mut i);
                    origin_max = origin_max.max(seg.origin2);
                }
                if seg.pgno_last < seg.pgno_first || seg.pgno_last as i64 > MAX_PGNO {
                    return None;
                }
                segs.push(seg);
            }
            if ilvl > 0 && s.levels[ilvl as usize - 1].nmerge != 0 && ntotal == 0 {
                return None;
            }
            if ilvl == nlevel - 1 && nmerge != 0 {
                return None;
            }
            s.levels.push(Level { nmerge, segs });
        }
        if nsegment != 0 {
            return None;
        }
        if v2 {
            s.origin_cntr = origin_max + 1;
        }
        Some((s, cookie))
    }

    pub(crate) fn encode(&self, cookie: i32) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(cookie.max(0) as u32).to_be_bytes());
        let v2 = self.origin_cntr > 0;
        if v2 {
            buf.extend_from_slice(&STRUCTURE_V2);
        }
        put_varint(&mut buf, self.levels.len() as u64);
        put_varint(&mut buf, self.nsegment() as u64);
        put_varint(&mut buf, self.write_counter);
        for lvl in &self.levels {
            put_varint(&mut buf, lvl.nmerge as u64);
            put_varint(&mut buf, lvl.segs.len() as u64);
            for seg in &lvl.segs {
                put_varint(&mut buf, seg.segid as u64);
                put_varint(&mut buf, seg.pgno_first as u64);
                put_varint(&mut buf, seg.pgno_last as u64);
                if v2 {
                    put_varint(&mut buf, seg.origin1);
                    put_varint(&mut buf, seg.origin2);
                    put_varint(&mut buf, seg.npg_tombstone as u64);
                    put_varint(&mut buf, seg.nentry_tombstone);
                    put_varint(&mut buf, seg.nentry);
                }
            }
        }
        buf
    }

    fn allocate_segid(&self) -> Result<i32> {
        if self.nsegment() >= MAX_SEGMENT {
            return Err(crate::LimboError::DatabaseFull);
        }
        let mut used = vec![false; MAX_SEGMENT + 2];
        for lvl in &self.levels {
            for seg in &lvl.segs {
                if seg.segid > 0 && seg.segid as usize <= MAX_SEGMENT {
                    used[seg.segid as usize] = true;
                }
            }
        }
        Ok((1..).find(|&i| !used[i]).unwrap() as i32)
    }

    /// fts5StructurePromoteTo
    fn promote_to(&mut self, ipromote: usize, szpromote: i32) {
        if self.levels[ipromote].nmerge != 0 {
            return;
        }
        for il in ipromote + 1..self.levels.len() {
            if self.levels[il].nmerge != 0 {
                return;
            }
            while let Some(seg) = self.levels[il].segs.last() {
                if seg.size() > szpromote {
                    return;
                }
                let seg = self.levels[il].segs.pop().unwrap();
                self.levels[ipromote].segs.insert(0, seg);
            }
        }
    }

    /// fts5StructurePromote
    fn promote(&mut self, ilvl: usize) {
        let Some(seg) = self.levels[ilvl].segs.last() else {
            return;
        };
        let szseg = seg.size();
        let mut ipromote = None;
        let mut szpromote = 0;
        let mut itst = ilvl as i64 - 1;
        while itst >= 0 && self.levels[itst as usize].segs.is_empty() {
            itst -= 1;
        }
        if itst >= 0 {
            let szmax = self.levels[itst as usize]
                .segs
                .iter()
                .map(|s| s.size())
                .max()
                .unwrap_or(0);
            if szmax >= szseg {
                ipromote = Some(itst as usize);
                szpromote = szmax;
            }
        }
        let (ipromote, szpromote) = match ipromote {
            Some(i) => (i, szpromote),
            None => (ilvl, szseg),
        };
        self.promote_to(ipromote, szpromote);
    }
}

// ------------------------------------------------------------------ data access

/// %_data / %_idx access on one connection.
pub(crate) struct Store<'a> {
    pub conn: &'a Arc<Connection>,
    data: String,
    idx: String,
}

impl<'a> Store<'a> {
    pub(crate) fn new(conn: &'a Arc<Connection>, cfg: &Fts5Config) -> Self {
        Self {
            conn,
            data: cfg.shadow("data"),
            idx: cfg.shadow("idx"),
        }
    }

    pub(crate) fn read(&self, id: i64) -> Result<Option<Vec<u8>>> {
        let row = sql::query_row(
            self.conn,
            &format!("SELECT block FROM {} WHERE id=?", self.data),
            &[sql::int(id)],
        )?;
        Ok(row.map(|r| sql::value_bytes(&r[0])))
    }

    /// Reads a block that must exist.
    pub(crate) fn read_req(&self, id: i64) -> Result<Vec<u8>> {
        self.read(id)?.ok_or_else(corrupt)
    }

    pub(crate) fn write(&self, id: i64, data: &[u8]) -> Result<()> {
        sql::exec(
            self.conn,
            &format!("REPLACE INTO {}(id, block) VALUES(?,?)", self.data),
            &[sql::int(id), sql::blob(data)],
        )
    }

    fn delete_range(&self, first: i64, last: i64) -> Result<()> {
        sql::exec(
            self.conn,
            &format!("DELETE FROM {} WHERE id>=? AND id<=?", self.data),
            &[sql::int(first), sql::int(last)],
        )
    }

    fn idx_insert(&self, segid: i32, term: &[u8], pgno: i64) -> Result<()> {
        sql::exec(
            self.conn,
            &format!("INSERT INTO {}(segid,term,pgno) VALUES(?,?,?)", self.idx),
            &[sql::int(segid as i64), sql::blob(term), sql::int(pgno)],
        )
    }

    /// The (page, has-dlidx) of the leaf that may contain `term`.
    fn idx_select(&self, segid: i32, term: &[u8]) -> Result<Option<(i32, bool)>> {
        let row = sql::query_row(
            self.conn,
            &format!(
                "SELECT pgno FROM {} WHERE segid=? AND term<=? ORDER BY term DESC LIMIT 1",
                self.idx
            ),
            &[sql::int(segid as i64), sql::blob(term)],
        )?;
        Ok(row.map(|r| {
            let v = sql::value_i64(&r[0]);
            ((v >> 1) as i32, v & 1 != 0)
        }))
    }

    fn remove_segment(&self, seg: &Segment) -> Result<()> {
        self.delete_range(
            segment_rowid(seg.segid, 0),
            segment_rowid(seg.segid + 1, 0) - 1,
        )?;
        if seg.npg_tombstone > 0 {
            self.delete_range(
                tombstone_rowid(seg.segid, 0),
                tombstone_rowid(seg.segid, seg.npg_tombstone - 1),
            )?;
        }
        sql::exec(
            self.conn,
            &format!("DELETE FROM {} WHERE segid=?", self.idx),
            &[sql::int(seg.segid as i64)],
        )
    }

    pub(crate) fn read_structure(&self) -> Result<(Structure, i32)> {
        let data = self.read_req(STRUCTURE_ROWID)?;
        Structure::decode(&data).ok_or_else(corrupt)
    }

    pub(crate) fn write_structure(&self, s: &Structure, cookie: i32) -> Result<()> {
        self.write(STRUCTURE_ROWID, &s.encode(cookie))
    }
}

/// A leaf page, with `szleaf` from its header.
struct Leaf {
    p: Vec<u8>,
    szleaf: usize,
}

impl Leaf {
    fn nn(&self) -> usize {
        self.p.len()
    }
    fn first_rowid_off(&self) -> usize {
        get_u16(&self.p, 0)
    }
    fn is_termless(&self) -> bool {
        self.szleaf >= self.nn()
    }
    fn first_term_off(&self) -> usize {
        let mut off = self.szleaf;
        get_varint32(&self.p, &mut off) as usize
    }
}

/// Writes a varint at `a[off..]`, growing `a` if needed; returns its length.
fn put_varint_at(a: &mut Vec<u8>, off: usize, v: u64) -> usize {
    let mut tmp = Vec::with_capacity(9);
    put_varint(&mut tmp, v);
    if a.len() < off + tmp.len() {
        a.resize(off + tmp.len(), 0);
    }
    a[off..off + tmp.len()].copy_from_slice(&tmp);
    tmp.len()
}

fn set_byte(a: &mut Vec<u8>, off: usize, b: u8) {
    if a.len() <= off {
        a.resize(off + 1, 0);
    }
    a[off] = b;
}

fn read_leaf(store: &Store, id: i64) -> Result<Leaf> {
    let p = store.read_req(id)?;
    let szleaf = if p.len() >= 4 { get_u16(&p, 2) } else { 0 };
    if szleaf < 4 || szleaf > p.len() {
        return Err(corrupt());
    }
    Ok(Leaf { p, szleaf })
}

// ------------------------------------------------------------------ segment iterator

/// Forward iterator over the entries of one segment (Fts5SegIter).
struct SegIter<'s> {
    seg: Segment,
    detail: Detail,
    one_term: bool,
    leaf: Option<Leaf>,
    leaf_pgno: i32,
    leaf_offset: usize,
    term_leaf_pgno: i32,
    term_leaf_offset: usize,
    pgidx_off: usize,
    end_of_doclist: usize,
    term: Vec<u8>,
    rowid: i64,
    npos: usize,
    del: bool,
    tombstones: Vec<Option<Vec<u8>>>,
    store: &'s Store<'s>,
}

impl<'s> SegIter<'s> {
    fn empty(store: &'s Store<'s>, seg: &Segment, detail: Detail) -> Self {
        Self {
            seg: seg.clone(),
            detail,
            one_term: false,
            leaf: None,
            leaf_pgno: 0,
            leaf_offset: 0,
            term_leaf_pgno: 0,
            term_leaf_offset: 0,
            pgidx_off: 0,
            end_of_doclist: 0,
            term: Vec::new(),
            rowid: 0,
            npos: 0,
            del: false,
            tombstones: vec![None; seg.npg_tombstone.max(0) as usize],
            store,
        }
    }

    fn eof(&self) -> bool {
        self.leaf.is_none()
    }

    fn leaf(&self) -> &Leaf {
        self.leaf.as_ref().expect("leaf")
    }

    /// fts5SegIterInit
    fn init(store: &'s Store<'s>, seg: &Segment, detail: Detail) -> Result<Self> {
        let mut it = Self::empty(store, seg, detail);
        if seg.pgno_first == 0 {
            return Ok(it);
        }
        it.leaf_pgno = seg.pgno_first - 1;
        loop {
            it.next_page()?;
            match &it.leaf {
                Some(l) if l.nn() == 4 => continue,
                _ => break,
            }
        }
        if it.leaf.is_some() {
            it.leaf_offset = 4;
            it.pgidx_off = it.leaf().szleaf + 1;
            it.load_term(0)?;
            it.load_npos();
        }
        Ok(it)
    }

    /// fts5SegIterSeekInit: positions on `term` (or, with `ge`, the first
    /// term >= it). With `one_term` the iterator stops at the end of that doclist.
    fn seek(
        store: &'s Store<'s>,
        seg: &Segment,
        detail: Detail,
        term: &[u8],
        ge: bool,
    ) -> Result<Self> {
        let mut it = Self::empty(store, seg, detail);
        if seg.pgno_first == 0 {
            return Ok(it);
        }
        let mut pg = 1;
        if let Some((p, _)) = store.idx_select(seg.segid, term)? {
            pg = p;
        }
        if pg < seg.pgno_first {
            pg = seg.pgno_first;
        }
        it.leaf_pgno = pg - 1;
        it.next_page()?;
        if it.leaf.is_some() {
            it.leaf_seek(ge, term)?;
        }
        if !ge {
            it.one_term = true;
        }
        Ok(it)
    }

    /// fts5SegIterNextPage
    fn next_page(&mut self) -> Result<()> {
        self.leaf_pgno += 1;
        if self.leaf_pgno <= self.seg.pgno_last {
            let leaf = read_leaf(self.store, segment_rowid(self.seg.segid, self.leaf_pgno))?;
            self.pgidx_off = leaf.szleaf;
            if leaf.is_termless() {
                self.end_of_doclist = leaf.nn() + 1;
            } else {
                self.end_of_doclist = get_varint32(&leaf.p, &mut self.pgidx_off) as usize;
            }
            self.leaf = Some(leaf);
        } else {
            self.leaf = None;
        }
        Ok(())
    }

    /// fts5LeafSeek
    fn leaf_seek(&mut self, ge: bool, term: &[u8]) -> Result<()> {
        let mut a = &self.leaf().p;
        let mut n = a.len();
        let mut ipgidx = self.leaf().szleaf;
        let mut iterm_off = get_varint32(a, &mut ipgidx) as usize;
        let mut ioff = iterm_off;
        if ioff > n {
            return Err(corrupt());
        }
        let mut nmatch = 0usize;
        let mut nkeep = 0usize;
        let mut nnew;
        let mut end_of_page = false;
        let success;
        loop {
            nnew = get_varint32(a, &mut ioff) as usize;
            if nkeep < nmatch {
                success = false;
                break;
            }
            if ioff + nnew > n {
                return Err(corrupt());
            }
            if nkeep == nmatch {
                let ncmp = nnew.min(term.len() - nmatch);
                let mut i = 0;
                while i < ncmp && a[ioff + i] == term[nmatch + i] {
                    i += 1;
                }
                nmatch += i;
                if term.len() == nmatch {
                    success = i == nnew;
                    break;
                } else if i < nnew && a[ioff + i] > term[nmatch] {
                    success = false;
                    break;
                }
            }
            if ipgidx >= n {
                end_of_page = true;
                success = false;
                break;
            }
            nkeep = get_varint32(a, &mut ipgidx) as usize;
            iterm_off += nkeep;
            ioff = iterm_off;
            if ioff >= n {
                return Err(corrupt());
            }
            nkeep = get_varint32(a, &mut ioff) as usize;
        }
        if !success {
            if !ge {
                self.leaf = None;
                return Ok(());
            } else if end_of_page {
                loop {
                    self.next_page()?;
                    let Some(leaf) = &self.leaf else {
                        return Ok(());
                    };
                    if !leaf.is_termless() {
                        a = &self.leaf().p;
                        ipgidx = self.leaf().szleaf;
                        ioff = get_varint32(a, &mut ipgidx) as usize;
                        if ioff < 4 || ioff >= self.leaf().szleaf {
                            return Err(corrupt());
                        }
                        nkeep = 0;
                        iterm_off = ioff;
                        n = a.len();
                        nnew = get_varint32(a, &mut ioff) as usize;
                        break;
                    }
                }
            }
        }
        if ioff + nnew > n || nnew < 1 {
            return Err(corrupt());
        }
        let mut t = term[..nkeep.min(term.len())].to_vec();
        let extra = {
            let a = &self.leaf().p;
            t.extend_from_slice(&a[ioff..ioff + nnew]);
            (ipgidx < n).then(|| get_varint32(a, &mut ipgidx) as usize)
        };
        self.leaf_offset = ioff + nnew;
        self.term_leaf_offset = self.leaf_offset;
        self.term_leaf_pgno = self.leaf_pgno;
        self.term = t;
        self.end_of_doclist = match extra {
            None => n + 1,
            Some(extra) => iterm_off + extra,
        };
        self.pgidx_off = ipgidx;
        self.load_rowid()?;
        self.load_npos();
        Ok(())
    }

    /// fts5SegIterLoadNPos
    fn load_npos(&mut self) {
        let Some(leaf) = &self.leaf else { return };
        let mut ioff = self.leaf_offset;
        if self.detail == Detail::None {
            let eod = self.end_of_doclist.min(leaf.szleaf);
            self.del = false;
            self.npos = 1;
            if ioff < eod && leaf.p[ioff] == 0 {
                self.del = true;
                ioff += 1;
                if ioff < eod && leaf.p[ioff] == 0 {
                    self.npos = 1;
                    ioff += 1;
                } else {
                    self.npos = 0;
                }
            }
        } else {
            let sz = get_varint32(&leaf.p, &mut ioff) as usize;
            self.del = sz & 1 != 0;
            self.npos = sz >> 1;
        }
        self.leaf_offset = ioff;
    }

    /// fts5SegIterLoadRowid
    fn load_rowid(&mut self) -> Result<()> {
        let mut ioff = self.leaf_offset;
        while ioff >= self.leaf().szleaf {
            self.next_page()?;
            if self.leaf.is_none() {
                return Err(corrupt());
            }
            ioff = 4;
        }
        self.rowid = get_varint(&self.leaf().p, &mut ioff) as i64;
        self.leaf_offset = ioff;
        Ok(())
    }

    /// fts5SegIterLoadTerm
    fn load_term(&mut self, nkeep: usize) -> Result<()> {
        let leaf = self.leaf.as_ref().expect("leaf");
        let a = &leaf.p;
        let mut ioff = self.leaf_offset;
        let nnew = get_varint32(a, &mut ioff) as usize;
        if ioff + nnew > leaf.szleaf || nkeep > self.term.len() || nnew == 0 {
            return Err(corrupt());
        }
        self.term.truncate(nkeep);
        self.term.extend_from_slice(&a[ioff..ioff + nnew]);
        ioff += nnew;
        self.term_leaf_offset = ioff;
        self.term_leaf_pgno = self.leaf_pgno;
        self.leaf_offset = ioff;
        if self.pgidx_off >= leaf.nn() {
            self.end_of_doclist = leaf.nn() + 1;
        } else {
            let extra = get_varint32(a, &mut self.pgidx_off) as usize;
            self.end_of_doclist += extra;
        }
        self.load_rowid()
    }

    /// Advances to the next entry. Returns true when the term changed.
    fn next(&mut self) -> Result<bool> {
        if self.detail == Detail::None {
            self.next_none()
        } else {
            self.next_full()
        }
    }

    /// fts5SegIterNext
    fn next_full(&mut self) -> Result<bool> {
        let mut new_term = false;
        let mut nkeep = 0usize;
        let leaf = self.leaf();
        let n = leaf.szleaf;
        let mut ioff = self.leaf_offset + self.npos;
        if ioff < n {
            if ioff >= self.end_of_doclist {
                new_term = true;
                if ioff != leaf.first_term_off() {
                    nkeep = get_varint32(&leaf.p, &mut ioff) as usize;
                }
            } else {
                let delta = get_varint(&leaf.p, &mut ioff);
                self.rowid = self.rowid.wrapping_add(delta as i64);
            }
            self.leaf_offset = ioff;
        } else {
            ioff = 0;
            while ioff == 0 {
                self.next_page()?;
                let Some(leaf) = &self.leaf else { break };
                let roff = leaf.first_rowid_off();
                if roff != 0 && roff < leaf.szleaf {
                    let mut o = roff;
                    self.rowid = get_varint(&leaf.p, &mut o) as i64;
                    self.leaf_offset = o;
                    ioff = o;
                    if leaf.nn() > leaf.szleaf {
                        let mut po = leaf.szleaf;
                        self.end_of_doclist = get_varint32(&leaf.p, &mut po) as usize;
                        self.pgidx_off = po;
                    }
                } else if leaf.nn() > leaf.szleaf {
                    let mut po = leaf.szleaf;
                    let o = get_varint32(&leaf.p, &mut po) as usize;
                    self.pgidx_off = po;
                    self.leaf_offset = o;
                    self.end_of_doclist = o;
                    new_term = true;
                    ioff = o;
                }
                if ioff > self.leaf().szleaf {
                    return Err(corrupt());
                }
            }
        }
        if self.leaf.is_some() {
            if new_term {
                if self.one_term {
                    self.leaf = None;
                } else {
                    self.load_term(nkeep)?;
                    self.load_npos();
                    return Ok(true);
                }
            } else {
                let leaf = self.leaf();
                let mut o = self.leaf_offset;
                let sz = get_varint32(&leaf.p, &mut o) as usize;
                self.leaf_offset = o;
                self.del = sz & 1 != 0;
                self.npos = sz >> 1;
            }
        }
        Ok(false)
    }

    /// fts5SegIterNext_None
    fn next_none(&mut self) -> Result<bool> {
        let mut ioff = self.leaf_offset;
        while ioff >= self.leaf().szleaf {
            self.next_page()?;
            if self.leaf.is_none() {
                return Ok(false);
            }
            self.rowid = 0;
            ioff = 4;
        }
        let mut new_term = false;
        if ioff < self.end_of_doclist {
            let delta = get_varint(&self.leaf().p, &mut ioff);
            self.leaf_offset = ioff;
            self.rowid = self.rowid.wrapping_add(delta as i64);
        } else if !self.one_term {
            let mut nkeep = 0;
            if ioff != self.leaf().first_term_off() {
                nkeep = get_varint32(&self.leaf().p, &mut ioff) as usize;
            }
            self.leaf_offset = ioff;
            self.load_term(nkeep)?;
            new_term = true;
        } else {
            self.leaf = None;
            return Ok(false);
        }
        self.load_npos();
        Ok(new_term)
    }

    /// The position list of the current entry (fts5ChunkIterate).
    fn poslist(&self) -> Result<Vec<u8>> {
        if self.detail == Detail::None {
            return Ok(Vec::new());
        }
        let leaf = self.leaf();
        let mut rem = self.npos;
        let take = rem.min(leaf.szleaf.saturating_sub(self.leaf_offset));
        let mut out = leaf.p[self.leaf_offset..self.leaf_offset + take].to_vec();
        rem -= take;
        let mut pgno = self.leaf_pgno;
        while rem > 0 {
            pgno += 1;
            let data = read_leaf(self.store, segment_rowid(self.seg.segid, pgno))?;
            let take = rem.min(data.szleaf - 4);
            out.extend_from_slice(&data.p[4..4 + take]);
            rem -= take;
            if take == 0 {
                return Err(corrupt());
            }
        }
        Ok(out)
    }

    /// Whether the current rowid has a tombstone in this segment.
    fn is_deleted(&mut self) -> Result<bool> {
        let n = self.tombstones.len();
        if n == 0 || self.leaf.is_none() {
            return Ok(false);
        }
        let rowid = self.rowid as u64;
        let ipg = (rowid % n as u64) as usize;
        if self.tombstones[ipg].is_none() {
            self.tombstones[ipg] = self
                .store
                .read(tombstone_rowid(self.seg.segid, ipg as i32))?;
            if self.tombstones[ipg].is_none() {
                return Ok(false);
            }
        }
        Ok(tombstone_query(
            self.tombstones[ipg].as_ref().unwrap(),
            n,
            rowid,
        ))
    }
}

fn tombstone_query(page: &[u8], npg: usize, rowid: u64) -> bool {
    let szkey = page.first().copied().unwrap_or(4) as usize;
    let szkey = if szkey == 8 { 8 } else { 4 };
    let nslot = page.len().saturating_sub(8) / szkey;
    if rowid == 0 {
        return page.get(1).copied().unwrap_or(0) != 0;
    }
    if nslot == 0 {
        return false;
    }
    let mut islot = ((rowid / npg as u64) % nslot as u64) as usize;
    let mut ncollide = nslot;
    loop {
        let v = if szkey == 4 {
            get_u32(page, 8 + islot * 4) as u64
        } else {
            get_u64(page, 8 + islot * 8)
        };
        if v == 0 {
            return false;
        }
        if v == rowid {
            return true;
        }
        if ncollide == 0 {
            return false;
        }
        ncollide -= 1;
        islot = (islot + 1) % nslot;
    }
}

// ------------------------------------------------------------------ multi iterator

/// One entry from a merge of several segments (and the pending hash).
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub term: Vec<u8>,
    pub rowid: i64,
    #[allow(dead_code)]
    pub del: bool,
    pub npos: usize,
    pub poslist: Vec<u8>,
}

/// Entries of the pending hash, in (term, rowid) order.
struct HashSource {
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    i: usize,
    off: usize,
    rowid: i64,
    cur: Option<Entry>,
    detail: Detail,
}

impl HashSource {
    fn new(entries: Vec<(Vec<u8>, Vec<u8>)>, detail: Detail) -> Self {
        let mut h = HashSource {
            entries,
            i: 0,
            off: 0,
            rowid: 0,
            cur: None,
            detail,
        };
        h.load();
        h
    }

    fn load(&mut self) {
        while let Some((term, dl)) = self.entries.get(self.i) {
            if self.off >= dl.len() {
                self.i += 1;
                self.off = 0;
                self.rowid = 0;
                continue;
            }
            let off = &mut self.off;
            self.rowid = self.rowid.wrapping_add(get_varint(dl, off) as i64);
            let (del, npos, poslist) = if self.detail == Detail::None {
                let mut del = false;
                let mut npos = 1;
                if *off < dl.len() && dl[*off] == 0 {
                    del = true;
                    *off += 1;
                    if *off < dl.len() && dl[*off] == 0 {
                        *off += 1;
                    } else {
                        npos = 0;
                    }
                }
                (del, npos, Vec::new())
            } else {
                let sz = get_varint32(dl, off) as usize;
                let n = (sz >> 1).min(dl.len() - *off);
                let p = dl[*off..*off + n].to_vec();
                *off += n;
                (sz & 1 != 0, n, p)
            };
            self.cur = Some(Entry {
                term: term.clone(),
                rowid: self.rowid,
                del,
                npos,
                poslist,
            });
            return;
        }
        self.cur = None;
    }
}

/// A source of entries in (term, rowid) order.
enum Source<'s> {
    Seg(SegIter<'s>),
    Hash(HashSource),
}

impl<'s> Source<'s> {
    fn key(&self) -> Option<(&[u8], i64)> {
        match self {
            Source::Seg(it) => (!it.eof()).then_some((it.term.as_slice(), it.rowid)),
            Source::Hash(h) => h.cur.as_ref().map(|e| (e.term.as_slice(), e.rowid)),
        }
    }

    fn advance(&mut self) -> Result<()> {
        match self {
            Source::Seg(it) => {
                it.next()?;
                Ok(())
            }
            Source::Hash(h) => {
                h.load();
                Ok(())
            }
        }
    }

    fn entry(&self) -> Result<Entry> {
        match self {
            Source::Seg(it) => Ok(Entry {
                term: it.term.clone(),
                rowid: it.rowid,
                del: it.del,
                npos: it.npos,
                poslist: it.poslist()?,
            }),
            Source::Hash(h) => Ok(h.cur.clone().expect("hash entry")),
        }
    }

    fn is_deleted(&mut self) -> Result<bool> {
        match self {
            Source::Seg(it) => it.is_deleted(),
            Source::Hash(_) => Ok(false),
        }
    }
}

/// Merges sources; sources earlier in the list are newer and win ties.
pub(crate) struct MultiIter<'s> {
    sources: Vec<Source<'s>>,
    skip_empty: bool,
}

impl<'s> MultiIter<'s> {
    /// The index of the source holding the next entry.
    fn first(&self) -> Option<usize> {
        let mut best: Option<(usize, (&[u8], i64))> = None;
        for (i, s) in self.sources.iter().enumerate() {
            if let Some(k) = s.key() {
                match &best {
                    Some((_, bk)) if k >= *bk => {}
                    _ => best = Some((i, k)),
                }
            }
        }
        best.map(|(i, _)| i)
    }

    /// The next live entry, or None at the end.
    pub(crate) fn next(&mut self) -> Result<Option<Entry>> {
        loop {
            let Some(i) = self.first() else {
                return Ok(None);
            };
            let entry = self.sources[i].entry()?;
            let deleted = self.sources[i].is_deleted()?;
            // Skip older copies of the same (term, rowid).
            for s in self.sources.iter_mut() {
                while let Some((t, r)) = s.key() {
                    if t == entry.term.as_slice() && r == entry.rowid {
                        s.advance()?;
                    } else {
                        break;
                    }
                }
            }
            if deleted || (self.skip_empty && entry.npos == 0) {
                continue;
            }
            return Ok(Some(entry));
        }
    }
}

// ------------------------------------------------------------------ writer

#[derive(Default)]
struct DlidxWriter {
    pgno: i32,
    prev_valid: bool,
    prev: i64,
    buf: Vec<u8>,
}

/// Writes one segment (Fts5SegWriter).
struct SegWriter {
    segid: i32,
    pgsz: usize,
    pgno: i32,
    prev_pgidx: usize,
    buf: Vec<u8>,
    pgidx: Vec<u8>,
    term: Vec<u8>,
    prev_rowid: i64,
    first_rowid_in_doclist: bool,
    first_rowid_in_page: bool,
    first_term_in_page: bool,
    nleaf_written: i32,
    nempty: i32,
    dlidx: Vec<DlidxWriter>,
    btterm: Vec<u8>,
    bt_page: i32,
}

fn prefix_compress(old: &[u8], new: &[u8]) -> usize {
    old.iter().zip(new).take_while(|(a, b)| a == b).count()
}

impl SegWriter {
    fn new(segid: i32, pgsz: i32) -> Self {
        let mut w = SegWriter {
            segid,
            pgsz: pgsz as usize,
            pgno: 1,
            prev_pgidx: 0,
            buf: vec![0; 4],
            pgidx: Vec::new(),
            term: Vec::new(),
            prev_rowid: 0,
            first_rowid_in_doclist: false,
            first_rowid_in_page: false,
            first_term_in_page: true,
            nleaf_written: 0,
            nempty: 0,
            dlidx: Vec::new(),
            btterm: Vec::new(),
            bt_page: 1,
        };
        w.dlidx.push(DlidxWriter::default());
        w
    }

    fn dlidx_clear(&mut self, store: &Store, flush: bool) -> Result<()> {
        for (i, d) in self.dlidx.iter_mut().enumerate() {
            if d.buf.is_empty() {
                break;
            }
            if flush {
                store.write(dlidx_rowid(self.segid, i as i32, d.pgno), &d.buf)?;
            }
            d.buf.clear();
            d.prev_valid = false;
        }
        Ok(())
    }

    fn flush_dlidx(&mut self, store: &Store) -> Result<bool> {
        let flag = !self.dlidx[0].buf.is_empty() && self.nempty >= MIN_DLIDX_SIZE;
        self.dlidx_clear(store, flag)?;
        self.nempty = 0;
        Ok(flag)
    }

    fn flush_btree(&mut self, store: &Store) -> Result<()> {
        if self.bt_page == 0 {
            return Ok(());
        }
        let flag = self.flush_dlidx(store)?;
        store.idx_insert(
            self.segid,
            &self.btterm,
            flag as i64 + ((self.bt_page as i64) << 1),
        )?;
        self.bt_page = 0;
        Ok(())
    }

    fn btree_term(&mut self, store: &Store, term: &[u8]) -> Result<()> {
        self.flush_btree(store)?;
        self.btterm = term.to_vec();
        self.bt_page = self.pgno;
        Ok(())
    }

    fn btree_noterm(&mut self) {
        if self.first_rowid_in_page && !self.dlidx[0].buf.is_empty() {
            put_varint(&mut self.dlidx[0].buf, 0);
        }
        self.nempty += 1;
    }

    fn dlidx_append(&mut self, store: &Store, rowid: i64) -> Result<()> {
        let mut i = 0;
        let mut done = false;
        while !done {
            if self.dlidx[i].buf.len() >= self.pgsz {
                self.dlidx[i].buf[0] = 0x01;
                store.write(
                    dlidx_rowid(self.segid, i as i32, self.dlidx[i].pgno),
                    &self.dlidx[i].buf,
                )?;
                if self.dlidx.len() < i + 2 {
                    self.dlidx.push(DlidxWriter::default());
                }
                if self.dlidx[i + 1].buf.is_empty() {
                    let first = {
                        let b = &self.dlidx[i].buf;
                        let mut off = 1;
                        get_varint(b, &mut off);
                        get_varint(b, &mut off) as i64
                    };
                    let pgno = self.dlidx[i].pgno;
                    let up = &mut self.dlidx[i + 1];
                    up.pgno = pgno;
                    put_varint(&mut up.buf, 0);
                    put_varint(&mut up.buf, pgno as u64);
                    put_varint(&mut up.buf, first as u64);
                    up.prev_valid = true;
                    up.prev = first;
                }
                self.dlidx[i].buf.clear();
                self.dlidx[i].prev_valid = false;
                self.dlidx[i].pgno += 1;
            } else {
                done = true;
            }
            let val = if self.dlidx[i].prev_valid {
                (rowid as u64).wrapping_sub(self.dlidx[i].prev as u64)
            } else {
                let pgno = if i == 0 {
                    self.pgno
                } else {
                    self.dlidx[i - 1].pgno
                };
                let d = &mut self.dlidx[i];
                put_varint(&mut d.buf, (!done) as u64);
                put_varint(&mut d.buf, pgno as u64);
                rowid as u64
            };
            let d = &mut self.dlidx[i];
            put_varint(&mut d.buf, val);
            d.prev_valid = true;
            d.prev = rowid;
            i += 1;
        }
        Ok(())
    }

    fn flush_leaf(&mut self, store: &Store) -> Result<()> {
        let n = self.buf.len() as u16;
        put_u16(&mut self.buf, 2, n);
        if self.first_term_in_page {
            self.btree_noterm();
        } else {
            let pgidx = std::mem::take(&mut self.pgidx);
            self.buf.extend_from_slice(&pgidx);
        }
        store.write(segment_rowid(self.segid, self.pgno), &self.buf)?;
        self.buf.clear();
        self.buf.extend_from_slice(&[0, 0, 0, 0]);
        self.pgidx.clear();
        self.prev_pgidx = 0;
        self.pgno += 1;
        self.nleaf_written += 1;
        self.first_term_in_page = true;
        self.first_rowid_in_page = true;
        Ok(())
    }

    fn append_term(&mut self, store: &Store, term: &[u8]) -> Result<()> {
        if self.buf.len() + self.pgidx.len() + term.len() + 2 >= self.pgsz && self.buf.len() > 4 {
            self.flush_leaf(store)?;
        }
        put_varint(&mut self.pgidx, (self.buf.len() - self.prev_pgidx) as u64);
        self.prev_pgidx = self.buf.len();
        let nprefix;
        if self.first_term_in_page {
            nprefix = 0;
            if self.pgno != 1 {
                let n = if !self.term.is_empty() {
                    1 + prefix_compress(&self.term, term)
                } else {
                    term.len()
                };
                let n = n.min(term.len());
                self.btree_term(store, &term[..n])?;
            }
        } else {
            nprefix = prefix_compress(&self.term, term);
            put_varint(&mut self.buf, nprefix as u64);
        }
        put_varint(&mut self.buf, (term.len() - nprefix) as u64);
        self.buf.extend_from_slice(&term[nprefix..]);
        self.term = term.to_vec();
        self.first_term_in_page = false;
        self.first_rowid_in_page = false;
        self.first_rowid_in_doclist = true;
        self.dlidx[0].pgno = self.pgno;
        Ok(())
    }

    fn append_rowid(&mut self, store: &Store, rowid: i64) -> Result<()> {
        if self.buf.len() + self.pgidx.len() >= self.pgsz {
            self.flush_leaf(store)?;
        }
        if self.first_rowid_in_page {
            let n = self.buf.len() as u16;
            put_u16(&mut self.buf, 0, n);
            self.dlidx_append(store, rowid)?;
        }
        if self.first_rowid_in_doclist || self.first_rowid_in_page {
            put_varint(&mut self.buf, rowid as u64);
        } else {
            put_varint(
                &mut self.buf,
                (rowid as u64).wrapping_sub(self.prev_rowid as u64),
            );
        }
        self.prev_rowid = rowid;
        self.first_rowid_in_doclist = false;
        self.first_rowid_in_page = false;
        Ok(())
    }

    fn append_poslist_data(&mut self, store: &Store, data: &[u8]) -> Result<()> {
        let mut a = data;
        while self.buf.len() + self.pgidx.len() + a.len() >= self.pgsz {
            let req = self.pgsz as i64 - self.buf.len() as i64 - self.pgidx.len() as i64;
            let mut ncopy = 0;
            while (ncopy as i64) < req {
                get_varint(a, &mut ncopy);
            }
            let ncopy = ncopy.min(a.len());
            self.buf.extend_from_slice(&a[..ncopy]);
            a = &a[ncopy..];
            self.flush_leaf(store)?;
        }
        self.buf.extend_from_slice(a);
        Ok(())
    }

    /// Flushes the last leaf; returns the last leaf page number.
    fn finish(&mut self, store: &Store) -> Result<i32> {
        if self.buf.len() > 4 {
            self.flush_leaf(store)?;
        }
        let nleaf = self.pgno - 1;
        if self.pgno > 1 {
            self.flush_btree(store)?;
        }
        Ok(nleaf)
    }
}

/// The size of the longest prefix of whole varints of `a` that fits in
/// `nmax` bytes; always at least one varint.
fn poslist_prefix(a: &[u8], nmax: i64) -> usize {
    let mut ret = 0;
    get_varint32(a, &mut ret);
    if (ret as i64) < nmax {
        loop {
            let mut i = ret;
            get_varint32(a, &mut i);
            if i as i64 > nmax {
                break;
            }
            ret = i;
        }
    }
    ret
}

// ------------------------------------------------------------------ index operations

/// A term with its doclist: (rowid, position list) pairs.
pub(crate) type TermDoclist = (Vec<u8>, Vec<(i64, Vec<u8>)>);

/// The per-operation view of the index: a store plus the settings.
pub(crate) struct Index<'a> {
    pub store: Store<'a>,
    pub cfg: &'a Fts5Config,
    pub vals: &'a ConfigValues,
    /// Set once secure-delete has switched %_config to version 5.
    pub version_bumped: std::cell::Cell<bool>,
}

/// Pending (unflushed) writes of one connection.
#[derive(Debug, Clone)]
pub(crate) struct Pending {
    pub hash: Hash,
    pub write_rowid: i64,
    pub bdelete: bool,
    pub npending_row: i64,
    pub ncontentless_delete: i64,
    pub started: bool,
}

impl Pending {
    pub(crate) fn new(detail: Detail) -> Self {
        Self {
            hash: Hash::new(detail),
            write_rowid: 0,
            bdelete: false,
            npending_row: 0,
            ncontentless_delete: 0,
            started: false,
        }
    }

    pub(crate) fn has_data(&self) -> bool {
        !self.hash.is_empty() || self.ncontentless_delete > 0
    }

    pub(crate) fn discard(&mut self) {
        self.hash.clear();
        self.npending_row = 0;
        self.ncontentless_delete = 0;
        self.started = false;
    }
}

/// Number of bytes of the first `nchar` characters of `p`, or 0 if `p` is
/// shorter (sqlite3Fts5IndexCharlenToBytelen).
pub(crate) fn charlen_to_bytelen(p: &[u8], nchar: usize) -> usize {
    let mut n = 0;
    for i in 0..nchar {
        if n >= p.len() {
            return 0;
        }
        let c = p[n];
        n += 1;
        if c >= 0xc0 {
            if n >= p.len() {
                return 0;
            }
            while (p[n] & 0xc0) == 0x80 {
                n += 1;
                if n >= p.len() {
                    if i + 1 == nchar {
                        break;
                    }
                    return 0;
                }
            }
        }
    }
    n
}

pub(crate) fn char_len(p: &[u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < p.len() {
        let c = p[i];
        i += 1;
        if c >= 0xc0 {
            while i < p.len() && (p[i] & 0xc0) == 0x80 {
                i += 1;
            }
        }
        n += 1;
    }
    n
}

impl<'a> Index<'a> {
    pub(crate) fn new(
        conn: &'a Arc<Connection>,
        cfg: &'a Fts5Config,
        vals: &'a ConfigValues,
    ) -> Self {
        Self {
            store: Store::new(conn, cfg),
            cfg,
            vals,
            version_bumped: std::cell::Cell::new(false),
        }
    }

    fn detail(&self) -> Detail {
        self.cfg.detail
    }

    /// sqlite3Fts5IndexReinit: an empty index.
    pub(crate) fn reinit(&self, cookie: i32) -> Result<()> {
        let s = Structure {
            origin_cntr: if self.cfg.contentless_delete { 1 } else { 0 },
            ..Default::default()
        };
        self.store.write(AVERAGES_ROWID, &[])?;
        self.store.write_structure(&s, cookie)
    }

    /// Adds a token to the pending hash (sqlite3Fts5IndexWrite).
    pub(crate) fn write_token(&self, pending: &mut Pending, col: i32, pos: i32, token: &[u8]) {
        let rowid = pending.write_rowid;
        pending.hash.write(rowid, col, pos, MAIN_PREFIX, token);
        for (i, &nchar) in self.cfg.prefixes.iter().enumerate() {
            let nbyte = charlen_to_bytelen(token, nchar);
            if nbyte > 0 {
                pending
                    .hash
                    .write(rowid, col, pos, MAIN_PREFIX + i as u8 + 1, &token[..nbyte]);
            }
        }
    }

    /// sqlite3Fts5IndexBeginWrite
    pub(crate) fn begin_write(
        &self,
        pending: &mut Pending,
        bdelete: bool,
        rowid: i64,
    ) -> Result<()> {
        if pending.started
            && (rowid < pending.write_rowid
                || (rowid == pending.write_rowid && !pending.bdelete)
                || pending.hash.nbyte > self.vals.hashsize as usize)
        {
            self.flush(pending)?;
        }
        pending.started = true;
        pending.write_rowid = rowid;
        pending.bdelete = bdelete;
        if !bdelete {
            pending.npending_row += 1;
        }
        Ok(())
    }

    /// fts5IndexFlush
    pub(crate) fn flush(&self, pending: &mut Pending) -> Result<()> {
        if pending.has_data() {
            self.flush_hash(pending)?;
        }
        let started = pending.started;
        pending.discard();
        pending.started = started;
        Ok(())
    }

    /// fts5FlushOneHash
    fn flush_hash(&self, pending: &mut Pending) -> Result<()> {
        let (mut s, cookie) = self.store.read_structure()?;
        let mut pgno_last = 0;
        if !pending.hash.is_empty() {
            let segid = s.allocate_segid()?;
            let secure = self.vals.secure_delete;
            let pgsz = self.vals.pgsz as usize;
            let detail = self.detail();
            let mut w = SegWriter::new(segid, self.vals.pgsz);
            for (term, doclist) in pending.hash.scan() {
                if !secure {
                    w.append_term(&self.store, &term)?;
                }
                if !secure && pgsz > w.buf.len() + w.pgidx.len() + doclist.len() {
                    w.buf.extend_from_slice(&doclist);
                    continue;
                }
                let mut term_written = !secure;
                let mut rowid: i64 = 0;
                let mut prev: i64 = 0;
                let mut off = 0;
                while off < doclist.len() {
                    let delta = get_varint(&doclist, &mut off);
                    rowid = rowid.wrapping_add(delta as i64);
                    // Secure delete removes the old entry from its segment; a
                    // pure delete marker is then not written at all.
                    if secure {
                        if detail == Detail::None {
                            if off < doclist.len() && doclist[off] == 0 {
                                self.secure_delete(&mut s, &term, rowid)?;
                                off += 1;
                                if off < doclist.len() && doclist[off] == 0 {
                                    off += 1;
                                } else {
                                    continue;
                                }
                            }
                        } else if (doclist[off] & 1) != 0 {
                            self.secure_delete(&mut s, &term, rowid)?;
                            if doclist[off] == 0x01 {
                                off += 1;
                                continue;
                            }
                        }
                    }
                    if !term_written {
                        w.append_term(&self.store, &term)?;
                        term_written = true;
                    }
                    if w.first_rowid_in_page {
                        let n = w.buf.len() as u16;
                        put_u16(&mut w.buf, 0, n);
                        put_varint(&mut w.buf, rowid as u64);
                        w.first_rowid_in_page = false;
                        w.dlidx_append(&self.store, rowid)?;
                    } else {
                        put_varint(&mut w.buf, (rowid as u64).wrapping_sub(prev as u64));
                    }
                    prev = rowid;
                    if detail == Detail::None {
                        if off < doclist.len() && doclist[off] == 0 {
                            w.buf.push(0);
                            off += 1;
                            if off < doclist.len() && doclist[off] == 0 {
                                w.buf.push(0);
                                off += 1;
                            }
                        }
                        if w.buf.len() + w.pgidx.len() >= pgsz {
                            w.flush_leaf(&self.store)?;
                        }
                    } else {
                        let mut o = off;
                        let sz = get_varint32(&doclist, &mut o) as usize;
                        let (npos, del) = (sz >> 1, sz & 1 != 0);
                        let ncopy;
                        if del && secure {
                            put_varint(&mut w.buf, (npos * 2) as u64);
                            off = o;
                            ncopy = npos;
                        } else {
                            ncopy = (o - off) + npos;
                        }
                        if w.buf.len() + w.pgidx.len() + ncopy <= pgsz {
                            w.buf.extend_from_slice(&doclist[off..off + ncopy]);
                        } else {
                            let pl = &doclist[off..off + ncopy];
                            let mut ipos = 0;
                            loop {
                                // May be negative: the rowid can overfill the page.
                                let nspace =
                                    pgsz as i64 - w.buf.len() as i64 - w.pgidx.len() as i64;
                                let n = if (ncopy - ipos) as i64 <= nspace {
                                    ncopy - ipos
                                } else {
                                    poslist_prefix(&pl[ipos..], nspace)
                                };
                                w.buf.extend_from_slice(&pl[ipos..ipos + n]);
                                ipos += n;
                                if w.buf.len() + w.pgidx.len() >= pgsz {
                                    w.flush_leaf(&self.store)?;
                                }
                                if ipos >= ncopy {
                                    break;
                                }
                            }
                        }
                        off += ncopy;
                    }
                }
            }
            pgno_last = w.finish(&self.store)?;
            if pgno_last > 0 {
                if s.levels.is_empty() {
                    s.levels.push(Level::default());
                }
                let mut seg = Segment {
                    segid,
                    pgno_first: 1,
                    pgno_last,
                    ..Default::default()
                };
                if s.origin_cntr > 0 {
                    seg.origin1 = s.origin_cntr;
                    seg.origin2 = s.origin_cntr;
                    seg.nentry = pending.npending_row as u64;
                    s.origin_cntr += 1;
                }
                s.levels[0].segs.push(seg);
                s.promote(0);
            }
        }
        self.automerge(&mut s, pgno_last as i64 + pending.ncontentless_delete)?;
        self.crisismerge(&mut s)?;
        // A version change bumps the cookie so other connections reload.
        let cookie = if self.version_bumped.get() {
            cookie + 1
        } else {
            cookie
        };
        self.store.write_structure(&s, cookie)
    }

    /// fts5IndexAutomerge
    fn automerge(&self, s: &mut Structure, nleaf: i64) -> Result<()> {
        if self.vals.automerge > 0 {
            let nwrite = s.write_counter as i64;
            let nwork = (nwrite + nleaf) / WORK_UNIT - nwrite / WORK_UNIT;
            s.write_counter += nleaf as u64;
            let nrem = WORK_UNIT * nwork * s.levels.len() as i64;
            self.merge(s, nrem as i32, self.vals.automerge as usize)?;
        }
        Ok(())
    }

    /// fts5IndexCrisismerge
    fn crisismerge(&self, s: &mut Structure) -> Result<()> {
        let ncrisis = self.vals.crisismerge as usize;
        let mut ilvl = 0;
        while ilvl < s.levels.len() && s.levels[ilvl].segs.len() >= ncrisis {
            self.merge_level(s, ilvl, None)?;
            s.promote(ilvl + 1);
            ilvl += 1;
        }
        Ok(())
    }

    fn find_delete_merge(&self, s: &Structure) -> Option<usize> {
        if !self.cfg.contentless_delete || self.vals.deletemerge <= 0 {
            return None;
        }
        let mut ret = None;
        let mut best = 0;
        for (i, lvl) in s.levels.iter().enumerate() {
            let nentry: u64 = lvl.segs.iter().map(|s| s.nentry).sum();
            let ntomb: u64 = lvl.segs.iter().map(|s| s.nentry_tombstone).sum();
            if nentry > 0 {
                let pct = (ntomb * 100 / nentry) as i32;
                if pct >= self.vals.deletemerge && pct > best {
                    ret = Some(i);
                    best = pct;
                }
            }
            if lvl.nmerge != 0 {
                break;
            }
        }
        ret
    }

    /// fts5IndexMerge: does up to `npg` pages of merge work. Returns true if
    /// anything changed.
    fn merge(&self, s: &mut Structure, npg: i32, nmin: usize) -> Result<bool> {
        let mut nrem = npg;
        let mut ret = false;
        let mut nmin = nmin;
        while nrem > 0 {
            let mut best_lvl = 0;
            let mut nbest = 0;
            for (i, lvl) in s.levels.iter().enumerate() {
                if lvl.nmerge != 0 {
                    if lvl.nmerge > nbest {
                        best_lvl = i;
                        nbest = nmin;
                    }
                    break;
                }
                if lvl.segs.len() > nbest {
                    nbest = lvl.segs.len();
                    best_lvl = i;
                }
            }
            let best = if nbest < nmin {
                self.find_delete_merge(s)
            } else {
                Some(best_lvl)
            };
            let Some(best) = best else { break };
            ret = true;
            self.merge_level(s, best, Some(&mut nrem))?;
            if s.levels[best].nmerge == 0 {
                s.promote(best + 1);
            }
            if nmin == 1 {
                nmin = 2;
            }
        }
        Ok(ret)
    }

    /// fts5IndexMergeLevel
    fn merge_level(&self, s: &mut Structure, ilvl: usize, nrem: Option<&mut i32>) -> Result<()> {
        let detail = self.detail();
        let budget = nrem.as_ref().map(|n| **n);
        let ninput;
        let mut w;
        let out_idx;
        if s.levels[ilvl].nmerge != 0 {
            ninput = s.levels[ilvl].nmerge;
            let lvl_out = &s.levels[ilvl + 1];
            out_idx = lvl_out.segs.len() - 1;
            let seg = &lvl_out.segs[out_idx];
            w = SegWriter::new(seg.segid, self.vals.pgsz);
            w.pgno = seg.pgno_last + 1;
            w.bt_page = 0;
        } else {
            let segid = s.allocate_segid()?;
            if ilvl == s.levels.len() - 1 {
                s.levels.push(Level::default());
            }
            ninput = s.levels[ilvl].segs.len();
            let mut seg = Segment {
                segid,
                pgno_first: 1,
                ..Default::default()
            };
            if s.origin_cntr > 0 {
                seg.origin1 = s.levels[ilvl].segs[0].origin1;
                seg.origin2 = s.levels[ilvl].segs[ninput - 1].origin2;
            }
            s.levels[ilvl + 1].segs.push(seg);
            out_idx = s.levels[ilvl + 1].segs.len() - 1;
            w = SegWriter::new(segid, self.vals.pgsz);
        }
        let oldest = s.levels[ilvl + 1].segs.len() == 1 && s.levels.len() == ilvl + 2;

        // Inputs, newest first.
        let mut sources = Vec::with_capacity(ninput);
        for seg in s.levels[ilvl].segs[..ninput].iter().rev() {
            sources.push(SegIter::init(&self.store, seg, detail)?);
        }
        let mut term: Option<Vec<u8>> = None;
        let mut term_written = false;
        let mut finished = true;
        loop {
            // Pick the next (term, rowid), newest segment first on ties.
            let mut first: Option<usize> = None;
            for (i, it) in sources.iter().enumerate() {
                if it.eof() {
                    continue;
                }
                match first {
                    Some(f)
                        if (sources[f].term.as_slice(), sources[f].rowid)
                            <= (it.term.as_slice(), it.rowid) => {}
                    _ => first = Some(i),
                }
            }
            let Some(f) = first else { break };
            let deleted = sources[f].is_deleted()?;
            let (eterm, erowid) = (sources[f].term.clone(), sources[f].rowid);
            if term.as_deref() != Some(eterm.as_slice()) {
                if let Some(b) = budget {
                    if w.nleaf_written > b {
                        finished = false;
                        break;
                    }
                }
                term = Some(eterm.clone());
                term_written = false;
            }
            let (npos, del) = (sources[f].npos, sources[f].del);
            let keep = !deleted && !(npos == 0 && (oldest || !del));
            if keep {
                if !term_written {
                    w.append_term(&self.store, &eterm)?;
                    term_written = true;
                }
                w.append_rowid(&self.store, erowid)?;
                if detail == Detail::None {
                    if del {
                        put_varint(&mut w.buf, 0);
                        if npos > 0 {
                            put_varint(&mut w.buf, 0);
                        }
                    }
                } else {
                    put_varint(&mut w.buf, (npos * 2 + del as usize) as u64);
                    let pl = sources[f].poslist()?;
                    w.append_poslist_data(&self.store, &pl)?;
                }
            }
            for it in sources.iter_mut() {
                while !it.eof() && it.term == eterm && it.rowid == erowid {
                    it.next()?;
                }
            }
        }
        let pgno_last = w.finish(&self.store)?;
        s.levels[ilvl + 1].segs[out_idx].pgno_last = pgno_last;
        if finished {
            let mut nentry = 0;
            for old in &s.levels[ilvl].segs[..ninput] {
                nentry += old.nentry.saturating_sub(old.nentry_tombstone);
                self.store.remove_segment(old)?;
            }
            s.levels[ilvl + 1].segs[out_idx].nentry += nentry;
            s.levels[ilvl].segs.drain(..ninput);
            s.levels[ilvl].nmerge = 0;
            if s.levels[ilvl + 1].segs[out_idx].pgno_last == 0 {
                s.levels[ilvl + 1].segs.remove(out_idx);
            }
        } else {
            // Trim the inputs to the unread part (fts5TrimSegments).
            for (k, it) in sources.iter().enumerate() {
                let idx = ninput - 1 - k;
                if it.eof() {
                    let seg = &mut s.levels[ilvl].segs[idx];
                    seg.pgno_first = 0;
                    seg.pgno_last = 0;
                    continue;
                }
                let id = segment_rowid(it.seg.segid, it.term_leaf_pgno);
                let data = read_leaf(&self.store, id)?;
                let off = it.term_leaf_offset;
                if off > data.szleaf {
                    return Err(corrupt());
                }
                let mut buf = vec![0u8; 4];
                put_varint(&mut buf, it.term.len() as u64);
                buf.extend_from_slice(&it.term);
                buf.extend_from_slice(&data.p[off..data.szleaf]);
                let n = buf.len() as u16;
                put_u16(&mut buf, 2, n);
                put_varint(&mut buf, 4);
                if it.leaf_pgno == it.term_leaf_pgno
                    && it.end_of_doclist < data.szleaf
                    && it.pgidx_off <= data.nn()
                {
                    let ndiff = data.szleaf - it.end_of_doclist;
                    let v = buf.len() - 1 - ndiff - 4;
                    put_varint(&mut buf, v as u64);
                    buf.extend_from_slice(&data.p[it.pgidx_off..]);
                }
                s.levels[ilvl].segs[idx].pgno_first = it.term_leaf_pgno;
                self.store
                    .delete_range(segment_rowid(it.seg.segid, 1), id)?;
                self.store.write(id, &buf)?;
            }
            s.levels[ilvl].nmerge = ninput;
        }
        if let Some(n) = nrem {
            *n -= w.nleaf_written;
        }
        Ok(())
    }

    /// sqlite3Fts5IndexOptimize
    pub(crate) fn optimize(&self, pending: &mut Pending) -> Result<()> {
        self.flush(pending)?;
        let (s, cookie) = self.store.read_structure()?;
        let Some(mut news) = optimize_struct(&s) else {
            return Ok(());
        };
        let ilvl = news
            .levels
            .iter()
            .position(|l| !l.segs.is_empty())
            .unwrap_or(0);
        while !news.levels[ilvl].segs.is_empty() {
            let mut nrem = OPT_WORK_UNIT;
            self.merge_level(&mut news, ilvl, Some(&mut nrem))?;
        }
        self.store.write_structure(&news, cookie)
    }

    /// sqlite3Fts5IndexMerge ('merge' command).
    pub(crate) fn user_merge(&self, pending: &mut Pending, nmerge: i64) -> Result<()> {
        self.flush(pending)?;
        let (s, cookie) = self.store.read_structure()?;
        let mut nmin = self.vals.usermerge as usize;
        let mut nmerge = nmerge;
        let mut s = Some(s);
        if nmerge < 0 {
            s = optimize_struct(s.as_ref().unwrap());
            nmin = 1;
            nmerge = if nmerge == i32::MIN as i64 {
                i32::MAX as i64
            } else {
                -nmerge
            };
        }
        if let Some(mut s) = s {
            if !s.levels.is_empty()
                && self.merge(&mut s, nmerge.min(i32::MAX as i64) as i32, nmin)?
            {
                self.store.write_structure(&s, cookie)?;
            }
        }
        Ok(())
    }

    /// Writes (or rewrites) the cookie in the structure record.
    pub(crate) fn set_cookie(&self, cookie: i32) -> Result<()> {
        let mut data = self.store.read_req(STRUCTURE_ROWID)?;
        if data.len() < 4 {
            return Err(corrupt());
        }
        put_u32(&mut data, 0, cookie as u32);
        self.store.write(STRUCTURE_ROWID, &data)
    }

    /// The averages record: (row count, per-column token totals).
    pub(crate) fn get_averages(&self) -> Result<(i64, Vec<i64>)> {
        let data = self.store.read_req(AVERAGES_ROWID)?;
        let mut sizes = vec![0i64; self.cfg.ncol()];
        let mut nrow = 0;
        if !data.is_empty() {
            let mut i = 0;
            nrow = get_varint(&data, &mut i) as i64;
            for s in sizes.iter_mut() {
                if i >= data.len() {
                    break;
                }
                *s = get_varint(&data, &mut i) as i64;
            }
        }
        Ok((nrow, sizes))
    }

    pub(crate) fn set_averages(&self, nrow: i64, sizes: &[i64]) -> Result<()> {
        let mut buf = Vec::new();
        put_varint(&mut buf, nrow as u64);
        for &s in sizes {
            put_varint(&mut buf, s as u64);
        }
        self.store.write(AVERAGES_ROWID, &buf)
    }

    /// The origin counter for the next level-0 segment.
    pub(crate) fn origin(&self) -> Result<u64> {
        Ok(self.store.read_structure()?.0.origin_cntr)
    }

    // -------------------------------------------------------------- reading

    /// A merged iterator over all segments (and `pending`, newest of all).
    pub(crate) fn iter_all<'s>(
        &'s self,
        s: &Structure,
        pending: Option<&Pending>,
        skip_empty: bool,
    ) -> Result<MultiIter<'s>> {
        let mut sources = Vec::new();
        if let Some(p) = pending.filter(|p| !p.hash.is_empty()) {
            sources.push(Source::Hash(HashSource::new(p.hash.scan(), self.detail())));
        }
        for lvl in &s.levels {
            for seg in lvl.segs.iter().rev() {
                sources.push(Source::Seg(SegIter::init(&self.store, seg, self.detail())?));
            }
        }
        Ok(MultiIter {
            sources,
            skip_empty,
        })
    }

    /// The merged doclist of one index key: (rowid, poslist) sorted by rowid.
    /// Delete markers and tombstoned rows are dropped.
    pub(crate) fn query_term(
        &self,
        s: &Structure,
        pending: Option<&Pending>,
        key: &[u8],
    ) -> Result<Vec<(i64, Vec<u8>)>> {
        let mut sources = Vec::new();
        if let Some(p) = pending {
            if let Some(dl) = p.hash.query(key) {
                sources.push(Source::Hash(HashSource::new(
                    vec![(key.to_vec(), dl)],
                    self.detail(),
                )));
            }
        }
        for lvl in &s.levels {
            for seg in lvl.segs.iter().rev() {
                let it = SegIter::seek(&self.store, seg, self.detail(), key, false)?;
                if !it.eof() {
                    sources.push(Source::Seg(it));
                }
            }
        }
        let mut m = MultiIter {
            sources,
            skip_empty: true,
        };
        let mut out = Vec::new();
        while let Some(e) = m.next()? {
            out.push((e.rowid, e.poslist));
        }
        Ok(out)
    }

    /// All (term, merged doclist) pairs whose key starts with `prefix`.
    pub(crate) fn scan_prefix(
        &self,
        s: &Structure,
        pending: Option<&Pending>,
        prefix: &[u8],
    ) -> Result<Vec<TermDoclist>> {
        let mut sources = Vec::new();
        if let Some(p) = pending {
            let entries = p.hash.scan_prefix(prefix);
            if !entries.is_empty() {
                sources.push(Source::Hash(HashSource::new(entries, self.detail())));
            }
        }
        for lvl in &s.levels {
            for seg in lvl.segs.iter().rev() {
                let mut it = SegIter::seek(&self.store, seg, self.detail(), prefix, true)?;
                if !it.eof() && !it.term.starts_with(prefix) {
                    it.leaf = None;
                }
                if !it.eof() {
                    sources.push(Source::Seg(it));
                }
            }
        }
        let mut m = MultiIter {
            sources,
            skip_empty: true,
        };
        let mut out: Vec<TermDoclist> = Vec::new();
        loop {
            // Stop each segment once it has moved past the prefix.
            for src in m.sources.iter_mut() {
                if let Source::Seg(it) = src {
                    if !it.eof() && !it.term.starts_with(prefix) {
                        it.leaf = None;
                    }
                }
            }
            let Some(e) = m.next()? else { break };
            if !e.term.starts_with(prefix) {
                continue;
            }
            match out.last_mut() {
                Some((t, dl)) if *t == e.term => dl.push((e.rowid, e.poslist)),
                _ => out.push((e.term, vec![(e.rowid, e.poslist)])),
            }
        }
        Ok(out)
    }

    // -------------------------------------------------------------- deletes

    /// Adds a tombstone for `rowid` to the segments holding `origin`.
    pub(crate) fn contentless_delete(
        &self,
        pending: &mut Pending,
        origin: i64,
        rowid: i64,
    ) -> Result<()> {
        let (mut s, cookie) = self.store.read_structure()?;
        let mut found = false;
        for ilvl in (0..s.levels.len()).rev() {
            for iseg in (0..s.levels[ilvl].segs.len()).rev() {
                let seg = &s.levels[ilvl].segs[iseg];
                if seg.origin1 <= origin as u64 && seg.origin2 >= origin as u64 {
                    if !found {
                        s.levels[ilvl].segs[iseg].nentry_tombstone += 1;
                        found = true;
                    }
                    pending.ncontentless_delete += 1;
                    self.tombstone_add(&mut s.levels[ilvl].segs[iseg], rowid as u64)?;
                }
            }
        }
        self.store.write_structure(&s, cookie)
    }

    fn tombstone_add(&self, seg: &mut Segment, rowid: u64) -> Result<()> {
        let mut page: Option<(Vec<u8>, usize)> = None;
        if seg.npg_tombstone > 0 {
            let ipg = (rowid % seg.npg_tombstone as u64) as usize;
            let mut pg = self
                .store
                .read_req(tombstone_rowid(seg.segid, ipg as i32))?;
            if tombstone_add_to_page(&mut pg, false, seg.npg_tombstone as u64, rowid) == 0 {
                self.store
                    .write(tombstone_rowid(seg.segid, ipg as i32), &pg)?;
                return Ok(());
            }
            page = Some((pg, ipg));
        }
        let mut szkey = page
            .as_ref()
            .map_or(4, |(p, _)| if p[0] == 8 { 8 } else { 4 });
        if rowid > 0xFFFF_FFFF {
            szkey = 8;
        }
        let mut pages = self.tombstone_rebuild(seg, page.as_ref(), szkey)?;
        let n = pages.len() as u64;
        tombstone_add_to_page(&mut pages[(rowid % n) as usize], true, n, rowid);
        for (i, p) in pages.iter().enumerate() {
            self.store.write(tombstone_rowid(seg.segid, i as i32), p)?;
        }
        seg.npg_tombstone = n as i32;
        Ok(())
    }

    fn tombstone_rebuild(
        &self,
        seg: &Segment,
        page1: Option<&(Vec<u8>, usize)>,
        szkey: usize,
    ) -> Result<Vec<Vec<u8>>> {
        const MINSLOT: usize = 32;
        let nslot_per_page = MINSLOT.max((self.vals.pgsz as usize - 8) / szkey);
        let mut nout = 0usize;
        let mut nslot = 0usize;
        if seg.npg_tombstone == 0 {
            nout = 1;
            nslot = MINSLOT;
        } else if seg.npg_tombstone == 1 {
            let nelem = get_u32(&page1.expect("page").0, 4) as usize;
            if nelem <= nslot_per_page / 4 {
                nout = 1;
                nslot = (nelem * 4).max(MINSLOT);
            }
        }
        if nout == 0 {
            nout = seg.npg_tombstone as usize * 2 + 1;
            nslot = nslot_per_page;
        }
        loop {
            let szpage = 8 + nslot * szkey;
            let mut out: Vec<Vec<u8>> = (0..nout)
                .map(|_| {
                    let mut p = vec![0u8; szpage];
                    p[0] = szkey as u8;
                    p
                })
                .collect();
            let mut res = 0;
            for ii in 0..seg.npg_tombstone as usize {
                let data = match page1 {
                    Some((p, i)) if *i == ii => Some(p.clone()),
                    _ => self.store.read(tombstone_rowid(seg.segid, ii as i32))?,
                };
                let Some(data) = data else { continue };
                let szin = if data[0] == 8 { 8 } else { 4 };
                let nslot_in = (data.len() - 8) / szin;
                for i in 0..nslot_in {
                    let v = if szin == 4 {
                        get_u32(&data, 8 + i * 4) as u64
                    } else {
                        get_u64(&data, 8 + i * 8)
                    };
                    if v != 0 {
                        let k = (v % nout as u64) as usize;
                        res = tombstone_add_to_page(&mut out[k], false, nout as u64, v);
                        if res != 0 {
                            break;
                        }
                    }
                }
                if ii == 0 {
                    out[0][1] = data[1];
                }
                if res != 0 {
                    break;
                }
            }
            if res == 0 {
                return Ok(out);
            }
            nslot = nslot_per_page;
            nout = nout * 2 + 1;
        }
    }

    // -------------------------------------------------------------- secure delete

    /// fts5FlushSecureDelete: removes (term, rowid) in place from the newest
    /// segment holding it. Returns true if the entry was found.
    fn secure_delete(&self, s: &mut Structure, term: &[u8], rowid: i64) -> Result<bool> {
        let detail = self.detail();
        // Set the version to the secure-delete format first.
        if self.vals.version != CURRENT_VERSION_SECUREDELETE && !self.version_bumped.get() {
            self.version_bumped.set(true);
            super::storage::set_config(
                self.store.conn,
                self.cfg,
                "version",
                &sql::int(CURRENT_VERSION_SECUREDELETE),
            )?;
        }
        for ilvl in 0..s.levels.len() {
            for iseg in (0..s.levels[ilvl].segs.len()).rev() {
                let seg = s.levels[ilvl].segs[iseg].clone();
                let mut it = SegIter::seek(&self.store, &seg, detail, term, false)?;
                while !it.eof() && it.term == term && it.rowid < rowid {
                    it.next()?;
                }
                if it.eof() || it.term != term || it.rowid != rowid {
                    continue;
                }
                self.do_secure_delete(&it)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// fts5SecureDeleteIdxEntry: drops the %_idx row of a page (never page 1).
    fn secure_delete_idx_entry(&self, segid: i32, pgno: i32) -> Result<()> {
        if pgno == 1 {
            return Ok(());
        }
        sql::exec(
            self.store.conn,
            &format!(
                "DELETE FROM {} WHERE segid=? AND (pgno/2)=?",
                self.store.idx
            ),
            &[sql::int(segid as i64), sql::int(pgno as i64)],
        )
    }

    /// fts5SecureDeleteOverflow: strips the part of a removed poslist that
    /// continues on pages `first..`. Returns true if the removed entry was the
    /// last of its doclist.
    fn secure_delete_overflow(&self, seg: &Segment, first: i32) -> Result<bool> {
        let none = self.detail() == Detail::None;
        let mut last_in_doclist = true;
        for pgno in first..=seg.pgno_last {
            let id = segment_rowid(seg.segid, pgno);
            let leaf = read_leaf(&self.store, id)?;
            let (szleaf, nn) = (leaf.szleaf, leaf.nn());
            let mut a = leaf.p;
            let mut inext = get_u16(&a, 0);
            if inext != 0 {
                last_in_doclist = false;
            }
            if inext == 0 && szleaf < nn {
                let mut o = szleaf;
                inext = get_varint32(&a, &mut o) as usize;
            }
            if inext == 0 {
                // Nothing but poslist data: the page becomes empty.
                if !none {
                    self.store.write(id, &[0, 0, 0, 4])?;
                }
                continue;
            }
            if none {
                break;
            }
            if inext >= szleaf || inext < 4 {
                return Err(corrupt());
            }
            let nshift = inext - 4;
            let mut footer = Vec::new();
            if nn > szleaf {
                let mut i1 = szleaf;
                let ifirst = get_varint32(&a, &mut i1) as usize;
                if ifirst < inext {
                    return Err(corrupt());
                }
                put_varint(&mut footer, (ifirst - nshift) as u64);
                if i1 < nn {
                    footer.extend_from_slice(&a[i1..nn]);
                }
            }
            let npg = szleaf - nshift;
            a.copy_within(4 + nshift..szleaf, 4);
            a.truncate(npg);
            put_u16(&mut a, 2, npg as u16);
            if get_u16(&a, 0) != 0 {
                put_u16(&mut a, 0, 4);
            }
            a.extend_from_slice(&footer);
            self.store.write(id, &a)?;
            break;
        }
        Ok(last_in_doclist)
    }

    /// fts5DoSecureDelete: removes the entry `it` points at by editing its
    /// leaf page (and any overflow or term pages) in place.
    fn do_secure_delete(&self, it: &SegIter) -> Result<()> {
        let none = self.detail() == Detail::None;
        let segid = it.seg.segid;
        let leaf = it.leaf();
        let npg = leaf.nn();
        let mut pgidx = leaf.szleaf;
        let footer: Vec<u8> = leaf.p[pgidx..].to_vec();
        let nidx = footer.len();
        let mut a = leaf.p.clone();
        a.resize(npg + 32, 0);
        let byte = |a: &[u8], i: usize| a.get(i).copied().unwrap_or(0);

        // start: offset of the entry's rowid varint; next_off: the entry after
        // it (past the page footer if its poslist overflows).
        let mut start = if it.leaf_pgno == it.term_leaf_pgno {
            it.term_leaf_offset
        } else {
            get_u16(&a, 0)
        };
        if start > npg {
            return Err(corrupt());
        }
        let mut sop = start;
        let mut delta = get_varint(&a, &mut sop);
        let mut next_off;
        if none {
            while sop < it.leaf_offset {
                if byte(&a, sop) == 0 {
                    sop += 1;
                }
                if byte(&a, sop) == 0 {
                    sop += 1;
                }
                start = sop;
                delta = get_varint(&a, &mut sop);
            }
            next_off = sop;
            for _ in 0..2 {
                if next_off < it.end_of_doclist && byte(&a, next_off) == 0 {
                    next_off += 1;
                }
            }
        } else {
            let mut np = get_varint32(&a, &mut sop) as usize;
            while sop < it.leaf_offset {
                start = sop + np / 2;
                sop = start;
                delta = get_varint(&a, &mut sop);
                np = get_varint32(&a, &mut sop) as usize;
            }
            next_off = sop + it.npos;
        }
        if start > npg {
            return Err(corrupt());
        }
        let mut off = start;
        let mut last_in_doclist = false;
        if next_off >= pgidx {
            last_in_doclist = self.secure_delete_overflow(&it.seg, it.leaf_pgno + 1)?;
            next_off = pgidx;
        }

        if !it.del {
            if next_off != pgidx {
                // The entry ends its doclist if a term starts right after it.
                let (mut i, mut key_off) = (0, 0usize);
                while i < nidx {
                    key_off += get_varint32(&footer, &mut i) as usize;
                    if key_off == next_off {
                        last_in_doclist = true;
                    }
                }
            }
            if get_u16(&a, 0) == start && (last_in_doclist || next_off == pgidx) {
                put_u16(&mut a, 0, 0);
            }
        }

        let mut del_key_off = 0usize;
        if it.del {
            // Keep a bare delete marker.
            off += put_varint_at(&mut a, off, delta);
            set_byte(&mut a, off, 0x01);
            off += 1;
        } else if !last_in_doclist {
            if next_off != pgidx {
                // Fold the deleted rowid delta into the next entry's.
                let nd = get_varint(&a, &mut next_off);
                off += put_varint_at(&mut a, off, delta.wrapping_add(nd));
            }
        } else if it.leaf_pgno == it.term_leaf_pgno && start == it.term_leaf_offset {
            // The only entry of its doclist: remove the term as well.
            let (mut i, mut ikey, mut key_off) = (0, 0usize, 0usize);
            while i < nidx {
                let v = get_varint32(&footer, &mut i) as usize;
                if key_off + v > start {
                    break;
                }
                key_off += v;
                ikey += 1;
            }
            del_key_off = key_off;
            off = key_off;
            if next_off != pgidx {
                // Another term follows on this page: rewrite it in place of
                // the removed one, re-expanding its shared prefix.
                del_key_off = next_off;
                let nprefix2 = get_varint(&a, &mut next_off);
                let nsuffix2 = get_varint(&a, &mut next_off);
                let mut ko = key_off;
                let mut nprefix = 0u64;
                if ikey != 1 {
                    nprefix = get_varint(&a, &mut ko);
                }
                get_varint(&a, &mut ko);
                let nprefix = nprefix.min(nprefix2);
                let nsuffix = (nprefix2 + nsuffix2).wrapping_sub(nprefix);
                if ko as u64 + nsuffix > pgidx as u64
                    || next_off as u64 + nsuffix2 > pgidx as u64
                    || nprefix2 > it.term.len() as u64
                {
                    return Err(corrupt());
                }
                if ikey != 1 {
                    off += put_varint_at(&mut a, off, nprefix);
                }
                off += put_varint_at(&mut a, off, nsuffix);
                let extra = &it.term[nprefix as usize..nprefix2 as usize];
                a[off..off + extra.len()].copy_from_slice(extra);
                off += extra.len();
                let n2 = nsuffix2 as usize;
                a.copy_within(next_off..next_off + n2, off);
                off += n2;
                next_off += n2;
            }
        } else if start == 4 {
            // First thing on a later page: if every page back to the term's
            // page is empty, the term ends its page and is removed there.
            let mut pgno = it.leaf_pgno - 1;
            while pgno > it.term_leaf_pgno {
                let pg = self.store.read_req(segment_rowid(segid, pgno))?;
                if pg.len() != 4 {
                    break;
                }
                pgno -= 1;
            }
            if pgno == it.term_leaf_pgno {
                let id = segment_rowid(segid, it.term_leaf_pgno);
                let tl = read_leaf(&self.store, id)?;
                if tl.szleaf == it.term_leaf_offset {
                    let tidx = &tl.p[tl.szleaf..];
                    let (mut ti, mut toff) = (0usize, 0usize);
                    loop {
                        let mut o = ti;
                        toff += get_varint32(tidx, &mut o) as usize;
                        if o >= tidx.len() {
                            break;
                        }
                        ti = o;
                    }
                    if toff > tl.szleaf {
                        return Err(corrupt());
                    }
                    let mut p = tl.p.clone();
                    p.copy_within(tl.szleaf..tl.szleaf + ti, toff);
                    put_u16(&mut p, 2, toff as u16);
                    p.truncate(toff + ti);
                    self.store.write(id, &p)?;
                    if ti == 0 {
                        self.secure_delete_idx_entry(segid, it.term_leaf_pgno)?;
                    }
                }
            }
        }

        // Close the gap [off, next_off) and rebuild the page footer.
        if next_off < off || next_off > npg {
            return Err(corrupt());
        }
        let nshift = next_off - off;
        a.copy_within(next_off..npg, off);
        pgidx -= nshift;
        let mut nnew = pgidx;
        put_u16(&mut a, 2, pgidx as u16);
        let (mut i, mut key_in, mut prev_out) = (0, 0usize, 0usize);
        while i < nidx {
            key_in += get_varint32(&footer, &mut i) as usize;
            if key_in > npg {
                return Err(corrupt());
            }
            if key_in != del_key_off {
                let key_out = key_in - if key_in > off { nshift } else { 0 };
                let d = key_out.checked_sub(prev_out).ok_or_else(corrupt)?;
                nnew += put_varint_at(&mut a, nnew, d as u64);
                prev_out = key_out;
            }
        }
        if pgidx == nnew && nidx > 0 && it.leaf_pgno != 1 {
            self.secure_delete_idx_entry(segid, it.leaf_pgno)?;
        }
        a.truncate(nnew);
        self.store.write(segment_rowid(segid, it.leaf_pgno), &a)
    }

    // -------------------------------------------------------------- integrity check

    /// sqlite3Fts5IndexIntegrityCheck
    pub(crate) fn integrity_check(
        &self,
        pending: Option<&Pending>,
        cksum: u64,
        use_cksum: bool,
    ) -> Result<()> {
        let (s, _) = self.store.read_structure()?;
        for lvl in &s.levels {
            for seg in &lvl.segs {
                self.integrity_check_segment(seg)?;
            }
        }
        let detail = self.detail();
        let mut cksum2 = 0u64;
        let mut m = self.iter_all(&s, pending, false)?;
        while let Some(e) = m.next()? {
            if detail == Detail::None {
                if e.npos != 0 {
                    cksum2 ^= entry_cksum(e.rowid, 0, 0, -1, &e.term);
                }
            } else {
                let mut off = 0;
                let mut pos = 0i64;
                while let Some(p) = poslist_next(&e.poslist, &mut off, &mut pos) {
                    cksum2 ^= entry_cksum(e.rowid, pos_col(p), pos_off(p), -1, &e.term);
                }
            }
        }
        if use_cksum && cksum != cksum2 {
            return Err(corrupt_msg(format!(
                "fts5: checksum mismatch for table \"{}\"",
                self.cfg.name
            )));
        }
        Ok(())
    }

    fn integrity_check_segment(&self, seg: &Segment) -> Result<()> {
        if seg.pgno_first == 0 {
            return Ok(());
        }
        let secure = self.vals.version == CURRENT_VERSION_SECUREDELETE;
        let rows = sql::query(
            self.store.conn,
            &format!(
                "SELECT segid, term, (pgno>>1), (pgno&1) FROM {} WHERE segid={} ORDER BY 1, 2",
                self.cfg.shadow("idx"),
                seg.segid
            ),
            &[],
        )?;
        let mut idx_prev_leaf = seg.pgno_first - 1;
        let mut dlidx_prev_leaf = seg.pgno_last;
        for row in rows {
            let idx_term = sql::value_bytes(&row[1]);
            let idx_leaf = sql::value_i64(&row[2]) as i32;
            let has_dlidx = sql::value_i64(&row[3]) != 0;
            if idx_leaf < seg.pgno_first {
                continue;
            }
            let id = segment_rowid(seg.segid, idx_leaf);
            let leaf = read_leaf(&self.store, id)?;
            if leaf.nn() <= leaf.szleaf {
                if !(idx_term.is_empty() && secure && leaf.nn() == leaf.szleaf && leaf.nn() == 4) {
                    return Err(corrupt());
                }
            } else {
                let mut off = leaf.first_term_off();
                let roff = leaf.first_rowid_off();
                if roff >= off || off >= leaf.szleaf {
                    return Err(corrupt());
                }
                let nterm = get_varint32(&leaf.p, &mut off) as usize;
                if off + nterm > leaf.szleaf {
                    return Err(corrupt());
                }
                let t = &leaf.p[off..off + nterm];
                if t < idx_term.as_slice() {
                    return Err(corrupt());
                }
                check_pgidx(&leaf)?;
            }
            // Leaves between index entries must exist and hold no terms.
            for i in idx_prev_leaf + 1..idx_leaf {
                let l = read_leaf(&self.store, segment_rowid(seg.segid, i))?;
                if !l.is_termless() || (i > dlidx_prev_leaf && l.first_rowid_off() != 0) {
                    return Err(corrupt());
                }
            }
            if has_dlidx {
                let mut prev_leaf = idx_leaf;
                let mut ipg = 0;
                for (pgno, drowid) in self.dlidx_entries(seg.segid, idx_leaf)? {
                    for p in prev_leaf + 1..pgno {
                        let l = read_leaf(&self.store, segment_rowid(seg.segid, p))?;
                        if l.first_rowid_off() != 0 {
                            return Err(corrupt());
                        }
                    }
                    prev_leaf = pgno;
                    ipg = pgno + 1;
                    let l = read_leaf(&self.store, segment_rowid(seg.segid, pgno))?;
                    let roff = l.first_rowid_off();
                    if roff >= l.szleaf {
                        return Err(corrupt());
                    } else if !secure || roff > 0 {
                        let mut o = roff;
                        let r = get_varint(&l.p, &mut o) as i64;
                        if r < drowid || (!secure && r != drowid) {
                            return Err(corrupt());
                        }
                    }
                }
                dlidx_prev_leaf = ipg;
            } else {
                dlidx_prev_leaf = seg.pgno_last;
            }
            idx_prev_leaf = idx_leaf;
        }
        Ok(())
    }

    /// The (page, first rowid) entries of the doclist index of a leaf.
    fn dlidx_entries(&self, segid: i32, leaf: i32) -> Result<Vec<(i32, i64)>> {
        // Read the leaf level of the dlidx b-tree starting at page `leaf`.
        let mut out = Vec::new();
        let mut pg = leaf;
        loop {
            let Some(data) = self.store.read(dlidx_rowid(segid, 0, pg))? else {
                break;
            };
            let mut off = 1;
            let mut pgno = get_varint32(&data, &mut off) as i32;
            let mut rowid = get_varint(&data, &mut off) as i64;
            out.push((pgno, rowid));
            while off < data.len() {
                let d = get_varint(&data, &mut off);
                pgno += 1;
                if d != 0 {
                    rowid = rowid.wrapping_add(d as i64);
                    out.push((pgno, rowid));
                }
            }
            // Only a lone root page has the flag clear.
            if data[0] & 1 == 0 {
                break;
            }
            pg += 1;
        }
        Ok(out)
    }
}

fn check_pgidx(leaf: &Leaf) -> Result<()> {
    let mut ii = leaf.szleaf;
    let mut term_off = 0usize;
    let mut buf1: Vec<u8> = Vec::new();
    let mut buf2: Vec<u8> = Vec::new();
    let mut first = true;
    while ii < leaf.nn() {
        let incr = get_varint32(&leaf.p, &mut ii) as usize;
        term_off += incr;
        let mut off = term_off;
        if off >= leaf.szleaf {
            return Err(corrupt());
        }
        if first {
            let nbyte = get_varint32(&leaf.p, &mut off) as usize;
            if off + nbyte > leaf.szleaf {
                return Err(corrupt());
            }
            buf1 = leaf.p[off..off + nbyte].to_vec();
        } else {
            let nkeep = get_varint32(&leaf.p, &mut off) as usize;
            let nbyte = get_varint32(&leaf.p, &mut off) as usize;
            if nkeep > buf1.len() || off + nbyte > leaf.szleaf {
                return Err(corrupt());
            }
            buf1.truncate(nkeep);
            buf1.extend_from_slice(&leaf.p[off..off + nbyte]);
            if buf1 <= buf2 {
                return Err(corrupt());
            }
        }
        first = false;
        buf2.clone_from(&buf1);
    }
    Ok(())
}

/// fts5IndexOptimizeStruct
fn optimize_struct(s: &Structure) -> Option<Structure> {
    let nseg = s.nsegment();
    if nseg == 0 {
        return None;
    }
    for lvl in &s.levels {
        let nthis = lvl.segs.len();
        if nthis > 0 && (nthis == nseg || (nthis == nseg - 1 && lvl.nmerge == nthis)) {
            if nseg == 1 && nthis == 1 && lvl.segs[0].npg_tombstone == 0 {
                return None;
            }
            return Some(s.clone());
        }
    }
    let nlevel = (s.levels.len() + 1).min(MAX_LEVEL);
    let mut news = Structure {
        write_counter: s.write_counter,
        origin_cntr: s.origin_cntr,
        levels: vec![Level::default(); nlevel],
    };
    let out = &mut news.levels[nlevel - 1];
    for lvl in s.levels.iter().rev() {
        out.segs.extend(lvl.segs.iter().cloned());
    }
    Some(news)
}

/// sqlite3Fts5IndexEntryCksum
pub(crate) fn entry_cksum(rowid: i64, col: i32, pos: i32, idx: i32, term: &[u8]) -> u64 {
    let mut ret = rowid as u64;
    ret = ret.wrapping_add((ret << 3).wrapping_add(col as i64 as u64));
    ret = ret.wrapping_add((ret << 3).wrapping_add(pos as i64 as u64));
    if idx >= 0 {
        ret = ret.wrapping_add((ret << 3).wrapping_add((MAIN_PREFIX as i32 + idx) as u64));
    }
    for &b in term {
        // pTerm is `const char *`: bytes are sign-extended.
        ret = ret.wrapping_add((ret << 3).wrapping_add(b as i8 as i64 as u64));
    }
    ret
}

/// fts5IndexTombstoneAddToPage. Returns 0 on success, 1 if full, 2 if the
/// key does not fit a 4-byte slot.
fn tombstone_add_to_page(pg: &mut [u8], force: bool, npg: u64, rowid: u64) -> i32 {
    let szkey = if pg[0] == 8 { 8 } else { 4 };
    let nslot = (pg.len() - 8) / szkey;
    let nelem = get_u32(pg, 4) as usize;
    let mut islot = ((rowid / npg) % nslot as u64) as usize;
    let mut ncollide = nslot;
    if szkey == 4 && rowid > 0xFFFF_FFFF {
        return 2;
    }
    if rowid == 0 {
        pg[1] = 1;
        return 0;
    }
    if !force && nelem >= nslot / 2 {
        return 1;
    }
    put_u32(pg, 4, nelem as u32 + 1);
    loop {
        let occupied = if szkey == 4 {
            get_u32(pg, 8 + islot * 4) != 0
        } else {
            get_u64(pg, 8 + islot * 8) != 0
        };
        if !occupied {
            break;
        }
        islot = (islot + 1) % nslot;
        if ncollide == 0 {
            return 0;
        }
        ncollide -= 1;
    }
    if szkey == 4 {
        put_u32(pg, 8 + islot * 4, rowid as u32);
    } else {
        pg[8 + islot * 8..16 + islot * 8].copy_from_slice(&rowid.to_be_bytes());
    }
    0
}

/// Values of an averages record.
pub(crate) fn decode_sizes(blob: &[u8], ncol: usize) -> Option<Vec<i64>> {
    let mut out = Vec::with_capacity(ncol);
    let mut off = 0;
    for _ in 0..ncol {
        if off >= blob.len() {
            return None;
        }
        out.push(get_varint32(blob, &mut off) as i64);
    }
    (off == blob.len()).then_some(out)
}

#[allow(dead_code)]
pub(crate) fn value_or_null(v: Option<Value>) -> Value {
    v.unwrap_or(Value::Null)
}
