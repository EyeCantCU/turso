//! The session extension (sqlite3session_*, sqlite3changeset_*,
//! sqlite3changegroup_*, sqlite3rebaser_*), ported from SQLite's
//! ext/session/sqlite3session.c. Changesets use SQLite's binary format, and
//! the hash-table order matches, so output is byte-identical to SQLite's.

#![allow(clippy::too_many_arguments)]
// Parts are used by a later stage (diff).
#![allow(dead_code)]

use std::ffi::{self, CStr, CString};
use std::ptr;
use turso_ext::Value as ExtValue;

use crate::{
    sqlite3, sqlite3_bind_blob, sqlite3_bind_double, sqlite3_bind_int64, sqlite3_bind_null,
    sqlite3_bind_text, sqlite3_column_blob, sqlite3_column_bytes, sqlite3_column_count,
    sqlite3_column_double, sqlite3_column_int64, sqlite3_column_text, sqlite3_column_type,
    sqlite3_column_value, sqlite3_errmsg, sqlite3_exec, sqlite3_finalize, sqlite3_prepare_v2,
    sqlite3_reset, sqlite3_step, sqlite3_stmt, sqlite3_value_blob, sqlite3_value_bytes,
    sqlite3_value_double, sqlite3_value_int64, sqlite3_value_text, sqlite3_value_type,
};

pub(crate) mod apply;
pub(crate) mod group;
pub(crate) mod iter;

pub(crate) const SQLITE_OK: i32 = 0;
pub(crate) const SQLITE_ERROR: i32 = 1;
pub(crate) const SQLITE_ABORT: i32 = 4;
pub(crate) const SQLITE_NOMEM: i32 = 7;
pub(crate) const SQLITE_CORRUPT: i32 = 11;
pub(crate) const SQLITE_SCHEMA: i32 = 17;
pub(crate) const SQLITE_CONSTRAINT: i32 = 19;
pub(crate) const SQLITE_MISUSE: i32 = 21;
pub(crate) const SQLITE_RANGE: i32 = 25;
pub(crate) const SQLITE_ROW: i32 = 100;
pub(crate) const SQLITE_DONE: i32 = 101;

pub(crate) const SQLITE_INTEGER: u8 = 1;
pub(crate) const SQLITE_FLOAT: u8 = 2;
pub(crate) const SQLITE_TEXT: u8 = 3;
pub(crate) const SQLITE_BLOB: u8 = 4;
pub(crate) const SQLITE_NULL: u8 = 5;

pub(crate) const SQLITE_DELETE: u8 = 9;
pub(crate) const SQLITE_INSERT: u8 = 18;
pub(crate) const SQLITE_UPDATE: u8 = 23;

const SQLITE_SESSION_OBJCONFIG_SIZE: i32 = 1;
const SQLITE_SESSION_OBJCONFIG_ROWID: i32 = 2;
const SQLITE_SESSION_CONFIG_STRMSIZE: i32 = 1;

pub(crate) const SESSIONS_ROWID: &str = "_rowid_";

/// Chunk size of the streaming functions (SESSIONS_STRM_CHUNK_SIZE).
static STRM_CHUNK_SIZE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1024);

pub(crate) fn strm_chunk_size() -> i32 {
    STRM_CHUNK_SIZE.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) type XOutput =
    unsafe extern "C" fn(*mut ffi::c_void, *const ffi::c_void, ffi::c_int) -> ffi::c_int;
pub(crate) type XInput =
    unsafe extern "C" fn(*mut ffi::c_void, *mut ffi::c_void, *mut ffi::c_int) -> ffi::c_int;

/// SQLite's corruption code; SQLITE_CORRUPT_BKPT.
pub(crate) const CORRUPT: i32 = SQLITE_CORRUPT;

// ---------------------------------------------------------------------------
// Varints, integers and the record format.

/// putVarint32.
pub(crate) fn put_varint(buf: &mut Vec<u8>, v: u32) {
    let mut tmp = [0u8; 10];
    let n = turso_core::storage::sqlite3_ondisk::write_varint(&mut tmp, v as u64);
    buf.extend_from_slice(&tmp[..n]);
}

pub(crate) fn varint_len(v: u32) -> usize {
    let mut tmp = [0u8; 10];
    turso_core::storage::sqlite3_ondisk::write_varint(&mut tmp, v as u64)
}

/// sessionVarintGet: a varint masked to 31 bits; reads past the end as 0.
pub(crate) fn get_varint(a: &[u8]) -> (i32, usize) {
    let b = |i: usize| a.get(i).copied().unwrap_or(0) as u64;
    // getVarint32 fast paths, then the general decoder truncated to 32 bits.
    let (v, n): (u64, usize) = if b(0) < 0x80 {
        (b(0), 1)
    } else if b(1) < 0x80 {
        (((b(0) & 0x7f) << 7) | b(1), 2)
    } else if b(2) < 0x80 {
        (((b(0) & 0x7f) << 14) | ((b(1) & 0x7f) << 7) | b(2), 3)
    } else {
        let mut v: u64 = 0;
        let mut n = 9;
        for i in 0..8 {
            let x = b(i);
            v = (v << 7) | (x & 0x7f);
            if x & 0x80 == 0 {
                n = i + 1;
                break;
            }
        }
        if n == 9 {
            v = (v << 8) | b(8);
        }
        let v32 = if v > u32::MAX as u64 {
            u32::MAX as u64
        } else {
            v
        };
        (v32, n)
    };
    (((v as u32) & 0x7FFF_FFFF) as i32, n)
}

/// sessionVarintGetSafe.
pub(crate) fn get_varint_safe(a: &[u8], n: usize) -> (i32, usize) {
    if n < 9 {
        let mut copy = [0u8; 9];
        copy[..n].copy_from_slice(&a[..n]);
        get_varint(&copy)
    } else {
        get_varint(a)
    }
}

pub(crate) fn get_i64(a: &[u8]) -> i64 {
    let mut b = [0u8; 8];
    let n = a.len().min(8);
    b[..n].copy_from_slice(&a[..n]);
    i64::from_be_bytes(b)
}

pub(crate) fn put_i64(buf: &mut Vec<u8>, i: i64) {
    buf.extend_from_slice(&i.to_be_bytes());
}

/// sessionSerialLen: bytes taken by the serialized value at `a`.
pub(crate) fn serial_len(a: &[u8]) -> usize {
    match a.first().copied().unwrap_or(0) {
        SQLITE_INTEGER | SQLITE_FLOAT => 9,
        SQLITE_TEXT | SQLITE_BLOB => {
            let (n, nv) = get_varint(&a[1..]);
            1 + nv + n as usize
        }
        _ => 1,
    }
}

