//! Application-registered SQLite VFSes.
//!
//! A connection opened through a non-default VFS does all of its file I/O
//! through the application's `sqlite3_vfs` / `sqlite3_io_methods`: [`VfsIo`]
//! implements turso_core's `IO` on top of a C VFS and [`VfsFile`] implements
//! `File` on top of a `sqlite3_file`.
//!
//! Multi-process access goes through the VFS too: the database-file locks
//! the SQLite interlock holds use xLock/xUnlock, and SQLite's wal-index
//! (`-shm`) uses xShmMap/xShmLock/xShmBarrier/xShmUnmap of the main
//! database file ([`VfsShmFile`]). Turso's private coordination file
//! (`-tshm`) needs host shared memory, so it is only available when the
//! database is a host file. Without multiprocess WAL (no shm methods, or not
//! a host file) a read-write connection holds the database EXCLUSIVE.

use super::*;
use std::collections::HashMap as Map;
use std::sync::Weak;
use turso_core::io::{FileId, FileSyncType, SharedWalLockKind, SharedWalMappedRegion};
use turso_core::{Buffer, Completion, CompletionError, File, OpenFlags, IO};

pub(crate) type VfsFn = Option<unsafe extern "C" fn()>;

/// Layout of `struct sqlite3_vfs` (iVersion 3).
#[repr(C)]
pub struct sqlite3_vfs {
    pub(crate) i_version: ffi::c_int,
    pub(crate) sz_os_file: ffi::c_int,
    pub(crate) mx_pathname: ffi::c_int,
    pub(crate) p_next: *mut sqlite3_vfs,
    pub(crate) z_name: *const ffi::c_char,
    pub(crate) p_app_data: *mut ffi::c_void,
    pub(crate) x_open: Option<
        unsafe extern "C" fn(
            *mut sqlite3_vfs,
            *const ffi::c_char,
            *mut sqlite3_file,
            ffi::c_int,
            *mut ffi::c_int,
        ) -> ffi::c_int,
    >,
    pub(crate) x_delete: Option<
        unsafe extern "C" fn(*mut sqlite3_vfs, *const ffi::c_char, ffi::c_int) -> ffi::c_int,
    >,
    pub(crate) x_access: Option<
        unsafe extern "C" fn(
            *mut sqlite3_vfs,
            *const ffi::c_char,
            ffi::c_int,
            *mut ffi::c_int,
        ) -> ffi::c_int,
    >,
    pub(crate) x_full_pathname: Option<
        unsafe extern "C" fn(
            *mut sqlite3_vfs,
            *const ffi::c_char,
            ffi::c_int,
            *mut ffi::c_char,
        ) -> ffi::c_int,
    >,
    pub(crate) x_dl_open:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const ffi::c_char) -> *mut ffi::c_void>,
    pub(crate) x_dl_error:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, ffi::c_int, *mut ffi::c_char)>,
    pub(crate) x_dl_sym: Option<
        unsafe extern "C" fn(*mut sqlite3_vfs, *mut ffi::c_void, *const ffi::c_char) -> VfsFn,
    >,
    pub(crate) x_dl_close: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *mut ffi::c_void)>,
    pub(crate) x_randomness:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, ffi::c_int, *mut ffi::c_char) -> ffi::c_int>,
    pub(crate) x_sleep: Option<unsafe extern "C" fn(*mut sqlite3_vfs, ffi::c_int) -> ffi::c_int>,
    pub(crate) x_current_time:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, *mut f64) -> ffi::c_int>,
    pub(crate) x_get_last_error:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, ffi::c_int, *mut ffi::c_char) -> ffi::c_int>,
    // iVersion 2
    pub(crate) x_current_time_int64:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, *mut i64) -> ffi::c_int>,
    // iVersion 3
    pub(crate) x_set_system_call:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const ffi::c_char, VfsFn) -> ffi::c_int>,
    pub(crate) x_get_system_call:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const ffi::c_char) -> VfsFn>,
    pub(crate) x_next_system_call:
        Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const ffi::c_char) -> *const ffi::c_char>,
}

/// Layout of `struct sqlite3_file`.
#[repr(C)]
pub struct sqlite3_file {
    pub(crate) p_methods: *const sqlite3_io_methods,
}

/// Layout of `struct sqlite3_io_methods` (iVersion 3).
#[repr(C)]
pub struct sqlite3_io_methods {
    pub(crate) i_version: ffi::c_int,
    pub(crate) x_close: Option<unsafe extern "C" fn(*mut sqlite3_file) -> ffi::c_int>,
    pub(crate) x_read: Option<
        unsafe extern "C" fn(*mut sqlite3_file, *mut ffi::c_void, ffi::c_int, i64) -> ffi::c_int,
    >,
    pub(crate) x_write: Option<
        unsafe extern "C" fn(*mut sqlite3_file, *const ffi::c_void, ffi::c_int, i64) -> ffi::c_int,
    >,
    pub(crate) x_truncate: Option<unsafe extern "C" fn(*mut sqlite3_file, i64) -> ffi::c_int>,
    pub(crate) x_sync: Option<unsafe extern "C" fn(*mut sqlite3_file, ffi::c_int) -> ffi::c_int>,
    pub(crate) x_file_size: Option<unsafe extern "C" fn(*mut sqlite3_file, *mut i64) -> ffi::c_int>,
    pub(crate) x_lock: Option<unsafe extern "C" fn(*mut sqlite3_file, ffi::c_int) -> ffi::c_int>,
    pub(crate) x_unlock: Option<unsafe extern "C" fn(*mut sqlite3_file, ffi::c_int) -> ffi::c_int>,
    pub(crate) x_check_reserved_lock:
        Option<unsafe extern "C" fn(*mut sqlite3_file, *mut ffi::c_int) -> ffi::c_int>,
    pub(crate) x_file_control:
        Option<unsafe extern "C" fn(*mut sqlite3_file, ffi::c_int, *mut ffi::c_void) -> ffi::c_int>,
    pub(crate) x_sector_size: Option<unsafe extern "C" fn(*mut sqlite3_file) -> ffi::c_int>,
    pub(crate) x_device_characteristics:
        Option<unsafe extern "C" fn(*mut sqlite3_file) -> ffi::c_int>,
    // iVersion 2
    pub(crate) x_shm_map: Option<
        unsafe extern "C" fn(
            *mut sqlite3_file,
            ffi::c_int,
            ffi::c_int,
            ffi::c_int,
            *mut *mut ffi::c_void,
        ) -> ffi::c_int,
    >,
    pub(crate) x_shm_lock: Option<
        unsafe extern "C" fn(*mut sqlite3_file, ffi::c_int, ffi::c_int, ffi::c_int) -> ffi::c_int,
    >,
    pub(crate) x_shm_barrier: Option<unsafe extern "C" fn(*mut sqlite3_file)>,
    pub(crate) x_shm_unmap:
        Option<unsafe extern "C" fn(*mut sqlite3_file, ffi::c_int) -> ffi::c_int>,
    // iVersion 3
    pub(crate) x_fetch: Option<
        unsafe extern "C" fn(
            *mut sqlite3_file,
            i64,
            ffi::c_int,
            *mut *mut ffi::c_void,
        ) -> ffi::c_int,
    >,
    pub(crate) x_unfetch:
        Option<unsafe extern "C" fn(*mut sqlite3_file, i64, *mut ffi::c_void) -> ffi::c_int>,
}

