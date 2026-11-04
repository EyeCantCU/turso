//! Virtual table planning tests: a recording module checks the
//! sqlite3_index_info it receives and what xFilter gets. The expected values
//! are SQLite's; every test must pass on turso and on SQLite.

use libc::{c_char, c_int, c_void};
use std::ffi::{CStr, CString};
use std::fmt::Write as _;
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

use super::{
    sqlite3, sqlite3_close, sqlite3_column_count, sqlite3_column_text, sqlite3_column_type,
    sqlite3_create_function_v2, sqlite3_errmsg, sqlite3_exec, sqlite3_finalize, sqlite3_open,
    sqlite3_prepare_v2, sqlite3_result_int, sqlite3_step, sqlite3_value_int64, sqlite3_value_text,
    sqlite3_value_type,
};

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_CONSTRAINT: c_int = 19;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_NULL: c_int = 5;
const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_TEXT: c_int = 3;
const SQLITE_UTF8: c_int = 1;
const SQLITE_DBCONFIG_DEFENSIVE: c_int = 1010;
const SQLITE_INDEX_SCAN_UNIQUE: c_int = 1;

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
    fn sqlite3_mprintf(fmt: *const c_char, ...) -> *mut c_char;
    fn sqlite3_result_null(ctx: *mut c_void);
    fn sqlite3_result_int64(ctx: *mut c_void, v: i64);
    fn sqlite3_result_text(
        ctx: *mut c_void,
        s: *const c_char,
        n: c_int,
        d: Option<unsafe extern "C" fn(*mut c_void)>,
    );
    fn sqlite3_result_error(ctx: *mut c_void, msg: *const c_char, n: c_int);
    fn sqlite3_user_data(ctx: *mut c_void) -> *mut c_void;
    fn sqlite3_value_double(v: *mut c_void) -> f64;
    fn sqlite3_create_module(
        db: *mut sqlite3,
        name: *const c_char,
        module: *const sqlite3_module,
        aux: *mut c_void,
    ) -> c_int;
    fn sqlite3_declare_vtab(db: *mut sqlite3, sql: *const c_char) -> c_int;
    fn sqlite3_vtab_nochange(ctx: *mut c_void) -> c_int;
    fn sqlite3_value_nochange(v: *mut c_void) -> c_int;
    fn sqlite3_vtab_collation(info: *mut sqlite3_index_info, i: c_int) -> *const c_char;
    fn sqlite3_vtab_distinct(info: *mut sqlite3_index_info) -> c_int;
    fn sqlite3_vtab_in(info: *mut sqlite3_index_info, i: c_int, handle: c_int) -> c_int;
    fn sqlite3_vtab_in_first(v: *mut c_void, out: *mut *mut c_void) -> c_int;
    fn sqlite3_vtab_in_next(v: *mut c_void, out: *mut *mut c_void) -> c_int;
    fn sqlite3_vtab_rhs_value(
        info: *mut sqlite3_index_info,
        i: c_int,
        out: *mut *mut c_void,
    ) -> c_int;
    fn sqlite3_db_config(db: *mut sqlite3, op: c_int, ...) -> c_int;
    fn sqlite3_vtab_on_conflict(db: *mut sqlite3) -> c_int;
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
type XFunc = unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_void);

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
    xFindFunction: Option<
        unsafe extern "C" fn(
            *mut sqlite3_vtab,
            c_int,
            *const c_char,
            *mut Option<XFunc>,
            *mut *mut c_void,
        ) -> c_int,
    >,
    xRename: Option<unsafe extern "C" fn(*mut sqlite3_vtab, *const c_char) -> c_int>,
    xSavepoint: Option<unsafe extern "C" fn(*mut sqlite3_vtab, c_int) -> c_int>,
    xRelease: Option<unsafe extern "C" fn(*mut sqlite3_vtab, c_int) -> c_int>,
    xRollbackTo: Option<unsafe extern "C" fn(*mut sqlite3_vtab, c_int) -> c_int>,
    xShadowName: Option<unsafe extern "C" fn(*const c_char) -> c_int>,
    xIntegrity: Option<
        unsafe extern "C" fn(
            *mut sqlite3_vtab,
            *const c_char,
            *const c_char,
            c_int,
            *mut *mut c_char,
        ) -> c_int,
    >,
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

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

/// What xBestIndex does with the constraints it is offered.
#[derive(Clone, Copy, PartialEq)]
#[repr(i32)]
enum Mode {
    /// Use nothing.
    Nothing = 0,
    /// Use every usable constraint, IN all at once; consume the ORDER BY
    /// (returning rows in the requested order).
    All = 1,
    /// Like All, but IN one value per xFilter and the ORDER BY not consumed.
    InPerValue = 2,
    /// SQLITE_CONSTRAINT when a constraint on h is unusable.
    NeedH = 3,
    /// Use usable EQ constraints only; unique when a= is used.
    EqOnly = 4,
    /// Claim the ORDER BY is consumed but return rows in descending order.
    LieAboutOrder = 5,
}

static MODE: AtomicI32 = AtomicI32::new(0);
/// The connection of the test in progress, for sqlite3_vtab_on_conflict.
static DB: std::sync::atomic::AtomicPtr<sqlite3> =
    std::sync::atomic::AtomicPtr::new(ptr::null_mut());
static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Rows of the module: a = 1..=N, b = 's<a>', c = a % 3, h = a.
const N: i64 = 10;

fn set_mode(m: Mode) {
    MODE.store(m as i32, Ordering::SeqCst);
}

fn mode() -> Mode {
    match MODE.load(Ordering::SeqCst) {
        1 => Mode::All,
        2 => Mode::InPerValue,
        3 => Mode::NeedH,
        4 => Mode::EqOnly,
        5 => Mode::LieAboutOrder,
        _ => Mode::Nothing,
    }
}

fn log(s: String) {
    LOG.lock().unwrap().push(s);
}

fn take_log() -> Vec<String> {
    std::mem::take(&mut *LOG.lock().unwrap())
}

