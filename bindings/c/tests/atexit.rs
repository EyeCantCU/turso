//! sqlite3_close() from an atexit handler must not abort (TLS is already
//! torn down by then). No harness: the process exit status is the result.
//! The library is dlopen'ed, as a C host would load it. Passes on turso and
//! with `--features sqlite3`.
#![cfg(unix)]
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicPtr, Ordering};

type Close = unsafe extern "C" fn(*mut c_void) -> c_int;
type Exec = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *const c_void,
    *mut c_void,
    *mut *mut c_char,
) -> c_int;
type Open = unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> c_int;

static DB: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static CLOSE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

extern "C" fn at_exit() {
    let close: Close = unsafe { std::mem::transmute(CLOSE.load(Ordering::SeqCst)) };
    let rc = unsafe { close(DB.load(Ordering::SeqCst)) };
    if rc != 0 {
        eprintln!("sqlite3_close from atexit returned {rc}");
        unsafe { libc::_exit(2) };
    }
}

unsafe fn sym(lib: *mut c_void, name: &CStr) -> *mut c_void {
    let p = libc::dlsym(lib, name.as_ptr());
    assert!(!p.is_null(), "missing {name:?}");
    p
}

fn main() {
    let lib_name = if cfg!(feature = "sqlite3") {
        "libsqlite3.so.0"
    } else {
        "libturso_sqlite3.so"
    };
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    let local = dir.join(lib_name);
    let target = if !cfg!(feature = "sqlite3") && local.exists() {
        local.to_str().unwrap().to_string()
    } else {
        lib_name.to_string()
    };
    let target = CString::new(target).unwrap();
    let tmp = std::env::temp_dir().join(format!("turso-atexit-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let path = CString::new(tmp.join("a.db").to_str().unwrap()).unwrap();
    unsafe {
        let lib = libc::dlopen(target.as_ptr(), libc::RTLD_NOW);
        assert!(!lib.is_null(), "dlopen {target:?} failed");
        let open: Open = std::mem::transmute(sym(lib, c"sqlite3_open"));
        let exec: Exec = std::mem::transmute(sym(lib, c"sqlite3_exec"));
        CLOSE.store(sym(lib, c"sqlite3_close"), Ordering::SeqCst);
        let mut db = std::ptr::null_mut();
        assert_eq!(open(path.as_ptr(), &mut db), 0);
        for sql in [
            c"PRAGMA journal_mode=WAL",
            c"CREATE TABLE t(x)",
            c"INSERT INTO t VALUES (1)",
        ] {
            let (arg, err) = (std::ptr::null_mut(), std::ptr::null_mut());
            assert_eq!(exec(db, sql.as_ptr(), std::ptr::null(), arg, err), 0);
        }
        DB.store(db, Ordering::SeqCst);
        libc::atexit(at_exit);
    }
}