const SQLITE_OPEN_READONLY: ffi::c_int = 0x1;
const SQLITE_OPEN_READWRITE: ffi::c_int = 0x2;
const SQLITE_OPEN_CREATE: ffi::c_int = 0x4;
const SQLITE_OPEN_DELETEONCLOSE: ffi::c_int = 0x8;
const SQLITE_OPEN_EXCLUSIVE: ffi::c_int = 0x10;
const SQLITE_OPEN_URI_FLAG: ffi::c_int = 0x40;
const SQLITE_OPEN_MAIN_DB: ffi::c_int = 0x100;
const SQLITE_OPEN_TEMP_DB: ffi::c_int = 0x200;
const SQLITE_OPEN_WAL: ffi::c_int = 0x80000;

const SQLITE_ACCESS_EXISTS: ffi::c_int = 0;

const SQLITE_LOCK_NONE: ffi::c_int = 0;
const SQLITE_LOCK_SHARED: ffi::c_int = 1;
const SQLITE_LOCK_RESERVED: ffi::c_int = 2;
const SQLITE_LOCK_EXCLUSIVE: ffi::c_int = 4;

const SQLITE_SHM_UNLOCK: ffi::c_int = 1;
const SQLITE_SHM_LOCK: ffi::c_int = 2;
const SQLITE_SHM_SHARED: ffi::c_int = 4;
const SQLITE_SHM_EXCLUSIVE: ffi::c_int = 8;
/// Byte offsets the core interlock and wal-index lock (see
/// turso_core's sqlite_shm_interlock / sqlite_wal_index).
const SHM_LOCK_BASE: u64 = 120;
const SHM_NLOCK: u64 = 8;
const SHM_DMS: u64 = 128;
const SHM_REGION: usize = 32768;
const DB_SHARED_FIRST: u64 = 0x4000_0000 + 2;

const SQLITE_SYNC_NORMAL: ffi::c_int = 2;
const SQLITE_SYNC_FULL: ffi::c_int = 3;

const SQLITE_IOERR_SHORT_READ: ffi::c_int = SQLITE_IOERR | (2 << 8);
const SQLITE_IOERR_DELETE_NOENT: ffi::c_int = SQLITE_IOERR | (23 << 8);

const SQLITE_FCNTL_FILE_POINTER: ffi::c_int = 7;
const SQLITE_FCNTL_VFS_POINTER: ffi::c_int = 27;
const SQLITE_FCNTL_JOURNAL_POINTER: ffi::c_int = 28;

/// Maps a VFS result code to a turso error.
fn vfs_err(rc: ffi::c_int, op: &'static str) -> LimboError {
    match rc & 0xff {
        SQLITE_BUSY | SQLITE_LOCKED => LimboError::Busy,
        SQLITE_FULL => LimboError::DatabaseFull,
        SQLITE_READONLY => LimboError::ReadOnly,
        SQLITE_CANTOPEN => {
            LimboError::CompletionError(CompletionError::IOError(std::io::ErrorKind::NotFound, op))
        }
        SQLITE_NOMEM => LimboError::InternalError("out of memory".into()),
        _ => LimboError::CompletionError(CompletionError::IOError(std::io::ErrorKind::Other, op)),
    }
}

/// A filename block and the offset of the WAL name inside it.
type NameBlock = (Arc<[u8]>, usize);

/// Owner of an application `sqlite3_vfs`. The application keeps the struct
/// alive while it is registered and while connections use it, as SQLite
/// requires.
pub(crate) struct VfsIo {
    vfs: *mut sqlite3_vfs,
    /// Open main-database files by path, for sqlite3_file_control.
    files: Mutex<Map<String, Weak<VfsFile>>>,
    /// SQLite-format filename blocks (name, URI parameters, journal and WAL
    /// names) of main databases, by path; xOpen names point into them.
    names: Mutex<Map<String, NameBlock>>,
    /// Host I/O for turso's `-tshm` coordination file.
    host: std::sync::OnceLock<Arc<dyn IO>>,
}

// The VFS methods are called with the per-file mutex held, as SQLite does.
unsafe impl Send for VfsIo {}
unsafe impl Sync for VfsIo {}

thread_local! {
    /// URI parameters for the main-database xOpen of the open in progress.
    static PENDING_PARAMS: std::cell::RefCell<Option<(String, Vec<u8>)>> =
        const { std::cell::RefCell::new(None) };
}

/// All live adapters, so a connection's `Arc<dyn IO>` can be mapped back.
static ADAPTERS: Mutex<Vec<Weak<VfsIo>>> = Mutex::new(Vec::new());

impl VfsIo {
    /// The adapter for `vfs`, shared by every connection that uses it.
    pub(crate) fn for_vfs(vfs: *mut sqlite3_vfs) -> Arc<VfsIo> {
        let mut all = ADAPTERS.lock().unwrap_or_else(|e| e.into_inner());
        all.retain(|w| w.strong_count() > 0);
        if let Some(io) = all
            .iter()
            .filter_map(Weak::upgrade)
            .find(|io| io.vfs == vfs)
        {
            return io;
        }
        let io = Arc::new(VfsIo {
            vfs,
            files: Mutex::new(Map::new()),
            names: Mutex::new(Map::new()),
            host: std::sync::OnceLock::new(),
        });
        all.push(Arc::downgrade(&io));
        io
    }

    /// The adapter behind a connection's IO object, if it is one.
    fn of_io(io: &Arc<dyn IO>) -> Option<Arc<VfsIo>> {
        let p = Arc::as_ptr(io) as *const u8;
        let all = ADAPTERS.lock().ok()?;
        all.iter()
            .filter_map(Weak::upgrade)
            .find(|a| Arc::as_ptr(a) as *const u8 == p)
    }

    pub(crate) fn vfs(&self) -> *mut sqlite3_vfs {
        self.vfs
    }