fn op_name(op: u8) -> String {
    match op {
        2 => "EQ".into(),
        4 => "GT".into(),
        8 => "LE".into(),
        16 => "LT".into(),
        32 => "GE".into(),
        64 => "MATCH".into(),
        65 => "LIKE".into(),
        66 => "GLOB".into(),
        67 => "REGEXP".into(),
        68 => "NE".into(),
        69 => "ISNOT".into(),
        70 => "ISNOTNULL".into(),
        71 => "ISNULL".into(),
        72 => "IS".into(),
        73 => "LIMIT".into(),
        74 => "OFFSET".into(),
        n => format!("op{n}"),
    }
}

unsafe fn value_text(v: *mut c_void) -> String {
    if v.is_null() {
        return "<none>".into();
    }
    match sqlite3_value_type(v) {
        SQLITE_NULL => "NULL".into(),
        SQLITE_INTEGER => sqlite3_value_int64(v).to_string(),
        SQLITE_FLOAT => sqlite3_value_double(v).to_string(),
        SQLITE_TEXT => format!(
            "'{}'",
            CStr::from_ptr(sqlite3_value_text(v) as *const c_char).to_string_lossy()
        ),
        _ => "blob".into(),
    }
}

unsafe extern "C" fn rec_connect(
    db: *mut sqlite3,
    _aux: *mut c_void,
    argc: c_int,
    argv: *const *const c_char,
    pp: *mut *mut sqlite3_vtab,
    _err: *mut *mut c_char,
) -> c_int {
    log(format!(
        "connect {} {}",
        CStr::from_ptr(*argv.add(1)).to_string_lossy(),
        CStr::from_ptr(*argv.add(2)).to_string_lossy()
    ));
    assert_eq!(argc, 3);
    let rc = sqlite3_declare_vtab(
        db,
        c("CREATE TABLE x(a INTEGER, b TEXT, c, h HIDDEN)").as_ptr(),
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

unsafe extern "C" fn rec_disconnect(v: *mut sqlite3_vtab) -> c_int {
    drop(Box::from_raw(v));
    SQLITE_OK
}

/// Logs the index info, then plans per [`Mode`]. idxStr lists the
/// constraints handed to xFilter as "col:OP,..." and a trailing "D" when
/// rows must come in descending order.
unsafe extern "C" fn rec_best_index(_v: *mut sqlite3_vtab, p: *mut sqlite3_index_info) -> c_int {
    let info = &mut *p;
    let mut line = format!(
        "bestindex n={} colUsed={:x} distinct={}",
        info.nConstraint,
        info.colUsed,
        sqlite3_vtab_distinct(p)
    );
    for i in 0..info.nConstraint {
        let cons = &*info.aConstraint.add(i as usize);
        let mut rhs = ptr::null_mut();
        let rc = sqlite3_vtab_rhs_value(p, i, &mut rhs);
        let rhs = if rc == SQLITE_OK {
            value_text(rhs)
        } else {
            format!("rc{rc}")
        };
        let _ = write!(
            line,
            " [{} {}{} {} in={} rhs={}]",
            cons.iColumn,
            op_name(cons.op),
            if cons.usable != 0 { "" } else { " unusable" },
            CStr::from_ptr(sqlite3_vtab_collation(p, i)).to_string_lossy(),
            sqlite3_vtab_in(p, i, -1),
            rhs
        );
    }
    for i in 0..info.nOrderBy {
        let o = &*info.aOrderBy.add(i as usize);
        let _ = write!(
            line,
            " order({}{})",
            o.iColumn,
            if o.desc != 0 { " desc" } else { "" }
        );
    }
    log(line);
    let m = mode();
    if m == Mode::Nothing {
        return SQLITE_OK;
    }
    if m == Mode::NeedH {
        for i in 0..info.nConstraint as usize {
            let cons = &*info.aConstraint.add(i);
            if cons.iColumn == 3 && cons.usable == 0 {
                log("constraint".into());
                return SQLITE_CONSTRAINT;
            }
        }
    }
    let mut n = 0;
    let mut used_a = false;
    let mut idx = String::new();
    for i in 0..info.nConstraint as usize {
        let cons = &*info.aConstraint.add(i);
        if cons.usable == 0 || (m == Mode::EqOnly && cons.op != 2) {
            continue;
        }
        let is_in = sqlite3_vtab_in(p, i as c_int, -1) != 0;
        if is_in && m != Mode::InPerValue {
            sqlite3_vtab_in(p, i as c_int, 1);
        }
        n += 1;
        let usage = &mut *info.aConstraintUsage.add(i);
        usage.argvIndex = n;
        // xFilter enforces EQ/IN on a, LIMIT and OFFSET; the rest is rechecked.
        usage.omit = ((cons.iColumn == 0 && cons.op == 2) || cons.op == 73 || cons.op == 74) as u8;
        used_a |= cons.iColumn == 0 && cons.op == 2;
        if !idx.is_empty() {
            idx.push(',');
        }
        let _ = write!(idx, "{}:{}", cons.iColumn, op_name(cons.op));
    }
    if info.nOrderBy > 0 && m != Mode::InPerValue {
        info.orderByConsumed = 1;
        let o = &*info.aOrderBy;
        if (o.iColumn == 0 && o.desc != 0) || m == Mode::LieAboutOrder {
            idx.push_str(",D");
        }
    }
    info.idxNum = n;
    info.idxStr = sqlite3_mprintf(c("%s").as_ptr(), c(&idx).as_ptr());
    info.needToFreeIdxStr = 1;
    info.estimatedCost = 1000.0 / (1 + n) as f64;
    info.estimatedRows = 10 / (1 + n) as i64;
    if m == Mode::EqOnly {
        info.estimatedCost = 10.0 / (1 + n) as f64;
        if used_a {
            info.idxFlags |= SQLITE_INDEX_SCAN_UNIQUE;
        }
    }
    SQLITE_OK
}

#[repr(C)]
struct RecCursor {
    base: sqlite3_vtab_cursor,
    rows: Vec<i64>,
    pos: usize,
}

unsafe extern "C" fn rec_open(_v: *mut sqlite3_vtab, pp: *mut *mut sqlite3_vtab_cursor) -> c_int {
    *pp = Box::into_raw(Box::new(RecCursor {
        base: sqlite3_vtab_cursor {
            pVtab: ptr::null_mut(),
        },
        rows: Vec::new(),
        pos: 0,
    })) as *mut sqlite3_vtab_cursor;
    SQLITE_OK
}

unsafe extern "C" fn rec_close(cur: *mut sqlite3_vtab_cursor) -> c_int {
    drop(Box::from_raw(cur as *mut RecCursor));
    SQLITE_OK
}

unsafe extern "C" fn rec_filter(
    cur: *mut sqlite3_vtab_cursor,
    idx_num: c_int,
    idx_str: *const c_char,
    argc: c_int,
    argv: *mut *mut c_void,
) -> c_int {
    let cur = &mut *(cur as *mut RecCursor);
    let idx = if idx_str.is_null() {
        String::new()
    } else {
        CStr::from_ptr(idx_str).to_string_lossy().into_owned()
    };
    let mut line = format!("filter {idx_num} '{idx}'");
    let mut rows: Vec<i64> = (1..=N).collect();
    let (mut limit, mut offset) = (None, 0usize);
    let parts: Vec<&str> = idx.split(',').filter(|s| !s.is_empty()).collect();
    for i in 0..argc as usize {
        let arg = *argv.add(i);
        let mut first = ptr::null_mut();
        let spec = parts.get(i).copied().unwrap_or("");
        if sqlite3_vtab_in_first(arg, &mut first) == SQLITE_OK {
            let mut values = Vec::new();
            let mut v = first;
            while !v.is_null() {
                values.push(value_text(v));
                if spec == "0:EQ" && sqlite3_value_type(v) == SQLITE_INTEGER {
                    // filtered below
                }
                if sqlite3_vtab_in_next(arg, &mut v) != SQLITE_OK {
                    break;
                }
            }
            let _ = write!(line, " in({})", values.join(" "));
            if spec == "0:EQ" {
                rows.retain(|r| values.contains(&r.to_string()));
            }
            continue;
        }
        let _ = write!(line, " {}", value_text(arg));
        match spec {
            "0:EQ" => {
                let v = sqlite3_value_int64(arg);
                rows.retain(|r| *r == v);
            }
            "0:LIMIT" => limit = Some(sqlite3_value_int64(arg) as usize),
            "0:OFFSET" => offset = sqlite3_value_int64(arg) as usize,
            _ => {}
        }
    }
    log(line);
    if parts.last() == Some(&"D") {
        rows.reverse();
    }
    let rows: Vec<i64> = rows.into_iter().skip(offset).collect();
    cur.rows = match limit {
        Some(l) => rows.into_iter().take(l).collect(),
        None => rows,
    };
    cur.pos = 0;
    SQLITE_OK
}

unsafe extern "C" fn rec_next(cur: *mut sqlite3_vtab_cursor) -> c_int {
    (*(cur as *mut RecCursor)).pos += 1;
    SQLITE_OK
}

unsafe extern "C" fn rec_eof(cur: *mut sqlite3_vtab_cursor) -> c_int {
    let cur = &*(cur as *mut RecCursor);
    (cur.pos >= cur.rows.len()) as c_int
}

unsafe extern "C" fn rec_column(
    cur: *mut sqlite3_vtab_cursor,
    ctx: *mut c_void,
    i: c_int,
) -> c_int {
    let cur = &*(cur as *mut RecCursor);
    if sqlite3_vtab_nochange(ctx) != 0 {
        log(format!("nochange {i}"));
        return SQLITE_OK;
    }
    let a = cur.rows[cur.pos];
    match i {
        0 => sqlite3_result_int64(ctx, a),
        1 => {
            let s = c(&format!("s{a}"));
            // SQLITE_TRANSIENT
            let transient =
                std::mem::transmute::<isize, Option<unsafe extern "C" fn(*mut c_void)>>(-1);
            sqlite3_result_text(ctx, s.as_ptr(), -1, transient);
        }
        2 => sqlite3_result_int64(ctx, a % 3),
        3 => sqlite3_result_int64(ctx, a),
        _ => sqlite3_result_null(ctx),
    }
    SQLITE_OK
}

unsafe extern "C" fn rec_rowid(cur: *mut sqlite3_vtab_cursor, rowid: *mut i64) -> c_int {
    let cur = &*(cur as *mut RecCursor);
    *rowid = cur.rows[cur.pos];
    SQLITE_OK
}

unsafe extern "C" fn rec_update(
    _v: *mut sqlite3_vtab,
    argc: c_int,
    argv: *mut *mut c_void,
    _rowid: *mut i64,
) -> c_int {
    let mut line = format!("update {argc}");
    for i in 0..argc as usize {
        let v = *argv.add(i);
        if sqlite3_value_nochange(v) != 0 {
            line.push_str(" nochange");
        } else {
            let _ = write!(line, " {}", value_text(v));
        }
    }
    log(line);
    log(format!(
        "onconflict {}",
        sqlite3_vtab_on_conflict(DB.load(Ordering::SeqCst))
    ));
    SQLITE_OK
}

unsafe extern "C" fn overload(ctx: *mut c_void, _argc: c_int, argv: *mut *mut c_void) {
    let name = CStr::from_ptr(sqlite3_user_data(ctx) as *const c_char).to_string_lossy();
    log(format!(
        "{name}({}, {})",
        value_text(*argv),
        value_text(*argv.add(1))
    ));
    sqlite3_result_int(ctx, (sqlite3_value_int64(*argv) % 2) as i32);
}

unsafe extern "C" fn placeholder(ctx: *mut c_void, _argc: c_int, _argv: *mut *mut c_void) {
    sqlite3_result_error(ctx, c("placeholder").as_ptr(), -1);
}

unsafe extern "C" fn rec_find_function(
    _v: *mut sqlite3_vtab,
    n_arg: c_int,
    name: *const c_char,
    pfunc: *mut Option<XFunc>,
    parg: *mut *mut c_void,
) -> c_int {
    let name = CStr::from_ptr(name).to_string_lossy();
    if name != "like" {
        log(format!("findfunction {name} {n_arg}"));
    }
    match name.as_ref() {
        "rec_fn" => {
            *pfunc = Some(overload);
            *parg = c"rec_fn".as_ptr() as *mut c_void;
            1
        }
        "rec_op" => {
            *pfunc = Some(overload);
            *parg = c"rec_op".as_ptr() as *mut c_void;
            151
        }
        _ => 0,
    }
}

unsafe extern "C" fn rec_shadow_name(suffix: *const c_char) -> c_int {
    (CStr::from_ptr(suffix).to_bytes() == b"data") as c_int
}

unsafe extern "C" fn rec_integrity(
    _v: *mut sqlite3_vtab,
    schema: *const c_char,
    table: *const c_char,
    quick: c_int,
    err: *mut *mut c_char,
) -> c_int {
    let table = CStr::from_ptr(table).to_string_lossy().into_owned();
    log(format!(
        "integrity {}.{table} {quick}",
        CStr::from_ptr(schema).to_string_lossy()
    ));
    *err = sqlite3_mprintf(c("%s needs attention").as_ptr(), c(&table).as_ptr());
    SQLITE_OK
}

static REC_MODULE: sqlite3_module = sqlite3_module {
    iVersion: 4,
    xCreate: Some(rec_connect),
    xConnect: Some(rec_connect),
    xBestIndex: Some(rec_best_index),
    xDisconnect: Some(rec_disconnect),
    xDestroy: Some(rec_disconnect),
    xOpen: Some(rec_open),
    xClose: Some(rec_close),
    xFilter: Some(rec_filter),
    xNext: Some(rec_next),
    xEof: Some(rec_eof),
    xColumn: Some(rec_column),
    xRowid: Some(rec_rowid),
    xUpdate: Some(rec_update),
    xBegin: None,
    xSync: None,
    xCommit: None,
    xRollback: None,
    xFindFunction: Some(rec_find_function),
    xRename: None,
    xSavepoint: None,
    xRelease: None,
    xRollbackTo: None,
    xShadowName: Some(rec_shadow_name),
    xIntegrity: Some(rec_integrity),
};

unsafe fn open_rec() -> *mut sqlite3 {
    let mut db = ptr::null_mut();
    assert_eq!(sqlite3_open(c(":memory:").as_ptr(), &mut db), SQLITE_OK);
    assert_eq!(
        sqlite3_create_module(db, c("rec").as_ptr(), &REC_MODULE, ptr::null_mut()),
        SQLITE_OK
    );
    DB.store(db, Ordering::SeqCst);
    exec_ok(db, "CREATE TABLE t1(x INTEGER PRIMARY KEY, y, z)");
    exec_ok(db, "INSERT INTO t1 VALUES(1, 2, 3), (2, 3, 4)");
    take_log();
    db
}

unsafe fn errmsg(db: *mut sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db))
        .to_string_lossy()
        .into_owned()
}

