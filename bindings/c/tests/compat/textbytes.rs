//! TEXT values and file paths that are not valid UTF-8.

use super::*;
use std::ffi::{CStr, CString};

extern "C" {
    fn sqlite3_create_collation_v2(
        db: *mut sqlite3,
        name: *const libc::c_char,
        enc: i32,
        ctx: *mut libc::c_void,
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
    fn sqlite3_mprintf(fmt: *const libc::c_char, ...) -> *mut libc::c_char;
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

#[test]
fn test_mprintf_keeps_invalid_utf8() {
    unsafe {
        let arg = CString::new(&b"it's\xfa"[..]).unwrap();
        let fmt = CString::new(&b"\xff%s|%q|%Q|%.2s"[..]).unwrap();
        let out = sqlite3_mprintf(
            fmt.as_ptr(),
            arg.as_ptr(),
            arg.as_ptr(),
            arg.as_ptr(),
            c"\xc3\xa9x".as_ptr(),
        );
        assert!(!out.is_null());
        let got = CStr::from_ptr(out).to_bytes().to_vec();
        sqlite3_free(out.cast());
        assert_eq!(got, b"\xffit's\xfa|it''s\xfa|'it''s\xfa'|\xc3\xa9");
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

unsafe fn one_text(db: *mut sqlite3, sql: &str) -> String {
    let stmt = prepare(db, sql);
    assert_eq!(sqlite3_step(stmt), SQLITE_ROW, "{sql}");
    let s = String::from_utf8(column_bytes(stmt, 0)).unwrap();
    sqlite3_finalize(stmt);
    s
}

const UTF16_VALUES: [&str; 6] = [
    "ab",
    "a\u{100}",
    "h\u{e9}llo",
    "\u{FF5E}",
    "\u{1F600}",
    "Zed",
];
const ORDER_SQL: &str = "SELECT group_concat(x, ',') FROM (SELECT x FROM t ORDER BY x)";
const NOCASE_ORDER_SQL: &str = "SELECT group_concat(y, ',') FROM (SELECT y FROM t ORDER BY y)";

#[test]
fn test_utf16_database_written_by_sqlite_is_read_and_written() {
    unsafe {
        for enc in ["UTF-16le", "UTF-16be"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("u16.db");
            let mut setup = format!(
                "PRAGMA encoding='{enc}'; CREATE TABLE t(x TEXT, y TEXT COLLATE NOCASE); \
                 CREATE INDEX tx ON t(x); CREATE INDEX ty ON t(y);"
            );
            for v in UTF16_VALUES {
                setup.push_str(&format!("INSERT INTO t VALUES ('{v}', upper('{v}'));"));
            }
            if sqlite3_shell(&path, &setup).is_none() {
                return;
            }
            let shell_order = sqlite3_shell(&path, ORDER_SQL).unwrap();

            let path_c = CString::new(path.to_str().unwrap()).unwrap();
            let mut db = ptr::null_mut();
            assert_eq!(sqlite3_open(path_c.as_ptr(), &mut db), SQLITE_OK);
            exec(db, "PRAGMA journal_mode=DELETE");
            assert_eq!(one_text(db, "PRAGMA encoding"), enc);
            assert_eq!(one_text(db, ORDER_SQL), shell_order.trim());
            assert_eq!(
                one_text(db, "SELECT x FROM t WHERE x = 'h\u{e9}llo'"),
                "h\u{e9}llo"
            );
            assert_eq!(one_text(db, "SELECT x FROM t WHERE y = 'zed'"), "Zed");
            let ins = prepare(db, "INSERT INTO t VALUES (?1, ?1)");
            bind_raw(ins, 1, "new \u{1F601} row".as_bytes());
            assert_eq!(sqlite3_step(ins), SQLITE_DONE);
            sqlite3_finalize(ins);
            assert_eq!(one_text(db, "PRAGMA integrity_check"), "ok");
            let ours = one_text(db, ORDER_SQL);
            let ours_nocase = one_text(db, NOCASE_ORDER_SQL);
            sqlite3_close(db);

            let check = sqlite3_shell(&path, "PRAGMA integrity_check").unwrap();
            assert_eq!(check.trim(), "ok");
            assert_eq!(sqlite3_shell(&path, ORDER_SQL).unwrap().trim(), ours);
            assert_eq!(
                sqlite3_shell(&path, NOCASE_ORDER_SQL).unwrap().trim(),
                ours_nocase
            );
            let got = sqlite3_shell(&path, "SELECT x FROM t WHERE x LIKE 'new%'").unwrap();
            assert_eq!(got.trim(), "new \u{1F601} row");
        }
    }
}

#[test]
fn test_utf16_database_created_here_is_read_by_sqlite() {
    unsafe {
        for enc in ["UTF-16le", "UTF-16be", "UTF-16"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("new16.db");
            let path_c = CString::new(path.to_str().unwrap()).unwrap();
            let mut db = ptr::null_mut();
            assert_eq!(sqlite3_open(path_c.as_ptr(), &mut db), SQLITE_OK);
            exec(db, &format!("PRAGMA encoding='{enc}'"));
            exec(db, "PRAGMA journal_mode=DELETE");
            exec(db, "CREATE TABLE t(x TEXT, y TEXT COLLATE NOCASE)");
            exec(db, "CREATE INDEX tx ON t(x)");
            exec(db, "CREATE INDEX ty ON t(y)");
            let ins = prepare(db, "INSERT INTO t VALUES (?1, upper(?1))");
            for v in UTF16_VALUES {
                bind_raw(ins, 1, v.as_bytes());
                assert_eq!(sqlite3_step(ins), SQLITE_DONE);
                sqlite3_reset(ins);
            }
            sqlite3_finalize(ins);
            let ours = one_text(db, ORDER_SQL);
            let actual_enc = one_text(db, "PRAGMA encoding");
            assert_eq!(one_text(db, "PRAGMA integrity_check"), "ok");
            sqlite3_close(db);

            let Some(shell_enc) = sqlite3_shell(&path, "PRAGMA encoding") else {
                return;
            };
            assert_eq!(shell_enc.trim(), actual_enc);
            assert!(actual_enc.starts_with("UTF-16"));
            let check = sqlite3_shell(&path, "PRAGMA integrity_check").unwrap();
            assert_eq!(check.trim(), "ok");
            assert_eq!(sqlite3_shell(&path, ORDER_SQL).unwrap().trim(), ours);
            let got = sqlite3_shell(&path, "SELECT y FROM t WHERE y = 'h\u{e9}lLo'").unwrap();
            assert_eq!(got.trim(), "H\u{e9}LLO");
        }
    }
}

/// Records both operands of every call and orders by reversed memcmp.
unsafe extern "C" fn recording_collation(
    ctx: *mut libc::c_void,
    n1: i32,
    p1: *const libc::c_void,
    n2: i32,
    p2: *const libc::c_void,
) -> i32 {
    let seen = &mut *(ctx as *mut Vec<Vec<u8>>);
    let a = std::slice::from_raw_parts(p1 as *const u8, n1 as usize);
    let b = std::slice::from_raw_parts(p2 as *const u8, n2 as usize);
    seen.push(a.to_vec());
    seen.push(b.to_vec());
    b.cmp(a) as i32
}

#[test]
fn test_collation_receives_its_declared_encoding() {
    unsafe {
        // SQLITE_UTF8 = 1, SQLITE_UTF16LE = 2, SQLITE_UTF16BE = 3, SQLITE_UTF16_ALIGNED = 8.
        let native: &[u8] = if cfg!(target_endian = "little") {
            b"\xe9\0"
        } else {
            b"\0\xe9"
        };
        for (enc, expected) in [
            (1, &b"\xc3\xa9"[..]),
            (2, &b"\xe9\0"[..]),
            (3, &b"\0\xe9"[..]),
            (8, native),
        ] {
            let db = open_memory();
            let mut seen: Vec<Vec<u8>> = Vec::new();
            let rc = sqlite3_create_collation_v2(
                db,
                c"rec".as_ptr(),
                enc,
                (&mut seen as *mut Vec<Vec<u8>>).cast(),
                Some(recording_collation),
                None,
            );
            assert_eq!(rc, SQLITE_OK);
            let order = one_text(
                db,
                "SELECT group_concat(v, ',') FROM (SELECT column1 v FROM (VALUES ('\u{e9}'), ('a')) ORDER BY v COLLATE rec)",
            );
            assert_eq!(order, "\u{e9},a");
            assert!(seen.iter().any(|v| v == expected), "enc {enc}: {seen:?}");
            sqlite3_close(db);
        }
        let db = open_memory();
        let rc = sqlite3_create_collation_v2(
            db,
            c"bad".as_ptr(),
            9,
            ptr::null_mut(),
            Some(recording_collation),
            None,
        );
        assert_eq!(rc, SQLITE_MISUSE);
        sqlite3_close(db);
    }
}
