//! C API added in SQLite 3.51 - 3.54: sqlite3_incomplete, sqlite3_set_errmsg,
//! sqlite3_db_status64, sqlite3_str_truncate/free, sqlite3_result_str, the new
//! sqlite3_limit ids, SQLITE_DBCONFIG_FP_DIGITS and SQLITE_CHECKPOINT_NOOP.
#![allow(non_camel_case_types)]
use std::ffi::{c_char, c_int, c_void, CStr, CString};
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
pub struct sqlite3_str {
    _private: [u8; 0],
}

type XFunc = unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_void);

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
    fn sqlite3_errcode(db: *mut sqlite3) -> c_int;
    fn sqlite3_extended_errcode(db: *mut sqlite3) -> c_int;
    fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char;
    fn sqlite3_prepare_v3(
        db: *mut sqlite3,
        sql: *const c_char,
        n: c_int,
        flags: u32,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_step(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_column_text(stmt: *mut sqlite3_stmt, idx: c_int) -> *const c_char;
    fn sqlite3_column_type(stmt: *mut sqlite3_stmt, idx: c_int) -> c_int;
    fn sqlite3_finalize(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_free(p: *mut c_void);
    fn sqlite3_complete(sql: *const c_char) -> c_int;
    fn sqlite3_complete16(sql: *const c_void) -> c_int;
    fn sqlite3_incomplete(sql: *const c_char) -> i64;
    fn sqlite3_set_errmsg(db: *mut sqlite3, code: c_int, msg: *const c_char) -> c_int;
    fn sqlite3_db_status64(
        db: *mut sqlite3,
        op: c_int,
        cur: *mut i64,
        hi: *mut i64,
        reset: c_int,
    ) -> c_int;
    fn sqlite3_limit(db: *mut sqlite3, id: c_int, new_value: c_int) -> c_int;
    fn sqlite3_db_config(db: *mut sqlite3, op: c_int, ...) -> c_int;
    fn sqlite3_wal_checkpoint_v2(
        db: *mut sqlite3,
        name: *const c_char,
        mode: c_int,
        log: *mut c_int,
        ckpt: *mut c_int,
    ) -> c_int;
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
    fn sqlite3_str_new(db: *mut sqlite3) -> *mut sqlite3_str;
    fn sqlite3_str_finish(p: *mut sqlite3_str) -> *mut c_char;
    fn sqlite3_str_free(p: *mut sqlite3_str);
    fn sqlite3_str_appendf(p: *mut sqlite3_str, fmt: *const c_char, ...);
    fn sqlite3_str_append(p: *mut sqlite3_str, z: *const c_char, n: c_int);
    fn sqlite3_str_appendall(p: *mut sqlite3_str, z: *const c_char);
    fn sqlite3_str_appendchar(p: *mut sqlite3_str, n: c_int, c: c_char);
    fn sqlite3_str_reset(p: *mut sqlite3_str);
    fn sqlite3_str_truncate(p: *mut sqlite3_str, n: c_int);
    fn sqlite3_str_errcode(p: *mut sqlite3_str) -> c_int;
    fn sqlite3_str_length(p: *mut sqlite3_str) -> c_int;
    fn sqlite3_str_value(p: *mut sqlite3_str) -> *mut c_char;
    fn sqlite3_result_str(ctx: *mut c_void, p: *mut sqlite3_str, own: c_int);
}

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_TOOBIG: c_int = 18;
const SQLITE_MISUSE: c_int = 21;
const SQLITE_ROW: c_int = 100;
const SQLITE_EMPTY: i64 = 16;
const SQLITE_UTF8: c_int = 1;
const SQLITE_COPY: c_int = 0;
const SQLITE_FINISH: c_int = 2;

unsafe fn open(path: &str) -> *mut sqlite3 {
    let mut db = ptr::null_mut();
    let path = CString::new(path).unwrap();
    assert_eq!(sqlite3_open(path.as_ptr(), &mut db), SQLITE_OK);
    db
}

unsafe fn exec(db: *mut sqlite3, sql: &str) {
    let sql = CString::new(sql).unwrap();
    let rc = sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "{:?}", CStr::from_ptr(sqlite3_errmsg(db)));
}

/// First column of the first row as text (None for NULL).
unsafe fn query_text(db: *mut sqlite3, sql: &str, flags: u32) -> Option<String> {
    let sql = CString::new(sql).unwrap();
    let mut stmt = ptr::null_mut();
    let rc = sqlite3_prepare_v3(db, sql.as_ptr(), -1, flags, &mut stmt, ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "{:?}", CStr::from_ptr(sqlite3_errmsg(db)));
    assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
    let out = if sqlite3_column_type(stmt, 0) == 5 {
        None
    } else {
        let p = sqlite3_column_text(stmt, 0);
        Some(CStr::from_ptr(p).to_str().unwrap().to_owned())
    };
    sqlite3_finalize(stmt);
    out
}

fn inc(sql: &str) -> i64 {
    let sql = CString::new(sql).unwrap();
    unsafe { sqlite3_incomplete(sql.as_ptr()) }
}

#[test]
fn incomplete_reports_what_is_missing() {
    assert_eq!(inc(""), SQLITE_EMPTY);
    assert_eq!(inc(" \n\t"), SQLITE_EMPTY);
    assert_eq!(inc("/* only a comment */ -- and another\n"), SQLITE_EMPTY);
    assert_eq!(inc("select 1;"), 0);
    assert_eq!(inc("select 1; -- trailing\n"), 0);
    assert_eq!(inc("select 1"), 0x101);
    assert_eq!(inc("select 'a;"), (b'\'' as i64) << 16 | 0x101);
    assert_eq!(inc("select \"a;"), (b'"' as i64) << 16 | 0x101);
    assert_eq!(inc("select [a;"), (b']' as i64) << 16 | 0x101);
    assert_eq!(inc("select `a;"), (b'`' as i64) << 16 | 0x101);
    assert_eq!(inc("select 1 /* x;"), (b'/' as i64) << 16 | 0x101);
    assert_eq!(inc("select 1 -- x;"), (b'-' as i64) << 16 | 0x101);
    assert_eq!(inc("/* x"), (b'/' as i64) << 16 | 1);
    assert_eq!(inc("select ((1)"), 1 << 32 | 0x101);
    assert_eq!(inc("select (1))"), (-1i64 << 32) | 0x101);
    assert_eq!(inc("select (1;"), 0);
    assert_eq!(inc("create trigger t after insert on x begin"), 0x301);
    assert_eq!(
        inc("create temp trigger t after insert on x begin select 1;"),
        0x201
    );
    assert_eq!(
        inc("create trigger t after insert on x begin select 1; end"),
        0x101
    );
    assert_eq!(
        inc("create trigger t after insert on x begin select 1; end;"),
        0
    );
    assert_eq!(
        inc("explain create trigger t after insert on x begin select 1;"),
        0x201
    );
    assert_eq!(inc("create table t(a);"), 0);
    // NULL gives SQLITE_MISUSE only with SQLITE_ENABLE_API_ARMOR, which
    // Wolfi's libsqlite3 lacks, so it is not exercised here.
}

#[test]
fn complete_follows_incomplete() {
    let cases = [
        ("select 1;", 1),
        ("select 1", 0),
        ("", 0),
        (";", 1),
        ("select 'a;'", 0),
        ("select 'a;';", 1),
        ("select 1; -- c", 0),
        ("select 1; -- c\n", 1),
        ("create trigger t after insert on x begin select 1;", 0),
        ("create trigger t after insert on x begin select 1; end;", 1),
    ];
    for (sql, want) in cases {
        let c = CString::new(sql).unwrap();
        let mut wide: Vec<u16> = sql.encode_utf16().collect();
        wide.push(0);
        unsafe {
            assert_eq!(sqlite3_complete(c.as_ptr()), want, "{sql}");
            assert_eq!(
                sqlite3_complete16(wide.as_ptr() as *const c_void),
                want,
                "{sql}"
            );
        }
    }
}

#[test]
fn set_errmsg_overrides_error_state() {
    unsafe {
        let db = open(":memory:");
        let msg = CString::new("custom failure").unwrap();
        assert_eq!(
            sqlite3_set_errmsg(db, 1 | (3 << 8), msg.as_ptr()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_extended_errcode(db), 1 | (3 << 8));
        assert_eq!(sqlite3_errcode(db), SQLITE_ERROR);
        assert_eq!(
            CStr::from_ptr(sqlite3_errmsg(db)).to_str().unwrap(),
            "custom failure"
        );
        // NULL message: the default text for the code.
        assert_eq!(
            sqlite3_set_errmsg(db, SQLITE_TOOBIG, ptr::null()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_errcode(db), SQLITE_TOOBIG);
        assert_eq!(
            CStr::from_ptr(sqlite3_errmsg(db)).to_str().unwrap(),
            "string or blob too big"
        );
        assert_eq!(sqlite3_set_errmsg(db, SQLITE_OK, ptr::null()), SQLITE_OK);
        assert_eq!(sqlite3_errcode(db), SQLITE_OK);
        assert_eq!(
            CStr::from_ptr(sqlite3_errmsg(db)).to_str().unwrap(),
            "not an error"
        );
        assert_eq!(
            sqlite3_set_errmsg(ptr::null_mut(), 1, ptr::null()),
            SQLITE_MISUSE
        );
        sqlite3_close(db);
    }
}

#[test]
fn db_status64_tempbuf_spill() {
    unsafe {
        let db = open(":memory:");
        let (mut cur, mut hi) = (-1i64, -1i64);
        // SQLITE_DBSTATUS_TEMPBUF_SPILL
        assert_eq!(sqlite3_db_status64(db, 13, &mut cur, &mut hi, 0), SQLITE_OK);
        assert_eq!((cur, hi), (0, 0));
        assert_eq!(
            sqlite3_db_status64(db, 14, &mut cur, &mut hi, 0),
            SQLITE_ERROR
        );
        sqlite3_close(db);
    }
}

#[test]
fn limit_ids_and_clamping() {
    unsafe {
        let db = open(":memory:");
        // 3.53 / 3.54 limits report their compile-time maxima.
        assert_eq!(sqlite3_limit(db, 12, -1), 2500); // PARSER_DEPTH
        assert_eq!(sqlite3_limit(db, 13, -1), 10_000_000); // SCHEMA
        assert_eq!(sqlite3_limit(db, 14, -1), 65_000); // TRIGGER_STEPS
        assert_eq!(sqlite3_limit(db, 15, -1), -1);
        assert_eq!(sqlite3_limit(db, -1, 5), -1);
        assert_eq!(sqlite3_limit(db, 0, -1), 1_000_000_000); // LENGTH
        assert_eq!(sqlite3_limit(db, 11, -1), 0); // WORKER_THREADS default
                                                  // Set returns the old value; values clamp to the hard limit.
        assert_eq!(sqlite3_limit(db, 14, 10), 65_000);
        assert_eq!(sqlite3_limit(db, 14, 1_000_000), 10);
        assert_eq!(sqlite3_limit(db, 14, -1), 65_000);
        assert_eq!(sqlite3_limit(db, 0, 5), 1_000_000_000);
        assert_eq!(sqlite3_limit(db, 0, -1), 30); // SQLITE_MIN_LENGTH
                                                  // Limits are per connection.
        let db2 = open(":memory:");
        assert_eq!(sqlite3_limit(db2, 0, -1), 1_000_000_000);
        sqlite3_close(db2);
        sqlite3_close(db);
    }
}

#[test]
fn fp_digits_reports_17() {
    unsafe {
        let db = open(":memory:");
        let mut out: c_int = -1;
        assert_eq!(
            sqlite3_db_config(db, 1023, 0 as c_int, &mut out as *mut c_int),
            SQLITE_OK
        );
        assert_eq!(out, 17);
        out = -1;
        assert_eq!(
            sqlite3_db_config(db, 1023, 17 as c_int, &mut out as *mut c_int),
            SQLITE_OK
        );
        assert_eq!(out, 17);
        // Out of range: left unchanged.
        out = -1;
        assert_eq!(
            sqlite3_db_config(db, 1023, 99 as c_int, &mut out as *mut c_int),
            SQLITE_OK
        );
        assert_eq!(out, 17);
        assert_eq!(
            sqlite3_db_config(db, 1023, 0 as c_int, ptr::null_mut::<c_int>()),
            SQLITE_OK
        );
        sqlite3_close(db);
    }
}

#[test]
fn str_builder() {
    unsafe {
        let p = sqlite3_str_new(ptr::null_mut());
        assert_eq!(sqlite3_str_errcode(p), SQLITE_OK);
        assert_eq!(sqlite3_str_length(p), 0);
        assert!(sqlite3_str_value(p).is_null());
        let hello = CString::new("hello").unwrap();
        sqlite3_str_append(p, hello.as_ptr(), 4);
        sqlite3_str_appendchar(p, 3, b'!' as c_char);
        let fmt = CString::new(" %d-%s-%q").unwrap();
        let arg = CString::new("it's").unwrap();
        sqlite3_str_appendf(p, fmt.as_ptr(), 42 as c_int, hello.as_ptr(), arg.as_ptr());
        let val = CStr::from_ptr(sqlite3_str_value(p)).to_str().unwrap();
        assert_eq!(val, "hell!!! 42-hello-it''s");
        assert_eq!(sqlite3_str_length(p), val.len() as c_int);
        sqlite3_str_truncate(p, 4);
        assert_eq!(
            CStr::from_ptr(sqlite3_str_value(p)).to_str().unwrap(),
            "hell"
        );
        sqlite3_str_truncate(p, 100);
        sqlite3_str_truncate(p, -1);
        assert_eq!(sqlite3_str_length(p), 4);
        sqlite3_str_appendall(p, hello.as_ptr());
        let z = sqlite3_str_finish(p);
        assert_eq!(CStr::from_ptr(z).to_str().unwrap(), "hellhello");
        sqlite3_free(z as *mut c_void);

        let p = sqlite3_str_new(ptr::null_mut());
        sqlite3_str_appendall(p, hello.as_ptr());
        sqlite3_str_reset(p);
        assert_eq!(sqlite3_str_length(p), 0);
        assert!(sqlite3_str_finish(p).is_null());

        let p = sqlite3_str_new(ptr::null_mut());
        sqlite3_str_appendall(p, hello.as_ptr());
        sqlite3_str_free(p);

        // The connection's SQLITE_LIMIT_LENGTH bounds the string.
        let db = open(":memory:");
        sqlite3_limit(db, 0, 30);
        let p = sqlite3_str_new(db);
        sqlite3_str_appendchar(p, 29, b'x' as c_char);
        assert_eq!(sqlite3_str_errcode(p), SQLITE_OK);
        sqlite3_str_appendchar(p, 100, b'x' as c_char);
        assert_eq!(sqlite3_str_errcode(p), SQLITE_TOOBIG);
        assert_eq!(sqlite3_str_length(p), 0);
        sqlite3_str_appendall(p, hello.as_ptr());
        assert_eq!(sqlite3_str_length(p), 0);
        assert!(sqlite3_str_finish(p).is_null());
        sqlite3_close(db);
    }
}

unsafe extern "C" fn str_func(ctx: *mut c_void, argc: c_int, _argv: *mut *mut c_void) {
    let p = sqlite3_str_new(ptr::null_mut());
    match argc {
        0 => {}
        1 => {
            let s = CString::new("copied").unwrap();
            sqlite3_str_appendall(p, s.as_ptr());
            sqlite3_result_str(ctx, p, SQLITE_COPY);
            // COPY leaves the content in place.
            assert_eq!(
                CStr::from_ptr(sqlite3_str_value(p)).to_str().unwrap(),
                "copied"
            );
            sqlite3_str_free(p);
            return;
        }
        _ => {
            let s = CString::new("x").unwrap();
            sqlite3_str_appendall(p, s.as_ptr());
            sqlite3_str_appendchar(p, 3, b'y' as c_char);
        }
    }
    sqlite3_result_str(ctx, p, SQLITE_FINISH);
}

#[test]
fn result_str_returns_dynamic_string() {
    unsafe {
        let db = open(":memory:");
        for n in [0, 1, 2] {
            let name = CString::new(format!("strf{n}")).unwrap();
            let rc = sqlite3_create_function_v2(
                db,
                name.as_ptr(),
                n,
                SQLITE_UTF8,
                ptr::null_mut(),
                Some(str_func),
                None,
                None,
                None,
            );
            assert_eq!(rc, SQLITE_OK);
        }
        assert_eq!(query_text(db, "select strf0()", 0).as_deref(), Some(""));
        assert_eq!(
            query_text(db, "select strf1(1)", 0).as_deref(),
            Some("copied")
        );
        assert_eq!(
            query_text(db, "select strf2(1, 2)", 0).as_deref(),
            Some("xyyy")
        );
        assert_eq!(
            query_text(db, "select typeof(strf0())", 0).as_deref(),
            Some("text")
        );
        sqlite3_close(db);
    }
}

#[test]
fn prepare_from_ddl_flag_is_accepted() {
    unsafe {
        let db = open(":memory:");
        // SQLITE_PREPARE_FROM_DDL
        assert_eq!(
            query_text(db, "select upper('a') || abs(-2)", 0x20).as_deref(),
            Some("A2")
        );
        sqlite3_close(db);
    }
}

#[test]
fn checkpoint_noop_reports_without_checkpointing() {
    unsafe {
        let dir = std::env::temp_dir().join(format!("sqlite354-noop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let db = open(path.to_str().unwrap());
        exec(
            db,
            "pragma journal_mode=wal; create table t(x); insert into t values(1);",
        );
        let (mut log, mut ckpt) = (-5, -5);
        let rc = sqlite3_wal_checkpoint_v2(db, ptr::null(), -1, &mut log, &mut ckpt);
        assert_eq!(rc, SQLITE_OK);
        assert!(log > 0, "log={log}");
        assert_eq!(ckpt, 0);
        let frames = log;
        // PASSIVE backfills; NOOP and a second PASSIVE report the same totals.
        assert_eq!(
            sqlite3_wal_checkpoint_v2(db, ptr::null(), 0, &mut log, &mut ckpt),
            SQLITE_OK
        );
        assert_eq!((log, ckpt), (frames, frames));
        assert_eq!(
            sqlite3_wal_checkpoint_v2(db, ptr::null(), -1, &mut log, &mut ckpt),
            SQLITE_OK
        );
        assert_eq!((log, ckpt), (frames, frames));
        assert_eq!(
            sqlite3_wal_checkpoint_v2(db, ptr::null(), 0, &mut log, &mut ckpt),
            SQLITE_OK
        );
        assert_eq!((log, ckpt), (frames, frames));
        assert_eq!(
            query_text(db, "pragma wal_checkpoint(noop)", 0).as_deref(),
            Some("0")
        );
        sqlite3_close(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
