//! Global configuration, run-time control and VFS entry points of the
//! SQLite C API (sqlite3_config, sqlite3_file_control, sqlite3_status, ...).
//!
//! Turso has no pluggable memory allocator, mutex layer, page cache or VFS
//! layer, so most of these are either accepted as harmless no-ops or refused
//! with the error code real SQLite uses for an unsupported feature.

use super::*;
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;

// ---------------------------------------------------------------------------
// sqlite3_temp_directory
// ---------------------------------------------------------------------------

/// Directory for temporary files; NULL means "use the system default".
/// Applications (e.g. NSS) assign to it directly. It is consulted whenever
/// turso creates a temporary database file.
#[no_mangle]
#[allow(non_upper_case_globals)]
pub static mut sqlite3_temp_directory: *mut ffi::c_char = std::ptr::null_mut();

/// Current value of `sqlite3_temp_directory`, if set and non-empty.
pub(crate) fn temp_directory_override() -> Option<std::path::PathBuf> {
    // SAFETY: plain read of a C-visible global; as in SQLite it must not be
    // changed concurrently with use.
    let p = unsafe { std::ptr::addr_of!(sqlite3_temp_directory).read() };
    if p.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(p) }.to_str().ok()?;
    if s.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(s))
    }
}

// ---------------------------------------------------------------------------
// sqlite3_config
// ---------------------------------------------------------------------------

const SQLITE_CONFIG_SINGLETHREAD: ffi::c_int = 1;
const SQLITE_CONFIG_MULTITHREAD: ffi::c_int = 2;
const SQLITE_CONFIG_SERIALIZED: ffi::c_int = 3;
const SQLITE_CONFIG_MEMSTATUS: ffi::c_int = 9;
const SQLITE_CONFIG_LOOKASIDE: ffi::c_int = 13;
const SQLITE_CONFIG_LOG: ffi::c_int = 16;
const SQLITE_CONFIG_URI: ffi::c_int = 17;
const SQLITE_CONFIG_COVERING_INDEX_SCAN: ffi::c_int = 20;
const SQLITE_CONFIG_MMAP_SIZE: ffi::c_int = 22;
const SQLITE_CONFIG_PMASZ: ffi::c_int = 25;
const SQLITE_CONFIG_STMTJRNL_SPILL: ffi::c_int = 26;
const SQLITE_CONFIG_SMALL_MALLOC: ffi::c_int = 27;
const SQLITE_CONFIG_SORTERREF_SIZE: ffi::c_int = 28;
const SQLITE_CONFIG_MEMDB_MAXSIZE: ffi::c_int = 29;

/// SQLITE_CONFIG_URI: treat "file:" filenames as URIs in sqlite3_open().
static URI_DEFAULT: AtomicBool = AtomicBool::new(false);

/// SQLITE_CONFIG_LOG: remembered (callback, context) pair. Turso has no
/// sqlite3_log() call sites, so the callback is stored but never invoked.
static LOG_CALLBACK: Mutex<(usize, usize)> = Mutex::new((0, 0));

/// True when SQLITE_CONFIG_URI(1) was requested.
pub(crate) fn uri_default() -> bool {
    URI_DEFAULT.load(Ordering::Relaxed)
}

extern "C" {
    /// Variadic decoder in varargs.c; see sqlite3_config below.
    fn turso_sqlite3_config_va();
}

/// Exported trampoline for the variadic sqlite3_config; same mechanism as
/// sqlite3_db_config (the variadic frame is handed untouched to varargs.c).
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_config(_op: ffi::c_int) -> ffi::c_int {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    core::arch::naked_asm!("jmp {}", sym turso_sqlite3_config_va);
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    core::arch::naked_asm!("b {}", sym turso_sqlite3_config_va);
}

