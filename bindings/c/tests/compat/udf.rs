//! Tests for application-defined functions: sqlite3_create_function*,
//! sqlite3_create_window_function.
#![allow(non_camel_case_types, clashing_extern_declarations)]
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

#[repr(C)]
pub struct sqlite3 {
    _private: [u8; 0],
}
#[repr(C)]
pub struct sqlite3_stmt {
    _private: [u8; 0],
}

type XFunc = unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_void);
type XFinal = unsafe extern "C" fn(*mut c_void);
type XDestroy = unsafe extern "C" fn(*mut c_void);

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
    fn sqlite3_open(filename: *const c_char, db: *mut *mut sqlite3) -> c_int;
    fn sqlite3_close(db: *mut sqlite3) -> c_int;
    fn sqlite3_exec(
        db: *mut sqlite3,
        sql: *const c_char,
        cb: Option<
            unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
        >,
        arg: *mut c_void,
        err: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char;
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
    fn sqlite3_column_text(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_char;
    fn sqlite3_value_int64(v: *mut c_void) -> i64;
    fn sqlite3_value_text(v: *mut c_void) -> *const c_char;
    fn sqlite3_result_int64(ctx: *mut c_void, v: i64);
    fn sqlite3_result_text(ctx: *mut c_void, s: *const c_char, n: c_int, d: isize);
    fn sqlite3_user_data(ctx: *mut c_void) -> *mut c_void;
    fn sqlite3_context_db_handle(ctx: *mut c_void) -> *mut sqlite3;
    fn sqlite3_aggregate_context(ctx: *mut c_void, n: c_int) -> *mut c_void;
    fn sqlite3_create_function_v2(
        db: *mut sqlite3,
        name: *const c_char,
        n_args: c_int,
        enc: c_int,
        app: *mut c_void,
        func: Option<XFunc>,
        step: Option<XFunc>,
        fin: Option<XFinal>,
        destroy: Option<XDestroy>,
    ) -> c_int;
    fn sqlite3_create_function16(
        db: *mut sqlite3,
        name: *const c_void,
        n_args: c_int,
        enc: c_int,
        app: *mut c_void,
        func: Option<XFunc>,
        step: Option<XFunc>,
        fin: Option<XFinal>,
    ) -> c_int;
    fn sqlite3_create_window_function(
        db: *mut sqlite3,
        name: *const c_char,
        n_args: c_int,
        enc: c_int,
        app: *mut c_void,
        step: Option<XFunc>,
        fin: Option<XFinal>,
        value: Option<XFinal>,
        inverse: Option<XFunc>,
        destroy: Option<XDestroy>,
    ) -> c_int;
}

const OK: c_int = 0;
const ROW: c_int = 100;
const DONE: c_int = 101;
const MISUSE: c_int = 21;
const BUSY: c_int = 5;
const UTF8: c_int = 1;
const UTF16LE: c_int = 2;
const UTF16BE: c_int = 3;
const UTF16: c_int = 4;
const ANY: c_int = 5;
const DETERMINISTIC: c_int = 0x800;
const DIRECTONLY: c_int = 0x80000;
const INNOCUOUS: c_int = 0x200000;
const TRANSIENT: isize = -1;

unsafe fn open() -> *mut sqlite3 {
    let mut db = ptr::null_mut();
    assert_eq!(sqlite3_open(c":memory:".as_ptr(), &mut db), OK);
    db
}

unsafe fn err(db: *mut sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db))
        .to_string_lossy()
        .into_owned()
}

unsafe fn exec(db: *mut sqlite3, sql: &str) -> Result<(), String> {
    let sql = CString::new(sql).unwrap();
    match sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut()) {
        OK => Ok(()),
        _ => Err(err(db)),
    }
}

/// Runs a query and returns the first column of every row as text.
unsafe fn query(db: *mut sqlite3, sql: &str) -> Result<Vec<String>, String> {
    let csql = CString::new(sql).unwrap();
    let mut st = ptr::null_mut();
    if sqlite3_prepare_v2(db, csql.as_ptr(), -1, &mut st, ptr::null_mut()) != OK {
        return Err(err(db));
    }
    let mut rows = Vec::new();
    loop {
        match sqlite3_step(st) {
            ROW => {
                let t = sqlite3_column_text(st, 0);
                rows.push(if t.is_null() {
                    "NULL".to_string()
                } else {
                    CStr::from_ptr(t).to_string_lossy().into_owned()
                });
            }
            DONE => break,
            _ => {
                let e = err(db);
                sqlite3_finalize(st);
                return Err(e);
            }
        }
    }
    sqlite3_finalize(st);
    Ok(rows)
}

