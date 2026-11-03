//! Limits, trace/profile, collation_needed, blob reopen, UDF error codes,
//! UTF-16 open and the SQL function load_extension(). Each test must pass on
//! turso and on real SQLite.
#![allow(non_camel_case_types, non_upper_case_globals)]
use libc::{c_char, c_int, c_uint, c_void};
use std::ffi::{CStr, CString};
use std::ptr;
use std::sync::Mutex;

use super::{
    sqlite3_bind_int, sqlite3_column_blob, sqlite3_column_bytes, sqlite3_column_text, sqlite3_stmt,
    sqlite3_value_blob, sqlite3_value_text,
};

#[repr(C)]
struct sqlite3 {
    _private: [u8; 0],
}

type Cb = unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int;
type TraceV2 = unsafe extern "C" fn(c_uint, *mut c_void, *mut c_void, *mut c_void) -> c_int;
type UdfFn = unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_void);

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
    fn sqlite3_config(op: c_int, ...) -> c_int;
    fn sqlite3_open(filename: *const c_char, db: *mut *mut sqlite3) -> c_int;
    fn sqlite3_open16(filename: *const c_void, db: *mut *mut sqlite3) -> c_int;
    fn sqlite3_close(db: *mut sqlite3) -> c_int;
    fn sqlite3_errcode(db: *mut sqlite3) -> c_int;
    fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char;
    fn sqlite3_free(p: *mut c_void);
    fn sqlite3_exec(
        db: *mut sqlite3,
        sql: *const c_char,
        cb: Option<Cb>,
        arg: *mut c_void,
        err: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_limit(db: *mut sqlite3, id: c_int, val: c_int) -> c_int;
    fn sqlite3_prepare_v2(
        db: *mut sqlite3,
        sql: *const c_char,
        n: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_step(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_reset(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_finalize(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_sql(stmt: *mut sqlite3_stmt) -> *const c_char;
    fn sqlite3_expanded_sql(stmt: *mut sqlite3_stmt) -> *mut c_char;
    fn sqlite3_bind_text(
        stmt: *mut sqlite3_stmt,
        idx: c_int,
        text: *const c_char,
        len: c_int,
        destructor: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    fn sqlite3_bind_blob(
        stmt: *mut sqlite3_stmt,
        idx: c_int,
        blob: *const c_void,
        len: c_int,
        destructor: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    fn sqlite3_trace_v2(
        db: *mut sqlite3,
        mask: c_uint,
        cb: Option<TraceV2>,
        ctx: *mut c_void,
    ) -> c_int;
    fn sqlite3_trace(
        db: *mut sqlite3,
        cb: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
        ctx: *mut c_void,
    ) -> *mut c_void;
    fn sqlite3_profile(
        db: *mut sqlite3,
        cb: Option<unsafe extern "C" fn(*mut c_void, *const c_char, u64)>,
        ctx: *mut c_void,
    ) -> *mut c_void;
    fn sqlite3_create_function_v2(
        db: *mut sqlite3,
        name: *const c_char,
        n_args: c_int,
        enc: c_int,
        ctx: *mut c_void,
        func: Option<UdfFn>,
        step: Option<unsafe extern "C" fn()>,
        fin: Option<unsafe extern "C" fn()>,
        destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    fn sqlite3_create_collation_v2(
        db: *mut sqlite3,
        name: *const c_char,
        enc: c_int,
        ctx: *mut c_void,
        cmp: Option<
            unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int,
        >,
        destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    fn sqlite3_collation_needed(
        db: *mut sqlite3,
        ctx: *mut c_void,
        cb: Option<unsafe extern "C" fn(*mut c_void, *mut sqlite3, c_int, *const c_char)>,
    ) -> c_int;
    fn sqlite3_collation_needed16(
        db: *mut sqlite3,
        ctx: *mut c_void,
        cb: Option<unsafe extern "C" fn(*mut c_void, *mut sqlite3, c_int, *const c_void)>,
    ) -> c_int;
    fn sqlite3_result_int(ctx: *mut c_void, v: c_int);
    fn sqlite3_result_error_toobig(ctx: *mut c_void);
    fn sqlite3_result_error_nomem(ctx: *mut c_void);
    fn sqlite3_result_error_code(ctx: *mut c_void, code: c_int);
    fn sqlite3_result_error(ctx: *mut c_void, msg: *const c_char, n: c_int);
    fn sqlite3_blob_open(
        db: *mut sqlite3,
        db_name: *const c_char,
        table: *const c_char,
        column: *const c_char,
        rowid: i64,
        flags: c_int,
        blob: *mut *mut c_void,
    ) -> c_int;
    fn sqlite3_blob_reopen(blob: *mut c_void, rowid: i64) -> c_int;
    fn sqlite3_blob_read(blob: *mut c_void, buf: *mut c_void, n: c_int, off: c_int) -> c_int;
    fn sqlite3_blob_write(blob: *mut c_void, buf: *const c_void, n: c_int, off: c_int) -> c_int;
    fn sqlite3_blob_bytes(blob: *mut c_void) -> c_int;
    fn sqlite3_blob_close(blob: *mut c_void) -> c_int;
    fn sqlite3_value_type(v: *mut c_void) -> c_int;
    fn sqlite3_value_int(v: *mut c_void) -> c_int;
    fn sqlite3_value_int64(v: *mut c_void) -> i64;
    fn sqlite3_value_double(v: *mut c_void) -> f64;
    fn sqlite3_value_text16(v: *mut c_void) -> *const u16;
    fn sqlite3_value_bytes(v: *mut c_void) -> c_int;
    fn sqlite3_value_bytes16(v: *mut c_void) -> c_int;
    fn sqlite3_result_text(ctx: *mut c_void, t: *const c_char, n: c_int, d: isize);
    fn sqlite3_column_type(stmt: *mut sqlite3_stmt, i: c_int) -> c_int;
    fn sqlite3_column_int64(stmt: *mut sqlite3_stmt, i: c_int) -> i64;
    fn sqlite3_column_double(stmt: *mut sqlite3_stmt, i: c_int) -> f64;
    fn sqlite3_enable_load_extension(db: *mut sqlite3, onoff: c_int) -> c_int;
    fn sqlite3_db_config(db: *mut sqlite3, op: c_int, ...) -> c_int;
}

#[cfg(not(feature = "sqlite3"))]
extern "C" {
    fn turso_enable_experimental();
}

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_NOMEM: c_int = 7;
const SQLITE_ABORT: c_int = 4;
const SQLITE_TOOBIG: c_int = 18;
const SQLITE_RANGE: c_int = 25;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;

const LIMIT_LENGTH: c_int = 0;
const LIMIT_SQL_LENGTH: c_int = 1;
const LIMIT_COLUMN: c_int = 2;
const LIMIT_EXPR_DEPTH: c_int = 3;
const LIMIT_COMPOUND_SELECT: c_int = 4;
const LIMIT_VDBE_OP: c_int = 5;
const LIMIT_FUNCTION_ARG: c_int = 6;
const LIMIT_ATTACHED: c_int = 7;
const LIMIT_LIKE_PATTERN_LENGTH: c_int = 8;
const LIMIT_VARIABLE_NUMBER: c_int = 9;
const LIMIT_TRIGGER_DEPTH: c_int = 10;
const LIMIT_WORKER_THREADS: c_int = 11;

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

unsafe fn open_mem() -> *mut sqlite3 {
    let mut db = ptr::null_mut();
    assert_eq!(sqlite3_open(c":memory:".as_ptr(), &mut db), SQLITE_OK);
    db
}

unsafe fn errmsg(db: *mut sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db))
        .to_string_lossy()
        .into_owned()
}

unsafe fn exec(db: *mut sqlite3, sql: &str) -> c_int {
    sqlite3_exec(db, cs(sql).as_ptr(), None, ptr::null_mut(), ptr::null_mut())
}

unsafe fn exec_ok(db: *mut sqlite3, sql: &str) {
    assert_eq!(exec(db, sql), SQLITE_OK, "{sql}: {}", errmsg(db));
}

/// Prepares and runs `sql` to completion; returns the first failing code.
unsafe fn run(db: *mut sqlite3, sql: &str) -> c_int {
    let mut stmt = ptr::null_mut();
    let rc = sqlite3_prepare_v2(db, cs(sql).as_ptr(), -1, &mut stmt, ptr::null_mut());
    if rc != SQLITE_OK {
        return rc;
    }
    let mut rc;
    loop {
        rc = sqlite3_step(stmt);
        if rc != SQLITE_ROW {
            break;
        }
    }
    sqlite3_finalize(stmt);
    if rc == SQLITE_DONE {
        SQLITE_OK
    } else {
        rc
    }
}

/// Process-global state (collation_needed, extension flags) is per handle,
/// but keep the tests that spawn work on shared files serialized anyway.
static GLOBALS: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    GLOBALS.lock().unwrap_or_else(|e| e.into_inner())
}

// ===== sqlite3_limit =====

#[test]
fn limit_defaults_and_hard_maxima() {
    unsafe {
        let db = open_mem();
        let defaults = [
            (LIMIT_LENGTH, 1_000_000_000),
            (LIMIT_SQL_LENGTH, 1_000_000_000),
            (LIMIT_COLUMN, 2000),
            (LIMIT_EXPR_DEPTH, 1000),
            (LIMIT_COMPOUND_SELECT, 500),
            (LIMIT_VDBE_OP, 250_000_000),
            (LIMIT_FUNCTION_ARG, 1000),
            (LIMIT_ATTACHED, 10),
            (LIMIT_LIKE_PATTERN_LENGTH, 50_000),
            (LIMIT_VARIABLE_NUMBER, 250_000),
            (LIMIT_TRIGGER_DEPTH, 1000),
            (LIMIT_WORKER_THREADS, 0),
        ];
        for (id, want) in defaults {
            assert_eq!(sqlite3_limit(db, id, -1), want, "default of limit {id}");
        }
        // Raising above the hard maximum clamps to it.
        assert_eq!(sqlite3_limit(db, LIMIT_COLUMN, 1_000_000), 2000);
        assert_eq!(sqlite3_limit(db, LIMIT_COLUMN, -1), 2000);
        // Lowering returns the old value; a negative value only reads.
        assert_eq!(sqlite3_limit(db, LIMIT_COLUMN, 50), 2000);
        assert_eq!(sqlite3_limit(db, LIMIT_COLUMN, -1), 50);
        assert_eq!(sqlite3_limit(db, LIMIT_COLUMN, -5), 50);
        assert_eq!(sqlite3_limit(db, LIMIT_WORKER_THREADS, 100), 0);
        assert_eq!(sqlite3_limit(db, LIMIT_WORKER_THREADS, -1), 8);
        // Unknown ids read as -1; limits are per connection.
        assert_eq!(sqlite3_limit(db, 9999, 1), -1);
        assert_eq!(sqlite3_limit(db, -1, 1), -1);
        let other = open_mem();
        assert_eq!(sqlite3_limit(other, LIMIT_COLUMN, -1), 2000);
        sqlite3_close(other);
        sqlite3_close(db);
    }
}

#[test]
fn limit_sql_length() {
    unsafe {
        let db = open_mem();
        // Loading the schema is subject to the limit, so do it first.
        assert_eq!(run(db, "SELECT 1"), SQLITE_OK);
        sqlite3_limit(db, LIMIT_SQL_LENGTH, 40);
        let short = cs("SELECT 1");
        let long = cs("SELECT 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16");
        let mut stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(db, short.as_ptr(), -1, &mut stmt, ptr::null_mut()),
            SQLITE_OK
        );
        sqlite3_finalize(stmt);
        // NUL-terminated and counted forms both exceed the limit.
        assert_eq!(
            sqlite3_prepare_v2(db, long.as_ptr(), -1, &mut stmt, ptr::null_mut()),
            SQLITE_TOOBIG
        );
        assert!(stmt.is_null());
        assert_eq!(
            sqlite3_prepare_v2(db, long.as_ptr(), 60, &mut stmt, ptr::null_mut()),
            SQLITE_TOOBIG
        );
        assert!(stmt.is_null());
        assert_eq!(sqlite3_errcode(db), SQLITE_TOOBIG);
        sqlite3_close(db);
    }
}

#[test]
fn limit_length_on_bind() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static FREED: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn freed(p: *mut c_void) {
        FREED.fetch_add(1, Ordering::SeqCst);
        libc::free(p);
    }
    unsafe {
        let db = open_mem();
        // SQLite does not accept a length limit below 30.
        sqlite3_limit(db, LIMIT_LENGTH, 5);
        assert_eq!(sqlite3_limit(db, LIMIT_LENGTH, 30), 30);
        let mut stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(db, c"SELECT ?".as_ptr(), -1, &mut stmt, ptr::null_mut()),
            SQLITE_OK
        );
        let ok = c"0123456789012345678901234567890";
        assert_eq!(sqlite3_bind_text(stmt, 1, ok.as_ptr(), 30, None), SQLITE_OK);
        assert_eq!(
            sqlite3_bind_text(stmt, 1, ok.as_ptr(), 31, None),
            SQLITE_TOOBIG
        );
        assert_eq!(
            sqlite3_bind_blob(stmt, 1, ok.as_ptr().cast(), 31, None),
            SQLITE_TOOBIG
        );
        // The destructor still runs when the bind fails.
        let heap = libc::strdup(ok.as_ptr());
        assert_eq!(
            sqlite3_bind_text(stmt, 1, heap, 31, Some(freed)),
            SQLITE_TOOBIG
        );
        assert_eq!(FREED.load(Ordering::SeqCst), 1);
        sqlite3_finalize(stmt);
        sqlite3_close(db);
    }
}

#[test]
fn limit_length_on_function_result() {
    unsafe extern "C" fn long_text(ctx: *mut c_void, _n: c_int, _a: *mut *mut c_void) {
        extern "C" {
            fn sqlite3_result_text(ctx: *mut c_void, t: *const c_char, n: c_int, d: isize);
        }
        sqlite3_result_text(ctx, c"0123456789012345678901234567890".as_ptr(), 31, -1);
    }
    unsafe {
        let db = open_mem();
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                c"longtext".as_ptr(),
                0,
                1,
                ptr::null_mut(),
                Some(long_text),
                None,
                None,
                None
            ),
            SQLITE_OK
        );
        assert_eq!(run(db, "SELECT longtext()"), SQLITE_OK);
        sqlite3_limit(db, LIMIT_LENGTH, 30);
        assert_eq!(run(db, "SELECT longtext()"), SQLITE_TOOBIG);
        sqlite3_close(db);
    }
}

