//! Application VFS tests: an in-memory VFS and a counting shim over the
//! default VFS, both written here, plus direct use of the default VFS's
//! methods. Each must pass on turso and on real SQLite.
#![allow(non_camel_case_types, non_upper_case_globals)]
use libc::{c_char, c_int, c_void};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use super::config::lock;

#[repr(C)]
struct sqlite3 {
    _private: [u8; 0],
}

type VfsFn = Option<unsafe extern "C" fn()>;

#[repr(C)]
#[derive(Clone, Copy)]
struct sqlite3_vfs {
    i_version: c_int,
    sz_os_file: c_int,
    mx_pathname: c_int,
    p_next: *mut sqlite3_vfs,
    z_name: *const c_char,
    p_app_data: *mut c_void,
    x_open: Option<
        unsafe extern "C" fn(
            *mut sqlite3_vfs,
            *const c_char,
            *mut sqlite3_file,
            c_int,
            *mut c_int,
        ) -> c_int,
    >,
    x_delete: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char, c_int) -> c_int>,
    x_access:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char, c_int, *mut c_int) -> c_int>,
    x_full_pathname:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char, c_int, *mut c_char) -> c_int>,
    x_dl_open: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char) -> *mut c_void>,
    x_dl_error: Option<unsafe extern "C" fn(*mut sqlite3_vfs, c_int, *mut c_char)>,
    x_dl_sym: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *mut c_void, *const c_char) -> VfsFn>,
    x_dl_close: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *mut c_void)>,
    x_randomness: Option<unsafe extern "C" fn(*mut sqlite3_vfs, c_int, *mut c_char) -> c_int>,
    x_sleep: Option<unsafe extern "C" fn(*mut sqlite3_vfs, c_int) -> c_int>,
    x_current_time: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *mut f64) -> c_int>,
    x_get_last_error: Option<unsafe extern "C" fn(*mut sqlite3_vfs, c_int, *mut c_char) -> c_int>,
    x_current_time_int64: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *mut i64) -> c_int>,
    x_set_system_call:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char, VfsFn) -> c_int>,
    x_get_system_call: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char) -> VfsFn>,
    x_next_system_call:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char) -> *const c_char>,
}

#[repr(C)]
struct sqlite3_file {
    p_methods: *const sqlite3_io_methods,
}

#[repr(C)]
struct sqlite3_io_methods {
    i_version: c_int,
    x_close: Option<unsafe extern "C" fn(*mut sqlite3_file) -> c_int>,
    x_read: Option<unsafe extern "C" fn(*mut sqlite3_file, *mut c_void, c_int, i64) -> c_int>,
    x_write: Option<unsafe extern "C" fn(*mut sqlite3_file, *const c_void, c_int, i64) -> c_int>,
    x_truncate: Option<unsafe extern "C" fn(*mut sqlite3_file, i64) -> c_int>,
    x_sync: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int) -> c_int>,
    x_file_size: Option<unsafe extern "C" fn(*mut sqlite3_file, *mut i64) -> c_int>,
    x_lock: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int) -> c_int>,
    x_unlock: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int) -> c_int>,
    x_check_reserved_lock: Option<unsafe extern "C" fn(*mut sqlite3_file, *mut c_int) -> c_int>,
    x_file_control: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int, *mut c_void) -> c_int>,
    x_sector_size: Option<unsafe extern "C" fn(*mut sqlite3_file) -> c_int>,
    x_device_characteristics: Option<unsafe extern "C" fn(*mut sqlite3_file) -> c_int>,
    x_shm_map: Option<
        unsafe extern "C" fn(*mut sqlite3_file, c_int, c_int, c_int, *mut *mut c_void) -> c_int,
    >,
    x_shm_lock: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int, c_int, c_int) -> c_int>,
    x_shm_barrier: Option<unsafe extern "C" fn(*mut sqlite3_file)>,
    x_shm_unmap: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int) -> c_int>,
    x_fetch: Option<unsafe extern "C" fn(*mut sqlite3_file, i64, c_int, *mut *mut c_void) -> c_int>,
    x_unfetch: Option<unsafe extern "C" fn(*mut sqlite3_file, i64, *mut c_void) -> c_int>,
}

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
// config.rs declares the vfs_* functions with its own partial struct.
#[allow(clashing_extern_declarations)]
extern "C" {
    fn sqlite3_open(filename: *const c_char, db: *mut *mut sqlite3) -> c_int;
    fn sqlite3_open_v2(
        filename: *const c_char,
        db: *mut *mut sqlite3,
        flags: c_int,
        vfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close(db: *mut sqlite3) -> c_int;
    fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char;
    fn sqlite3_exec(
        db: *mut sqlite3,
        sql: *const c_char,
        cb: Option<
            unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
        >,
        arg: *mut c_void,
        err: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_file_control(
        db: *mut sqlite3,
        name: *const c_char,
        op: c_int,
        arg: *mut c_void,
    ) -> c_int;
    fn sqlite3_vfs_find(name: *const c_char) -> *mut sqlite3_vfs;
    fn sqlite3_vfs_register(vfs: *mut sqlite3_vfs, make_default: c_int) -> c_int;
    fn sqlite3_vfs_unregister(vfs: *mut sqlite3_vfs) -> c_int;
    #[cfg(not(feature = "sqlite3"))]
    fn turso_enable_experimental();
}

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_BUSY: c_int = 5;
const SQLITE_NOTFOUND: c_int = 12;
const SQLITE_CANTOPEN: c_int = 14;
const SQLITE_IOERR_SHORT_READ: c_int = 10 | (2 << 8);
const SQLITE_IOERR_DELETE_NOENT: c_int = 10 | (23 << 8);

const OPEN_READONLY: c_int = 0x1;
const OPEN_READWRITE: c_int = 0x2;
const OPEN_CREATE: c_int = 0x4;
const OPEN_DELETEONCLOSE: c_int = 0x8;
const OPEN_URI: c_int = 0x40;
const OPEN_MAIN_DB: c_int = 0x100;

const LOCK_NONE: c_int = 0;
const LOCK_SHARED: c_int = 1;
const LOCK_RESERVED: c_int = 2;
const LOCK_EXCLUSIVE: c_int = 4;

const SHM_UNLOCK: c_int = 1;
const SHM_LOCK: c_int = 2;
const SHM_SHARED: c_int = 4;
const SHM_EXCLUSIVE: c_int = 8;

const FCNTL_FILE_POINTER: c_int = 7;
const FCNTL_VFS_POINTER: c_int = 27;

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

unsafe fn errmsg(db: *mut sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db))
        .to_string_lossy()
        .into_owned()
}

