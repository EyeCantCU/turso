//! Entry points and built-in virtual tables used by the sqlite3 shell.
//! Expected values are SQLite 3.54's; the virtual table tests need a SQLite
//! built with DBPAGE_VTAB, BYTECODE_VTAB and STMTVTAB (Wolfi sqlite-libs).

use libc::{c_char, c_int, c_uint, c_void};
use std::ffi::{CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::Mutex;

use super::{
    sqlite3, sqlite3_close, sqlite3_column_count, sqlite3_column_name, sqlite3_column_text,
    sqlite3_column_type, sqlite3_column_value, sqlite3_create_function_v2, sqlite3_db_filename,
    sqlite3_errmsg, sqlite3_exec, sqlite3_finalize, sqlite3_open, sqlite3_prepare_v2,
    sqlite3_reset, sqlite3_step, sqlite3_stmt, sqlite3_table_column_metadata, sqlite3_value_text,
};

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_BUSY: c_int = 5;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_UTF8: c_int = 1;

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
#[allow(clashing_extern_declarations)]
extern "C" {
    fn sqlite3_get_auxdata(ctx: *mut c_void, n: c_int) -> *mut c_void;
    fn sqlite3_set_auxdata(
        ctx: *mut c_void,
        n: c_int,
        p: *mut c_void,
        d: Option<unsafe extern "C" fn(*mut c_void)>,
    );
    fn sqlite3_get_clientdata(db: *mut sqlite3, name: *const c_char) -> *mut c_void;
    fn sqlite3_set_clientdata(
        db: *mut sqlite3,
        name: *const c_char,
        p: *mut c_void,
        d: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    fn sqlite3_mutex_alloc(id: c_int) -> *mut c_void;
    fn sqlite3_mutex_free(m: *mut c_void);
    fn sqlite3_mutex_enter(m: *mut c_void);
    fn sqlite3_mutex_try(m: *mut c_void) -> c_int;
    fn sqlite3_mutex_leave(m: *mut c_void);
    fn sqlite3_strglob(pattern: *const c_char, s: *const c_char) -> c_int;
    fn sqlite3_strlike(pattern: *const c_char, s: *const c_char, esc: c_uint) -> c_int;
    fn sqlite3_txn_state(db: *mut sqlite3, schema: *const c_char) -> c_int;
    fn sqlite3_randomness(n: c_int, p: *mut c_void);
    fn sqlite3_result_blob64(
        ctx: *mut c_void,
        z: *const c_void,
        n: u64,
        d: Option<unsafe extern "C" fn(*mut c_void)>,
    );
    fn sqlite3_result_text16le(
        ctx: *mut c_void,
        z: *const c_void,
        n: c_int,
        d: Option<unsafe extern "C" fn(*mut c_void)>,
    );
    fn sqlite3_result_text16be(
        ctx: *mut c_void,
        z: *const c_void,
        n: c_int,
        d: Option<unsafe extern "C" fn(*mut c_void)>,
    );
    fn sqlite3_bind_value(s: *mut sqlite3_stmt, i: c_int, v: *mut c_void) -> c_int;
    fn sqlite3_bind_pointer(
        s: *mut sqlite3_stmt,
        i: c_int,
        p: *mut c_void,
        ty: *const c_char,
        d: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    fn sqlite3_value_pointer(v: *mut c_void, ty: *const c_char) -> *mut c_void;
    fn sqlite3_column_database_name(s: *mut sqlite3_stmt, i: c_int) -> *const c_char;
    fn sqlite3_filename_database(f: *const c_char) -> *const c_char;
    fn sqlite3_overload_function(db: *mut sqlite3, name: *const c_char, n: c_int) -> c_int;
    fn sqlite3_result_int64(ctx: *mut c_void, v: i64);
    fn sqlite3_result_null(ctx: *mut c_void);
    fn sqlite3_value_int64(v: *mut c_void) -> i64;
    fn sqlite3_db_config(db: *mut sqlite3, op: c_int, ...) -> c_int;
}

#[cfg(not(feature = "sqlite3"))]
extern "C" {
    // Only in SQLITE_DEBUG builds of SQLite.
    fn sqlite3_mutex_held(m: *mut c_void) -> c_int;
    fn sqlite3_mutex_notheld(m: *mut c_void) -> c_int;
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

unsafe fn errmsg(db: *mut sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db))
        .to_string_lossy()
        .into_owned()
}

unsafe fn open() -> *mut sqlite3 {
    let mut db = ptr::null_mut();
    assert_eq!(sqlite3_open(c(":memory:").as_ptr(), &mut db), SQLITE_OK);
    db
}

unsafe fn open_file(dir: &tempfile::TempDir) -> *mut sqlite3 {
    let path = dir.path().join("t.db");
    let mut db = ptr::null_mut();
    let p = c(path.to_str().unwrap());
    assert_eq!(sqlite3_open(p.as_ptr(), &mut db), SQLITE_OK);
    db
}

unsafe fn exec(db: *mut sqlite3, sql: &str) -> c_int {
    sqlite3_exec(db, c(sql).as_ptr(), None, ptr::null_mut(), ptr::null_mut())
}

unsafe fn exec_ok(db: *mut sqlite3, sql: &str) {
    assert_eq!(exec(db, sql), SQLITE_OK, "{sql}: {}", errmsg(db));
}

unsafe fn prepare(db: *mut sqlite3, sql: &str) -> *mut sqlite3_stmt {
    let mut stmt = ptr::null_mut();
    let rc = sqlite3_prepare_v2(db, c(sql).as_ptr(), -1, &mut stmt, ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "{sql}: {}", errmsg(db));
    stmt
}

unsafe fn text(stmt: *mut sqlite3_stmt, i: c_int) -> String {
    let p = sqlite3_column_text(stmt, i);
    if p.is_null() {
        "NULL".to_string()
    } else {
        CStr::from_ptr(p as *const c_char)
            .to_string_lossy()
            .into_owned()
    }
}

/// Rows joined with '|', NULL as "NULL".
unsafe fn query(db: *mut sqlite3, sql: &str) -> Vec<String> {
    let stmt = prepare(db, sql);
    let mut rows = Vec::new();
    loop {
        match sqlite3_step(stmt) {
            SQLITE_ROW => {
                let n = sqlite3_column_count(stmt);
                rows.push((0..n).map(|i| text(stmt, i)).collect::<Vec<_>>().join("|"));
            }
            SQLITE_DONE => break,
            rc => panic!("{sql}: step rc={rc}: {}", errmsg(db)),
        }
    }
    sqlite3_finalize(stmt);
    rows
}

unsafe fn query1(db: *mut sqlite3, sql: &str) -> String {
    query(db, sql).into_iter().next().unwrap_or_default()
}

/// Steps `sql` to its first error and returns the message.
unsafe fn query_err(db: *mut sqlite3, sql: &str) -> String {
    let mut stmt = ptr::null_mut();
    if sqlite3_prepare_v2(db, c(sql).as_ptr(), -1, &mut stmt, ptr::null_mut()) != SQLITE_OK {
        return errmsg(db);
    }
    loop {
        match sqlite3_step(stmt) {
            SQLITE_ROW => continue,
            SQLITE_DONE => {
                sqlite3_finalize(stmt);
                panic!("{sql}: no error");
            }
            _ => {
                let m = errmsg(db);
                sqlite3_finalize(stmt);
                return m;
            }
        }
    }
}

// ------------------------------------------------------------ auxdata

static AUX_BUILT: AtomicI32 = AtomicI32::new(0);
static AUX_FREED: AtomicI32 = AtomicI32::new(0);

unsafe extern "C" fn aux_free(p: *mut c_void) {
    AUX_FREED.fetch_add(1, Ordering::SeqCst);
    drop(Box::from_raw(p as *mut i64));
}

/// auxf(key, x): caches a value for argument 0 and returns how many calls
/// have reused it.
unsafe extern "C" fn auxf(ctx: *mut c_void, _argc: c_int, _argv: *mut *mut c_void) {
    let p = sqlite3_get_auxdata(ctx, 0) as *mut i64;
    if p.is_null() {
        AUX_BUILT.fetch_add(1, Ordering::SeqCst);
        let b = Box::into_raw(Box::new(0i64));
        sqlite3_set_auxdata(ctx, 0, b.cast(), Some(aux_free));
        sqlite3_result_int64(ctx, 0);
    } else {
        *p += 1;
        sqlite3_result_int64(ctx, *p);
    }
}

/// auxset(x): sets argument 0's auxdata twice in one call.
unsafe extern "C" fn auxset(ctx: *mut c_void, _argc: c_int, _argv: *mut *mut c_void) {
    for _ in 0..2 {
        let b = Box::into_raw(Box::new(0i64));
        sqlite3_set_auxdata(ctx, 0, b.cast(), Some(aux_free));
    }
    sqlite3_result_null(ctx);
}

#[test]
fn test_cli_auxdata() {
    unsafe {
        let db = open();
        exec_ok(
            db,
            "CREATE TABLE t(x); INSERT INTO t VALUES(1),(2),(3),(4),(5)",
        );
        for (name, n, f) in [
            ("auxf", 2, auxf as unsafe extern "C" fn(_, _, _)),
            ("auxset", 1, auxset),
        ] {
            let rc = sqlite3_create_function_v2(
                db,
                c(name).as_ptr(),
                n,
                SQLITE_UTF8,
                ptr::null_mut(),
                Some(f),
                None,
                None,
                None,
            );
            assert_eq!(rc, SQLITE_OK);
        }
        // A constant argument keeps its data across rows.
        AUX_BUILT.store(0, Ordering::SeqCst);
        AUX_FREED.store(0, Ordering::SeqCst);
        assert_eq!(
            query(db, "SELECT auxf('k', x) FROM t"),
            ["0", "1", "2", "3", "4"]
        );
        assert_eq!(AUX_BUILT.load(Ordering::SeqCst), 1);
        assert_eq!(AUX_FREED.load(Ordering::SeqCst), 1);
        // A column argument loses it after every call.
        AUX_BUILT.store(0, Ordering::SeqCst);
        AUX_FREED.store(0, Ordering::SeqCst);
        assert_eq!(
            query(db, "SELECT auxf(x, 'k') FROM t"),
            ["0", "0", "0", "0", "0"]
        );
        assert_eq!(AUX_BUILT.load(Ordering::SeqCst), 5);
        assert_eq!(AUX_FREED.load(Ordering::SeqCst), 5);
        // Two call sites keep separate data; it is freed when the
        // statement finishes, before finalize.
        AUX_BUILT.store(0, Ordering::SeqCst);
        AUX_FREED.store(0, Ordering::SeqCst);
        let stmt = prepare(db, "SELECT auxf('a', x), auxf('a', x) FROM t");
        let mut rows = Vec::new();
        while sqlite3_step(stmt) == SQLITE_ROW {
            rows.push(format!("{}|{}", text(stmt, 0), text(stmt, 1)));
        }
        assert_eq!(rows, ["0|0", "1|1", "2|2", "3|3", "4|4"]);
        assert_eq!(AUX_BUILT.load(Ordering::SeqCst), 2);
        assert_eq!(AUX_FREED.load(Ordering::SeqCst), 2);
        // A reset before the end frees it too.
        sqlite3_reset(stmt);
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
        assert_eq!(AUX_BUILT.load(Ordering::SeqCst), 4);
        sqlite3_reset(stmt);
        assert_eq!(AUX_FREED.load(Ordering::SeqCst), 4);
        sqlite3_finalize(stmt);
        // Replacing data in the same call frees the old value at once.
        AUX_FREED.store(0, Ordering::SeqCst);
        assert_eq!(query(db, "SELECT auxset('k')"), ["NULL"]);
        assert_eq!(AUX_FREED.load(Ordering::SeqCst), 2);
        sqlite3_close(db);
    }
}

// ------------------------------------------------------------ client data

static CD_FREED: Mutex<Vec<usize>> = Mutex::new(Vec::new());

unsafe extern "C" fn cd_free(p: *mut c_void) {
    CD_FREED.lock().unwrap().push(p as usize);
}

#[test]
fn test_cli_clientdata() {
    unsafe {
        CD_FREED.lock().unwrap().clear();
        let db = open();
        let (a, b, d) = (c("a"), c("b"), c("d"));
        assert!(sqlite3_get_clientdata(db, a.as_ptr()).is_null());
        let p1 = 0x1000usize as *mut c_void;
        let p2 = 0x2000usize as *mut c_void;
        let p3 = 0x3000usize as *mut c_void;
        let p4 = 0x4000usize as *mut c_void;
        assert_eq!(
            sqlite3_set_clientdata(db, a.as_ptr(), p1, Some(cd_free)),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3_set_clientdata(db, b.as_ptr(), p2, Some(cd_free)),
            SQLITE_OK
        );
        assert_eq!(sqlite3_get_clientdata(db, a.as_ptr()), p1);
        assert_eq!(sqlite3_get_clientdata(db, b.as_ptr()), p2);
        // Replacing runs the old destructor.
        assert_eq!(
            sqlite3_set_clientdata(db, a.as_ptr(), p3, Some(cd_free)),
            SQLITE_OK
        );
        assert_eq!(sqlite3_get_clientdata(db, a.as_ptr()), p3);
        assert_eq!(*CD_FREED.lock().unwrap(), [0x1000]);
        // NULL removes the entry and runs its destructor.
        assert_eq!(
            sqlite3_set_clientdata(db, b.as_ptr(), ptr::null_mut(), None),
            SQLITE_OK
        );
        assert!(sqlite3_get_clientdata(db, b.as_ptr()).is_null());
        assert_eq!(*CD_FREED.lock().unwrap(), [0x1000, 0x2000]);
        // Names are per connection.
        let db2 = open();
        assert!(sqlite3_get_clientdata(db2, a.as_ptr()).is_null());
        sqlite3_close(db2);
        assert_eq!(
            sqlite3_set_clientdata(db, d.as_ptr(), p4, Some(cd_free)),
            SQLITE_OK
        );
        // Close runs the remaining destructors.
        sqlite3_close(db);
        let mut freed = CD_FREED.lock().unwrap().clone();
        freed.sort();
        assert_eq!(freed, [0x1000, 0x2000, 0x3000, 0x4000]);
    }
}

// ------------------------------------------------------------ mutexes

struct SendPtr(*mut c_void);
unsafe impl Send for SendPtr {}

unsafe fn try_elsewhere(m: *mut c_void) -> c_int {
    let p = SendPtr(m);
    std::thread::spawn(move || {
        let p = p;
        let rc = sqlite3_mutex_try(p.0);
        if rc == SQLITE_OK {
            sqlite3_mutex_leave(p.0);
        }
        rc
    })
    .join()
    .unwrap()
}

#[test]
fn test_cli_mutex() {
    unsafe {
        // Recursive: the owner may enter again; others are refused.
        let m = sqlite3_mutex_alloc(1);
        assert!(!m.is_null());
        sqlite3_mutex_enter(m);
        sqlite3_mutex_enter(m);
        assert_eq!(sqlite3_mutex_try(m), SQLITE_OK);
        assert_eq!(try_elsewhere(m), SQLITE_BUSY);
        sqlite3_mutex_leave(m);
        sqlite3_mutex_leave(m);
        assert_eq!(try_elsewhere(m), SQLITE_BUSY);
        sqlite3_mutex_leave(m);
        assert_eq!(try_elsewhere(m), SQLITE_OK);
        sqlite3_mutex_free(m);
        // Fast.
        let f = sqlite3_mutex_alloc(0);
        assert!(!f.is_null());
        sqlite3_mutex_enter(f);
        assert_eq!(try_elsewhere(f), SQLITE_BUSY);
        sqlite3_mutex_leave(f);
        assert_eq!(try_elsewhere(f), SQLITE_OK);
        sqlite3_mutex_free(f);
        // Static mutexes are shared and never freed.
        let s1 = sqlite3_mutex_alloc(9);
        assert!(!s1.is_null());
        assert_eq!(s1, sqlite3_mutex_alloc(9));
        assert_ne!(s1, sqlite3_mutex_alloc(10));
        sqlite3_mutex_enter(s1);
        assert_eq!(try_elsewhere(s1), SQLITE_BUSY);
        sqlite3_mutex_leave(s1);
        // Waiting: another thread blocks in enter until we leave.
        let r = sqlite3_mutex_alloc(1);
        sqlite3_mutex_enter(r);
        let p = SendPtr(r);
        let t = std::thread::spawn(move || {
            let p = p;
            sqlite3_mutex_enter(p.0);
            sqlite3_mutex_leave(p.0);
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        sqlite3_mutex_leave(r);
        t.join().unwrap();
        sqlite3_mutex_free(r);
        // NULL is accepted everywhere.
        sqlite3_mutex_enter(ptr::null_mut());
        assert_eq!(sqlite3_mutex_try(ptr::null_mut()), SQLITE_OK);
        sqlite3_mutex_leave(ptr::null_mut());
        sqlite3_mutex_free(ptr::null_mut());
    }
}

#[cfg(not(feature = "sqlite3"))]
#[test]
fn test_cli_mutex_held() {
    unsafe {
        let m = sqlite3_mutex_alloc(1);
        assert_eq!(sqlite3_mutex_held(m), 0);
        assert_eq!(sqlite3_mutex_notheld(m), 1);
        sqlite3_mutex_enter(m);
        assert_eq!(sqlite3_mutex_held(m), 1);
        assert_eq!(sqlite3_mutex_notheld(m), 0);
        sqlite3_mutex_leave(m);
        sqlite3_mutex_free(m);
        assert_eq!(sqlite3_mutex_held(ptr::null_mut()), 1);
        assert_eq!(sqlite3_mutex_notheld(ptr::null_mut()), 1);
        assert!(sqlite3_mutex_alloc(14).is_null());
        assert!(sqlite3_mutex_alloc(-1).is_null());
    }
}

// ------------------------------------------------------------ strings

#[test]
fn test_cli_strglob_strlike() {
    unsafe {
        let glob = |p: &str, s: &str| sqlite3_strglob(c(p).as_ptr(), c(s).as_ptr());
        let like =
            |p: &str, s: &str, e: u8| sqlite3_strlike(c(p).as_ptr(), c(s).as_ptr(), e as c_uint);
        let globs = [
            ("*.txt", "a.txt", 0),
            ("*.txt", "a.TXT", 2),
            ("a?c", "abc", 0),
            ("a?c", "ac", 1),
            ("[a-c]x", "bx", 0),
            ("[^a-c]x", "bx", 1),
            ("[]]", "]", 0),
            ("[a-]", "-", 0),
            ("*[0-9]", "abc", 2),
            ("*[0-9]", "ab3", 0),
            ("*", "", 0),
            ("a*", "b", 1),
            ("*x", "abc", 2),
            ("CREATE TABLE ['\"]*", "CREATE TABLE \"t\"(x)", 0),
            ("h\u{e9}*", "h\u{e9}llo", 0),
            ("*\u{e9}", "abc", 2),
            ("[", "[", 1),
        ];
        for (p, s, want) in globs {
            assert_eq!(glob(p, s), want, "glob({p:?}, {s:?})");
        }
        let likes = [
            ("abc%", "ABCdef", 0, 0),
            ("a_c", "AbC", 0, 0),
            ("%b%", "ABC", 0, 0),
            ("%z", "abc", 0, 2),
            ("a\\%", "a%", b'\\', 0),
            ("a\\%", "ab", b'\\', 1),
            ("a\\_c", "abc", b'\\', 1),
            ("temp", "TEMP", 0, 0),
            ("CREATE VIEW%", "create view v as select 1", 0, 0),
            ("\u{e9}", "\u{c9}", 0, 1),
            ("%", "", 0, 0),
            ("a%%b", "axxb", 0, 0),
        ];
        for (p, s, e, want) in likes {
            assert_eq!(like(p, s, e), want, "like({p:?}, {s:?}, {e})");
        }
        assert_eq!(sqlite3_strglob(ptr::null(), ptr::null()), 0);
        assert_eq!(sqlite3_strglob(c("a").as_ptr(), ptr::null()), 1);
        assert_eq!(sqlite3_strglob(ptr::null(), c("a").as_ptr()), 1);
        assert_eq!(sqlite3_strlike(ptr::null(), ptr::null(), 0), 0);
        assert_eq!(sqlite3_strlike(c("a").as_ptr(), ptr::null(), 0), 1);
    }
}

// ------------------------------------------------------------ txn state

#[test]
fn test_cli_txn_state() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_file(&dir);
        let main = c("main");
        let state =
            |s: Option<&CString>| sqlite3_txn_state(db, s.map_or(ptr::null(), |s| s.as_ptr()));
        exec_ok(db, "CREATE TABLE t(x)");
        assert_eq!(state(None), 0);
        assert_eq!(state(Some(&main)), 0);
        assert_eq!(state(Some(&c("nosuch"))), -1);
        exec_ok(db, "BEGIN");
        assert_eq!(state(None), 0);
        assert_eq!(query(db, "SELECT count(*) FROM t"), ["0"]);
        assert_eq!(state(None), 1);
        assert_eq!(state(Some(&c("MAIN"))), 1);
        exec_ok(db, "INSERT INTO t VALUES(1)");
        assert_eq!(state(None), 2);
        assert_eq!(state(Some(&main)), 2);
        exec_ok(db, "COMMIT");
        assert_eq!(state(None), 0);
        exec_ok(db, "BEGIN IMMEDIATE");
        assert_eq!(state(Some(&main)), 2);
        exec_ok(db, "ROLLBACK");
        assert_eq!(state(Some(&main)), 0);
        sqlite3_close(db);
    }
}

// ------------------------------------------------------------ results

static RES_FREED: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn res_free(_p: *mut c_void) {
    RES_FREED.fetch_add(1, Ordering::SeqCst);
}

static BLOB: [u8; 4] = [1, 2, 3, 0xff];
static LE: [u8; 12] = [b'h', 0, 0xe9, 0, 0x3d, 0xd8, 0x00, 0xde, b'!', 0, 0, 0];
static BE: [u8; 12] = [0, b'h', 0, 0xe9, 0xd8, 0x3d, 0xde, 0x00, 0, b'!', 0, 0];

/// res(kind): blob64, blob64-big, le, le-nul, be, be-odd.
unsafe extern "C" fn resf(ctx: *mut c_void, _argc: c_int, argv: *mut *mut c_void) {
    let kind = CStr::from_ptr(sqlite3_value_text(*argv) as *const c_char)
        .to_str()
        .unwrap()
        .to_string();
    let d = Some(res_free as unsafe extern "C" fn(*mut c_void));
    match kind.as_str() {
        "blob64" => sqlite3_result_blob64(ctx, BLOB.as_ptr().cast(), 4, d),
        "blob64-big" => sqlite3_result_blob64(ctx, BLOB.as_ptr().cast(), 0x8000_0000, d),
        "le" => sqlite3_result_text16le(ctx, LE.as_ptr().cast(), 10, d),
        "le-nul" => sqlite3_result_text16le(ctx, LE.as_ptr().cast(), -1, d),
        "be" => sqlite3_result_text16be(ctx, BE.as_ptr().cast(), 10, d),
        "be-odd" => sqlite3_result_text16be(ctx, BE.as_ptr().cast(), 5, d),
        _ => sqlite3_result_null(ctx),
    }
}

#[test]
fn test_cli_result_blob64_text16() {
    unsafe {
        let db = open();
        let rc = sqlite3_create_function_v2(
            db,
            c("res").as_ptr(),
            1,
            SQLITE_UTF8,
            ptr::null_mut(),
            Some(resf),
            None,
            None,
            None,
        );
        assert_eq!(rc, SQLITE_OK);
        RES_FREED.store(0, Ordering::SeqCst);
        assert_eq!(
            query(db, "SELECT hex(res('blob64')), typeof(res('blob64'))"),
            ["010203FF|blob"]
        );
        assert_eq!(
            query(db, "SELECT res('le'), length(res('le')), hex(res('le'))"),
            ["h\u{e9}\u{1f600}!|4|68C3A9F09F988021"]
        );
        assert_eq!(query(db, "SELECT res('le-nul')"), ["h\u{e9}\u{1f600}!"]);
        assert_eq!(query(db, "SELECT res('be')"), ["h\u{e9}\u{1f600}!"]);
        assert_eq!(query(db, "SELECT res('be-odd')"), ["h\u{e9}"]);
        assert_eq!(RES_FREED.load(Ordering::SeqCst), 8);
        // The text is sqlite3_result_error_toobig's (compat/errmsg on turso).
        let msg = query_err(db, "SELECT res('blob64-big')");
        assert!(!msg.is_empty());
        assert_eq!(RES_FREED.load(Ordering::SeqCst), 9);
        sqlite3_close(db);
    }
}

// ------------------------------------------------------------ binding

#[test]
fn test_cli_bind_value() {
    unsafe {
        let db = open();
        let src = prepare(db, "SELECT 42, 2.5, 'h\u{e9}', x'00ff', NULL, x''");
        assert_eq!(sqlite3_step(src), SQLITE_ROW);
        let dst = prepare(db, "SELECT typeof(?1), quote(?1)");
        let mut out = Vec::new();
        for i in 0..6 {
            assert_eq!(
                sqlite3_bind_value(dst, 1, sqlite3_column_value(src, i)),
                SQLITE_OK
            );
            assert_eq!(sqlite3_step(dst), SQLITE_ROW);
            out.push(format!("{}|{}", text(dst, 0), text(dst, 1)));
            sqlite3_reset(dst);
        }
        assert_eq!(
            out,
            [
                "integer|42",
                "real|2.5",
                "text|'h\u{e9}'",
                "blob|X'00FF'",
                "null|NULL",
                "blob|X''"
            ]
        );
        assert_eq!(
            sqlite3_bind_value(dst, 2, sqlite3_column_value(src, 0)),
            25 // SQLITE_RANGE
        );
        sqlite3_finalize(dst);
        sqlite3_finalize(src);
        sqlite3_close(db);
    }
}

static PTR_FREED: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn ptr_free(p: *mut c_void) {
    PTR_FREED.store(p as usize, Ordering::SeqCst);
}

/// getptr(x): the address carried by a "cli-test" pointer, or NULL.
unsafe extern "C" fn getptr(ctx: *mut c_void, _argc: c_int, argv: *mut *mut c_void) {
    let p = sqlite3_value_pointer(*argv, c("cli-test").as_ptr());
    if p.is_null() {
        sqlite3_result_null(ctx);
    } else {
        sqlite3_result_int64(ctx, p as i64);
    }
}

#[test]
fn test_cli_bind_pointer() {
    unsafe {
        let db = open();
        let rc = sqlite3_create_function_v2(
            db,
            c("getptr").as_ptr(),
            1,
            SQLITE_UTF8,
            ptr::null_mut(),
            Some(getptr),
            None,
            None,
            None,
        );
        assert_eq!(rc, SQLITE_OK);
        PTR_FREED.store(0, Ordering::SeqCst);
        let stmt = prepare(db, "SELECT typeof(?1), getptr(?1), quote(?1)");
        let tag = c("cli-test");
        let rc = sqlite3_bind_pointer(stmt, 1, 0x1234 as *mut c_void, tag.as_ptr(), Some(ptr_free));
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
        assert_eq!(text(stmt, 0), "null");
        assert_eq!(text(stmt, 1), (0x1234).to_string());
        assert_eq!(text(stmt, 2), "NULL");
        sqlite3_finalize(stmt);
        assert_eq!(PTR_FREED.load(Ordering::SeqCst), 0x1234);
        // A different type tag reads as NULL.
        let stmt = prepare(db, "SELECT getptr(?1)");
        let other = c("other");
        sqlite3_bind_pointer(stmt, 1, 0x99 as *mut c_void, other.as_ptr(), None);
        assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
        assert_eq!(text(stmt, 0), "NULL");
        // Out of range: the destructor runs at once.
        sqlite3_reset(stmt);
        PTR_FREED.store(0, Ordering::SeqCst);
        let rc = sqlite3_bind_pointer(stmt, 5, 0x77 as *mut c_void, tag.as_ptr(), Some(ptr_free));
        assert_eq!(rc, 25);
        assert_eq!(PTR_FREED.load(Ordering::SeqCst), 0x77);
        sqlite3_finalize(stmt);
        sqlite3_close(db);
    }
}

#[test]
fn test_cli_column_database_name() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_file(&dir);
        exec_ok(db, "CREATE TABLE t(a, b); CREATE TEMP TABLE tt(c)");
        let stmt = prepare(db, "SELECT a, b+1, (SELECT 1) FROM t");
        let name = |s: *mut sqlite3_stmt, i| {
            let p = sqlite3_column_database_name(s, i);
            (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
        };
        assert_eq!(name(stmt, 0).as_deref(), Some("main"));
        assert_eq!(name(stmt, 1), None);
        assert_eq!(name(stmt, 2), None);
        assert_eq!(name(stmt, 3), None);
        assert_eq!(name(stmt, -1), None);
        sqlite3_finalize(stmt);
        let stmt = prepare(db, "SELECT c FROM tt");
        assert_eq!(name(stmt, 0).as_deref(), Some("temp"));
        sqlite3_finalize(stmt);
        let f = sqlite3_db_filename(db, c("main").as_ptr());
        let g = sqlite3_filename_database(f);
        assert_eq!(CStr::from_ptr(g), CStr::from_ptr(f));
        assert!(CStr::from_ptr(g).to_string_lossy().ends_with("t.db"));
        sqlite3_close(db);
    }
}

#[test]
fn test_cli_overload_function() {
    unsafe {
        let db = open();
        assert_eq!(
            sqlite3_overload_function(db, c("nosuchfn").as_ptr(), 2),
            SQLITE_OK
        );
        // turso prefixes function errors until compat/errmsg is merged.
        let msg = query_err(db, "SELECT nosuchfn(1, 2)");
        assert!(
            msg.ends_with("unable to use function nosuchfn in the requested context"),
            "{msg}"
        );
        // Existing functions are left alone.
        assert_eq!(
            sqlite3_overload_function(db, c("upper").as_ptr(), 1),
            SQLITE_OK
        );
        assert_eq!(query(db, "SELECT upper('a')"), ["A"]);
        // A built-in with another argument count gets a placeholder.
        assert_eq!(
            sqlite3_overload_function(db, c("upper").as_ptr(), 3),
            SQLITE_OK
        );
        // (Without compat/udf turso's built-in still wins and reports the
        // argument count; either way the call fails.)
        let msg = query_err(db, "SELECT upper(1, 2, 3)");
        assert!(
            msg.ends_with("unable to use function upper in the requested context")
                || msg.ends_with("wrong number of arguments to function upper()"),
            "{msg}"
        );
        assert_eq!(query(db, "SELECT upper('b')"), ["B"]);
        // A variadic user function already covers any count.
        let rc = sqlite3_create_function_v2(
            db,
            c("anyargs").as_ptr(),
            -1,
            SQLITE_UTF8,
            ptr::null_mut(),
            Some(getptr),
            None,
            None,
            None,
        );
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(
            sqlite3_overload_function(db, c("anyargs").as_ptr(), 2),
            SQLITE_OK
        );
        assert_eq!(query(db, "SELECT anyargs(1, 2)"), ["NULL"]);
        // -1 needs a variadic function: a fixed-count one gets a placeholder.
        assert_eq!(
            sqlite3_overload_function(db, c("lower").as_ptr(), -1),
            SQLITE_OK
        );
        sqlite3_close(db);
    }
}

#[test]
fn test_cli_randomness() {
    unsafe {
        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        sqlite3_randomness(64, a.as_mut_ptr().cast());
        sqlite3_randomness(64, b.as_mut_ptr().cast());
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 64]);
        let mut z = [7u8; 4];
        sqlite3_randomness(0, z.as_mut_ptr().cast());
        assert_eq!(z, [7u8; 4]);
        sqlite3_randomness(-1, ptr::null_mut());
    }
}