unsafe fn one(db: *mut sqlite3, sql: &str) -> String {
    let rows = query(db, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(rows.len(), 1, "{sql}");
    rows.into_iter().next().unwrap()
}

unsafe fn scalar(
    db: *mut sqlite3,
    name: &CStr,
    n_args: c_int,
    enc: c_int,
    app: *mut c_void,
    f: XFunc,
    destroy: Option<XDestroy>,
) -> c_int {
    sqlite3_create_function_v2(
        db,
        name.as_ptr(),
        n_args,
        enc,
        app,
        Some(f),
        None,
        None,
        destroy,
    )
}

unsafe fn delete(db: *mut sqlite3, name: &CStr, n_args: c_int, enc: c_int) -> c_int {
    sqlite3_create_function_v2(
        db,
        name.as_ptr(),
        n_args,
        enc,
        ptr::null_mut(),
        None,
        None,
        None,
        None,
    )
}

/// Returns pApp as an integer.
unsafe extern "C" fn ret_app(ctx: *mut c_void, _: c_int, _: *mut *mut c_void) {
    sqlite3_result_int64(ctx, sqlite3_user_data(ctx) as i64);
}

/// Returns the first argument plus pApp.
unsafe extern "C" fn add_app(ctx: *mut c_void, _: c_int, argv: *mut *mut c_void) {
    let x = sqlite3_value_int64(*argv);
    sqlite3_result_int64(ctx, x + sqlite3_user_data(ctx) as i64);
}

/// Returns the argument count.
unsafe extern "C" fn ret_argc(ctx: *mut c_void, argc: c_int, _: *mut *mut c_void) {
    sqlite3_result_int64(ctx, argc as i64);
}

/// Returns pApp, a C string, as text.
unsafe extern "C" fn ret_text(ctx: *mut c_void, _: c_int, _: *mut *mut c_void) {
    sqlite3_result_text(ctx, sqlite3_user_data(ctx) as *const c_char, -1, TRANSIENT);
}

unsafe extern "C" fn upper_x(ctx: *mut c_void, _: c_int, argv: *mut *mut c_void) {
    let s = CStr::from_ptr(sqlite3_value_text(*argv)).to_string_lossy();
    let r = CString::new(format!("X{}", s.to_uppercase())).unwrap();
    sqlite3_result_text(ctx, r.as_ptr(), -1, TRANSIENT);
}

static DESTROYED: AtomicUsize = AtomicUsize::new(0);

/// pApp points at an AtomicUsize that counts calls.
unsafe extern "C" fn count_destroy(p: *mut c_void) {
    (*(p as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst);
}

fn counter() -> *mut c_void {
    Box::into_raw(Box::new(AtomicUsize::new(0))) as *mut c_void
}

unsafe fn count(p: *mut c_void) -> usize {
    (*(p as *const AtomicUsize)).load(Ordering::SeqCst)
}

unsafe extern "C" fn sum_step(ctx: *mut c_void, _: c_int, argv: *mut *mut c_void) {
    let acc = sqlite3_aggregate_context(ctx, 8) as *mut i64;
    *acc += sqlite3_value_int64(*argv);
}

/// Sums the product of two arguments.
unsafe extern "C" fn dot_step(ctx: *mut c_void, _: c_int, argv: *mut *mut c_void) {
    let acc = sqlite3_aggregate_context(ctx, 8) as *mut i64;
    *acc += sqlite3_value_int64(*argv) * sqlite3_value_int64(*argv.add(1));
}

unsafe extern "C" fn sum_inverse(ctx: *mut c_void, _: c_int, argv: *mut *mut c_void) {
    let acc = sqlite3_aggregate_context(ctx, 8) as *mut i64;
    *acc -= sqlite3_value_int64(*argv);
}

unsafe extern "C" fn sum_final(ctx: *mut c_void) {
    let acc = sqlite3_aggregate_context(ctx, 8) as *mut i64;
    sqlite3_result_int64(ctx, *acc);
}

unsafe extern "C" fn db_is_same(ctx: *mut c_void, _: c_int, _: *mut *mut c_void) {
    let db = sqlite3_user_data(ctx) as *mut sqlite3;
    sqlite3_result_int64(ctx, (sqlite3_context_db_handle(ctx) == db) as i64);
}

#[test]
fn registers_200_functions() {
    unsafe {
        let db = open();
        for i in 0..200 {
            let name = CString::new(format!("fn{i}")).unwrap();
            assert_eq!(
                scalar(db, &name, 0, UTF8, i as *mut c_void, ret_app, None),
                OK
            );
        }
        for i in 0..200 {
            assert_eq!(one(db, &format!("SELECT fn{i}()")), i.to_string());
        }
        // One statement can call all of them, each with its own pApp.
        let calls = (0..200)
            .map(|i| format!("SELECT fn{i}()"))
            .collect::<Vec<_>>();
        let rows = query(db, &calls.join(" UNION ALL ")).unwrap();
        assert_eq!(rows, (0..200).map(|i| i.to_string()).collect::<Vec<_>>());
        assert_eq!(sqlite3_close(db), OK);
    }
}

#[test]
fn functions_are_per_connection() {
    unsafe {
        let a = open();
        let b = open();
        assert_eq!(scalar(a, c"shared", 0, UTF8, 1 as _, ret_app, None), OK);
        assert_eq!(scalar(b, c"shared", 0, UTF8, 2 as _, ret_app, None), OK);
        assert_eq!(scalar(a, c"only_a", 0, UTF8, 3 as _, ret_app, None), OK);
        assert_eq!(one(a, "SELECT shared()"), "1");
        assert_eq!(one(b, "SELECT shared()"), "2");
        assert!(query(b, "SELECT only_a()").is_err());
        // Replacing and deleting on one connection leaves the other alone.
        assert_eq!(scalar(a, c"shared", 0, UTF8, 10 as _, ret_app, None), OK);
        assert_eq!(delete(a, c"shared", 0, UTF8), OK);
        assert!(query(a, "SELECT shared()").is_err());
        assert_eq!(one(b, "SELECT shared()"), "2");
        // Closing one connection leaves the other's functions working.
        assert_eq!(sqlite3_close(a), OK);
        assert_eq!(one(b, "SELECT shared()"), "2");
        assert_eq!(sqlite3_close(b), OK);
    }
}

#[test]
fn overloads_by_argument_count() {
    unsafe {
        let db = open();
        // Registered variadic first: an exact count still wins.
        assert_eq!(scalar(db, c"f", -1, UTF8, 100 as _, ret_app, None), OK);
        assert_eq!(scalar(db, c"f", 1, UTF8, 1 as _, ret_app, None), OK);
        assert_eq!(scalar(db, c"F", 2, UTF8, 2 as _, ret_app, None), OK);
        assert_eq!(one(db, "SELECT f()"), "100");
        assert_eq!(one(db, "SELECT f(1)"), "1");
        assert_eq!(one(db, "SELECT f(1, 2)"), "2");
        assert_eq!(one(db, "SELECT F(1, 2, 3)"), "100");
        // Registered variadic last.
        assert_eq!(scalar(db, c"g", 3, UTF8, 3 as _, ret_app, None), OK);
        assert_eq!(scalar(db, c"g", -1, UTF8, 100 as _, ret_app, None), OK);
        assert_eq!(one(db, "SELECT g(1, 2, 3)"), "3");
        assert_eq!(one(db, "SELECT g(1)"), "100");
        // Replacing one count leaves the others.
        assert_eq!(scalar(db, c"f", 1, UTF8, 11 as _, ret_app, None), OK);
        assert_eq!(one(db, "SELECT f(1)"), "11");
        assert_eq!(one(db, "SELECT f(1, 2)"), "2");
        // Deleting the variadic one leaves the exact ones.
        assert_eq!(delete(db, c"f", -1, UTF8), OK);
        assert_eq!(one(db, "SELECT f(1)"), "11");
        let e = query(db, "SELECT f(1, 2, 3)").unwrap_err();
        assert!(
            e.contains("wrong number of arguments to function f()"),
            "{e}"
        );
        let e = query(db, "SELECT nosuch(1)").unwrap_err();
        assert!(e.contains("no such function: nosuch"), "{e}");
        sqlite3_close(db);
    }
}

#[test]
fn text_representation_selection() {
    unsafe {
        let db = open();
        // UTF-8 beats UTF-16 for a UTF-8 database, whatever the order.
        assert_eq!(scalar(db, c"h", 0, UTF16LE, 16 as _, ret_app, None), OK);
        assert_eq!(one(db, "SELECT h()"), "16");
        assert_eq!(scalar(db, c"h", 0, UTF8, 8 as _, ret_app, None), OK);
        assert_eq!(one(db, "SELECT h()"), "8");
        assert_eq!(scalar(db, c"k", 0, UTF8, 8 as _, ret_app, None), OK);
        assert_eq!(scalar(db, c"k", 0, UTF16BE, 16 as _, ret_app, None), OK);
        assert_eq!(one(db, "SELECT k()"), "8");
        // UTF16 and ANY register native/all encodings.
        assert_eq!(scalar(db, c"m", 0, UTF16, 4 as _, ret_app, None), OK);
        assert_eq!(one(db, "SELECT m()"), "4");
        assert_eq!(scalar(db, c"n", 0, ANY, 5 as _, ret_app, None), OK);
        assert_eq!(one(db, "SELECT n()"), "5");
        // An ANY registration is replaced entry by entry.
        assert_eq!(scalar(db, c"n", 0, UTF8, 6 as _, ret_app, None), OK);
        assert_eq!(one(db, "SELECT n()"), "6");
        // Unknown encodings behave as UTF-8.
        assert_eq!(scalar(db, c"p", 0, 0, 7 as _, ret_app, None), OK);
        assert_eq!(scalar(db, c"p", 0, UTF8, 9 as _, ret_app, None), OK);
        assert_eq!(one(db, "SELECT p()"), "9");
        // The text arguments are readable regardless of the registered encoding.
        assert_eq!(
            scalar(db, c"up", 1, UTF16LE, ptr::null_mut(), upper_x, None),
            OK
        );
        assert_eq!(one(db, "SELECT up('ab')"), "XAB");
        sqlite3_close(db);
    }
}

#[test]
fn deterministic_functions_work_in_indexes_and_checks() {
    unsafe {
        let db = open();
        let flags = UTF8 | DETERMINISTIC;
        assert_eq!(scalar(db, c"det", 1, flags, 100 as _, add_app, None), OK);
        assert_eq!(scalar(db, c"nondet", 1, UTF8, 100 as _, add_app, None), OK);
        exec(db, "CREATE TABLE t(x INTEGER)").unwrap();
        exec(db, "CREATE INDEX t_det ON t(det(x))").unwrap();
        assert!(exec(db, "CREATE INDEX t_nondet ON t(nondet(x))").is_err());
        assert!(exec(db, "CREATE INDEX t_partial ON t(x) WHERE nondet(x) > 0").is_err());
        exec(db, "CREATE INDEX t_partial ON t(x) WHERE det(x) > 0").unwrap();
        exec(db, "INSERT INTO t VALUES (1), (-500), (2)").unwrap();
        assert_eq!(one(db, "SELECT x FROM t WHERE det(x) = 102"), "2");
        // Generated columns accept deterministic functions only.
        exec(db, "CREATE TABLE g(x INTEGER, y AS (det(x)))").unwrap();
        assert!(exec(db, "CREATE TABLE g2(x INTEGER, y AS (nondet(x)))").is_err());
        exec(db, "INSERT INTO g VALUES (5)").unwrap();
        assert_eq!(one(db, "SELECT y FROM g"), "105");
        // CHECK constraints evaluate them on every write.
        exec(db, "CREATE TABLE c(x INTEGER CHECK (det(x) > 100))").unwrap();
        exec(db, "INSERT INTO c VALUES (1)").unwrap();
        assert!(exec(db, "INSERT INTO c VALUES (-200)").is_err());
        sqlite3_close(db);
    }
}

#[test]
fn directonly_functions_are_refused_from_schema_code() {
    unsafe {
        let db = open();
        let direct = UTF8 | DIRECTONLY | DETERMINISTIC;
        assert_eq!(scalar(db, c"d", 1, direct, 7 as _, add_app, None), OK);
        assert_eq!(
            scalar(db, c"safe", 1, direct | INNOCUOUS, 7 as _, add_app, None),
            OK
        );
        exec(db, "CREATE TABLE t(x INTEGER)").unwrap();
        // Direct statements may call it.
        assert_eq!(one(db, "SELECT d(1)"), "8");
        exec(db, "INSERT INTO t VALUES (1)").unwrap();
        assert_eq!(one(db, "SELECT d(x) FROM t"), "8");
        // Views, triggers, CHECK constraints and indexes may not.
        exec(db, "CREATE VIEW v AS SELECT d(x) AS y FROM t").unwrap();
        let e = query(db, "SELECT * FROM v").unwrap_err();
        assert!(e.contains("unsafe use of d()"), "{e}");
        exec(db, "CREATE TABLE log(y)").unwrap();
        exec(
            db,
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES (d(new.x)); END",
        )
        .unwrap();
        let e = exec(db, "INSERT INTO t VALUES (2)").unwrap_err();
        assert!(e.contains("unsafe use of d()"), "{e}");
        let e = exec(db, "CREATE TABLE c(x CHECK (d(x) > 0))").unwrap_err();
        assert!(e.contains("unsafe use of d()"), "{e}");
        let e = exec(db, "CREATE INDEX i ON t(d(x))").unwrap_err();
        assert!(e.contains("unsafe use of d()"), "{e}");
        // The function is still usable directly afterwards.
        assert_eq!(one(db, "SELECT d(2)"), "9");
        sqlite3_close(db);
    }
}

#[test]
fn subtype_and_innocuous_flags_are_accepted() {
    unsafe {
        let db = open();
        const SUBTYPE: c_int = 0x100000;
        const RESULT_SUBTYPE: c_int = 0x1000000;
        let flags = UTF8 | INNOCUOUS | SUBTYPE | RESULT_SUBTYPE | DETERMINISTIC;
        assert_eq!(scalar(db, c"s", 1, flags, 1 as _, add_app, None), OK);
        assert_eq!(one(db, "SELECT s(1)"), "2");
        sqlite3_close(db);
    }
}

#[test]
fn xdestroy_runs_on_replace_delete_close_and_failure() {
    unsafe {
        let db = open();
        let a = counter();
        let b = counter();
        assert_eq!(
            scalar(db, c"f", 1, UTF8, a, ret_app, Some(count_destroy)),
            OK
        );
        assert_eq!(count(a), 0);
        // A prepared statement does not delay xDestroy on replacement.
        let mut st = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(db, c"SELECT f(1)".as_ptr(), -1, &mut st, ptr::null_mut()),
            OK
        );
        assert_eq!(
            scalar(db, c"f", 1, UTF8, b, ret_app, Some(count_destroy)),
            OK
        );
        assert_eq!(count(a), 1);
        assert_eq!(count(b), 0);
        sqlite3_finalize(st);
        // Deleting with NULL callbacks runs xDestroy of the passed pointer.
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                c"f".as_ptr(),
                1,
                UTF8,
                b,
                None,
                None,
                None,
                Some(count_destroy)
            ),
            OK
        );
        // The deleting call's xDestroy stays attached to the deleted entry.
        assert_eq!(count(b), 1);
        assert!(query(db, "SELECT f(1)").is_err());
        // A refused registration also runs it.
        let c = counter();
        assert_eq!(
            scalar(db, c"bad", 2000, UTF8, c, ret_app, Some(count_destroy)),
            MISUSE
        );
        assert_eq!(count(c), 1);
        // Close runs it for every function; ANY registers once.
        let d = counter();
        let e = counter();
        assert_eq!(
            scalar(db, c"g1", 0, UTF8, d, ret_app, Some(count_destroy)),
            OK
        );
        assert_eq!(
            scalar(db, c"g2", 0, ANY, e, ret_app, Some(count_destroy)),
            OK
        );
        assert_eq!(scalar(db, c"g2", 0, UTF8, 1 as _, ret_app, None), OK);
        assert_eq!(count(e), 0, "UTF-16 entries of the ANY registration remain");
        assert_eq!(sqlite3_close(db), OK);
        assert_eq!(count(b), 2);
        assert_eq!(count(d), 1);
        assert_eq!(count(e), 1);
    }
}

