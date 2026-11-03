//! Config, file control, status, VFS registry, auto-extension and
//! loadable-extension tests. Each must pass on turso and on real SQLite.
#![allow(non_camel_case_types, non_upper_case_globals)]
use libc::{c_char, c_int, c_void};
use std::ffi::{CStr, CString};
use std::ptr;
use std::sync::Mutex;

use super::sqlite3_stmt;

#[repr(C)]
struct sqlite3 {
    _private: [u8; 0],
}

/// First fields of `struct sqlite3_vfs`; only used through pointers.
#[repr(C)]
struct sqlite3_vfs {
    i_version: c_int,
    sz_os_file: c_int,
    mx_pathname: c_int,
    p_next: *mut sqlite3_vfs,
    z_name: *const c_char,
    p_app_data: *mut c_void,
    rest: [*const c_void; 16],
}

type Entry = unsafe extern "C" fn(*mut sqlite3, *mut *mut c_char, *const c_void) -> c_int;

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
    static mut sqlite3_temp_directory: *mut c_char;
    fn sqlite3_config(op: c_int, ...) -> c_int;
    fn sqlite3_test_control(op: c_int, ...) -> c_int;
    fn sqlite3_open(filename: *const c_char, db: *mut *mut sqlite3) -> c_int;
    fn sqlite3_open_v2(
        filename: *const c_char,
        db: *mut *mut sqlite3,
        flags: c_int,
        vfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close(db: *mut sqlite3) -> c_int;
    fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char;
    fn sqlite3_free(p: *mut c_void);
    fn sqlite3_mprintf(fmt: *const c_char, ...) -> *mut c_char;
    fn sqlite3_exec(
        db: *mut sqlite3,
        sql: *const c_char,
        cb: Option<
            unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
        >,
        arg: *mut c_void,
        err: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_db_config(db: *mut sqlite3, op: c_int, ...) -> c_int;
    fn sqlite3_file_control(
        db: *mut sqlite3,
        name: *const c_char,
        op: c_int,
        arg: *mut c_void,
    ) -> c_int;
    fn sqlite3_status(op: c_int, cur: *mut c_int, hi: *mut c_int, reset: c_int) -> c_int;
    fn sqlite3_status64(op: c_int, cur: *mut i64, hi: *mut i64, reset: c_int) -> c_int;
    fn sqlite3_db_status(
        db: *mut sqlite3,
        op: c_int,
        cur: *mut c_int,
        hi: *mut c_int,
        reset: c_int,
    ) -> c_int;
    fn sqlite3_prepare_v2(
        db: *mut sqlite3,
        sql: *const c_char,
        n: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_stmt_readonly(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_finalize(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_db_mutex(db: *mut sqlite3) -> *mut c_void;
    fn sqlite3_unlock_notify(
        db: *mut sqlite3,
        cb: Option<unsafe extern "C" fn(*mut *mut c_void, c_int)>,
        arg: *mut c_void,
    ) -> c_int;
    fn sqlite3_auto_extension(f: Option<unsafe extern "C" fn()>) -> c_int;
    fn sqlite3_cancel_auto_extension(f: Option<unsafe extern "C" fn()>) -> c_int;
    fn sqlite3_reset_auto_extension();
    fn sqlite3_enable_load_extension(db: *mut sqlite3, onoff: c_int) -> c_int;
    fn sqlite3_load_extension(
        db: *mut sqlite3,
        file: *const c_char,
        proc_: *const c_char,
        err: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_vfs_find(name: *const c_char) -> *mut sqlite3_vfs;
    fn sqlite3_vfs_register(vfs: *mut sqlite3_vfs, make_default: c_int) -> c_int;
    fn sqlite3_vfs_unregister(vfs: *mut sqlite3_vfs) -> c_int;
}

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_MISUSE: c_int = 21;
const SQLITE_NOTFOUND: c_int = 12;

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

unsafe fn open(path: &str) -> *mut sqlite3 {
    let mut db = ptr::null_mut();
    assert_eq!(sqlite3_open(cs(path).as_ptr(), &mut db), SQLITE_OK);
    db
}

unsafe fn open_mem() -> *mut sqlite3 {
    open(":memory:")
}

unsafe fn exec(db: *mut sqlite3, sql: &str) -> c_int {
    let rc = sqlite3_exec(db, cs(sql).as_ptr(), None, ptr::null_mut(), ptr::null_mut());
    if rc != SQLITE_OK {
        eprintln!("exec({sql}) -> {rc}: {}", errmsg(db));
    }
    rc
}

/// First column of the first row as text, via sqlite3_exec.
unsafe fn query_text(db: *mut sqlite3, sql: &str) -> Option<String> {
    unsafe extern "C" fn cb(
        arg: *mut c_void,
        n: c_int,
        vals: *mut *mut c_char,
        _names: *mut *mut c_char,
    ) -> c_int {
        let out = &mut *(arg as *mut Option<String>);
        if n > 0 && out.is_none() && !(*vals).is_null() {
            *out = Some(CStr::from_ptr(*vals).to_string_lossy().into_owned());
        }
        0
    }
    let mut out: Option<String> = None;
    let rc = sqlite3_exec(
        db,
        cs(sql).as_ptr(),
        Some(cb),
        &mut out as *mut _ as *mut c_void,
        ptr::null_mut(),
    );
    assert_eq!(rc, SQLITE_OK, "{sql}: {}", errmsg(db));
    out
}

unsafe fn errmsg(db: *mut sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db))
        .to_string_lossy()
        .into_owned()
}

/// Takes and frees a sqlite3_malloc'd C string.
unsafe fn take(p: *mut c_char) -> String {
    assert!(!p.is_null());
    let s = CStr::from_ptr(p).to_string_lossy().into_owned();
    sqlite3_free(p.cast());
    s
}

/// Serializes tests that touch process-global state.
static GLOBALS: Mutex<()> = Mutex::new(());

pub(super) fn lock() -> std::sync::MutexGuard<'static, ()> {
    GLOBALS.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn config_before_and_after_init() {
    let _g = lock();
    unsafe {
        // Either before init (accepted) or after it (misuse), never a crash.
        let rc = sqlite3_config(2 /* MULTITHREAD */);
        assert!(rc == SQLITE_OK || rc == SQLITE_MISUSE, "rc={rc}");
        let db = open_mem();
        assert_eq!(sqlite3_config(2), SQLITE_MISUSE);
        assert_eq!(sqlite3_config(9 /* MEMSTATUS */, 0 as c_int), SQLITE_MISUSE);
        sqlite3_close(db);
    }
}

#[test]
fn config_uri_child() {
    // Runs in a child process: SQLITE_CONFIG_URI must precede initialization.
    if std::env::var_os("TURSO_CFG_URI_CHILD").is_none() {
        return;
    }
    unsafe {
        assert_eq!(sqlite3_config(17 /* URI */, 1 as c_int), SQLITE_OK);
        let dir = std::env::var("TURSO_CFG_URI_DIR").unwrap();
        let uri = format!("file:{dir}/uri%20db.sqlite?mode=rwc");
        let mut db = ptr::null_mut();
        assert_eq!(sqlite3_open(cs(&uri).as_ptr(), &mut db), SQLITE_OK);
        assert_eq!(exec(db, "CREATE TABLE t(x)"), SQLITE_OK);
        sqlite3_close(db);
    }
}

#[test]
fn config_uri_applies_to_sqlite3_open() {
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "config::config_uri_child", "--test-threads=1"])
        .env("TURSO_CFG_URI_CHILD", "1")
        .env("TURSO_CFG_URI_DIR", dir.path())
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );
    assert!(dir.path().join("uri db.sqlite").exists());
    assert!(!dir.path().join("file:").exists());
}