/// A column value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Val {
    Null,
    Int(i64),
    Float(f64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

impl Val {
    pub(crate) fn type_code(&self) -> u8 {
        match self {
            Val::Null => SQLITE_NULL,
            Val::Int(_) => SQLITE_INTEGER,
            Val::Float(_) => SQLITE_FLOAT,
            Val::Text(_) => SQLITE_TEXT,
            Val::Blob(_) => SQLITE_BLOB,
        }
    }

    /// Reads a `sqlite3_value*` (an ExtValue).
    pub(crate) unsafe fn from_ptr(p: *mut ffi::c_void) -> Option<Val> {
        if p.is_null() {
            return None;
        }
        Some(match sqlite3_value_type(p) as u8 {
            SQLITE_INTEGER => Val::Int(sqlite3_value_int64(p)),
            SQLITE_FLOAT => Val::Float(sqlite3_value_double(p)),
            SQLITE_TEXT => {
                let n = sqlite3_value_bytes(p) as usize;
                let z = sqlite3_value_text(p);
                Val::Text(if z.is_null() || n == 0 {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(z, n).to_vec()
                })
            }
            SQLITE_BLOB => {
                let n = sqlite3_value_bytes(p) as usize;
                let z = sqlite3_value_blob(p) as *const u8;
                Val::Blob(if z.is_null() || n == 0 {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(z, n).to_vec()
                })
            }
            _ => Val::Null,
        })
    }

    pub(crate) fn to_ext(&self) -> ExtValue {
        match self {
            Val::Null => ExtValue::null(),
            Val::Int(i) => ExtValue::from_integer(*i),
            Val::Float(f) => ExtValue::from_float(*f),
            Val::Text(t) => ExtValue::from_text(String::from_utf8_lossy(t).into_owned()),
            Val::Blob(b) => ExtValue::from_blob(b.clone()),
        }
    }

    /// Column `i` of a statement's current row.
    pub(crate) unsafe fn from_column(stmt: *mut sqlite3_stmt, i: i32) -> Val {
        match sqlite3_column_type(stmt, i) as u8 {
            SQLITE_INTEGER => Val::Int(sqlite3_column_int64(stmt, i)),
            SQLITE_FLOAT => Val::Float(sqlite3_column_double(stmt, i)),
            SQLITE_TEXT => {
                let z = sqlite3_column_text(stmt, i);
                let n = sqlite3_column_bytes(stmt, i) as usize;
                Val::Text(if z.is_null() || n == 0 {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(z, n).to_vec()
                })
            }
            SQLITE_BLOB => {
                let z = sqlite3_column_blob(stmt, i) as *const u8;
                let n = sqlite3_column_bytes(stmt, i) as usize;
                Val::Blob(if z.is_null() || n == 0 {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(z, n).to_vec()
                })
            }
            _ => Val::Null,
        }
    }

    /// Binds this value to parameter `i`.
    pub(crate) unsafe fn bind(&self, stmt: *mut sqlite3_stmt, i: i32) -> i32 {
        match self {
            Val::Null => sqlite3_bind_null(stmt, i),
            Val::Int(v) => sqlite3_bind_int64(stmt, i, *v),
            Val::Float(v) => sqlite3_bind_double(stmt, i, *v),
            Val::Text(t) => sqlite3_bind_text(
                stmt,
                i,
                t.as_ptr() as *const ffi::c_char,
                t.len() as i32,
                transient(),
            ),
            Val::Blob(b) => sqlite3_bind_blob(
                stmt,
                i,
                b.as_ptr() as *const ffi::c_void,
                b.len() as i32,
                transient(),
            ),
        }
    }

    /// Serialized size (sessionSerializeValue with aBuf == NULL).
    pub(crate) fn serialized_len(v: Option<&Val>) -> i64 {
        match v {
            None | Some(Val::Null) => 1,
            Some(Val::Int(_)) | Some(Val::Float(_)) => 9,
            Some(Val::Text(t)) | Some(Val::Blob(t)) => {
                1 + varint_len(t.len() as u32) as i64 + t.len() as i64
            }
        }
    }

    /// sessionSerializeValue.
    pub(crate) fn serialize(v: Option<&Val>, buf: &mut Vec<u8>) {
        match v {
            None => buf.push(0),
            Some(Val::Null) => buf.push(SQLITE_NULL),
            Some(Val::Int(i)) => {
                buf.push(SQLITE_INTEGER);
                put_i64(buf, *i);
            }
            Some(Val::Float(f)) => {
                buf.push(SQLITE_FLOAT);
                put_i64(buf, f.to_bits() as i64);
            }
            Some(Val::Text(t)) => {
                buf.push(SQLITE_TEXT);
                put_varint(buf, t.len() as u32);
                buf.extend_from_slice(t);
            }
            Some(Val::Blob(t)) => {
                buf.push(SQLITE_BLOB);
                put_varint(buf, t.len() as u32);
                buf.extend_from_slice(t);
            }
        }
    }

    /// Reads one serialized value; None for "undefined" (0x00).
    pub(crate) fn deserialize(a: &[u8]) -> Option<Val> {
        match a.first().copied().unwrap_or(0) {
            SQLITE_INTEGER => Some(Val::Int(get_i64(&a[1..]))),
            SQLITE_FLOAT => Some(Val::Float(f64::from_bits(get_i64(&a[1..]) as u64))),
            t @ (SQLITE_TEXT | SQLITE_BLOB) => {
                let (n, nv) = get_varint(&a[1..]);
                let start = (1 + nv).min(a.len());
                let end = (start + n as usize).min(a.len());
                let bytes = a[start..end].to_vec();
                Some(if t == SQLITE_TEXT {
                    Val::Text(bytes)
                } else {
                    Val::Blob(bytes)
                })
            }
            SQLITE_NULL => Some(Val::Null),
            _ => None,
        }
    }
}

pub(crate) fn transient() -> Option<unsafe extern "C" fn(*mut ffi::c_void)> {
    // SQLITE_TRANSIENT is (destructor)-1.
    unsafe { std::mem::transmute::<isize, Option<unsafe extern "C" fn(*mut ffi::c_void)>>(-1) }
}

/// Copies `data` into a buffer from sqlite3_malloc (NULL for empty data).
pub(crate) unsafe fn to_sqlite_buf(data: &[u8]) -> *mut ffi::c_void {
    if data.is_empty() {
        return ptr::null_mut();
    }
    let p = libc::malloc(data.len()) as *mut u8;
    if !p.is_null() {
        ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
    }
    p as *mut ffi::c_void
}

pub(crate) unsafe fn mprintf(s: &str) -> *mut ffi::c_char {
    let bytes = s.as_bytes();
    let p = libc::malloc(bytes.len() + 1) as *mut u8;
    if !p.is_null() {
        ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        *p.add(bytes.len()) = 0;
    }
    p as *mut ffi::c_char
}

pub(crate) unsafe fn cstr(p: *const ffi::c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// %q: double single quotes.
pub(crate) fn q(s: &str) -> String {
    s.replace('\'', "''")
}

/// %w: double double quotes.
pub(crate) fn w(s: &str) -> String {
    s.replace('"', "\"\"")
}

/// %Q: quoted literal.
pub(crate) fn qq(s: &str) -> String {
    format!("'{}'", q(s))
}

/// sessionAppendIdent.
pub(crate) fn ident(s: &str) -> String {
    format!("\"{}\"", w(s))
}

// ---------------------------------------------------------------------------
// Statements.

/// A prepared statement on our own C API, finalized on drop.
pub(crate) struct Stmt(pub(crate) *mut sqlite3_stmt);

impl Stmt {
    pub(crate) unsafe fn prepare(db: *mut sqlite3, sql: &str) -> Result<Stmt, i32> {
        let c = CString::new(sql).map_err(|_| SQLITE_ERROR)?;
        let mut stmt = ptr::null_mut();
        let rc = sqlite3_prepare_v2(db, c.as_ptr(), -1, &mut stmt, ptr::null_mut());
        if rc != SQLITE_OK {
            if !stmt.is_null() {
                sqlite3_finalize(stmt);
            }
            return Err(rc);
        }
        Ok(Stmt(stmt))
    }

    pub(crate) unsafe fn step(&self) -> i32 {
        sqlite3_step(self.0)
    }

    pub(crate) unsafe fn reset(&self) -> i32 {
        sqlite3_reset(self.0)
    }

    pub(crate) unsafe fn finalize(mut self) -> i32 {
        let rc = sqlite3_finalize(self.0);
        self.0 = ptr::null_mut();
        rc
    }

    pub(crate) unsafe fn col_int(&self, i: i32) -> i64 {
        sqlite3_column_int64(self.0, i)
    }

    pub(crate) unsafe fn col_text(&self, i: i32) -> Option<String> {
        let z = sqlite3_column_text(self.0, i);
        if z.is_null() {
            return None;
        }
        let n = sqlite3_column_bytes(self.0, i) as usize;
        Some(String::from_utf8_lossy(std::slice::from_raw_parts(z, n)).into_owned())
    }

    pub(crate) unsafe fn col(&self, i: i32) -> Val {
        Val::from_column(self.0, i)
    }

    pub(crate) unsafe fn col_count(&self) -> i32 {
        sqlite3_column_count(self.0)
    }
}

impl Drop for Stmt {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { sqlite3_finalize(self.0) };
        }
    }
}

