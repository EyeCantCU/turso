//! Virtual table C API tests: modules defined here in Rust through the
//! sqlite3_module ABI. They must pass on turso and on SQLite.

use libc::{c_char, c_int, c_void};
use std::ffi::{CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

use super::{
    sqlite3, sqlite3_close, sqlite3_column_count, sqlite3_column_text, sqlite3_column_type,
    sqlite3_errcode, sqlite3_errmsg, sqlite3_exec, sqlite3_finalize, sqlite3_free,
    sqlite3_last_insert_rowid, sqlite3_open, sqlite3_prepare_v2, sqlite3_result_int64,
    sqlite3_step, sqlite3_stmt, sqlite3_value_int64, sqlite3_value_text, sqlite3_value_type,
};

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_CONSTRAINT: c_int = 19;
const SQLITE_MISUSE: c_int = 21;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_NULL: c_int = 5;
const SQLITE_INDEX_CONSTRAINT_EQ: u8 = 2;
const SQLITE_ABORT: c_int = 4;
const SQLITE_REPLACE: c_int = 5;
const SQLITE_IGNORE: c_int = 2;

#[cfg_attr(
    all(not(feature = "sqlite3"), not(target_os = "windows")),
    link(name = "turso_sqlite3")
)]
#[cfg_attr(
    all(
        feature = "sqlite3",
        not(all(target_os = "windows", target_env = "msvc"))
    ),
    link(name = "sqlite3")
)]
#[cfg_attr(
    all(feature = "sqlite3", target_os = "windows", target_env = "msvc"),
    link(name = "sqlite3", kind = "static")
)]
#[cfg_attr(
    all(not(feature = "sqlite3"), target_os = "windows"),
    link(name = "turso_sqlite3", kind = "raw-dylib")
)]
extern "C" {
    fn sqlite3_malloc(n: c_int) -> *mut c_void;
    fn sqlite3_mprintf(fmt: *const c_char, ...) -> *mut c_char;
    fn sqlite3_result_null(ctx: *mut c_void);
    fn sqlite3_result_text(
        ctx: *mut c_void,
        s: *const c_char,
        n: c_int,
        d: Option<unsafe extern "C" fn(*mut c_void)>,
    );
    fn sqlite3_create_module(
        db: *mut sqlite3,
        name: *const c_char,
        module: *const sqlite3_module,
        aux: *mut c_void,
    ) -> c_int;
    fn sqlite3_create_module_v2(
        db: *mut sqlite3,
        name: *const c_char,
        module: *const sqlite3_module,
        aux: *mut c_void,
        destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    fn sqlite3_declare_vtab(db: *mut sqlite3, sql: *const c_char) -> c_int;
    fn sqlite3_vtab_config(db: *mut sqlite3, op: c_int, ...) -> c_int;
    fn sqlite3_vtab_on_conflict(db: *mut sqlite3) -> c_int;
    fn sqlite3_vtab_nochange(ctx: *mut c_void) -> c_int;
    fn sqlite3_vtab_collation(info: *mut sqlite3_index_info, i: c_int) -> *const c_char;
    fn sqlite3_vtab_distinct(info: *mut sqlite3_index_info) -> c_int;
}

type XCreate = unsafe extern "C" fn(
    *mut sqlite3,
    *mut c_void,
    c_int,
    *const *const c_char,
    *mut *mut sqlite3_vtab,
    *mut *mut c_char,
) -> c_int;
type XVtab = unsafe extern "C" fn(*mut sqlite3_vtab) -> c_int;
type XCur = unsafe extern "C" fn(*mut sqlite3_vtab_cursor) -> c_int;

#[repr(C)]
#[allow(non_snake_case)]
pub struct sqlite3_module {
    iVersion: c_int,
    xCreate: Option<XCreate>,
    xConnect: Option<XCreate>,
    xBestIndex: Option<unsafe extern "C" fn(*mut sqlite3_vtab, *mut sqlite3_index_info) -> c_int>,
    xDisconnect: Option<XVtab>,
    xDestroy: Option<XVtab>,
    xOpen: Option<unsafe extern "C" fn(*mut sqlite3_vtab, *mut *mut sqlite3_vtab_cursor) -> c_int>,
    xClose: Option<XCur>,
    xFilter: Option<
        unsafe extern "C" fn(
            *mut sqlite3_vtab_cursor,
            c_int,
            *const c_char,
            c_int,
            *mut *mut c_void,
        ) -> c_int,
    >,
    xNext: Option<XCur>,
    xEof: Option<XCur>,
    xColumn: Option<unsafe extern "C" fn(*mut sqlite3_vtab_cursor, *mut c_void, c_int) -> c_int>,
    xRowid: Option<unsafe extern "C" fn(*mut sqlite3_vtab_cursor, *mut i64) -> c_int>,
    xUpdate:
        Option<unsafe extern "C" fn(*mut sqlite3_vtab, c_int, *mut *mut c_void, *mut i64) -> c_int>,
    xBegin: Option<XVtab>,
    xSync: Option<XVtab>,
    xCommit: Option<XVtab>,
    xRollback: Option<XVtab>,
    xFindFunction: Option<unsafe extern "C" fn()>,
    xRename: Option<unsafe extern "C" fn(*mut sqlite3_vtab, *const c_char) -> c_int>,
    xSavepoint: Option<unsafe extern "C" fn(*mut sqlite3_vtab, c_int) -> c_int>,
    xRelease: Option<unsafe extern "C" fn(*mut sqlite3_vtab, c_int) -> c_int>,
    xRollbackTo: Option<unsafe extern "C" fn(*mut sqlite3_vtab, c_int) -> c_int>,
    xShadowName: Option<unsafe extern "C" fn(*const c_char) -> c_int>,
    xIntegrity: Option<unsafe extern "C" fn()>,
}

#[repr(C)]
#[allow(non_snake_case)]
pub struct sqlite3_vtab {
    pModule: *const sqlite3_module,
    nRef: c_int,
    zErrMsg: *mut c_char,
}

#[repr(C)]
#[allow(non_snake_case)]
pub struct sqlite3_vtab_cursor {
    pVtab: *mut sqlite3_vtab,
}

#[repr(C)]
#[allow(non_snake_case)]
pub struct sqlite3_index_constraint {
    iColumn: c_int,
    op: u8,
    usable: u8,
    iTermOffset: c_int,
}

#[repr(C)]
#[allow(non_snake_case)]
pub struct sqlite3_index_orderby {
    iColumn: c_int,
    desc: u8,
}

#[repr(C)]
#[allow(non_snake_case)]
pub struct sqlite3_index_constraint_usage {
    argvIndex: c_int,
    omit: u8,
}

#[repr(C)]
#[allow(non_snake_case)]
pub struct sqlite3_index_info {
    nConstraint: c_int,
    aConstraint: *mut sqlite3_index_constraint,
    nOrderBy: c_int,
    aOrderBy: *mut sqlite3_index_orderby,
    aConstraintUsage: *mut sqlite3_index_constraint_usage,
    idxNum: c_int,
    idxStr: *mut c_char,
    needToFreeIdxStr: c_int,
    orderByConsumed: c_int,
    estimatedCost: f64,
    estimatedRows: i64,
    idxFlags: c_int,
    colUsed: u64,
}

const EMPTY_MODULE: sqlite3_module = sqlite3_module {
    iVersion: 2,
    xCreate: None,
    xConnect: None,
    xBestIndex: None,
    xDisconnect: None,
    xDestroy: None,
    xOpen: None,
    xClose: None,
    xFilter: None,
    xNext: None,
    xEof: None,
    xColumn: None,
    xRowid: None,
    xUpdate: None,
    xBegin: None,
    xSync: None,
    xCommit: None,
    xRollback: None,
    xFindFunction: None,
    xRename: None,
    xSavepoint: None,
    xRelease: None,
    xRollbackTo: None,
    xShadowName: None,
    xIntegrity: None,
};

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

unsafe fn open_mem() -> *mut sqlite3 {
    let mut db = ptr::null_mut();
    assert_eq!(sqlite3_open(c(":memory:").as_ptr(), &mut db), SQLITE_OK);
    db
}

unsafe fn exec(db: *mut sqlite3, sql: &str) -> c_int {
    sqlite3_exec(db, c(sql).as_ptr(), None, ptr::null_mut(), ptr::null_mut())
}

unsafe fn exec_ok(db: *mut sqlite3, sql: &str) {
    let rc = exec(db, sql);
    assert_eq!(rc, SQLITE_OK, "{sql}: {}", errmsg(db));
}

unsafe fn errmsg(db: *mut sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db))
        .to_string_lossy()
        .into_owned()
}