/// Backend for sqlite3_config. `i1`/`i2` carry integer arguments and `p1`/`p2`
/// pointer arguments, decoded by varargs.c only for the ops whose argument
/// shape is known; every other op arrives with all four zeroed.
///
/// As in SQLite, configuration is refused with SQLITE_MISUSE once the library
/// has been initialized (turso initializes on first sqlite3_open*).
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn turso_sqlite3_config_apply(
    op: ffi::c_int,
    i1: i64,
    _i2: i64,
    p1: *mut ffi::c_void,
    p2: *mut ffi::c_void,
) -> ffi::c_int {
    if INIT_DONE.is_completed() {
        return SQLITE_MISUSE;
    }
    match op {
        // Turso is always safe to use from multiple threads (each connection
        // is internally serialized), so every threading mode is satisfied.
        SQLITE_CONFIG_SINGLETHREAD | SQLITE_CONFIG_MULTITHREAD | SQLITE_CONFIG_SERIALIZED => {
            SQLITE_OK
        }
        SQLITE_CONFIG_URI => {
            URI_DEFAULT.store(i1 != 0, Ordering::Relaxed);
            SQLITE_OK
        }
        SQLITE_CONFIG_LOG => {
            if let Ok(mut g) = LOG_CALLBACK.lock() {
                *g = (p1 as usize, p2 as usize);
            }
            SQLITE_OK
        }
        // Tuning knobs for subsystems turso does not have (or manages
        // itself): accepted and ignored, which only affects performance.
        SQLITE_CONFIG_MEMSTATUS
        | SQLITE_CONFIG_LOOKASIDE
        | SQLITE_CONFIG_COVERING_INDEX_SCAN
        | SQLITE_CONFIG_MMAP_SIZE
        | SQLITE_CONFIG_PMASZ
        | SQLITE_CONFIG_STMTJRNL_SPILL
        | SQLITE_CONFIG_SMALL_MALLOC
        | SQLITE_CONFIG_SORTERREF_SIZE
        | SQLITE_CONFIG_MEMDB_MAXSIZE => SQLITE_OK,
        // MALLOC, GETMALLOC, SCRATCH, PAGECACHE, HEAP, MUTEX, GETMUTEX,
        // PCACHE*, SQLLOG, WIN32_HEAPSIZE, ... : turso has no replaceable
        // allocator, mutex or page-cache layer.
        _ => SQLITE_ERROR,
    }
}

// ---------------------------------------------------------------------------
// sqlite3_test_control
// ---------------------------------------------------------------------------

/// sqlite3_test_control(int op, ...). None of the test hooks exist in turso;
/// SQLite itself returns 0 for ops it does not act on. The variadic
/// arguments are never read, which is valid for a callee on every ABI, so a
/// plain fixed-signature function suffices here.
/// Variadic in SQLite; the trampoline tail-jumps to varargs.c.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_test_control(_op: ffi::c_int) -> ffi::c_int {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    core::arch::naked_asm!("jmp {}", sym turso_sqlite3_test_control_va);
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    core::arch::naked_asm!("b {}", sym turso_sqlite3_test_control_va);
}

extern "C" {
    fn turso_sqlite3_test_control_va(op: ffi::c_int, ...) -> ffi::c_int;
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn turso_test_control_impl(_op: ffi::c_int) -> ffi::c_int {
    0
}

// ---------------------------------------------------------------------------
// sqlite3_status / sqlite3_status64 / sqlite3_db_status
// ---------------------------------------------------------------------------

const SQLITE_STATUS_MAX: ffi::c_int = 9;
const SQLITE_DBSTATUS_MAX: ffi::c_int = 13;

/// Turso does not track SQLite's allocator counters, so every global status
/// parameter reads as zero.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_status64(
    op: ffi::c_int,
    p_current: *mut i64,
    p_highwater: *mut i64,
    _reset_flag: ffi::c_int,
) -> ffi::c_int {
    if !(0..=SQLITE_STATUS_MAX).contains(&op) || p_current.is_null() || p_highwater.is_null() {
        return SQLITE_MISUSE;
    }
    *p_current = 0;
    *p_highwater = 0;
    SQLITE_OK
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_status(
    op: ffi::c_int,
    p_current: *mut ffi::c_int,
    p_highwater: *mut ffi::c_int,
    _reset_flag: ffi::c_int,
) -> ffi::c_int {
    if !(0..=SQLITE_STATUS_MAX).contains(&op) || p_current.is_null() || p_highwater.is_null() {
        return SQLITE_MISUSE;
    }
    *p_current = 0;
    *p_highwater = 0;
    SQLITE_OK
}

/// Per-connection status counters (lookaside, cache, schema memory, ...) are
/// not tracked; valid ops read as zero.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_status(
    db: *mut sqlite3,
    op: ffi::c_int,
    p_current: *mut ffi::c_int,
    p_highwater: *mut ffi::c_int,
    _reset_flag: ffi::c_int,
) -> ffi::c_int {
    if db.is_null() || p_current.is_null() || p_highwater.is_null() {
        return SQLITE_MISUSE;
    }
    if !(0..=SQLITE_DBSTATUS_MAX).contains(&op) {
        return SQLITE_ERROR;
    }
    *p_current = 0;
    *p_highwater = 0;
    SQLITE_OK
}

