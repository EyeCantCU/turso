//! Segment readers (`Fts3SegReader`, `Fts3MultiSegReader` in
//! `fts3_write.c`): iterate the terms of one or more segments, and of the
//! pending terms, merging the doclists of equal terms.

use super::table::Ctx;
use super::{corrupt, get_varint, get_varint32, put_varint};
use crate::Result;

pub(crate) const REQUIRE_POS: i32 = 0x01;
pub(crate) const IGNORE_EMPTY: i32 = 0x02;
pub(crate) const COLUMN_FILTER: i32 = 0x04;
pub(crate) const PREFIX: i32 = 0x08;
pub(crate) const SCAN: i32 = 0x10;
pub(crate) const FIRST: i32 = 0x20;

/// Segment levels: `FTS3_SEGCURSOR_PENDING` and `FTS3_SEGCURSOR_ALL`.
pub(crate) const CURSOR_PENDING: i32 = -1;
pub(crate) const CURSOR_ALL: i32 = -2;

const MAX_BTREE_HEIGHT: i32 = 48;

#[derive(Default, Clone)]
pub(crate) struct SegFilter {
    pub term: Option<Vec<u8>>,
    pub icol: i32,
    pub flags: i32,
}

#[derive(Default)]
pub(crate) struct SegReader {
    pub idx: i32,
    lookup: bool,
    root_only: bool,
    pub start_block: i64,
    pub leaf_end_block: i64,
    pub end_block: i64,
    current_block: i64,
    /// The current node; `None` once the reader is at EOF.
    node: Option<Vec<u8>>,
    /// Pending-terms reader: the remaining (term, doclist) pairs.
    pending: Option<std::vec::IntoIter<(Vec<u8>, Vec<u8>)>>,
    pub term: Vec<u8>,
    doclist: Option<usize>,
    ndoclist: usize,
    offset_list: Option<usize>,
    noffset_list: usize,
    pub docid: i64,
    prev_state: Option<PrevState>,
}

impl SegReader {
    /// `sqlite3Fts3SegReaderNew`.
    pub(crate) fn new(
        age: i32,
        lookup: bool,
        start_leaf: i64,
        end_leaf: i64,
        end_block: i64,
        root: &[u8],
    ) -> Result<Self> {
        let mut r = SegReader {
            idx: age,
            lookup,
            start_block: start_leaf,
            leaf_end_block: end_leaf,
            end_block,
            ..Default::default()
        };
        if start_leaf == 0 {
            if end_leaf != 0 {
                return Err(corrupt());
            }
            r.root_only = true;
            r.node = Some(root.to_vec());
        } else {
            r.current_block = start_leaf - 1;
        }
        Ok(r)
    }

