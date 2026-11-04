//! Several write statements active at once on one connection, interleaved
//! by the application. Every test runs against turso and, with
//! `--features sqlite3`, SQLite.

use super::*;
use std::ffi::{CStr, CString};

const SQLITE_CONSTRAINT: i32 = 19;
const SQLITE_BUSY: i32 = 5;

unsafe fn open_db(dir: &tempfile::TempDir) -> *mut sqlite3 {
    let path = CString::new(dir.path().join("il.db").to_str().unwrap()).unwrap();
    let mut db = ptr::null_mut();
    assert_eq!(sqlite3_open(path.as_ptr(), &mut db), SQLITE_OK);
    exec(db, "PRAGMA journal_mode=WAL");
    db
}

unsafe fn try_exec(db: *mut sqlite3, sql: &str) -> i32 {
    let sql = CString::new(sql).unwrap();
    sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut())
}

unsafe fn exec(db: *mut sqlite3, sql: &str) {
    let rc = try_exec(db, sql);
    assert_eq!(rc, SQLITE_OK, "exec {sql:?}: {}", errmsg(db));
}

unsafe fn errmsg(db: *mut sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db))
        .to_string_lossy()
        .into_owned()
}

unsafe fn prepare(db: *mut sqlite3, sql: &str) -> *mut sqlite3_stmt {
    let sql = CString::new(sql).unwrap();
    let mut stmt = ptr::null_mut();
    let rc = sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "prepare {sql:?}: {}", errmsg(db));
    stmt
}

/// Steps once and returns column 0 as text, or None on SQLITE_DONE.
unsafe fn next(stmt: *mut sqlite3_stmt) -> Option<String> {
    match sqlite3_step(stmt) {
        SQLITE_ROW => {
            let p = sqlite3_column_text(stmt, 0);
            Some(if p.is_null() {
                "NULL".into()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            })
        }
        SQLITE_DONE => None,
        rc => panic!("step failed: {rc}"),
    }
}

/// Steps `stmt` to the end and returns the remaining rows.
unsafe fn drain(stmt: *mut sqlite3_stmt) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(row) = next(stmt) {
        out.push(row);
    }
    out
}

/// All rows of `sql`, each row's columns joined by ':'.
unsafe fn rows(db: *mut sqlite3, sql: &str) -> Vec<String> {
    let stmt = prepare(db, sql);
    let mut out = Vec::new();
    loop {
        match sqlite3_step(stmt) {
            SQLITE_ROW => {
                let n = sqlite3_column_count(stmt);
                let cols: Vec<String> = (0..n)
                    .map(|i| {
                        let p = sqlite3_column_text(stmt, i);
                        if p.is_null() {
                            "NULL".into()
                        } else {
                            CStr::from_ptr(p).to_string_lossy().into_owned()
                        }
                    })
                    .collect();
                out.push(cols.join(":"));
            }
            SQLITE_DONE => break,
            rc => panic!("query {sql:?} failed: {rc} {}", errmsg(db)),
        }
    }
    assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);
    out
}

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// A connection with t(a INTEGER PRIMARY KEY, b UNIQUE) holding 1:1..3:3,
/// and a second connection on the same file to observe commits.
unsafe fn setup(dir: &tempfile::TempDir) -> (*mut sqlite3, *mut sqlite3) {
    let db = open_db(dir);
    exec(db, "CREATE TABLE t(a INTEGER PRIMARY KEY, b UNIQUE)");
    exec(db, "INSERT INTO t VALUES (1, 1), (2, 2), (3, 3)");
    let other = open_db(dir);
    (db, other)
}

#[test]
fn test_returning_rows_interleaved_with_updates() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let (db, other) = setup(&dir);
        let del = prepare(db, "DELETE FROM t WHERE a <= 2 RETURNING a");
        assert_eq!(next(del).as_deref(), Some("1"));
        // A second writer runs while the DELETE is still active. It joins
        // the DELETE's transaction, so nothing commits yet.
        exec(db, "UPDATE t SET b = b + 10");
        assert_eq!(sqlite3_get_autocommit(db), 1);
        assert_eq!(rows(db, "SELECT a, b FROM t"), v(&["3:13"]));
        assert_eq!(rows(other, "SELECT a, b FROM t"), v(&["1:1", "2:2", "3:3"]));
        // The transaction commits when the last writer finishes.
        assert_eq!(drain(del), v(&["2"]));
        assert_eq!(rows(other, "SELECT a, b FROM t"), v(&["3:13"]));
        assert_eq!(sqlite3_finalize(del), SQLITE_OK);
        sqlite3_close(other);
        sqlite3_close(db);
    }
}