/// Runs a query and returns its rows, each column as text ("NULL" for NULL).
unsafe fn query(db: *mut sqlite3, sql: &str) -> Vec<Vec<String>> {
    let mut stmt = ptr::null_mut();
    let rc = sqlite3_prepare_v2(db, c(sql).as_ptr(), -1, &mut stmt, ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "{sql}: {}", errmsg(db));
    let mut rows = Vec::new();
    loop {
        match sqlite3_step(stmt) {
            SQLITE_ROW => {
                let n = sqlite3_column_count(stmt);
                rows.push(
                    (0..n)
                        .map(|i| {
                            if sqlite3_column_type(stmt, i) == SQLITE_NULL {
                                "NULL".to_string()
                            } else {
                                CStr::from_ptr(sqlite3_column_text(stmt, i))
                                    .to_string_lossy()
                                    .into_owned()
                            }
                        })
                        .collect(),
                );
            }
            SQLITE_DONE => break,
            rc => panic!("{sql}: step rc={rc}: {}", errmsg(db)),
        }
    }
    sqlite3_finalize(stmt);
    rows
}

unsafe fn ints(db: *mut sqlite3, sql: &str) -> Vec<i64> {
    query(db, sql)
        .into_iter()
        .map(|r| r[0].parse().unwrap())
        .collect()
}