    unsafe fn full_pathname(&self, path: &str) -> turso_core::Result<String> {
        let v = &*self.vfs;
        let Some(f) = v.x_full_pathname else {
            return Ok(path.to_string());
        };
        let c = CString::new(path).map_err(|_| vfs_err(SQLITE_CANTOPEN, "full_pathname"))?;
        let n = (v.mx_pathname.max(0) + 1) as usize;
        let mut out = vec![0u8; n.max(2)];
        let rc = f(
            self.vfs,
            c.as_ptr(),
            n as ffi::c_int,
            out.as_mut_ptr() as *mut _,
        );
        if rc & 0xff != SQLITE_OK {
            return Err(vfs_err(rc, "full_pathname"));
        }
        let len = out.iter().position(|&b| b == 0).unwrap_or(out.len());
        Ok(String::from_utf8_lossy(&out[..len]).into_owned())
    }

    /// Absolute paths are used as given: the main database path already went
    /// through xFullPathname, and derived names (WAL) inherit it.
    unsafe fn resolve(&self, path: &str) -> turso_core::Result<String> {
        if path.starts_with('/') {
            Ok(path.to_string())
        } else {
            self.full_pathname(path)
        }
    }

    pub(crate) unsafe fn access(&self, path: &str, flags: ffi::c_int) -> turso_core::Result<bool> {
        let v = &*self.vfs;
        let Some(f) = v.x_access else {
            return Ok(false);
        };
        let c = CString::new(path).map_err(|_| vfs_err(SQLITE_CANTOPEN, "access"))?;
        let mut out = 0;
        let rc = f(self.vfs, c.as_ptr(), flags, &mut out);
        if rc != SQLITE_OK {
            return Err(vfs_err(rc, "access"));
        }
        Ok(out != 0)
    }

    /// Name block for a main database: 4 zero bytes, name, key/value pairs,
    /// an empty key, then the journal and WAL names (SQLite's layout, which
    /// sqlite3_uri_parameter and sqlite3_filename_wal walk).
    fn name_block(&self, path: &str, params: &[u8]) -> Arc<[u8]> {
        let mut b = vec![0u8; 4];
        b.extend_from_slice(path.as_bytes());
        b.push(0);
        b.extend_from_slice(params);
        b.push(0);
        b.extend_from_slice(path.as_bytes());
        b.extend_from_slice(b"-journal\0");
        let wal_off = b.len();
        b.extend_from_slice(path.as_bytes());
        b.extend_from_slice(b"-wal\0");
        b.extend_from_slice(&[0, 0, 0]);
        let block: Arc<[u8]> = b.into();
        if let Ok(mut m) = self.names.lock() {
            m.insert(path.to_string(), (block.clone(), wal_off));
        }
        block
    }

    /// Host I/O for turso's `-tshm` coordination file at `full`: shared host
    /// memory, so only next to a host database file.
    fn host_coordination_io(&self, full: &str) -> turso_core::Result<Arc<dyn IO>> {
        let db = &full[..full.len() - "-tshm".len()];
        if turso_core::io::get_file_id(db).is_err() {
            return Err(LimboError::InvalidArgument(format!(
                "experimental multiprocess WAL is not supported for '{db}': not a host file"
            )));
        }
        if let Some(h) = self.host.get() {
            return Ok(h.clone());
        }
        let io: Arc<dyn IO> = Arc::new(turso_core::UnixIO::new()?);
        Ok(self.host.get_or_init(|| io).clone())
    }

    /// Main-database file opened at `path` through this adapter.
    fn main_file(&self, path: &str) -> Option<Arc<VfsFile>> {
        self.files.lock().ok()?.get(path).and_then(Weak::upgrade)
    }
}

impl turso_core::Clock for VfsIo {
    fn current_time_monotonic(&self) -> turso_core::MonotonicInstant {
        turso_core::MonotonicInstant::now()
    }

    fn current_time_wall_clock(&self) -> turso_core::WallClockInstant {
        // xCurrentTimeInt64 (v2) or xCurrentTime, as SQLite's 'now'.
        unsafe {
            let v = &*self.vfs;
            let mut ms: Option<i64> = None;
            if v.i_version >= 2 {
                if let Some(f) = v.x_current_time_int64 {
                    let mut t = 0i64;
                    if f(self.vfs, &mut t) == SQLITE_OK {
                        ms = Some(t);
                    }
                }
            }
            if ms.is_none() {
                if let Some(f) = v.x_current_time {
                    let mut d = 0f64;
                    if f(self.vfs, &mut d) == SQLITE_OK {
                        ms = Some((d * 86_400_000.0) as i64);
                    }
                }
            }
            match ms {
                Some(jd_ms) => {
                    let unix_ms = jd_ms - 210_866_760_000_000;
                    turso_core::WallClockInstant {
                        secs: unix_ms.div_euclid(1000),
                        micros: (unix_ms.rem_euclid(1000) * 1000) as u32,
                    }
                }
                None => turso_core::WallClockInstant::now(),
            }
        }
    }
}

enum FileKind {
    Main,
    Wal,
    Temp,
}

impl IO for VfsIo {
    fn open_file(
        &self,
        path: &str,
        flags: OpenFlags,
        _direct: bool,
    ) -> turso_core::Result<Arc<dyn File>> {
        unsafe {
            // Turso's own temp files (sorters, ephemeral tables) are anonymous
            // temp files of the VFS, as in SQLite.
            if path.ends_with("-tshm") {
                let full = self.resolve(path)?;
                // No host file, no coordination file: report it missing.
                let Ok(host) = self.host_coordination_io(&full) else {
                    return Err(LimboError::CompletionError(CompletionError::IOError(
                        std::io::ErrorKind::NotFound,
                        "open",
                    )));
                };
                return host.open_file(&full, flags, false);
            }
            let kind = if path.ends_with("/tursodb_temp_file") {
                FileKind::Temp
            } else if path.ends_with("-wal") {
                FileKind::Wal
            } else {
                FileKind::Main
            };
            let readonly = flags.contains(OpenFlags::ReadOnly);
            let mut oflags = if readonly {
                SQLITE_OPEN_READONLY
            } else {
                SQLITE_OPEN_READWRITE
                    | if flags.contains(OpenFlags::Create) {
                        SQLITE_OPEN_CREATE
                    } else {
                        0
                    }
            };
            let full = match kind {
                FileKind::Temp => String::new(),
                _ => self.resolve(path)?,
            };
            let name: Option<(Arc<[u8]>, usize)> = match kind {
                FileKind::Temp => {
                    oflags = SQLITE_OPEN_READWRITE
                        | SQLITE_OPEN_CREATE
                        | SQLITE_OPEN_EXCLUSIVE
                        | SQLITE_OPEN_DELETEONCLOSE
                        | SQLITE_OPEN_TEMP_DB;
                    None
                }
                FileKind::Main => {
                    oflags |= SQLITE_OPEN_MAIN_DB;
                    let params = PENDING_PARAMS.with(|p| {
                        let mut p = p.borrow_mut();
                        match p.as_ref() {
                            Some((pp, _)) if *pp == full => p.take().map(|(_, v)| v),
                            _ => None,
                        }
                    });
                    if params.as_ref().is_some_and(|p| !p.is_empty()) {
                        oflags |= SQLITE_OPEN_URI_FLAG;
                    }
                    Some((self.name_block(&full, params.as_deref().unwrap_or(&[])), 4))
                }
                FileKind::Wal => {
                    oflags |= SQLITE_OPEN_WAL;
                    // A read-only connection without a WAL has nothing to read.
                    if readonly && !self.access(&full, SQLITE_ACCESS_EXISTS)? {
                        return Err(LimboError::CompletionError(CompletionError::IOError(
                            std::io::ErrorKind::NotFound,
                            "open",
                        )));
                    }
                    let db = &full[..full.len() - 4];
                    match self.names.lock().ok().and_then(|m| m.get(db).cloned()) {
                        // The WAL name inside the main database's block.
                        Some((b, off)) => Some((b, off)),
                        None => {
                            let mut v = full.as_bytes().to_vec();
                            v.extend_from_slice(&[0, 0, 0]);
                            Some((v.into(), 0))
                        }
                    }
                }
            };
            let (file, out_flags) = VfsFile::open(self.vfs, name, oflags)?;
            if let FileKind::Main = kind {
                // The VFS fell back to read-only (as unix does for a file it
                // cannot write); the caller reopens read-only.
                if !readonly && out_flags & SQLITE_OPEN_READONLY != 0 {
                    return Err(LimboError::ReadOnly);
                }
                if !readonly && !flags.contains(OpenFlags::NoLock) {
                    // Turso's single-process WAL needs the file to itself.
                    file.lock(SQLITE_LOCK_SHARED)?;
                    file.lock(SQLITE_LOCK_EXCLUSIVE)?;
                }
                if let Ok(mut m) = self.files.lock() {
                    m.retain(|_, w| w.strong_count() > 0);
                    // The first handle is the engine's (the interlock opens more).
                    m.entry(full).or_insert_with(|| Arc::downgrade(&file));
                }
            }
            Ok(file)
        }
    }

