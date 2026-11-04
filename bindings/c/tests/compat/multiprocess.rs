//! Cross-process locking: a second process opens the same database file.
//!
//! Child processes re-run this test binary on `multiprocess::child_entry`
//! with the role and database path in the environment.

use super::*;
use std::ffi::CString;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SQLITE_BUSY: i32 = 5;

const ROLE_ENV: &str = "TURSO_COMPAT_MP_ROLE";
const DB_ENV: &str = "TURSO_COMPAT_MP_DB";
const ARG_ENV: &str = "TURSO_COMPAT_MP_ARG";

unsafe fn open(path: &str) -> *mut sqlite3 {
    let c_path = CString::new(path).unwrap();
    let mut db = ptr::null_mut();
    let rc = sqlite3_open_v2(
        c_path.as_ptr(),
        &mut db,
        SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE,
        ptr::null(),
    );
    assert_eq!(rc, SQLITE_OK, "open {path}");
    db
}

unsafe fn exec(db: *mut sqlite3, sql: &str) -> i32 {
    let sql = CString::new(sql).unwrap();
    sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut())
}

unsafe fn query_text(db: *mut sqlite3, sql: &str) -> String {
    let sql = CString::new(sql).unwrap();
    let mut stmt = ptr::null_mut();
    assert_eq!(
        sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()),
        SQLITE_OK
    );
    assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
    let text = std::ffi::CStr::from_ptr(sqlite3_column_text(stmt, 0))
        .to_string_lossy()
        .into_owned();
    assert_eq!(sqlite3_finalize(stmt), SQLITE_OK);
    text
}

unsafe fn query_i64(db: *mut sqlite3, sql: &str) -> i64 {
    query_text(db, sql).parse().unwrap()
}

/// Create a WAL database with table `t(p, i)` holding one row.
unsafe fn setup(dir: &tempfile::TempDir) -> (String, *mut sqlite3) {
    let path = dir.path().join("mp.db").to_str().unwrap().to_string();
    let db = open(&path);
    assert_eq!(query_text(db, "PRAGMA journal_mode=WAL"), "wal");
    assert_eq!(exec(db, "CREATE TABLE t(p, i)"), SQLITE_OK);
    assert_eq!(exec(db, "INSERT INTO t VALUES (-1, 0)"), SQLITE_OK);
    (path, db)
}

fn spawn(role: &str, path: &str, arg: &str) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "multiprocess::child_entry",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROLE_ENV, role)
        .env(DB_ENV, path)
        .env(ARG_ENV, arg)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Wait for a child and return its exit code and `RESULT=` value.
fn finish(child: Child) -> (i32, Option<String>) {
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let result = stdout
        .lines()
        .find_map(|l| l.split_once("RESULT=").map(|(_, v)| v.trim().to_string()));
    (out.status.code().unwrap_or(-1), result)
}

static BUSY_CALLS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

unsafe extern "C" fn busy_retry(_arg: *mut libc::c_void, count: i32) -> i32 {
    BUSY_CALLS.store(count + 1, std::sync::atomic::Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(10));
    (count < 1000) as i32
}

/// Print a child's result for `finish`, bypassing libtest output capture.
fn report(value: impl std::fmt::Display) {
    use std::io::Write;
    let mut out = std::io::stdout();
    writeln!(out, "\nRESULT={value}").unwrap();
    out.flush().unwrap();
}

