//! Nested statements run on the same connection from inside a callback.
//! Every test runs against turso and, with `--features sqlite3`, SQLite.

use super::*;
use std::ffi::{CStr, CString};

extern "C" {
    fn sqlite3_user_data(ctx: *mut libc::c_void) -> *mut libc::c_void;
    fn sqlite3_result_error(ctx: *mut libc::c_void, msg: *const libc::c_char, n: i32);
    fn sqlite3_create_collation_v2(
        db: *mut sqlite3,
        name: *const libc::c_char,
        enc: i32,
        arg: *mut libc::c_void,
        cmp: Option<
            unsafe extern "C" fn(
                *mut libc::c_void,
                i32,
                *const libc::c_void,
                i32,
                *const libc::c_void,
            ) -> i32,
        >,
        destroy: Option<unsafe extern "C" fn(*mut libc::c_void)>,
    ) -> i32;
}

unsafe fn open_temp(dir: &tempfile::TempDir) -> *mut sqlite3 {
    let path = CString::new(dir.path().join("re.db").to_str().unwrap()).unwrap();
    let mut db = ptr::null_mut();
    assert_eq!(sqlite3_open(path.as_ptr(), &mut db), SQLITE_OK);
    db
}

unsafe fn exec(db: *mut sqlite3, sql: &str) {
    let sql = CString::new(sql).unwrap();
    let mut err = ptr::null_mut();
    let rc = sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), &mut err);
    if rc != SQLITE_OK {
        let msg = CStr::from_ptr(sqlite3_errmsg(db)).to_string_lossy();
        panic!("exec {sql:?} failed: {rc} {msg}");
    }
}

/// Runs `sql` and returns the first column of every row as text, or the
/// error code of the first failing call.
unsafe fn query(db: *mut sqlite3, sql: &str) -> Result<Vec<String>, i32> {
    let sql = CString::new(sql).unwrap();
    let mut stmt = ptr::null_mut();
    let rc = sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut());
    if rc != SQLITE_OK {
        return Err(rc);
    }
    let mut out = Vec::new();
    loop {
        match sqlite3_step(stmt) {
            SQLITE_ROW => {
                let p = sqlite3_column_text(stmt, 0);
                out.push(if p.is_null() {
                    "NULL".into()
                } else {
                    CStr::from_ptr(p).to_string_lossy().into_owned()
                });
            }
            SQLITE_DONE => break,
            rc => {
                sqlite3_finalize(stmt);
                return Err(rc);
            }
        }
    }
    assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);
    Ok(out)
}

/// `nested(sql)`: runs `sql` on the function's own connection and returns
/// its rows joined by ',' (or raises the error code as the message).
unsafe extern "C" fn nested_fn(ctx: *mut libc::c_void, _argc: i32, argv: *mut *mut libc::c_void) {
    let db = sqlite3_user_data(ctx) as *mut sqlite3;
    let sql = CStr::from_ptr(sqlite3_value_text(*argv))
        .to_string_lossy()
        .into_owned();
    match query(db, &sql) {
        Ok(rows) => {
            let s = CString::new(rows.join(",")).unwrap();
            sqlite3_result_text(ctx, s.as_ptr(), -1, SQLITE_TRANSIENT());
        }
        Err(rc) => {
            let s = CString::new(format!("nested rc={rc}")).unwrap();
            sqlite3_result_error(ctx, s.as_ptr(), -1);
        }
    }
}

#[allow(non_snake_case)]
fn SQLITE_TRANSIENT() -> Option<unsafe extern "C" fn(*mut libc::c_void)> {
    // SQLITE_TRANSIENT is ((sqlite3_destructor_type)-1).
    unsafe { std::mem::transmute::<isize, _>(-1) }
}

extern "C" {
    fn sqlite3_result_text(
        ctx: *mut libc::c_void,
        text: *const libc::c_char,
        n: i32,
        destructor: Option<unsafe extern "C" fn(*mut libc::c_void)>,
    );
}

unsafe fn register_nested(db: *mut sqlite3) {
    assert_eq!(
        sqlite3_create_function_v2(
            db,
            c"nested".as_ptr(),
            1,
            SQLITE_UTF8,
            db as *mut libc::c_void,
            Some(nested_fn),
            None,
            None,
            None,
        ),
        SQLITE_OK
    );
}