// ---------------------------------------------------------------------------
// series: an eponymous-only table-valued function, generate_series style.
// Its xBestIndex hands xFilter a binary, sqlite3_malloc'd idxStr.
// ---------------------------------------------------------------------------

#[repr(C)]
struct SeriesCursor {
    base: sqlite3_vtab_cursor,
    value: i64,
    stop: i64,
}

static SERIES_CONNECTS: AtomicI32 = AtomicI32::new(0);
static SERIES_COLLATION_OK: AtomicI32 = AtomicI32::new(0);

unsafe extern "C" fn series_connect(
    db: *mut sqlite3,
    _aux: *mut c_void,
    argc: c_int,
    argv: *const *const c_char,
    pp: *mut *mut sqlite3_vtab,
    _err: *mut *mut c_char,
) -> c_int {
    SERIES_CONNECTS.fetch_add(1, Ordering::SeqCst);
    assert_eq!(argc, 3);
    assert_eq!(CStr::from_ptr(*argv.add(1)).to_str().unwrap(), "main");
    let rc = sqlite3_declare_vtab(
        db,
        c("CREATE TABLE x(value, start HIDDEN, stop HIDDEN)").as_ptr(),
    );
    if rc != SQLITE_OK {
        return rc;
    }
    *pp = Box::into_raw(Box::new(sqlite3_vtab {
        pModule: ptr::null(),
        nRef: 0,
        zErrMsg: ptr::null_mut(),
    }));
    SQLITE_OK
}

unsafe extern "C" fn series_disconnect(v: *mut sqlite3_vtab) -> c_int {
    drop(Box::from_raw(v));
    SQLITE_OK
}

/// idxStr layout: [0xff, 0x00, mask] — the NUL checks it is not read as text.
unsafe extern "C" fn series_best_index(
    _v: *mut sqlite3_vtab,
    info: *mut sqlite3_index_info,
) -> c_int {
    let info = &mut *info;
    let mut mask = 0u8;
    let mut next_arg = 1;
    for i in 0..info.nConstraint as usize {
        let cons = &*info.aConstraint.add(i);
        if cons.usable == 0 || cons.op != SQLITE_INDEX_CONSTRAINT_EQ {
            continue;
        }
        if cons.iColumn == 1 || cons.iColumn == 2 {
            if !sqlite3_vtab_collation(info, i as c_int).is_null()
                && CStr::from_ptr(sqlite3_vtab_collation(info, i as c_int)).to_bytes() == b"BINARY"
            {
                SERIES_COLLATION_OK.store(1, Ordering::SeqCst);
            }
            let usage = &mut *info.aConstraintUsage.add(i);
            usage.argvIndex = next_arg;
            usage.omit = 1;
            next_arg += 1;
            mask |= cons.iColumn as u8;
        }
    }
    assert_eq!(sqlite3_vtab_distinct(info), 0);
    let s = sqlite3_malloc(3) as *mut u8;
    *s = 0xff;
    *s.add(1) = 0;
    *s.add(2) = mask;
    info.idxStr = s as *mut c_char;
    info.needToFreeIdxStr = 1;
    info.idxNum = 7;
    info.estimatedCost = 10.0;
    info.estimatedRows = 10;
    SQLITE_OK
}

