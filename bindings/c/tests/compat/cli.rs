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
        let mut rows = query(
            db,
            "SELECT sql, ncol, ro, sql LIKE '%sqlite_stmt' OR busy=0 FROM sqlite_stmt",
        );
        rows.sort();
        assert_eq!(
            rows,
            [
                "INSERT INTO t VALUES(1)|0|0|1",
                "SELECT a, a FROM t|2|1|1",
                "SELECT sql, ncol, ro, sql LIKE '%sqlite_stmt' OR busy=0 FROM sqlite_stmt|4|1|1",
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