#[test]
fn test_autocommit_commits_when_last_writer_finishes() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let (db, other) = setup(&dir);
        let a = prepare(db, "INSERT INTO t(b) VALUES (10), (11) RETURNING b");
        let b = prepare(db, "INSERT INTO t(b) VALUES (20), (21) RETURNING b");
        assert_eq!(next(a).as_deref(), Some("10"));
        assert_eq!(next(b).as_deref(), Some("20"));
        // The first writer finishes, but the second still shares the
        // transaction: no commit.
        assert_eq!(drain(a), v(&["11"]));
        assert_eq!(sqlite3_finalize(a), SQLITE_OK);
        assert_eq!(rows(other, "SELECT count(*) FROM t"), v(&["3"]));
        assert_eq!(rows(db, "SELECT count(*) FROM t"), v(&["7"]));
        // Resetting the last writer after a row ends it and commits.
        assert_eq!(sqlite3_reset(b), SQLITE_OK);
        assert_eq!(rows(other, "SELECT count(*) FROM t"), v(&["7"]));
        assert_eq!(sqlite3_finalize(b), SQLITE_OK);
        // A reader does not hold the commit back.
        let sel = prepare(db, "SELECT a FROM t ORDER BY a");
        assert_eq!(next(sel).as_deref(), Some("1"));
        exec(db, "DELETE FROM t WHERE a > 3");
        assert_eq!(rows(other, "SELECT count(*) FROM t"), v(&["3"]));
        assert_eq!(drain(sel), v(&["2", "3"]));
        assert_eq!(sqlite3_finalize(sel), SQLITE_OK);
        sqlite3_close(other);
        sqlite3_close(db);
    }
}

#[test]
fn test_failing_writer_rolls_back_only_itself() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let (db, other) = setup(&dir);
        let ins = prepare(db, "INSERT INTO t(b) VALUES (10), (11) RETURNING b");
        assert_eq!(next(ins).as_deref(), Some("10"));
        // b=5 for every row: the second row violates UNIQUE. Only this
        // UPDATE is undone; the INSERT's rows stay.
        assert_eq!(try_exec(db, "UPDATE t SET b = 5"), SQLITE_CONSTRAINT);
        assert_eq!(
            rows(db, "SELECT a, b FROM t"),
            v(&["1:1", "2:2", "3:3", "4:10", "5:11"])
        );
        assert_eq!(rows(other, "SELECT count(*) FROM t"), v(&["3"]));
        // A successful writer after the failed one keeps its changes.
        exec(db, "UPDATE t SET b = b * 100 WHERE a = 1");
        assert_eq!(drain(ins), v(&["11"]));
        assert_eq!(sqlite3_finalize(ins), SQLITE_OK);
        assert_eq!(
            rows(other, "SELECT a, b FROM t"),
            v(&["1:100", "2:2", "3:3", "4:10", "5:11"])
        );

        // The same inside an explicit transaction.
        exec(db, "BEGIN");
        let ins = prepare(db, "INSERT INTO t(b) VALUES (30), (31) RETURNING b");
        assert_eq!(next(ins).as_deref(), Some("30"));
        assert_eq!(try_exec(db, "UPDATE t SET b = 7"), SQLITE_CONSTRAINT);
        assert_eq!(sqlite3_get_autocommit(db), 0);
        assert_eq!(drain(ins), v(&["31"]));
        assert_eq!(sqlite3_finalize(ins), SQLITE_OK);
        exec(db, "COMMIT");
        assert_eq!(
            rows(other, "SELECT a, b FROM t"),
            v(&["1:100", "2:2", "3:3", "4:10", "5:11", "6:30", "7:31"])
        );
        sqlite3_close(other);
        sqlite3_close(db);
    }
}

#[test]
fn test_update_loop_driven_by_returning_writer() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let (db, other) = setup(&dir);
        exec(db, "CREATE TABLE log(a, note)");
        let upd = prepare(db, "UPDATE t SET b = b + 100 RETURNING a");
        let mut seen = Vec::new();
        while let Some(a) = next(upd) {
            // Write the same table and another one for every returned row.
            exec(db, &format!("UPDATE t SET b = b + 1000 WHERE a = {a}"));
            exec(db, &format!("INSERT INTO log VALUES ({a}, 'x')"));
            seen.push(a);
        }
        assert_eq!(seen, v(&["1", "2", "3"]));
        assert_eq!(sqlite3_finalize(upd), SQLITE_OK);
        assert_eq!(
            rows(other, "SELECT a, b FROM t"),
            v(&["1:1101", "2:1102", "3:1103"])
        );
        assert_eq!(rows(other, "SELECT a FROM log"), v(&["1", "2", "3"]));
        sqlite3_close(other);
        sqlite3_close(db);
    }
}