#[test]
fn config_unknown_op_is_rejected() {
    let _g = lock();
    unsafe {
        let rc = sqlite3_config(99999);
        assert!(rc == SQLITE_ERROR || rc == SQLITE_MISUSE, "rc={rc}");
    }
}

#[test]
fn test_control_unknown_ops_return_zero() {
    let _g = lock();
    unsafe {
        let db = open_mem();
        // SQLITE_TESTCTRL_OPTIMIZATIONS with no optimizations disabled.
        assert_eq!(sqlite3_test_control(15, db, 0 as c_int), 0);
        sqlite3_close(db);
    }
}

#[test]
fn file_control_basics() {
    let _g = lock();
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path().join("fc.db").to_str().unwrap());
        assert_eq!(exec(db, "CREATE TABLE t(x)"), SQLITE_OK);

        let mut name: *mut c_char = ptr::null_mut();
        assert_eq!(
            sqlite3_file_control(
                db,
                ptr::null(),
                12, /* VFSNAME */
                &mut name as *mut _ as *mut c_void
            ),
            SQLITE_OK
        );
        assert_eq!(take(name), "unix");

        let mut tmp: *mut c_char = ptr::null_mut();
        assert_eq!(
            sqlite3_file_control(
                db,
                cs("main").as_ptr(),
                16, /* TEMPFILENAME */
                &mut tmp as *mut _ as *mut c_void
            ),
            SQLITE_OK
        );
        assert!(!take(tmp).is_empty());

        let mut errno: c_int = -1;
        assert_eq!(
            sqlite3_file_control(
                db,
                ptr::null(),
                4, /* LAST_ERRNO */
                &mut errno as *mut _ as *mut c_void
            ),
            SQLITE_OK
        );
        assert_eq!(errno, 0);

        // Hints are accepted.
        let mut hint: i64 = 1 << 20;
        assert_eq!(
            sqlite3_file_control(
                db,
                ptr::null(),
                5, /* SIZE_HINT */
                &mut hint as *mut _ as *mut c_void
            ),
            SQLITE_OK
        );

        // Unknown schema and unknown opcode.
        let mut arg: c_int = 0;
        assert_eq!(
            sqlite3_file_control(
                db,
                cs("nosuch").as_ptr(),
                10,
                &mut arg as *mut _ as *mut c_void
            ),
            SQLITE_ERROR
        );
        assert_eq!(
            sqlite3_file_control(db, ptr::null(), 99999, &mut arg as *mut _ as *mut c_void),
            SQLITE_NOTFOUND
        );
        sqlite3_close(db);
    }
}