// ------------------------------------------------------------ vtabs

unsafe fn column_names(db: *mut sqlite3, sql: &str) -> Vec<String> {
    let stmt = prepare(db, sql);
    let names = (0..sqlite3_column_count(stmt))
        .map(|i| {
            CStr::from_ptr(sqlite3_column_name(stmt, i))
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    sqlite3_finalize(stmt);
    names
}

#[test]
fn test_cli_sqlite_dbpage() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_file(&dir);
        exec_ok(db, "PRAGMA page_size=4096");
        exec_ok(
            db,
            "CREATE TABLE t(a, b); CREATE INDEX ti ON t(b);
             WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i<40)
             INSERT INTO t SELECT i, randomblob(300) FROM s",
        );
        let pages = query1(db, "PRAGMA page_count");
        assert_eq!(
            column_names(db, "SELECT * FROM sqlite_dbpage"),
            ["pgno", "data"]
        );
        assert_eq!(query1(db, "SELECT count(*) FROM sqlite_dbpage"), pages);
        assert_eq!(
            query1(db, "SELECT max(pgno) FROM sqlite_dbpage('main')"),
            pages
        );
        assert_eq!(
            query(
                db,
                "SELECT pgno, length(data), substr(data, 1, 15), schema FROM sqlite_dbpage WHERE pgno=1"
            ),
            ["1|4096|SQLite format 3|main"]
        );
        assert_eq!(
            query(
                db,
                "SELECT pgno FROM sqlite_dbpage WHERE pgno IN (2, 3) ORDER BY pgno"
            ),
            ["2", "3"]
        );
        assert!(query(db, "SELECT * FROM sqlite_dbpage WHERE pgno=0").is_empty());
        assert!(query(db, "SELECT * FROM sqlite_dbpage WHERE pgno=100000").is_empty());
        assert!(query(db, "SELECT * FROM sqlite_dbpage('nosuch')").is_empty());
        assert!(query(db, "SELECT * FROM sqlite_dbpage WHERE schema='nosuch'").is_empty());
        // Page 1's header agrees with PRAGMA page_count (bytes 28..32).
        assert_eq!(
            query1(
                db,
                "SELECT hex(substr(data, 29, 4)) FROM sqlite_dbpage WHERE pgno=1"
            ),
            format!("{:08X}", pages.parse::<u32>().unwrap())
        );
        // Writes: rewrite a page with its own content, then with a change.
        exec_ok(
            db,
            "UPDATE sqlite_dbpage SET data=(SELECT data FROM sqlite_dbpage WHERE pgno=2) WHERE pgno=2",
        );
        assert_eq!(query1(db, "PRAGMA integrity_check"), "ok");
        // The b-tree part of page 1 (COMMIT bumps the change counter).
        let before = query1(
            db,
            "SELECT hex(substr(data, 101)) FROM sqlite_dbpage WHERE pgno=1",
        );
        exec_ok(
            db,
            "INSERT INTO sqlite_dbpage(pgno, data) SELECT 1, data FROM sqlite_dbpage WHERE pgno=1",
        );
        assert_eq!(
            query1(
                db,
                "SELECT hex(substr(data, 101)) FROM sqlite_dbpage WHERE pgno=1"
            ),
            before
        );
        assert_eq!(query1(db, "SELECT count(*) FROM t"), "40");
        // Errors are dbpage.c's.
        assert_eq!(
            query_err(db, "UPDATE sqlite_dbpage SET data=x'00' WHERE pgno=2"),
            "bad page value"
        );
        assert_eq!(
            query_err(db, "DELETE FROM sqlite_dbpage WHERE pgno=2"),
            "cannot delete"
        );
        assert_eq!(
            query_err(
                db,
                "INSERT INTO sqlite_dbpage(pgno, data, schema) VALUES(2, zeroblob(4096), 'nosuch')"
            ),
            "no such schema"
        );
        assert_eq!(
            query_err(
                db,
                "INSERT INTO sqlite_dbpage(pgno, data) VALUES(0, zeroblob(4096))"
            ),
            "bad page number"
        );
        sqlite3_close(db);
    }
}