// ---------------------------------------------------------------------------
// sqlite3_db_mutex / sqlite3_unlock_notify
// ---------------------------------------------------------------------------

/// SQLite returns NULL when the library is not threadsafe (no connection
/// mutex). Turso serializes each connection internally but exposes no mutex
/// object, so NULL is the honest answer. Botan's `is_threadsafe()` is
/// `sqlite3_db_mutex(db) != NULL`; with NULL its TLS session manager adds its
/// own lock around the database, which is the safe outcome.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_mutex(_db: *mut sqlite3) -> *mut ffi::c_void {
    std::ptr::null_mut()
}

/// Shared-cache unlock notification. Turso has no shared-cache table locks, so
/// a connection can never be blocked by another one; SQLite specifies that
/// the callback is then invoked immediately, and returns SQLITE_OK.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_unlock_notify(
    db: *mut sqlite3,
    x_notify: Option<unsafe extern "C" fn(*mut *mut ffi::c_void, ffi::c_int)>,
    p_arg: *mut ffi::c_void,
) -> ffi::c_int {
    if db.is_null() {
        return SQLITE_MISUSE;
    }
    if let Some(cb) = x_notify {
        let mut args = [p_arg];
        cb(args.as_mut_ptr(), 1);
    }
    SQLITE_OK
}

// ---------------------------------------------------------------------------
// sqlite3_file_control
// ---------------------------------------------------------------------------

const SQLITE_FCNTL_LAST_ERRNO: ffi::c_int = 4;
const SQLITE_FCNTL_SIZE_HINT: ffi::c_int = 5;
const SQLITE_FCNTL_CHUNK_SIZE: ffi::c_int = 6;
const SQLITE_FCNTL_PERSIST_WAL: ffi::c_int = 10;
const SQLITE_FCNTL_VFSNAME: ffi::c_int = 12;
const SQLITE_FCNTL_POWERSAFE_OVERWRITE: ffi::c_int = 13;
const SQLITE_FCNTL_TEMPFILENAME: ffi::c_int = 16;
const SQLITE_FCNTL_MMAP_SIZE: ffi::c_int = 18;
const SQLITE_FCNTL_HAS_MOVED: ffi::c_int = 20;
const SQLITE_FCNTL_DATA_VERSION: ffi::c_int = 35;
const SQLITE_FCNTL_RESERVE_BYTES: ffi::c_int = 38;
const SQLITE_FCNTL_RESET_CACHE: ffi::c_int = 42;
const SQLITE_FCNTL_VFS_POINTER: ffi::c_int = 27;

/// Values set through SQLITE_FCNTL_PERSIST_WAL / POWERSAFE_OVERWRITE, keyed by
/// connection address. They are remembered so that a later query returns what
/// was set (callers such as GDAL save and restore the setting), but turso does
/// not act on PERSIST_WAL: its WAL is checkpointed and removed on last close.
static FCNTL_PERSIST_WAL: Mutex<Option<HashMap<usize, ffi::c_int>>> = Mutex::new(None);
static FCNTL_PSOW: Mutex<Option<HashMap<usize, ffi::c_int>>> = Mutex::new(None);