#[test]
fn overriding_and_deleting_builtins() {
    unsafe {
        let db = open();
        assert_eq!(one(db, "SELECT lower('AB')"), "ab");
        assert_eq!(scalar(db, c"lower", 1, UTF8, 5 as _, ret_app, None), OK);
        assert_eq!(one(db, "SELECT lower('AB')"), "5");
        assert_eq!(one(db, "SELECT LOWER('AB')"), "5");
        // Other argument counts of the built-in are unaffected.
        assert_eq!(scalar(db, c"substr", 5, UTF8, 6 as _, ret_app, None), OK);
        assert_eq!(one(db, "SELECT substr('abc', 2)"), "bc");
        assert_eq!(one(db, "SELECT substr('abc', 1, 2, 3, 4)"), "6");
        // A variadic override takes every count.
        assert_eq!(scalar(db, c"abs", -1, UTF8, 7 as _, ret_app, None), OK);
        assert_eq!(one(db, "SELECT abs(-3)"), "7");
        // Deleting an override does not bring the built-in back.
        assert_eq!(delete(db, c"lower", 1, UTF8), OK);
        assert!(query(db, "SELECT lower('AB')").is_err());
        sqlite3_close(db);
        // Another connection still sees the built-in.
        let db = open();
        assert_eq!(one(db, "SELECT lower('AB')"), "ab");
        sqlite3_close(db);
    }
}