#[test]
fn test_cli_sqlite_dbpage_truncate() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_file(&dir);
        exec_ok(
            db,
            "PRAGMA page_size=4096; CREATE TABLE t(a); INSERT INTO t VALUES(1);
             CREATE TABLE big(b); INSERT INTO big VALUES(zeroblob(20000)); DROP TABLE big",
        );
        let pages: u32 = query1(db, "PRAGMA page_count").parse().unwrap();
        assert!(pages > 3);
        // Drop the free pages at the end: page 1 gets the new size and an
        // empty freelist, then INSERT (3, NULL) cuts page 3 and after.
        exec_ok(
            db,
            "BEGIN;
             INSERT INTO sqlite_dbpage(pgno, data)
               SELECT 1, CAST(substr(data, 1, 28) || x'00000002' || zeroblob(8)
                              || substr(data, 41) AS BLOB)
               FROM sqlite_dbpage WHERE pgno=1;
             INSERT INTO sqlite_dbpage VALUES(3, NULL);
             COMMIT",
        );
        assert_eq!(query1(db, "SELECT count(*) FROM sqlite_dbpage"), "2");
        assert_eq!(query1(db, "PRAGMA page_count"), "2");
        assert_eq!(query1(db, "PRAGMA integrity_check"), "ok");
        assert_eq!(query1(db, "SELECT a FROM t"), "1");
        assert_eq!(
            query_err(db, "INSERT INTO sqlite_dbpage VALUES(1, NULL)"),
            "bad page value"
        );
        sqlite3_close(db);
    }
}