unsafe fn exec(db: *mut sqlite3, sql: &str) {
    let rc = sqlite3_exec(db, cs(sql).as_ptr(), None, ptr::null_mut(), ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "{sql}: {}", errmsg(db));
}

/// All rows, columns joined by '|', rows by ','.
unsafe fn rows(db: *mut sqlite3, sql: &str) -> String {
    unsafe extern "C" fn cb(
        arg: *mut c_void,
        n: c_int,
        vals: *mut *mut c_char,
        _names: *mut *mut c_char,
    ) -> c_int {
        let out = &mut *(arg as *mut Vec<String>);
        let mut row = Vec::new();
        for i in 0..n as usize {
            let v = *vals.add(i);
            row.push(if v.is_null() {
                "NULL".to_string()
            } else {
                CStr::from_ptr(v).to_string_lossy().into_owned()
            });
        }
        out.push(row.join("|"));
        0
    }
    let mut out: Vec<String> = Vec::new();
    let rc = sqlite3_exec(
        db,
        cs(sql).as_ptr(),
        Some(cb),
        &mut out as *mut _ as *mut c_void,
        ptr::null_mut(),
    );
    assert_eq!(rc, SQLITE_OK, "{sql}: {}", errmsg(db));
    out.join(",")
}

unsafe fn open_v2(path: &str, flags: c_int, vfs: Option<&CStr>) -> (c_int, *mut sqlite3) {
    let mut db = ptr::null_mut();
    let rc = sqlite3_open_v2(
        cs(path).as_ptr(),
        &mut db,
        flags,
        vfs.map_or(ptr::null(), |v| v.as_ptr()),
    );
    (rc, db)
}

/// A zeroed buffer to hold a `sqlite3_file` of `n` bytes.
fn file_buf(n: c_int) -> Vec<u64> {
    vec![0u64; (n.max(8) as usize).div_ceil(8)]
}

// ---------------------------------------------------------------------------
// In-memory VFS (iVersion 1, no shm)
// ---------------------------------------------------------------------------

static MEM_FILES: Mutex<Option<HashMap<String, Vec<u8>>>> = Mutex::new(None);

fn mem<R>(f: impl FnOnce(&mut HashMap<String, Vec<u8>>) -> R) -> R {
    let mut g = MEM_FILES.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(HashMap::new))
}

#[repr(C)]
struct MemFile {
    base: sqlite3_file,
    name: *mut c_char,
    delete_on_close: c_int,
}

unsafe fn mem_name(f: *mut sqlite3_file) -> String {
    CStr::from_ptr((*(f as *mut MemFile)).name)
        .to_string_lossy()
        .into_owned()
}

unsafe extern "C" fn mem_close(f: *mut sqlite3_file) -> c_int {
    let mf = &mut *(f as *mut MemFile);
    if mf.delete_on_close != 0 {
        let n = mem_name(f);
        mem(|m| m.remove(&n));
    }
    drop(CString::from_raw(mf.name));
    SQLITE_OK
}

unsafe extern "C" fn mem_read(
    f: *mut sqlite3_file,
    buf: *mut c_void,
    amt: c_int,
    off: i64,
) -> c_int {
    let n = mem_name(f);
    let out = std::slice::from_raw_parts_mut(buf as *mut u8, amt as usize);
    mem(|m| {
        let data = m.get(&n).map(|v| v.as_slice()).unwrap_or(&[]);
        let off = off as usize;
        let avail = data.len().saturating_sub(off).min(out.len());
        if avail > 0 {
            out[..avail].copy_from_slice(&data[off..off + avail]);
        }
        out[avail..].fill(0);
        if avail < out.len() {
            SQLITE_IOERR_SHORT_READ
        } else {
            SQLITE_OK
        }
    })
}

unsafe extern "C" fn mem_write(
    f: *mut sqlite3_file,
    buf: *const c_void,
    amt: c_int,
    off: i64,
) -> c_int {
    let n = mem_name(f);
    let src = std::slice::from_raw_parts(buf as *const u8, amt as usize);
    mem(|m| {
        let data = m.entry(n).or_default();
        let end = off as usize + src.len();
        if data.len() < end {
            data.resize(end, 0);
        }
        data[off as usize..end].copy_from_slice(src);
    });
    SQLITE_OK
}

unsafe extern "C" fn mem_truncate(f: *mut sqlite3_file, size: i64) -> c_int {
    let n = mem_name(f);
    mem(|m| m.entry(n).or_default().resize(size as usize, 0));
    SQLITE_OK
}

unsafe extern "C" fn mem_sync(_: *mut sqlite3_file, _: c_int) -> c_int {
    SQLITE_OK
}

unsafe extern "C" fn mem_file_size(f: *mut sqlite3_file, out: *mut i64) -> c_int {
    let n = mem_name(f);
    *out = mem(|m| m.get(&n).map_or(0, |v| v.len())) as i64;
    SQLITE_OK
}

unsafe extern "C" fn mem_lock(_: *mut sqlite3_file, _: c_int) -> c_int {
    SQLITE_OK
}

unsafe extern "C" fn mem_check_reserved(_: *mut sqlite3_file, out: *mut c_int) -> c_int {
    *out = 0;
    SQLITE_OK
}

unsafe extern "C" fn mem_file_control(_: *mut sqlite3_file, op: c_int, arg: *mut c_void) -> c_int {
    // A private opcode, to see that file_control reaches the VFS.
    if op == 0x7e57 {
        *(arg as *mut c_int) = 4242;
        return SQLITE_OK;
    }
    SQLITE_NOTFOUND
}