#[test]
fn limit_column() {
    unsafe {
        let db = open_mem();
        sqlite3_limit(db, LIMIT_COLUMN, 5);
        assert_eq!(run(db, "SELECT 1, 2, 3, 4, 5"), SQLITE_OK);
        assert_ne!(run(db, "SELECT 1, 2, 3, 4, 5, 6"), SQLITE_OK);
        assert!(errmsg(db).contains("too many columns"), "{}", errmsg(db));
        assert_eq!(run(db, "CREATE TABLE t5(a, b, c, d, e)"), SQLITE_OK);
        assert_ne!(run(db, "CREATE TABLE t6(a, b, c, d, e, f)"), SQLITE_OK);
        assert!(
            errmsg(db).contains("too many columns on t6"),
            "{}",
            errmsg(db)
        );
        sqlite3_close(db);
    }
}

#[test]
fn limit_variable_number() {
    unsafe {
        let db = open_mem();
        sqlite3_limit(db, LIMIT_VARIABLE_NUMBER, 3);
        assert_eq!(run(db, "SELECT ?, ?, ? WHERE 0"), SQLITE_OK);
        let mut stmt = ptr::null_mut();
        assert_ne!(
            sqlite3_prepare_v2(
                db,
                c"SELECT ?, ?, ?, ?".as_ptr(),
                -1,
                &mut stmt,
                ptr::null_mut()
            ),
            SQLITE_OK
        );
        assert_eq!(errmsg(db), "too many SQL variables");
        assert_ne!(
            sqlite3_prepare_v2(db, c"SELECT ?4".as_ptr(), -1, &mut stmt, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(errmsg(db), "variable number must be between ?1 and ?3");
        sqlite3_close(db);
    }
}

#[test]
fn limit_attached() {
    unsafe {
        #[cfg(not(feature = "sqlite3"))]
        turso_enable_experimental();
        let db = open_mem();
        sqlite3_limit(db, LIMIT_ATTACHED, 1);
        assert_eq!(run(db, "ATTACH ':memory:' AS a1"), SQLITE_OK);
        assert_ne!(run(db, "ATTACH ':memory:' AS a2"), SQLITE_OK);
        assert_eq!(errmsg(db), "too many attached databases - max 1");
        assert_eq!(run(db, "DETACH a1"), SQLITE_OK);
        assert_eq!(run(db, "ATTACH ':memory:' AS a2"), SQLITE_OK);
        sqlite3_close(db);
    }
}

#[test]
fn limit_function_arg() {
    unsafe {
        let db = open_mem();
        sqlite3_limit(db, LIMIT_FUNCTION_ARG, 2);
        assert_eq!(run(db, "SELECT coalesce(1, 2)"), SQLITE_OK);
        assert_ne!(run(db, "SELECT coalesce(1, 2, 3)"), SQLITE_OK);
        assert_eq!(errmsg(db), "too many arguments on function coalesce");
        sqlite3_close(db);
    }
}

#[test]
fn limit_expr_depth() {
    unsafe {
        let db = open_mem();
        sqlite3_limit(db, LIMIT_EXPR_DEPTH, 5);
        assert_eq!(run(db, "SELECT 1+(1+(1+(1+1)))"), SQLITE_OK);
        assert_ne!(run(db, "SELECT 1+(1+(1+(1+(1+(1+1)))))"), SQLITE_OK);
        assert_eq!(errmsg(db), "Expression tree is too large (maximum depth 5)");
        // A left-leaning chain is as deep as a nested one.
        assert_ne!(run(db, "SELECT 1+1+1+1+1+1+1+1"), SQLITE_OK);
        sqlite3_close(db);
    }
}

#[test]
fn limit_compound_select() {
    unsafe {
        let db = open_mem();
        sqlite3_limit(db, LIMIT_COMPOUND_SELECT, 3);
        assert_eq!(run(db, "SELECT 1 UNION SELECT 2 UNION SELECT 3"), SQLITE_OK);
        assert_ne!(
            run(db, "SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4"),
            SQLITE_OK
        );
        assert_eq!(errmsg(db), "too many terms in compound SELECT");
        sqlite3_close(db);
    }
}

#[test]
fn limit_like_pattern_length() {
    unsafe {
        let db = open_mem();
        sqlite3_limit(db, LIMIT_LIKE_PATTERN_LENGTH, 5);
        assert_eq!(run(db, "SELECT 'abc' LIKE 'a%'"), SQLITE_OK);
        assert_ne!(run(db, "SELECT 'abc' LIKE 'aaaaaaaaaa%'"), SQLITE_OK);
        assert_eq!(errmsg(db), "LIKE or GLOB pattern too complex");
        assert_ne!(run(db, "SELECT 'abc' GLOB 'aaaaaaaaaa*'"), SQLITE_OK);
        assert_eq!(errmsg(db), "LIKE or GLOB pattern too complex");
        sqlite3_close(db);
    }
}

#[test]
fn limit_trigger_depth() {
    unsafe {
        let db = open_mem();
        exec_ok(
            db,
            "CREATE TABLE a(x); CREATE TABLE b(x); CREATE TABLE c(x); CREATE TABLE d(x);
             CREATE TRIGGER ta AFTER INSERT ON a BEGIN INSERT INTO b VALUES (new.x); END;
             CREATE TRIGGER tb AFTER INSERT ON b BEGIN INSERT INTO c VALUES (new.x); END;
             CREATE TRIGGER tc AFTER INSERT ON c BEGIN INSERT INTO d VALUES (new.x); END;",
        );
        assert_eq!(run(db, "INSERT INTO a VALUES (1)"), SQLITE_OK);
        sqlite3_limit(db, LIMIT_TRIGGER_DEPTH, 2);
        assert_ne!(run(db, "INSERT INTO a VALUES (2)"), SQLITE_OK);
        assert_eq!(errmsg(db), "triggers nested too deep");
        sqlite3_close(db);
    }
}

// ===== trace and profile =====

#[derive(Default)]
struct Events {
    stmts: Vec<String>,
    expanded: Vec<String>,
    profiles: Vec<(String, i64)>,
    rows: usize,
    closes: usize,
    legacy: Vec<String>,
    legacy_profile: Vec<(String, u64)>,
}

unsafe extern "C" fn on_trace(
    kind: c_uint,
    ctx: *mut c_void,
    p: *mut c_void,
    x: *mut c_void,
) -> c_int {
    let ev = &mut *(ctx as *mut Events);
    match kind {
        1 => {
            let text = CStr::from_ptr(x as *const c_char)
                .to_string_lossy()
                .into_owned();
            if !text.starts_with("--") {
                let e = sqlite3_expanded_sql(p as *mut sqlite3_stmt);
                ev.expanded
                    .push(CStr::from_ptr(e).to_string_lossy().into_owned());
                sqlite3_free(e.cast());
            }
            ev.stmts.push(text);
        }
        2 => {
            let sql = CStr::from_ptr(sqlite3_sql(p as *mut sqlite3_stmt))
                .to_string_lossy()
                .into_owned();
            ev.profiles.push((sql, *(x as *const i64)));
        }
        4 => {
            assert!(x.is_null());
            ev.rows += 1;
        }
        8 => {
            assert!(x.is_null());
            ev.closes += 1;
        }
        _ => panic!("unexpected trace kind {kind}"),
    }
    0
}

#[test]
fn trace_v2_stmt_profile_row_close() {
    unsafe {
        let db = open_mem();
        exec_ok(db, "CREATE TABLE t(x)");
        let mut ev = Events::default();
        assert_eq!(
            sqlite3_trace_v2(db, 0x0f, Some(on_trace), &mut ev as *mut _ as *mut c_void),
            SQLITE_OK
        );
        let mut stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(
                db,
                c"INSERT INTO t VALUES (?)".as_ptr(),
                -1,
                &mut stmt,
                ptr::null_mut()
            ),
            SQLITE_OK
        );
        sqlite3_bind_int(stmt, 1, 7);
        assert_eq!(sqlite3_step(stmt), SQLITE_DONE);
        // A reset starts a second run.
        sqlite3_reset(stmt);
        sqlite3_bind_int(stmt, 1, 8);
        assert_eq!(sqlite3_step(stmt), SQLITE_DONE);
        sqlite3_finalize(stmt);
        assert_eq!(run(db, "SELECT x FROM t ORDER BY x"), SQLITE_OK);
        assert_eq!(
            ev.stmts,
            [
                "INSERT INTO t VALUES (?)",
                "INSERT INTO t VALUES (?)",
                "SELECT x FROM t ORDER BY x"
            ]
        );
        assert_eq!(
            ev.expanded,
            [
                "INSERT INTO t VALUES (7)",
                "INSERT INTO t VALUES (8)",
                "SELECT x FROM t ORDER BY x"
            ]
        );
        assert_eq!(ev.rows, 2);
        assert_eq!(ev.profiles.len(), 3);
        assert!(ev.profiles.iter().all(|(_, ns)| *ns >= 0));
        assert_eq!(ev.profiles[0].0, "INSERT INTO t VALUES (?)");
        assert_eq!(ev.closes, 0);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
        assert_eq!(ev.closes, 1);
    }
}

#[test]
fn trace_v2_mask_selects_events() {
    unsafe {
        let db = open_mem();
        let mut ev = Events::default();
        sqlite3_trace_v2(db, 0x04, Some(on_trace), &mut ev as *mut _ as *mut c_void);
        assert_eq!(run(db, "SELECT 1 UNION ALL SELECT 2"), SQLITE_OK);
        assert_eq!((ev.stmts.len(), ev.profiles.len(), ev.rows), (0, 0, 2));
        // A zero mask or NULL callback turns tracing off.
        sqlite3_trace_v2(db, 0, Some(on_trace), &mut ev as *mut _ as *mut c_void);
        assert_eq!(run(db, "SELECT 1"), SQLITE_OK);
        assert_eq!(ev.rows, 2);
        sqlite3_trace_v2(db, 0x0f, None, ptr::null_mut());
        assert_eq!(run(db, "SELECT 1"), SQLITE_OK);
        assert_eq!(ev.stmts.len(), 0);
        sqlite3_close(db);
    }
}

#[test]
fn trace_v2_trigger_statements_are_comments() {
    unsafe {
        let db = open_mem();
        exec_ok(
            db,
            "CREATE TABLE t(x); CREATE TABLE log(x);
             CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES (new.x); END;",
        );
        let mut ev = Events::default();
        sqlite3_trace_v2(db, 1, Some(on_trace), &mut ev as *mut _ as *mut c_void);
        assert_eq!(run(db, "INSERT INTO t VALUES (1)"), SQLITE_OK);
        assert_eq!(
            ev.stmts,
            [
                "INSERT INTO t VALUES (1)",
                "-- TRIGGER tr",
                "-- INSERT INTO log VALUES (new.x)"
            ]
        );
        sqlite3_close(db);
    }
}

#[test]
fn trace_v2_nested_statement_is_comment() {
    unsafe extern "C" fn nested(ctx: *mut c_void, _n: c_int, _a: *mut *mut c_void) {
        let db = *(sqlite3_user_data_slot());
        let _ = ctx;
        assert_eq!(run(db, "SELECT 5"), SQLITE_OK);
        sqlite3_result_int(ctx, 1);
    }
    unsafe {
        let db = open_mem();
        *sqlite3_user_data_slot() = db;
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                c"nested".as_ptr(),
                0,
                1,
                ptr::null_mut(),
                Some(nested),
                None,
                None,
                None
            ),
            SQLITE_OK
        );
        let mut ev = Events::default();
        sqlite3_trace_v2(db, 1, Some(on_trace), &mut ev as *mut _ as *mut c_void);
        assert_eq!(run(db, "SELECT nested()"), SQLITE_OK);
        assert_eq!(ev.stmts, ["SELECT nested()", "-- SELECT 5"]);
        sqlite3_close(db);
    }
}

