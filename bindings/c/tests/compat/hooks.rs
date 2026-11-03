//! Commit, rollback, update, preupdate and WAL hooks, and the authorizer.
//! Every test runs against turso and, with `--features sqlite3`, SQLite.

use super::*;
use std::ffi::{CStr, CString};
use std::sync::Mutex;

type Log = Mutex<Vec<String>>;

extern "C" {
    fn sqlite3_commit_hook(
        db: *mut sqlite3,
        cb: Option<unsafe extern "C" fn(*mut libc::c_void) -> i32>,
        arg: *mut libc::c_void,
    ) -> *mut libc::c_void;
    fn sqlite3_rollback_hook(
        db: *mut sqlite3,
        cb: Option<unsafe extern "C" fn(*mut libc::c_void)>,
        arg: *mut libc::c_void,
    ) -> *mut libc::c_void;
    fn sqlite3_update_hook(
        db: *mut sqlite3,
        cb: Option<
            unsafe extern "C" fn(
                *mut libc::c_void,
                i32,
                *const libc::c_char,
                *const libc::c_char,
                i64,
            ),
        >,
        arg: *mut libc::c_void,
    ) -> *mut libc::c_void;
    fn sqlite3_preupdate_hook(
        db: *mut sqlite3,
        cb: Option<
            unsafe extern "C" fn(
                *mut libc::c_void,
                *mut sqlite3,
                i32,
                *const libc::c_char,
                *const libc::c_char,
                i64,
                i64,
            ),
        >,
        arg: *mut libc::c_void,
    ) -> *mut libc::c_void;
    fn sqlite3_preupdate_old(db: *mut sqlite3, i: i32, v: *mut *mut libc::c_void) -> i32;
    fn sqlite3_preupdate_new(db: *mut sqlite3, i: i32, v: *mut *mut libc::c_void) -> i32;
    fn sqlite3_preupdate_count(db: *mut sqlite3) -> i32;
    fn sqlite3_preupdate_depth(db: *mut sqlite3) -> i32;
    fn sqlite3_preupdate_blobwrite(db: *mut sqlite3) -> i32;
    fn sqlite3_wal_hook(
        db: *mut sqlite3,
        cb: Option<
            unsafe extern "C" fn(*mut libc::c_void, *mut sqlite3, *const libc::c_char, i32) -> i32,
        >,
        arg: *mut libc::c_void,
    ) -> *mut libc::c_void;
    fn sqlite3_wal_autocheckpoint(db: *mut sqlite3, n: i32) -> i32;
    fn sqlite3_set_authorizer(
        db: *mut sqlite3,
        cb: Option<
            unsafe extern "C" fn(
                *mut libc::c_void,
                i32,
                *const libc::c_char,
                *const libc::c_char,
                *const libc::c_char,
                *const libc::c_char,
            ) -> i32,
        >,
        arg: *mut libc::c_void,
    ) -> i32;
    fn sqlite3_extended_errcode(db: *mut sqlite3) -> i32;
    fn sqlite3_extended_result_codes(db: *mut sqlite3, onoff: i32) -> i32;
}

#[cfg(not(feature = "sqlite3"))]
extern "C" {
    fn turso_enable_experimental();
}

const SQLITE_ROW: i32 = 100;
const SQLITE_DONE: i32 = 101;
const SQLITE_MISUSE: i32 = 21;
const SQLITE_RANGE: i32 = 25;
const SQLITE_AUTH: i32 = 23;
const SQLITE_CONSTRAINT: i32 = 19;
const SQLITE_CONSTRAINT_COMMITHOOK: i32 = 531;
const SQLITE_INSERT: i32 = 18;
const SQLITE_DENY: i32 = 1;
const SQLITE_IGNORE: i32 = 2;

fn log_of(p: *mut libc::c_void) -> &'static Log {
    unsafe { &*(p as *const Log) }
}

fn push(p: *mut libc::c_void, s: String) {
    log_of(p).lock().unwrap().push(s);
}

fn take(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.lock().unwrap())
}