    fn remove_file(&self, path: &str) -> turso_core::Result<()> {
        unsafe {
            let full = self.resolve(path)?;
            let v = &*self.vfs;
            let Some(f) = v.x_delete else {
                return Err(vfs_err(SQLITE_IOERR, "delete"));
            };
            let c = CString::new(full).map_err(|_| vfs_err(SQLITE_CANTOPEN, "delete"))?;
            match f(self.vfs, c.as_ptr(), 0) {
                SQLITE_OK => Ok(()),
                SQLITE_IOERR_DELETE_NOENT => Err(LimboError::CompletionError(
                    CompletionError::IOError(std::io::ErrorKind::NotFound, "delete"),
                )),
                rc => Err(vfs_err(rc, "delete")),
            }
        }
    }

    fn supports_shared_wal_coordination(&self) -> bool {
        true
    }

    fn open_shared_wal_file(&self, path: &str) -> turso_core::Result<Arc<dyn File>> {
        let unsupported = |why: &str| {
            LimboError::InvalidArgument(format!(
                "experimental multiprocess WAL is not supported for '{path}': {why}"
            ))
        };
        let full = unsafe { self.resolve(path)? };
        if let Some(db) = full.strip_suffix("-shm") {
            // SQLite's wal-index lives in the VFS's shared memory of the
            // main database file.
            let main = self
                .main_file(db)
                .ok_or_else(|| unsupported("main database not open"))?;
            return Ok(Arc::new(
                VfsShmFile::new(main).ok_or_else(|| unsupported("no shared-memory methods"))?,
            ));
        }
        if full.ends_with("-tshm") {
            return self
                .host_coordination_io(&full)?
                .open_shared_wal_file(&full);
        }
        self.open_file(path, OpenFlags::Create | OpenFlags::NoLock, false)
    }

    fn file_id(&self, path: &str) -> turso_core::Result<FileId> {
        let full = unsafe { self.resolve(path)? };
        // A name that is a host file shares the Database of every other
        // connection to that file (native or through another VFS): turso's
        // WAL index lives in that shared object, so two instances over one
        // file would corrupt it.
        if let Ok(id) = turso_core::io::get_file_id(&full) {
            return Ok(id);
        }
        // Otherwise identity is (VFS, full path): the VFS defines the name.
        let mut id = FileId::from_path_hash(&full);
        id.dev = self.vfs as usize as u64;
        Ok(id)
    }
}

/// A lock that its owning thread may take again.
struct ReentrantLock {
    state: Mutex<(Option<std::thread::ThreadId>, usize)>,
    cv: std::sync::Condvar,
}

struct ReentrantGuard<'a>(&'a ReentrantLock);

impl ReentrantLock {
    fn new() -> Self {
        ReentrantLock {
            state: Mutex::new((None, 0)),
            cv: std::sync::Condvar::new(),
        }
    }

    fn lock(&self) -> ReentrantGuard<'_> {
        let me = std::thread::current().id();
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while st.0.is_some_and(|t| t != me) {
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        st.0 = Some(me);
        st.1 += 1;
        ReentrantGuard(self)
    }
}

impl Drop for ReentrantGuard<'_> {
    fn drop(&mut self) {
        let mut st = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        st.1 -= 1;
        if st.1 == 0 {
            st.0 = None;
            self.0.cv.notify_one();
        }
    }
}

/// A `sqlite3_file` opened through an application VFS.
pub(crate) struct VfsFile {
    /// Backing store of the `sqlite3_file` (szOsFile bytes, 8-aligned).
    mem: std::cell::UnsafeCell<Box<[u64]>>,
    /// Keeps the xOpen name alive for the life of the file.
    _name: Option<Arc<[u8]>>,
    lock_level: std::sync::atomic::AtomicI32,
    /// Serialises calls into the C methods across threads. Reentrant: a VFS
    /// callback may use another connection to the same file on this thread.
    guard: ReentrantLock,
}

unsafe impl Send for VfsFile {}
unsafe impl Sync for VfsFile {}

impl VfsFile {
    unsafe fn open(
        vfs: *mut sqlite3_vfs,
        name: Option<(Arc<[u8]>, usize)>,
        flags: ffi::c_int,
    ) -> turso_core::Result<(Arc<VfsFile>, ffi::c_int)> {
        let v = &*vfs;
        let Some(x_open) = v.x_open else {
            return Err(vfs_err(SQLITE_CANTOPEN, "open"));
        };
        let words = (v.sz_os_file.max(8) as usize).div_ceil(8);
        let file = Arc::new(VfsFile {
            mem: std::cell::UnsafeCell::new(vec![0u64; words].into_boxed_slice()),
            _name: name.as_ref().map(|(b, _)| b.clone()),
            lock_level: std::sync::atomic::AtomicI32::new(SQLITE_LOCK_NONE),
            guard: ReentrantLock::new(),
        });
        let zname = match &name {
            Some((b, off)) => b.as_ptr().add(*off) as *const ffi::c_char,
            None => std::ptr::null(),
        };
        let mut out_flags = 0;
        let rc = x_open(vfs, zname, file.raw(), flags, &mut out_flags);
        if rc != SQLITE_OK {
            // Drop runs xClose if the VFS left pMethods set.
            return Err(vfs_err(rc, "open"));
        }
        if file.methods().is_none() {
            return Err(vfs_err(SQLITE_CANTOPEN, "open"));
        }
        Ok((file, out_flags))
    }