    /// A reader over pending terms, in term order.
    pub(crate) fn pending(items: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
        SegReader {
            idx: 0x7FFFFFFF,
            pending: Some(items.into_iter()),
            ..Default::default()
        }
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn at_eof(&self) -> bool {
        self.node.is_none()
    }

    fn set_eof(&mut self) {
        self.node = None;
    }

    fn node(&self) -> &[u8] {
        self.node.as_deref().unwrap_or(&[])
    }

    pub(crate) fn doclist(&self) -> &[u8] {
        match self.doclist {
            Some(o) => &self.node()[o..o + self.ndoclist],
            None => &[],
        }
    }

    /// `fts3SegReaderNext`: moves to the next term.
    pub(crate) fn next(&mut self, ctx: &Ctx) -> Result<()> {
        let mut pnext = match self.doclist {
            None => 0,
            Some(o) => o + self.ndoclist,
        };
        if self.node.is_none() || pnext >= self.node().len() {
            if let Some(it) = self.pending.as_mut() {
                match it.next() {
                    Some((term, list)) => {
                        self.term = term;
                        self.ndoclist = list.len();
                        self.node = Some(list);
                        self.doclist = Some(0);
                        self.offset_list = None;
                    }
                    None => {
                        self.node = None;
                        self.doclist = None;
                    }
                }
                return Ok(());
            }
            self.set_eof();
            if self.root_only || self.current_block >= self.leaf_end_block {
                self.doclist = None;
                return Ok(());
            }
            self.current_block += 1;
            self.node = Some(ctx.read_block(self.current_block)?);
            self.doclist = None;
            pnext = 0;
        }
        let node = self.node.as_ref().unwrap();
        let (nprefix, n1) = get_varint32(node, pnext);
        pnext += n1;
        let (nsuffix, n2) = get_varint32(node, pnext);
        pnext += n2;
        let (nprefix, nsuffix) = (nprefix as usize, nsuffix as usize);
        if nsuffix == 0 || node.len().saturating_sub(pnext) < nsuffix || nprefix > self.term.len() {
            return Err(corrupt());
        }
        self.term.truncate(nprefix);
        self.term.extend_from_slice(&node[pnext..pnext + nsuffix]);
        pnext += nsuffix;
        let (nd, n3) = get_varint32(node, pnext);
        pnext += n3;
        let nd = nd as usize;
        if nd == 0 || nd > node.len().saturating_sub(pnext) || node[pnext + nd - 1] != 0 {
            return Err(corrupt());
        }
        self.doclist = Some(pnext);
        self.ndoclist = nd;
        self.offset_list = None;
        Ok(())
    }

    /// `fts3SegReaderFirstDocid`.
    pub(crate) fn first_docid(&mut self, desc_idx: bool) -> Result<()> {
        let start = self.doclist.ok_or_else(corrupt)?;
        if desc_idx && self.is_pending() {
            // Pending doclists are ascending; walk them backwards.
            let list = self.doclist().to_vec();
            let mut it = PrevIter::new(&list);
            match it.prev() {
                Some((docid, off, n)) => {
                    self.docid = docid;
                    self.offset_list = Some(start + off);
                    self.noffset_list = n;
                    self.prev_state = Some(it.state());
                }
                None => self.offset_list = None,
            }
        } else {
            let (d, n) = get_varint(self.node(), start);
            self.docid = d;
            self.offset_list = Some(start + n);
        }
        Ok(())
    }

    /// `fts3SegReaderNextDocid`: returns the current position list (without
    /// its terminator) and advances.
    pub(crate) fn next_docid(&mut self, desc_idx: bool) -> Result<Vec<u8>> {
        let p = self.offset_list.ok_or_else(corrupt)?;
        let start = self.doclist.ok_or_else(corrupt)?;
        if desc_idx && self.is_pending() {
            let out = self.node()[p..p + self.noffset_list.saturating_sub(1)].to_vec();
            let list = self.doclist().to_vec();
            let mut it = PrevIter::restore(&list, self.prev_state.take().unwrap_or_default());
            match it.prev() {
                Some((docid, off, n)) => {
                    self.docid = docid;
                    self.offset_list = Some(start + off);
                    self.noffset_list = n;
                    self.prev_state = Some(it.state());
                }
                None => self.offset_list = None,
            }
            return Ok(out);
        }
        let node = self.node.as_ref().unwrap();
        let end = start + self.ndoclist;
        let mut q = p;
        let mut c = 0u8;
        while q < node.len() && (node[q] | c) != 0 {
            c = node[q] & 0x80;
            q += 1;
        }
        let out = node[p..q.min(node.len())].to_vec();
        q += 1;
        while q < end && node[q] == 0 {
            q += 1;
        }
        if q >= end {
            self.offset_list = None;
        } else {
            let (delta, n) = get_varint(node, q);
            self.offset_list = Some(q + n);
            if desc_idx {
                self.docid = (self.docid as u64).wrapping_sub(delta as u64) as i64;
            } else {
                self.docid = (self.docid as u64).wrapping_add(delta as u64) as i64;
            }
        }
        Ok(out)
    }

    pub(crate) fn has_offset_list(&self) -> bool {
        self.offset_list.is_some()
    }
}

/// Backwards iteration over an ascending doclist (`sqlite3Fts3DoclistPrev`
/// on a pending list). Entries are decoded once up front.
#[derive(Default, Clone)]
pub(crate) struct PrevState {
    entries: Vec<(i64, usize, usize)>,
    next: usize,
}

struct PrevIter {
    st: PrevState,
}

impl PrevIter {
    fn new(list: &[u8]) -> Self {
        let mut entries = Vec::new();
        let mut p = 0;
        let mut docid: i64 = 0;
        while p < list.len() {
            let (d, n) = get_varint(list, p);
            docid = (docid as u64).wrapping_add(d as u64) as i64;
            p += n;
            let off = p;
            let mut c = 0u8;
            while p < list.len() && (list[p] | c) != 0 {
                c = list[p] & 0x80;
                p += 1;
            }
            p += 1;
            entries.push((docid, off, p - off));
            while p < list.len() && list[p] == 0 {
                p += 1;
            }
        }
        let next = entries.len();
        Self {
            st: PrevState { entries, next },
        }
    }
    fn restore(_list: &[u8], st: PrevState) -> Self {
        Self { st }
    }
    fn prev(&mut self) -> Option<(i64, usize, usize)> {
        if self.st.next == 0 {
            return None;
        }
        self.st.next -= 1;
        Some(self.st.entries[self.st.next])
    }
    fn state(self) -> PrevState {
        self.st
    }
}

fn term_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.cmp(b)
}

