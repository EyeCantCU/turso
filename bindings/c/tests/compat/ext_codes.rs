//! Extended result codes are masked to the primary code unless
//! sqlite3_extended_result_codes() is on, in step, reset, finalize, exec and
//! errcode; sqlite3_extended_errcode always has the extended code.
//! Must pass on turso and on SQLite.

use libc::{c_char, c_int};
use std::ffi::CString;
use std::ptr;

use super::{
    sqlite3, sqlite3_close, sqlite3_errcode, sqlite3_exec, sqlite3_finalize, sqlite3_open,
    sqlite3_prepare_v2, sqlite3_reset, sqlite3_step, sqlite3_stmt,
};

const SQLITE_CONSTRAINT: c_int = 19;
const NOTNULL: c_int = 19 | (5 << 8);
const PRIMARYKEY: c_int = 19 | (6 << 8);
const UNIQUE: c_int = 19 | (8 << 8);

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
    fn sqlite3_extended_errcode(db: *mut sqlite3) -> c_int;
    fn sqlite3_extended_result_codes(db: *mut sqlite3, onoff: c_int) -> c_int;
}

unsafe fn exec(db: *mut sqlite3, sql: &str) -> c_int {
    let sql = CString::new(sql).unwrap();
    sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut())
}

/// Runs `sql` as a statement and returns (step, reset, finalize) results.
unsafe fn run(db: *mut sqlite3, sql: &str) -> (c_int, c_int, c_int, c_int) {
    let sql = CString::new(sql).unwrap();
    let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
    assert_eq!(
        sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut::<*const c_char>()),
        0
    );
    let step = sqlite3_step(stmt);
    let errcode = sqlite3_errcode(db);
    let reset = sqlite3_reset(stmt);
    let fin = sqlite3_finalize(stmt);
    (step, errcode, reset, fin)
}

#[test]
fn test_extended_result_codes_masking() {
    unsafe {
        let mut db: *mut sqlite3 = ptr::null_mut();
        let path = CString::new(":memory:").unwrap();
        assert_eq!(sqlite3_open(path.as_ptr(), &mut db), 0);
        assert_eq!(
            exec(
                db,
                "CREATE TABLE t(a INTEGER PRIMARY KEY, b UNIQUE, c NOT NULL); \
                 INSERT INTO t VALUES (1, 1, 1);"
            ),
            0
        );
        let cases = [
            ("INSERT INTO t VALUES (2, 1, 1)", UNIQUE),
            ("INSERT INTO t VALUES (1, 2, 1)", PRIMARYKEY),
            ("INSERT INTO t VALUES (3, 3, NULL)", NOTNULL),
        ];
        for extended in [false, true] {
            assert_eq!(sqlite3_extended_result_codes(db, extended as c_int), 0);
            for (sql, full) in cases {
                let want = if extended { full } else { SQLITE_CONSTRAINT };
                assert_eq!(exec(db, sql), want, "exec {sql} extended={extended}");
                assert_eq!(sqlite3_errcode(db), want, "errcode {sql}");
                assert_eq!(sqlite3_extended_errcode(db), full, "extended_errcode {sql}");
                let (step, errcode, reset, fin) = run(db, sql);
                assert_eq!(step, want, "step {sql} extended={extended}");
                assert_eq!(errcode, want, "errcode after step {sql}");
                assert_eq!(reset, want, "reset {sql} extended={extended}");
                assert_eq!(fin, 0, "finalize after reset {sql}");
                assert_eq!(sqlite3_extended_errcode(db), full);
                let sqlc = CString::new(sql).unwrap();
                let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
                sqlite3_prepare_v2(db, sqlc.as_ptr(), -1, &mut stmt, ptr::null_mut::<*const c_char>());
                sqlite3_step(stmt);
                assert_eq!(sqlite3_finalize(stmt), want, "finalize {sql}");
            }
        }
        sqlite3_close(db);
    }
}

#[test]
fn test_extended_result_codes_masking_corrupt_vtab() {
    const CORRUPT: c_int = 11;
    const CORRUPT_VTAB: c_int = 11 | (1 << 8);
    unsafe {
        let mut db: *mut sqlite3 = ptr::null_mut();
        let path = CString::new(":memory:").unwrap();
        assert_eq!(sqlite3_open(path.as_ptr(), &mut db), 0);
        assert_eq!(
            exec(
                db,
                "CREATE VIRTUAL TABLE f USING fts3(x); \
                 INSERT INTO f VALUES ('hello world'); \
                 UPDATE f_segdir SET root = x'ff';"
            ),
            0
        );
        let q = "SELECT * FROM f WHERE f MATCH 'hello'";
        for extended in [false, true] {
            sqlite3_extended_result_codes(db, extended as c_int);
            let want = if extended { CORRUPT_VTAB } else { CORRUPT };
            let (step, errcode, reset, _) = run(db, q);
            assert_eq!(step, want, "step extended={extended}");
            assert_eq!(errcode, want);
            assert_eq!(reset, want);
            assert_eq!(exec(db, q), want, "exec extended={extended}");
            assert_eq!(sqlite3_extended_errcode(db), CORRUPT_VTAB);
        }
        sqlite3_close(db);
    }
}