    pub(crate) fn raw(&self) -> *mut sqlite3_file {
        unsafe { (*self.mem.get()).as_mut_ptr() as *mut sqlite3_file }
    }

    fn methods(&self) -> Option<&sqlite3_io_methods> {
        unsafe {
            let m = (*self.raw()).p_methods;
            if m.is_null() {
                None
            } else {
                Some(&*m)
            }
        }
    }

    fn lock(&self, level: ffi::c_int) -> turso_core::Result<()> {
        let _g = self.guard.lock();
        let Some(f) = self.methods().and_then(|m| m.x_lock) else {
            return Ok(());
        };
        let rc = unsafe { f(self.raw(), level) };
        if rc != SQLITE_OK {
            return Err(match rc & 0xff {
                SQLITE_BUSY => LimboError::LockingError(
                    "Failed locking file. File is locked by another process".into(),
                ),
                _ => vfs_err(rc, "lock"),
            });
        }
        self.lock_level.store(level, Ordering::Relaxed);
        Ok(())
    }

    fn unlock(&self, level: ffi::c_int) -> turso_core::Result<()> {
        let _g = self.guard.lock();
        let Some(f) = self.methods().and_then(|m| m.x_unlock) else {
            return Ok(());
        };
        let rc = unsafe { f(self.raw(), level) };
        if rc != SQLITE_OK {
            return Err(vfs_err(rc, "unlock"));
        }
        self.lock_level.store(level, Ordering::Relaxed);
        Ok(())
    }

    fn file_size(&self) -> turso_core::Result<u64> {
        let Some(f) = self.methods().and_then(|m| m.x_file_size) else {
            return Err(vfs_err(SQLITE_IOERR, "file_size"));
        };
        let mut sz = 0i64;
        let rc = unsafe { f(self.raw(), &mut sz) };
        if rc != SQLITE_OK {
            return Err(vfs_err(rc, "file_size"));
        }
        Ok(sz.max(0) as u64)
    }

    /// sqlite3_file_control on this file.
    pub(crate) unsafe fn file_control(&self, op: ffi::c_int, arg: *mut ffi::c_void) -> ffi::c_int {
        let _g = self.guard.lock();
        match self.methods().and_then(|m| m.x_file_control) {
            Some(f) => f(self.raw(), op, arg),
            None => SQLITE_NOTFOUND,
        }
    }
}

impl Drop for VfsFile {
    fn drop(&mut self) {
        let _g = self.guard.lock();
        let Some(m) = self.methods() else {
            return;
        };
        unsafe {
            if self.lock_level.load(Ordering::Relaxed) != SQLITE_LOCK_NONE {
                if let Some(f) = m.x_unlock {
                    f(self.raw(), SQLITE_LOCK_NONE);
                }
            }
            if let Some(f) = m.x_close {
                f(self.raw());
            }
        }
    }
}

impl File for VfsFile {
    fn lock_file(&self, exclusive: bool) -> turso_core::Result<()> {
        if self.lock_level.load(Ordering::Relaxed) < SQLITE_LOCK_SHARED {
            self.lock(SQLITE_LOCK_SHARED)?;
        }
        if exclusive {
            self.lock(SQLITE_LOCK_EXCLUSIVE)?;
        }
        Ok(())
    }

    fn unlock_file(&self) -> turso_core::Result<()> {
        self.unlock(SQLITE_LOCK_NONE)
    }

    // The SQLite interlock's database lock: SHARED for the life of the
    // connection, EXCLUSIVE for locking_mode=EXCLUSIVE, through xLock.
    fn shared_wal_try_lock_byte(
        &self,
        offset: u64,
        exclusive: bool,
        _kind: SharedWalLockKind,
    ) -> turso_core::Result<bool> {
        if offset != DB_SHARED_FIRST {
            return Err(vfs_err(SQLITE_IOERR, "lock"));
        }
        let busy = |r: turso_core::Result<()>| match r {
            Ok(()) => Ok(true),
            Err(LimboError::LockingError(_)) | Err(LimboError::Busy) => Ok(false),
            Err(e) => Err(e),
        };
        if !busy(self.lock(SQLITE_LOCK_SHARED))? {
            return Ok(false);
        }
        if !exclusive {
            return Ok(true);
        }
        if busy(self.lock(SQLITE_LOCK_RESERVED))? && busy(self.lock(SQLITE_LOCK_EXCLUSIVE))? {
            return Ok(true);
        }
        self.unlock(SQLITE_LOCK_SHARED)?;
        Ok(false)
    }

    fn shared_wal_lock_byte(
        &self,
        offset: u64,
        exclusive: bool,
        kind: SharedWalLockKind,
    ) -> turso_core::Result<()> {
        if !exclusive && self.lock_level.load(Ordering::Relaxed) > SQLITE_LOCK_SHARED {
            return self.unlock(SQLITE_LOCK_SHARED);
        }
        match self.shared_wal_try_lock_byte(offset, exclusive, kind)? {
            true => Ok(()),
            false => Err(LimboError::Busy),
        }
    }

    fn shared_wal_unlock_byte(
        &self,
        _offset: u64,
        _kind: SharedWalLockKind,
    ) -> turso_core::Result<()> {
        self.unlock(SQLITE_LOCK_NONE)
    }

    fn pread(&self, pos: u64, c: Completion) -> turso_core::Result<Completion> {
        let _g = self.guard.lock();
        let Some(f) = self.methods().and_then(|m| m.x_read) else {
            return Err(vfs_err(SQLITE_IOERR, "pread"));
        };
        let n = {
            let r = c.as_read();
            let buf = r.buf();
            let slice = buf.as_mut_slice();
            let len = slice.len();
            let rc = unsafe {
                f(
                    self.raw(),
                    slice.as_mut_ptr() as *mut ffi::c_void,
                    len as ffi::c_int,
                    pos as i64,
                )
            };
            match rc {
                SQLITE_OK => len,
                SQLITE_IOERR_SHORT_READ => {
                    // The VFS zero-filled the tail; report the bytes that exist.
                    let size = self.file_size()?;
                    (size.saturating_sub(pos) as usize).min(len)
                }
                rc => return Err(vfs_err(rc, "pread")),
            }
        };
        c.complete(n as i32);
        Ok(c)
    }