unsafe extern "C" fn mem_sector_size(_: *mut sqlite3_file) -> c_int {
    512
}

unsafe extern "C" fn mem_device_characteristics(_: *mut sqlite3_file) -> c_int {
    0
}

static MEM_METHODS: sqlite3_io_methods = sqlite3_io_methods {
    i_version: 1,
    x_close: Some(mem_close),
    x_read: Some(mem_read),
    x_write: Some(mem_write),
    x_truncate: Some(mem_truncate),
    x_sync: Some(mem_sync),
    x_file_size: Some(mem_file_size),
    x_lock: Some(mem_lock),
    x_unlock: Some(mem_lock),
    x_check_reserved_lock: Some(mem_check_reserved),
    x_file_control: Some(mem_file_control),
    x_sector_size: Some(mem_sector_size),
    x_device_characteristics: Some(mem_device_characteristics),
    x_shm_map: None,
    x_shm_lock: None,
    x_shm_barrier: None,
    x_shm_unmap: None,
    x_fetch: None,
    x_unfetch: None,
};

static MEM_ANON: AtomicUsize = AtomicUsize::new(0);
static MEM_OPEN_FLAGS: Mutex<Vec<(String, c_int)>> = Mutex::new(Vec::new());

unsafe extern "C" fn mem_open(
    _: *mut sqlite3_vfs,
    name: *const c_char,
    f: *mut sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    let mf = &mut *(f as *mut MemFile);
    let name = if name.is_null() {
        format!("anon-{}", MEM_ANON.fetch_add(1, Ordering::Relaxed))
    } else {
        CStr::from_ptr(name).to_string_lossy().into_owned()
    };
    MEM_OPEN_FLAGS.lock().unwrap().push((name.clone(), flags));
    let exists = mem(|m| m.contains_key(&name));
    if !exists {
        if flags & OPEN_CREATE == 0 {
            return SQLITE_CANTOPEN;
        }
        mem(|m| m.insert(name.clone(), Vec::new()));
    }
    mf.name = CString::new(name).unwrap().into_raw();
    mf.delete_on_close = flags & OPEN_DELETEONCLOSE;
    mf.base.p_methods = &MEM_METHODS;
    if !out_flags.is_null() {
        *out_flags = flags;
    }
    SQLITE_OK
}

unsafe extern "C" fn mem_delete(_: *mut sqlite3_vfs, name: *const c_char, _: c_int) -> c_int {
    let n = CStr::from_ptr(name).to_string_lossy().into_owned();
    if mem(|m| m.remove(&n)).is_some() {
        SQLITE_OK
    } else {
        SQLITE_IOERR_DELETE_NOENT
    }
}

unsafe extern "C" fn mem_access(
    _: *mut sqlite3_vfs,
    name: *const c_char,
    _: c_int,
    out: *mut c_int,
) -> c_int {
    let n = CStr::from_ptr(name).to_string_lossy().into_owned();
    *out = mem(|m| m.get(&n).is_some_and(|v| !v.is_empty())) as c_int;
    SQLITE_OK
}

unsafe extern "C" fn mem_full_pathname(
    _: *mut sqlite3_vfs,
    name: *const c_char,
    n_out: c_int,
    out: *mut c_char,
) -> c_int {
    // Names live in the VFS's own namespace: "/mem/<name>".
    let n = CStr::from_ptr(name).to_bytes();
    let full = if n.starts_with(b"/") {
        n.to_vec()
    } else {
        [b"/mem/".as_slice(), n].concat()
    };
    if full.len() + 1 > n_out as usize {
        return SQLITE_CANTOPEN;
    }
    ptr::copy_nonoverlapping(full.as_ptr(), out as *mut u8, full.len());
    *out.add(full.len()) = 0;
    SQLITE_OK
}

unsafe extern "C" fn mem_randomness(_: *mut sqlite3_vfs, n: c_int, out: *mut c_char) -> c_int {
    ptr::write_bytes(out, 7, n as usize);
    n
}

unsafe extern "C" fn mem_sleep(_: *mut sqlite3_vfs, us: c_int) -> c_int {
    us
}

unsafe extern "C" fn mem_current_time(_: *mut sqlite3_vfs, out: *mut f64) -> c_int {
    *out = 2440587.5 + 365.0; // 1971-01-01
    SQLITE_OK
}

unsafe extern "C" fn mem_get_last_error(_: *mut sqlite3_vfs, _: c_int, _: *mut c_char) -> c_int {
    0
}

fn mem_vfs(name: &'static CStr) -> *mut sqlite3_vfs {
    Box::into_raw(Box::new(sqlite3_vfs {
        i_version: 1,
        sz_os_file: std::mem::size_of::<MemFile>() as c_int,
        mx_pathname: 256,
        p_next: ptr::null_mut(),
        z_name: name.as_ptr(),
        p_app_data: ptr::null_mut(),
        x_open: Some(mem_open),
        x_delete: Some(mem_delete),
        x_access: Some(mem_access),
        x_full_pathname: Some(mem_full_pathname),
        x_dl_open: None,
        x_dl_error: None,
        x_dl_sym: None,
        x_dl_close: None,
        x_randomness: Some(mem_randomness),
        x_sleep: Some(mem_sleep),
        x_current_time: Some(mem_current_time),
        x_get_last_error: Some(mem_get_last_error),
        x_current_time_int64: None,
        x_set_system_call: None,
        x_get_system_call: None,
        x_next_system_call: None,
    }))
}