/// Child process body. Exits with the SQLite result code of its work.
#[test]
#[ignore]
fn child_entry() {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    let path = std::env::var(DB_ENV).unwrap();
    let arg = std::env::var(ARG_ENV).unwrap();
    let pid = std::process::id() as i64;
    let rc = unsafe {
        // SQLite opens lazily and reports a lock at first use; turso may
        // already report it from the open.
        let c_path = CString::new(path.as_str()).unwrap();
        let mut db = ptr::null_mut();
        let rc = sqlite3_open_v2(c_path.as_ptr(), &mut db, SQLITE_OPEN_READWRITE, ptr::null());
        if rc != SQLITE_OK {
            let msg = std::ffi::CStr::from_ptr(sqlite3_errmsg(db)).to_string_lossy();
            assert_eq!(msg, "database is locked");
            sqlite3_close(db);
            std::process::exit(rc);
        }
        let rc = match role.as_str() {
            // arg: busy timeout in ms
            "insert" => {
                sqlite3_busy_timeout(db, arg.parse().unwrap());
                exec(db, &format!("INSERT INTO t VALUES ({pid}, 0)"))
            }
            "insert_handler" => {
                sqlite3_busy_handler(db, Some(busy_retry), ptr::null_mut());
                let rc = exec(db, &format!("INSERT INTO t VALUES ({pid}, 0)"));
                report(BUSY_CALLS.load(std::sync::atomic::Ordering::SeqCst));
                rc
            }
            "count" => {
                let sql = CString::new("SELECT count(*) FROM t").unwrap();
                let mut stmt = ptr::null_mut();
                let mut rc = sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut());
                if rc == SQLITE_OK {
                    rc = match sqlite3_step(stmt) {
                        SQLITE_ROW => {
                            report(sqlite3_column_int64(stmt, 0));
                            SQLITE_OK
                        }
                        rc => rc,
                    };
                }
                sqlite3_finalize(stmt);
                rc
            }
            // arg: rows; inserted five per transaction, retrying on BUSY
            "stress" => {
                sqlite3_busy_timeout(db, 20000);
                let rows: i64 = arg.parse().unwrap();
                let mut rc = SQLITE_OK;
                let mut i = 0;
                while i < rows && rc == SQLITE_OK {
                    rc = exec(db, "BEGIN IMMEDIATE");
                    if rc == SQLITE_BUSY {
                        rc = SQLITE_OK;
                        continue;
                    }
                    for j in i..(i + 5).min(rows) {
                        if rc == SQLITE_OK {
                            rc = exec(db, &format!("INSERT INTO t VALUES ({pid}, {j})"));
                        }
                    }
                    if rc == SQLITE_OK {
                        rc = exec(db, "COMMIT");
                    }
                    if rc == SQLITE_OK && i % 25 == 0 {
                        query_i64(db, "SELECT count(*) FROM t");
                    }
                    i += 5;
                }
                rc
            }
            _ => unreachable!("unknown role {role}"),
        };
        assert_eq!(sqlite3_close(db), SQLITE_OK);
        rc
    };
    std::process::exit(rc);
}