#[test]
fn test_reader_cursor_survives_interleaved_writes() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        exec(db, "CREATE TABLE t(a INTEGER PRIMARY KEY, b UNIQUE)");
        exec(
            db,
            "INSERT INTO t VALUES (1, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6)",
        );
        let sel = prepare(db, "SELECT a FROM t ORDER BY a");
        assert_eq!(next(sel).as_deref(), Some("1"));
        // Rows deleted ahead of the cursor are not seen, rows inserted
        // ahead of it are.
        exec(db, "DELETE FROM t WHERE a = 3");
        exec(db, "INSERT INTO t VALUES (10, 10)");
        assert_eq!(next(sel).as_deref(), Some("2"));
        // A failed multi-row writer is rolled back without disturbing the
        // scan.
        assert_eq!(try_exec(db, "UPDATE t SET b = 0"), SQLITE_CONSTRAINT);
        assert_eq!(drain(sel), v(&["4", "5", "6", "10"]));
        assert_eq!(sqlite3_finalize(sel), SQLITE_OK);
        sqlite3_close(db);
    }
}

#[test]
fn test_commit_and_savepoint_with_active_writer() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let (db, other) = setup(&dir);
        exec(db, "BEGIN");
        let ins = prepare(db, "INSERT INTO t(b) VALUES (10), (11) RETURNING b");
        assert_eq!(next(ins).as_deref(), Some("10"));
        assert_eq!(try_exec(db, "COMMIT"), SQLITE_BUSY);
        assert_eq!(
            errmsg(db),
            "cannot commit transaction - SQL statements in progress"
        );
        assert_eq!(try_exec(db, "SAVEPOINT s"), SQLITE_BUSY);
        assert_eq!(
            errmsg(db),
            "cannot open savepoint - SQL statements in progress"
        );
        assert_eq!(sqlite3_get_autocommit(db), 0);
        // A second writer is fine.
        exec(db, "UPDATE t SET b = b + 100 WHERE a = 1");
        assert_eq!(drain(ins), v(&["11"]));
        assert_eq!(sqlite3_finalize(ins), SQLITE_OK);
        exec(db, "COMMIT");
        assert_eq!(
            rows(other, "SELECT a, b FROM t"),
            v(&["1:101", "2:2", "3:3", "4:10", "5:11"])
        );

        // COMMIT with only a reader active succeeds.
        exec(db, "BEGIN");
        exec(db, "INSERT INTO t(b) VALUES (12)");
        let sel = prepare(db, "SELECT a FROM t ORDER BY a");
        assert_eq!(next(sel).as_deref(), Some("1"));
        exec(db, "COMMIT");
        assert_eq!(rows(other, "SELECT count(*) FROM t"), v(&["6"]));
        assert_eq!(sqlite3_finalize(sel), SQLITE_OK);
        sqlite3_close(other);
        sqlite3_close(db);
    }
}

#[test]
fn test_interleaved_writers_in_explicit_transaction_roll_back_together() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let (db, other) = setup(&dir);
        exec(db, "BEGIN");
        let a = prepare(db, "INSERT INTO t(b) VALUES (10), (11) RETURNING b");
        let b = prepare(db, "DELETE FROM t WHERE a <= 2 RETURNING a");
        assert_eq!(next(a).as_deref(), Some("10"));
        assert_eq!(next(b).as_deref(), Some("1"));
        // Closing the statements out of order is fine.
        assert_eq!(sqlite3_finalize(a), SQLITE_OK);
        assert_eq!(sqlite3_finalize(b), SQLITE_OK);
        assert_eq!(rows(db, "SELECT a FROM t"), v(&["3", "4", "5"]));
        exec(db, "ROLLBACK");
        assert_eq!(rows(db, "SELECT a FROM t"), v(&["1", "2", "3"]));
        assert_eq!(rows(other, "SELECT a FROM t"), v(&["1", "2", "3"]));
        sqlite3_close(other);
        sqlite3_close(db);
    }
}