#[test]
fn memory_vfs_holds_the_whole_database() {
    let _g = lock();
    unsafe {
        let name = c"compat-memvfs";
        let vfs = mem_vfs(name);
        assert_eq!(sqlite3_vfs_register(vfs, 0), SQLITE_OK);
        let cwd_file = std::path::Path::new("memvfs-test.db");
        let _ = std::fs::remove_file(cwd_file);

        let (rc, db) = open_v2("memvfs-test.db", OPEN_READWRITE | OPEN_CREATE, Some(name));
        assert_eq!(rc, SQLITE_OK);
        exec(db, "CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)");
        exec(
            db,
            "INSERT INTO t(b) VALUES ('one'), ('two'), (randomblob(5000))",
        );
        exec(db, "CREATE INDEX tb ON t(b)");
        assert_eq!(rows(db, "SELECT a, b FROM t WHERE a < 3"), "1|one,2|two");
        // sqlite3_file_control goes to the VFS's xFileControl.
        let mut v: c_int = 0;
        assert_eq!(
            sqlite3_file_control(db, ptr::null(), 0x7e57, &mut v as *mut c_int as *mut c_void),
            SQLITE_OK
        );
        assert_eq!(v, 4242);
        let mut p: *mut sqlite3_vfs = ptr::null_mut();
        assert_eq!(
            sqlite3_file_control(
                db,
                ptr::null(),
                FCNTL_VFS_POINTER,
                &mut p as *mut _ as *mut c_void
            ),
            SQLITE_OK
        );
        assert_eq!(p, vfs);
        assert_eq!(sqlite3_close(db), SQLITE_OK);

        // Nothing reached the real file system; the VFS has a database.
        assert!(!cwd_file.exists());
        let header = mem(|m| m.get("/mem/memvfs-test.db").map(|v| v[..16].to_vec()));
        assert_eq!(header.as_deref(), Some(&b"SQLite format 3\0"[..]));
        let flags = MEM_OPEN_FLAGS.lock().unwrap().clone();
        assert!(flags
            .iter()
            .any(|(n, f)| n == "/mem/memvfs-test.db" && f & OPEN_MAIN_DB != 0));

        // Reopen, read-only, through the VFS.
        let (rc, db) = open_v2("memvfs-test.db", OPEN_READONLY, Some(name));
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(rows(db, "SELECT count(*), max(length(b)) FROM t"), "3|5000");
        assert_eq!(rows(db, "PRAGMA integrity_check"), "ok");
        assert_eq!(sqlite3_close(db), SQLITE_OK);

        // A missing database without CREATE fails in xOpen.
        let (rc, db) = open_v2("nope.db", OPEN_READWRITE, Some(name));
        assert_eq!(rc, SQLITE_CANTOPEN);
        sqlite3_close(db);

        assert_eq!(sqlite3_vfs_unregister(vfs), SQLITE_OK);
        mem(|m| m.retain(|k, _| !k.starts_with("/mem/memvfs-test.db")));
    }
}

#[test]
fn attach_uses_the_connection_vfs_or_the_uri_vfs() {
    let _g = lock();
    unsafe {
        // ATTACH is behind turso's experimental switch.
        #[cfg(not(feature = "sqlite3"))]
        turso_enable_experimental();
        let name = c"compat-memvfs-attach";
        let vfs = mem_vfs(name);
        assert_eq!(sqlite3_vfs_register(vfs, 0), SQLITE_OK);
        let (rc, db) = open_v2(
            "file:attach-main.db?vfs=compat-memvfs-attach",
            OPEN_READWRITE | OPEN_CREATE | OPEN_URI,
            None,
        );
        assert_eq!(rc, SQLITE_OK);
        exec(db, "ATTACH 'attach-aux.db' AS aux");
        exec(db, "CREATE TABLE aux.a(x); INSERT INTO aux.a VALUES (42)");
        exec(
            db,
            "ATTACH 'file:attach-aux2.db?vfs=compat-memvfs-attach' AS aux2",
        );
        exec(
            db,
            "CREATE TABLE aux2.b(y); INSERT INTO aux2.b SELECT x + 1 FROM aux.a",
        );
        assert_eq!(rows(db, "SELECT y FROM aux2.b"), "43");
        assert_eq!(sqlite3_close(db), SQLITE_OK);
        assert!(!std::path::Path::new("attach-aux.db").exists());
        assert!(mem(
            |m| m.contains_key("/mem/attach-aux.db") && m.contains_key("/mem/attach-aux2.db")
        ));
        assert_eq!(sqlite3_vfs_unregister(vfs), SQLITE_OK);
        mem(|m| m.retain(|k, _| !k.starts_with("/mem/attach-")));
    }
}

// ---------------------------------------------------------------------------
// Counting shim over the default VFS
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Counts {
    open: AtomicUsize,
    delete: AtomicUsize,
    access: AtomicUsize,
    full_pathname: AtomicUsize,
    read: AtomicUsize,
    write: AtomicUsize,
    sync: AtomicUsize,
    file_size: AtomicUsize,
    lock: AtomicUsize,
    unlock: AtomicUsize,
    close: AtomicUsize,
    shm_map: AtomicUsize,
    shm_lock: AtomicUsize,
}

impl Counts {
    const fn new() -> Self {
        Counts {
            open: AtomicUsize::new(0),
            delete: AtomicUsize::new(0),
            access: AtomicUsize::new(0),
            full_pathname: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
            sync: AtomicUsize::new(0),
            file_size: AtomicUsize::new(0),
            lock: AtomicUsize::new(0),
            unlock: AtomicUsize::new(0),
            close: AtomicUsize::new(0),
            shm_map: AtomicUsize::new(0),
            shm_lock: AtomicUsize::new(0),
        }
    }
    fn get(c: &AtomicUsize) -> usize {
        c.load(Ordering::SeqCst)
    }
}

static SHIM: Counts = Counts::new();

#[repr(C)]
struct ShimFile {
    base: sqlite3_file,
    real: *mut sqlite3_file,
}

unsafe fn real(f: *mut sqlite3_file) -> *mut sqlite3_file {
    (*(f as *mut ShimFile)).real
}

unsafe fn real_m(f: *mut sqlite3_file) -> &'static sqlite3_io_methods {
    &*(*real(f)).p_methods
}

unsafe fn parent(v: *mut sqlite3_vfs) -> *mut sqlite3_vfs {
    (*v).p_app_data as *mut sqlite3_vfs
}