unsafe fn exec_ok(db: *mut sqlite3, sql: &str) {
    let rc = sqlite3_exec(db, c(sql).as_ptr(), None, ptr::null_mut(), ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "{sql}: {}", errmsg(db));
}

/// Runs `sql`; returns its rows (columns joined by '|') or "error: <msg>".
unsafe fn run(db: *mut sqlite3, sql: &str) -> Vec<String> {
    let mut stmt = ptr::null_mut();
    let rc = sqlite3_prepare_v2(db, c(sql).as_ptr(), -1, &mut stmt, ptr::null_mut());
    if rc != SQLITE_OK {
        return vec![format!("error: {}", errmsg(db))];
    }
    let mut rows = Vec::new();
    loop {
        match sqlite3_step(stmt) {
            SQLITE_ROW => {
                let n = sqlite3_column_count(stmt);
                let cols: Vec<String> = (0..n)
                    .map(|i| {
                        if sqlite3_column_type(stmt, i) == SQLITE_NULL {
                            "NULL".to_string()
                        } else {
                            CStr::from_ptr(sqlite3_column_text(stmt, i) as *const c_char)
                                .to_string_lossy()
                                .into_owned()
                        }
                    })
                    .collect();
                rows.push(cols.join("|"));
            }
            SQLITE_DONE => break,
            _ => {
                rows.push(format!("error: {}", errmsg(db)));
                break;
            }
        }
    }
    sqlite3_finalize(stmt);
    rows
}