#[test]
fn overrides_apply_to_prepared_statements() {
    unsafe {
        let db = open();
        assert_eq!(scalar(db, c"f", 0, UTF8, 1 as _, ret_app, None), OK);
        let mut st = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(db, c"SELECT f()".as_ptr(), -1, &mut st, ptr::null_mut()),
            OK
        );
        assert_eq!(sqlite3_step(st), ROW);
        assert_eq!(sqlite3_reset(st), OK);
        assert_eq!(scalar(db, c"f", 0, UTF8, 2 as _, ret_app, None), OK);
        assert_eq!(sqlite3_step(st), ROW);
        let v = CStr::from_ptr(sqlite3_column_text(st, 0)).to_str().unwrap();
        assert_eq!(v, "2");
        sqlite3_finalize(st);
        sqlite3_close(db);
    }
}

#[test]
fn replacing_a_function_in_use_is_busy() {
    unsafe {
        let db = open();
        assert_eq!(scalar(db, c"f", 1, UTF8, 1 as _, ret_app, None), OK);
        let mut st = ptr::null_mut();
        let sql = c"SELECT f(1) UNION ALL SELECT f(1)";
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()),
            OK
        );
        assert_eq!(sqlite3_step(st), ROW);
        assert_eq!(scalar(db, c"f", 1, UTF8, 2 as _, ret_app, None), BUSY);
        assert_eq!(delete(db, c"f", 1, UTF8), BUSY);
        // New names and other argument counts are not affected.
        assert_eq!(scalar(db, c"f", 2, UTF8, 2 as _, ret_app, None), OK);
        sqlite3_finalize(st);
        assert_eq!(scalar(db, c"f", 1, UTF8, 2 as _, ret_app, None), OK);
        assert_eq!(one(db, "SELECT f(1)"), "2");
        sqlite3_close(db);
    }
}