pub(crate) unsafe fn exec(db: *mut sqlite3, sql: &str) -> i32 {
    let c = CString::new(sql).unwrap_or_default();
    sqlite3_exec(db, c.as_ptr(), None, ptr::null_mut(), ptr::null_mut())
}

pub(crate) unsafe fn errmsg(db: *mut sqlite3) -> String {
    cstr(sqlite3_errmsg(db))
}

// ---------------------------------------------------------------------------
// Hashing (must match SQLite to reproduce its change order).

fn hash_append(h: u32, add: u32) -> u32 {
    (h << 3) ^ h ^ add
}

fn hash_i64(h: u32, i: i64) -> u32 {
    let h = hash_append(h, (i & 0xFFFF_FFFF) as u32);
    hash_append(h, ((i >> 32) & 0xFFFF_FFFF) as u32)
}

fn hash_blob(mut h: u32, z: &[u8]) -> u32 {
    for b in z {
        h = hash_append(h, *b as u32);
    }
    h
}

/// Appends one PK value to a hash; the flag is true for NULL.
pub(crate) fn hash_val(h: u32, v: &Val) -> (u32, bool) {
    let h = hash_append(h, v.type_code() as u32);
    match v {
        Val::Int(i) => (hash_i64(h, *i), false),
        Val::Float(f) => (hash_i64(h, f.to_bits() as i64), false),
        Val::Text(t) | Val::Blob(t) => (hash_blob(h, t), false),
        Val::Null => (h, true),
    }
}

// ---------------------------------------------------------------------------
// Tables.

#[derive(Default)]
pub(crate) struct SessionChange {
    pub(crate) op: u8,
    pub(crate) indirect: bool,
    pub(crate) n_record_field: usize,
    pub(crate) n_max_size: i64,
    pub(crate) record: Vec<u8>,
}

/// A table tracked by a session or a changegroup.
#[derive(Default)]
pub(crate) struct SessionTable {
    pub(crate) name: String,
    /// Columns, not counting hidden ones.
    pub(crate) n_col: usize,
    pub(crate) n_total_col: usize,
    pub(crate) stat1: bool,
    pub(crate) rowid: bool,
    pub(crate) cols: Vec<String>,
    pub(crate) dflt: Vec<Option<String>>,
    pub(crate) idx: Vec<i32>,
    /// Empty when the table has no primary key.
    pub(crate) pk: Vec<u8>,
    pub(crate) n_entry: usize,
    /// Hash buckets; each chain's head is its last element.
    pub(crate) buckets: Vec<Vec<SessionChange>>,
    /// SELECT of the column defaults (changegroups only).
    pub(crate) dflt_stmt: Option<Stmt>,
}

impl SessionTable {
    pub(crate) fn new(name: &str) -> Self {
        SessionTable {
            name: name.to_string(),
            ..Default::default()
        }
    }

    pub(crate) fn n_change(&self) -> usize {
        self.buckets.len()
    }

    /// sessionChangeHash.
    pub(crate) fn change_hash(&self, pk_only: bool, rec: &[u8], n_bucket: usize) -> usize {
        let mut h: u32 = 0;
        let mut a = 0usize;
        for i in 0..self.n_col {
            let is_pk = self.pk.get(i).copied().unwrap_or(0) != 0;
            if pk_only && !is_pk {
                continue;
            }
            if is_pk {
                let t = rec.get(a).copied().unwrap_or(0);
                a += 1;
                h = hash_append(h, t as u32);
                if t == SQLITE_INTEGER || t == SQLITE_FLOAT {
                    h = hash_i64(h, get_i64(rec.get(a..).unwrap_or(&[])));
                    a += 8;
                } else if t == SQLITE_TEXT || t == SQLITE_BLOB {
                    let (n, nv) = get_varint(rec.get(a..).unwrap_or(&[]));
                    a += nv;
                    let end = (a + n as usize).min(rec.len());
                    h = hash_blob(h, rec.get(a.min(end)..end).unwrap_or(&[]));
                    a += n as usize;
                }
            } else {
                a += serial_len(rec.get(a..).unwrap_or(&[]));
            }
        }
        (h as usize) % n_bucket
    }

    /// sessionGrowHash.
    pub(crate) fn grow_hash(&mut self, patchset: bool) {
        let n_change = self.n_change();
        if n_change == 0 || self.n_entry >= n_change / 2 {
            let n_new = 2 * if n_change > 0 { n_change } else { 128 };
            let mut new: Vec<Vec<SessionChange>> = (0..n_new).map(|_| Vec::new()).collect();
            let old = std::mem::take(&mut self.buckets);
            for chain in old {
                for p in chain.into_iter().rev() {
                    let pk_only = p.op == SQLITE_DELETE && patchset;
                    let h = self.change_hash(pk_only, &p.record, n_new);
                    new[h].push(p);
                }
            }
            self.buckets = new;
        }
    }

    /// Every change in SQLite's iteration order.
    pub(crate) fn changes(&self) -> impl Iterator<Item = &SessionChange> {
        self.buckets.iter().flat_map(|c| c.iter().rev())
    }

    pub(crate) fn has_pk(&self) -> bool {
        !self.pk.is_empty()
    }
}

/// Column details from `PRAGMA table_xinfo` (sessionTableInfo).
pub(crate) struct TableInfo {
    pub(crate) n_col: usize,
    pub(crate) n_total_col: usize,
    pub(crate) cols: Vec<String>,
    pub(crate) dflt: Vec<Option<String>>,
    pub(crate) idx: Vec<i32>,
    pub(crate) pk: Vec<u8>,
    pub(crate) rowid: bool,
}