/// `fts3SegReaderCmp`: by term, EOF last, newer segments first.
fn seg_cmp(l: &SegReader, r: &SegReader) -> i64 {
    let mut rc: i64 = if !l.at_eof() && !r.at_eof() {
        match term_cmp(&l.term, &r.term) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }
    } else {
        l.at_eof() as i64 - r.at_eof() as i64
    };
    if rc == 0 {
        rc = r.idx as i64 - l.idx as i64;
    }
    rc
}

fn doclist_cmp(l: &SegReader, r: &SegReader, desc: bool) -> i64 {
    let mut rc = (!l.has_offset_list()) as i64 - (!r.has_offset_list()) as i64;
    if rc == 0 {
        if l.docid == r.docid {
            rc = r.idx as i64 - l.idx as i64;
        } else if desc {
            rc = if l.docid < r.docid { 1 } else { -1 };
        } else {
            rc = if l.docid > r.docid { 1 } else { -1 };
        }
    }
    rc
}

/// `fts3SegReaderSort`: re-sorts after the first `n_suspect` entries moved.
fn seg_sort(segs: &mut [SegReader], n_suspect: usize, cmp: &dyn Fn(&SegReader, &SegReader) -> i64) {
    let n = segs.len();
    let mut n_suspect = n_suspect.min(n);
    if n_suspect == n {
        n_suspect = n_suspect.saturating_sub(1);
    }
    for i in (0..n_suspect).rev() {
        let mut j = i;
        while j + 1 < n {
            if cmp(&segs[j], &segs[j + 1]) < 0 {
                break;
            }
            segs.swap(j, j + 1);
            j += 1;
        }
    }
}

/// `fts3SegReaderTermCmp`.
fn seg_term_cmp(seg: &SegReader, term: &[u8]) -> i64 {
    if seg.at_eof() {
        return 0;
    }
    let n = seg.term.len().min(term.len());
    match seg.term[..n].cmp(&term[..n]) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Equal => seg.term.len() as i64 - term.len() as i64,
    }
}

/// `fts3ColumnFilter`: the part of `list` for column `icol`, keeping its
/// column marker as SQLite does.
pub(crate) fn column_filter(icol: i32, list: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut p = 0;
    let mut cur = 0;
    loop {
        p = skip_collist(list, p);
        if icol == cur {
            return &list[start..p];
        }
        start = p;
        if start >= list.len() {
            return &[];
        }
        let (v, n) = get_varint32(list, start + 1);
        cur = v;
        p = (start + 1 + n).min(list.len());
    }
}

/// `sqlite3Fts3FirstFilter`: keeps only columns whose first position is 0.
fn first_filter(delta: i64, list: &[u8], out: &mut Vec<u8>) -> bool {
    let mut written = false;
    let mut p = 0;
    if list.first() != Some(&0x01) {
        if list.first() == Some(&0x02) {
            put_varint(out, delta);
            out.push(0x02);
            written = true;
        }
        p = skip_collist(list, p);
    }
    while p < list.len() {
        p += 1;
        let (icol, n) = get_varint(list, p);
        p += n;
        if list.get(p) == Some(&0x02) {
            if !written {
                put_varint(out, delta);
                written = true;
            }
            out.push(0x01);
            put_varint(out, icol);
            out.push(0x02);
        }
        p = skip_collist(list, p);
    }
    if written {
        out.push(0x00);
    }
    written
}