unsafe extern "C" fn shim_close(f: *mut sqlite3_file) -> c_int {
    SHIM.close.fetch_add(1, Ordering::SeqCst);
    (real_m(f).x_close.unwrap())(real(f))
}
unsafe extern "C" fn shim_read(f: *mut sqlite3_file, b: *mut c_void, n: c_int, o: i64) -> c_int {
    SHIM.read.fetch_add(1, Ordering::SeqCst);
    (real_m(f).x_read.unwrap())(real(f), b, n, o)
}
unsafe extern "C" fn shim_write(f: *mut sqlite3_file, b: *const c_void, n: c_int, o: i64) -> c_int {
    SHIM.write.fetch_add(1, Ordering::SeqCst);
    (real_m(f).x_write.unwrap())(real(f), b, n, o)
}
unsafe extern "C" fn shim_truncate(f: *mut sqlite3_file, s: i64) -> c_int {
    (real_m(f).x_truncate.unwrap())(real(f), s)
}
unsafe extern "C" fn shim_sync(f: *mut sqlite3_file, fl: c_int) -> c_int {
    SHIM.sync.fetch_add(1, Ordering::SeqCst);
    (real_m(f).x_sync.unwrap())(real(f), fl)
}
unsafe extern "C" fn shim_file_size(f: *mut sqlite3_file, o: *mut i64) -> c_int {
    SHIM.file_size.fetch_add(1, Ordering::SeqCst);
    (real_m(f).x_file_size.unwrap())(real(f), o)
}
unsafe extern "C" fn shim_lock(f: *mut sqlite3_file, l: c_int) -> c_int {
    SHIM.lock.fetch_add(1, Ordering::SeqCst);
    (real_m(f).x_lock.unwrap())(real(f), l)
}
unsafe extern "C" fn shim_unlock(f: *mut sqlite3_file, l: c_int) -> c_int {
    SHIM.unlock.fetch_add(1, Ordering::SeqCst);
    (real_m(f).x_unlock.unwrap())(real(f), l)
}
unsafe extern "C" fn shim_check_reserved(f: *mut sqlite3_file, o: *mut c_int) -> c_int {
    (real_m(f).x_check_reserved_lock.unwrap())(real(f), o)
}
unsafe extern "C" fn shim_file_control(f: *mut sqlite3_file, op: c_int, a: *mut c_void) -> c_int {
    (real_m(f).x_file_control.unwrap())(real(f), op, a)
}
unsafe extern "C" fn shim_sector_size(f: *mut sqlite3_file) -> c_int {
    (real_m(f).x_sector_size.unwrap())(real(f))
}
unsafe extern "C" fn shim_device_characteristics(f: *mut sqlite3_file) -> c_int {
    (real_m(f).x_device_characteristics.unwrap())(real(f))
}
unsafe extern "C" fn shim_shm_map(
    f: *mut sqlite3_file,
    r: c_int,
    s: c_int,
    e: c_int,
    o: *mut *mut c_void,
) -> c_int {
    SHIM.shm_map.fetch_add(1, Ordering::SeqCst);
    (real_m(f).x_shm_map.unwrap())(real(f), r, s, e, o)
}
unsafe extern "C" fn shim_shm_lock(f: *mut sqlite3_file, o: c_int, n: c_int, fl: c_int) -> c_int {
    SHIM.shm_lock.fetch_add(1, Ordering::SeqCst);
    (real_m(f).x_shm_lock.unwrap())(real(f), o, n, fl)
}
unsafe extern "C" fn shim_shm_barrier(f: *mut sqlite3_file) {
    (real_m(f).x_shm_barrier.unwrap())(real(f))
}
unsafe extern "C" fn shim_shm_unmap(f: *mut sqlite3_file, d: c_int) -> c_int {
    (real_m(f).x_shm_unmap.unwrap())(real(f), d)
}

static SHIM_METHODS: sqlite3_io_methods = sqlite3_io_methods {
    i_version: 2,
    x_close: Some(shim_close),
    x_read: Some(shim_read),
    x_write: Some(shim_write),
    x_truncate: Some(shim_truncate),
    x_sync: Some(shim_sync),
    x_file_size: Some(shim_file_size),
    x_lock: Some(shim_lock),
    x_unlock: Some(shim_unlock),
    x_check_reserved_lock: Some(shim_check_reserved),
    x_file_control: Some(shim_file_control),
    x_sector_size: Some(shim_sector_size),
    x_device_characteristics: Some(shim_device_characteristics),
    x_shm_map: Some(shim_shm_map),
    x_shm_lock: Some(shim_shm_lock),
    x_shm_barrier: Some(shim_shm_barrier),
    x_shm_unmap: Some(shim_shm_unmap),
    x_fetch: None,
    x_unfetch: None,
};

unsafe extern "C" fn shim_open(
    v: *mut sqlite3_vfs,
    name: *const c_char,
    f: *mut sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    SHIM.open.fetch_add(1, Ordering::SeqCst);
    let sf = &mut *(f as *mut ShimFile);
    sf.base.p_methods = ptr::null();
    // The parent's file lives right after ours (szOsFile covers both).
    sf.real = (f as *mut u8).add(std::mem::size_of::<ShimFile>()) as *mut sqlite3_file;
    let p = parent(v);
    let rc = ((*p).x_open.unwrap())(p, name, sf.real, flags, out);
    if !(*sf.real).p_methods.is_null() {
        sf.base.p_methods = &SHIM_METHODS;
    }
    rc
}
unsafe extern "C" fn shim_delete(v: *mut sqlite3_vfs, n: *const c_char, s: c_int) -> c_int {
    SHIM.delete.fetch_add(1, Ordering::SeqCst);
    let p = parent(v);
    ((*p).x_delete.unwrap())(p, n, s)
}
unsafe extern "C" fn shim_access(
    v: *mut sqlite3_vfs,
    n: *const c_char,
    fl: c_int,
    o: *mut c_int,
) -> c_int {
    SHIM.access.fetch_add(1, Ordering::SeqCst);
    let p = parent(v);
    ((*p).x_access.unwrap())(p, n, fl, o)
}
unsafe extern "C" fn shim_full_pathname(
    v: *mut sqlite3_vfs,
    n: *const c_char,
    no: c_int,
    o: *mut c_char,
) -> c_int {
    SHIM.full_pathname.fetch_add(1, Ordering::SeqCst);
    let p = parent(v);
    ((*p).x_full_pathname.unwrap())(p, n, no, o)
}