/// sessionTableInfo. `want_rowid` is pbRowid != NULL.
pub(crate) unsafe fn table_info(
    db: *mut sqlite3,
    zdb: &str,
    table: &str,
    want_rowid: bool,
) -> Result<TableInfo, i32> {
    let sql = if table.eq_ignore_ascii_case("sqlite_stat1") {
        let exists = crate::sqlite3_table_column_metadata(
            db,
            CString::new(zdb).unwrap_or_default().as_ptr(),
            CString::new(table).unwrap_or_default().as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        );
        if exists == SQLITE_OK {
            "SELECT 0, 'tbl',  '', 0, '', 1, 0     UNION ALL \
             SELECT 1, 'idx',  '', 0, '', 2, 0     UNION ALL \
             SELECT 2, 'stat', '', 0, '', 0, 0"
                .to_string()
        } else if exists == SQLITE_ERROR {
            String::new()
        } else {
            return Err(exists);
        }
    } else {
        format!("PRAGMA '{}'.table_xinfo('{}')", q(zdb), q(table))
    };
    let mut info = TableInfo {
        n_col: 0,
        n_total_col: 0,
        cols: Vec::new(),
        dflt: Vec::new(),
        idx: Vec::new(),
        pk: Vec::new(),
        rowid: false,
    };
    if sql.is_empty() {
        // Preparing "" yields no statement: no columns.
        return Ok(info);
    }
    let stmt = Stmt::prepare(db, &sql)?;
    let mut rows: Vec<(i32, String, Option<String>, i64, bool)> = Vec::new();
    let mut rowid = want_rowid;
    loop {
        let rc = stmt.step();
        if rc != SQLITE_ROW {
            if rc != SQLITE_DONE {
                return Err(rc);
            }
            break;
        }
        let hidden = stmt.col_int(6) != 0;
        let pk = stmt.col_int(5);
        if pk != 0 {
            rowid = false;
        }
        rows.push((
            stmt.col_int(0) as i32,
            stmt.col_text(1).unwrap_or_default(),
            stmt.col_text(4),
            pk,
            hidden,
        ));
    }
    let n_visible = rows.iter().filter(|r| !r.4).count();
    if n_visible == 0 {
        rowid = false;
    }
    if rowid {
        info.cols.push(SESSIONS_ROWID.to_string());
        info.dflt.push(None);
        info.pk.push(1);
        info.idx.push(-1);
    }
    for (cid, name, dflt, pk, hidden) in rows {
        if !hidden {
            info.cols.push(name);
            info.dflt.push(dflt);
            info.pk.push(pk as u8);
            info.idx.push(cid);
        }
        info.n_total_col += 1;
    }
    info.n_col = info.cols.len();
    info.rowid = rowid;
    Ok(info)
}

// ---------------------------------------------------------------------------
// Sessions.

/// Where sessionPreupdateOneChange reads old/new values from.
enum Hook {
    /// The preupdate hook of `db`.
    Preupdate,
    /// A diff query row: new.* at bRowid.., old.* at nOldOff+bRowid...
    Diff {
        stmt: *mut sqlite3_stmt,
        rowid: bool,
        old_off: usize,
    },
}

pub struct Sqlite3Session {
    db: *mut sqlite3,
    zdb: String,
    enable_size: bool,
    enable: bool,
    indirect: bool,
    auto_attach: i32,
    implicit_pk: bool,
    rc: i32,
    filter_ctx: *mut ffi::c_void,
    filter: Option<unsafe extern "C" fn(*mut ffi::c_void, *const ffi::c_char) -> ffi::c_int>,
    n_malloc: i64,
    n_max_changeset_size: i64,
    next: *mut Sqlite3Session,
    tables: Vec<SessionTable>,
    hook: Hook,
}

impl Sqlite3Session {
    /// Approximate heap accounting for sqlite3session_memory_used.
    fn account(&mut self, n: i64) {
        self.n_malloc += n;
    }

    unsafe fn hook_count(&self) -> usize {
        match &self.hook {
            Hook::Preupdate => crate::hooks::sqlite3_preupdate_count(self.db) as usize,
            Hook::Diff {
                stmt,
                rowid,
                old_off,
            } => {
                let n = if *old_off > 0 {
                    *old_off
                } else {
                    sqlite3_column_count(*stmt) as usize
                };
                n - *rowid as usize
            }
        }
    }

    unsafe fn hook_depth(&self) -> i32 {
        match &self.hook {
            Hook::Preupdate => crate::hooks::sqlite3_preupdate_depth(self.db),
            Hook::Diff { .. } => 0,
        }
    }

    /// xOld / xNew, with the sqlite_stat1 NULL-idx substitution.
    unsafe fn hook_val(&self, new: bool, i: i32, stat1: bool) -> Result<Option<Val>, i32> {
        let v = match &self.hook {
            Hook::Preupdate => {
                let mut p = ptr::null_mut();
                let rc = if new {
                    crate::hooks::sqlite3_preupdate_new(self.db, i, &mut p)
                } else {
                    crate::hooks::sqlite3_preupdate_old(self.db, i, &mut p)
                };
                if rc != SQLITE_OK {
                    return Err(rc);
                }
                Val::from_ptr(p)
            }
            Hook::Diff {
                stmt,
                rowid,
                old_off,
            } => {
                let col = i + *rowid as i32 + if new { 0 } else { *old_off as i32 };
                Some(Val::from_column(*stmt, col))
            }
        };
        if stat1 && i == 1 && matches!(v, Some(Val::Null)) {
            return Ok(Some(Val::Blob(Vec::new())));
        }
        Ok(v)
    }

    /// sessionInitTable for table `ti`. Returns true when changes to it
    /// must be ignored (error or no primary key).
    unsafe fn init_table(&mut self, ti: usize) -> bool {
        let mut rc = SQLITE_OK;
        if self.tables[ti].n_col == 0 {
            let name = self.tables[ti].name.clone();
            match table_info(self.db, &self.zdb, &name, self.implicit_pk) {
                Ok(info) => {
                    let t = &mut self.tables[ti];
                    t.n_col = info.n_col;
                    t.n_total_col = info.n_total_col;
                    t.cols = info.cols;
                    t.dflt = info.dflt;
                    t.idx = info.idx;
                    t.rowid = info.rowid;
                    t.pk = if info.pk.iter().any(|p| *p != 0) {
                        info.pk
                    } else {
                        Vec::new()
                    };
                    if t.name.eq_ignore_ascii_case("sqlite_stat1") {
                        t.stat1 = true;
                    }
                    let add = 1
                        + varint_len(t.n_col as u32) as i64
                        + t.n_col as i64
                        + t.name.len() as i64
                        + 1;
                    let mem = (t.cols.iter().map(|c| c.len() + 24).sum::<usize>()) as i64;
                    if self.enable_size {
                        self.n_max_changeset_size += add;
                    }
                    self.account(mem);
                }
                Err(e) => rc = e,
            }
        }
        self.rc = rc;
        rc != SQLITE_OK || !self.tables[ti].has_pk()
    }

    /// sessionReinitTable.
    unsafe fn reinit_table(&mut self, ti: usize) -> i32 {
        let name = self.tables[ti].name.clone();
        let info = match table_info(self.db, &self.zdb, &name, self.implicit_pk) {
            Ok(i) => i,
            Err(e) => {
                self.rc = e;
                return e;
            }
        };
        let t = &mut self.tables[ti];
        if t.n_col > info.n_col || t.rowid != info.rowid {
            self.rc = SQLITE_SCHEMA;
        } else {
            let n_old = t.n_col;
            for ii in 0..info.n_col {
                if ii < t.n_col {
                    if t.pk.get(ii).copied().unwrap_or(0) != info.pk[ii] {
                        self.rc = SQLITE_SCHEMA;
                    }
                } else if info.pk[ii] != 0 {
                    self.rc = SQLITE_SCHEMA;
                }
            }
            if self.rc == SQLITE_OK {
                t.cols = info.cols;
                t.n_col = info.n_col;
                t.n_total_col = info.n_total_col;
                t.dflt = info.dflt;
                t.pk = info.pk;
                t.idx = info.idx;
            }
            if self.enable_size {
                self.n_max_changeset_size += info.n_col as i64 - n_old as i64;
                self.n_max_changeset_size += varint_len(info.n_col as u32) as i64;
                self.n_max_changeset_size -= varint_len(n_old as u32) as i64;
            }
        }
        self.rc
    }