/// `fts3ColumnlistCopy` without output: skips one column's positions.
pub(crate) fn skip_collist(list: &[u8], mut p: usize) -> usize {
    let mut c = 0u8;
    while p < list.len() && (0xFE & (list[p] | c)) != 0 {
        c = list[p] & 0x80;
        p += 1;
    }
    p
}

/// `Fts3MultiSegReader`.
#[derive(Default)]
pub(crate) struct MultiSegReader {
    pub segs: Vec<SegReader>,
    nadvance: usize,
    filter: SegFilter,
    pub term: Vec<u8>,
    pub doclist: Vec<u8>,
}

impl MultiSegReader {
    /// `sqlite3Fts3SegReaderStart`.
    pub(crate) fn start(&mut self, ctx: &Ctx, filter: SegFilter) -> Result<()> {
        self.filter = filter;
        let term = self.filter.term.clone();
        for seg in self.segs.iter_mut() {
            let mut res = 0;
            loop {
                seg.next(ctx)?;
                match &term {
                    Some(t) => {
                        res = seg_term_cmp(seg, t);
                        if res >= 0 {
                            break;
                        }
                    }
                    None => break,
                }
            }
            if seg.lookup && res != 0 {
                seg.set_eof();
            }
        }
        let n = self.segs.len();
        seg_sort(&mut self.segs, n, &seg_cmp);
        Ok(())
    }

    /// `sqlite3Fts3SegReaderStep`: advances to the next term; false at EOF.
    pub(crate) fn step(&mut self, ctx: &Ctx) -> Result<bool> {
        let flags = self.filter.flags;
        let ignore_empty = flags & IGNORE_EMPTY != 0;
        let require_pos = flags & REQUIRE_POS != 0;
        let col_filter = flags & COLUMN_FILTER != 0;
        let is_prefix = flags & PREFIX != 0;
        let is_scan = flags & SCAN != 0;
        let is_first = flags & FIRST != 0;
        let desc = ctx.tab.desc;
        if self.segs.is_empty() {
            return Ok(false);
        }
        loop {
            for i in 0..self.nadvance {
                let seg = &mut self.segs[i];
                if seg.lookup {
                    seg.set_eof();
                } else {
                    seg.next(ctx)?;
                }
            }
            let nadv = self.nadvance;
            seg_sort(&mut self.segs, nadv, &seg_cmp);
            self.nadvance = 0;
            if self.segs[0].at_eof() {
                return Ok(false);
            }
            self.term = self.segs[0].term.clone();
            if let Some(ft) = &self.filter.term {
                if !is_scan
                    && (self.term.len() < ft.len()
                        || (!is_prefix && self.term.len() > ft.len())
                        || self.term[..ft.len()] != ft[..])
                {
                    return Ok(false);
                }
            }
            let mut nmerge = 1;
            while nmerge < self.segs.len()
                && !self.segs[nmerge].at_eof()
                && self.segs[nmerge].term == self.term
            {
                nmerge += 1;
            }
            let mut found = false;
            if nmerge == 1 && !ignore_empty && !is_first && (!desc || !self.segs[0].is_pending()) {
                self.doclist = self.segs[0].doclist().to_vec();
                found = true;
            } else {
                let mut out = Vec::new();
                let mut iprev: i64 = 0;
                let cmp = move |l: &SegReader, r: &SegReader| doclist_cmp(l, r, desc);
                for i in 0..nmerge {
                    self.segs[i].first_docid(desc)?;
                }
                seg_sort(&mut self.segs[..nmerge], nmerge, &cmp);
                while self.segs[0].has_offset_list() {
                    let docid = self.segs[0].docid;
                    let list = self.segs[0].next_docid(desc)?;
                    let mut j = 1;
                    while j < nmerge
                        && self.segs[j].has_offset_list()
                        && self.segs[j].docid == docid
                    {
                        self.segs[j].next_docid(desc)?;
                        j += 1;
                    }
                    let list: &[u8] = if col_filter {
                        column_filter(self.filter.icol, &list)
                    } else {
                        &list
                    };
                    if !ignore_empty || !list.is_empty() {
                        let delta = if desc && !out.is_empty() {
                            if iprev <= docid {
                                return Err(corrupt());
                            }
                            (iprev as u64).wrapping_sub(docid as u64) as i64
                        } else {
                            if !out.is_empty() && iprev >= docid {
                                return Err(corrupt());
                            }
                            (docid as u64).wrapping_sub(iprev as u64) as i64
                        };
                        if is_first {
                            if first_filter(delta, list, &mut out) {
                                iprev = docid;
                            }
                        } else {
                            put_varint(&mut out, delta);
                            iprev = docid;
                            if require_pos {
                                out.extend_from_slice(list);
                                out.push(0);
                            }
                        }
                    }
                    seg_sort(&mut self.segs[..nmerge], j, &cmp);
                }
                if !out.is_empty() {
                    self.doclist = out;
                    found = true;
                }
            }
            self.nadvance = nmerge;
            if found {
                return Ok(true);
            }
        }
    }
}