/// A shim named `name` over the current default VFS, as GDAL and proj build.
unsafe fn shim_vfs(name: &'static CStr) -> *mut sqlite3_vfs {
    let dflt = sqlite3_vfs_find(ptr::null());
    assert!(!dflt.is_null());
    let mut v = *dflt;
    v.i_version = (*dflt).i_version.min(3);
    v.sz_os_file = std::mem::size_of::<ShimFile>() as c_int + (*dflt).sz_os_file;
    v.p_next = ptr::null_mut();
    v.z_name = name.as_ptr();
    v.p_app_data = dflt as *mut c_void;
    v.x_open = Some(shim_open);
    v.x_delete = Some(shim_delete);
    v.x_access = Some(shim_access);
    v.x_full_pathname = Some(shim_full_pathname);
    Box::into_raw(Box::new(v))
}

#[test]
fn shim_vfs_routes_file_io_to_the_default_vfs() {
    let _g = lock();
    unsafe {
        let name = c"compat-shim";
        let vfs = shim_vfs(name);
        assert_eq!(sqlite3_vfs_register(vfs, 0), SQLITE_OK);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shim.db");
        let p = path.to_str().unwrap();

        let opens = Counts::get(&SHIM.open);
        let writes = Counts::get(&SHIM.write);
        let reads = Counts::get(&SHIM.read);
        let locks = Counts::get(&SHIM.lock);
        let (rc, db) = open_v2(p, OPEN_READWRITE | OPEN_CREATE, Some(name));
        assert_eq!(rc, SQLITE_OK);
        exec(db, "CREATE TABLE t(x)");
        exec(db, "BEGIN; INSERT INTO t VALUES (1), (2), (3); COMMIT");
        assert_eq!(rows(db, "SELECT sum(x) FROM t"), "6");

        // FILE_POINTER is the shim's sqlite3_file, VFS_POINTER the shim.
        let mut fp: *mut sqlite3_file = ptr::null_mut();
        assert_eq!(
            sqlite3_file_control(
                db,
                c"main".as_ptr(),
                FCNTL_FILE_POINTER,
                &mut fp as *mut _ as *mut c_void
            ),
            SQLITE_OK
        );
        assert!(!fp.is_null());
        assert!(ptr::eq((*fp).p_methods, &SHIM_METHODS));
        let mut vp: *mut sqlite3_vfs = ptr::null_mut();
        assert_eq!(
            sqlite3_file_control(
                db,
                c"main".as_ptr(),
                FCNTL_VFS_POINTER,
                &mut vp as *mut _ as *mut c_void
            ),
            SQLITE_OK
        );
        assert_eq!(vp, vfs);
        // Opcodes the shim passes through reach the default VFS.
        let mut name_out: *mut c_char = ptr::null_mut();
        assert_eq!(
            sqlite3_file_control(
                db,
                ptr::null(),
                12, /* VFSNAME */
                &mut name_out as *mut _ as *mut c_void
            ),
            SQLITE_OK
        );
        assert!(!name_out.is_null());
        assert_eq!(sqlite3_close(db), SQLITE_OK);

        assert!(Counts::get(&SHIM.open) > opens, "xOpen");
        assert!(Counts::get(&SHIM.write) > writes, "xWrite");
        assert!(Counts::get(&SHIM.read) > reads, "xRead");
        assert!(Counts::get(&SHIM.lock) > locks, "xLock");
        assert!(Counts::get(&SHIM.full_pathname) > 0, "xFullPathname");
        assert!(Counts::get(&SHIM.sync) > 0, "xSync");
        assert_eq!(
            Counts::get(&SHIM.open),
            Counts::get(&SHIM.close),
            "every file closed"
        );

        // The data is in the real file: a plain connection reads it.
        let mut plain = ptr::null_mut();
        assert_eq!(sqlite3_open(cs(p).as_ptr(), &mut plain), SQLITE_OK);
        assert_eq!(rows(plain, "SELECT group_concat(x) FROM t"), "1,2,3");
        assert_eq!(sqlite3_close(plain), SQLITE_OK);

        // The `vfs=` URI parameter selects it too.
        let opens = Counts::get(&SHIM.open);
        let uri = format!("file:{p}?vfs=compat-shim");
        let (rc, db) = open_v2(&uri, OPEN_READWRITE | OPEN_URI, None);
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(rows(db, "SELECT count(*) FROM t"), "3");
        assert_eq!(sqlite3_close(db), SQLITE_OK);
        assert!(Counts::get(&SHIM.open) > opens, "xOpen through vfs=");

        // nolock=1: no xLock/xUnlock calls at all.
        let locks = Counts::get(&SHIM.lock);
        let unlocks = Counts::get(&SHIM.unlock);
        let (rc, db) = open_v2(
            &format!("file:{p}?vfs=compat-shim&nolock=1"),
            OPEN_READWRITE | OPEN_URI,
            None,
        );
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(rows(db, "SELECT max(x) FROM t"), "3");
        assert_eq!(sqlite3_close(db), SQLITE_OK);
        assert_eq!(Counts::get(&SHIM.lock), locks, "xLock with nolock=1");
        assert_eq!(Counts::get(&SHIM.unlock), unlocks, "xUnlock with nolock=1");

        // An unknown `vfs=` fails like SQLite.
        let (rc, db) = open_v2(
            &format!("file:{p}?vfs=nosuchvfs"),
            OPEN_READWRITE | OPEN_URI,
            None,
        );
        assert_eq!(rc, SQLITE_ERROR);
        sqlite3_close(db);

        assert_eq!(sqlite3_vfs_unregister(vfs), SQLITE_OK);
    }
}