fn sqlite3_user_data_slot() -> *mut *mut sqlite3 {
    static mut SLOT: *mut sqlite3 = ptr::null_mut();
    ptr::addr_of_mut!(SLOT)
}

#[test]
fn trace_v2_reset_reports_profile() {
    unsafe {
        let db = open_mem();
        let mut ev = Events::default();
        sqlite3_trace_v2(db, 0x03, Some(on_trace), &mut ev as *mut _ as *mut c_void);
        let mut stmt = ptr::null_mut();
        sqlite3_prepare_v2(
            db,
            c"SELECT 1 UNION ALL SELECT 2".as_ptr(),
            -1,
            &mut stmt,
            ptr::null_mut(),
        );
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
        assert_eq!(ev.profiles.len(), 0);
        sqlite3_reset(stmt);
        assert_eq!((ev.stmts.len(), ev.profiles.len()), (1, 1));
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
        sqlite3_finalize(stmt);
        assert_eq!((ev.stmts.len(), ev.profiles.len()), (2, 2));
        sqlite3_close(db);
    }
}

#[test]
fn legacy_trace_and_profile() {
    unsafe extern "C" fn legacy(ctx: *mut c_void, sql: *const c_char) {
        let ev = &mut *(ctx as *mut Events);
        ev.legacy
            .push(CStr::from_ptr(sql).to_string_lossy().into_owned());
    }
    unsafe extern "C" fn prof(ctx: *mut c_void, sql: *const c_char, ns: u64) {
        let ev = &mut *(ctx as *mut Events);
        ev.legacy_profile
            .push((CStr::from_ptr(sql).to_string_lossy().into_owned(), ns));
    }
    unsafe {
        let db = open_mem();
        let mut ev = Events::default();
        let ctx = &mut ev as *mut _ as *mut c_void;
        assert!(sqlite3_trace(db, Some(legacy), ctx).is_null());
        let mut stmt = ptr::null_mut();
        sqlite3_prepare_v2(db, c"SELECT ?".as_ptr(), -1, &mut stmt, ptr::null_mut());
        sqlite3_bind_int(stmt, 1, 42);
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
        sqlite3_finalize(stmt);
        // The legacy callback gets the expanded text.
        assert_eq!(ev.legacy, ["SELECT 42"]);
        // The old context comes back.
        assert_eq!(sqlite3_trace(db, None, ptr::null_mut()), ctx);
        assert_eq!(run(db, "SELECT 1"), SQLITE_OK);
        assert_eq!(ev.legacy.len(), 1);
        // Profile reports the unexpanded text and a duration.
        assert!(sqlite3_profile(db, Some(prof), ctx).is_null());
        let mut stmt = ptr::null_mut();
        sqlite3_prepare_v2(db, c"SELECT ?".as_ptr(), -1, &mut stmt, ptr::null_mut());
        sqlite3_bind_int(stmt, 1, 42);
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
        assert_eq!(ev.legacy_profile.len(), 0);
        assert_eq!(sqlite3_step(stmt), SQLITE_DONE);
        sqlite3_finalize(stmt);
        assert_eq!(ev.legacy_profile.len(), 1);
        assert_eq!(ev.legacy_profile[0].0, "SELECT ?");
        assert_eq!(sqlite3_profile(db, None, ptr::null_mut()), ctx);
        sqlite3_close(db);
    }
}