#[test]
fn file_control_persist_wal_and_powersafe_overwrite_roundtrip() {
    let _g = lock();
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path().join("pw.db").to_str().unwrap());
        for (op, default) in [(10 /* PERSIST_WAL */, 0), (13 /* POWERSAFE_OVERWRITE */, 1)] {
            let mut v: c_int = -1;
            assert_eq!(
                sqlite3_file_control(db, ptr::null(), op, &mut v as *mut _ as *mut c_void),
                SQLITE_OK
            );
            assert_eq!(v, default, "op {op} default");
            v = 1 - default;
            assert_eq!(
                sqlite3_file_control(db, ptr::null(), op, &mut v as *mut _ as *mut c_void),
                SQLITE_OK
            );
            assert_eq!(v, 1 - default);
            v = -1;
            assert_eq!(
                sqlite3_file_control(db, ptr::null(), op, &mut v as *mut _ as *mut c_void),
                SQLITE_OK
            );
            assert_eq!(v, 1 - default, "op {op} set value is remembered");
        }
        sqlite3_close(db);
    }
}

#[test]
fn file_control_on_memory_database_is_notfound() {
    let _g = lock();
    unsafe {
        let db = open_mem();
        let mut arg: c_int = 0;
        assert_eq!(
            sqlite3_file_control(db, ptr::null(), 10, &mut arg as *mut _ as *mut c_void),
            SQLITE_NOTFOUND
        );
        sqlite3_close(db);
    }
}

#[test]
fn temp_directory_is_used_for_temp_file_names() {
    let _g = lock();
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let dir_c = CString::new(dir.path().to_str().unwrap()).unwrap();
        let db = open(dir.path().join("td.db").to_str().unwrap());
        sqlite3_temp_directory = dir_c.as_ptr() as *mut c_char;
        let mut tmp: *mut c_char = ptr::null_mut();
        let rc = sqlite3_file_control(db, ptr::null(), 16, &mut tmp as *mut _ as *mut c_void);
        sqlite3_temp_directory = ptr::null_mut();
        assert_eq!(rc, SQLITE_OK);
        assert!(take(tmp).starts_with(dir.path().to_str().unwrap()));
        sqlite3_close(db);
    }
}