#[test]
fn test_cli_bytecode_tables_used() {
    unsafe {
        let db = open();
        exec_ok(db, "CREATE TABLE t(a, b); CREATE INDEX tb ON t(b)");
        assert_eq!(
            column_names(db, "SELECT * FROM bytecode('SELECT 1')"),
            [
                "addr", "opcode", "p1", "p2", "p3", "p4", "p5", "comment", "subprog", "nexec",
                "ncycle"
            ]
        );
        assert_eq!(
            column_names(db, "SELECT * FROM tables_used('SELECT 1')"),
            ["type", "schema", "name", "wr", "subprog"]
        );
        assert_eq!(
            query(
                db,
                "SELECT addr, opcode FROM bytecode('SELECT 1') WHERE addr=0"
            ),
            ["0|Init"]
        );
        assert_eq!(
            query1(db, "SELECT count(*) > 2 FROM bytecode('SELECT a FROM t')"),
            "1"
        );
        assert_eq!(
            query1(
                db,
                "SELECT count(*) FROM bytecode('SELECT a FROM t') WHERE subprog IS NOT NULL OR nexec<>0 OR ncycle<>0"
            ),
            "0"
        );
        assert_eq!(
            query(db, "SELECT * FROM tables_used('SELECT a FROM t')"),
            ["table|main|t|0|NULL"]
        );
        assert_eq!(
            query(
                db,
                "SELECT type, name, wr FROM tables_used('SELECT a FROM t WHERE b=1') ORDER BY name"
            ),
            ["table|t|0", "index|tb|0"]
        );
        assert_eq!(
            query(db, "SELECT type, name, wr FROM tables_used('INSERT INTO t VALUES(1, 2)') ORDER BY name"),
            ["table|t|1", "index|tb|1"]
        );
        // Trigger programs are listed after the main one, then dropped by
        // subprog IS NULL.
        exec_ok(
            db,
            "CREATE TABLE log(x); CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES(new.a); END",
        );
        assert_eq!(
            query(
                db,
                "SELECT DISTINCT subprog FROM bytecode('INSERT INTO t VALUES(1, 2)') WHERE subprog IS NOT NULL"
            ),
            ["TRIGGER tr"]
        );
        assert_eq!(
            query(
                db,
                "SELECT name, wr, subprog FROM tables_used('INSERT INTO t VALUES(1, 2)') WHERE name='log'"
            ),
            ["log|1|TRIGGER tr"]
        );
        assert!(query(
            db,
            "SELECT * FROM tables_used('INSERT INTO t VALUES(1, 2)') WHERE subprog IS NULL AND name='log'"
        )
        .is_empty());
        // The statement argument is required and must be SQL text.
        assert_eq!(
            query_err(db, "SELECT * FROM bytecode(1)"),
            "argument to bytecode() is not a valid SQL statement"
        );
        assert_eq!(
            query_err(db, "SELECT * FROM tables_used(NULL)"),
            "argument to tables_used() is not a valid SQL statement"
        );
        let mut stmt = ptr::null_mut();
        assert_eq!(
            sqlite3_prepare_v2(
                db,
                c("SELECT * FROM bytecode").as_ptr(),
                -1,
                &mut stmt,
                ptr::null_mut()
            ),
            SQLITE_ERROR
        );
        sqlite3_close(db);
    }
}