extern "C" {
    fn sqlite3_user_data(ctx: *mut libc::c_void) -> *mut libc::c_void;
}

/// `swallow(sql)`: runs `sql` on the function's own connection and returns
/// its result code, ignoring any error.
unsafe extern "C" fn swallow_fn(ctx: *mut libc::c_void, _argc: i32, argv: *mut *mut libc::c_void) {
    let db = sqlite3_user_data(ctx) as *mut sqlite3;
    let sql = CStr::from_ptr(sqlite3_value_text(*argv)).to_owned();
    let rc = sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut());
    sqlite3_result_int(ctx, rc);
}

#[test]
fn test_failed_statement_inside_callback_rolls_back_alone() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let (db, other) = setup(&dir);
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                c"swallow".as_ptr(),
                1,
                SQLITE_UTF8,
                db as *mut libc::c_void,
                Some(swallow_fn),
                None,
                None,
                None,
            ),
            SQLITE_OK
        );
        exec(db, "CREATE TABLE log(x UNIQUE)");
        exec(db, "INSERT INTO log VALUES (2)");
        // The trigger's statement writes 1, then fails on 2; the function
        // ignores the error, so only that statement is undone.
        exec(
            db,
            "CREATE TRIGGER t_del AFTER DELETE ON t BEGIN \
             SELECT swallow('INSERT INTO log VALUES (1), (2)'); END",
        );
        for explicit in [false, true] {
            if explicit {
                exec(db, "BEGIN");
            }
            exec(db, "DELETE FROM t WHERE a = (SELECT min(a) FROM t)");
            assert_eq!(rows(db, "SELECT x FROM log"), v(&["2"]));
            if explicit {
                exec(db, "COMMIT");
            }
        }
        assert_eq!(rows(other, "SELECT a FROM t"), v(&["3"]));
        assert_eq!(rows(other, "SELECT x FROM log"), v(&["2"]));

        // The scan that called the function keeps its position across the
        // failed statement's rollback (tkt3718).
        exec(db, "INSERT INTO t VALUES (4, 4), (5, 5)");
        for explicit in [false, true] {
            if explicit {
                exec(db, "BEGIN");
            }
            assert_eq!(
                rows(
                    db,
                    "SELECT a, swallow(CASE a WHEN 4 THEN 'INSERT INTO log VALUES (1), (2)' \
                     ELSE 'SELECT 1' END) FROM t"
                ),
                v(&["3:0", "4:19", "5:0"])
            );
            if explicit {
                exec(db, "COMMIT");
            }
        }
        sqlite3_close(other);
        sqlite3_close(db);
    }
}

#[test]
fn test_scan_ends_when_another_statement_clears_the_table() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        exec(db, "CREATE TABLE t(x)");
        for order in ["DESC", "ASC"] {
            exec(db, "INSERT INTO t VALUES (1), (2), (3), (4)");
            let sel = prepare(db, &format!("SELECT x FROM t ORDER BY rowid {order}"));
            assert!(next(sel).is_some());
            exec(db, "DELETE FROM t");
            assert_eq!(drain(sel), Vec::<String>::new());
            assert_eq!(sqlite3_finalize(sel), SQLITE_OK);
            assert_eq!(rows(db, "SELECT count(*) FROM t"), v(&["0"]));
        }
        sqlite3_close(db);
    }
}

#[test]
fn test_reader_continues_after_rollback() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_db(&dir);
        exec(db, "CREATE TABLE t(a INTEGER PRIMARY KEY, b)");
        exec(
            db,
            "INSERT INTO t VALUES (1, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6), (7, 7), (8, 8)",
        );
        // ROLLBACK keeps a reader's position; it continues on the restored
        // rows, as with SQLite.
        for (order, expected) in [
            ("ASC", v(&["2", "3", "4", "5", "6", "7", "8"])),
            ("DESC", v(&["8", "7", "6", "5", "4", "3", "2", "1"])),
        ] {
            exec(db, "BEGIN");
            exec(db, "DELETE FROM t WHERE a % 2 = 1");
            let sel = prepare(db, &format!("SELECT a FROM t ORDER BY a {order}"));
            let mut got = vec![next(sel).unwrap()];
            exec(db, "ROLLBACK");
            got.extend(drain(sel));
            assert_eq!(got, expected, "{order}");
            assert_eq!(sqlite3_finalize(sel), SQLITE_OK);
        }
        sqlite3_close(db);
    }
}