/// Called from sqlite3_close so a recycled connection address does not
/// inherit the previous owner's remembered settings.
pub(crate) fn forget_connection(db: *mut sqlite3) {
    for m in [&FCNTL_PERSIST_WAL, &FCNTL_PSOW] {
        if let Ok(mut g) = m.lock() {
            if let Some(map) = g.as_mut() {
                map.remove(&(db as usize));
            }
        }
    }
    super::extension::forget_connection(db);
    if let Ok(mut g) = DBCONFIG_STORE.lock() {
        if let Some(map) = g.as_mut() {
            map.retain(|(k, _), _| *k != db as usize);
        }
    }
}

/// get/set semantics shared by PERSIST_WAL and POWERSAFE_OVERWRITE: a negative
/// *arg queries; otherwise the value is stored (as a boolean) and *arg is set
/// to the new value.
unsafe fn fcntl_bool_setting(
    store: &Mutex<Option<HashMap<usize, ffi::c_int>>>,
    key: usize,
    default: ffi::c_int,
    arg: *mut ffi::c_int,
) {
    let mut g = match store.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let map = g.get_or_insert_with(HashMap::new);
    if *arg < 0 {
        *arg = *map.get(&key).unwrap_or(&default);
    } else {
        let v = (*arg != 0) as ffi::c_int;
        map.insert(key, v);
        *arg = v;
    }
}

unsafe fn malloc_cstring(s: &str) -> *mut ffi::c_char {
    let p = sqlite3_malloc((s.len() + 1) as ffi::c_int) as *mut u8;
    if p.is_null() {
        return std::ptr::null_mut();
    }
    std::ptr::copy_nonoverlapping(s.as_ptr(), p, s.len());
    *p.add(s.len()) = 0;
    p as *mut ffi::c_char
}

/// Run `PRAGMA <sql>` on the connection and return the first integer cell.
fn pragma_int(db: &sqlite3, pragma: &str) -> Option<i64> {
    let conn = db.inner.lock().ok()?.conn.clone();
    let rows = conn.pragma_query(pragma).ok()?;
    match rows.first()?.first()? {
        Value::Numeric(turso_core::Numeric::Integer(i)) => Some(*i),
        _ => None,
    }
}