/// The xBestIndex lines logged while preparing and running `sql`.
unsafe fn best_index_log(db: *mut sqlite3, sql: &str) -> Vec<String> {
    take_log();
    run(db, sql);
    take_log()
        .into_iter()
        .filter(|l| l.starts_with("bestindex"))
        .collect()
}

fn strip_error_prefix(rows: Vec<String>) -> Vec<String> {
    rows.into_iter()
        .map(|r| r.replace("error: Parse error: ", "error: "))
        .collect()
}

#[test]
fn test_vtab_plan_constraint_list() {
    unsafe {
        let db = open_rec();
        set_mode(Mode::Nothing);
        let cases: &[(&str, &str)] = &[
            (
                "SELECT a FROM rec WHERE a = 5",
                "bestindex n=1 colUsed=1 distinct=0 [0 EQ BINARY in=0 rhs=5]",
            ),
            (
                "SELECT a FROM rec WHERE 5 < a",
                "bestindex n=1 colUsed=1 distinct=0 [0 GT BINARY in=0 rhs=5]",
            ),
            (
                "SELECT a FROM rec WHERE a > 5 AND b < 'x' AND c >= 1 AND a <= 9",
                "bestindex n=4 colUsed=7 distinct=0 [0 GT BINARY in=0 rhs=5] \
                 [1 LT BINARY in=0 rhs='x'] [2 GE BINARY in=0 rhs=1] [0 LE BINARY in=0 rhs=9]",
            ),
            // Indexable terms keep their place; !=, IS NOT and IS NOT NULL
            // follow, last term first.
            (
                "SELECT a FROM rec WHERE a != 5 AND b IS NOT 'x' AND c IS NOT NULL \
                 AND a IS NULL AND b IS 'q'",
                "bestindex n=5 colUsed=7 distinct=0 [0 ISNULL BINARY in=0 rhs=rc12] \
                 [1 IS BINARY in=0 rhs='q'] [2 ISNOTNULL BINARY in=0 rhs=rc12] \
                 [1 ISNOT BINARY in=0 rhs='x'] [0 NE BINARY in=0 rhs=5]",
            ),
            (
                "SELECT a FROM rec WHERE a = b",
                "bestindex n=0 colUsed=3 distinct=0",
            ),
            (
                "SELECT a FROM rec WHERE b LIKE 'abc%'",
                "bestindex n=3 colUsed=3 distinct=0 [1 GE NOCASE in=0 rhs='ABC'] \
                 [1 LT NOCASE in=0 rhs='abd'] [1 LIKE BINARY in=0 rhs='abc%']",
            ),
            (
                "SELECT a FROM rec WHERE b LIKE '%abc'",
                "bestindex n=1 colUsed=3 distinct=0 [1 LIKE BINARY in=0 rhs='%abc']",
            ),
            (
                "SELECT a FROM rec WHERE b GLOB 'a*'",
                "bestindex n=3 colUsed=3 distinct=0 [1 GE BINARY in=0 rhs='a'] \
                 [1 LT BINARY in=0 rhs='b'] [1 GLOB BINARY in=0 rhs='a*']",
            ),
            (
                "SELECT a FROM rec WHERE b NOT LIKE '%abc'",
                "bestindex n=0 colUsed=3 distinct=0",
            ),
            (
                "SELECT a FROM rec WHERE a IN (1,2,3)",
                "bestindex n=1 colUsed=1 distinct=0 [0 EQ BINARY in=1 rhs=rc12]",
            ),
            (
                "SELECT a FROM rec WHERE a IN (7)",
                "bestindex n=1 colUsed=1 distinct=0 [0 EQ BINARY in=0 rhs=7]",
            ),
            (
                "SELECT a FROM rec WHERE a IN (SELECT x FROM t1)",
                "bestindex n=1 colUsed=1 distinct=0 [0 EQ BINARY in=1 rhs=rc12]",
            ),
            (
                "SELECT a FROM rec WHERE a NOT IN (1,2)",
                "bestindex n=0 colUsed=1 distinct=0",
            ),
            (
                "SELECT a FROM rec WHERE a BETWEEN 1 AND 3",
                "bestindex n=2 colUsed=1 distinct=0 [0 GE BINARY in=0 rhs=1] \
                 [0 LE BINARY in=0 rhs=3]",
            ),
            (
                "SELECT a FROM rec WHERE b = 'x' COLLATE nocase",
                "bestindex n=1 colUsed=3 distinct=0 [1 EQ NOCASE in=0 rhs=rc12]",
            ),
            (
                "SELECT a FROM rec WHERE b COLLATE rtrim = 'x'",
                "bestindex n=1 colUsed=3 distinct=0 [1 EQ RTRIM in=0 rhs='x']",
            ),
            (
                "SELECT a FROM rec WHERE rowid = 3",
                "bestindex n=1 colUsed=1 distinct=0 [-1 EQ BINARY in=0 rhs=3]",
            ),
            (
                "SELECT a FROM rec WHERE a = ?1",
                "bestindex n=1 colUsed=1 distinct=0 [0 EQ BINARY in=0 rhs=rc12]",
            ),
            (
                "SELECT a FROM rec WHERE a = -5 AND b = 1.5",
                "bestindex n=2 colUsed=3 distinct=0 [0 EQ BINARY in=0 rhs=-5] \
                 [1 EQ BINARY in=0 rhs=1.5]",
            ),
            (
                "SELECT a FROM rec WHERE a = 1+2",
                "bestindex n=1 colUsed=1 distinct=0 [0 EQ BINARY in=0 rhs=rc12]",
            ),
            (
                "SELECT a FROM rec WHERE (a, b) = (1, 'x')",
                "bestindex n=2 colUsed=3 distinct=0 [0 EQ BINARY in=0 rhs=1] \
                 [1 EQ BINARY in=0 rhs='x']",
            ),
            (
                "SELECT a FROM rec WHERE h = 5",
                "bestindex n=1 colUsed=9 distinct=0 [3 EQ BINARY in=0 rhs=5]",
            ),
            (
                "SELECT * FROM rec(5)",
                "bestindex n=1 colUsed=f distinct=0 [3 EQ BINARY in=0 rhs=5]",
            ),
            (
                "SELECT a, b, h FROM rec",
                "bestindex n=0 colUsed=b distinct=0",
            ),
            (
                "SELECT a FROM t1, rec WHERE a = x AND b = y AND c > 1",
                "bestindex n=3 colUsed=7 distinct=0 [0 EQ BINARY in=0 rhs=rc12] \
                 [1 EQ BINARY in=0 rhs=rc12] [2 GT BINARY in=0 rhs=1]",
            ),
        ];
        for (sql, expected) in cases {
            assert_eq!(best_index_log(db, sql), vec![expected.to_string()], "{sql}");
        }
        sqlite3_close(db);
    }
}

