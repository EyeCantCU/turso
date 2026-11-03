//! Connection lifecycle: open failures, prepare outputs, close at exit, and
//! the panic safety net. Passes on turso and with `--features sqlite3`.
use super::*;
use std::ffi::{CStr, CString};

extern "C" {
    fn sqlite3_extended_errcode(db: *mut sqlite3) -> i32;
}

#[cfg(not(feature = "sqlite3"))]
extern "C" {
    fn turso_test_panic(db: *mut sqlite3) -> i32;
    fn turso_test_panic_stmt(stmt: *mut sqlite3_stmt) -> i32;
    fn turso_test_panic_ptr() -> *const libc::c_char;
    fn turso_test_panic_value(stmt: *mut sqlite3_stmt) -> i32;
}

const SQLITE_NOMEM: i32 = 7;
const SQLITE_PERM: i32 = 3;
const SQLITE_READONLY: i32 = 8;
const SQLITE_OPEN_READONLY: i32 = 0x1;

unsafe fn errmsg(db: *mut sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db))
        .to_string_lossy()
        .into_owned()
}

unsafe fn open_v2(path: &str, flags: i32) -> (i32, *mut sqlite3) {
    let c = CString::new(path).unwrap();
    let mut db = ptr::null_mut();
    let rc = sqlite3_open_v2(c.as_ptr(), &mut db, flags, ptr::null());
    (rc, db)
}

/// Assert a failed open returned a handle carrying `code`/`msg`, then close it.
unsafe fn expect_open_failure(path: &str, flags: i32, code: i32, msg: &str) {
    let (rc, db) = open_v2(path, flags);
    assert_eq!(rc, code, "{path} flags={flags:#x}");
    assert!(!db.is_null(), "failed open must return a handle");
    assert_eq!(sqlite3_errcode(db), code);
    assert_eq!(sqlite3_extended_errcode(db), code);
    assert_eq!(errmsg(db), msg);
    assert_eq!(sqlite3_close(db), SQLITE_OK);
}

#[test]
fn prepare_sets_outputs_on_error() {
    unsafe {
        let mut db = ptr::null_mut();
        assert_eq!(sqlite3_open(c":memory:".as_ptr(), &mut db), SQLITE_OK);
        let sql = c"SELEC nonsense";
        // Poison the outputs: they must be overwritten on failure.
        let mut stmt = ptr::dangling_mut::<sqlite3_stmt>();
        let mut tail = ptr::dangling::<libc::c_char>();
        let rc = sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, &mut tail);
        assert_eq!(rc, SQLITE_ERROR);
        assert!(stmt.is_null());
        let end = sql.as_ptr().add(sql.to_bytes().len());
        assert!(tail >= sql.as_ptr() && tail <= end, "tail not set");

        // Empty and comment-only SQL: OK with a NULL statement.
        for s in [c"", c"  ", c"-- c"] {
            stmt = ptr::dangling_mut::<sqlite3_stmt>();
            tail = ptr::null();
            assert_eq!(
                sqlite3_prepare_v2(db, s.as_ptr(), -1, &mut stmt, &mut tail),
                SQLITE_OK
            );
            assert!(stmt.is_null());
            assert!(!tail.is_null());
        }

        // v3 behaves the same.
        stmt = ptr::dangling_mut::<sqlite3_stmt>();
        tail = ptr::null();
        let rc = sqlite3_prepare_v3(db, sql.as_ptr(), -1, 0, &mut stmt, &mut tail);
        assert_eq!(rc, SQLITE_ERROR);
        assert!(stmt.is_null());
        assert!(!tail.is_null());

        // A NULL db still clears the outputs.
        stmt = ptr::dangling_mut::<sqlite3_stmt>();
        let rc = sqlite3_prepare_v2(ptr::null_mut(), sql.as_ptr(), -1, &mut stmt, &mut tail);
        assert_eq!(rc, SQLITE_MISUSE);
        assert!(stmt.is_null());
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn null_handle_accessors() {
    unsafe {
        assert_eq!(errmsg(ptr::null_mut()), "out of memory");
        assert_eq!(sqlite3_errcode(ptr::null_mut()), SQLITE_NOMEM);
        assert_eq!(sqlite3_extended_errcode(ptr::null_mut()), SQLITE_NOMEM);
        assert_eq!(sqlite3_close(ptr::null_mut()), SQLITE_OK);
    }
}

#[test]
fn failed_open_returns_handle_with_error() {
    unsafe {
        let msg = "unable to open database file";
        let mut db = ptr::null_mut();
        let rc = sqlite3_open(c"/nonexistent-dir/x.db".as_ptr(), &mut db);
        assert_eq!(rc, SQLITE_CANTOPEN);
        assert!(!db.is_null());
        assert_eq!(sqlite3_errcode(db), SQLITE_CANTOPEN);
        assert_eq!(errmsg(db), msg);
        // A failed-open handle cannot prepare statements.
        let mut stmt = ptr::null_mut();
        let rc = sqlite3_prepare_v2(db, c"SELECT 1".as_ptr(), -1, &mut stmt, ptr::null_mut());
        assert_ne!(rc, SQLITE_OK);
        assert!(stmt.is_null());
        assert_eq!(sqlite3_close(db), SQLITE_OK);

        let rw_create = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE;
        expect_open_failure("/nonexistent-dir/x.db", rw_create, SQLITE_CANTOPEN, msg);
    }
}

#[test]
fn open_without_create_fails_on_missing_file() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.db");
        let p = path.to_str().unwrap();
        let msg = "unable to open database file";
        expect_open_failure(p, SQLITE_OPEN_READWRITE, SQLITE_CANTOPEN, msg);
        expect_open_failure(p, SQLITE_OPEN_READONLY, SQLITE_CANTOPEN, msg);
        let rw = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_URI;
        for mode in ["ro", "rw"] {
            expect_open_failure(&format!("file:{p}?mode={mode}"), rw, SQLITE_CANTOPEN, msg);
        }
        assert!(!path.exists(), "a failed open must not create the file");

        // With CREATE the file is made.
        let (rc, db) = open_v2(p, SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE);
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(sqlite3_exec_ok(db, c"CREATE TABLE t(a)"), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
        assert!(path.exists());
    }
}