#[test]
fn statement_text_runs_to_the_tail() {
    unsafe {
        let db = open_mem();
        for sql in [
            "BEGIN ",
            "SELECT 1 ;  ",
            "SELECT 2; SELECT 3",
            "SELECT 4 -- x\n",
            "SELECT 5 /* c */  ",
        ] {
            let mut stmt = ptr::null_mut();
            let mut tail = ptr::null();
            let c = cs(sql);
            assert_eq!(
                sqlite3_prepare_v2(db, c.as_ptr(), -1, &mut stmt, &mut tail),
                SQLITE_OK
            );
            let text = CStr::from_ptr(sqlite3_sql(stmt)).to_str().unwrap();
            let consumed = tail.offset_from(c.as_ptr()) as usize;
            assert_eq!(text, &sql[..consumed], "{sql:?}");
            let e = sqlite3_expanded_sql(stmt);
            assert_eq!(
                CStr::from_ptr(e).to_str().unwrap(),
                text,
                "expanded {sql:?}"
            );
            sqlite3_free(e.cast());
            if !sql.contains(';') {
                assert_eq!(consumed, sql.len(), "{sql:?} runs to the end of the input");
            }
            sqlite3_finalize(stmt);
        }
        sqlite3_close(db);
    }
}

// ===== collation_needed =====