#[test]
fn status_and_db_status() {
    let _g = lock();
    unsafe {
        let (mut cur, mut hi) = (-1i64, -1i64);
        assert_eq!(
            sqlite3_status64(0 /* MEMORY_USED */, &mut cur, &mut hi, 0),
            SQLITE_OK
        );
        assert!(cur >= 0 && hi >= 0);
        let (mut c32, mut h32) = (-1, -1);
        assert_eq!(sqlite3_status(0, &mut c32, &mut h32, 0), SQLITE_OK);
        assert!(c32 >= 0 && h32 >= 0);
        assert_eq!(sqlite3_status(9999, &mut c32, &mut h32, 0), SQLITE_MISUSE);
        assert_eq!(sqlite3_status64(9999, &mut cur, &mut hi, 0), SQLITE_MISUSE);

        let db = open_mem();
        assert_eq!(
            sqlite3_db_status(db, 0 /* LOOKASIDE_USED */, &mut c32, &mut h32, 0),
            SQLITE_OK
        );
        assert!(c32 >= 0 && h32 >= 0);
        assert_eq!(
            sqlite3_db_status(db, 9999, &mut c32, &mut h32, 0),
            SQLITE_ERROR
        );
        sqlite3_close(db);
    }
}

#[test]
fn db_mutex_does_not_crash() {
    let _g = lock();
    unsafe {
        let db = open_mem();
        // NULL when the library is not threadsafe, else a mutex.
        let _ = sqlite3_db_mutex(db);
        sqlite3_close(db);
    }
}

#[test]
fn unlock_notify_fires_immediately_when_unblocked() {
    let _g = lock();
    unsafe extern "C" fn notify(args: *mut *mut c_void, n: c_int) {
        assert_eq!(n, 1);
        let counter = &mut *(*args as *mut c_int);
        *counter += 1;
    }
    unsafe {
        let db = open_mem();
        let mut counter: c_int = 0;
        assert_eq!(
            sqlite3_unlock_notify(db, Some(notify), &mut counter as *mut _ as *mut c_void),
            SQLITE_OK
        );
        assert_eq!(counter, 1);
        sqlite3_close(db);
    }
}

#[test]
fn db_config_boolean_ops() {
    let _g = lock();
    unsafe {
        let db = open_mem();
        let mut out: c_int = -1;
        // FKEY: query, set, read back through the pragma.
        assert_eq!(
            sqlite3_db_config(db, 1002, -1 as c_int, &mut out as *mut c_int),
            SQLITE_OK
        );
        assert_eq!(out, 0);
        assert_eq!(
            sqlite3_db_config(db, 1002, 1 as c_int, &mut out as *mut c_int),
            SQLITE_OK
        );
        assert_eq!(out, 1);
        assert_eq!(query_text(db, "PRAGMA foreign_keys").as_deref(), Some("1"));
        assert_eq!(
            sqlite3_db_config(db, 1002, 0 as c_int, &mut out as *mut c_int),
            SQLITE_OK
        );
        assert_eq!(out, 0);
        assert_eq!(query_text(db, "PRAGMA foreign_keys").as_deref(), Some("0"));
        // Triggers and views are enabled by default.
        for op in [1003, 1015] {
            assert_eq!(
                sqlite3_db_config(db, op, -1 as c_int, &mut out as *mut c_int),
                SQLITE_OK
            );
            assert_eq!(out, 1, "op {op}");
        }
        // The output pointer is optional.
        assert_eq!(
            sqlite3_db_config(db, 1002, 0 as c_int, ptr::null_mut::<c_int>()),
            SQLITE_OK
        );
        sqlite3_close(db);
    }
}

#[test]
fn vfs_find_register_unregister() {
    let _g = lock();
    unsafe {
        let default = sqlite3_vfs_find(ptr::null());
        assert!(!default.is_null());
        assert_eq!(CStr::from_ptr((*default).z_name).to_str().unwrap(), "unix");
        assert!((*default).i_version >= 1);
        assert!(sqlite3_vfs_find(cs("no-such-vfs").as_ptr()).is_null());

        // A pass-through VFS: a renamed copy of the default one.
        let name = cs("compat-test-vfs");
        let mut copy = Box::new(ptr::read(default));
        copy.z_name = name.as_ptr();
        copy.p_next = ptr::null_mut();
        let p: *mut sqlite3_vfs = &mut *copy;
        assert_eq!(sqlite3_vfs_register(p, 0), SQLITE_OK);
        assert_eq!(sqlite3_vfs_find(name.as_ptr()), p);
        assert_eq!(sqlite3_vfs_find(ptr::null()), default, "not made default");

        // A read-only connection can be opened through it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vfs.db");
        let db = open(path.to_str().unwrap());
        assert_eq!(
            exec(db, "CREATE TABLE t(x); INSERT INTO t VALUES (7)"),
            SQLITE_OK
        );
        sqlite3_close(db);
        let mut ro = ptr::null_mut();
        assert_eq!(
            sqlite3_open_v2(
                cs(path.to_str().unwrap()).as_ptr(),
                &mut ro,
                1, /* READONLY */
                name.as_ptr()
            ),
            SQLITE_OK
        );
        assert_eq!(query_text(ro, "SELECT x FROM t").as_deref(), Some("7"));
        sqlite3_close(ro);

        assert_eq!(sqlite3_vfs_unregister(p), SQLITE_OK);
        assert!(sqlite3_vfs_find(name.as_ptr()).is_null());

        // Opening through an unknown VFS fails.
        let mut db = ptr::null_mut();
        let rc = sqlite3_open_v2(
            cs(path.to_str().unwrap()).as_ptr(),
            &mut db,
            2,
            name.as_ptr(),
        );
        assert_eq!(rc, SQLITE_ERROR);
        if !db.is_null() {
            sqlite3_close(db);
        }
    }
}

