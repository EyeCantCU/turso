//! TEXT values and file paths that are not valid UTF-8.

use super::*;
use std::ffi::{CStr, CString};

extern "C" {
    fn sqlite3_result_text(
        context: *mut libc::c_void,
        text: *const libc::c_char,
        len: i32,
        destructor: isize,
    );
}

const RAW: &[u8] = b"xxx\xfayyy";

fn transient() -> Option<unsafe extern "C" fn(*mut libc::c_void)> {
    // SAFETY: SQLITE_TRANSIENT is the sentinel -1, never called.
    unsafe { std::mem::transmute::<isize, _>(-1) }
}

unsafe fn open_memory() -> *mut sqlite3 {
    let mut db = ptr::null_mut();
    assert_eq!(sqlite3_open(c":memory:".as_ptr(), &mut db), SQLITE_OK);
    db
}

unsafe fn exec(db: *mut sqlite3, sql: &str) {
    let sql = CString::new(sql).unwrap();
    let rc = sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "{sql:?}");
}

unsafe fn prepare(db: *mut sqlite3, sql: &str) -> *mut sqlite3_stmt {
    let sql = CString::new(sql).unwrap();
    let mut stmt = ptr::null_mut();
    let rc = sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "{sql:?}");
    stmt
}

unsafe fn column_bytes(stmt: *mut sqlite3_stmt, idx: i32) -> Vec<u8> {
    let p = sqlite3_column_text(stmt, idx) as *const u8;
    let n = sqlite3_column_bytes(stmt, idx) as i32 as usize;
    std::slice::from_raw_parts(p, n).to_vec()
}

unsafe fn bind_raw(stmt: *mut sqlite3_stmt, idx: i32, bytes: &[u8]) {
    let rc = sqlite3_bind_text(
        stmt,
        idx,
        bytes.as_ptr().cast(),
        bytes.len() as i32,
        transient(),
    );
    assert_eq!(rc, SQLITE_OK);
}

#[test]
fn test_invalid_utf8_text_round_trips() {
    unsafe {
        let db = open_memory();
        exec(db, "CREATE TABLE t(x TEXT)");
        let ins = prepare(db, "INSERT INTO t VALUES (?)");
        bind_raw(ins, 1, RAW);
        assert_eq!(sqlite3_step(ins), SQLITE_DONE);
        sqlite3_finalize(ins);

        let sel = prepare(db, "SELECT x, typeof(x), length(x), hex(x), x = ? FROM t");
        bind_raw(sel, 1, RAW);
        assert_eq!(sqlite3_step(sel), SQLITE_ROW);
        assert_eq!(sqlite3_column_type(sel, 0), SQLITE_TEXT);
        assert_eq!(column_bytes(sel, 0), RAW);
        assert_eq!(column_bytes(sel, 1), b"text");
        assert_eq!(sqlite3_column_int64(sel, 2), 7);
        assert_eq!(column_bytes(sel, 3), b"787878FA797979");
        assert_eq!(sqlite3_column_int64(sel, 4), 1);
        sqlite3_finalize(sel);
        sqlite3_close(db);
    }
}

#[test]
fn test_concat_with_invalid_utf8_blob_keeps_bytes() {
    unsafe {
        let db = open_memory();
        // CPython's test_error_msg_decode_error runs this query.
        let stmt = prepare(db, "SELECT 'xxx' || ? || 'yyy' colname");
        let rc = sqlite3_bind_blob(stmt, 1, [0xfau8].as_ptr().cast(), 1, transient());
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
        assert_eq!(sqlite3_column_type(stmt, 0), SQLITE_TEXT);
        assert_eq!(column_bytes(stmt, 0), RAW);
        sqlite3_finalize(stmt);
        sqlite3_close(db);
    }
}

unsafe extern "C" fn echo_raw(ctx: *mut libc::c_void, _argc: i32, argv: *mut *mut libc::c_void) {
    let v = *argv;
    let p = sqlite3_value_text(v);
    let n = sqlite3_value_bytes(v);
    sqlite3_result_text(ctx, p, n, -1);
}

#[test]
fn test_function_values_keep_invalid_utf8() {
    unsafe {
        let db = open_memory();
        let rc = sqlite3_create_function_v2(
            db,
            c"echo_raw".as_ptr(),
            1,
            SQLITE_UTF8,
            ptr::null_mut(),
            Some(echo_raw),
            None,
            None,
            None,
        );
        assert_eq!(rc, SQLITE_OK);
        let stmt = prepare(db, "SELECT echo_raw(?), length(echo_raw(?1))");
        bind_raw(stmt, 1, RAW);
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
        assert_eq!(column_bytes(stmt, 0), RAW);
        assert_eq!(sqlite3_column_int64(stmt, 1), 7);
        sqlite3_finalize(stmt);
        sqlite3_close(db);
    }
}