#[test]
fn test_vtab_plan_order_by_and_distinct() {
    unsafe {
        let db = open_rec();
        set_mode(Mode::Nothing);
        let cases: &[(&str, &str)] = &[
            (
                "SELECT a FROM rec ORDER BY a",
                "bestindex n=0 colUsed=1 distinct=0 order(0)",
            ),
            (
                "SELECT a FROM rec ORDER BY a DESC, b",
                "bestindex n=0 colUsed=3 distinct=0 order(0 desc) order(1)",
            ),
            (
                "SELECT a FROM rec ORDER BY rowid",
                "bestindex n=0 colUsed=1 distinct=0 order(-1)",
            ),
            (
                "SELECT a FROM rec ORDER BY a NULLS LAST",
                "bestindex n=0 colUsed=1 distinct=0",
            ),
            (
                "SELECT a FROM rec ORDER BY a+1",
                "bestindex n=0 colUsed=1 distinct=0",
            ),
            (
                "SELECT a FROM rec ORDER BY b COLLATE nocase",
                "bestindex n=0 colUsed=3 distinct=0",
            ),
            (
                "SELECT a FROM rec ORDER BY 1",
                "bestindex n=0 colUsed=1 distinct=0 order(0)",
            ),
            (
                "SELECT a, count(*) FROM rec GROUP BY a",
                "bestindex n=0 colUsed=1 distinct=1 order(0)",
            ),
            (
                "SELECT a, count(*) FROM rec GROUP BY a ORDER BY a",
                "bestindex n=0 colUsed=1 distinct=0 order(0)",
            ),
            (
                "SELECT DISTINCT a FROM rec",
                "bestindex n=0 colUsed=1 distinct=2 order(0)",
            ),
            (
                "SELECT DISTINCT a FROM rec ORDER BY a",
                "bestindex n=0 colUsed=1 distinct=3 order(0)",
            ),
            (
                "SELECT DISTINCT a, b FROM rec ORDER BY b",
                "bestindex n=0 colUsed=3 distinct=3 order(1)",
            ),
            (
                "SELECT count(*) FROM rec",
                "bestindex n=0 colUsed=0 distinct=0",
            ),
        ];
        for (sql, expected) in cases {
            assert_eq!(best_index_log(db, sql), vec![expected.to_string()], "{sql}");
        }

        // A consumed ORDER BY is trusted: the engine does not sort again.
        set_mode(Mode::LieAboutOrder);
        assert_eq!(
            run(db, "SELECT a FROM rec WHERE a > 7 ORDER BY a"),
            vec!["10", "9", "8"]
        );
        // Not consumed: the engine sorts.
        set_mode(Mode::InPerValue);
        assert_eq!(
            run(db, "SELECT a FROM rec WHERE a IN (3, 1, 2) ORDER BY a DESC"),
            vec!["3", "2", "1"]
        );
        set_mode(Mode::All);
        assert_eq!(
            run(db, "SELECT a FROM rec ORDER BY a DESC LIMIT 2"),
            vec!["10", "9"]
        );
        sqlite3_close(db);
    }
}