    /// sessionUpdateChanges: pad old.* records to the current column count
    /// with the columns' default values.
    unsafe fn update_changes(&mut self, ti: usize) -> i32 {
        let t = &self.tables[ti];
        let rc = match prepare_dflt_stmt(self.db, t) {
            Ok(stmt) => {
                if stmt.step() == SQLITE_ROW {
                    let n_col = t.n_col;
                    let mut incr_total = 0i64;
                    let t = &mut self.tables[ti];
                    for chain in t.buckets.iter_mut() {
                        for c in chain.iter_mut() {
                            incr_total += update_one_change(c, n_col, &stmt);
                        }
                    }
                    self.n_max_changeset_size += incr_total;
                }
                stmt.finalize()
            }
            Err(e) => e,
        };
        self.rc = rc;
        rc
    }

    /// sessionPreupdateHash: (bucket, has NULL PK).
    unsafe fn preupdate_hash(
        &self,
        ti: usize,
        rowid: i64,
        new: bool,
    ) -> Result<(usize, bool), i32> {
        let t = &self.tables[ti];
        let mut h: u32 = 0;
        let mut null_pk = false;
        if t.rowid {
            h = hash_i64(h, rowid);
        } else {
            for i in 0..t.n_col {
                if t.pk[i] != 0 {
                    let v = self.hook_val(new, t.idx[i], t.stat1)?.unwrap_or(Val::Null);
                    let (nh, is_null) = hash_val(h, &v);
                    h = nh;
                    null_pk |= is_null;
                }
            }
        }
        Ok(((h as usize) % t.n_change(), null_pk))
    }

    /// sessionPreupdateEqual.
    unsafe fn preupdate_equal(&self, ti: usize, rowid: i64, c: &SessionChange, op: u8) -> bool {
        let t = &self.tables[ti];
        let a = &c.record;
        if t.rowid {
            if a.first() != Some(&SQLITE_INTEGER) {
                return false;
            }
            return get_i64(&a[1..]) == rowid;
        }
        let mut off = 0;
        for i in 0..t.n_col {
            if t.pk[i] == 0 {
                off += serial_len(&a[off..]);
                continue;
            }
            let rec = Val::deserialize(&a[off..]);
            off += serial_len(&a[off..]);
            let v = match self.hook_val(op == SQLITE_INSERT, t.idx[i], t.stat1) {
                Ok(v) => v.unwrap_or(Val::Null),
                Err(_) => return false,
            };
            let Some(rec) = rec else { return false };
            if rec.type_code() != v.type_code() {
                return false;
            }
            let eq = match (&rec, &v) {
                (Val::Int(a), Val::Int(b)) => a == b,
                (Val::Float(a), Val::Float(b)) => a == b,
                (Val::Text(a), Val::Text(b)) | (Val::Blob(a), Val::Blob(b)) => a == b,
                _ => false,
            };
            if !eq {
                return false;
            }
        }
        true
    }

    /// sessionUpdateMaxSize.
    unsafe fn update_max_size(&mut self, op: u8, ti: usize, bucket: usize, ci: usize) -> i32 {
        let t = &self.tables[ti];
        let c = &t.buckets[bucket][ci];
        let mut n_new: i64 = 2;
        if c.op == SQLITE_INSERT {
            if t.rowid {
                n_new += 9;
            }
            if op != SQLITE_DELETE {
                for ii in 0..t.n_col {
                    let p = self.hook_val(true, t.idx[ii], t.stat1).ok().flatten();
                    n_new += Val::serialized_len(p.as_ref());
                }
            }
        } else if op == SQLITE_DELETE {
            n_new += c.record.len() as i64;
            if matches!(self.hook, Hook::Preupdate)
                && crate::hooks::sqlite3_preupdate_blobwrite(self.db) >= 0
            {
                n_new += c.record.len() as i64;
            }
        } else {
            let mut csr = 0usize;
            let rec = &c.record;
            if t.rowid {
                n_new += 9 + 1;
                csr += 9;
            }
            for ii in (t.rowid as usize)..t.n_col {
                let p = match self.hook_val(true, t.idx[ii], t.stat1) {
                    Ok(Some(p)) => p,
                    _ => return SQLITE_NOMEM,
                };
                let e = rec.get(csr).copied().unwrap_or(0);
                csr += 1;
                let mut changed = true;
                let mut n_old: i64 = 0;
                match e {
                    SQLITE_NULL => changed = p.type_code() != SQLITE_NULL,
                    SQLITE_FLOAT | SQLITE_INTEGER => {
                        if e == p.type_code() {
                            let iv = get_i64(&rec[csr..]);
                            changed = match &p {
                                Val::Int(x) => iv != *x,
                                Val::Float(x) => f64::from_bits(iv as u64) != *x,
                                _ => true,
                            };
                        }
                        n_old = 8;
                        csr += 8;
                    }
                    _ => {
                        let (n, nv) = get_varint(&rec[csr..]);
                        n_old = nv as i64;
                        csr += nv;
                        n_old += n as i64;
                        let bytes = &rec[csr..(csr + n as usize).min(rec.len())];
                        if e == p.type_code() {
                            if let Val::Text(x) | Val::Blob(x) = &p {
                                if x.as_slice() == bytes {
                                    changed = false;
                                }
                            }
                        }
                        csr += n as usize;
                    }
                }
                if changed && t.pk[ii] != 0 {
                    n_new = c.record.len() as i64 + 2;
                    break;
                }
                if changed {
                    n_new += 1 + n_old;
                    n_new += Val::serialized_len(Some(&p));
                } else if t.pk[ii] != 0 {
                    n_new += 2 + n_old;
                } else {
                    n_new += 2;
                }
            }
        }
        let c = &mut self.tables[ti].buckets[bucket][ci];
        if n_new > c.n_max_size {
            let incr = n_new - c.n_max_size;
            c.n_max_size = n_new;
            self.n_max_changeset_size += incr;
        }
        SQLITE_OK
    }