#[test]
fn vfs_registered_as_default_is_found_and_restored() {
    let _g = lock();
    unsafe {
        let default = sqlite3_vfs_find(ptr::null());
        let name = cs("compat-default-vfs");
        let mut copy = Box::new(ptr::read(default));
        copy.z_name = name.as_ptr();
        copy.p_next = ptr::null_mut();
        let p: *mut sqlite3_vfs = &mut *copy;
        assert_eq!(sqlite3_vfs_register(p, 1), SQLITE_OK);
        assert_eq!(sqlite3_vfs_find(ptr::null()), p);
        assert_eq!(sqlite3_vfs_unregister(p), SQLITE_OK);
        assert_eq!(sqlite3_vfs_find(ptr::null()), default);
    }
}

// ---- auto-extensions and loadable extensions ----

static CALLS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

unsafe extern "C" fn counting_init(
    db: *mut sqlite3,
    _err: *mut *mut c_char,
    api: *const c_void,
) -> c_int {
    assert!(!api.is_null());
    CALLS.lock().unwrap().push(db as usize);
    SQLITE_OK
}

unsafe extern "C" fn failing_init(
    _db: *mut sqlite3,
    err: *mut *mut c_char,
    _api: *const c_void,
) -> c_int {
    *err = sqlite3_mprintf(c"boom".as_ptr());
    SQLITE_ERROR
}

fn as_fn(f: Entry) -> Option<unsafe extern "C" fn()> {
    // SAFETY: SQLite casts entry points to void(*)(void) the same way.
    Some(unsafe { std::mem::transmute::<Entry, unsafe extern "C" fn()>(f) })
}

#[test]
fn auto_extension_runs_on_every_open_and_can_be_cancelled() {
    let _g = lock();
    unsafe {
        sqlite3_reset_auto_extension();
        CALLS.lock().unwrap().clear();
        assert_eq!(sqlite3_auto_extension(as_fn(counting_init)), SQLITE_OK);
        // Registering twice is a no-op.
        assert_eq!(sqlite3_auto_extension(as_fn(counting_init)), SQLITE_OK);
        let a = open_mem();
        let b = open_mem();
        assert_eq!(*CALLS.lock().unwrap(), vec![a as usize, b as usize]);
        assert_eq!(sqlite3_cancel_auto_extension(as_fn(counting_init)), 1);
        assert_eq!(sqlite3_cancel_auto_extension(as_fn(counting_init)), 0);
        let c = open_mem();
        assert_eq!(CALLS.lock().unwrap().len(), 2);
        for db in [a, b, c] {
            sqlite3_close(db);
        }

        assert_eq!(sqlite3_auto_extension(as_fn(counting_init)), SQLITE_OK);
        sqlite3_reset_auto_extension();
        let d = open_mem();
        assert_eq!(CALLS.lock().unwrap().len(), 2, "reset removes all entries");
        sqlite3_close(d);
    }
}

#[test]
fn auto_extension_failure_fails_the_open() {
    let _g = lock();
    unsafe {
        sqlite3_reset_auto_extension();
        assert_eq!(sqlite3_auto_extension(as_fn(failing_init)), SQLITE_OK);
        let mut db = ptr::null_mut();
        let rc = sqlite3_open(cs(":memory:").as_ptr(), &mut db);
        sqlite3_reset_auto_extension();
        assert_eq!(rc, SQLITE_ERROR);
        assert!(
            !db.is_null(),
            "a handle is returned so the error can be read"
        );
        assert_eq!(errmsg(db), "automatic extension loading failed: boom");
        assert_eq!(sqlite3_close(db), SQLITE_OK);
    }
}