/// Subset of SQLite's file-control opcodes that turso can honour or answer
/// truthfully; every other opcode (including the ones that hand out
/// `sqlite3_file*`/`sqlite3_vfs*` pointers, which do not exist here) returns
/// SQLITE_NOTFOUND, which is what SQLite returns for opcodes the VFS does not
/// understand.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_file_control(
    db: *mut sqlite3,
    z_db_name: *const ffi::c_char,
    op: ffi::c_int,
    p_arg: *mut ffi::c_void,
) -> ffi::c_int {
    if db.is_null() {
        return SQLITE_MISUSE;
    }
    if !z_db_name.is_null() {
        match CStr::from_ptr(z_db_name).to_bytes() {
            b"main" | b"temp" => {}
            _ => return SQLITE_ERROR,
        }
    }
    // Through an application VFS, SQLite hands the opcode to the file.
    if let Some(rc) = super::vfs::file_control(db, op, p_arg) {
        return rc;
    }
    if op == SQLITE_FCNTL_VFS_POINTER && !p_arg.is_null() {
        *(p_arg as *mut *mut sqlite3_vfs) = super::vfs_unix::unix_vfs();
        return SQLITE_OK;
    }
    // A plain in-memory database has no file to control.
    {
        let inner = (*db).inner.lock().unwrap();
        if inner.filename.as_bytes().is_empty() && inner.temp_path.is_none() {
            return SQLITE_NOTFOUND;
        }
    }
    let key = db as usize;
    let ip = p_arg as *mut ffi::c_int;
    let need_arg = |p: *mut ffi::c_void| -> bool { !p.is_null() };
    match op {
        SQLITE_FCNTL_LAST_ERRNO if need_arg(p_arg) => {
            *ip = 0;
            SQLITE_OK
        }
        // Advisory hints: accepted, no effect.
        SQLITE_FCNTL_SIZE_HINT | SQLITE_FCNTL_CHUNK_SIZE | SQLITE_FCNTL_RESET_CACHE => SQLITE_OK,
        SQLITE_FCNTL_PERSIST_WAL if need_arg(p_arg) => {
            fcntl_bool_setting(&FCNTL_PERSIST_WAL, key, 0, ip);
            SQLITE_OK
        }
        SQLITE_FCNTL_POWERSAFE_OVERWRITE if need_arg(p_arg) => {
            fcntl_bool_setting(&FCNTL_PSOW, key, 1, ip);
            SQLITE_OK
        }
        SQLITE_FCNTL_VFSNAME if need_arg(p_arg) => {
            *(p_arg as *mut *mut ffi::c_char) = malloc_cstring("unix");
            SQLITE_OK
        }
        SQLITE_FCNTL_TEMPFILENAME if need_arg(p_arg) => {
            let name = temp_database_path_in_dir();
            *(p_arg as *mut *mut ffi::c_char) = malloc_cstring(&name);
            SQLITE_OK
        }
        // Memory mapping is not used; report a zero limit when queried.
        SQLITE_FCNTL_MMAP_SIZE if need_arg(p_arg) => {
            let ip64 = p_arg as *mut i64;
            *ip64 = 0;
            SQLITE_OK
        }
        SQLITE_FCNTL_HAS_MOVED if need_arg(p_arg) => {
            *ip = 0;
            SQLITE_OK
        }
        SQLITE_FCNTL_RESERVE_BYTES if need_arg(p_arg) => {
            // Reserved bytes per page are not supported: always 0.
            let requested = *ip;
            *ip = 0;
            if requested > 0 {
                SQLITE_NOTFOUND
            } else {
                SQLITE_OK
            }
        }
        SQLITE_FCNTL_DATA_VERSION if need_arg(p_arg) => match pragma_int(&*db, "data_version") {
            Some(v) => {
                *(p_arg as *mut u32) = v as u32;
                SQLITE_OK
            }
            None => SQLITE_NOTFOUND,
        },
        _ => SQLITE_NOTFOUND,
    }
}

fn temp_database_path_in_dir() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = temp_directory_override().unwrap_or_else(std::env::temp_dir);
    dir.join(format!("etilqs_{:x}{:x}", std::process::id(), n))
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------------------
// VFS registry
// ---------------------------------------------------------------------------

pub use super::vfs::sqlite3_vfs;

/// Registered application VFSes, in registration order. Stored as
/// addresses; the caller owns the structs and unregisters them before freeing,
/// as the SQLite contract requires.
static VFS_REGISTRY: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Address of the VFS registered with makeDflt (0 = built-in "unix").
static VFS_DEFAULT: AtomicUsize = AtomicUsize::new(0);

unsafe fn vfs_name_of(p: usize) -> Option<&'static [u8]> {
    let v = p as *const sqlite3_vfs;
    if (*v).z_name.is_null() {
        None
    } else {
        Some(CStr::from_ptr((*v).z_name).to_bytes())
    }
}

/// Looks up `zVfsName` ("unix", the default, and its "unix-*" aliases map to
/// the built-in VFS; otherwise the registry is searched). NULL selects the
/// default VFS: the one registered with makeDflt, else "unix".
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_vfs_find(z_vfs_name: *const ffi::c_char) -> *mut sqlite3_vfs {
    let builtin = super::vfs_unix::unix_vfs();
    let reg = match VFS_REGISTRY.lock() {
        Ok(g) => g,
        Err(_) => return std::ptr::null_mut(),
    };
    if z_vfs_name.is_null() {
        let d = VFS_DEFAULT.load(Ordering::Relaxed);
        return if d != 0 {
            d as *mut sqlite3_vfs
        } else {
            builtin
        };
    }
    let want = CStr::from_ptr(z_vfs_name).to_bytes();
    for &p in reg.iter() {
        if vfs_name_of(p) == Some(want) {
            return p as *mut sqlite3_vfs;
        }
    }
    if is_builtin_vfs_name(want) {
        builtin
    } else {
        std::ptr::null_mut()
    }
}