unsafe extern "C" fn series_open(
    _v: *mut sqlite3_vtab,
    pp: *mut *mut sqlite3_vtab_cursor,
) -> c_int {
    *pp = Box::into_raw(Box::new(SeriesCursor {
        base: sqlite3_vtab_cursor {
            pVtab: ptr::null_mut(),
        },
        value: 0,
        stop: 0,
    })) as *mut sqlite3_vtab_cursor;
    SQLITE_OK
}

unsafe extern "C" fn series_close(cur: *mut sqlite3_vtab_cursor) -> c_int {
    drop(Box::from_raw(cur as *mut SeriesCursor));
    SQLITE_OK
}

unsafe extern "C" fn series_filter(
    cur: *mut sqlite3_vtab_cursor,
    idx_num: c_int,
    idx_str: *const c_char,
    argc: c_int,
    argv: *mut *mut c_void,
) -> c_int {
    let cur = &mut *(cur as *mut SeriesCursor);
    assert_eq!(idx_num, 7);
    let s = idx_str as *const u8;
    assert_eq!((*s, *s.add(1)), (0xff, 0));
    let mask = *s.add(2);
    let mut arg = 0;
    cur.value = 1;
    cur.stop = 3;
    if mask & 1 != 0 {
        cur.value = sqlite3_value_int64(*argv.add(arg));
        arg += 1;
    }
    if mask & 2 != 0 {
        cur.stop = sqlite3_value_int64(*argv.add(arg));
        arg += 1;
    }
    assert_eq!(arg as c_int, argc);
    SQLITE_OK
}

unsafe extern "C" fn series_next(cur: *mut sqlite3_vtab_cursor) -> c_int {
    (*(cur as *mut SeriesCursor)).value += 1;
    SQLITE_OK
}

unsafe extern "C" fn series_eof(cur: *mut sqlite3_vtab_cursor) -> c_int {
    let cur = &*(cur as *mut SeriesCursor);
    (cur.value > cur.stop) as c_int
}

unsafe extern "C" fn series_column(
    cur: *mut sqlite3_vtab_cursor,
    ctx: *mut c_void,
    i: c_int,
) -> c_int {
    let cur = &*(cur as *mut SeriesCursor);
    assert_eq!(sqlite3_vtab_nochange(ctx), 0);
    match i {
        0 => sqlite3_result_int64(ctx, cur.value),
        _ => sqlite3_result_null(ctx),
    }
    SQLITE_OK
}

unsafe extern "C" fn series_rowid(cur: *mut sqlite3_vtab_cursor, rowid: *mut i64) -> c_int {
    *rowid = (*(cur as *mut SeriesCursor)).value;
    SQLITE_OK
}

static SERIES_MODULE: sqlite3_module = sqlite3_module {
    xConnect: Some(series_connect),
    xBestIndex: Some(series_best_index),
    xDisconnect: Some(series_disconnect),
    xDestroy: Some(series_disconnect),
    xOpen: Some(series_open),
    xClose: Some(series_close),
    xFilter: Some(series_filter),
    xNext: Some(series_next),
    xEof: Some(series_eof),
    xColumn: Some(series_column),
    xRowid: Some(series_rowid),
    ..EMPTY_MODULE
};