#[test]
fn test_cli_sqlite_stmt() {
    unsafe {
        let db = open();
        exec_ok(db, "CREATE TABLE t(a)");
        assert_eq!(
            column_names(db, "SELECT * FROM sqlite_stmt"),
            [
                "sql", "ncol", "ro", "busy", "nscan", "nsort", "naidx", "nstep", "reprep", "run",
                "mem"
            ]
        );
        let s1 = prepare(db, "SELECT a, a FROM t");
        let s2 = prepare(db, "INSERT INTO t VALUES(1)");
        let mut rows = query(db, "SELECT sql, ncol, ro, busy FROM sqlite_stmt");
        rows.sort();
        // The statement reading sqlite_stmt is busy during its own step.
        assert_eq!(
            rows,
            [
                "INSERT INTO t VALUES(1)|0|0|0",
                "SELECT a, a FROM t|2|1|0",
                "SELECT sql, ncol, ro, busy FROM sqlite_stmt|4|1|1",
            ]
        );
        assert_eq!(sqlite3_step(s2), SQLITE_DONE);
        assert_eq!(
            query(db, "SELECT run FROM sqlite_stmt WHERE sql LIKE 'INSERT%'"),
            ["1"]
        );
        sqlite3_finalize(s1);
        sqlite3_finalize(s2);
        assert_eq!(query1(db, "SELECT count(*) FROM sqlite_stmt"), "1");
        sqlite3_close(db);
    }
}