    /// sessionPreupdateOneChange.
    unsafe fn preupdate_one_change(&mut self, op: u8, rowid: i64, ti: usize) {
        if self.rc != SQLITE_OK {
            return;
        }
        if self.init_table(ti) {
            return;
        }
        let n_expect = self.hook_count();
        if self.tables[ti].n_total_col < n_expect {
            if self.reinit_table(ti) != SQLITE_OK {
                return;
            }
            if self.update_changes(ti) != SQLITE_OK {
                return;
            }
        }
        if self.tables[ti].n_total_col != n_expect {
            self.rc = SQLITE_SCHEMA;
            return;
        }
        let before = self.tables[ti].n_change();
        self.tables[ti].grow_hash(false);
        let after = self.tables[ti].n_change();
        if after != before {
            self.account(((after - before) * 8) as i64);
        }

        let (hash, null_pk) = match self.preupdate_hash(ti, rowid, op == SQLITE_INSERT) {
            Ok(x) => x,
            Err(rc) => {
                self.rc = rc;
                return;
            }
        };
        if null_pk {
            return;
        }
        let found = {
            let chain = &self.tables[ti].buckets[hash];
            let mut found = None;
            for (pos, c) in chain.iter().enumerate().rev() {
                if self.preupdate_equal(ti, rowid, c, op) {
                    found = Some(pos);
                    break;
                }
            }
            found
        };
        let ci = match found {
            None => {
                let t = &self.tables[ti];
                let mut rec = Vec::new();
                if t.rowid {
                    rec.push(SQLITE_INTEGER);
                    put_i64(&mut rec, rowid);
                }
                for i in (t.rowid as usize)..t.n_col {
                    let p = if op != SQLITE_INSERT {
                        match self.hook_val(false, t.idx[i], t.stat1) {
                            Ok(p) => p,
                            Err(rc) => {
                                self.rc = rc;
                                return;
                            }
                        }
                    } else if t.pk[i] != 0 {
                        self.hook_val(true, t.idx[i], t.stat1).ok().flatten()
                    } else {
                        None
                    };
                    Val::serialize(p.as_ref(), &mut rec);
                }
                let indirect = self.indirect || self.hook_depth() != 0;
                let n_col = t.n_col;
                let mem = rec.len() as i64 + 32;
                let t = &mut self.tables[ti];
                t.n_entry += 1;
                t.buckets[hash].push(SessionChange {
                    op,
                    indirect,
                    n_record_field: n_col,
                    n_max_size: 0,
                    record: rec,
                });
                self.account(mem);
                self.tables[ti].buckets[hash].len() - 1
            }
            Some(pos) => {
                if self.tables[ti].buckets[hash][pos].indirect
                    && self.hook_depth() == 0
                    && !self.indirect
                {
                    self.tables[ti].buckets[hash][pos].indirect = false;
                }
                pos
            }
        };
        if self.enable_size {
            let rc = self.update_max_size(op, ti, hash, ci);
            if rc != SQLITE_OK {
                self.rc = rc;
            }
        }
    }

    /// sessionFindTable: the table's index, attaching it when auto-attach
    /// is on and the filter accepts it.
    unsafe fn find_table(&mut self, name: &str) -> Result<Option<usize>, i32> {
        if let Some(i) = self
            .tables
            .iter()
            .position(|t| t.name.eq_ignore_ascii_case(name))
        {
            return Ok(Some(i));
        }
        if self.auto_attach != 0 {
            let accept = match self.filter {
                None => true,
                Some(f) => {
                    let c = CString::new(name).unwrap_or_default();
                    f(self.filter_ctx, c.as_ptr()) != 0
                }
            };
            if accept {
                self.attach(name);
                return Ok(Some(self.tables.len() - 1));
            }
        }
        Ok(None)
    }

    fn attach(&mut self, name: &str) {
        if !self
            .tables
            .iter()
            .any(|t| t.name.eq_ignore_ascii_case(name))
        {
            self.account(name.len() as i64 + 128);
            self.tables.push(SessionTable::new(name));
        }
    }

    /// sessionGenerateChangeset.
    unsafe fn generate(
        &mut self,
        patchset: bool,
        mut out: Option<(XOutput, *mut ffi::c_void)>,
    ) -> Result<Vec<u8>, i32> {
        if self.rc != SQLITE_OK {
            return Err(self.rc);
        }
        let db = self.db;
        let rc = exec(db, "SAVEPOINT changeset");
        if rc != SQLITE_OK {
            return Err(rc);
        }
        let mut buf: Vec<u8> = Vec::new();
        let mut rc = SQLITE_OK;
        let chunk = strm_chunk_size() as usize;
        for ti in 0..self.tables.len() {
            if rc != SQLITE_OK {
                break;
            }
            if self.tables[ti].n_entry == 0 {
                continue;
            }
            let n_rewind = buf.len();
            let n_old_col = self.tables[ti].n_col;
            rc = self.reinit_table(ti);
            if rc == SQLITE_OK && self.tables[ti].n_col != n_old_col {
                rc = self.update_changes(ti);
            }
            let t = &self.tables[ti];
            if rc == SQLITE_OK {
                append_table_hdr(&mut buf, patchset, t);
            }
            let mut sel = None;
            if rc == SQLITE_OK {
                match select_stmt(db, false, &self.zdb, &t.name, t.n_col, &t.cols, &t.pk) {
                    Ok(s) => sel = Some(s),
                    Err(e) => rc = e,
                }
            }
            let mut n_noop: isize = buf.len() as isize;
            if let Some(sel) = &sel {
                'outer: for chain in t.buckets.iter() {
                    for p in chain.iter().rev() {
                        if rc != SQLITE_OK {
                            break 'outer;
                        }
                        rc = select_bind(sel.0, t.n_col, &t.pk, &p.record);
                        if rc != SQLITE_OK {
                            continue;
                        }
                        if sel.step() == SQLITE_ROW {
                            if p.op == SQLITE_INSERT {
                                buf.push(SQLITE_INSERT);
                                buf.push(p.indirect as u8);
                                for i in 0..t.n_col {
                                    append_col(&mut buf, sel, i as i32);
                                }
                            } else {
                                append_update(&mut buf, patchset, sel, p, &t.pk);
                            }
                        } else if p.op != SQLITE_INSERT {
                            append_delete(&mut buf, patchset, p, t.n_col, &t.pk);
                        }
                        rc = sel.reset();
                        if let Some((x, ctx)) = out {
                            if rc == SQLITE_OK && buf.len() as isize > n_noop && buf.len() > chunk {
                                rc = x(ctx, buf.as_ptr() as *const ffi::c_void, buf.len() as i32);
                                n_noop = -1;
                                buf.clear();
                            }
                        }
                    }
                }
            }
            drop(sel);
            if buf.len() as isize == n_noop {
                buf.truncate(n_rewind);
            }
        }
        if rc == SQLITE_OK {
            if let Some((x, ctx)) = out.take() {
                if !buf.is_empty() {
                    rc = x(ctx, buf.as_ptr() as *const ffi::c_void, buf.len() as i32);
                }
                buf.clear();
            }
        }
        exec(db, "RELEASE changeset");
        if rc != SQLITE_OK {
            return Err(rc);
        }
        Ok(buf)
    }
}

/// sessionUpdateOneChange; returns the added size.
unsafe fn update_one_change(c: &mut SessionChange, n_col: usize, dflt: &Stmt) -> i64 {
    let mut total = 0;
    while c.n_record_field < n_col {
        let v = dflt.col(c.n_record_field as i32);
        let before = c.record.len();
        Val::serialize(Some(&v), &mut c.record);
        let incr = (c.record.len() - before) as i64;
        c.n_record_field += 1;
        c.n_max_size += incr;
        total += incr;
    }
    total
}

/// sessionPrepareDfltStmt.
pub(crate) unsafe fn prepare_dflt_stmt(db: *mut sqlite3, t: &SessionTable) -> Result<Stmt, i32> {
    let mut sql = String::from("SELECT");
    let mut sep = " ";
    for i in 0..t.n_col {
        let d = t
            .dflt
            .get(i)
            .cloned()
            .flatten()
            .unwrap_or_else(|| "NULL".to_string());
        sql.push_str(sep);
        sql.push_str(&d);
        sep = ", ";
    }
    Stmt::prepare(db, &sql)
}