/// `fts3ScanInteriorNode`.
fn scan_interior(
    term: &[u8],
    node: &[u8],
    mut first: Option<&mut i64>,
    mut last: Option<&mut i64>,
) -> Result<()> {
    let mut p = 0;
    let (_, n) = get_varint(node, p);
    p += n;
    let (child, n) = get_varint(node, p);
    p += n;
    let mut ichild = child;
    if p > node.len() {
        return Err(corrupt());
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut is_first = true;
    while p < node.len() && (first.is_some() || last.is_some()) {
        let mut nprefix = 0usize;
        if !is_first {
            let (v, n) = get_varint32(node, p);
            p += n;
            nprefix = v as usize;
            if nprefix > buf.len() {
                return Err(corrupt());
            }
        }
        is_first = false;
        let (v, n) = get_varint32(node, p);
        p += n;
        let nsuffix = v as usize;
        if nprefix > p || nsuffix > node.len().saturating_sub(p) || nsuffix == 0 {
            return Err(corrupt());
        }
        buf.truncate(nprefix);
        buf.extend_from_slice(&node[p..p + nsuffix]);
        p += nsuffix;
        let n = buf.len().min(term.len());
        let cmp = term[..n].cmp(&buf[..n]);
        if first.is_some()
            && (cmp == std::cmp::Ordering::Less
                || (cmp == std::cmp::Ordering::Equal && buf.len() > term.len()))
        {
            **first.as_mut().unwrap() = ichild;
            first = None;
        }
        if last.is_some() && cmp == std::cmp::Ordering::Less {
            **last.as_mut().unwrap() = ichild;
            last = None;
        }
        ichild += 1;
    }
    if let Some(f) = first {
        *f = ichild;
    }
    if let Some(l) = last {
        *l = ichild;
    }
    Ok(())
}

/// `fts3SelectLeaf`: the leaf (range) that may contain `term`.
pub(crate) fn select_leaf(
    ctx: &Ctx,
    term: &[u8],
    node: &[u8],
    leaf: Option<&mut i64>,
    leaf2: Option<&mut i64>,
) -> Result<()> {
    let (height, _) = get_varint32(node, 0);
    if height > MAX_BTREE_HEIGHT {
        return Err(corrupt());
    }
    let mut leaf = leaf;
    let mut leaf2 = leaf2;
    scan_interior(term, node, leaf.as_deref_mut(), leaf2.as_deref_mut())?;
    if height > 1 {
        if let (Some(a), Some(b)) = (leaf.as_deref(), leaf2.as_deref()) {
            if a != b {
                let blob = ctx.read_block(*a)?;
                select_leaf(ctx, term, &blob, leaf.as_deref_mut(), None)?;
                leaf = None;
            }
        }
        let id = match (&leaf, &leaf2) {
            (Some(a), _) => **a,
            (None, Some(b)) => **b,
            _ => return Ok(()),
        };
        let blob = ctx.read_block(id)?;
        let (h2, _) = get_varint32(&blob, 0);
        if h2 >= height {
            return Err(corrupt());
        }
        select_leaf(ctx, term, &blob, leaf, leaf2)?;
    }
    Ok(())
}

/// `fts3SegReaderCursor`: readers for every segment of an index (plus
/// pending terms), positioned for `term`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn seg_reader_cursor(
    ctx: &Ctx,
    langid: i32,
    index: usize,
    level: i32,
    term: Option<&[u8]>,
    is_prefix: bool,
    is_scan: bool,
    csr: &mut MultiSegReader,
) -> Result<()> {
    if level < 0 && !ctx.tab.aux && ctx.prev_langid() == langid {
        if let Some(r) = ctx.pending_reader(index, term, is_prefix || is_scan) {
            csr.segs.push(r);
        }
    }
    if level != CURSOR_PENDING {
        let rows = ctx.all_segdirs(langid, index, level)?;
        for (_idx, start, leaves_end, end, root) in rows {
            let mut start_block = start;
            let mut leaves_end_block = leaves_end;
            if start_block != 0 {
                if let Some(t) = term {
                    if is_prefix {
                        select_leaf(
                            ctx,
                            t,
                            &root,
                            Some(&mut start_block),
                            Some(&mut leaves_end_block),
                        )?;
                    } else {
                        select_leaf(ctx, t, &root, Some(&mut start_block), None)?;
                    }
                    if !is_prefix && !is_scan {
                        leaves_end_block = start_block;
                    }
                }
            }
            let seg = SegReader::new(
                csr.segs.len() as i32 + 1,
                !is_prefix && !is_scan,
                start_block,
                leaves_end_block,
                end,
                &root,
            )?;
            csr.segs.push(seg);
        }
    }
    Ok(())
}