#[test]
fn test_vtab_plan_limit_offset() {
    unsafe {
        let db = open_rec();
        set_mode(Mode::Nothing);
        let cases: &[(&str, &str)] = &[
            (
                "SELECT a FROM rec LIMIT 3",
                "bestindex n=1 colUsed=1 distinct=0 [0 LIMIT BINARY in=0 rhs=3]",
            ),
            (
                "SELECT a FROM rec LIMIT 3 OFFSET 2",
                "bestindex n=2 colUsed=1 distinct=0 [0 OFFSET BINARY in=0 rhs=2] \
                 [0 LIMIT BINARY in=0 rhs=3]",
            ),
            (
                "SELECT a FROM rec WHERE a = 1 LIMIT 3",
                "bestindex n=2 colUsed=1 distinct=0 [0 EQ BINARY in=0 rhs=1] \
                 [0 LIMIT BINARY in=0 rhs=3]",
            ),
            (
                "SELECT a FROM rec ORDER BY a LIMIT 3",
                "bestindex n=1 colUsed=1 distinct=0 [0 LIMIT BINARY in=0 rhs=3] order(0)",
            ),
            (
                "SELECT a FROM rec ORDER BY a+1 LIMIT 3",
                "bestindex n=0 colUsed=1 distinct=0",
            ),
            (
                "SELECT count(*) FROM rec LIMIT 3",
                "bestindex n=0 colUsed=0 distinct=0",
            ),
            (
                "SELECT DISTINCT a FROM rec LIMIT 3",
                "bestindex n=0 colUsed=1 distinct=2 order(0)",
            ),
            (
                "SELECT a FROM rec JOIN t1 LIMIT 3",
                "bestindex n=0 colUsed=1 distinct=0",
            ),
            (
                "SELECT a FROM rec WHERE a + 1 = 3 LIMIT 3",
                "bestindex n=0 colUsed=1 distinct=0",
            ),
        ];
        for (sql, expected) in cases {
            assert_eq!(best_index_log(db, sql), vec![expected.to_string()], "{sql}");
        }

        // The module skips the OFFSET rows itself, so the engine must not.
        set_mode(Mode::All);
        take_log();
        assert_eq!(
            run(db, "SELECT a FROM rec LIMIT 3 OFFSET 2"),
            vec!["3", "4", "5"]
        );
        assert!(take_log().contains(&"filter 2 '0:OFFSET,0:LIMIT' 2 3".to_string()));
        // One xFilter per IN value cannot use LIMIT: SQLite retries without it.
        set_mode(Mode::InPerValue);
        assert_eq!(
            best_index_log(db, "SELECT a FROM rec WHERE a IN (1,2) LIMIT 3"),
            vec![
                "bestindex n=2 colUsed=1 distinct=0 [0 EQ BINARY in=1 rhs=rc12] \
                 [0 LIMIT BINARY in=0 rhs=3]",
                "bestindex n=2 colUsed=1 distinct=0 [0 EQ BINARY in=1 rhs=rc12] \
                 [0 LIMIT unusable BINARY in=0 rhs=3]",
                "bestindex n=2 colUsed=1 distinct=0 [0 EQ unusable BINARY in=1 rhs=rc12] \
                 [0 LIMIT unusable BINARY in=0 rhs=3]",
            ]
        );
        assert_eq!(
            run(db, "SELECT a FROM rec WHERE a IN (1,2) LIMIT 3"),
            vec!["1", "2"]
        );
        sqlite3_close(db);
    }
}

#[test]
fn test_vtab_plan_in_constraints() {
    unsafe {
        let db = open_rec();
        // All at once: one xFilter with the sorted, distinct values.
        set_mode(Mode::All);
        take_log();
        assert_eq!(
            run(db, "SELECT a FROM rec WHERE a IN (3, 1, 2, 1)"),
            vec!["1", "2", "3"]
        );
        let filters: Vec<String> = take_log()
            .into_iter()
            .filter(|l| l.starts_with("filter"))
            .collect();
        assert_eq!(filters, vec!["filter 1 '0:EQ' in(1 2 3)"]);
        take_log();
        assert_eq!(
            run(db, "SELECT a FROM rec WHERE a IN (3, 1, NULL) AND b = 's1'"),
            vec!["1"]
        );
        let filters: Vec<String> = take_log()
            .into_iter()
            .filter(|l| l.starts_with("filter"))
            .collect();
        assert_eq!(filters, vec!["filter 2 '0:EQ,1:EQ' in(NULL 1 3) 's1'"]);
        take_log();
        assert_eq!(
            run(db, "SELECT a FROM rec WHERE a IN (SELECT x FROM t1)"),
            vec!["1", "2"]
        );
        assert!(take_log().contains(&"filter 1 '0:EQ' in(1 2)".to_string()));

        // One value per xFilter; NULL never reaches xFilter.
        set_mode(Mode::InPerValue);
        take_log();
        assert_eq!(
            run(db, "SELECT a FROM rec WHERE a IN (3, 1, NULL) AND b = 's1'"),
            vec!["1"]
        );
        let filters: Vec<String> = take_log()
            .into_iter()
            .filter(|l| l.starts_with("filter"))
            .collect();
        assert_eq!(
            filters,
            vec!["filter 2 '0:EQ,1:EQ' 1 's1'", "filter 2 '0:EQ,1:EQ' 3 's1'"]
        );
        take_log();
        assert_eq!(
            run(db, "SELECT a, c FROM rec WHERE a IN (1, 2) AND c IN (1, 2)"),
            vec!["1|1", "2|2"]
        );
        let filters: Vec<String> = take_log()
            .into_iter()
            .filter(|l| l.starts_with("filter"))
            .collect();
        assert_eq!(
            filters,
            vec![
                "filter 2 '0:EQ,2:EQ' 1 1",
                "filter 2 '0:EQ,2:EQ' 1 2",
                "filter 2 '0:EQ,2:EQ' 2 1",
                "filter 2 '0:EQ,2:EQ' 2 2",
            ]
        );
        // Outside xBestIndex, an argument is not an IN list.
        sqlite3_close(db);
    }
}