#[test]
fn invalid_registrations_are_misuse() {
    unsafe {
        let db = open();
        let long = CString::new("x".repeat(256)).unwrap();
        let ok_long = CString::new("x".repeat(255)).unwrap();
        assert_eq!(
            scalar(db, &long, 1, UTF8, ptr::null_mut(), ret_app, None),
            MISUSE
        );
        assert_eq!(
            scalar(db, &ok_long, 1, UTF8, ptr::null_mut(), ret_app, None),
            OK
        );
        assert_eq!(
            scalar(db, c"f", -2, UTF8, ptr::null_mut(), ret_app, None),
            MISUSE
        );
        assert_eq!(
            scalar(db, c"f", 1000, UTF8, ptr::null_mut(), ret_app, None),
            OK
        );
        assert_eq!(
            scalar(db, c"f", 1001, UTF8, ptr::null_mut(), ret_app, None),
            MISUSE
        );
        let reg = |func: Option<XFunc>, step: Option<XFunc>, fin: Option<XFinal>| {
            sqlite3_create_function_v2(
                db,
                c"agg".as_ptr(),
                1,
                UTF8,
                ptr::null_mut(),
                func,
                step,
                fin,
                None,
            )
        };
        assert_eq!(reg(Some(ret_app), None, Some(sum_final)), MISUSE);
        assert_eq!(reg(None, Some(sum_step), None), MISUSE);
        assert_eq!(reg(None, None, Some(sum_final)), MISUSE);
        assert_eq!(reg(None, Some(sum_step), Some(sum_final)), OK);
        let win = |value: Option<XFinal>, inverse: Option<XFunc>| {
            sqlite3_create_window_function(
                db,
                c"w".as_ptr(),
                1,
                UTF8,
                ptr::null_mut(),
                Some(sum_step),
                Some(sum_final),
                value,
                inverse,
                None,
            )
        };
        assert_eq!(win(Some(sum_final), None), MISUSE);
        assert_eq!(win(None, Some(sum_inverse)), MISUSE);
        assert_eq!(win(Some(sum_final), Some(sum_inverse)), OK);
        sqlite3_close(db);
    }
}