    fn pwrite(
        &self,
        pos: u64,
        buffer: Arc<Buffer>,
        c: Completion,
    ) -> turso_core::Result<Completion> {
        let _g = self.guard.lock();
        let Some(f) = self.methods().and_then(|m| m.x_write) else {
            return Err(vfs_err(SQLITE_IOERR, "pwrite"));
        };
        let data = buffer.as_slice();
        let rc = unsafe {
            f(
                self.raw(),
                data.as_ptr() as *const ffi::c_void,
                data.len() as ffi::c_int,
                pos as i64,
            )
        };
        if rc != SQLITE_OK {
            return Err(vfs_err(rc, "pwrite"));
        }
        c.complete(data.len() as i32);
        Ok(c)
    }

    fn sync(&self, c: Completion, sync_type: FileSyncType) -> turso_core::Result<Completion> {
        let _g = self.guard.lock();
        if let Some(f) = self.methods().and_then(|m| m.x_sync) {
            let flags = match sync_type {
                FileSyncType::Fsync => SQLITE_SYNC_NORMAL,
                FileSyncType::FullFsync => SQLITE_SYNC_FULL,
            };
            let rc = unsafe { f(self.raw(), flags) };
            if rc != SQLITE_OK {
                return Err(vfs_err(rc, "sync"));
            }
        }
        c.complete(0);
        Ok(c)
    }

    fn size(&self) -> turso_core::Result<u64> {
        let _g = self.guard.lock();
        self.file_size()
    }

    fn truncate(&self, len: u64, c: Completion) -> turso_core::Result<Completion> {
        let _g = self.guard.lock();
        let Some(f) = self.methods().and_then(|m| m.x_truncate) else {
            return Err(vfs_err(SQLITE_IOERR, "truncate"));
        };
        let rc = unsafe { f(self.raw(), len as i64) };
        if rc != SQLITE_OK {
            return Err(vfs_err(rc, "truncate"));
        }
        c.complete(0);
        Ok(c)
    }
}

/// SQLite's wal-index (`-shm`) through the VFS shared-memory methods of a
/// main-database file: `shared_wal_map` maps 32 KiB regions with xShmMap,
/// the lock bytes 120..127 are xShmLock slots, the barrier is xShmBarrier,
/// and xShmUnmap runs on drop. The DMS byte is the VFS's own business.
pub(crate) struct VfsShmFile {
    file: Arc<VfsFile>,
    /// Lock held per slot: 0 none, else SQLITE_SHM_SHARED or _EXCLUSIVE.
    held: Mutex<[ffi::c_int; SHM_NLOCK as usize]>,
}

unsafe impl Send for VfsShmFile {}
unsafe impl Sync for VfsShmFile {}

struct VfsShmRegion(std::ptr::NonNull<u8>);

unsafe impl Send for VfsShmRegion {}
unsafe impl Sync for VfsShmRegion {}

impl SharedWalMappedRegion for VfsShmRegion {
    fn ptr(&self) -> std::ptr::NonNull<u8> {
        self.0
    }

    fn len(&self) -> usize {
        SHM_REGION
    }
}

impl VfsShmFile {
    /// None if the file's methods have no shared memory (iVersion < 2).
    fn new(file: Arc<VfsFile>) -> Option<Self> {
        let m = file.methods()?;
        if m.i_version < 2 || m.x_shm_map.is_none() || m.x_shm_lock.is_none() {
            return None;
        }
        Some(Self {
            file,
            held: Mutex::new([0; SHM_NLOCK as usize]),
        })
    }

    fn methods(&self) -> &sqlite3_io_methods {
        self.file.methods().expect("checked in new")
    }

    fn map_region(&self, region: usize) -> turso_core::Result<std::ptr::NonNull<u8>> {
        let _g = self.file.guard.lock();
        let f = self.methods().x_shm_map.expect("checked in new");
        let mut p: *mut ffi::c_void = std::ptr::null_mut();
        let rc = unsafe {
            f(
                self.file.raw(),
                region as ffi::c_int,
                SHM_REGION as ffi::c_int,
                1,
                &mut p,
            )
        };
        if rc != SQLITE_OK {
            return Err(vfs_err(rc, "shm map"));
        }
        std::ptr::NonNull::new(p as *mut u8).ok_or_else(|| vfs_err(SQLITE_IOERR, "shm map"))
    }

    fn shm_lock(&self, slot: u64, flags: ffi::c_int) -> turso_core::Result<bool> {
        let _g = self.file.guard.lock();
        let f = self.methods().x_shm_lock.expect("checked in new");
        match unsafe { f(self.file.raw(), slot as ffi::c_int, 1, flags) } {
            SQLITE_OK => Ok(true),
            rc if rc & 0xff == SQLITE_BUSY => Ok(false),
            rc => Err(vfs_err(rc, "shm lock")),
        }
    }

    fn slot(offset: u64) -> turso_core::Result<u64> {
        if (SHM_LOCK_BASE..SHM_LOCK_BASE + SHM_NLOCK).contains(&offset) {
            Ok(offset - SHM_LOCK_BASE)
        } else {
            Err(vfs_err(SQLITE_IOERR, "shm lock"))
        }
    }
}

impl Drop for VfsShmFile {
    fn drop(&mut self) {
        let _g = self.file.guard.lock();
        if let Some(f) = self.methods().x_shm_unmap {
            unsafe { f(self.file.raw(), 0) };
        }
    }
}

impl File for VfsShmFile {
    fn lock_file(&self, _exclusive: bool) -> turso_core::Result<()> {
        Ok(())
    }

    fn unlock_file(&self) -> turso_core::Result<()> {
        Ok(())
    }

    fn pread(&self, _pos: u64, _c: Completion) -> turso_core::Result<Completion> {
        Err(vfs_err(SQLITE_IOERR, "shm read"))
    }

    fn pwrite(
        &self,
        _pos: u64,
        _buffer: Arc<Buffer>,
        _c: Completion,
    ) -> turso_core::Result<Completion> {
        Err(vfs_err(SQLITE_IOERR, "shm write"))
    }

    fn sync(&self, c: Completion, _sync_type: FileSyncType) -> turso_core::Result<Completion> {
        c.complete(0);
        Ok(c)
    }

    /// xShmMap grows the shared memory itself.
    fn size(&self) -> turso_core::Result<u64> {
        Ok(u64::MAX)
    }

    fn truncate(&self, _len: u64, _c: Completion) -> turso_core::Result<Completion> {
        Err(vfs_err(SQLITE_IOERR, "shm truncate"))
    }

    fn shared_wal_set_len(&self, _len: u64) -> turso_core::Result<()> {
        Ok(())
    }

    fn shared_wal_map(
        &self,
        offset: u64,
        len: usize,
    ) -> turso_core::Result<Box<dyn SharedWalMappedRegion>> {
        if len != SHM_REGION || offset % SHM_REGION as u64 != 0 {
            return Err(vfs_err(SQLITE_IOERR, "shm map"));
        }
        let p = self.map_region((offset / SHM_REGION as u64) as usize)?;
        Ok(Box::new(VfsShmRegion(p)))
    }