#[test]
fn test_udf_nested_read_and_write() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_temp(&dir);
        register_nested(db);
        exec(db, "CREATE TABLE t(a); CREATE TABLE log(x)");
        exec(db, "INSERT INTO t VALUES (1), (2), (3)");

        // Autocommit read: the nested statement reads a table and writes
        // another one while the outer SELECT is mid-step.
        let rows = query(
            db,
            "SELECT a, nested('SELECT count(*) FROM t'), \
             nested('INSERT INTO log VALUES (' || a || ') RETURNING x') FROM t",
        )
        .unwrap();
        assert_eq!(rows, ["1", "2", "3"]);
        assert_eq!(
            query(db, "SELECT x FROM log ORDER BY x").unwrap(),
            ["1", "2", "3"]
        );

        // Inside an explicit transaction the nested statement sees the
        // outer transaction's uncommitted rows and joins that transaction.
        exec(db, "BEGIN; INSERT INTO t VALUES (4)");
        assert_eq!(
            query(db, "SELECT nested('SELECT group_concat(a) FROM t')").unwrap(),
            ["1,2,3,4"]
        );
        assert_eq!(
            query(
                db,
                "SELECT nested('INSERT INTO log VALUES (40) RETURNING x')"
            )
            .unwrap(),
            ["40"]
        );
        assert_eq!(sqlite3_get_autocommit(db), 0);
        exec(db, "ROLLBACK");
        assert_eq!(query(db, "SELECT count(*) FROM t").unwrap(), ["3"]);
        assert_eq!(query(db, "SELECT count(*) FROM log").unwrap(), ["3"]);

        // An outer write statement: the nested reads see the rows the
        // outer statement has written so far.
        exec(db, "DELETE FROM log");
        let rows = query(
            db,
            "INSERT INTO log SELECT nested('SELECT count(*) FROM log') FROM t RETURNING x",
        )
        .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], "0");
        assert_eq!(sqlite3_get_autocommit(db), 1);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_trigger_udf_nested_statements() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_temp(&dir);
        register_nested(db);
        exec(
            db,
            "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT); \
             CREATE TABLE log(x); \
             CREATE TRIGGER t_del AFTER DELETE ON t BEGIN \
               SELECT nested('INSERT INTO log SELECT ''' || old.name || ':'' || count(*) FROM t'); \
             END; \
             INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        );

        // Autocommit DELETE: the trigger's UDF writes another table and
        // reads the table being deleted from, seeing the outer deletion.
        exec(db, "DELETE FROM t WHERE id = 1");
        assert_eq!(query(db, "SELECT x FROM log").unwrap(), ["a:2"]);

        // Same inside an explicit transaction, then rolled back as a unit.
        exec(db, "BEGIN; DELETE FROM t");
        assert_eq!(query(db, "SELECT count(*) FROM log").unwrap(), ["3"]);
        exec(db, "ROLLBACK");
        assert_eq!(query(db, "SELECT x FROM log").unwrap(), ["a:2"]);
        assert_eq!(query(db, "SELECT count(*) FROM t").unwrap(), ["2"]);

        // Committed as a unit too.
        exec(db, "BEGIN; DELETE FROM t WHERE id = 2; COMMIT");
        assert_eq!(
            query(db, "SELECT x FROM log ORDER BY rowid").unwrap(),
            ["a:2", "b:1"]
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);

        // The writes are durable on disk.
        let db = open_temp(&dir);
        assert_eq!(
            query(db, "SELECT x FROM log ORDER BY rowid").unwrap(),
            ["a:2", "b:1"]
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

unsafe extern "C" fn nested_collation(
    arg: *mut libc::c_void,
    n1: i32,
    p1: *const libc::c_void,
    n2: i32,
    p2: *const libc::c_void,
) -> i32 {
    // A collation that consults the same connection on every compare.
    let db = arg as *mut sqlite3;
    assert_eq!(query(db, "SELECT count(*) FROM t").unwrap().len(), 1);
    let a = std::slice::from_raw_parts(p1 as *const u8, n1 as usize);
    let b = std::slice::from_raw_parts(p2 as *const u8, n2 as usize);
    b.cmp(a) as i32
}

#[test]
fn test_collation_nested_read() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_temp(&dir);
        assert_eq!(
            sqlite3_create_collation_v2(
                db,
                c"rev".as_ptr(),
                SQLITE_UTF8,
                db as *mut libc::c_void,
                Some(nested_collation),
                None,
            ),
            SQLITE_OK
        );
        exec(
            db,
            "CREATE TABLE t(s TEXT); INSERT INTO t VALUES ('a'), ('c'), ('b')",
        );
        assert_eq!(
            query(db, "SELECT s FROM t ORDER BY s COLLATE rev").unwrap(),
            ["c", "b", "a"]
        );
        exec(db, "BEGIN; INSERT INTO t VALUES ('d')");
        assert_eq!(
            query(db, "SELECT s FROM t ORDER BY s COLLATE rev").unwrap(),
            ["d", "c", "b", "a"]
        );
        exec(db, "COMMIT");
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_trigger_udf_nested_error() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_temp(&dir);
        register_nested(db);
        exec(
            db,
            "CREATE TABLE t(id INTEGER PRIMARY KEY); \
             CREATE TABLE log(x UNIQUE); \
             CREATE TRIGGER t_del AFTER DELETE ON t BEGIN \
               SELECT nested('INSERT INTO log VALUES (' || (old.id % 2) || ')'); \
             END; \
             INSERT INTO t VALUES (1), (2), (3)",
        );

        // The nested INSERT for id 3 hits the UNIQUE constraint, the UDF
        // raises, and the whole DELETE is undone, nested writes included.
        let sql = c"DELETE FROM t";
        let mut err = ptr::null_mut();
        assert_eq!(
            sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), &mut err),
            SQLITE_ERROR
        );
        assert!(CStr::from_ptr(sqlite3_errmsg(db))
            .to_string_lossy()
            .contains("nested rc=19"));
        sqlite3_free(err as *mut libc::c_void);
        assert_eq!(sqlite3_get_autocommit(db), 1);
        assert_eq!(query(db, "SELECT count(*) FROM t").unwrap(), ["3"]);
        assert_eq!(query(db, "SELECT count(*) FROM log").unwrap(), ["0"]);

        // Same inside an explicit transaction: only the failed DELETE is
        // undone and the transaction stays usable.
        exec(db, "BEGIN; INSERT INTO log VALUES (7)");
        assert_eq!(
            sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), &mut err),
            SQLITE_ERROR
        );
        sqlite3_free(err as *mut libc::c_void);
        assert_eq!(sqlite3_get_autocommit(db), 0);
        exec(db, "DELETE FROM t WHERE id = 1; COMMIT");
        assert_eq!(
            query(db, "SELECT x FROM log ORDER BY x").unwrap(),
            ["1", "7"]
        );
        assert_eq!(
            query(db, "SELECT id FROM t ORDER BY id").unwrap(),
            ["2", "3"]
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_udf_nested_statement_outlives_call() {
    use std::sync::atomic::{AtomicPtr, Ordering};
    static KEPT: AtomicPtr<sqlite3_stmt> = AtomicPtr::new(ptr::null_mut());

    // Prepares a statement, steps it once and keeps it for later.
    unsafe extern "C" fn keep_fn(
        ctx: *mut libc::c_void,
        _argc: i32,
        _argv: *mut *mut libc::c_void,
    ) {
        let db = sqlite3_user_data(ctx) as *mut sqlite3;
        let mut stmt = ptr::null_mut();
        let sql = c"SELECT a FROM t ORDER BY a";
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
        sqlite3_result_int64(ctx, sqlite3_column_int64(stmt, 0));
        KEPT.store(stmt, Ordering::SeqCst);
    }

    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_temp(&dir);
        assert_eq!(
            sqlite3_create_function_v2(
                db,
                c"keep".as_ptr(),
                0,
                SQLITE_UTF8,
                db as *mut libc::c_void,
                Some(keep_fn),
                None,
                None,
                None,
            ),
            SQLITE_OK
        );
        exec(
            db,
            "CREATE TABLE t(a); CREATE TABLE u(b); INSERT INTO t VALUES (1), (2)",
        );
        assert_eq!(
            query(db, "INSERT INTO u SELECT keep() RETURNING b").unwrap(),
            ["1"]
        );
        // The kept statement finishes after the outer one has committed.
        let stmt = KEPT.load(Ordering::SeqCst);
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
        assert_eq!(sqlite3_column_int64(stmt, 0), 2);
        assert_eq!(sqlite3_step(stmt), SQLITE_DONE);
        assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);
        assert_eq!(sqlite3_get_autocommit(db), 1);
        exec(db, "INSERT INTO u VALUES (5)");
        assert_eq!(query(db, "SELECT b FROM u ORDER BY b").unwrap(), ["1", "5"]);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}