#[test]
fn aggregates_and_windows_overload_by_argument_count() {
    unsafe {
        let db = open();
        let agg = |name: &CStr, n: c_int, step: XFunc, app: usize| {
            sqlite3_create_function_v2(
                db,
                name.as_ptr(),
                n,
                UTF8,
                app as *mut c_void,
                None,
                Some(step),
                Some(sum_final),
                None,
            )
        };
        assert_eq!(agg(c"total", 1, sum_step, 0), OK);
        assert_eq!(agg(c"total", 2, dot_step, 0), OK);
        exec(db, "CREATE TABLE t(a INTEGER, b INTEGER)").unwrap();
        exec(db, "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)").unwrap();
        assert_eq!(one(db, "SELECT total(a) FROM t"), "6");
        assert_eq!(one(db, "SELECT total(a, b) FROM t"), "140");
        let e = query(db, "SELECT total(a, b, a) FROM t").unwrap_err();
        assert!(
            e.contains("wrong number of arguments to function total()"),
            "{e}"
        );
        // The same name as a scalar with another count.
        assert_eq!(scalar(db, c"total", 3, UTF8, 33 as _, ret_app, None), OK);
        assert_eq!(one(db, "SELECT total(1, 2, 3)"), "33");
        assert_eq!(one(db, "SELECT total(a) FROM t"), "6");
        // Window functions.
        assert_eq!(
            sqlite3_create_window_function(
                db,
                c"wsum".as_ptr(),
                1,
                UTF8,
                ptr::null_mut(),
                Some(sum_step),
                Some(sum_final),
                Some(sum_final),
                Some(sum_inverse),
                None,
            ),
            OK
        );
        let rows = query(
            db,
            "SELECT wsum(a) OVER (ORDER BY a ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
        )
        .unwrap();
        assert_eq!(rows, ["1", "3", "5"]);
        // An ordinary aggregate still cannot be a window function.
        assert!(query(db, "SELECT total(a) OVER () FROM t").is_err());
        sqlite3_close(db);
    }
}