#[test]
fn test_vtab_plan_join_calls_and_costs() {
    unsafe {
        let db = open_rec();
        set_mode(Mode::All);
        assert_eq!(
            best_index_log(db, "SELECT a FROM t1, rec WHERE a = x"),
            vec![
                "bestindex n=1 colUsed=1 distinct=0 [0 EQ BINARY in=0 rhs=rc12]",
                "bestindex n=1 colUsed=1 distinct=0 [0 EQ unusable BINARY in=0 rhs=rc12]",
            ]
        );
        assert_eq!(
            best_index_log(db, "SELECT a FROM t1, rec WHERE a = x AND h = 5"),
            vec![
                "bestindex n=2 colUsed=9 distinct=0 [0 EQ BINARY in=0 rhs=rc12] \
                 [3 EQ BINARY in=0 rhs=5]",
                "bestindex n=2 colUsed=9 distinct=0 [0 EQ unusable BINARY in=0 rhs=rc12] \
                 [3 EQ BINARY in=0 rhs=5]",
            ]
        );
        // A plan that needs h is rejected until t1 is available.
        set_mode(Mode::NeedH);
        take_log();
        assert_eq!(
            run(db, "SELECT x, a FROM rec, t1 WHERE h = x"),
            vec!["1|1", "2|2"]
        );
        let log = take_log();
        assert!(log.contains(&"constraint".to_string()), "{log:?}");
        assert_eq!(
            log.iter().filter(|l| l.starts_with("filter")).count(),
            2,
            "{log:?}"
        );
        // SQLite prefers one scan of rec and a rowid search of t1.
        set_mode(Mode::EqOnly);
        take_log();
        assert_eq!(
            run(db, "SELECT x, b FROM t1, rec WHERE a = x"),
            vec!["1|s1", "2|s2"]
        );
        let filters: Vec<String> = take_log()
            .into_iter()
            .filter(|l| l.starts_with("filter"))
            .collect();
        assert_eq!(filters, vec!["filter 0 ''"]);
        sqlite3_close(db);
    }
}

#[test]
fn test_vtab_plan_explain_query_plan() {
    unsafe {
        let db = open_rec();
        set_mode(Mode::All);
        let rows = run(db, "EXPLAIN QUERY PLAN SELECT a FROM rec WHERE a = 5");
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(
            rows[0].ends_with("|SCAN rec VIRTUAL TABLE INDEX 1:0:EQ"),
            "{rows:?}"
        );
        set_mode(Mode::Nothing);
        let rows = run(db, "EXPLAIN QUERY PLAN SELECT a FROM rec");
        assert!(
            rows[0].ends_with("|SCAN rec VIRTUAL TABLE INDEX 0:"),
            "{rows:?}"
        );
        sqlite3_close(db);
    }
}

#[test]
fn test_vtab_plan_find_function() {
    unsafe {
        let db = open_rec();
        for name in ["rec_fn", "rec_op"] {
            assert_eq!(
                sqlite3_create_function_v2(
                    db,
                    c(name).as_ptr(),
                    2,
                    SQLITE_UTF8,
                    ptr::null_mut(),
                    Some(placeholder),
                    None,
                    None,
                    None,
                ),
                SQLITE_OK
            );
        }
        set_mode(Mode::Nothing);
        take_log();
        assert_eq!(
            run(db, "SELECT a FROM rec WHERE rec_fn(a, 3) AND a < 4"),
            vec!["1", "3"]
        );
        let log = take_log();
        assert!(log.contains(&"rec_fn(1, 3)".to_string()), "{log:?}");
        assert!(
            log.contains(&"findfunction rec_fn 2".to_string()),
            "{log:?}"
        );
        // A code of at least 150 makes the call a constraint.
        assert_eq!(
            best_index_log(db, "SELECT a FROM rec WHERE rec_op(a, 3)"),
            vec!["bestindex n=1 colUsed=1 distinct=0 [0 op151 BINARY in=0 rhs=3]"]
        );
        assert_eq!(
            run(db, "SELECT a FROM rec WHERE rec_op(a, 3) AND a > 8"),
            vec!["9"]
        );
        // Not a column first: the registered function runs.
        let rows = run(db, "SELECT a FROM rec WHERE rec_op(3, a)");
        assert!(
            rows.len() == 1 && rows[0].starts_with("error:") && rows[0].contains("placeholder"),
            "{rows:?}"
        );
        sqlite3_close(db);
    }
}

#[test]
fn test_vtab_plan_nochange_update() {
    unsafe {
        let db = open_rec();
        set_mode(Mode::All);
        take_log();
        run(db, "UPDATE rec SET b = 'q' WHERE a = 3");
        let log = take_log();
        let updates: Vec<&String> = log.iter().filter(|l| l.starts_with("update")).collect();
        assert_eq!(
            updates,
            vec!["update 6 3 3 nochange 'q' nochange nochange"],
            "{log:?}"
        );
        assert!(log.contains(&"nochange 0".to_string()), "{log:?}");
        assert!(log.contains(&"nochange 2".to_string()), "{log:?}");
        take_log();
        run(db, "UPDATE rec SET c = a WHERE a = 2");
        let log = take_log();
        let updates: Vec<&String> = log.iter().filter(|l| l.starts_with("update")).collect();
        assert_eq!(
            updates,
            vec!["update 6 2 2 nochange nochange 2 nochange"],
            "{log:?}"
        );
        sqlite3_close(db);
    }
}

