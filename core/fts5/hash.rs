//! In-memory pending terms, flushed to a level-0 segment (fts5_hash.c).

use super::config::Detail;
use super::varint::{put_varint, varint_len};
use std::collections::BTreeMap;

/// Approximate per-entry overhead, standing in for sizeof(Fts5HashEntry).
const ENTRY_OVERHEAD: usize = 48;

#[derive(Debug, Clone)]
struct Entry {
    /// Doclist bytes. The poslist size of the last entry is not filled in yet.
    data: Vec<u8>,
    /// Offset of the poslist-size slot of the last rowid (0 when closed).
    sz_poslist: usize,
    open: bool,
    del: bool,
    content: bool,
    col: i32,
    pos: i32,
    rowid: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct Hash {
    detail: Detail,
    entries: BTreeMap<Vec<u8>, Entry>,
    pub(crate) nbyte: usize,
}

impl Hash {
    pub(crate) fn new(detail: Detail) -> Self {
        Self {
            detail,
            entries: BTreeMap::new(),
            nbyte: 0,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.nbyte = 0;
    }

    /// Fills in the size field of the last poslist of `e` (in place when
    /// `close`, otherwise into a copy). Returns the bytes to append.
    fn add_poslist_size(detail: Detail, e: &Entry, data: &mut Vec<u8>) {
        if !e.open {
            return;
        }
        if detail == Detail::None {
            if e.del {
                data.push(0);
                if e.content {
                    data.push(0);
                }
            }
        } else {
            let sz = data.len() - e.sz_poslist - 1;
            let npos = (sz * 2 + e.del as usize) as u64;
            if npos <= 127 {
                data[e.sz_poslist] = npos as u8;
            } else {
                let mut v = Vec::new();
                put_varint(&mut v, npos);
                data.splice(e.sz_poslist..e.sz_poslist + 1, v);
            }
        }
    }

    fn close(detail: Detail, e: &mut Entry) {
        let mut data = std::mem::take(&mut e.data);
        Self::add_poslist_size(detail, e, &mut data);
        e.data = data;
        e.open = false;
        e.sz_poslist = 0;
        e.del = false;
        e.content = false;
    }

    /// Adds (rowid, col, pos) for `token` under index byte `byte`; a negative
    /// `col` records a delete marker (sqlite3Fts5HashWrite).
    pub(crate) fn write(&mut self, rowid: i64, col: i32, pos: i32, byte: u8, token: &[u8]) {
        let detail = self.detail;
        let mut key = Vec::with_capacity(token.len() + 1);
        key.push(byte);
        key.extend_from_slice(token);
        let mut bnew = detail == Detail::Full;
        let before;
        let e = match self.entries.get_mut(&key) {
            Some(e) => {
                before = e.data.len();
                e
            }
            None => {
                let mut data = Vec::with_capacity(64);
                put_varint(&mut data, rowid as u64);
                let sz = data.len();
                if detail != Detail::None {
                    data.push(0);
                }
                before = 0;
                self.nbyte += ENTRY_OVERHEAD + key.len();
                self.entries.entry(key).or_insert(Entry {
                    data,
                    sz_poslist: sz,
                    open: true,
                    del: false,
                    content: false,
                    col: if detail == Detail::Full { 0 } else { -1 },
                    pos: 0,
                    rowid,
                })
            }
        };
        if rowid != e.rowid {
            let diff = (rowid as u64).wrapping_sub(e.rowid as u64);
            Self::close(detail, e);
            put_varint(&mut e.data, diff);
            e.rowid = rowid;
            bnew = true;
            e.sz_poslist = e.data.len();
            e.open = true;
            if detail != Detail::None {
                e.data.push(0);
                e.col = if detail == Detail::Full { 0 } else { -1 };
                e.pos = 0;
            }
        }
        if col >= 0 {
            if detail == Detail::None {
                e.content = true;
            } else {
                let mut pos = pos;
                if col != e.col {
                    if detail == Detail::Full {
                        e.data.push(1);
                        put_varint(&mut e.data, col as u64);
                        e.col = col;
                        e.pos = 0;
                    } else {
                        bnew = true;
                        pos = col;
                        e.col = col;
                    }
                }
                if bnew {
                    put_varint(&mut e.data, (pos - e.pos + 2) as u64);
                    e.pos = pos;
                }
            }
        } else {
            e.del = true;
        }
        self.nbyte += e.data.len().saturating_sub(before);
    }

    /// Sorted (key, doclist) pairs, with the poslist sizes filled in.
    pub(crate) fn scan(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.scan_prefix(&[])
    }

    pub(crate) fn scan_prefix(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.entries
            .range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, e)| (k.clone(), self.doclist(e)))
            .collect()
    }

    fn doclist(&self, e: &Entry) -> Vec<u8> {
        let mut data = e.data.clone();
        Self::add_poslist_size(self.detail, e, &mut data);
        data
    }

    /// The doclist for an exact key, if present.
    pub(crate) fn query(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.entries.get(key).map(|e| self.doclist(e))
    }
}

#[allow(dead_code)]
fn _size(v: u64) -> usize {
    varint_len(v)
}