#[test]
fn create_function16_registers_by_utf16_name() {
    unsafe {
        let db = open();
        let name: Vec<u16> = "fn16\u{e9}".encode_utf16().chain([0]).collect();
        assert_eq!(
            sqlite3_create_function16(
                db,
                name.as_ptr() as *const c_void,
                1,
                UTF16LE,
                9 as _,
                Some(add_app),
                None,
                None,
            ),
            OK
        );
        assert_eq!(one(db, "SELECT fn16\u{e9}(1)"), "10");
        // Overloads and deletion work as for the UTF-8 variant.
        assert_eq!(
            sqlite3_create_function16(
                db,
                name.as_ptr() as *const c_void,
                2,
                UTF16LE,
                0 as _,
                Some(ret_argc),
                None,
                None,
            ),
            OK
        );
        assert_eq!(one(db, "SELECT fn16\u{e9}(1, 2)"), "2");
        assert_eq!(
            sqlite3_create_function16(
                db,
                name.as_ptr() as *const c_void,
                2,
                UTF16LE,
                ptr::null_mut(),
                None,
                None,
                None,
            ),
            OK
        );
        assert!(query(db, "SELECT fn16\u{e9}(1, 2)").is_err());
        assert_eq!(
            sqlite3_create_function16(
                db,
                ptr::null(),
                2,
                UTF16LE,
                ptr::null_mut(),
                Some(ret_app),
                None,
                None,
            ),
            MISUSE
        );
        sqlite3_close(db);
    }
}

#[test]
fn context_reports_its_connection() {
    unsafe {
        let a = open();
        let b = open();
        assert_eq!(scalar(a, c"same", 0, UTF8, a as _, db_is_same, None), OK);
        assert_eq!(scalar(b, c"same", 0, UTF8, a as _, db_is_same, None), OK);
        assert_eq!(one(a, "SELECT same()"), "1");
        assert_eq!(one(b, "SELECT same()"), "0");
        sqlite3_close(a);
        sqlite3_close(b);
    }
}