pub(crate) fn append_table_hdr(buf: &mut Vec<u8>, patchset: bool, t: &SessionTable) {
    buf.push(if patchset { b'P' } else { b'T' });
    put_varint(buf, t.n_col as u32);
    buf.extend_from_slice(&t.pk[..t.n_col.min(t.pk.len())]);
    buf.extend_from_slice(t.name.as_bytes());
    buf.push(0);
}

/// sessionAppendCol.
pub(crate) unsafe fn append_col(buf: &mut Vec<u8>, stmt: &Stmt, i: i32) {
    let v = stmt.col(i);
    Val::serialize(Some(&v), buf);
}

/// sessionAppendUpdate.
unsafe fn append_update(
    buf: &mut Vec<u8>,
    patchset: bool,
    stmt: &Stmt,
    p: &SessionChange,
    pk: &[u8],
) {
    let n_rewind = buf.len();
    let mut buf2: Vec<u8> = Vec::new();
    let mut noop = true;
    buf.push(SQLITE_UPDATE);
    buf.push(p.indirect as u8);
    let rec = &p.record;
    let mut csr = 0usize;
    for i in 0..stmt.col_count() as usize {
        let e = rec.get(csr).copied().unwrap_or(0);
        let cur = stmt.col(i as i32);
        let advance;
        let mut changed = false;
        match e {
            SQLITE_NULL => {
                advance = 1;
                if cur.type_code() != SQLITE_NULL {
                    changed = true;
                }
            }
            SQLITE_FLOAT | SQLITE_INTEGER => {
                advance = 9;
                let iv = get_i64(&rec[csr + 1..]);
                let same = match (&cur, e) {
                    (Val::Int(x), SQLITE_INTEGER) => *x == iv,
                    (Val::Float(x), SQLITE_FLOAT) => *x == f64::from_bits(iv as u64),
                    _ => false,
                };
                changed = !same;
            }
            _ => {
                let (n, nv) = get_varint(&rec[csr + 1..]);
                let hdr = 1 + nv;
                advance = hdr + n as usize;
                let bytes = &rec[(csr + hdr).min(rec.len())..(csr + advance).min(rec.len())];
                let same = match &cur {
                    Val::Text(x) if e == SQLITE_TEXT => x.as_slice() == bytes,
                    Val::Blob(x) if e == SQLITE_BLOB => x.as_slice() == bytes,
                    _ => false,
                };
                changed = !same;
            }
        }
        if changed {
            noop = false;
        }
        let is_pk = pk.get(i).copied().unwrap_or(0) != 0;
        if !patchset {
            if changed || is_pk {
                buf.extend_from_slice(&rec[csr..(csr + advance).min(rec.len())]);
            } else {
                buf.push(0);
            }
        }
        if changed || (patchset && is_pk) {
            Val::serialize(Some(&cur), &mut buf2);
        } else {
            buf2.push(0);
        }
        csr += advance;
    }
    if noop {
        buf.truncate(n_rewind);
    } else {
        buf.extend_from_slice(&buf2);
    }
}

/// sessionAppendDelete.
fn append_delete(buf: &mut Vec<u8>, patchset: bool, p: &SessionChange, n_col: usize, pk: &[u8]) {
    buf.push(SQLITE_DELETE);
    buf.push(p.indirect as u8);
    if !patchset {
        buf.extend_from_slice(&p.record);
    } else {
        let a = &p.record;
        let mut off = 0;
        for i in 0..n_col {
            let n = serial_len(&a[off..]);
            if pk.get(i).copied().unwrap_or(0) != 0 {
                buf.extend_from_slice(&a[off..off + n]);
            }
            off += n;
        }
    }
}

/// sessionSelectStmt.
pub(crate) unsafe fn select_stmt(
    db: *mut sqlite3,
    ignore_noop: bool,
    zdb: &str,
    tab: &str,
    n_col: usize,
    cols: &[String],
    pk: &[u8],
) -> Result<Stmt, i32> {
    let mut nooptest = String::from(", 1");
    let mut colsql = String::new();
    let mut pkfield = String::new();
    let mut pkvar = String::new();
    if tab.eq_ignore_ascii_case("sqlite_stat1") {
        nooptest.push_str(" AND (?6 OR ?3 IS stat)");
        pkfield.push_str("tbl, idx");
        pkvar.push_str("?1, (CASE WHEN ?2=X'' THEN NULL ELSE ?2 END)");
        colsql.push_str("tbl, ?2, stat");
    } else {
        let mut sep = "";
        for i in 0..n_col {
            if !colsql.is_empty() {
                colsql.push_str(", ");
            }
            colsql.push_str(&ident(&cols[i]));
            if pk[i] != 0 {
                pkfield.push_str(sep);
                pkvar.push_str(sep);
                sep = ", ";
                pkfield.push_str(&ident(&cols[i]));
                pkvar.push_str(&format!("?{}", i + 1));
            } else {
                nooptest.push_str(&format!(
                    " AND (?{} OR ?{} IS {}.{})",
                    i + 1 + n_col,
                    i + 1,
                    w(tab),
                    w(&cols[i])
                ));
            }
        }
    }
    let sql = format!(
        "SELECT {}{} FROM {}.{} WHERE ({}) IS ({})",
        colsql,
        if ignore_noop { nooptest.as_str() } else { "" },
        qq(zdb),
        qq(tab),
        pkfield,
        pkvar
    );
    Stmt::prepare(db, &sql)
}

/// sessionSelectBind.
pub(crate) unsafe fn select_bind(
    stmt: *mut sqlite3_stmt,
    n_col: usize,
    pk: &[u8],
    rec: &[u8],
) -> i32 {
    let mut off = 0;
    let mut rc = SQLITE_OK;
    for i in 0..n_col {
        if rc != SQLITE_OK {
            break;
        }
        let n = serial_len(&rec[off.min(rec.len())..]);
        if pk[i] != 0 {
            if let Some(v) = Val::deserialize(&rec[off..]) {
                if v != Val::Null {
                    rc = v.bind(stmt, i as i32 + 1);
                }
            }
        }
        off += n;
    }
    rc
}

// ---------------------------------------------------------------------------
// The preupdate hook and the session C API.