#[test]
fn default_vfs_registered_by_the_application_is_used_by_sqlite3_open() {
    let _g = lock();
    unsafe {
        let name = c"compat-shim-default";
        let vfs = shim_vfs(name);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("dflt.db");
        let opens = Counts::get(&SHIM.open);
        assert_eq!(sqlite3_vfs_register(vfs, 1), SQLITE_OK);
        let mut db = ptr::null_mut();
        let rc = sqlite3_open(cs(p.to_str().unwrap()).as_ptr(), &mut db);
        assert_eq!(sqlite3_vfs_unregister(vfs), SQLITE_OK);
        assert_eq!(rc, SQLITE_OK);
        exec(
            db,
            "CREATE TABLE t(x); INSERT INTO t VALUES ('via default')",
        );
        assert_eq!(rows(db, "SELECT x FROM t"), "via default");
        assert_eq!(sqlite3_close(db), SQLITE_OK);
        assert!(Counts::get(&SHIM.open) > opens);
    }
}

// ---------------------------------------------------------------------------
// The default VFS's methods, called directly
// ---------------------------------------------------------------------------

unsafe fn vopen(
    v: *mut sqlite3_vfs,
    path: &CStr,
    flags: c_int,
    buf: &mut Vec<u64>,
) -> *mut sqlite3_file {
    *buf = file_buf((*v).sz_os_file);
    let f = buf.as_mut_ptr() as *mut sqlite3_file;
    let mut out = 0;
    let rc = ((*v).x_open.unwrap())(v, path.as_ptr(), f, flags, &mut out);
    assert_eq!(rc, SQLITE_OK);
    assert_eq!(out & OPEN_READWRITE, flags & OPEN_READWRITE);
    f
}

unsafe fn m(f: *mut sqlite3_file) -> &'static sqlite3_io_methods {
    &*(*f).p_methods
}

#[test]
fn default_vfs_methods_work() {
    let _g = lock();
    unsafe {
        let v = sqlite3_vfs_find(ptr::null());
        assert!((*v).i_version >= 2);
        let dir = tempfile::tempdir().unwrap();
        let path = cs(dir.path().join("raw.db").to_str().unwrap());

        // xFullPathname of a relative name is absolute.
        let mut full = vec![0u8; (*v).mx_pathname as usize + 1];
        assert_eq!(
            ((*v).x_full_pathname.unwrap())(
                v,
                c"rel.db".as_ptr(),
                full.len() as c_int,
                full.as_mut_ptr() as *mut c_char
            ),
            SQLITE_OK
        );
        assert!(CStr::from_ptr(full.as_ptr() as *const c_char)
            .to_bytes()
            .starts_with(b"/"));

        let mut ba = Vec::new();
        let a = vopen(
            v,
            &path,
            OPEN_READWRITE | OPEN_CREATE | OPEN_MAIN_DB,
            &mut ba,
        );
        let data = b"hello, vfs";
        assert_eq!(
            (m(a).x_write.unwrap())(a, data.as_ptr() as *const c_void, data.len() as c_int, 100),
            SQLITE_OK
        );
        let mut size = 0i64;
        assert_eq!((m(a).x_file_size.unwrap())(a, &mut size), SQLITE_OK);
        assert_eq!(size, 110);
        let mut back = [0u8; 10];
        assert_eq!(
            (m(a).x_read.unwrap())(a, back.as_mut_ptr() as *mut c_void, 10, 100),
            SQLITE_OK
        );
        assert_eq!(&back, data);
        // A read past the end is a zero-filled short read.
        let mut tail = [9u8; 20];
        assert_eq!(
            (m(a).x_read.unwrap())(a, tail.as_mut_ptr() as *mut c_void, 20, 105),
            SQLITE_IOERR_SHORT_READ
        );
        assert_eq!(&tail[..5], &data[5..]);
        assert!(tail[5..].iter().all(|&b| b == 0));
        assert_eq!((m(a).x_sync.unwrap())(a, 2), SQLITE_OK);
        assert_eq!((m(a).x_truncate.unwrap())(a, 50), SQLITE_OK);
        assert_eq!((m(a).x_file_size.unwrap())(a, &mut size), SQLITE_OK);
        assert_eq!(size, 50);
        assert!((m(a).x_sector_size.unwrap())(a) > 0);

        let mut exists = 0;
        assert_eq!(
            ((*v).x_access.unwrap())(v, path.as_ptr(), 0, &mut exists),
            SQLITE_OK
        );
        assert_eq!(exists, 1);

        // Lock protocol between two handles of this process.
        let mut bb = Vec::new();
        let b = vopen(v, &path, OPEN_READWRITE | OPEN_MAIN_DB, &mut bb);
        assert_eq!((m(a).x_lock.unwrap())(a, LOCK_SHARED), SQLITE_OK);
        assert_eq!((m(a).x_lock.unwrap())(a, LOCK_RESERVED), SQLITE_OK);
        let mut res = 0;
        assert_eq!(
            (m(b).x_check_reserved_lock.unwrap())(b, &mut res),
            SQLITE_OK
        );
        assert_eq!(res, 1);
        assert_eq!((m(b).x_lock.unwrap())(b, LOCK_SHARED), SQLITE_OK);
        assert_eq!((m(b).x_lock.unwrap())(b, LOCK_RESERVED), SQLITE_BUSY);
        assert_eq!((m(a).x_lock.unwrap())(a, LOCK_EXCLUSIVE), SQLITE_BUSY);
        assert_eq!((m(b).x_unlock.unwrap())(b, LOCK_NONE), SQLITE_OK);
        assert_eq!((m(a).x_lock.unwrap())(a, LOCK_EXCLUSIVE), SQLITE_OK);
        assert_eq!((m(b).x_lock.unwrap())(b, LOCK_SHARED), SQLITE_BUSY);
        assert_eq!((m(a).x_unlock.unwrap())(a, LOCK_SHARED), SQLITE_OK);
        assert_eq!((m(b).x_lock.unwrap())(b, LOCK_SHARED), SQLITE_OK);
        assert_eq!((m(a).x_unlock.unwrap())(a, LOCK_NONE), SQLITE_OK);
        assert_eq!((m(b).x_unlock.unwrap())(b, LOCK_NONE), SQLITE_OK);

        // Shared memory: a region is shared, and shm locks conflict.
        let (mut pa, mut pb): (*mut c_void, *mut c_void) = (ptr::null_mut(), ptr::null_mut());
        assert_eq!(
            (m(a).x_shm_map.unwrap())(a, 0, 32768, 1, &mut pa),
            SQLITE_OK
        );
        assert_eq!(
            (m(b).x_shm_map.unwrap())(b, 0, 32768, 1, &mut pb),
            SQLITE_OK
        );
        assert!(!pa.is_null() && !pb.is_null());
        *(pa as *mut u32).add(10) = 0xfeed;
        (m(a).x_shm_barrier.unwrap())(a);
        assert_eq!(*(pb as *const u32).add(10), 0xfeed);
        assert_eq!(
            (m(a).x_shm_lock.unwrap())(a, 3, 1, SHM_LOCK | SHM_SHARED),
            SQLITE_OK
        );
        assert_eq!(
            (m(b).x_shm_lock.unwrap())(b, 3, 1, SHM_LOCK | SHM_SHARED),
            SQLITE_OK
        );
        assert_eq!(
            (m(b).x_shm_lock.unwrap())(b, 3, 1, SHM_UNLOCK | SHM_SHARED),
            SQLITE_OK
        );
        assert_eq!(
            (m(b).x_shm_lock.unwrap())(b, 3, 1, SHM_LOCK | SHM_EXCLUSIVE),
            SQLITE_BUSY
        );
        assert_eq!(
            (m(a).x_shm_lock.unwrap())(a, 3, 1, SHM_UNLOCK | SHM_SHARED),
            SQLITE_OK
        );
        assert_eq!(
            (m(b).x_shm_lock.unwrap())(b, 3, 1, SHM_LOCK | SHM_EXCLUSIVE),
            SQLITE_OK
        );
        assert_eq!(
            (m(b).x_shm_lock.unwrap())(b, 3, 1, SHM_UNLOCK | SHM_EXCLUSIVE),
            SQLITE_OK
        );
        assert_eq!((m(b).x_shm_unmap.unwrap())(b, 0), SQLITE_OK);
        assert_eq!((m(a).x_shm_unmap.unwrap())(a, 1), SQLITE_OK);

        assert_eq!((m(b).x_close.unwrap())(b), SQLITE_OK);
        assert_eq!((m(a).x_close.unwrap())(a), SQLITE_OK);
        assert_eq!(((*v).x_delete.unwrap())(v, path.as_ptr(), 0), SQLITE_OK);
        assert_eq!(
            ((*v).x_access.unwrap())(v, path.as_ptr(), 0, &mut exists),
            SQLITE_OK
        );
        assert_eq!(exists, 0);
        assert_eq!(
            ((*v).x_delete.unwrap())(v, path.as_ptr(), 0),
            SQLITE_IOERR_DELETE_NOENT
        );

        // An anonymous temp file.
        let mut bt = file_buf((*v).sz_os_file);
        let t = bt.as_mut_ptr() as *mut sqlite3_file;
        let mut out = 0;
        assert_eq!(
            ((*v).x_open.unwrap())(
                v,
                ptr::null(),
                t,
                OPEN_READWRITE | OPEN_CREATE | OPEN_DELETEONCLOSE | 0x200,
                &mut out
            ),
            SQLITE_OK
        );
        assert_eq!(
            (m(t).x_write.unwrap())(t, data.as_ptr() as *const c_void, 10, 0),
            SQLITE_OK
        );
        assert_eq!((m(t).x_close.unwrap())(t), SQLITE_OK);

        let mut now = 0i64;
        assert_eq!(((*v).x_current_time_int64.unwrap())(v, &mut now), SQLITE_OK);
        assert!(now > 210_866_760_000_000 + 1_600_000_000_000);
    }
}