#[test]
fn collation_needed_registers_and_retries() {
    static CALLS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    unsafe extern "C" fn reverse(
        _ctx: *mut c_void,
        n1: c_int,
        a: *const c_void,
        n2: c_int,
        b: *const c_void,
    ) -> c_int {
        let a = std::slice::from_raw_parts(a.cast::<u8>(), n1 as usize);
        let b = std::slice::from_raw_parts(b.cast::<u8>(), n2 as usize);
        b.cmp(a) as c_int
    }
    unsafe extern "C" fn needed(
        _ctx: *mut c_void,
        db: *mut sqlite3,
        enc: c_int,
        name: *const c_char,
    ) {
        assert_eq!(enc, 1);
        let name = CStr::from_ptr(name).to_str().unwrap().to_owned();
        CALLS.lock().unwrap().push(name.clone());
        if name == "revcoll" {
            let c = cs(&name);
            assert_eq!(
                sqlite3_create_collation_v2(
                    db,
                    c.as_ptr(),
                    1,
                    ptr::null_mut(),
                    Some(reverse),
                    None
                ),
                SQLITE_OK
            );
        }
    }
    unsafe {
        let _g = lock();
        let db = open_mem();
        exec_ok(
            db,
            "CREATE TABLE t(x); INSERT INTO t VALUES ('a'),('b'),('c')",
        );
        assert_eq!(
            run(db, "SELECT x FROM t ORDER BY x COLLATE revcoll"),
            SQLITE_ERROR
        );
        assert!(
            errmsg(db).contains("no such collation sequence: revcoll"),
            "{}",
            errmsg(db)
        );
        CALLS.lock().unwrap().clear();
        assert_eq!(
            sqlite3_collation_needed(db, ptr::null_mut(), Some(needed)),
            SQLITE_OK
        );
        let mut stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(
                db,
                c"SELECT x FROM t ORDER BY x COLLATE revcoll".as_ptr(),
                -1,
                &mut stmt,
                ptr::null_mut()
            ),
            SQLITE_OK
        );
        let mut got = String::new();
        while sqlite3_step(stmt) == SQLITE_ROW {
            extern "C" {
                fn sqlite3_column_text(s: *mut sqlite3_stmt, i: c_int) -> *const c_char;
            }
            got += CStr::from_ptr(sqlite3_column_text(stmt, 0))
                .to_str()
                .unwrap();
        }
        sqlite3_finalize(stmt);
        assert_eq!(got, "cba");
        assert_eq!(*CALLS.lock().unwrap(), ["revcoll"]);
        // An unknown name the callback does not provide still fails.
        assert_eq!(
            run(db, "SELECT x FROM t ORDER BY x COLLATE nosuch"),
            SQLITE_ERROR
        );
        assert!(errmsg(db).contains("no such collation sequence: nosuch"));
        // Once registered, the callback is not asked again.
        CALLS.lock().unwrap().clear();
        assert_eq!(
            run(db, "SELECT x FROM t ORDER BY x COLLATE revcoll"),
            SQLITE_OK
        );
        assert!(CALLS.lock().unwrap().is_empty());
        // NULL removes the callback.
        assert_eq!(
            sqlite3_collation_needed(db, ptr::null_mut(), None),
            SQLITE_OK
        );
        CALLS.lock().unwrap().clear();
        assert_eq!(
            run(db, "SELECT x FROM t ORDER BY x COLLATE other"),
            SQLITE_ERROR
        );
        assert!(CALLS.lock().unwrap().is_empty());
        sqlite3_close(db);
    }
}

#[test]
fn collation_needed16_gets_utf16_name() {
    static SEEN: Mutex<Option<String>> = Mutex::new(None);
    unsafe extern "C" fn needed(
        _ctx: *mut c_void,
        _db: *mut sqlite3,
        _enc: c_int,
        name: *const c_void,
    ) {
        let mut p = name.cast::<u16>();
        let mut units = Vec::new();
        while *p != 0 {
            units.push(*p);
            p = p.add(1);
        }
        *SEEN.lock().unwrap() = Some(String::from_utf16(&units).unwrap());
    }
    unsafe {
        let _g = lock();
        let db = open_mem();
        exec_ok(db, "CREATE TABLE t(x)");
        assert_eq!(
            sqlite3_collation_needed16(db, ptr::null_mut(), Some(needed)),
            SQLITE_OK
        );
        assert_eq!(
            run(db, "SELECT x FROM t ORDER BY x COLLATE wide"),
            SQLITE_ERROR
        );
        assert_eq!(SEEN.lock().unwrap().as_deref(), Some("wide"));
        sqlite3_close(db);
    }
}

// ===== blob =====

#[test]
fn blob_reopen_moves_the_handle() {
    unsafe {
        let db = open_mem();
        exec_ok(db, "CREATE TABLE t(b BLOB); INSERT INTO t VALUES (x'0102030405'), (x'0a0b0c0d0e'), (x'ff')");
        let mut blob = ptr::null_mut();
        assert_eq!(
            sqlite3_blob_open(
                db,
                c"main".as_ptr(),
                c"t".as_ptr(),
                c"b".as_ptr(),
                1,
                0,
                &mut blob
            ),
            SQLITE_OK
        );
        let mut buf = [0u8; 5];
        assert_eq!(
            sqlite3_blob_read(blob, buf.as_mut_ptr().cast(), 5, 0),
            SQLITE_OK
        );
        assert_eq!(buf, [1, 2, 3, 4, 5]);
        assert_eq!(sqlite3_blob_reopen(blob, 2), SQLITE_OK);
        assert_eq!(sqlite3_blob_bytes(blob), 5);
        assert_eq!(
            sqlite3_blob_read(blob, buf.as_mut_ptr().cast(), 5, 0),
            SQLITE_OK
        );
        assert_eq!(buf, [10, 11, 12, 13, 14]);
        assert_eq!(sqlite3_blob_reopen(blob, 3), SQLITE_OK);
        assert_eq!(sqlite3_blob_bytes(blob), 1);
        assert_eq!(
            sqlite3_blob_read(blob, buf.as_mut_ptr().cast(), 2, 0),
            SQLITE_ERROR
        );
        // A missing row fails and leaves the handle expired.
        assert_eq!(sqlite3_blob_reopen(blob, 99), SQLITE_ERROR);
        assert_eq!(
            sqlite3_blob_read(blob, buf.as_mut_ptr().cast(), 1, 0),
            SQLITE_ABORT
        );
        assert_eq!(sqlite3_blob_close(blob), SQLITE_OK);
        assert_eq!(sqlite3_blob_reopen(ptr::null_mut(), 1), 21);
        sqlite3_close(db);
    }
}

#[test]
fn blob_errors_are_recorded_on_the_connection() {
    unsafe {
        let db = open_mem();
        exec_ok(
            db,
            "CREATE TABLE t(b BLOB); INSERT INTO t VALUES (x'0102030405')",
        );
        let mut blob = ptr::null_mut();
        assert_eq!(
            sqlite3_blob_open(
                db,
                c"main".as_ptr(),
                c"t".as_ptr(),
                c"b".as_ptr(),
                1,
                0,
                &mut blob
            ),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_blob_write(blob, [9u8].as_ptr().cast(), 1, 0),
            8 /* READONLY */
        );
        assert_eq!(sqlite3_errcode(db), 8);
        assert!(errmsg(db).contains("readonly"), "{}", errmsg(db));
        let mut buf = [0u8; 9];
        assert_eq!(
            sqlite3_blob_read(blob, buf.as_mut_ptr().cast(), 9, 0),
            SQLITE_ERROR
        );
        assert_eq!(sqlite3_errcode(db), SQLITE_ERROR);
        sqlite3_blob_close(blob);
        let mut blob = ptr::null_mut();
        assert_ne!(
            sqlite3_blob_open(
                db,
                c"main".as_ptr(),
                c"t".as_ptr(),
                c"b".as_ptr(),
                7,
                0,
                &mut blob
            ),
            SQLITE_OK
        );
        assert!(errmsg(db).contains("no such rowid"), "{}", errmsg(db));
        sqlite3_close(db);
    }
}