/// Runs the sqlite3 shell on `path`, if one is installed.
fn sqlite3_shell(path: &std::path::Path, sql: &str) -> Option<String> {
    let out = std::process::Command::new("sqlite3")
        .arg(path)
        .arg(sql)
        .output()
        .ok()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8(out.stdout).unwrap())
}

#[test]
fn test_invalid_utf8_text_is_exchanged_with_the_sqlite3_shell() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        // Written here, read by the shell.
        let ours = dir.path().join("ours.db");
        let ours_c = CString::new(ours.to_str().unwrap()).unwrap();
        let mut db = ptr::null_mut();
        assert_eq!(sqlite3_open(ours_c.as_ptr(), &mut db), SQLITE_OK);
        exec(db, "PRAGMA journal_mode=DELETE");
        exec(db, "CREATE TABLE t(x TEXT)");
        let ins = prepare(db, "INSERT INTO t VALUES (?)");
        bind_raw(ins, 1, RAW);
        assert_eq!(sqlite3_step(ins), SQLITE_DONE);
        sqlite3_finalize(ins);
        sqlite3_close(db);
        let Some(out) = sqlite3_shell(&ours, "SELECT typeof(x), hex(x) FROM t") else {
            return;
        };
        assert_eq!(out.trim(), "text|787878FA797979");

        // Written by the shell, read here.
        let theirs = dir.path().join("theirs.db");
        sqlite3_shell(
            &theirs,
            "CREATE TABLE t(x TEXT); INSERT INTO t VALUES (CAST(x'787878fa797979' AS TEXT));",
        )
        .unwrap();
        let theirs_c = CString::new(theirs.to_str().unwrap()).unwrap();
        let mut db = ptr::null_mut();
        assert_eq!(sqlite3_open(theirs_c.as_ptr(), &mut db), SQLITE_OK);
        let sel = prepare(db, "SELECT x, typeof(x) FROM t");
        assert_eq!(sqlite3_step(sel), SQLITE_ROW);
        assert_eq!(column_bytes(sel, 0), RAW);
        assert_eq!(column_bytes(sel, 1), b"text");
        sqlite3_finalize(sel);
        sqlite3_close(db);
    }
}

#[cfg(unix)]
fn raw_path(dir: &std::path::Path, name: &[u8]) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut path = dir.as_os_str().as_bytes().to_vec();
    path.push(b'/');
    path.extend_from_slice(name);
    path
}

#[cfg(unix)]
#[test]
fn test_open_non_utf8_path() {
    use std::os::unix::ffi::OsStrExt;
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let path = raw_path(dir.path(), b"\xff\xfe.db");
        let path_c = CString::new(path.clone()).unwrap();
        let mut db = ptr::null_mut();
        assert_eq!(sqlite3_open(path_c.as_ptr(), &mut db), SQLITE_OK);
        exec(db, "CREATE TABLE t(x); INSERT INTO t VALUES (1)");
        let name = CStr::from_ptr(sqlite3_db_filename(db, c"main".as_ptr()));
        assert!(name.to_bytes().ends_with(b"/\xff\xfe.db"));
        sqlite3_close(db);
        assert!(std::path::Path::new(std::ffi::OsStr::from_bytes(&path)).exists());

        // The same file through a percent-encoded URI.
        let mut uri = b"file:".to_vec();
        for &b in &path {
            if b.is_ascii_alphanumeric() || b"/._-".contains(&b) {
                uri.push(b);
            } else {
                uri.extend_from_slice(format!("%{b:02X}").as_bytes());
            }
        }
        let uri_c = CString::new(uri).unwrap();
        let mut db = ptr::null_mut();
        let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_URI;
        assert_eq!(
            sqlite3_open_v2(uri_c.as_ptr(), &mut db, flags, ptr::null()),
            SQLITE_OK
        );
        let stmt = prepare(db, "SELECT count(*) FROM t");
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
        assert_eq!(sqlite3_column_int64(stmt, 0), 1);
        sqlite3_finalize(stmt);
        sqlite3_close(db);

        // And as a plain filename through sqlite3_open_v2.
        let mut db = ptr::null_mut();
        let flags = SQLITE_OPEN_READWRITE;
        assert_eq!(
            sqlite3_open_v2(path_c.as_ptr(), &mut db, flags, ptr::null()),
            SQLITE_OK
        );
        let stmt = prepare(db, "SELECT count(*) FROM t");
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
        assert_eq!(sqlite3_column_int64(stmt, 0), 1);
        sqlite3_finalize(stmt);
        sqlite3_close(db);
    }
}