/// Run SQL in the system `sqlite3` shell (real SQLite); None if absent.
fn sqlite_shell(path: &str, sql: &str) -> Option<String> {
    let out = std::process::Command::new("sqlite3")
        .args(["-batch", "-cmd", ".timeout 5000", path, sql])
        .output()
        .ok()?;
    assert!(out.status.success(), "sqlite3 failed on: {sql}");
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[test]
fn shim_vfs_shares_a_wal_database_with_other_processes() {
    let _g = lock();
    unsafe {
        let name = c"compat-shim-mp";
        let vfs = shim_vfs(name);
        assert_eq!(sqlite3_vfs_register(vfs, 0), SQLITE_OK);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shim-mp.db");
        let p = path.to_str().unwrap();
        let (rc, db) = open_v2(p, OPEN_READWRITE | OPEN_CREATE, Some(name));
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(rows(db, "PRAGMA journal_mode=WAL"), "wal");
        exec(db, "CREATE TABLE t(x); INSERT INTO t VALUES (1)");
        // Another process writes while this connection is open: the
        // database is shared (no lifetime EXCLUSIVE lock), through the
        // VFS's locks and shared-memory wal-index.
        let Some(_) = sqlite_shell(p, "INSERT INTO t VALUES (2); INSERT INTO t VALUES (3);") else {
            eprintln!("sqlite3 shell not available, skipping");
            assert_eq!(sqlite3_close(db), SQLITE_OK);
            sqlite3_vfs_unregister(vfs);
            return;
        };
        assert_eq!(rows(db, "SELECT count(*), sum(x) FROM t"), "3|6");
        exec(db, "INSERT INTO t VALUES (4)");
        assert_eq!(
            sqlite_shell(p, "SELECT count(*), sum(x) FROM t; PRAGMA integrity_check").unwrap(),
            "4|10\nok"
        );
        assert!(Counts::get(&SHIM.shm_map) > 0, "xShmMap through the VFS");
        assert!(Counts::get(&SHIM.shm_lock) > 0, "xShmLock through the VFS");
        assert_eq!(sqlite3_close(db), SQLITE_OK);
        assert_eq!(sqlite3_vfs_unregister(vfs), SQLITE_OK);
    }
}