#[test]
fn test_cli_open_empty_name_and_table_check() {
    unsafe {
        // "" opens a private temporary database (.recover does this).
        let mut db = ptr::null_mut();
        assert_eq!(sqlite3_open(c("").as_ptr(), &mut db), SQLITE_OK);
        exec_ok(db, "CREATE TABLE t(a); CREATE VIEW v AS SELECT a FROM t");
        assert_eq!(query1(db, "SELECT count(*) FROM t"), "0");
        // A NULL column only checks that the table exists (.import).
        let meta = |name: &str| {
            sqlite3_table_column_metadata(
                db,
                ptr::null(),
                c(name).as_ptr(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        assert_eq!(meta("t"), SQLITE_OK);
        assert_eq!(meta("nosuch"), SQLITE_ERROR);
        assert_eq!(meta("v"), SQLITE_ERROR);
        sqlite3_close(db);
    }
}

/// (change counter, version-valid-for, software version, header bytes
/// 18..20 and 48..52 as hex) from page 1.
unsafe fn header_fields(db: *mut sqlite3) -> (u32, u32, i64, String) {
    let row = query1(
        db,
        "SELECT hex(substr(data, 25, 4)), hex(substr(data, 93, 4)),
                hex(substr(data, 97, 4)), hex(substr(data, 19, 2)) || hex(substr(data, 49, 4))
         FROM sqlite_dbpage WHERE pgno=1",
    );
    let f: Vec<&str> = row.split('|').collect();
    let n = |s: &str| u32::from_str_radix(s, 16).unwrap();
    (n(f[0]), n(f[1]), n(f[2]) as i64, f[3].to_string())
}

#[test]
fn test_cli_header_fields_wal() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_file(&dir);
        let version = super::sqlite3_libversion_number() as i64;
        assert_eq!(query1(db, "PRAGMA journal_mode=wal"), "wal");
        // A commit that writes page 1 bumps the counter; others do not.
        exec_ok(db, "CREATE TABLE t(x)");
        assert_eq!(header_fields(db), (2, 2, version, "020200000000".into()));
        exec_ok(db, "INSERT INTO t VALUES(1)");
        exec_ok(db, "INSERT INTO t VALUES(2)");
        assert_eq!(header_fields(db).0, 2);
        exec_ok(db, "PRAGMA wal_checkpoint(TRUNCATE)");
        assert_eq!(header_fields(db).0, 2);
        // While page 1 stays cached the counter restarts from the value the
        // connection loaded (SQLite's dbFileVers), so it stays at 2.
        exec_ok(db, "CREATE TABLE u(x)");
        assert_eq!(header_fields(db), (2, 2, version, "020200000000".into()));
        exec_ok(db, "PRAGMA user_version=7; PRAGMA application_id=99");
        assert_eq!(header_fields(db).0, 2);
        sqlite3_close(db);
        // A new connection loads 2 and writes 3.
        let db = open_file(&dir);
        assert_eq!(header_fields(db).0, 2);
        exec_ok(db, "CREATE TABLE v(x)");
        assert_eq!(header_fields(db), (3, 3, version, "020200000000".into()));
        // Freelist trunk/count, schema cookie, format, cache size, autovacuum,
        // encoding, user_version, incremental vacuum, application id.
        assert_eq!(
            query1(
                db,
                "SELECT hex(substr(data, 33, 40)) FROM sqlite_dbpage WHERE pgno=1"
            ),
            "00000000000000000000000300000004000000000000000000000001000000070000000000000063"
        );
        sqlite3_close(db);
    }
}

#[test]
fn test_cli_data_version() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let a = open_file(&dir);
        exec_ok(a, "PRAGMA journal_mode=wal; CREATE TABLE t(x)");
        let b = open_file(&dir);
        let dv = |db| query1(db, "PRAGMA data_version").parse::<i64>().unwrap();
        let (a0, b0) = (dv(a), dv(b));
        assert!(a0 > 0 && b0 > 0);
        assert_eq!(dv(a), a0);
        // Own commits leave it alone; another connection's change it.
        exec_ok(a, "INSERT INTO t VALUES(1)");
        assert_eq!(dv(a), a0);
        let b1 = dv(b);
        assert_ne!(b1, b0);
        assert_eq!(dv(b), b1);
        exec_ok(b, "INSERT INTO t VALUES(2)");
        assert_eq!(dv(b), b1);
        assert_ne!(dv(a), a0);
        sqlite3_close(b);
        sqlite3_close(a);
    }
}