    fn shared_wal_barrier(&self) {
        let _g = self.file.guard.lock();
        match self.methods().x_shm_barrier {
            Some(f) => unsafe { f(self.file.raw()) },
            None => std::sync::atomic::fence(Ordering::SeqCst),
        }
    }

    fn shared_wal_try_lock_byte(
        &self,
        offset: u64,
        exclusive: bool,
        _kind: SharedWalLockKind,
    ) -> turso_core::Result<bool> {
        if offset == SHM_DMS {
            // xShmMap takes (and if first, resets under) the VFS's DMS lock.
            // Whether others are present cannot be probed through the VFS.
            if exclusive {
                return Ok(false);
            }
            // A VFS whose shared memory cannot be mapped (read-only, absent)
            // gets the single-process mode, as before.
            return match self.map_region(0) {
                Ok(_) => Ok(true),
                Err(err) => Err(LimboError::InvalidArgument(format!(
                    "experimental multiprocess WAL is not supported: xShmMap failed ({err})"
                ))),
            };
        }
        let slot = Self::slot(offset)?;
        let want = if exclusive {
            SQLITE_SHM_EXCLUSIVE
        } else {
            SQLITE_SHM_SHARED
        };
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        let cur = held[slot as usize];
        if cur == want {
            return Ok(true);
        }
        // A VFS like turso's unix converts a held lock in place.
        if self.shm_lock(slot, SQLITE_SHM_LOCK | want)? {
            held[slot as usize] = want;
            return Ok(true);
        }
        if cur == 0 {
            return Ok(false);
        }
        // SQLite's unix VFS does not convert: drop ours, then retry.
        self.shm_lock(slot, SQLITE_SHM_UNLOCK | cur)?;
        held[slot as usize] = 0;
        if self.shm_lock(slot, SQLITE_SHM_LOCK | want)? {
            held[slot as usize] = want;
            return Ok(true);
        }
        if self.shm_lock(slot, SQLITE_SHM_LOCK | cur)? {
            held[slot as usize] = cur;
        }
        Ok(false)
    }