/// Appends a pending doclist entry (`fts3PendingListAppend`).
#[derive(Debug, Clone, Default)]
pub(crate) struct PendingList {
    pub data: Vec<u8>,
    last_docid: i64,
    last_col: i64,
    last_pos: i64,
    started: bool,
}

impl PendingList {
    pub(crate) fn append(&mut self, docid: i64, col: i64, pos: i64) {
        if !self.started || self.last_docid != docid {
            let delta = (docid as u64).wrapping_sub(if self.started {
                self.last_docid as u64
            } else {
                0
            });
            if self.started {
                self.data.push(0);
            }
            put_varint(&mut self.data, delta as i64);
            self.started = true;
            self.last_col = -1;
            self.last_pos = 0;
            self.last_docid = docid;
        }
        if col > 0 && self.last_col != col {
            put_varint(&mut self.data, 1);
            put_varint(&mut self.data, col);
            self.last_col = col;
            self.last_pos = 0;
        }
        if col >= 0 {
            put_varint(&mut self.data, 2 + pos - self.last_pos);
            self.last_pos = pos;
        }
    }

    /// The doclist with its final terminator.
    pub(crate) fn doclist(&self) -> Vec<u8> {
        let mut v = self.data.clone();
        v.push(0);
        v
    }

    /// Bytes counted against the pending-data limit, as SQLite counts them.
    pub(crate) fn n_data(&self) -> usize {
        self.data.len()
    }
}

/// Decodes a position list (without terminator) into columns.
pub(crate) fn decode_poslist(list: &[u8]) -> Vec<(i32, Vec<i64>)> {
    let mut out: Vec<(i32, Vec<i64>)> = Vec::new();
    let mut p = 0;
    let mut col = 0i32;
    let mut cur: Vec<i64> = Vec::new();
    let mut pos = 0i64;
    while p < list.len() {
        let (v, n) = get_varint(list, p);
        p += n;
        match v {
            0 => break,
            1 => {
                if !cur.is_empty() {
                    out.push((col, std::mem::take(&mut cur)));
                }
                let (c, n) = get_varint32(list, p);
                p += n;
                col = c;
                pos = 0;
            }
            _ => {
                pos += v - 2;
                cur.push(pos);
            }
        }
    }
    if !cur.is_empty() {
        out.push((col, cur));
    }
    out
}

/// Decodes a doclist into (docid, position list bytes) in storage order.
pub(crate) fn decode_doclist(list: &[u8], desc: bool, with_pos: bool) -> Vec<(i64, Vec<u8>)> {
    let mut out = Vec::new();
    let mut p = 0;
    let mut docid: i64 = 0;
    let mut first = true;
    while p < list.len() {
        let (d, n) = get_varint(list, p);
        p += n;
        if first || !desc {
            docid = (docid as u64).wrapping_add(d as u64) as i64;
        } else {
            docid = (docid as u64).wrapping_sub(d as u64) as i64;
        }
        first = false;
        if with_pos {
            let s = p;
            let mut c = 0u8;
            while p < list.len() && (list[p] | c) != 0 {
                c = list[p] & 0x80;
                p += 1;
            }
            out.push((docid, list[s..p.min(list.len())].to_vec()));
            p += 1;
        } else {
            out.push((docid, Vec::new()));
        }
    }
    out
}