// ===== UDF error codes =====

#[test]
fn udf_error_codes_reach_step() {
    unsafe extern "C" fn toobig(ctx: *mut c_void, _n: c_int, _a: *mut *mut c_void) {
        sqlite3_result_error_toobig(ctx);
    }
    unsafe extern "C" fn nomem(ctx: *mut c_void, _n: c_int, _a: *mut *mut c_void) {
        sqlite3_result_error_nomem(ctx);
    }
    unsafe extern "C" fn code(ctx: *mut c_void, _n: c_int, _a: *mut *mut c_void) {
        sqlite3_result_error_code(ctx, SQLITE_RANGE);
    }
    unsafe extern "C" fn plain(ctx: *mut c_void, _n: c_int, _a: *mut *mut c_void) {
        sqlite3_result_error(ctx, c"boom".as_ptr(), -1);
    }
    unsafe extern "C" fn ok(ctx: *mut c_void, _n: c_int, _a: *mut *mut c_void) {
        sqlite3_result_int(ctx, 1);
    }
    unsafe {
        let db = open_mem();
        for (name, f) in [
            ("toobig", toobig as UdfFn),
            ("nomem", nomem),
            ("code", code),
            ("plain", plain),
            ("ok", ok),
        ] {
            let c = cs(name);
            assert_eq!(
                sqlite3_create_function_v2(
                    db,
                    c.as_ptr(),
                    0,
                    1,
                    ptr::null_mut(),
                    Some(f),
                    None,
                    None,
                    None
                ),
                SQLITE_OK
            );
        }
        assert_eq!(run(db, "SELECT toobig()"), SQLITE_TOOBIG);
        assert_eq!(sqlite3_errcode(db), SQLITE_TOOBIG);
        assert!(
            errmsg(db).contains("string or blob too big"),
            "{}",
            errmsg(db)
        );
        assert_eq!(run(db, "SELECT nomem()"), SQLITE_NOMEM);
        assert_eq!(sqlite3_errcode(db), SQLITE_NOMEM);
        assert_eq!(run(db, "SELECT code()"), SQLITE_RANGE);
        assert_eq!(sqlite3_errcode(db), SQLITE_RANGE);
        assert_eq!(run(db, "SELECT plain()"), SQLITE_ERROR);
        assert!(errmsg(db).contains("boom"), "{}", errmsg(db));
        // A stale code never leaks into an unrelated error.
        assert_eq!(run(db, "SELECT ok()"), SQLITE_OK);
        assert_eq!(run(db, "SELECT * FROM nosuchtable"), SQLITE_ERROR);
        sqlite3_close(db);
    }
}

// ===== sqlite3_open16 =====

fn utf16z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[test]
fn open16_opens_a_file_and_memory() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("caf\u{e9}.db");
        let name = utf16z(path.to_str().unwrap());
        let mut db = ptr::null_mut();
        assert_eq!(sqlite3_open16(name.as_ptr().cast(), &mut db), SQLITE_OK);
        exec_ok(db, "CREATE TABLE t(x)");
        sqlite3_close(db);
        assert!(path.exists());
        let mem = utf16z(":memory:");
        assert_eq!(sqlite3_open16(mem.as_ptr().cast(), &mut db), SQLITE_OK);
        exec_ok(db, "CREATE TABLE t(x)");
        sqlite3_close(db);
        // A NULL name opens a private temporary database, as in sqlite3_open.
        assert_eq!(sqlite3_open16(ptr::null(), &mut db), SQLITE_OK);
        exec_ok(db, "CREATE TABLE t(x)");
        sqlite3_close(db);
    }
}

#[test]
fn open_uri_child() {
    // Runs in a child process: SQLITE_CONFIG_URI must precede initialization.
    if std::env::var_os("TURSO_MISC_URI_CHILD").is_none() {
        return;
    }
    unsafe {
        assert_eq!(sqlite3_config(17 /* URI */, 1 as c_int), SQLITE_OK);
        let dir = std::env::var("TURSO_MISC_URI_DIR").unwrap();
        let mut db = ptr::null_mut();
        let uri = format!("file:{dir}/uri%20one.db?mode=rwc");
        assert_eq!(sqlite3_open(cs(&uri).as_ptr(), &mut db), SQLITE_OK);
        exec_ok(db, "CREATE TABLE t(x)");
        sqlite3_close(db);
        // The UTF-16 entry point honours URIs too.
        let uri = utf16z(&format!("file:{dir}/uri%20two.db?mode=rwc"));
        assert_eq!(sqlite3_open16(uri.as_ptr().cast(), &mut db), SQLITE_OK);
        exec_ok(db, "CREATE TABLE t(x)");
        sqlite3_close(db);
        // file::memory: is a private memory database.
        assert_eq!(sqlite3_open(c"file::memory:".as_ptr(), &mut db), SQLITE_OK);
        exec_ok(db, "CREATE TABLE t(x)");
        sqlite3_close(db);
    }
}

#[test]
fn open_and_open16_honour_uris_when_configured() {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "misc::open_uri_child", "--test-threads=1"])
        .env("TURSO_MISC_URI_CHILD", "1")
        .env("TURSO_MISC_URI_DIR", dir.path())
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(dir.path().join("uri one.db").exists());
    assert!(dir.path().join("uri two.db").exists());
    assert!(!dir.path().join("file:").exists());
}

// ===== SQL function load_extension() =====

#[cfg(unix)]
#[test]
fn sql_load_extension_is_gated_like_sqlite() {
    let _g = lock();
    unsafe {
        let (lib, _) = super::config::build_extension();
        let lib = lib.to_str().unwrap().to_owned();
        let db = open_mem();
        let q = format!("SELECT load_extension('{lib}', 'sqlite3_hello_init')");
        // Disabled by default.
        assert_ne!(run(db, &q), SQLITE_OK);
        assert_eq!(errmsg(db), "not authorized");
        // SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION enables only the C API.
        let mut out: c_int = -1;
        assert_eq!(
            sqlite3_db_config(db, 1005, 1 as c_int, &mut out as *mut c_int),
            SQLITE_OK
        );
        assert_ne!(run(db, &q), SQLITE_OK);
        assert_eq!(errmsg(db), "not authorized");
        // sqlite3_enable_load_extension enables the SQL function too.
        assert_eq!(sqlite3_enable_load_extension(db, 1), SQLITE_OK);
        assert_eq!(exec(db, &q), SQLITE_OK, "{}", errmsg(db));
        assert_eq!(run(db, "SELECT hello_answer()"), SQLITE_OK);
        // One argument uses the default entry point.
        let db2 = open_mem();
        sqlite3_enable_load_extension(db2, 1);
        assert_eq!(
            exec(db2, &format!("SELECT load_extension('{lib}')")),
            SQLITE_OK,
            "{}",
            errmsg(db2)
        );
        assert_eq!(run(db2, "SELECT hello_echo('x')"), SQLITE_OK);
        // Errors carry the loader's message.
        assert_ne!(
            exec(db2, "SELECT load_extension('/nonexistent/ext.so')"),
            SQLITE_OK
        );
        assert!(
            errmsg(db2).contains("/nonexistent/ext.so"),
            "{}",
            errmsg(db2)
        );
        assert_ne!(
            exec(
                db2,
                &format!("SELECT load_extension('{lib}', 'sqlite3_failing_init')")
            ),
            SQLITE_OK
        );
        assert!(
            errmsg(db2).contains("deliberate failure"),
            "{}",
            errmsg(db2)
        );
        // Turning it off again closes the gate.
        assert_eq!(sqlite3_enable_load_extension(db2, 0), SQLITE_OK);
        assert_ne!(
            exec(db2, &format!("SELECT load_extension('{lib}')")),
            SQLITE_OK
        );
        assert_eq!(errmsg(db2), "not authorized");
        sqlite3_close(db2);
        sqlite3_close(db);
    }
}

