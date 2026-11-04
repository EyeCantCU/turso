//! A statement run from a user function commits on the same connection
//! while the outer query is still stepping (SQLite's tkt-b72787b1). The
//! outer query must keep its read snapshot and return all of its rows.
//! DROP fails with SQLITE_LOCKED while another statement is active.

use super::*;
use std::cell::{Cell, RefCell};
use std::ffi::CString;

thread_local! {
    static DB: Cell<*mut sqlite3> = const { Cell::new(ptr::null_mut()) };
    static NESTED_SQL: RefCell<CString> = RefCell::new(CString::default());
    static MAX_X: Cell<*mut sqlite3_stmt> = const { Cell::new(ptr::null_mut()) };
    static NESTED_RC: Cell<i32> = const { Cell::new(-1) };
}

/// runsql(): run NESTED_SQL, then return `SELECT max(x) FROM t1`.
unsafe extern "C" fn runsql(ctx: *mut libc::c_void, _argc: i32, _argv: *mut *mut libc::c_void) {
    let db = DB.with(|d| d.get());
    NESTED_SQL.with(|sql| {
        let rc = sqlite3_exec(
            db,
            sql.borrow().as_ptr(),
            None,
            ptr::null_mut(),
            ptr::null_mut(),
        );
        NESTED_RC.with(|r| r.set(rc));
    });
    let stmt = MAX_X.with(|s| s.get());
    assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
    let max = sqlite3_column_int64(stmt, 0);
    sqlite3_reset(stmt);
    sqlite3_result_int64(ctx, max);
}

unsafe fn exec_ok(db: *mut sqlite3, sql: &str) {
    let c = CString::new(sql).unwrap();
    let rc = sqlite3_exec(db, c.as_ptr(), None, ptr::null_mut(), ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "{sql}");
}

unsafe fn column_i64s(db: *mut sqlite3, sql: &str) -> Vec<i64> {
    let c = CString::new(sql).unwrap();
    let mut stmt = ptr::null_mut();
    assert_eq!(
        sqlite3_prepare_v2(db, c.as_ptr(), -1, &mut stmt, ptr::null_mut()),
        SQLITE_OK,
        "{sql}"
    );
    let mut out = Vec::new();
    loop {
        match sqlite3_step(stmt) {
            SQLITE_ROW => out.push(sqlite3_column_int64(stmt, 0)),
            SQLITE_DONE => break,
            rc => panic!(
                "step {sql}: {rc} {:?}",
                std::ffi::CStr::from_ptr(sqlite3_errmsg(db))
            ),
        }
    }
    assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);
    out
}

/// Open a fresh WAL database with tkt-b72787b1's tables and `runsql()`.
unsafe fn setup(dir: &tempfile::TempDir, nested: &str) -> *mut sqlite3 {
    let path = CString::new(dir.path().join("n.db").to_str().unwrap()).unwrap();
    let mut db = ptr::null_mut();
    assert_eq!(sqlite3_open(path.as_ptr(), &mut db), SQLITE_OK);
    exec_ok(
        db,
        "PRAGMA journal_mode=WAL;
         CREATE TABLE t1(x); INSERT INTO t1 VALUES(1); INSERT INTO t1 VALUES(2);
         CREATE TABLE t2(y); CREATE TABLE t5(z); CREATE INDEX i5b ON t5(z);
         INSERT INTO t2 SELECT x+2 FROM t1; INSERT INTO t2 SELECT x+4 FROM t1;
         CREATE TABLE big(v); WITH RECURSIVE c(i) AS (SELECT 1 UNION ALL
           SELECT i+1 FROM c WHERE i<2000) INSERT INTO big SELECT i FROM c;",
    );
    assert_eq!(
        sqlite3_create_function_v2(
            db,
            c"runsql".as_ptr(),
            0,
            SQLITE_UTF8,
            ptr::null_mut(),
            Some(runsql),
            None,
            None,
            None,
        ),
        SQLITE_OK
    );
    let mut stmt = ptr::null_mut();
    assert_eq!(
        sqlite3_prepare_v2(
            db,
            c"SELECT max(x) FROM t1".as_ptr(),
            -1,
            &mut stmt,
            ptr::null_mut()
        ),
        SQLITE_OK
    );
    DB.with(|d| d.set(db));
    MAX_X.with(|s| s.set(stmt));
    NESTED_SQL.with(|s| *s.borrow_mut() = CString::new(nested).unwrap());
    NESTED_RC.with(|r| r.set(-1));
    db
}

unsafe fn teardown(db: *mut sqlite3) {
    sqlite3_finalize(MAX_X.with(|s| s.replace(ptr::null_mut())));
    // The outer query released its read transaction: TRUNCATE succeeds.
    let (mut log, mut ckpt) = (-1, -1);
    assert_eq!(
        sqlite3_wal_checkpoint_v2(db, ptr::null(), 3, &mut log, &mut ckpt),
        SQLITE_OK
    );
    assert_eq!(log, 0);
    assert_eq!(sqlite3_close(db), SQLITE_OK);
}

/// Nested statements that commit.
const NESTED: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS t4(q)",
    "CREATE INDEX IF NOT EXISTS i5 ON t5(z)",
    "ALTER TABLE t5 ADD COLUMN w",
    "ALTER TABLE t5 RENAME TO t6",
    "CREATE VIEW IF NOT EXISTS v1 AS SELECT 1",
    "INSERT INTO t5 VALUES(1)",
    "UPDATE t1 SET x = x",
    "DELETE FROM t5",
];

#[test]
fn nested_commit_keeps_outer_union_all_rows() {
    for nested in NESTED {
        unsafe {
            let dir = tempfile::tempdir().unwrap();
            let db = setup(&dir, nested);
            let rows = column_i64s(
                db,
                "SELECT CASE WHEN y=3 THEN y+100 WHEN y==4 THEN runsql()+200
                 ELSE 300+y END FROM t2 UNION ALL SELECT * FROM t1",
            );
            assert_eq!(NESTED_RC.with(|r| r.get()), SQLITE_OK, "{nested}");
            assert_eq!(rows, vec![103, 202, 305, 306, 1, 2], "nested {nested}");
            teardown(db);
        }
    }
}

#[test]
fn nested_commit_keeps_outer_scan_over_many_pages() {
    let drops = [("DROP TABLE IF EXISTS t5", "t5"), ("DROP INDEX i5b", "i5b")];
    let cases = NESTED
        .iter()
        .map(|sql| (*sql, None))
        .chain(drops.iter().map(|(sql, name)| (*sql, Some(*name))));
    for (nested, dropped) in cases {
        unsafe {
            let dir = tempfile::tempdir().unwrap();
            let db = setup(&dir, nested);
            let rows = column_i64s(db, "SELECT CASE WHEN v=1 THEN runsql() ELSE v END FROM big");
            let expected_rc = if dropped.is_some() { 6 } else { SQLITE_OK };
            assert_eq!(NESTED_RC.with(|r| r.get()), expected_rc, "{nested}");
            assert_eq!(rows.len(), 2000, "nested {nested}");
            assert_eq!(rows[1999], 2000, "nested {nested}");
            if let Some(name) = dropped {
                // SQLITE_LOCKED: the object is still there.
                let sql = format!("SELECT count(*) FROM sqlite_schema WHERE name = '{name}'");
                assert_eq!(column_i64s(db, &sql), vec![1], "{nested}");
            }
            teardown(db);
        }
    }
}