fn is_builtin_vfs_name(n: &[u8]) -> bool {
    matches!(
        n,
        b"unix" | b"unix-none" | b"unix-excl" | b"unix-dotfile" | b"unix-namedsem"
    )
}

/// Registers an application VFS. Connections opened through it (zVfs,
/// `vfs=` URI parameter, or makeDflt) do their file I/O through its methods;
/// ATTACH finds it by name through turso_core's IO registry.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_vfs_register(
    p_vfs: *mut sqlite3_vfs,
    make_dflt: ffi::c_int,
) -> ffi::c_int {
    if p_vfs.is_null() || (*p_vfs).z_name.is_null() {
        return SQLITE_MISUSE;
    }
    let mut reg = match VFS_REGISTRY.lock() {
        Ok(g) => g,
        Err(_) => return SQLITE_MISUSE,
    };
    reg.retain(|&p| p != p_vfs as usize);
    reg.push(p_vfs as usize);
    if make_dflt != 0 {
        VFS_DEFAULT.store(p_vfs as usize, Ordering::Relaxed);
    }
    if p_vfs != super::vfs_unix::unix_vfs() {
        if let Ok(name) = CStr::from_ptr((*p_vfs).z_name).to_str() {
            let _ = turso_core::register_io(name, super::vfs::VfsIo::for_vfs(p_vfs));
        }
    }
    SQLITE_OK
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_vfs_unregister(p_vfs: *mut sqlite3_vfs) -> ffi::c_int {
    if p_vfs.is_null() {
        return SQLITE_OK;
    }
    if let Ok(mut reg) = VFS_REGISTRY.lock() {
        let was_registered = reg.contains(&(p_vfs as usize));
        reg.retain(|&p| p != p_vfs as usize);
        let _ =
            VFS_DEFAULT.compare_exchange(p_vfs as usize, 0, Ordering::Relaxed, Ordering::Relaxed);
        if was_registered && !(*p_vfs).z_name.is_null() {
            if let Ok(name) = CStr::from_ptr((*p_vfs).z_name).to_str() {
                // Another registered VFS of the same name takes over.
                match reg
                    .iter()
                    .rev()
                    .find(|&&p| vfs_name_of(p) == Some(name.as_bytes()))
                {
                    Some(&p) => {
                        let _ = turso_core::register_io(
                            name,
                            super::vfs::VfsIo::for_vfs(p as *mut sqlite3_vfs),
                        );
                    }
                    None => {
                        turso_core::unregister_io(name);
                    }
                }
            }
        }
    }
    SQLITE_OK
}

/// Gate for the `zVfs` argument of sqlite3_open_v2: NULL, "", a built-in
/// name or a registered VFS proceed; any other name is SQLITE_ERROR ("no such
/// vfs"), as in SQLite. Routing happens in [`super::vfs::open_v2_via_vfs`].
pub(crate) unsafe fn check_open_vfs(z_vfs: *const ffi::c_char, _flags: ffi::c_int) -> ffi::c_int {
    if z_vfs.is_null() || *z_vfs == 0 || !sqlite3_vfs_find(z_vfs).is_null() {
        SQLITE_OK
    } else {
        SQLITE_ERROR
    }
}

// ---------------------------------------------------------------------------
// sqlite3_db_config boolean ops (1002..1017, except DEFENSIVE)
// ---------------------------------------------------------------------------

/// Per-connection values of the accepted-but-inert db_config switches, keyed
/// by (connection address, op).
static DBCONFIG_STORE: Mutex<Option<HashMap<(usize, ffi::c_int), ffi::c_int>>> = Mutex::new(None);

/// How turso treats one boolean db_config op.
enum DbConfigKind {
    /// Backed by real connection state.
    Live,
    /// Turso's behaviour is fixed at this value. Querying, or "setting" it to
    /// the same value, succeeds; asking for the opposite would silently
    /// change semantics, so it is refused with SQLITE_ERROR.
    Fixed(ffi::c_int),
    /// Only a hint or a file-format preference that does not alter query
    /// semantics: accepted and remembered per connection (default given).
    Remembered(ffi::c_int),
}

fn dbconfig_kind(op: ffi::c_int) -> Option<DbConfigKind> {
    use DbConfigKind::*;
    Some(match op {
        // foreign_keys / dqs_dml are real connection flags in turso_core.
        SQLITE_DBCONFIG_ENABLE_FKEY
        | SQLITE_DBCONFIG_DQS_DML
        | SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION => Live,
        // Triggers and views are always enabled.
        SQLITE_DBCONFIG_ENABLE_TRIGGER | SQLITE_DBCONFIG_ENABLE_VIEW => Fixed(1),
        // No fts3_tokenizer(), always checkpoints on close, no reset via
        // VACUUM, no legacy ALTER TABLE behaviour.
        SQLITE_DBCONFIG_ENABLE_FTS3_TOKENIZER
        | SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE
        | SQLITE_DBCONFIG_RESET_DATABASE
        | SQLITE_DBCONFIG_LEGACY_ALTER_TABLE => Fixed(0),
        // Double-quoted strings in DDL always work; no untrusted-schema mode.
        SQLITE_DBCONFIG_DQS_DDL | SQLITE_DBCONFIG_TRUSTED_SCHEMA => Fixed(1),
        // Planner-stability and EXPLAIN-display hints, and the format used
        // for newly created files: no effect on results.
        SQLITE_DBCONFIG_ENABLE_QPSG
        | SQLITE_DBCONFIG_TRIGGER_EQP
        | SQLITE_DBCONFIG_LEGACY_FILE_FORMAT => Remembered(0),
        _ => return None,
    })
}

/// Backend for the (int value, int *pOut) db_config ops. A negative `value`
/// queries without changing anything; `*p_out` (when non-NULL) receives the
/// resulting state. SQLITE_DBCONFIG_DEFENSIVE is handled by the caller.
pub(crate) unsafe fn db_config_bool(
    db: *mut sqlite3,
    op: ffi::c_int,
    value: ffi::c_int,
    p_out: *mut ffi::c_int,
) -> ffi::c_int {
    let Some(kind) = dbconfig_kind(op) else {
        return SQLITE_ERROR;
    };
    let conn = match (*db).inner.lock() {
        Ok(g) => g.conn.clone(),
        Err(_) => return SQLITE_MISUSE,
    };
    let query = value < 0;
    let want = (value != 0) as ffi::c_int;
    let result = match kind {
        DbConfigKind::Live => {
            if !query {
                match op {
                    SQLITE_DBCONFIG_ENABLE_FKEY => conn.set_foreign_keys_enabled(want != 0),
                    SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION => {
                        super::extension::set_load_extension_enabled(db, want != 0)
                    }
                    _ => conn.set_dqs_dml(want != 0),
                }
            }
            match op {
                SQLITE_DBCONFIG_ENABLE_FKEY => conn.foreign_keys_enabled() as ffi::c_int,
                SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION => {
                    super::extension::load_extension_enabled(db) as ffi::c_int
                }
                _ => conn.get_dqs_dml() as ffi::c_int,
            }
        }
        DbConfigKind::Fixed(v) => {
            if !query && want != v {
                return SQLITE_ERROR;
            }
            v
        }
        DbConfigKind::Remembered(default) => {
            let mut g = match DBCONFIG_STORE.lock() {
                Ok(g) => g,
                Err(_) => return SQLITE_MISUSE,
            };
            let map = g.get_or_insert_with(HashMap::new);
            if !query {
                map.insert((db as usize, op), want);
            }
            *map.get(&(db as usize, op)).unwrap_or(&default)
        }
    };
    if !p_out.is_null() {
        *p_out = result;
    }
    SQLITE_OK
}