#[test]
fn test_vtab_eponymous_series() {
    unsafe {
        let db = open_mem();
        assert_eq!(
            sqlite3_create_module(db, c("series").as_ptr(), &SERIES_MODULE, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(ints(db, "SELECT value FROM series(2, 5)"), vec![2, 3, 4, 5]);
        assert_eq!(
            ints(db, "SELECT value FROM series WHERE start = 4 AND stop = 6"),
            vec![4, 5, 6]
        );
        assert_eq!(ints(db, "SELECT value FROM series"), vec![1, 2, 3]);
        assert_eq!(
            ints(
                db,
                "SELECT sum(value) FROM series(1, 10) WHERE value % 2 = 0"
            ),
            vec![30]
        );
        assert_eq!(
            ints(
                db,
                "SELECT a.value * b.value FROM series(1, 2) a, series(10, 10) b ORDER BY 1"
            ),
            vec![10, 20]
        );
        assert_eq!(SERIES_COLLATION_OK.load(Ordering::SeqCst), 1);
        assert!(SERIES_CONNECTS.load(Ordering::SeqCst) >= 1);
        // Eponymous-only modules cannot back CREATE VIRTUAL TABLE.
        assert_eq!(
            exec(db, "CREATE VIRTUAL TABLE s USING series"),
            SQLITE_ERROR
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_vtab_declare_and_config_outside_constructor() {
    unsafe {
        let db = open_mem();
        assert_eq!(
            sqlite3_declare_vtab(db, c("CREATE TABLE x(a)").as_ptr()),
            SQLITE_MISUSE
        );
        assert_eq!(sqlite3_vtab_config(db, 1, 1 as c_int), SQLITE_MISUSE);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_vtab_unknown_module() {
    unsafe {
        let db = open_mem();
        assert_eq!(
            exec(db, "CREATE VIRTUAL TABLE t USING nosuchmod"),
            SQLITE_ERROR
        );
        assert!(
            errmsg(db).contains("no such module: nosuchmod"),
            "{}",
            errmsg(db)
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

// ---------------------------------------------------------------------------
// kv: a writable table (xCreate != xConnect) with xUpdate and transaction
// hooks. Rows live in a process-wide store keyed by table name, so a table
// survives reconnecting.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct KvState {
    rows: Vec<(i64, String)>,
    next_rowid: i64,
    log: Vec<String>,
    args: Vec<String>,
    conflict: Vec<c_int>,
}

static KV: Mutex<Option<KvState>> = Mutex::new(None);
static KV_AUX_FREED: AtomicI32 = AtomicI32::new(0);

fn kv<R>(f: impl FnOnce(&mut KvState) -> R) -> R {
    let mut guard = KV.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(KvState::default))
}

fn kv_log(s: &str) {
    kv(|k| k.log.push(s.to_string()));
}

#[repr(C)]
struct KvCursor {
    base: sqlite3_vtab_cursor,
    rows: Vec<(i64, String)>,
    pos: usize,
}

unsafe fn kv_ctor(
    db: *mut sqlite3,
    aux: *mut c_void,
    argc: c_int,
    argv: *const *const c_char,
    pp: *mut *mut sqlite3_vtab,
    what: &str,
) -> c_int {
    assert_eq!(aux as usize, 0x1234);
    let args: Vec<String> = (0..argc as usize)
        .map(|i| CStr::from_ptr(*argv.add(i)).to_string_lossy().into_owned())
        .collect();
    kv(|k| {
        k.log.push(what.to_string());
        k.args = args;
    });
    assert_eq!(sqlite3_vtab_config(db, 1, 1 as c_int), SQLITE_OK);
    if what == "connect" {
        // A constructor may query its own connection.
        assert_eq!(exec(db, "SELECT count(*) FROM sqlite_schema"), SQLITE_OK);
    }
    let rc = sqlite3_declare_vtab(db, c("CREATE TABLE ignored(v TEXT)").as_ptr());
    if rc != SQLITE_OK {
        return rc;
    }
    *pp = Box::into_raw(Box::new(sqlite3_vtab {
        pModule: ptr::null(),
        nRef: 0,
        zErrMsg: ptr::null_mut(),
    }));
    SQLITE_OK
}

unsafe extern "C" fn kv_create(
    db: *mut sqlite3,
    aux: *mut c_void,
    argc: c_int,
    argv: *const *const c_char,
    pp: *mut *mut sqlite3_vtab,
    _err: *mut *mut c_char,
) -> c_int {
    kv_ctor(db, aux, argc, argv, pp, "create")
}

unsafe extern "C" fn kv_connect(
    db: *mut sqlite3,
    aux: *mut c_void,
    argc: c_int,
    argv: *const *const c_char,
    pp: *mut *mut sqlite3_vtab,
    _err: *mut *mut c_char,
) -> c_int {
    kv_ctor(db, aux, argc, argv, pp, "connect")
}

unsafe extern "C" fn kv_disconnect(v: *mut sqlite3_vtab) -> c_int {
    kv_log("disconnect");
    drop(Box::from_raw(v));
    SQLITE_OK
}

unsafe extern "C" fn kv_destroy(v: *mut sqlite3_vtab) -> c_int {
    kv(|k| {
        k.log.push("destroy".to_string());
        k.rows.clear();
    });
    drop(Box::from_raw(v));
    SQLITE_OK
}

unsafe extern "C" fn kv_best_index(_v: *mut sqlite3_vtab, info: *mut sqlite3_index_info) -> c_int {
    let info = &mut *info;
    info.idxStr = c"full scan".as_ptr() as *mut c_char;
    info.needToFreeIdxStr = 0;
    info.estimatedCost = 100.0;
    SQLITE_OK
}

unsafe extern "C" fn kv_open(_v: *mut sqlite3_vtab, pp: *mut *mut sqlite3_vtab_cursor) -> c_int {
    *pp = Box::into_raw(Box::new(KvCursor {
        base: sqlite3_vtab_cursor {
            pVtab: ptr::null_mut(),
        },
        rows: Vec::new(),
        pos: 0,
    })) as *mut sqlite3_vtab_cursor;
    SQLITE_OK
}

unsafe extern "C" fn kv_close(cur: *mut sqlite3_vtab_cursor) -> c_int {
    drop(Box::from_raw(cur as *mut KvCursor));
    SQLITE_OK
}

unsafe extern "C" fn kv_filter(
    cur: *mut sqlite3_vtab_cursor,
    _idx_num: c_int,
    idx_str: *const c_char,
    _argc: c_int,
    _argv: *mut *mut c_void,
) -> c_int {
    // A module-owned idxStr comes back as the same pointer.
    assert_eq!(idx_str, c"full scan".as_ptr());
    let cur = &mut *(cur as *mut KvCursor);
    cur.rows = kv(|k| k.rows.clone());
    cur.pos = 0;
    SQLITE_OK
}

unsafe extern "C" fn kv_next(cur: *mut sqlite3_vtab_cursor) -> c_int {
    (*(cur as *mut KvCursor)).pos += 1;
    SQLITE_OK
}

unsafe extern "C" fn kv_eof(cur: *mut sqlite3_vtab_cursor) -> c_int {
    let cur = &*(cur as *mut KvCursor);
    (cur.pos >= cur.rows.len()) as c_int
}

unsafe extern "C" fn kv_column(
    cur: *mut sqlite3_vtab_cursor,
    ctx: *mut c_void,
    _i: c_int,
) -> c_int {
    let cur = &*(cur as *mut KvCursor);
    let v = &cur.rows[cur.pos].1;
    sqlite3_result_text(ctx, v.as_ptr() as *const c_char, v.len() as c_int, None);
    SQLITE_OK
}

unsafe extern "C" fn kv_rowid(cur: *mut sqlite3_vtab_cursor, rowid: *mut i64) -> c_int {
    let cur = &*(cur as *mut KvCursor);
    *rowid = cur.rows[cur.pos].0;
    SQLITE_OK
}

unsafe fn text(v: *mut c_void) -> String {
    CStr::from_ptr(sqlite3_value_text(v))
        .to_string_lossy()
        .into_owned()
}

unsafe extern "C" fn kv_update(
    vtab: *mut sqlite3_vtab,
    argc: c_int,
    argv: *mut *mut c_void,
    rowid: *mut i64,
) -> c_int {
    let db_conflict = KV_DB.load(Ordering::SeqCst) as *mut sqlite3;
    let conflict = sqlite3_vtab_on_conflict(db_conflict);
    kv(|k| k.conflict.push(conflict));
    let arg = |i: usize| *argv.add(i);
    if argc == 1 {
        let id = sqlite3_value_int64(arg(0));
        kv(|k| k.rows.retain(|r| r.0 != id));
        return SQLITE_OK;
    }
    let value = text(arg(2));
    if value == "bad" {
        (*vtab).zErrMsg = sqlite3_mprintf(c"kv rejects %s".as_ptr(), c"bad".as_ptr());
        return SQLITE_CONSTRAINT;
    }
    if sqlite3_value_type(arg(0)) == SQLITE_NULL {
        let id = kv(|k| {
            k.next_rowid += 1;
            let id = k.next_rowid;
            k.rows.push((id, value));
            id
        });
        *rowid = id;
    } else {
        let old = sqlite3_value_int64(arg(0));
        kv(|k| {
            for r in k.rows.iter_mut().filter(|r| r.0 == old) {
                r.1 = value.clone();
            }
        });
    }
    SQLITE_OK
}

unsafe extern "C" fn kv_begin(_v: *mut sqlite3_vtab) -> c_int {
    kv_log("begin");
    SQLITE_OK
}
unsafe extern "C" fn kv_sync(_v: *mut sqlite3_vtab) -> c_int {
    kv_log("sync");
    SQLITE_OK
}
unsafe extern "C" fn kv_commit(_v: *mut sqlite3_vtab) -> c_int {
    kv_log("commit");
    SQLITE_OK
}
unsafe extern "C" fn kv_rollback(_v: *mut sqlite3_vtab) -> c_int {
    kv_log("rollback");
    SQLITE_OK
}

unsafe extern "C" fn kv_free_aux(aux: *mut c_void) {
    assert_eq!(aux as usize, 0x1234);
    KV_AUX_FREED.fetch_add(1, Ordering::SeqCst);
}

static KV_DB: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

static KV_MODULE: sqlite3_module = sqlite3_module {
    iVersion: 1,
    xCreate: Some(kv_create),
    xConnect: Some(kv_connect),
    xBestIndex: Some(kv_best_index),
    xDisconnect: Some(kv_disconnect),
    xDestroy: Some(kv_destroy),
    xOpen: Some(kv_open),
    xClose: Some(kv_close),
    xFilter: Some(kv_filter),
    xNext: Some(kv_next),
    xEof: Some(kv_eof),
    xColumn: Some(kv_column),
    xRowid: Some(kv_rowid),
    xUpdate: Some(kv_update),
    xBegin: Some(kv_begin),
    xSync: Some(kv_sync),
    xCommit: Some(kv_commit),
    xRollback: Some(kv_rollback),
    ..EMPTY_MODULE
};

unsafe fn kv_register(db: *mut sqlite3) {
    KV_DB.store(db as usize, Ordering::SeqCst);
    assert_eq!(
        sqlite3_create_module_v2(
            db,
            c("kvmod").as_ptr(),
            &KV_MODULE,
            0x1234 as *mut c_void,
            Some(kv_free_aux),
        ),
        SQLITE_OK
    );
}

fn take_log() -> Vec<String> {
    kv(|k| std::mem::take(&mut k.log))
}

#[test]
fn test_vtab_writable_module() {
    unsafe {
        *KV.lock().unwrap_or_else(|e| e.into_inner()) = None;
        KV_AUX_FREED.store(0, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.db");
        let path = c(path.to_str().unwrap());

        let mut db = ptr::null_mut();
        assert_eq!(sqlite3_open(path.as_ptr(), &mut db), SQLITE_OK);
        kv_register(db);
        exec_ok(db, "CREATE VIRTUAL TABLE kv USING KVMOD(alpha, 'b c')");
        assert_eq!(take_log(), vec!["create", "sync", "commit"]);
        assert_eq!(
            kv(|k| k.args.clone()),
            vec!["KVMOD", "main", "kv", "alpha", "'b c'"]
        );

        exec_ok(db, "INSERT INTO kv VALUES ('one')");
        assert_eq!(sqlite3_last_insert_rowid(db), 1);
        assert_eq!(take_log(), vec!["begin", "sync", "commit"]);
        exec_ok(db, "INSERT INTO kv(v) VALUES ('two'), ('three')");
        assert_eq!(sqlite3_last_insert_rowid(db), 3);
        take_log();
        assert_eq!(
            query(db, "SELECT rowid, v FROM kv ORDER BY rowid"),
            vec![
                vec!["1".to_string(), "one".to_string()],
                vec!["2".to_string(), "two".to_string()],
                vec!["3".to_string(), "three".to_string()],
            ]
        );
        exec_ok(db, "UPDATE kv SET v = 'TWO' WHERE rowid = 2");
        exec_ok(db, "DELETE FROM kv WHERE v = 'one'");
        assert_eq!(
            query(db, "SELECT v FROM kv ORDER BY rowid"),
            vec![vec!["TWO".to_string()], vec!["three".to_string()]]
        );

        // ON CONFLICT reaches xUpdate through sqlite3_vtab_on_conflict.
        kv(|k| k.conflict.clear());
        exec_ok(db, "INSERT INTO kv VALUES ('x')");
        exec_ok(db, "INSERT OR REPLACE INTO kv VALUES ('y')");
        exec_ok(db, "INSERT OR IGNORE INTO kv VALUES ('z')");
        assert_eq!(
            kv(|k| k.conflict.clone()),
            vec![SQLITE_ABORT, SQLITE_REPLACE, SQLITE_IGNORE]
        );

        // A constraint failure keeps its code and the module's message.
        assert_eq!(exec(db, "INSERT INTO kv VALUES ('bad')"), SQLITE_CONSTRAINT);
        assert_eq!(sqlite3_errcode(db) & 0xff, SQLITE_CONSTRAINT);
        assert!(errmsg(db).contains("kv rejects bad"), "{}", errmsg(db));

        // Explicit transaction: one xBegin, xSync+xCommit at COMMIT.
        take_log();
        exec_ok(db, "BEGIN");
        exec_ok(db, "INSERT INTO kv VALUES ('t1')");
        exec_ok(db, "INSERT INTO kv VALUES ('t2')");
        exec_ok(db, "COMMIT");
        assert_eq!(take_log(), vec!["begin", "sync", "commit"]);
        exec_ok(db, "BEGIN");
        exec_ok(db, "INSERT INTO kv VALUES ('t3')");
        exec_ok(db, "ROLLBACK");
        assert_eq!(take_log(), vec!["begin", "rollback"]);

        let count = ints(db, "SELECT count(*) FROM kv");

        // Closing disconnects the table, then frees the client data.
        assert_eq!(sqlite3_close(db), SQLITE_OK);
        assert!(take_log().contains(&"disconnect".to_string()));
        assert_eq!(KV_AUX_FREED.load(Ordering::SeqCst), 1);

        // Reopening connects the stored table with the same arguments.
        let mut db = ptr::null_mut();
        assert_eq!(sqlite3_open(path.as_ptr(), &mut db), SQLITE_OK);
        kv_register(db);
        assert_eq!(ints(db, "SELECT count(*) FROM kv"), count);
        assert!(take_log().contains(&"connect".to_string()));
        assert_eq!(
            kv(|k| k.args.clone()),
            vec!["KVMOD", "main", "kv", "alpha", "'b c'"]
        );

        exec_ok(db, "DROP TABLE kv");
        assert!(take_log().contains(&"destroy".to_string()));
        assert_eq!(exec(db, "SELECT * FROM kv"), SQLITE_ERROR);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
        assert_eq!(KV_AUX_FREED.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn test_vtab_constructor_error() {
    unsafe extern "C" fn bad_create(
        _db: *mut sqlite3,
        _aux: *mut c_void,
        _argc: c_int,
        _argv: *const *const c_char,
        _pp: *mut *mut sqlite3_vtab,
        err: *mut *mut c_char,
    ) -> c_int {
        *err = sqlite3_mprintf(c"cannot build %s".as_ptr(), c"it".as_ptr());
        SQLITE_ERROR
    }
    static BAD: sqlite3_module = sqlite3_module {
        xCreate: Some(bad_create),
        xConnect: Some(kv_connect),
        xBestIndex: Some(kv_best_index),
        xDisconnect: Some(kv_disconnect),
        xDestroy: Some(kv_destroy),
        ..EMPTY_MODULE
    };
    unsafe {
        let db = open_mem();
        assert_eq!(
            sqlite3_create_module(db, c("bad").as_ptr(), &BAD, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(exec(db, "CREATE VIRTUAL TABLE t USING bad"), SQLITE_ERROR);
        assert!(errmsg(db).contains("cannot build it"), "{}", errmsg(db));
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

unsafe extern "C" fn kv_savepoint(_v: *mut sqlite3_vtab, i: c_int) -> c_int {
    kv_log(&format!("savepoint:{i}"));
    SQLITE_OK
}
unsafe extern "C" fn kv_release(_v: *mut sqlite3_vtab, i: c_int) -> c_int {
    kv_log(&format!("release:{i}"));
    SQLITE_OK
}
unsafe extern "C" fn kv_rollback_to(_v: *mut sqlite3_vtab, i: c_int) -> c_int {
    kv_log(&format!("rollback_to:{i}"));
    SQLITE_OK
}

#[test]
fn test_vtab_savepoint_methods() {
    static SP_MODULE: sqlite3_module = sqlite3_module {
        iVersion: 2,
        xSavepoint: Some(kv_savepoint),
        xRelease: Some(kv_release),
        xRollbackTo: Some(kv_rollback_to),
        ..KV_MODULE
    };
    unsafe {
        *KV.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let db = open_mem();
        KV_DB.store(db as usize, Ordering::SeqCst);
        assert_eq!(
            sqlite3_create_module(db, c("spmod").as_ptr(), &SP_MODULE, 0x1234 as *mut c_void),
            SQLITE_OK
        );
        exec_ok(db, "CREATE VIRTUAL TABLE sp USING spmod");
        exec_ok(db, "BEGIN");
        exec_ok(db, "INSERT INTO sp VALUES ('a')");
        take_log();
        exec_ok(db, "SAVEPOINT s1");
        exec_ok(db, "INSERT INTO sp VALUES ('b')");
        exec_ok(db, "ROLLBACK TO s1");
        exec_ok(db, "RELEASE s1");
        exec_ok(db, "COMMIT");
        let log = take_log();
        for want in ["savepoint:0", "rollback_to:0", "release:0", "commit"] {
            assert!(log.contains(&want.to_string()), "{want} not in {log:?}");
        }
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}