#[test]
fn test_second_process_reads_and_writes() {
    let dir = tempfile::tempdir().unwrap();
    unsafe {
        let (path, db) = setup(&dir);
        let (rc, count) = finish(spawn("count", &path, ""));
        assert_eq!((rc, count.as_deref()), (SQLITE_OK, Some("1")));
        assert_eq!(finish(spawn("insert", &path, "0")).0, SQLITE_OK);
        assert_eq!(query_i64(db, "SELECT count(*) FROM t"), 2);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_writer_in_other_process_gets_busy() {
    let dir = tempfile::tempdir().unwrap();
    unsafe {
        let (path, db) = setup(&dir);
        assert_eq!(exec(db, "BEGIN IMMEDIATE"), SQLITE_OK);
        // Readers still proceed while the write lock is held.
        let (rc, count) = finish(spawn("count", &path, ""));
        assert_eq!((rc, count.as_deref()), (SQLITE_OK, Some("1")));
        assert_eq!(finish(spawn("insert", &path, "0")).0, SQLITE_BUSY);
        let start = Instant::now();
        assert_eq!(finish(spawn("insert", &path, "300")).0, SQLITE_BUSY);
        assert!(start.elapsed() >= Duration::from_millis(300));
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);
        assert_eq!(query_i64(db, "SELECT count(*) FROM t"), 1);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_busy_timeout_waits_for_other_process() {
    let dir = tempfile::tempdir().unwrap();
    unsafe {
        let (path, db) = setup(&dir);
        assert_eq!(exec(db, "BEGIN IMMEDIATE"), SQLITE_OK);
        assert_eq!(exec(db, "INSERT INTO t VALUES (-2, 0)"), SQLITE_OK);
        let child = spawn("insert", &path, "20000");
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);
        assert_eq!(finish(child).0, SQLITE_OK);
        assert_eq!(query_i64(db, "SELECT count(*) FROM t"), 3);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_busy_handler_retries_across_processes() {
    let dir = tempfile::tempdir().unwrap();
    unsafe {
        let (path, db) = setup(&dir);
        assert_eq!(exec(db, "BEGIN IMMEDIATE"), SQLITE_OK);
        let child = spawn("insert_handler", &path, "");
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);
        let (rc, calls) = finish(child);
        assert_eq!(rc, SQLITE_OK);
        assert!(calls.unwrap().parse::<i32>().unwrap() > 0);
        assert_eq!(query_i64(db, "SELECT count(*) FROM t"), 2);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_reader_snapshot_survives_other_process_commit() {
    let dir = tempfile::tempdir().unwrap();
    unsafe {
        let (path, db) = setup(&dir);
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(query_i64(db, "SELECT count(*) FROM t"), 1);
        assert_eq!(finish(spawn("insert", &path, "5000")).0, SQLITE_OK);
        assert_eq!(query_i64(db, "SELECT count(*) FROM t"), 1);
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);
        assert_eq!(query_i64(db, "SELECT count(*) FROM t"), 2);
        let (rc, count) = finish(spawn("count", &path, ""));
        assert_eq!((rc, count.as_deref()), (SQLITE_OK, Some("2")));
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_concurrent_writer_processes_keep_every_row() {
    const PROCS: usize = 6;
    const ROWS: i64 = 100;
    let dir = tempfile::tempdir().unwrap();
    unsafe {
        let (path, db) = setup(&dir);
        let children: Vec<_> = (0..PROCS)
            .map(|_| spawn("stress", &path, &ROWS.to_string()))
            .collect();
        for child in children {
            assert_eq!(finish(child).0, SQLITE_OK);
        }
        assert_eq!(
            query_i64(db, "SELECT count(*) FROM t"),
            1 + PROCS as i64 * ROWS
        );
        assert_eq!(
            query_i64(db, "SELECT count(DISTINCT p) FROM t WHERE p > 0"),
            PROCS as i64
        );
        assert_eq!(query_text(db, "PRAGMA integrity_check"), "ok");
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_locking_mode_pragma() {
    let dir = tempfile::tempdir().unwrap();
    unsafe {
        let (_path, db) = setup(&dir);
        assert_eq!(query_text(db, "PRAGMA locking_mode"), "normal");
        assert_eq!(query_text(db, "PRAGMA locking_mode=NORMAL"), "normal");
        assert_eq!(query_text(db, "PRAGMA locking_mode=bogus"), "normal");
        assert_eq!(query_text(db, "PRAGMA locking_mode=EXCLUSIVE"), "exclusive");
        assert_eq!(query_text(db, "PRAGMA locking_mode"), "exclusive");
        assert_eq!(query_text(db, "PRAGMA locking_mode=normal"), "normal");
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_exclusive_locking_mode_locks_out_other_processes() {
    let dir = tempfile::tempdir().unwrap();
    unsafe {
        let (path, db) = setup(&dir);
        assert_eq!(query_text(db, "PRAGMA locking_mode=EXCLUSIVE"), "exclusive");
        // The lock is taken by the next write and kept after it.
        assert_eq!(exec(db, "INSERT INTO t VALUES (-2, 0)"), SQLITE_OK);
        assert_eq!(finish(spawn("count", &path, "")).0, SQLITE_BUSY);
        assert_eq!(query_text(db, "PRAGMA locking_mode=NORMAL"), "normal");
        // NORMAL releases it at the next access.
        assert_eq!(query_i64(db, "SELECT count(*) FROM t"), 2);
        let (rc, count) = finish(spawn("count", &path, ""));
        assert_eq!((rc, count.as_deref()), (SQLITE_OK, Some("2")));
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// Run SQL in the system `sqlite3` shell (real SQLite); None if it is absent.
fn sqlite_cli(path: &str, sql: &str) -> Option<String> {
    let out = Command::new("sqlite3")
        .args(["-batch", "-cmd", ".timeout 5000", path, sql])
        .stderr(Stdio::inherit())
        .output()
        .ok()?;
    assert!(out.status.success(), "sqlite3 failed on: {sql}");
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[test]
fn test_sqlite_shell_writes_alongside_open_connection() {
    let dir = tempfile::tempdir().unwrap();
    unsafe {
        let (path, db) = setup(&dir);
        // The checkpoint backfills everything, so the next insert restarts
        // the WAL into a new generation.
        let Some(_) = sqlite_cli(
            &path,
            "INSERT INTO t VALUES (1, 1); PRAGMA wal_checkpoint(PASSIVE); \
             INSERT INTO t VALUES (2, 2); INSERT INTO t VALUES (3, 3);",
        ) else {
            eprintln!("sqlite3 shell not available, skipping");
            assert_eq!(sqlite3_close(db), SQLITE_OK);
            return;
        };
        assert_eq!(query_i64(db, "SELECT count(*) FROM t"), 4);
        assert_eq!(exec(db, "INSERT INTO t VALUES (4, 4)"), SQLITE_OK);
        assert_eq!(
            sqlite_cli(
                &path,
                "SELECT count(*), sum(p) FROM t; PRAGMA integrity_check"
            )
            .unwrap(),
            "5|9\nok"
        );
        // Again, with this connection's read transaction open meanwhile.
        assert_eq!(exec(db, "BEGIN"), SQLITE_OK);
        assert_eq!(query_i64(db, "SELECT count(*) FROM t"), 5);
        sqlite_cli(
            &path,
            "PRAGMA wal_checkpoint(PASSIVE); INSERT INTO t VALUES (5, 5);",
        )
        .unwrap();
        assert_eq!(query_i64(db, "SELECT count(*) FROM t"), 5);
        assert_eq!(exec(db, "COMMIT"), SQLITE_OK);
        assert_eq!(query_i64(db, "SELECT count(*) FROM t"), 6);
        assert_eq!(exec(db, "INSERT INTO t VALUES (6, 6)"), SQLITE_OK);
        assert_eq!(query_text(db, "PRAGMA integrity_check"), "ok");
        assert_eq!(sqlite3_close(db), SQLITE_OK);
        assert_eq!(
            sqlite_cli(
                &path,
                "SELECT count(*), sum(p) FROM t; PRAGMA integrity_check"
            )
            .unwrap(),
            "7|20\nok"
        );
    }
}