/// xPreUpdate: the session list is the hook's context.
unsafe extern "C" fn x_preupdate(
    ctx: *mut ffi::c_void,
    _db: *mut sqlite3,
    op: ffi::c_int,
    zdb: *const ffi::c_char,
    zname: *const ffi::c_char,
    key1: i64,
    key2: i64,
) {
    let zdb = cstr(zdb);
    let zname = cstr(zname);
    let mut p = ctx as *mut Sqlite3Session;
    while !p.is_null() {
        let s = &mut *p;
        p = s.next;
        if !s.enable || s.rc != SQLITE_OK || !zdb.eq_ignore_ascii_case(&s.zdb) {
            continue;
        }
        match s.find_table(&zname) {
            Ok(Some(ti)) => {
                s.preupdate_one_change(op as u8, key1, ti);
                if op as u8 == SQLITE_UPDATE {
                    s.preupdate_one_change(SQLITE_INSERT, key2, ti);
                }
            }
            Ok(None) => {}
            Err(rc) => s.rc = rc,
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_create(
    db: *mut sqlite3,
    zdb: *const ffi::c_char,
    pp_session: *mut *mut Sqlite3Session,
) -> ffi::c_int {
    if pp_session.is_null() {
        return SQLITE_MISUSE;
    }
    *pp_session = ptr::null_mut();
    if db.is_null() || zdb.is_null() {
        return SQLITE_MISUSE;
    }
    let s = Box::into_raw(Box::new(Sqlite3Session {
        db,
        zdb: cstr(zdb),
        enable_size: false,
        enable: true,
        indirect: false,
        auto_attach: 0,
        implicit_pk: false,
        rc: SQLITE_OK,
        filter_ctx: ptr::null_mut(),
        filter: None,
        n_malloc: 0,
        n_max_changeset_size: 0,
        next: ptr::null_mut(),
        tables: Vec::new(),
        hook: Hook::Preupdate,
    }));
    let old = crate::hooks::sqlite3_preupdate_hook(db, Some(x_preupdate), s as *mut ffi::c_void);
    (*s).next = old as *mut Sqlite3Session;
    *pp_session = s;
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_delete(p_session: *mut Sqlite3Session) {
    if p_session.is_null() {
        return;
    }
    let db = (*p_session).db;
    let mut head =
        crate::hooks::sqlite3_preupdate_hook(db, None, ptr::null_mut()) as *mut Sqlite3Session;
    // Unlink it from the list.
    let mut pp: *mut *mut Sqlite3Session = &mut head;
    while !(*pp).is_null() {
        if *pp == p_session {
            *pp = (*p_session).next;
            break;
        }
        pp = &mut (**pp).next;
    }
    if !head.is_null() {
        crate::hooks::sqlite3_preupdate_hook(db, Some(x_preupdate), head as *mut ffi::c_void);
    }
    drop(Box::from_raw(p_session));
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_table_filter(
    p_session: *mut Sqlite3Session,
    x_filter: Option<unsafe extern "C" fn(*mut ffi::c_void, *const ffi::c_char) -> ffi::c_int>,
    p_ctx: *mut ffi::c_void,
) {
    if p_session.is_null() {
        return;
    }
    let s = &mut *p_session;
    s.auto_attach = 1;
    s.filter_ctx = p_ctx;
    s.filter = x_filter;
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_attach(
    p_session: *mut Sqlite3Session,
    z_tab: *const ffi::c_char,
) -> ffi::c_int {
    if p_session.is_null() {
        return SQLITE_MISUSE;
    }
    let s = &mut *p_session;
    if z_tab.is_null() {
        s.auto_attach = 1;
    } else {
        s.attach(&cstr(z_tab));
    }
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_changeset(
    p_session: *mut Sqlite3Session,
    pn_changeset: *mut ffi::c_int,
    pp_changeset: *mut *mut ffi::c_void,
) -> ffi::c_int {
    output_buffer(p_session, false, pn_changeset, pp_changeset)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_patchset(
    p_session: *mut Sqlite3Session,
    pn_patchset: *mut ffi::c_int,
    pp_patchset: *mut *mut ffi::c_void,
) -> ffi::c_int {
    output_buffer(p_session, true, pn_patchset, pp_patchset)
}

unsafe fn output_buffer(
    p_session: *mut Sqlite3Session,
    patchset: bool,
    pn: *mut ffi::c_int,
    pp: *mut *mut ffi::c_void,
) -> ffi::c_int {
    if pn.is_null() || pp.is_null() || p_session.is_null() {
        return SQLITE_MISUSE;
    }
    *pn = 0;
    *pp = ptr::null_mut();
    match (*p_session).generate(patchset, None) {
        Ok(buf) => {
            *pn = buf.len() as i32;
            *pp = to_sqlite_buf(&buf);
            SQLITE_OK
        }
        Err(rc) => rc,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_changeset_strm(
    p_session: *mut Sqlite3Session,
    x_output: Option<XOutput>,
    p_out: *mut ffi::c_void,
) -> ffi::c_int {
    let (Some(x), false) = (x_output, p_session.is_null()) else {
        return SQLITE_MISUSE;
    };
    (*p_session)
        .generate(false, Some((x, p_out)))
        .err()
        .unwrap_or(SQLITE_OK)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_patchset_strm(
    p_session: *mut Sqlite3Session,
    x_output: Option<XOutput>,
    p_out: *mut ffi::c_void,
) -> ffi::c_int {
    let (Some(x), false) = (x_output, p_session.is_null()) else {
        return SQLITE_MISUSE;
    };
    (*p_session)
        .generate(true, Some((x, p_out)))
        .err()
        .unwrap_or(SQLITE_OK)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_enable(
    p_session: *mut Sqlite3Session,
    b_enable: ffi::c_int,
) -> ffi::c_int {
    if p_session.is_null() {
        return 0;
    }
    let s = &mut *p_session;
    if b_enable >= 0 {
        s.enable = b_enable != 0;
    }
    s.enable as ffi::c_int
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_indirect(
    p_session: *mut Sqlite3Session,
    b_indirect: ffi::c_int,
) -> ffi::c_int {
    if p_session.is_null() {
        return 0;
    }
    let s = &mut *p_session;
    if b_indirect >= 0 {
        s.indirect = b_indirect != 0;
    }
    s.indirect as ffi::c_int
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_isempty(p_session: *mut Sqlite3Session) -> ffi::c_int {
    if p_session.is_null() {
        return 1;
    }
    (!(*p_session).tables.iter().any(|t| t.n_entry > 0)) as ffi::c_int
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_memory_used(p_session: *mut Sqlite3Session) -> i64 {
    if p_session.is_null() {
        return 0;
    }
    (*p_session).n_malloc
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_object_config(
    p_session: *mut Sqlite3Session,
    op: ffi::c_int,
    p_arg: *mut ffi::c_void,
) -> ffi::c_int {
    if p_session.is_null() {
        return SQLITE_MISUSE;
    }
    let s = &mut *p_session;
    match op {
        SQLITE_SESSION_OBJCONFIG_SIZE | SQLITE_SESSION_OBJCONFIG_ROWID => {
            if p_arg.is_null() {
                return SQLITE_MISUSE;
            }
            let arg = *(p_arg as *mut ffi::c_int);
            let mut rc = SQLITE_OK;
            let flag = if op == SQLITE_SESSION_OBJCONFIG_SIZE {
                &mut s.enable_size
            } else {
                &mut s.implicit_pk
            };
            if arg >= 0 {
                if !s.tables.is_empty() {
                    rc = SQLITE_MISUSE;
                } else {
                    *flag = arg != 0;
                }
            }
            *(p_arg as *mut ffi::c_int) = *flag as ffi::c_int;
            rc
        }
        _ => SQLITE_MISUSE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_changeset_size(p_session: *mut Sqlite3Session) -> i64 {
    if p_session.is_null() {
        return 0;
    }
    (*p_session).n_max_changeset_size
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_config(
    op: ffi::c_int,
    p_arg: *mut ffi::c_void,
) -> ffi::c_int {
    match op {
        SQLITE_SESSION_CONFIG_STRMSIZE => {
            if p_arg.is_null() {
                return SQLITE_MISUSE;
            }
            let p = p_arg as *mut ffi::c_int;
            if *p > 0 {
                STRM_CHUNK_SIZE.store(*p, std::sync::atomic::Ordering::Relaxed);
            }
            *p = strm_chunk_size();
            SQLITE_OK
        }
        _ => SQLITE_MISUSE,
    }
}