// ===== sqlite3_value_* / sqlite3_column_* conversions =====

unsafe fn show_text(p: *const u8, n: c_int) -> String {
    if p.is_null() {
        "nil".into()
    } else {
        String::from_utf8_lossy(std::slice::from_raw_parts(p, n as usize)).into_owned()
    }
}

/// Describes every accessor on one value; the call order is part of the check.
unsafe extern "C" fn describe_value(ctx: *mut c_void, _n: c_int, argv: *mut *mut c_void) {
    let v = *argv;
    let ty = sqlite3_value_type(v);
    let (i, i64v, d) = (
        sqlite3_value_int(v),
        sqlite3_value_int64(v),
        sqlite3_value_double(v),
    );
    let bytes_before = sqlite3_value_bytes(v);
    let text_ptr = sqlite3_value_text(v) as *const u8;
    let bytes = sqlite3_value_bytes(v);
    let text = show_text(text_ptr, bytes);
    // The text is NUL terminated.
    let nul = if text_ptr.is_null() {
        0
    } else {
        *text_ptr.add(bytes as usize)
    };
    let blob_ptr = sqlite3_value_blob(v) as *const u8;
    let blob = show_text(blob_ptr, sqlite3_value_bytes(v));
    let w = sqlite3_value_text16(v);
    let wide = sqlite3_value_bytes16(v);
    let wide_nul = if w.is_null() {
        0
    } else {
        *w.add(wide as usize / 2)
    };
    let s = format!(
        "ty={ty} i={i} i64={i64v} d={d:?} bytes0={bytes_before} text={text:?} bytes={bytes} nul={nul} blob={blob:?} wide={wide} wide_nul={wide_nul}"
    );
    let c = cs(&s);
    sqlite3_result_text(ctx, c.as_ptr(), -1, -1);
}

unsafe fn first_text(db: *mut sqlite3, sql: &str) -> String {
    let mut stmt = ptr::null_mut();
    assert_eq!(
        sqlite3_prepare_v2(db, cs(sql).as_ptr(), -1, &mut stmt, ptr::null_mut()),
        SQLITE_OK,
        "{sql}: {}",
        errmsg(db)
    );
    assert_eq!(sqlite3_step(stmt), SQLITE_ROW, "{sql}");
    let p = sqlite3_column_text(stmt, 0);
    let out = show_text(p as *const u8, sqlite3_column_bytes(stmt, 0) as c_int);
    sqlite3_finalize(stmt);
    out
}

const VALUE_EXPECTED: &[(&str, &str)] = &[
    (
        r#"NULL"#,
        r#"ty=5 i=0 i64=0 d=0.0 bytes0=0 text="nil" bytes=0 nul=0 blob="nil" wide=0 wide_nul=0"#,
    ),
    (
        r#"42"#,
        r#"ty=1 i=42 i64=42 d=42.0 bytes0=2 text="42" bytes=2 nul=0 blob="42" wide=4 wide_nul=0"#,
    ),
    (
        r#"-7"#,
        r#"ty=1 i=-7 i64=-7 d=-7.0 bytes0=2 text="-7" bytes=2 nul=0 blob="-7" wide=4 wide_nul=0"#,
    ),
    (
        r#"9223372036854775807"#,
        r#"ty=1 i=-1 i64=9223372036854775807 d=9.223372036854776e18 bytes0=19 text="9223372036854775807" bytes=19 nul=0 blob="9223372036854775807" wide=38 wide_nul=0"#,
    ),
    (
        r#"1.5"#,
        r#"ty=2 i=1 i64=1 d=1.5 bytes0=3 text="1.5" bytes=3 nul=0 blob="1.5" wide=6 wide_nul=0"#,
    ),
    (
        r#"1e14"#,
        r#"ty=2 i=276447232 i64=100000000000000 d=100000000000000.0 bytes0=17 text="100000000000000.0" bytes=17 nul=0 blob="100000000000000.0" wide=34 wide_nul=0"#,
    ),
    (
        r#"0.1"#,
        r#"ty=2 i=0 i64=0 d=0.1 bytes0=3 text="0.1" bytes=3 nul=0 blob="0.1" wide=6 wide_nul=0"#,
    ),
    (
        r#"1e100"#,
        r#"ty=2 i=-1 i64=9223372036854775807 d=1e100 bytes0=8 text="1.0e+100" bytes=8 nul=0 blob="1.0e+100" wide=16 wide_nul=0"#,
    ),
    (
        r#"-0.0"#,
        r#"ty=2 i=0 i64=0 d=-0.0 bytes0=3 text="0.0" bytes=3 nul=0 blob="0.0" wide=6 wide_nul=0"#,
    ),
    (
        r#"1e30"#,
        r#"ty=2 i=-1 i64=9223372036854775807 d=1e30 bytes0=7 text="1.0e+30" bytes=7 nul=0 blob="1.0e+30" wide=14 wide_nul=0"#,
    ),
    (
        r#"'12abc'"#,
        r#"ty=3 i=12 i64=12 d=12.0 bytes0=5 text="12abc" bytes=5 nul=0 blob="12abc" wide=10 wide_nul=0"#,
    ),
    (
        r#"' 5'"#,
        r#"ty=3 i=5 i64=5 d=5.0 bytes0=2 text=" 5" bytes=2 nul=0 blob=" 5" wide=4 wide_nul=0"#,
    ),
    (
        r#"''"#,
        r#"ty=3 i=0 i64=0 d=0.0 bytes0=0 text="" bytes=0 nul=0 blob="nil" wide=0 wide_nul=0"#,
    ),
    (
        r#"'3.7e2'"#,
        r#"ty=3 i=3 i64=3 d=370.0 bytes0=5 text="3.7e2" bytes=5 nul=0 blob="3.7e2" wide=10 wide_nul=0"#,
    ),
    (
        r#"'0x10'"#,
        r#"ty=3 i=0 i64=0 d=0.0 bytes0=4 text="0x10" bytes=4 nul=0 blob="0x10" wide=8 wide_nul=0"#,
    ),
    (
        r#"'héllo'"#,
        r#"ty=3 i=0 i64=0 d=0.0 bytes0=6 text="héllo" bytes=6 nul=0 blob="héllo" wide=10 wide_nul=0"#,
    ),
    (
        r#"x'31323334'"#,
        r#"ty=4 i=1234 i64=1234 d=1234.0 bytes0=4 text="1234" bytes=4 nul=0 blob="1234" wide=8 wide_nul=0"#,
    ),
    (
        r#"x''"#,
        r#"ty=4 i=0 i64=0 d=0.0 bytes0=0 text="" bytes=0 nul=0 blob="nil" wide=0 wide_nul=0"#,
    ),
    (
        r#"x'ff'"#,
        r#"ty=4 i=0 i64=0 d=0.0 bytes0=1 text="�" bytes=1 nul=0 blob="�" wide=2 wide_nul=0"#,
    ),
    (
        r#"x'2037'"#,
        r#"ty=4 i=7 i64=7 d=7.0 bytes0=2 text=" 7" bytes=2 nul=0 blob=" 7" wide=4 wide_nul=0"#,
    ),
];