unsafe fn sqlite3_exec_ok(db: *mut sqlite3, sql: &CStr) -> i32 {
    sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut())
}

#[test]
fn uri_mode_ro_opens_read_only() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ro.db");
        let p = p.to_str().unwrap();
        let (rc, db) = open_v2(p, SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE);
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(sqlite3_exec_ok(db, c"CREATE TABLE t(a)"), SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);

        let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_URI;
        let (rc, db) = open_v2(&format!("file:{p}?mode=ro"), flags);
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(sqlite3_exec_ok(db, c"SELECT * FROM t"), SQLITE_OK);
        assert_eq!(
            sqlite3_exec_ok(db, c"INSERT INTO t VALUES (1)"),
            SQLITE_READONLY
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn uri_mode_rwc_requires_create_flag() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rwc.db");
        let uri = format!("file:{}?mode=rwc", path.to_str().unwrap());
        let msg = "access mode not allowed: rwc";
        expect_open_failure(
            &uri,
            SQLITE_OPEN_READWRITE | SQLITE_OPEN_URI,
            SQLITE_PERM,
            msg,
        );
        expect_open_failure(
            &uri,
            SQLITE_OPEN_READONLY | SQLITE_OPEN_URI,
            SQLITE_PERM,
            msg,
        );
        assert!(!path.exists());

        let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_URI;
        let (rc, db) = open_v2(&uri, flags);
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
        assert!(path.exists());
    }
}

#[cfg(all(not(feature = "sqlite3"), panic = "unwind"))]
mod panics {
    use super::*;

    #[test]
    fn panic_on_handle_becomes_error_code() {
        unsafe {
            let mut db = ptr::null_mut();
            assert_eq!(sqlite3_open(c":memory:".as_ptr(), &mut db), SQLITE_OK);
            assert_eq!(turso_test_panic(db), 2);
            assert_eq!(sqlite3_errcode(db), 2);
            assert!(errmsg(db).contains("injected panic"), "{}", errmsg(db));
            // The connection stays usable.
            assert_eq!(sqlite3_exec_ok(db, c"CREATE TABLE t(a)"), SQLITE_OK);
            assert_eq!(sqlite3_close(db), SQLITE_OK);
        }
    }

    #[test]
    fn panic_on_stmt_records_error_on_its_db() {
        unsafe {
            let mut db = ptr::null_mut();
            assert_eq!(sqlite3_open(c":memory:".as_ptr(), &mut db), SQLITE_OK);
            let mut stmt = ptr::null_mut();
            let rc = sqlite3_prepare_v2(db, c"SELECT 1".as_ptr(), -1, &mut stmt, ptr::null_mut());
            assert_eq!(rc, SQLITE_OK);
            assert_eq!(turso_test_panic_stmt(stmt), 2);
            assert!(errmsg(db).contains("injected panic"));
            assert_eq!(turso_test_panic_value(stmt), 0);
            assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);
            assert_eq!(sqlite3_close(db), SQLITE_OK);
        }
    }

    #[test]
    fn panic_without_handle_returns_null() {
        unsafe {
            assert!(turso_test_panic_ptr().is_null());
            // NULL handles are tolerated.
            assert_eq!(turso_test_panic(ptr::null_mut()), 2);
        }
    }
}

/// A read statement left mid-run before BEGIN must not end the explicit
/// transaction when it is finalized (Qt does this).
#[test]
fn finalizing_unfinished_reader_keeps_explicit_transaction() {
    unsafe {
        let (rc, db) = open_v2(":memory:", 0x6);
        assert_eq!(rc, SQLITE_OK);
        let exec =
            |sql: &CStr| sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut());
        assert_eq!(
            exec(c"CREATE TABLE m(x); INSERT INTO m VALUES (1),(2)"),
            SQLITE_OK
        );
        let mut sel = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(
                db,
                c"SELECT x FROM m".as_ptr(),
                -1,
                &mut sel,
                ptr::null_mut()
            ),
            SQLITE_OK
        );
        assert_eq!(sqlite3_step(sel), SQLITE_ROW);
        assert_eq!(exec(c"BEGIN"), SQLITE_OK);
        assert_eq!(sqlite3_finalize(sel), SQLITE_OK);
        assert_eq!(sqlite3_get_autocommit(db), 0);
        assert_eq!(exec(c"DELETE FROM m"), SQLITE_OK);
        assert_eq!(exec(c"ROLLBACK"), SQLITE_OK);
        let mut cnt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(
                db,
                c"SELECT count(*) FROM m".as_ptr(),
                -1,
                &mut cnt,
                ptr::null_mut()
            ),
            SQLITE_OK
        );
        assert_eq!(sqlite3_step(cnt), SQLITE_ROW);
        assert_eq!(sqlite3_column_int(cnt, 0), 2);
        sqlite3_finalize(cnt);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}
