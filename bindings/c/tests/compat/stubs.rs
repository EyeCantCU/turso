//! Tests for former stubs: sleep, aggregates, window functions, backup,
//! serialize/deserialize, UTF-16, NULL guards, reset/finalize codes, malloc64.
#![allow(non_camel_case_types)]
use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr;

#[repr(C)]
pub struct sqlite3 {
    _private: [u8; 0],
}
#[repr(C)]
pub struct sqlite3_stmt {
    _private: [u8; 0],
}
#[repr(C)]
pub struct sqlite3_backup {
    _private: [u8; 0],
}

type XFunc = unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_void);
type XFinal = unsafe extern "C" fn(*mut c_void);

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
    fn sqlite3_errmsg16(db: *mut sqlite3) -> *const c_void;
    fn sqlite3_prepare_v2(
        db: *mut sqlite3,
        sql: *const c_char,
        n: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_prepare16_v2(
        db: *mut sqlite3,
        sql: *const c_void,
        n: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_void,
    ) -> c_int;
    fn sqlite3_step(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_reset(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_finalize(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_column_count(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_data_count(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_bind_parameter_count(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_column_int64(stmt: *mut sqlite3_stmt, i: c_int) -> i64;
    fn sqlite3_column_text(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_char;
    fn sqlite3_column_text16(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_void;
    fn sqlite3_column_bytes16(stmt: *mut sqlite3_stmt, i: c_int) -> c_int;
    fn sqlite3_column_name(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_char;
    fn sqlite3_column_name16(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_void;
    fn sqlite3_column_decltype(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_char;
    fn sqlite3_column_table_name(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_char;
    fn sqlite3_bind_text16(
        stmt: *mut sqlite3_stmt,
        i: c_int,
        data: *const c_void,
        n: c_int,
        destructor: isize,
    ) -> c_int;
    fn sqlite3_bind_int64(stmt: *mut sqlite3_stmt, i: c_int, v: i64) -> c_int;
    fn sqlite3_value_int64(v: *mut c_void) -> i64;
    fn sqlite3_result_int64(ctx: *mut c_void, v: i64);
    fn sqlite3_result_error(ctx: *mut c_void, msg: *const c_char, n: c_int);
    fn sqlite3_aggregate_context(ctx: *mut c_void, n: c_int) -> *mut c_void;
    fn sqlite3_create_function_v2(
        db: *mut sqlite3,
        name: *const c_char,
        n_args: c_int,
        enc: c_int,
        app: *mut c_void,
        func: Option<XFunc>,
        step: Option<unsafe extern "C" fn()>,
        fin: Option<unsafe extern "C" fn()>,
        destroy: Option<unsafe extern "C" fn(*mut c_void)>,
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
        destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    fn sqlite3_backup_init(
        dest: *mut sqlite3,
        dest_name: *const c_char,
        src: *mut sqlite3,
        src_name: *const c_char,
    ) -> *mut sqlite3_backup;
    fn sqlite3_backup_step(b: *mut sqlite3_backup, n: c_int) -> c_int;
    fn sqlite3_backup_finish(b: *mut sqlite3_backup) -> c_int;
    fn sqlite3_backup_remaining(b: *mut sqlite3_backup) -> c_int;
    fn sqlite3_backup_pagecount(b: *mut sqlite3_backup) -> c_int;
    fn sqlite3_serialize(
        db: *mut sqlite3,
        schema: *const c_char,
        size: *mut i64,
        flags: u32,
    ) -> *mut u8;
    fn sqlite3_deserialize(
        db: *mut sqlite3,
        schema: *const c_char,
        data: *mut u8,
        sz_db: i64,
        sz_buf: i64,
        flags: u32,
    ) -> c_int;
    fn sqlite3_sleep(ms: c_int) -> c_int;
    fn sqlite3_malloc64(n: u64) -> *mut c_void;
    fn sqlite3_free(p: *mut c_void);
}

const OK: c_int = 0;
const ERROR: c_int = 1;
const CONSTRAINT: c_int = 19;
const ROW: c_int = 100;
const DONE: c_int = 101;
const SQLITE_UTF8: c_int = 1;
const SQLITE_TRANSIENT: isize = -1;
const SERIALIZE_FREEONCLOSE: u32 = 1;

/// Registers an aggregate through the (xStep, xFinal) slots of create_function_v2.
unsafe fn create_aggregate(
    db: *mut sqlite3,
    name: &CStr,
    step: XFunc,
    fin: Option<XFinal>,
) -> c_int {
    sqlite3_create_function_v2(
        db,
        name.as_ptr(),
        1,
        SQLITE_UTF8,
        ptr::null_mut(),
        None,
        Some(std::mem::transmute::<XFunc, unsafe extern "C" fn()>(step)),
        fin.map(|f| std::mem::transmute::<XFinal, unsafe extern "C" fn()>(f)),
        None,
    )
}

unsafe fn open() -> *mut sqlite3 {
    let mut db = ptr::null_mut();
    assert_eq!(sqlite3_open(c":memory:".as_ptr(), &mut db), OK);
    db
}

unsafe fn exec(db: *mut sqlite3, sql: &CStr) {
    let rc = sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut());
    assert_eq!(
        rc,
        OK,
        "{:?}: {:?}",
        sql,
        CStr::from_ptr(sqlite3_errmsg(db))
    );
}

unsafe fn prepare(db: *mut sqlite3, sql: &CStr) -> *mut sqlite3_stmt {
    let mut st = ptr::null_mut();
    assert_eq!(
        sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()),
        OK,
        "{:?}",
        CStr::from_ptr(sqlite3_errmsg(db))
    );
    st
}

/// Runs a query and returns the first column of every row as i64.
unsafe fn ints(db: *mut sqlite3, sql: &CStr) -> Vec<i64> {
    let st = prepare(db, sql);
    let mut out = vec![];
    while sqlite3_step(st) == ROW {
        out.push(sqlite3_column_int64(st, 0));
    }
    assert_eq!(sqlite3_finalize(st), OK);
    out
}

unsafe fn u16_to_string(p: *const c_void) -> String {
    let p = p as *const u16;
    let mut v = vec![];
    let mut i = 0;
    while *p.add(i) != 0 {
        v.push(*p.add(i));
        i += 1;
    }
    String::from_utf16(&v).unwrap()
}

fn to_u16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

unsafe extern "C" fn sum_step(ctx: *mut c_void, argc: c_int, argv: *mut *mut c_void) {
    assert_eq!(argc, 1);
    let acc = sqlite3_aggregate_context(ctx, 8) as *mut i64;
    *acc += sqlite3_value_int64(*argv);
}

unsafe extern "C" fn sum_final(ctx: *mut c_void) {
    // No row was stepped: the context is not allocated.
    let acc = sqlite3_aggregate_context(ctx, 0) as *mut i64;
    sqlite3_result_int64(ctx, if acc.is_null() { -1 } else { *acc });
}

unsafe extern "C" fn sum_value(ctx: *mut c_void) {
    let acc = sqlite3_aggregate_context(ctx, 0) as *mut i64;
    sqlite3_result_int64(ctx, if acc.is_null() { 0 } else { *acc });
}

unsafe extern "C" fn sum_inverse(ctx: *mut c_void, _argc: c_int, argv: *mut *mut c_void) {
    let acc = sqlite3_aggregate_context(ctx, 8) as *mut i64;
    *acc -= sqlite3_value_int64(*argv);
}

unsafe extern "C" fn failing_func(ctx: *mut c_void, _argc: c_int, _argv: *mut *mut c_void) {
    sqlite3_result_error(ctx, c"boom".as_ptr(), -1);
}

#[test]
fn sleep_returns_milliseconds() {
    unsafe {
        let t = std::time::Instant::now();
        assert!(sqlite3_sleep(30) >= 30);
        assert!(t.elapsed() >= std::time::Duration::from_millis(30));
        assert_eq!(sqlite3_sleep(0), 0);
    }
}

#[test]
fn malloc64_takes_64_bit_size() {
    unsafe {
        let p = sqlite3_malloc64(1 << 20);
        assert!(!p.is_null());
        sqlite3_free(p);
        // Larger than SQLite's 0x7fffff00 limit.
        assert!(sqlite3_malloc64(u64::MAX).is_null());
    }
}

#[test]
fn aggregate_with_context() {
    unsafe {
        let db = open();
        exec(
            db,
            c"CREATE TABLE t(g, v); INSERT INTO t VALUES (1,10),(1,20),(2,5),(2,NULL);",
        );
        assert_eq!(
            create_aggregate(db, c"mysum", sum_step, Some(sum_final)),
            OK
        );
        assert_eq!(
            ints(db, c"SELECT mysum(v) FROM t GROUP BY g ORDER BY g"),
            vec![30, 5]
        );
        assert_eq!(ints(db, c"SELECT mysum(v) FROM t WHERE 0"), vec![-1]);
        assert_eq!(ints(db, c"SELECT mysum(v) FROM t"), vec![35]);
        assert_eq!(sqlite3_close(db), OK);
    }
}

#[test]
fn window_function_sliding_frame() {
    unsafe {
        let db = open();
        exec(
            db,
            c"CREATE TABLE t(v); INSERT INTO t VALUES (1),(2),(3),(4),(5);",
        );
        assert_eq!(
            sqlite3_create_window_function(
                db,
                c"wsum".as_ptr(),
                1,
                SQLITE_UTF8,
                ptr::null_mut(),
                Some(sum_step),
                Some(sum_final),
                Some(sum_value),
                Some(sum_inverse),
                None,
            ),
            OK
        );
        assert_eq!(
            ints(
                db,
                c"SELECT wsum(v) OVER (ORDER BY v ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t"
            ),
            vec![1, 3, 5, 7, 9]
        );
        assert_eq!(
            ints(db, c"SELECT wsum(v) OVER (ORDER BY v) FROM t"),
            vec![1, 3, 6, 10, 15]
        );
        // Also usable as a plain aggregate.
        assert_eq!(ints(db, c"SELECT wsum(v) FROM t"), vec![15]);
        // Deleting the function.
        assert_eq!(
            sqlite3_create_window_function(
                db,
                c"wsum".as_ptr(),
                1,
                SQLITE_UTF8,
                ptr::null_mut(),
                None,
                None,
                None,
                None,
                None,
            ),
            OK
        );
        let mut st = ptr::null_mut();
        assert_ne!(
            sqlite3_prepare_v2(
                db,
                c"SELECT wsum(v) OVER () FROM t".as_ptr(),
                -1,
                &mut st,
                ptr::null_mut()
            ),
            OK
        );
        assert_eq!(sqlite3_close(db), OK);
    }
}

#[test]
fn plain_aggregate_is_not_a_window_function() {
    unsafe {
        let db = open();
        exec(db, c"CREATE TABLE t(v); INSERT INTO t VALUES (1);");
        assert_eq!(
            create_aggregate(db, c"mysum", sum_step, Some(sum_final)),
            OK
        );
        let mut st = ptr::null_mut();
        assert_ne!(
            sqlite3_prepare_v2(
                db,
                c"SELECT mysum(v) OVER () FROM t".as_ptr(),
                -1,
                &mut st,
                ptr::null_mut()
            ),
            OK
        );
        assert_eq!(sqlite3_close(db), OK);
    }
}

#[test]
fn error_in_function_repeats_after_reset() {
    unsafe {
        let db = open();
        exec(db, c"CREATE TABLE t(v); INSERT INTO t VALUES (1),(2);");
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                c"boom".as_ptr(),
                1,
                SQLITE_UTF8,
                ptr::null_mut(),
                Some(failing_func),
                None,
                None,
                None,
            ),
            OK
        );
        assert_eq!(
            create_aggregate(db, c"aboom", failing_func, Some(sum_final)),
            OK
        );
        for sql in [c"SELECT boom(v) FROM t", c"SELECT aboom(v) FROM t"] {
            let st = prepare(db, sql);
            for _ in 0..3 {
                assert_eq!(sqlite3_step(st), ERROR);
                assert_eq!(CStr::from_ptr(sqlite3_errmsg(db)), c"boom");
                assert_eq!(sqlite3_reset(st), ERROR);
            }
            assert_eq!(sqlite3_finalize(st), OK);
        }
        assert_eq!(sqlite3_close(db), OK);
    }
}

#[test]
fn reset_and_finalize_return_step_error() {
    unsafe {
        let db = open();
        exec(
            db,
            c"CREATE TABLE t(a PRIMARY KEY); INSERT INTO t VALUES (1);",
        );
        let st = prepare(db, c"INSERT INTO t VALUES (1)");
        assert_eq!(sqlite3_step(st), CONSTRAINT);
        assert_eq!(sqlite3_reset(st), CONSTRAINT);
        assert_eq!(sqlite3_reset(st), OK);
        assert_eq!(sqlite3_step(st), CONSTRAINT);
        assert_eq!(sqlite3_finalize(st), CONSTRAINT);
        let st = prepare(db, c"SELECT a FROM t");
        assert_eq!(sqlite3_step(st), ROW);
        assert_eq!(sqlite3_reset(st), OK);
        assert_eq!(sqlite3_finalize(st), OK);
        assert_eq!(sqlite3_close(db), OK);
    }
}

#[test]
fn null_statement_guards() {
    unsafe {
        assert_eq!(sqlite3_finalize(ptr::null_mut()), OK);
        assert_eq!(sqlite3_reset(ptr::null_mut()), OK);
        assert_eq!(sqlite3_column_count(ptr::null_mut()), 0);
        assert_eq!(sqlite3_data_count(ptr::null_mut()), 0);
        assert_eq!(sqlite3_bind_parameter_count(ptr::null_mut()), 0);
    }
}

#[test]
fn column_names_are_stable() {
    unsafe {
        let db = open();
        exec(
            db,
            c"CREATE TABLE t(a INTEGER, b TEXT); INSERT INTO t VALUES (1,'x');",
        );
        let st = prepare(db, c"SELECT a AS first, b FROM t");
        assert_eq!(sqlite3_step(st), ROW);
        for i in 0..2 {
            let n1 = sqlite3_column_name(st, i);
            let n2 = sqlite3_column_name(st, i);
            assert_eq!(n1, n2);
            let d1 = sqlite3_column_decltype(st, i);
            assert_eq!(d1, sqlite3_column_decltype(st, i));
            let t1 = sqlite3_column_table_name(st, i);
            assert_eq!(t1, sqlite3_column_table_name(st, i));
            assert_eq!(CStr::from_ptr(t1), c"t");
        }
        assert_eq!(CStr::from_ptr(sqlite3_column_name(st, 0)), c"first");
        assert_eq!(CStr::from_ptr(sqlite3_column_decltype(st, 1)), c"TEXT");
        assert!(sqlite3_column_name(st, 2).is_null());
        assert!(sqlite3_column_name(st, -1).is_null());
        assert_eq!(sqlite3_finalize(st), OK);
        assert_eq!(sqlite3_close(db), OK);
    }
}

#[test]
fn utf16_api() {
    unsafe {
        let db = open();
        let sql =
            to_u16("CREATE TABLE t(a TEXT); INSERT INTO t VALUES (?); SELECT a AS \u{e9}x FROM t");
        let mut st = ptr::null_mut();
        let mut tail: *const c_void = ptr::null();
        let mut p = sql.as_ptr() as *const c_void;
        // Walk the three statements via pzTail.
        assert_eq!(sqlite3_prepare16_v2(db, p, -1, &mut st, &mut tail), OK);
        assert_eq!(sqlite3_step(st), DONE);
        sqlite3_finalize(st);
        p = tail;
        assert_eq!(sqlite3_prepare16_v2(db, p, -1, &mut st, &mut tail), OK);
        let val = to_u16("h\u{e9}llo \u{1f600}");
        assert_eq!(
            sqlite3_bind_text16(
                st,
                1,
                val.as_ptr() as *const c_void,
                (val.len() as c_int - 1) * 2,
                SQLITE_TRANSIENT
            ),
            OK
        );
        assert_eq!(sqlite3_step(st), DONE);
        sqlite3_finalize(st);
        p = tail;
        assert_eq!(sqlite3_prepare16_v2(db, p, -1, &mut st, &mut tail), OK);
        assert_eq!(sqlite3_step(st), ROW);
        assert_eq!(
            u16_to_string(sqlite3_column_text16(st, 0)),
            "h\u{e9}llo \u{1f600}"
        );
        assert_eq!(sqlite3_column_bytes16(st, 0), (val.len() as c_int - 1) * 2);
        assert_eq!(u16_to_string(sqlite3_column_name16(st, 0)), "\u{e9}x");
        sqlite3_finalize(st);

        let bad = to_u16("SELEC 1");
        assert_ne!(
            sqlite3_prepare16_v2(
                db,
                bad.as_ptr() as *const c_void,
                -1,
                &mut st,
                ptr::null_mut()
            ),
            OK
        );
        let m8 = CStr::from_ptr(sqlite3_errmsg(db))
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(u16_to_string(sqlite3_errmsg16(db)), m8);
        assert_eq!(sqlite3_close(db), OK);
    }
}

unsafe fn fill(db: *mut sqlite3) {
    exec(
        db,
        c"CREATE TABLE t(id INTEGER PRIMARY KEY, s TEXT); CREATE INDEX ti ON t(s); PRAGMA user_version=7;",
    );
    exec(db, c"BEGIN");
    for i in 0..1500 {
        let sql = std::ffi::CString::new(format!(
            "INSERT INTO t(s) VALUES ('{}-{}')",
            i,
            "x".repeat(100)
        ))
        .unwrap();
        exec(db, &sql);
    }
    exec(db, c"COMMIT");
}

#[test]
fn backup_steps_incrementally() {
    unsafe {
        let src = open();
        fill(src);
        let dst = open();
        let b = sqlite3_backup_init(dst, c"main".as_ptr(), src, c"main".as_ptr());
        assert!(!b.is_null());
        let mut steps = 0;
        let mut last_remaining = i32::MAX;
        loop {
            let rc = sqlite3_backup_step(b, 1);
            steps += 1;
            assert!(rc == OK || rc == DONE, "rc={rc}");
            assert!(sqlite3_backup_remaining(b) <= last_remaining);
            last_remaining = sqlite3_backup_remaining(b);
            if rc == DONE {
                break;
            }
            assert!(steps < 100_000);
        }
        // SQLite reports the page count only once stepping started.
        assert!(sqlite3_backup_pagecount(b) > 1);
        assert!(steps > 1, "expected several steps, got {steps}");
        assert_eq!(sqlite3_backup_remaining(b), 0);
        assert_eq!(sqlite3_backup_finish(b), OK);
        assert_eq!(ints(dst, c"SELECT count(*) FROM t"), vec![1500]);
        assert_eq!(
            ints(dst, c"SELECT id FROM t WHERE s LIKE '1499-%'"),
            vec![1500]
        );
        assert_eq!(ints(dst, c"PRAGMA user_version"), vec![7]);
        assert_eq!(
            ints(dst, c"SELECT count(*) FROM sqlite_master WHERE name='ti'"),
            vec![1]
        );
        assert_eq!(sqlite3_close(src), OK);
        assert_eq!(sqlite3_close(dst), OK);
    }
}

#[test]
fn backup_all_at_once_replaces_destination() {
    unsafe {
        let src = open();
        exec(src, c"CREATE TABLE a(x); INSERT INTO a VALUES (1),(2),(3);");
        let dst = open();
        exec(dst, c"CREATE TABLE old(y); INSERT INTO old VALUES (9);");
        let b = sqlite3_backup_init(dst, c"main".as_ptr(), src, c"main".as_ptr());
        assert!(!b.is_null());
        assert_eq!(sqlite3_backup_step(b, -1), DONE);
        assert_eq!(sqlite3_backup_finish(b), OK);
        assert_eq!(ints(dst, c"SELECT sum(x) FROM a"), vec![6]);
        assert_eq!(
            ints(dst, c"SELECT count(*) FROM sqlite_master WHERE name='old'"),
            vec![0]
        );
        sqlite3_close(src);
        sqlite3_close(dst);
    }
}

#[test]
fn backup_from_temp_schema() {
    unsafe {
        let src = open();
        exec(
            src,
            c"CREATE TEMP TABLE tt(x); INSERT INTO tt VALUES (4),(5);",
        );
        let dst = open();
        let b = sqlite3_backup_init(dst, c"main".as_ptr(), src, c"temp".as_ptr());
        assert!(!b.is_null());
        assert_eq!(sqlite3_backup_step(b, -1), DONE);
        assert_eq!(sqlite3_backup_finish(b), OK);
        assert_eq!(ints(dst, c"SELECT sum(x) FROM main.tt"), vec![9]);
        sqlite3_close(src);
        sqlite3_close(dst);
    }
}

#[test]
fn backup_init_errors() {
    unsafe {
        let a = open();
        let b = open();
        assert!(sqlite3_backup_init(a, c"main".as_ptr(), a, c"main".as_ptr()).is_null());
        assert_eq!(
            CStr::from_ptr(sqlite3_errmsg(a)),
            c"source and destination must be distinct"
        );
        assert!(sqlite3_backup_init(a, c"main".as_ptr(), b, c"nope".as_ptr()).is_null());
        assert_eq!(CStr::from_ptr(sqlite3_errmsg(a)), c"unknown database nope");
        sqlite3_close(a);
        sqlite3_close(b);
    }
}

#[test]
fn serialize_deserialize_round_trip() {
    unsafe {
        let src = open();
        exec(
            src,
            c"CREATE TABLE t(a, b); INSERT INTO t VALUES (1,'one'),(2,'two');",
        );
        let mut size = 0i64;
        let buf = sqlite3_serialize(src, c"main".as_ptr(), &mut size, 0);
        assert!(!buf.is_null());
        assert!(size >= 512);
        assert_eq!(std::slice::from_raw_parts(buf, 16), b"SQLite format 3\0");
        let dst = open();
        assert_eq!(
            sqlite3_deserialize(
                dst,
                c"main".as_ptr(),
                buf,
                size,
                size,
                SERIALIZE_FREEONCLOSE
            ),
            OK
        );
        assert_eq!(ints(dst, c"SELECT sum(a) FROM t"), vec![3]);
        sqlite3_close(src);
        sqlite3_close(dst);
    }
}