const COLUMN_EXPECTED: &[(&str, &str)] = &[
    (
        r#"NULL"#,
        r#"ty=5 i=0 i64=0 d=0.0 bytes0=0 text="nil" bytes=0 nul=0 blob="nil""#,
    ),
    (
        r#"42"#,
        r#"ty=1 i=42 i64=42 d=42.0 bytes0=2 text="42" bytes=2 nul=0 blob="42""#,
    ),
    (
        r#"-7"#,
        r#"ty=1 i=-7 i64=-7 d=-7.0 bytes0=2 text="-7" bytes=2 nul=0 blob="-7""#,
    ),
    (
        r#"9223372036854775807"#,
        r#"ty=1 i=-1 i64=9223372036854775807 d=9.223372036854776e18 bytes0=19 text="9223372036854775807" bytes=19 nul=0 blob="9223372036854775807""#,
    ),
    (
        r#"1.5"#,
        r#"ty=2 i=1 i64=1 d=1.5 bytes0=3 text="1.5" bytes=3 nul=0 blob="1.5""#,
    ),
    (
        r#"1e14"#,
        r#"ty=2 i=276447232 i64=100000000000000 d=100000000000000.0 bytes0=17 text="100000000000000.0" bytes=17 nul=0 blob="100000000000000.0""#,
    ),
    (
        r#"0.1"#,
        r#"ty=2 i=0 i64=0 d=0.1 bytes0=3 text="0.1" bytes=3 nul=0 blob="0.1""#,
    ),
    (
        r#"1e100"#,
        r#"ty=2 i=-1 i64=9223372036854775807 d=1e100 bytes0=8 text="1.0e+100" bytes=8 nul=0 blob="1.0e+100""#,
    ),
    (
        r#"-0.0"#,
        r#"ty=2 i=0 i64=0 d=-0.0 bytes0=3 text="0.0" bytes=3 nul=0 blob="0.0""#,
    ),
    (
        r#"1e30"#,
        r#"ty=2 i=-1 i64=9223372036854775807 d=1e30 bytes0=7 text="1.0e+30" bytes=7 nul=0 blob="1.0e+30""#,
    ),
    (
        r#"'12abc'"#,
        r#"ty=3 i=12 i64=12 d=12.0 bytes0=5 text="12abc" bytes=5 nul=0 blob="12abc""#,
    ),
    (
        r#"' 5'"#,
        r#"ty=3 i=5 i64=5 d=5.0 bytes0=2 text=" 5" bytes=2 nul=0 blob=" 5""#,
    ),
    (
        r#"''"#,
        r#"ty=3 i=0 i64=0 d=0.0 bytes0=0 text="" bytes=0 nul=0 blob="nil""#,
    ),
    (
        r#"'3.7e2'"#,
        r#"ty=3 i=3 i64=3 d=370.0 bytes0=5 text="3.7e2" bytes=5 nul=0 blob="3.7e2""#,
    ),
    (
        r#"'0x10'"#,
        r#"ty=3 i=0 i64=0 d=0.0 bytes0=4 text="0x10" bytes=4 nul=0 blob="0x10""#,
    ),
    (
        r#"'héllo'"#,
        r#"ty=3 i=0 i64=0 d=0.0 bytes0=6 text="héllo" bytes=6 nul=0 blob="héllo""#,
    ),
    (
        r#"x'31323334'"#,
        r#"ty=4 i=1234 i64=1234 d=1234.0 bytes0=4 text="1234" bytes=4 nul=0 blob="1234""#,
    ),
    (
        r#"x''"#,
        r#"ty=4 i=0 i64=0 d=0.0 bytes0=0 text="" bytes=0 nul=0 blob="nil""#,
    ),
    (
        r#"x'ff'"#,
        r#"ty=4 i=0 i64=0 d=0.0 bytes0=1 text="�" bytes=1 nul=0 blob="�""#,
    ),
    (
        r#"x'2037'"#,
        r#"ty=4 i=7 i64=7 d=7.0 bytes0=2 text=" 7" bytes=2 nul=0 blob=" 7""#,
    ),
];

const LITERALS: &[&str] = &[
    "NULL",
    "42",
    "-7",
    "9223372036854775807",
    "1.5",
    "1e14",
    "0.1",
    "1e100",
    "-0.0",
    "1e30",
    "'12abc'",
    "' 5'",
    "''",
    "'3.7e2'",
    "'0x10'",
    "'h\u{e9}llo'",
    "x'31323334'",
    "x''",
    "x'ff'",
    "x'2037'",
];

#[test]
fn value_accessors_convert_between_storage_classes() {
    unsafe {
        let db = open_mem();
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                c"describe".as_ptr(),
                1,
                1,
                ptr::null_mut(),
                Some(describe_value),
                None,
                None,
                None
            ),
            SQLITE_OK
        );
        for lit in LITERALS {
            let got = first_text(db, &format!("SELECT describe({lit})"));
            let want = VALUE_EXPECTED.iter().find(|(l, _)| l == lit).unwrap().1;
            assert_eq!(got, want, "describe({lit})");
        }
        sqlite3_close(db);
    }
}

#[test]
fn column_accessors_convert_between_storage_classes() {
    unsafe {
        let db = open_mem();
        for lit in LITERALS {
            let mut stmt = ptr::null_mut();
            assert_eq!(
                sqlite3_prepare_v2(
                    db,
                    cs(&format!("SELECT {lit}")).as_ptr(),
                    -1,
                    &mut stmt,
                    ptr::null_mut()
                ),
                SQLITE_OK
            );
            assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
            let ty = sqlite3_column_type(stmt, 0);
            let (i64v, d) = (
                sqlite3_column_int64(stmt, 0),
                sqlite3_column_double(stmt, 0),
            );
            let i = i64v as i32;
            let bytes_before = sqlite3_column_bytes(stmt, 0) as c_int;
            let text_ptr = sqlite3_column_text(stmt, 0) as *const u8;
            let bytes = sqlite3_column_bytes(stmt, 0) as c_int;
            let text = show_text(text_ptr, bytes);
            let nul = if text_ptr.is_null() {
                0
            } else {
                *text_ptr.add(bytes as usize)
            };
            let blob = show_text(
                sqlite3_column_blob(stmt, 0) as *const u8,
                sqlite3_column_bytes(stmt, 0) as c_int,
            );
            let got = format!("ty={ty} i={i} i64={i64v} d={d:?} bytes0={bytes_before} text={text:?} bytes={bytes} nul={nul} blob={blob:?}");
            let want = COLUMN_EXPECTED.iter().find(|(l, _)| l == lit).unwrap().1;
            assert_eq!(got, want, "SELECT {lit}");
            sqlite3_finalize(stmt);
        }
        sqlite3_close(db);
    }
}

#[test]
fn value_bytes_counts_utf8_after_utf16_reads() {
    unsafe extern "C" fn probe(ctx: *mut c_void, _n: c_int, argv: *mut *mut c_void) {
        let v = *argv;
        let a = sqlite3_value_bytes(v);
        sqlite3_value_text16(v);
        let b = sqlite3_value_bytes(v);
        sqlite3_value_text(v);
        let c = sqlite3_value_bytes(v);
        let s = cs(&format!("{a},{b},{c}"));
        sqlite3_result_text(ctx, s.as_ptr(), -1, -1);
    }
    unsafe {
        let db = open_mem();
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                c"probe".as_ptr(),
                1,
                1,
                ptr::null_mut(),
                Some(probe),
                None,
                None,
                None
            ),
            SQLITE_OK
        );
        assert_eq!(first_text(db, "SELECT probe('abc')"), "3,3,3");
        assert_eq!(first_text(db, "SELECT probe(12345)"), "5,5,5");
        sqlite3_close(db);
    }
}