unsafe fn cstr(p: *const libc::c_char) -> String {
    if p.is_null() {
        "-".to_string()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

fn op_name(op: i32) -> &'static str {
    match op {
        9 => "DELETE",
        18 => "INSERT",
        23 => "UPDATE",
        _ => "?",
    }
}

static VETO: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

unsafe extern "C" fn commit_cb(p: *mut libc::c_void) -> i32 {
    let veto = VETO.load(std::sync::atomic::Ordering::SeqCst);
    push(p, format!("commit {veto}"));
    veto
}

unsafe extern "C" fn rollback_cb(p: *mut libc::c_void) {
    push(p, "rollback".to_string());
}

unsafe extern "C" fn update_cb(
    p: *mut libc::c_void,
    op: i32,
    db: *const libc::c_char,
    table: *const libc::c_char,
    rowid: i64,
) {
    push(
        p,
        format!("{} {}.{} {rowid}", op_name(op), cstr(db), cstr(table)),
    );
}

unsafe fn value_text(v: *mut libc::c_void) -> String {
    match sqlite3_value_type(v) {
        1 => sqlite3_value_int64(v).to_string(),
        2 => format!("{:.1}", sqlite3_value_double(v)),
        3 => format!("'{}'", cstr(sqlite3_value_text(v))),
        5 => "NULL".to_string(),
        t => format!("type{t}"),
    }
}

unsafe fn row_text(db: *mut sqlite3, old: bool) -> String {
    let n = sqlite3_preupdate_count(db);
    let mut parts = Vec::new();
    for i in 0..n {
        let mut v: *mut libc::c_void = ptr::null_mut();
        let rc = if old {
            sqlite3_preupdate_old(db, i, &mut v)
        } else {
            sqlite3_preupdate_new(db, i, &mut v)
        };
        assert_eq!(rc, SQLITE_OK);
        parts.push(value_text(v));
    }
    parts.join(",")
}

unsafe extern "C" fn preupdate_cb(
    p: *mut libc::c_void,
    db: *mut sqlite3,
    op: i32,
    zdb: *const libc::c_char,
    table: *const libc::c_char,
    k1: i64,
    k2: i64,
) {
    let mut v: *mut libc::c_void = ptr::null_mut();
    let n = sqlite3_preupdate_count(db);
    // Out-of-range and wrong-direction accesses.
    assert_eq!(
        sqlite3_preupdate_old(db, -1, &mut v),
        if op == SQLITE_INSERT {
            SQLITE_MISUSE
        } else {
            SQLITE_RANGE
        }
    );
    // Past the last column, or new values in a DELETE.
    assert_eq!(sqlite3_preupdate_new(db, n, &mut v), SQLITE_MISUSE);
    let old = if op == SQLITE_INSERT {
        assert_eq!(sqlite3_preupdate_old(db, 0, &mut v), SQLITE_MISUSE);
        "-".to_string()
    } else {
        row_text(db, true)
    };
    let new = if op == 9 {
        assert_eq!(sqlite3_preupdate_new(db, 0, &mut v), SQLITE_MISUSE);
        "-".to_string()
    } else {
        row_text(db, false)
    };
    push(
        p,
        format!(
            "pre {} {}.{} {k1} {k2} depth={} blob={} old={old} new={new}",
            op_name(op),
            cstr(zdb),
            cstr(table),
            sqlite3_preupdate_depth(db),
            sqlite3_preupdate_blobwrite(db),
        ),
    );
}

unsafe fn open_mem() -> *mut sqlite3 {
    let mut db: *mut sqlite3 = ptr::null_mut();
    assert_eq!(sqlite3_open(c":memory:".as_ptr(), &mut db), SQLITE_OK);
    db
}

unsafe fn exec(db: *mut sqlite3, sql: &str) -> i32 {
    let sql = CString::new(sql).unwrap();
    sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut())
}

/// Prepare and step to completion; returns the prepare error or the final step result.
unsafe fn run(db: *mut sqlite3, sql: &str) -> i32 {
    let sql = CString::new(sql).unwrap();
    let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
    let rc = sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut());
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
    rc
}

unsafe fn query_i64(db: *mut sqlite3, sql: &str) -> i64 {
    let sql = CString::new(sql).unwrap();
    let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
    assert_eq!(
        sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()),
        SQLITE_OK
    );
    assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
    let v = sqlite3_column_int64(stmt, 0);
    sqlite3_finalize(stmt);
    v
}

unsafe fn errmsg(db: *mut sqlite3) -> String {
    cstr(sqlite3_errmsg(db))
}