#[test]
fn test_vtab_plan_eponymous_writes() {
    unsafe {
        let db = open_rec();
        set_mode(Mode::All);
        take_log();
        assert!(run(db, "INSERT INTO rec(a, b) VALUES (11, 'x')").is_empty());
        assert!(run(db, "DELETE FROM rec WHERE a = 4").is_empty());
        let log = take_log();
        assert!(
            log.contains(&"update 6 NULL NULL 11 'x' NULL NULL".to_string()),
            "{log:?}"
        );
        assert!(log.contains(&"update 1 4".to_string()), "{log:?}");
        assert!(log.contains(&"onconflict 4".to_string()), "{log:?}");

        // RETURNING works for INSERT only; the rowid it reports is -1.
        assert_eq!(
            run(
                db,
                "INSERT INTO rec(a, b) VALUES (7, 'p'), (8, 'q') RETURNING b, a + 1, rowid"
            ),
            vec!["p|8|-1", "q|9|-1"]
        );
        assert_eq!(
            run(db, "INSERT INTO rec(a) VALUES (1) RETURNING *"),
            vec!["1|NULL|NULL"]
        );
        assert_eq!(
            strip_error_prefix(run(db, "UPDATE rec SET b = 'y' WHERE a = 2 RETURNING a")),
            vec!["error: UPDATE RETURNING is not available on virtual tables"]
        );
        assert_eq!(
            strip_error_prefix(run(db, "DELETE FROM rec WHERE a = 2 RETURNING a")),
            vec!["error: DELETE RETURNING is not available on virtual tables"]
        );
        // The statement's conflict mode reaches sqlite3_vtab_on_conflict.
        for (sql, mode) in [
            ("INSERT OR ROLLBACK INTO rec(a) VALUES (1)", 1),
            ("INSERT OR IGNORE INTO rec(a) VALUES (1)", 2),
            ("INSERT OR FAIL INTO rec(a) VALUES (1)", 3),
            ("INSERT OR REPLACE INTO rec(a) VALUES (1)", 5),
            ("UPDATE OR FAIL rec SET b = 'z' WHERE a = 4", 3),
            ("UPDATE OR ROLLBACK rec SET b = 'z' WHERE a = 4", 1),
        ] {
            take_log();
            assert!(run(db, sql).is_empty(), "{sql}");
            assert!(take_log().contains(&format!("onconflict {mode}")), "{sql}");
        }
        sqlite3_close(db);
    }
}

#[test]
fn test_vtab_plan_temp_schema_and_integrity() {
    unsafe {
        let db = open_rec();
        set_mode(Mode::All);
        take_log();
        exec_ok(db, "CREATE VIRTUAL TABLE temp.tv USING rec");
        assert_eq!(take_log(), vec!["connect temp tv"]);
        assert_eq!(run(db, "SELECT name FROM temp.sqlite_master"), vec!["tv"]);
        assert_eq!(
            run(db, "SELECT count(*) FROM sqlite_master WHERE name = 'tv'"),
            vec!["0"]
        );
        assert_eq!(run(db, "SELECT a FROM tv WHERE a = 2"), vec!["2"]);
        exec_ok(db, "DROP TABLE tv");

        take_log();
        exec_ok(db, "CREATE VIRTUAL TABLE pv USING rec");
        assert_eq!(take_log(), vec!["connect main pv"]);
        assert_eq!(
            run(db, "PRAGMA integrity_check"),
            vec!["pv needs attention"]
        );
        assert_eq!(take_log(), vec!["integrity main.pv 0"]);
        assert_eq!(run(db, "PRAGMA quick_check"), vec!["pv needs attention"]);
        assert_eq!(take_log(), vec!["integrity main.pv 1"]);
        sqlite3_close(db);
    }
}

#[test]
fn test_vtab_plan_shadow_tables_defensive() {
    unsafe {
        let db = open_rec();
        exec_ok(db, "CREATE VIRTUAL TABLE pv USING rec");
        exec_ok(db, "CREATE TABLE pv_data(x)");
        let mut on: c_int = -1;
        assert_eq!(
            sqlite3_db_config(
                db,
                SQLITE_DBCONFIG_DEFENSIVE,
                1 as c_int,
                &mut on as *mut c_int
            ),
            SQLITE_OK
        );
        assert_eq!(on, 1);
        for (sql, err) in [
            (
                "INSERT INTO pv_data VALUES(1)",
                "table pv_data may not be modified",
            ),
            (
                "UPDATE pv_data SET x = 2",
                "table pv_data may not be modified",
            ),
            ("DELETE FROM pv_data", "table pv_data may not be modified"),
            (
                "ALTER TABLE pv_data ADD COLUMN y",
                "table pv_data may not be altered",
            ),
            (
                "CREATE TRIGGER tr AFTER INSERT ON pv_data BEGIN SELECT 1; END",
                "cannot create triggers on shadow tables",
            ),
            ("DROP TABLE pv_data", "table pv_data may not be dropped"),
        ] {
            assert_eq!(
                strip_error_prefix(run(db, sql)),
                vec![format!("error: {err}")],
                "{sql}"
            );
        }
        // Reads and other tables are unaffected.
        assert_eq!(run(db, "SELECT count(*) FROM pv_data"), vec!["0"]);
        exec_ok(db, "CREATE TABLE pv_other(x)");
        exec_ok(db, "INSERT INTO pv_other VALUES(1)");
        assert_eq!(
            sqlite3_db_config(
                db,
                SQLITE_DBCONFIG_DEFENSIVE,
                0 as c_int,
                &mut on as *mut c_int
            ),
            SQLITE_OK
        );
        assert_eq!(on, 0);
        exec_ok(db, "INSERT INTO pv_data VALUES(1)");
        exec_ok(db, "DROP TABLE pv_data");
        sqlite3_close(db);
    }
}