    fn shared_wal_lock_byte(
        &self,
        offset: u64,
        exclusive: bool,
        kind: SharedWalLockKind,
    ) -> turso_core::Result<()> {
        for _ in 0..2000 {
            if self.shared_wal_try_lock_byte(offset, exclusive, kind)? {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Err(LimboError::Busy)
    }

    fn shared_wal_unlock_byte(
        &self,
        offset: u64,
        _kind: SharedWalLockKind,
    ) -> turso_core::Result<()> {
        if offset == SHM_DMS {
            return Ok(());
        }
        let slot = Self::slot(offset)?;
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        let cur = std::mem::take(&mut held[slot as usize]);
        if cur != 0 {
            self.shm_lock(slot, SQLITE_SHM_UNLOCK | cur)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Opening connections
// ---------------------------------------------------------------------------

/// Decodes the query part of a "file:" URI into SQLite's key/value list
/// (`k\0v\0...`), following sqlite3ParseUri: %00 truncates the current
/// component and options with an empty name are dropped.
fn uri_params(uri: &[u8]) -> Vec<u8> {
    let q = match uri.iter().position(|&c| c == b'?' || c == b'#') {
        Some(q) if uri[q] == b'?' => q,
        _ => return Vec::new(),
    };
    let hex = |c: Option<&u8>| c.and_then(|&c| (c as char).to_digit(16)).map(|d| d as u8);
    // Starts with the path's terminator, as SQLite's output buffer does.
    let mut out: Vec<u8> = vec![0];
    let mut state = 1; // 1: key, 2: value
    let mut i = q + 1;
    while i < uri.len() && uri[i] != b'#' {
        let mut c = uri[i];
        i += 1;
        if c == b'%' {
            if let (Some(h), Some(l)) = (hex(uri.get(i)), hex(uri.get(i + 1))) {
                i += 2;
                let octet = (h << 4) | l;
                if octet == 0 {
                    while i < uri.len()
                        && uri[i] != b'#'
                        && !(state == 1 && (uri[i] == b'=' || uri[i] == b'&'))
                        && !(state == 2 && uri[i] == b'&')
                    {
                        i += 1;
                    }
                    continue;
                }
                out.push(octet);
                continue;
            }
        }
        if state == 1 && (c == b'&' || c == b'=') {
            if out.last() == Some(&0) {
                // Empty option name: skip the whole option.
                while i < uri.len() && uri[i] != b'#' && uri[i - 1] != b'&' {
                    i += 1;
                }
                continue;
            }
            if c == b'&' {
                out.push(0);
            } else {
                state = 2;
            }
            c = 0;
        } else if state == 2 && c == b'&' {
            c = 0;
            state = 1;
        }
        out.push(c);
    }
    if state == 1 {
        out.push(0);
    }
    out.extend_from_slice(&[0; 4]);
    let mut res = Vec::new();
    let mut it = out[1..].split(|&b| b == 0);
    while let Some(k) = it.next() {
        if k.is_empty() {
            break;
        }
        res.extend_from_slice(k);
        res.push(0);
        res.extend_from_slice(it.next().unwrap_or(&[]));
        res.push(0);
    }
    res
}

fn param_value<'a>(params: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut it = params.split(|&b| b == 0);
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        if k.is_empty() {
            break;
        }
        if k == key {
            return Some(v);
        }
    }
    None
}

/// Resolves the VFS for an open: the URI's `vfs=` wins over zVfs, and no
/// name means the default VFS. `Ok(None)` selects turso's built-in I/O.
unsafe fn choose_vfs(
    z_vfs: *const ffi::c_char,
    uri_vfs: Option<&[u8]>,
) -> Result<Option<*mut sqlite3_vfs>, String> {
    let name: Option<Vec<u8>> = match uri_vfs {
        Some(v) => Some(v.to_vec()),
        None if !z_vfs.is_null() => Some(CStr::from_ptr(z_vfs).to_bytes().to_vec()),
        None => None,
    };
    let p = match &name {
        Some(n) if !n.is_empty() => {
            let c = CString::new(n.clone()).map_err(|_| String::new())?;
            compat_config::sqlite3_vfs_find(c.as_ptr())
        }
        _ => compat_config::sqlite3_vfs_find(std::ptr::null()),
    };
    if p.is_null() {
        return Err(String::from_utf8_lossy(name.as_deref().unwrap_or(b"")).into_owned());
    }
    if p == super::vfs_unix::unix_vfs() {
        Ok(None)
    } else {
        Ok(Some(p))
    }
}

/// True when the default VFS is an application VFS (registered with
/// makeDflt), so even sqlite3_open() must go through it.
pub(crate) fn default_is_app_vfs() -> bool {
    unsafe { compat_config::sqlite3_vfs_find(std::ptr::null()) != super::vfs_unix::unix_vfs() }
}

/// VFS routing for sqlite3_open_v2. Returns None when turso's own I/O should
/// open the database (built-in "unix" VFS, in-memory and temporary
/// databases); otherwise opens it through the application VFS and returns
/// the result code.
pub(crate) unsafe fn open_v2_via_vfs(
    filename: &str,
    path: &str,
    use_memory: bool,
    flags: ffi::c_int,
    z_vfs: *const ffi::c_char,
    db_out: *mut *mut sqlite3,
) -> Option<ffi::c_int> {
    let is_uri = flags & SQLITE_OPEN_URI != 0 && filename.starts_with("file:");
    let params = if is_uri {
        uri_params(filename.as_bytes())
    } else {
        Vec::new()
    };
    let vfs = match choose_vfs(z_vfs, param_value(&params, b"vfs")) {
        Ok(v) => v,
        Err(name) => {
            let msg = format!("no such vfs: {name}");
            return Some(compat_lifecycle::open_failed_with(
                db_out,
                SQLITE_ERROR,
                &msg,
            ));
        }
    };
    let vfs = vfs?;
    if use_memory || filename.is_empty() {
        return None;
    }
    let io = VfsIo::for_vfs(vfs);
    let full = match io.full_pathname(path) {
        Ok(p) => p,
        Err(_) => return Some(compat_lifecycle::open_failed(db_out, SQLITE_CANTOPEN)),
    };
    // nolock=1 / immutable=1 (SQLite's URI parameters): no file locking;
    // an immutable database is also never written.
    let flag = |k: &[u8]| {
        param_value(&params, k).is_some_and(|v| {
            matches!(v.first(), Some(b'1' | b'y' | b'Y' | b't' | b'T'))
                || v.eq_ignore_ascii_case(b"on")
        })
    };
    let immutable = flag(b"immutable");
    let nolock = immutable || flag(b"nolock");
    let mut readonly = flags & SQLITE_OPEN_READONLY != 0 || immutable;
    let io_dyn: Arc<dyn IO> = io;
    let res = loop {
        let mut open_flags = if readonly {
            OpenFlags::ReadOnly
        } else if flags & SQLITE_OPEN_CREATE != 0 {
            OpenFlags::Create
        } else {
            OpenFlags::None
        };
        if nolock {
            open_flags |= OpenFlags::NoLock;
        }
        PENDING_PARAMS.with(|p| *p.borrow_mut() = Some((full.clone(), params.clone())));
        // Shared with other processes through the VFS's locks and shared
        // memory when it has them and names a host file; else exclusive.
        let host_file = std::path::Path::new(&full)
            .parent()
            .is_some_and(|dir| dir.is_dir());
        let res = if host_file && !nolock {
            super::multiprocess::open_file_db(io_dyn.clone(), &full, open_flags, default_db_opts())
        } else {
            turso_core::Database::open_file_with_flags(
                io_dyn.clone(),
                &full,
                open_flags,
                default_db_opts(),
                None,
                Arc::new(SqliteDialect),
            )
        };
        PENDING_PARAMS.with(|p| *p.borrow_mut() = None);
        match res {
            // xOpen granted read-only access only: open read-only, as SQLite.
            Err(LimboError::ReadOnly) if !readonly => readonly = true,
            res => break res,
        }
    };
    let db = match res {
        Ok(db) => db,
        Err(e) => {
            trace!("error opening {full} through a VFS: {e:?}");
            return Some(compat_lifecycle::open_failed_err(db_out, &e));
        }
    };
    let conn = match db.connect(ConnectOpts::new()) {
        Ok(c) => c,
        Err(e) => return Some(compat_lifecycle::open_failed_err(db_out, &e)),
    };
    let name = CString::new(full).unwrap_or_default();
    *db_out = sqlite3::new(io_dyn, db, conn, name).into_raw();
    Some(extension::run_auto_extensions(*db_out))
}

/// Closes the engine connection (the final checkpoint) while the handle is
/// still valid, so VFS callbacks that re-enter it during close do not touch
/// freed memory. Skipped while statements keep a zombie handle alive.
pub(crate) unsafe fn close_connection_early(db: *mut sqlite3) {
    let conn = match (*db).inner.lock() {
        Ok(inner) if inner.stmt_list.is_null() => inner.conn.clone(),
        _ => return,
    };
    let _ = conn.close();
}

/// sqlite3_file_control for a connection whose main database was opened
/// through an application VFS. None when it was not.
pub(crate) unsafe fn file_control(
    db: *mut sqlite3,
    op: ffi::c_int,
    arg: *mut ffi::c_void,
) -> Option<ffi::c_int> {
    let (io, path) = {
        let inner = (*db).inner.lock().ok()?;
        (inner._db.io.clone(), inner._db.path.clone())
    };
    let adapter = VfsIo::of_io(&io)?;
    let file = adapter.main_file(&path);
    Some(match op {
        SQLITE_FCNTL_FILE_POINTER => {
            *(arg as *mut *mut sqlite3_file) =
                file.as_ref().map_or(std::ptr::null_mut(), |f| f.raw());
            SQLITE_OK
        }
        SQLITE_FCNTL_VFS_POINTER => {
            *(arg as *mut *mut sqlite3_vfs) = adapter.vfs();
            SQLITE_OK
        }
        SQLITE_FCNTL_JOURNAL_POINTER => {
            *(arg as *mut *mut sqlite3_file) = std::ptr::null_mut();
            SQLITE_OK
        }
        _ => match file {
            Some(f) => f.file_control(op, arg),
            None => SQLITE_NOTFOUND,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(uri: &str) -> Vec<String> {
        uri_params(uri.as_bytes())
            .split(|&b| b == 0)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    }

    #[test]
    fn uri_params_like_sqlite() {
        assert_eq!(kv("file:test.db?hello=world"), ["hello", "world", ""]);
        assert_eq!(
            kv("file:test.db?hello&world"),
            ["hello", "", "world", "", ""]
        );
        assert_eq!(
            kv("file:test.db?%68%65%6C%6C%6F=%77%6F%72%6C%64"),
            ["hello", "world", ""]
        );
        assert_eq!(
            kv("file:testdb%00.db?hello%00extra=world%00ex"),
            ["hello", "world", ""]
        );
        assert_eq!(kv("file:testdb%00.db?=world&xyz=abc"), ["xyz", "abc", ""]);
        assert_eq!(
            kv("file:test.db?hello=%00world&xyz="),
            ["hello", "", "xyz", "", ""]
        );
        assert_eq!(kv("file:test.db?=#ravada"), [""]);
        assert_eq!(
            kv("file:test.db?&&&&&&&&hello=world&&&&&&&"),
            ["hello", "world", ""]
        );
    }
}