/// Compiles the C test extension once; returns the paths of the two builds
/// (with and without the default `sqlite3_extension_init` entry point).
pub(super) fn build_extension() -> &'static (std::path::PathBuf, std::path::PathBuf) {
    use std::sync::OnceLock;
    static BUILT: OnceLock<(std::path::PathBuf, std::path::PathBuf)> = OnceLock::new();
    BUILT.get_or_init(|| {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let src = root.join("tests/compat/ext/hello_ext.c");
        let include = root.join("../../testing/sqlite_test_ext/include");
        let dir = std::env::temp_dir().join(format!("turso-compat-ext-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
        let build = |out: &std::path::Path, extra: &[&str]| {
            let status = std::process::Command::new(&cc)
                .args(["-shared", "-fPIC", "-I"])
                .arg(&include)
                .args(extra)
                .arg("-o")
                .arg(out)
                .arg(&src)
                .status()
                .expect("run C compiler");
            assert!(status.success(), "compiling the test extension failed");
        };
        let with_default = dir.join("hello.so");
        let derived = dir.join("libhello.so");
        build(&with_default, &[]);
        build(&derived, &["-DNO_DEFAULT_ENTRY"]);
        (with_default, derived)
    })
}

#[cfg(unix)]
#[test]
fn auto_extension_with_real_c_extension() {
    let _g = lock();
    unsafe {
        let (lib, _) = build_extension();
        let handle = libc::dlopen(cs(lib.to_str().unwrap()).as_ptr(), libc::RTLD_NOW);
        assert!(!handle.is_null());
        let sym = libc::dlsym(handle, c"sqlite3_hello_init".as_ptr());
        assert!(!sym.is_null());
        let entry: unsafe extern "C" fn() = std::mem::transmute(sym);
        sqlite3_reset_auto_extension();
        assert_eq!(sqlite3_auto_extension(Some(entry)), SQLITE_OK);
        let db = open_mem();
        sqlite3_reset_auto_extension();
        assert_eq!(
            query_text(db, "SELECT hello_answer()").as_deref(),
            Some("42")
        );
        assert_eq!(
            query_text(db, "SELECT hello_echo('hi')").as_deref(),
            Some("hi")
        );
        sqlite3_close(db);
        // Without the auto-extension a new connection has no such function.
        let db = open_mem();
        assert_ne!(exec(db, "SELECT hello_answer()"), SQLITE_OK);
        sqlite3_close(db);
    }
}

#[cfg(unix)]
#[test]
fn load_extension_requires_enabling() {
    let _g = lock();
    unsafe {
        let (lib, _) = build_extension();
        let db = open_mem();
        let mut err: *mut c_char = ptr::null_mut();
        let rc = sqlite3_load_extension(
            db,
            cs(lib.to_str().unwrap()).as_ptr(),
            ptr::null(),
            &mut err,
        );
        assert_eq!(rc, SQLITE_ERROR);
        assert_eq!(take(err), "not authorized");

        // db_config(ENABLE_LOAD_EXTENSION) enables it too.
        let mut out: c_int = -1;
        assert_eq!(
            sqlite3_db_config(db, 1005, 1 as c_int, &mut out as *mut c_int),
            SQLITE_OK
        );
        assert_eq!(out, 1);
        let rc = sqlite3_load_extension(
            db,
            cs(lib.to_str().unwrap()).as_ptr(),
            ptr::null(),
            ptr::null_mut(),
        );
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(
            query_text(db, "SELECT hello_answer()").as_deref(),
            Some("42")
        );
        assert_eq!(
            sqlite3_db_config(db, 1005, 0 as c_int, &mut out as *mut c_int),
            SQLITE_OK
        );
        assert_eq!(out, 0);
        let rc = sqlite3_load_extension(
            db,
            cs(lib.to_str().unwrap()).as_ptr(),
            ptr::null(),
            &mut err,
        );
        assert_eq!(rc, SQLITE_ERROR);
        take(err);
        sqlite3_close(db);
    }
}

#[cfg(unix)]
#[test]
fn load_extension_entry_points_and_errors() {
    let _g = lock();
    unsafe {
        let (lib, derived) = build_extension();
        let lib_c = cs(lib.to_str().unwrap());
        let derived_c = cs(derived.to_str().unwrap());
        let db = open_mem();
        assert_eq!(sqlite3_enable_load_extension(db, 1), SQLITE_OK);

        // Default entry point.
        assert_eq!(
            sqlite3_load_extension(db, lib_c.as_ptr(), ptr::null(), ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(
            query_text(db, "SELECT hello_answer()").as_deref(),
            Some("42")
        );

        // Entry point derived from "libhello.so" (no sqlite3_extension_init).
        let db2 = open_mem();
        assert_eq!(sqlite3_enable_load_extension(db2, 1), SQLITE_OK);
        assert_eq!(
            sqlite3_load_extension(db2, derived_c.as_ptr(), ptr::null(), ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(
            query_text(db2, "SELECT hello_echo('x')").as_deref(),
            Some("x")
        );
        sqlite3_close(db2);

        // Explicit entry point.
        let db3 = open_mem();
        assert_eq!(sqlite3_enable_load_extension(db3, 1), SQLITE_OK);
        assert_eq!(
            sqlite3_load_extension(
                db3,
                lib_c.as_ptr(),
                c"sqlite3_hello_init".as_ptr(),
                ptr::null_mut()
            ),
            SQLITE_OK
        );
        assert_eq!(
            query_text(db3, "SELECT hello_answer()").as_deref(),
            Some("42")
        );
        sqlite3_close(db3);

        // Missing entry point.
        let mut err: *mut c_char = ptr::null_mut();
        let rc = sqlite3_load_extension(db, lib_c.as_ptr(), c"nope".as_ptr(), &mut err);
        assert_eq!(rc, SQLITE_ERROR);
        let msg = take(err);
        assert!(msg.contains("nope"), "{msg}");

        // Entry point that fails.
        let rc = sqlite3_load_extension(
            db,
            lib_c.as_ptr(),
            c"sqlite3_failing_init".as_ptr(),
            &mut err,
        );
        assert_eq!(rc, SQLITE_ERROR);
        assert_eq!(take(err), "error during initialization: deliberate failure");

        // Missing file.
        let rc = sqlite3_load_extension(db, c"/nonexistent/lib.so".as_ptr(), ptr::null(), &mut err);
        assert_eq!(rc, SQLITE_ERROR);
        assert!(take(err).contains("/nonexistent/lib.so"));
        sqlite3_close(db);
    }
}

// ---- engine fixes that the C consumers rely on ----

#[test]
fn create_table_if_not_exists_works_on_readonly_database() {
    let _g = lock();
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro.db");
        let db = open(path.to_str().unwrap());
        assert_eq!(exec(db, "CREATE TABLE t(x)"), SQLITE_OK);
        sqlite3_close(db);
        let mut ro = ptr::null_mut();
        assert_eq!(
            sqlite3_open_v2(cs(path.to_str().unwrap()).as_ptr(), &mut ro, 1, ptr::null()),
            SQLITE_OK
        );
        assert_eq!(exec(ro, "CREATE TABLE IF NOT EXISTS t(x)"), SQLITE_OK);
        assert_ne!(exec(ro, "CREATE TABLE IF NOT EXISTS u(x)"), SQLITE_OK);
        // SQLite still reports the no-op statement as not read-only.
        let mut stmt = ptr::null_mut();
        let sql = cs("CREATE TABLE IF NOT EXISTS t(x)");
        assert_eq!(
            sqlite3_prepare_v2(ro, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()),
            SQLITE_OK
        );
        assert_eq!(sqlite3_stmt_readonly(stmt), 0);
        sqlite3_finalize(stmt);
        sqlite3_close(ro);
    }
}

#[test]
fn pragma_encoding_utf8_is_accepted() {
    let _g = lock();
    unsafe {
        let db = open_mem();
        assert_eq!(exec(db, "PRAGMA encoding = 'UTF-8'"), SQLITE_OK);
        assert_eq!(exec(db, "PRAGMA encoding = \"UTF-8\""), SQLITE_OK);
        assert_eq!(exec(db, "PRAGMA encoding = utf8"), SQLITE_OK);
        assert_eq!(query_text(db, "PRAGMA encoding").as_deref(), Some("UTF-8"));
        sqlite3_close(db);
    }
}