unsafe fn columns_of(db: *mut sqlite3, table: &str) -> String {
    query(
        db,
        &format!("SELECT name FROM pragma_table_info('{table}')"),
    )
    .join(",")
}

#[test]
fn test_cli_writable_schema() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_file(&dir);
        exec_ok(db, "CREATE TABLE t(a, b); INSERT INTO t VALUES(1, 2)");
        assert_eq!(query1(db, "PRAGMA writable_schema"), "0");
        let msg = query_err(db, "UPDATE sqlite_schema SET sql=sql WHERE name='t'");
        assert!(msg.ends_with("may not be modified"), "{msg}");
        let msg = query_err(db, "CREATE TABLE sqlite_x(a)");
        assert!(
            msg.to_lowercase()
                .ends_with("object name reserved for internal use: sqlite_x"),
            "{msg}"
        );
        // ON: sqlite_ names and sqlite_schema edits are allowed.
        exec_ok(db, "PRAGMA writable_schema=ON");
        assert_eq!(query1(db, "PRAGMA writable_schema"), "1");
        exec_ok(
            db,
            "CREATE TABLE IF NOT EXISTS temp.sqlite_parameters(key TEXT PRIMARY KEY, value)",
        );
        exec_ok(db, "INSERT INTO temp.sqlite_parameters VALUES(':x', 2)");
        exec_ok(
            db,
            "UPDATE sqlite_schema SET sql='CREATE TABLE t(a, b, c)' WHERE name='t'",
        );
        assert_eq!(
            query1(db, "SELECT sql FROM sqlite_schema WHERE name='t'"),
            "CREATE TABLE t(a, b, c)"
        );
        // The loaded schema keeps the old definition until RESET.
        assert_eq!(columns_of(db, "t"), "a,b");
        exec_ok(db, "PRAGMA writable_schema=RESET");
        assert_eq!(query1(db, "PRAGMA writable_schema"), "0");
        assert_eq!(columns_of(db, "t"), "a,b,c");
        assert_eq!(query1(db, "SELECT a, b, c FROM t"), "1|2|NULL");
        let msg = query_err(db, "DELETE FROM sqlite_schema WHERE name='t'");
        assert!(msg.ends_with("may not be modified"), "{msg}");
        // OFF after ON.
        exec_ok(db, "PRAGMA writable_schema=1; PRAGMA writable_schema=OFF");
        assert_eq!(query1(db, "PRAGMA writable_schema"), "0");
        // SQLITE_DBCONFIG_WRITABLE_SCHEMA is the same switch.
        let mut out: c_int = -1;
        assert_eq!(
            sqlite3_db_config(db, 1011, 1 as c_int, &mut out as *mut c_int),
            SQLITE_OK
        );
        assert_eq!(out, 1);
        assert_eq!(query1(db, "PRAGMA writable_schema"), "1");
        assert_eq!(
            sqlite3_db_config(db, 1011, 0 as c_int, &mut out as *mut c_int),
            SQLITE_OK
        );
        assert_eq!(
            (out, query1(db, "PRAGMA writable_schema").as_str()),
            (0, "0")
        );
        sqlite3_close(db);
        // A new connection reads the edited schema.
        let db = open_file(&dir);
        assert_eq!(columns_of(db, "t"), "a,b,c");
        sqlite3_close(db);
    }
}

// A module whose tables hold a prepared statement on their own connection
// until xDisconnect, like the shell's sqlite_dbdata.
#[repr(C)]
struct HolderVtab {
    p_module: *const c_void,
    n_ref: c_int,
    z_err: *mut c_char,
    stmt: *mut sqlite3_stmt,
}

#[repr(C)]
struct HolderCursor {
    p_vtab: *mut c_void,
}

#[repr(C)]
struct Module {
    version: c_int,
    methods: [*const c_void; 24],
}
unsafe impl Sync for Module {}
unsafe impl Send for Module {}

static HOLDER_DISCONNECTS: AtomicI32 = AtomicI32::new(0);