fn as_arg(log: &Log) -> *mut libc::c_void {
    log as *const Log as *mut libc::c_void
}

#[test]
fn test_hooks_return_previous_user_data() {
    unsafe {
        let db = open_mem();
        let (x, y) = (0u8, 0u8);
        let (a, b) = (
            &x as *const u8 as *mut libc::c_void,
            &y as *const u8 as *mut libc::c_void,
        );
        assert!(sqlite3_commit_hook(db, Some(commit_cb), a).is_null());
        assert_eq!(sqlite3_commit_hook(db, None, b), a);
        assert!(sqlite3_rollback_hook(db, Some(rollback_cb), a).is_null());
        assert_eq!(sqlite3_rollback_hook(db, None, ptr::null_mut()), a);
        assert!(sqlite3_update_hook(db, Some(update_cb), a).is_null());
        assert_eq!(sqlite3_update_hook(db, None, ptr::null_mut()), a);
        assert!(sqlite3_preupdate_hook(db, Some(preupdate_cb), a).is_null());
        assert_eq!(sqlite3_preupdate_hook(db, None, b), a);
        assert_eq!(sqlite3_preupdate_hook(db, None, ptr::null_mut()), b);
        // SQLite's open installs its auto-checkpoint WAL hook with (void*)1000.
        assert_eq!(
            sqlite3_wal_hook(db, None, a),
            1000usize as *mut libc::c_void
        );
        assert_eq!(sqlite3_wal_hook(db, None, ptr::null_mut()), a);
        // Outside a preupdate callback.
        let mut v: *mut libc::c_void = ptr::null_mut();
        assert_eq!(sqlite3_preupdate_old(db, 0, &mut v), SQLITE_MISUSE);
        assert_eq!(sqlite3_preupdate_new(db, 0, &mut v), SQLITE_MISUSE);
        assert_eq!(sqlite3_preupdate_count(db), 0);
        assert_eq!(sqlite3_preupdate_depth(db), 0);
        assert_eq!(sqlite3_preupdate_blobwrite(db), -1);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_commit_rollback_update_hooks() {
    unsafe {
        let db = open_mem();
        let log = Log::default();
        sqlite3_commit_hook(db, Some(commit_cb), as_arg(&log));
        sqlite3_rollback_hook(db, Some(rollback_cb), as_arg(&log));
        sqlite3_update_hook(db, Some(update_cb), as_arg(&log));
        assert_eq!(
            exec(db, "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT UNIQUE)"),
            SQLITE_OK
        );
        assert_eq!(take(&log), ["commit 0"]);
        assert_eq!(run(db, "INSERT INTO t(v) VALUES('a'), ('b')"), SQLITE_DONE);
        assert_eq!(run(db, "UPDATE t SET id = 10 WHERE id = 2"), SQLITE_DONE);
        assert_eq!(run(db, "DELETE FROM t WHERE id = 1"), SQLITE_DONE);
        assert_eq!(
            run(db, "INSERT OR REPLACE INTO t(id, v) VALUES(10, 'c')"),
            SQLITE_DONE
        );
        assert_eq!(
            take(&log),
            [
                "INSERT main.t 1",
                "INSERT main.t 2",
                "commit 0",
                "UPDATE main.t 10",
                "commit 0",
                "DELETE main.t 1",
                "commit 0",
                "INSERT main.t 10",
                "commit 0",
            ]
        );
        // Reads do not commit; explicit transactions commit once.
        assert_eq!(run(db, "SELECT * FROM t"), SQLITE_DONE);
        assert_eq!(
            exec(
                db,
                "BEGIN; INSERT INTO t(v) VALUES('x'); INSERT INTO t(v) VALUES('y'); COMMIT"
            ),
            SQLITE_OK
        );
        assert_eq!(
            take(&log),
            ["INSERT main.t 11", "INSERT main.t 12", "commit 0"]
        );
        assert_eq!(
            exec(db, "BEGIN; INSERT INTO t(v) VALUES('r'); ROLLBACK"),
            SQLITE_OK
        );
        assert_eq!(take(&log), ["INSERT main.t 13", "rollback"]);
        // An error rolls back the autocommit statement.
        assert_eq!(run(db, "INSERT INTO t(v) VALUES('x')"), SQLITE_CONSTRAINT);
        assert_eq!(take(&log), ["rollback"]);
        // ROLLBACK TO is not a transaction rollback.
        assert_eq!(
            exec(
                db,
                "BEGIN; SAVEPOINT s; INSERT INTO t(v) VALUES('s'); ROLLBACK TO s; COMMIT"
            ),
            SQLITE_OK
        );
        assert_eq!(take(&log), ["INSERT main.t 13", "commit 0"]);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_commit_hook_veto_is_constraint_commithook() {
    unsafe {
        let db = open_mem();
        let log = Log::default();
        assert_eq!(exec(db, "CREATE TABLE t(x)"), SQLITE_OK);
        sqlite3_commit_hook(db, Some(commit_cb), as_arg(&log));
        sqlite3_rollback_hook(db, Some(rollback_cb), as_arg(&log));
        VETO.store(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(run(db, "INSERT INTO t VALUES(1)"), SQLITE_CONSTRAINT);
        assert_eq!(sqlite3_errcode(db), SQLITE_CONSTRAINT);
        assert_eq!(sqlite3_extended_errcode(db), SQLITE_CONSTRAINT_COMMITHOOK);
        assert_eq!(errmsg(db), "constraint failed");
        assert_eq!(take(&log), ["commit 1", "rollback"]);
        // Explicit COMMIT: the whole transaction is rolled back.
        assert_eq!(exec(db, "BEGIN; INSERT INTO t VALUES(2)"), SQLITE_OK);
        assert_eq!(run(db, "COMMIT"), SQLITE_CONSTRAINT);
        assert_eq!(sqlite3_extended_errcode(db), SQLITE_CONSTRAINT_COMMITHOOK);
        assert_eq!(sqlite3_get_autocommit(db), 1);
        assert_eq!(take(&log), ["commit 1", "rollback"]);
        // With extended result codes on, step returns the extended code.
        sqlite3_extended_result_codes(db, 1);
        assert_eq!(
            run(db, "INSERT INTO t VALUES(3)"),
            SQLITE_CONSTRAINT_COMMITHOOK
        );
        sqlite3_extended_result_codes(db, 0);
        VETO.store(0, std::sync::atomic::Ordering::SeqCst);
        take(&log);
        assert_eq!(query_i64(db, "SELECT count(*) FROM t"), 0);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_commit_hook_fires_for_attached_only_writes() {
    unsafe {
        #[cfg(not(feature = "sqlite3"))]
        turso_enable_experimental();
        let dir = tempfile::TempDir::new().unwrap();
        let aux = dir.path().join("aux.db");
        let db = open_mem();
        let log = Log::default();
        assert_eq!(
            exec(db, &format!("ATTACH '{}' AS aux", aux.display())),
            SQLITE_OK
        );
        assert_eq!(exec(db, "CREATE TABLE aux.a(x)"), SQLITE_OK);
        sqlite3_commit_hook(db, Some(commit_cb), as_arg(&log));
        sqlite3_rollback_hook(db, Some(rollback_cb), as_arg(&log));
        sqlite3_update_hook(db, Some(update_cb), as_arg(&log));
        assert_eq!(run(db, "INSERT INTO aux.a VALUES(1)"), SQLITE_DONE);
        assert_eq!(take(&log), ["INSERT aux.a 1", "commit 0"]);
        VETO.store(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(run(db, "INSERT INTO aux.a VALUES(2)"), SQLITE_CONSTRAINT);
        VETO.store(0, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(take(&log), ["INSERT aux.a 2", "commit 1", "rollback"]);
        assert_eq!(query_i64(db, "SELECT count(*) FROM aux.a"), 1);
        // Temp-only writes commit through the hook too.
        assert_eq!(exec(db, "CREATE TEMP TABLE tt(x)"), SQLITE_OK);
        take(&log);
        assert_eq!(run(db, "INSERT INTO tt VALUES(1)"), SQLITE_DONE);
        assert_eq!(take(&log), ["INSERT temp.tt 1", "commit 0"]);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_preupdate_hook_values() {
    unsafe {
        let db = open_mem();
        let log = Log::default();
        assert_eq!(
            exec(
                db,
                "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT UNIQUE, r REAL)"
            ),
            SQLITE_OK
        );
        sqlite3_preupdate_hook(db, Some(preupdate_cb), as_arg(&log));
        assert_eq!(
            run(db, "INSERT INTO t(v, r) VALUES('a', 1), ('b', 2.5)"),
            SQLITE_DONE
        );
        assert_eq!(run(db, "UPDATE t SET v = 'bb' WHERE id = 2"), SQLITE_DONE);
        assert_eq!(run(db, "UPDATE t SET id = 10 WHERE id = 2"), SQLITE_DONE);
        assert_eq!(run(db, "DELETE FROM t WHERE id = 1"), SQLITE_DONE);
        assert_eq!(
            run(db, "INSERT OR REPLACE INTO t(id, v) VALUES(10, 'x')"),
            SQLITE_DONE
        );
        assert_eq!(run(db, "INSERT INTO t(id, v) VALUES(11, 'y')"), SQLITE_DONE);
        assert_eq!(
            run(db, "UPDATE OR REPLACE t SET v = 'x' WHERE id = 11"),
            SQLITE_DONE
        );
        assert_eq!(
            take(&log),
            [
                "pre INSERT main.t 1 1 depth=0 blob=-1 old=- new=1,'a',1.0",
                "pre INSERT main.t 2 2 depth=0 blob=-1 old=- new=2,'b',2.5",
                "pre UPDATE main.t 2 2 depth=0 blob=-1 old=2,'b',2.5 new=2,'bb',2.5",
                "pre UPDATE main.t 2 10 depth=0 blob=-1 old=2,'bb',2.5 new=10,'bb',2.5",
                "pre DELETE main.t 1 1 depth=0 blob=-1 old=1,'a',1.0 new=-",
                "pre DELETE main.t 10 10 depth=0 blob=-1 old=10,'bb',2.5 new=-",
                "pre INSERT main.t 10 10 depth=0 blob=-1 old=- new=10,'x',NULL",
                "pre INSERT main.t 11 11 depth=0 blob=-1 old=- new=11,'y',NULL",
                "pre DELETE main.t 10 10 depth=0 blob=-1 old=10,'x',NULL new=-",
                "pre UPDATE main.t 11 11 depth=0 blob=-1 old=11,'y',NULL new=11,'x',NULL",
            ]
        );
        // Columns added later read as their default; trigger changes have depth 1.
        assert_eq!(
            exec(db, "ALTER TABLE t ADD COLUMN e DEFAULT 'dflt'"),
            SQLITE_OK
        );
        assert_eq!(exec(db, "CREATE TABLE log(m)"), SQLITE_OK);
        assert_eq!(
            exec(
                db,
                "CREATE TRIGGER tr AFTER UPDATE ON t BEGIN INSERT INTO log VALUES(new.v); END"
            ),
            SQLITE_OK
        );
        assert_eq!(run(db, "UPDATE t SET r = 3"), SQLITE_DONE);
        assert_eq!(
            take(&log),
            [
                "pre UPDATE main.t 11 11 depth=0 blob=-1 old=11,'x',NULL,'dflt' new=11,'x',3.0,'dflt'",
                "pre INSERT main.log 1 1 depth=1 blob=-1 old=- new='x'",
            ]
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_delete_without_where_reports_no_rows() {
    unsafe {
        let db = open_mem();
        let log = Log::default();
        assert_eq!(
            exec(db, "CREATE TABLE t(x); INSERT INTO t VALUES(1), (2)"),
            SQLITE_OK
        );
        sqlite3_update_hook(db, Some(update_cb), as_arg(&log));
        // The truncate optimization: no per-row update hook calls.
        assert_eq!(run(db, "DELETE FROM t"), SQLITE_DONE);
        assert_eq!(sqlite3_changes(db), 2);
        assert!(take(&log).is_empty());
        assert_eq!(exec(db, "INSERT INTO t VALUES(1), (2)"), SQLITE_OK);
        take(&log);
        assert_eq!(run(db, "DELETE FROM t WHERE 1"), SQLITE_DONE);
        assert_eq!(take(&log), ["DELETE main.t 1", "DELETE main.t 2"]);
        // A preupdate hook disables the optimization.
        assert_eq!(exec(db, "INSERT INTO t(rowid, x) VALUES(7, 1)"), SQLITE_OK);
        sqlite3_preupdate_hook(db, Some(preupdate_cb), as_arg(&log));
        take(&log);
        assert_eq!(run(db, "DELETE FROM t"), SQLITE_DONE);
        assert_eq!(
            take(&log),
            [
                "pre DELETE main.t 7 7 depth=0 blob=-1 old=1 new=-",
                "DELETE main.t 7"
            ]
        );
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

unsafe extern "C" fn wal_cb(
    p: *mut libc::c_void,
    _db: *mut sqlite3,
    name: *const libc::c_char,
    frames: i32,
) -> i32 {
    push(p, format!("{} {frames}", cstr(name)));
    WAL_RC.load(std::sync::atomic::Ordering::SeqCst)
}

static WAL_RC: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[test]
fn test_wal_hook() {
    unsafe {
        let dir = tempfile::TempDir::new().unwrap();
        let path = CString::new(dir.path().join("w.db").to_str().unwrap()).unwrap();
        let mut db: *mut sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(path.as_ptr(), &mut db), SQLITE_OK);
        assert_eq!(run(db, "PRAGMA journal_mode = WAL"), SQLITE_DONE);
        assert_eq!(
            exec(db, "CREATE TABLE t(x); PRAGMA wal_autocheckpoint = 2"),
            SQLITE_OK
        );
        let log = Log::default();
        sqlite3_wal_hook(db, Some(wal_cb), as_arg(&log));
        // The WAL hook replaces the auto-checkpoint.
        assert_eq!(query_i64(db, "PRAGMA wal_autocheckpoint"), 0);
        for i in 0..4 {
            assert_eq!(run(db, &format!("INSERT INTO t VALUES({i})")), SQLITE_DONE);
        }
        let frames: Vec<i64> = take(&log)
            .iter()
            .map(|e| e.strip_prefix("main ").unwrap().parse().unwrap())
            .collect();
        assert_eq!(frames.len(), 4);
        assert!(
            frames.windows(2).all(|w| w[1] > w[0]),
            "no checkpoint expected: {frames:?}"
        );
        // A failing hook fails the step, but the commit stands.
        WAL_RC.store(10, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(run(db, "INSERT INTO t VALUES(9)"), SQLITE_ERROR);
        WAL_RC.store(0, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(query_i64(db, "SELECT count(*) FROM t WHERE x = 9"), 1);
        // sqlite3_wal_autocheckpoint and the WAL hook replace each other.
        assert_eq!(sqlite3_wal_autocheckpoint(db, 500), SQLITE_OK);
        assert_eq!(query_i64(db, "PRAGMA wal_autocheckpoint"), 500);
        take(&log);
        assert_eq!(run(db, "INSERT INTO t VALUES(10)"), SQLITE_DONE);
        assert!(take(&log).is_empty());
        assert_eq!(
            sqlite3_wal_hook(db, Some(wal_cb), as_arg(&log)),
            500usize as *mut libc::c_void
        );
        assert_eq!(exec(db, "PRAGMA wal_autocheckpoint = 100"), SQLITE_OK);
        assert_eq!(run(db, "INSERT INTO t VALUES(11)"), SQLITE_DONE);
        assert!(take(&log).is_empty());
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

static REENTER_DB: std::sync::atomic::AtomicPtr<sqlite3> =
    std::sync::atomic::AtomicPtr::new(ptr::null_mut());

unsafe extern "C" fn reentrant_update_cb(
    p: *mut libc::c_void,
    _op: i32,
    _db: *const libc::c_char,
    _table: *const libc::c_char,
    _rowid: i64,
) {
    let db = REENTER_DB.load(std::sync::atomic::Ordering::SeqCst);
    push(p, format!("autocommit={}", sqlite3_get_autocommit(db)));
}

#[test]
fn test_hooks_may_reenter_the_api_from_exec() {
    unsafe {
        let db = open_mem();
        REENTER_DB.store(db, std::sync::atomic::Ordering::SeqCst);
        let log = Log::default();
        assert_eq!(exec(db, "CREATE TABLE t(x)"), SQLITE_OK);
        sqlite3_update_hook(db, Some(reentrant_update_cb), as_arg(&log));
        assert_eq!(exec(db, "INSERT INTO t VALUES(1)"), SQLITE_OK);
        assert_eq!(take(&log), ["autocommit=1"]);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

const AUTH_NAMES: [&str; 34] = [
    "COPY",
    "CREATE_INDEX",
    "CREATE_TABLE",
    "CREATE_TEMP_INDEX",
    "CREATE_TEMP_TABLE",
    "CREATE_TEMP_TRIGGER",
    "CREATE_TEMP_VIEW",
    "CREATE_TRIGGER",
    "CREATE_VIEW",
    "DELETE",
    "DROP_INDEX",
    "DROP_TABLE",
    "DROP_TEMP_INDEX",
    "DROP_TEMP_TABLE",
    "DROP_TEMP_TRIGGER",
    "DROP_TEMP_VIEW",
    "DROP_TRIGGER",
    "DROP_VIEW",
    "INSERT",
    "PRAGMA",
    "READ",
    "SELECT",
    "TRANSACTION",
    "UPDATE",
    "ATTACH",
    "DETACH",
    "ALTER_TABLE",
    "REINDEX",
    "ANALYZE",
    "CREATE_VTABLE",
    "DROP_VTABLE",
    "FUNCTION",
    "SAVEPOINT",
    "RECURSIVE",
];

/// Logs each request; answers `AUTH_ANSWER` for requests whose first or
/// second argument is `AUTH_TARGET`.
static AUTH_TARGET: Mutex<Option<&'static str>> = Mutex::new(None);
static AUTH_ANSWER: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

unsafe extern "C" fn auth_cb(
    p: *mut libc::c_void,
    code: i32,
    a: *const libc::c_char,
    b: *const libc::c_char,
    c: *const libc::c_char,
    _d: *const libc::c_char,
) -> i32 {
    let (a, b, c) = (cstr(a), cstr(b), cstr(c));
    // Skip SQLite's requests for its own schema-table updates during DDL.
    if !a.starts_with("sqlite_") {
        push(p, format!("{} {a} {b} {c}", AUTH_NAMES[code as usize]));
    }
    match *AUTH_TARGET.lock().unwrap() {
        Some(t) if a == t || b == t => AUTH_ANSWER.load(std::sync::atomic::Ordering::SeqCst),
        _ => SQLITE_OK,
    }
}

fn set_target(target: Option<&'static str>, answer: i32) {
    *AUTH_TARGET.lock().unwrap() = target;
    AUTH_ANSWER.store(answer, std::sync::atomic::Ordering::SeqCst);
}

#[test]
fn test_authorizer_actions() {
    unsafe {
        let db = open_mem();
        let log = Log::default();
        assert_eq!(
            exec(db, "CREATE TABLE t(id INTEGER PRIMARY KEY, v, w)"),
            SQLITE_OK
        );
        set_target(None, SQLITE_OK);
        assert_eq!(
            sqlite3_set_authorizer(db, Some(auth_cb), as_arg(&log)),
            SQLITE_OK
        );
        let cases: &[(&str, &[&str])] = &[
            ("SELECT v FROM t", &["SELECT - - -", "READ t v main"]),
            (
                "SELECT count(*) FROM t",
                &["SELECT - - -", "FUNCTION - count -", "READ t  -"],
            ),
            ("INSERT INTO t(v) VALUES(1)", &["INSERT t - main"]),
            (
                "UPDATE t SET v = 2, w = 3",
                &["UPDATE t v main", "UPDATE t w main"],
            ),
            (
                "DELETE FROM t WHERE v = 2",
                &["DELETE t - main", "READ t v main"],
            ),
            ("BEGIN", &["TRANSACTION BEGIN - -"]),
            ("SAVEPOINT s", &["SAVEPOINT BEGIN s -"]),
            ("RELEASE s", &["SAVEPOINT RELEASE s -"]),
            ("COMMIT", &["TRANSACTION COMMIT - -"]),
            ("PRAGMA user_version", &["PRAGMA user_version - -"]),
            (
                "PRAGMA main.user_version = 3",
                &["PRAGMA user_version 3 main"],
            ),
            ("CREATE TABLE c(x)", &["CREATE_TABLE c - main"]),
            (
                "CREATE VIEW cv AS SELECT x FROM c",
                &["CREATE_VIEW cv - main"],
            ),
            ("DROP VIEW cv", &["DROP_VIEW cv - main", "DELETE cv - main"]),
            ("DROP TABLE c", &["DROP_TABLE c - main", "DELETE c - main"]),
            ("CREATE TEMP TABLE ct(x)", &["CREATE_TEMP_TABLE ct - temp"]),
        ];
        for (sql, expected) in cases {
            assert_eq!(run(db, sql), SQLITE_DONE, "{sql}: {}", errmsg(db));
            assert_eq!(take(&log), *expected, "{sql}");
        }
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_authorizer_deny_ignore_and_errors() {
    unsafe {
        let db = open_mem();
        let log = Log::default();
        assert_eq!(
            exec(db, "CREATE TABLE t(v, w); INSERT INTO t VALUES('a', 'b')"),
            SQLITE_OK
        );
        sqlite3_set_authorizer(db, Some(auth_cb), as_arg(&log));
        // A denied column read fails the prepare.
        set_target(Some("w"), SQLITE_DENY);
        assert_eq!(run(db, "SELECT w FROM t"), SQLITE_AUTH);
        assert_eq!(sqlite3_errcode(db), SQLITE_AUTH);
        assert_eq!(errmsg(db), "access to t.w is prohibited");
        // An ignored column reads as NULL.
        set_target(Some("w"), SQLITE_IGNORE);
        let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(
                db,
                c"SELECT v, w FROM t".as_ptr(),
                -1,
                &mut stmt,
                ptr::null_mut()
            ),
            SQLITE_OK
        );
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
        assert_eq!(cstr(sqlite3_column_text(stmt, 0)), "a");
        assert_eq!(sqlite3_column_type(stmt, 1), 5);
        sqlite3_finalize(stmt);
        // An ignored UPDATE column is left unchanged.
        assert_eq!(run(db, "UPDATE t SET v = 'x', w = 'y'"), SQLITE_DONE);
        set_target(None, SQLITE_OK);
        assert_eq!(query_i64(db, "SELECT v = 'x' AND w = 'b' FROM t"), 1);
        // Statement-level denial and IGNORE.
        set_target(Some("t"), SQLITE_DENY);
        assert_eq!(run(db, "INSERT INTO t VALUES(1, 2)"), SQLITE_AUTH);
        assert_eq!(errmsg(db), "not authorized");
        set_target(Some("t"), SQLITE_IGNORE);
        assert_eq!(run(db, "INSERT INTO t VALUES(1, 2)"), SQLITE_DONE);
        set_target(Some("upper"), SQLITE_DENY);
        assert_eq!(
            run(db, "SELECT upper(v) FROM t"),
            SQLITE_ERROR,
            "{}",
            errmsg(db)
        );
        assert_eq!(errmsg(db), "not authorized to use function: upper");
        // Any other answer is a malfunction.
        set_target(Some("t"), 42);
        assert_eq!(run(db, "INSERT INTO t VALUES(1, 2)"), SQLITE_ERROR);
        assert_eq!(errmsg(db), "authorizer malfunction");
        set_target(None, SQLITE_OK);
        sqlite3_set_authorizer(db, None, ptr::null_mut());
        assert_eq!(query_i64(db, "SELECT count(*) FROM t"), 1);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

static AUTH_DB: std::sync::atomic::AtomicPtr<sqlite3> =
    std::sync::atomic::AtomicPtr::new(ptr::null_mut());

unsafe extern "C" fn clearing_auth_cb(
    _p: *mut libc::c_void,
    _code: i32,
    _a: *const libc::c_char,
    _b: *const libc::c_char,
    _c: *const libc::c_char,
    _d: *const libc::c_char,
) -> i32 {
    // Re-enters the API from inside prepare, as CPython's tests do.
    sqlite3_set_authorizer(
        AUTH_DB.load(std::sync::atomic::Ordering::SeqCst),
        None,
        ptr::null_mut(),
    );
    SQLITE_DENY
}

#[test]
fn test_authorizer_may_clear_itself() {
    unsafe {
        let db = open_mem();
        AUTH_DB.store(db, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(exec(db, "CREATE TABLE t(x)"), SQLITE_OK);
        sqlite3_set_authorizer(db, Some(clearing_auth_cb), ptr::null_mut());
        assert_eq!(run(db, "SELECT * FROM t"), SQLITE_AUTH);
        assert_eq!(errmsg(db), "not authorized");
        assert_eq!(run(db, "SELECT * FROM t"), SQLITE_DONE);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}