unsafe extern "C" fn holder_connect(
    db: *mut sqlite3,
    _aux: *mut c_void,
    _argc: c_int,
    _argv: *const *const c_char,
    out: *mut *mut c_void,
    _err: *mut *mut c_char,
) -> c_int {
    #[allow(clashing_extern_declarations)]
    extern "C" {
        fn sqlite3_declare_vtab(db: *mut sqlite3, sql: *const c_char) -> c_int;
    }
    let rc = sqlite3_declare_vtab(db, c("CREATE TABLE x(a)").as_ptr());
    if rc != SQLITE_OK {
        return rc;
    }
    let mut stmt = ptr::null_mut();
    sqlite3_prepare_v2(db, c("SELECT 1").as_ptr(), -1, &mut stmt, ptr::null_mut());
    let v = Box::new(HolderVtab {
        p_module: ptr::null(),
        n_ref: 0,
        z_err: ptr::null_mut(),
        stmt,
    });
    *out = Box::into_raw(v).cast();
    SQLITE_OK
}
unsafe extern "C" fn holder_disconnect(v: *mut c_void) -> c_int {
    HOLDER_DISCONNECTS.fetch_add(1, Ordering::SeqCst);
    let v = Box::from_raw(v as *mut HolderVtab);
    sqlite3_finalize(v.stmt);
    SQLITE_OK
}
unsafe extern "C" fn holder_best_index(_v: *mut c_void, _info: *mut c_void) -> c_int {
    SQLITE_OK
}
unsafe extern "C" fn holder_open(v: *mut c_void, out: *mut *mut c_void) -> c_int {
    *out = Box::into_raw(Box::new(HolderCursor { p_vtab: v })).cast();
    SQLITE_OK
}
unsafe extern "C" fn holder_close(cur: *mut c_void) -> c_int {
    drop(Box::from_raw(cur as *mut HolderCursor));
    SQLITE_OK
}
unsafe extern "C" fn holder_filter(
    _cur: *mut c_void,
    _n: c_int,
    _s: *const c_char,
    _argc: c_int,
    _argv: *mut *mut c_void,
) -> c_int {
    SQLITE_OK
}
unsafe extern "C" fn holder_next(_cur: *mut c_void) -> c_int {
    SQLITE_OK
}
unsafe extern "C" fn holder_eof(_cur: *mut c_void) -> c_int {
    1
}
unsafe extern "C" fn holder_column(_cur: *mut c_void, ctx: *mut c_void, _i: c_int) -> c_int {
    sqlite3_result_null(ctx);
    SQLITE_OK
}
unsafe extern "C" fn holder_rowid(_cur: *mut c_void, r: *mut i64) -> c_int {
    *r = 0;
    SQLITE_OK
}

fn holder_module() -> &'static Module {
    static M: std::sync::OnceLock<Module> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let mut methods = [ptr::null::<c_void>(); 24];
        methods[0] = holder_connect as *const c_void; // xCreate
        methods[1] = holder_connect as *const c_void; // xConnect
        methods[2] = holder_best_index as *const c_void;
        methods[3] = holder_disconnect as *const c_void; // xDisconnect
        methods[4] = holder_disconnect as *const c_void; // xDestroy
        methods[5] = holder_open as *const c_void;
        methods[6] = holder_close as *const c_void;
        methods[7] = holder_filter as *const c_void;
        methods[8] = holder_next as *const c_void;
        methods[9] = holder_eof as *const c_void;
        methods[10] = holder_column as *const c_void;
        methods[11] = holder_rowid as *const c_void;
        Module {
            version: 0,
            methods,
        }
    })
}

unsafe fn open_with_holder(dir: &tempfile::TempDir) -> *mut sqlite3 {
    #[allow(clashing_extern_declarations)]
    extern "C" {
        fn sqlite3_create_module(
            db: *mut sqlite3,
            name: *const c_char,
            module: *const Module,
            aux: *mut c_void,
        ) -> c_int;
    }
    let db = open_file(dir);
    let rc = sqlite3_create_module(db, c("holder").as_ptr(), holder_module(), ptr::null_mut());
    assert_eq!(rc, SQLITE_OK);
    db
}

#[test]
fn test_cli_close_disconnects_created_vtabs() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_with_holder(&dir);
        exec_ok(db, "CREATE VIRTUAL TABLE h USING holder; CREATE TABLE t(a)");
        assert_eq!(query(db, "SELECT count(*) FROM h"), ["0"]);
        // The table's own statement is finalized by xDisconnect: close works.
        HOLDER_DISCONNECTS.store(0, Ordering::SeqCst);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
        assert_eq!(HOLDER_DISCONNECTS.load(Ordering::SeqCst), 1);

        // With an application statement open, close is BUSY; the table
        // reconnects on its next use.
        let db = open_with_holder(&dir);
        assert_eq!(query(db, "SELECT count(*) FROM h"), ["0"]);
        let s = prepare(db, "SELECT a FROM t");
        assert_eq!(sqlite3_close(db), SQLITE_BUSY);
        assert_eq!(query(db, "SELECT count(*) FROM h"), ["0"]);
        assert_eq!(query(db, "SELECT count(*) FROM t"), ["0"]);
        sqlite3_finalize(s);
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

#[test]
fn test_cli_sqlite_dbpage_other_schema_write() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_file(&dir);
        exec_ok(
            db,
            "CREATE TABLE m(x); CREATE TEMP TABLE tt(y); INSERT INTO tt VALUES(42)",
        );
        assert_eq!(
            query1(db, "SELECT count(*) > 0 FROM sqlite_dbpage('temp')"),
            "1"
        );
        // Rewriting a page of temp opens the write transaction on temp.
        exec_ok(
            db,
            "UPDATE sqlite_dbpage SET data=data WHERE schema='temp' AND pgno=2",
        );
        exec_ok(
            db,
            "INSERT INTO sqlite_dbpage(pgno, data, schema)
             SELECT pgno, data, 'temp' FROM sqlite_dbpage('temp') WHERE pgno=1",
        );
        assert_eq!(query1(db, "SELECT y FROM tt"), "42");
        sqlite3_close(db);
    }
}

#[test]
fn test_cli_bytecode_stmt_pointer() {
    unsafe {
        let db = open();
        exec_ok(db, "CREATE TABLE t(a)");
        let target = prepare(db, "SELECT a FROM t");
        let q = prepare(
            db,
            "SELECT (SELECT opcode FROM bytecode(?1) WHERE addr=0), (SELECT name FROM tables_used(?1))",
        );
        let tag = c("stmt-pointer");
        assert_eq!(
            sqlite3_bind_pointer(q, 1, target.cast(), tag.as_ptr(), None),
            SQLITE_OK
        );
        assert_eq!(sqlite3_step(q), SQLITE_ROW);
        assert_eq!(format!("{}|{}", text(q, 0), text(q, 1)), "Init|t");
        sqlite3_finalize(q);
        // Another pointer type is not a statement.
        let q = prepare(db, "SELECT * FROM bytecode(?1)");
        let other = c("other");
        sqlite3_bind_pointer(q, 1, target.cast(), other.as_ptr(), None);
        assert_eq!(sqlite3_step(q), SQLITE_ERROR);
        assert_eq!(
            errmsg(db),
            "argument to bytecode() is not a valid SQL statement"
        );
        sqlite3_finalize(q);
        sqlite3_finalize(target);
        sqlite3_close(db);
    }
}

#[test]
fn test_cli_filename_database_block() {
    unsafe {
        // SQLite's filename block: 4 zero bytes, name, URI key/values, an
        // empty key, the journal name, the WAL name.
        let mut block = vec![0u8; 4];
        let start = block.len();
        block.extend_from_slice(b"/x/a.db\0mode\0rw\0\0");
        let journal = block.len();
        block.extend_from_slice(b"/x/a.db-journal\0");
        let wal = block.len();
        block.extend_from_slice(b"/x/a.db-wal\0\0\0\0\0");
        let base = block.as_ptr() as *const c_char;
        for off in [start, journal, wal] {
            let p = sqlite3_filename_database(base.add(off));
            assert_eq!(p, base.add(start));
            assert_eq!(CStr::from_ptr(p).to_str().unwrap(), "/x/a.db");
        }
    }
}
