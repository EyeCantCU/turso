//! The built-in "unix" VFS returned by sqlite3_vfs_find(NULL).
//!
//! Turso's own connections do file I/O through turso_core, not through this
//! VFS. It exists so that applications that wrap the default VFS (shim VFSes
//! such as GDAL's and proj's delegate to it) get working methods. Locks use
//! SQLite's byte offsets with OFD locks, so they also conflict with real
//! SQLite processes and between connections of this process.

use super::vfs::{sqlite3_file, sqlite3_io_methods, sqlite3_vfs, VfsFn};
use super::*;

const PENDING_BYTE: i64 = 0x4000_0000;
const RESERVED_BYTE: i64 = PENDING_BYTE + 1;
const SHARED_FIRST: i64 = PENDING_BYTE + 2;
const SHARED_SIZE: i64 = 510;

const NO_LOCK: ffi::c_int = 0;
const SHARED_LOCK: ffi::c_int = 1;
const RESERVED_LOCK: ffi::c_int = 2;
const PENDING_LOCK: ffi::c_int = 3;
const EXCLUSIVE_LOCK: ffi::c_int = 4;

const SQLITE_OPEN_READONLY: ffi::c_int = 0x1;
const SQLITE_OPEN_READWRITE: ffi::c_int = 0x2;
const SQLITE_OPEN_CREATE: ffi::c_int = 0x4;
const SQLITE_OPEN_DELETEONCLOSE: ffi::c_int = 0x8;
const SQLITE_OPEN_EXCLUSIVE: ffi::c_int = 0x10;

const SQLITE_IOERR_READ: ffi::c_int = SQLITE_IOERR | (1 << 8);
const SQLITE_IOERR_SHORT_READ: ffi::c_int = SQLITE_IOERR | (2 << 8);
const SQLITE_IOERR_WRITE: ffi::c_int = SQLITE_IOERR | (3 << 8);
const SQLITE_IOERR_FSYNC: ffi::c_int = SQLITE_IOERR | (4 << 8);
const SQLITE_IOERR_TRUNCATE: ffi::c_int = SQLITE_IOERR | (6 << 8);
const SQLITE_IOERR_FSTAT: ffi::c_int = SQLITE_IOERR | (7 << 8);
const SQLITE_IOERR_UNLOCK: ffi::c_int = SQLITE_IOERR | (8 << 8);
const SQLITE_IOERR_RDLOCK: ffi::c_int = SQLITE_IOERR | (9 << 8);
const SQLITE_IOERR_DELETE: ffi::c_int = SQLITE_IOERR | (10 << 8);
const SQLITE_IOERR_LOCK: ffi::c_int = SQLITE_IOERR | (15 << 8);
const SQLITE_IOERR_CHECKRESERVEDLOCK: ffi::c_int = SQLITE_IOERR | (14 << 8);
const SQLITE_IOERR_SHMOPEN: ffi::c_int = SQLITE_IOERR | (18 << 8);
const SQLITE_IOERR_SHMSIZE: ffi::c_int = SQLITE_IOERR | (19 << 8);
const SQLITE_IOERR_SHMMAP: ffi::c_int = SQLITE_IOERR | (21 << 8);
const SQLITE_IOERR_DELETE_NOENT: ffi::c_int = SQLITE_IOERR | (23 << 8);
const SQLITE_FULL_CODE: ffi::c_int = SQLITE_FULL;

const SQLITE_FCNTL_LOCKSTATE: ffi::c_int = 1;
const SQLITE_FCNTL_LAST_ERRNO: ffi::c_int = 4;
const SQLITE_FCNTL_SIZE_HINT: ffi::c_int = 5;
const SQLITE_FCNTL_CHUNK_SIZE: ffi::c_int = 6;
const SQLITE_FCNTL_PERSIST_WAL: ffi::c_int = 10;
const SQLITE_FCNTL_VFSNAME: ffi::c_int = 12;
const SQLITE_FCNTL_POWERSAFE_OVERWRITE: ffi::c_int = 13;
const SQLITE_FCNTL_HAS_MOVED: ffi::c_int = 20;

const SQLITE_SHM_UNLOCK: ffi::c_int = 1;
const SQLITE_SHM_SHARED: ffi::c_int = 4;
const SQLITE_SHM_EXCLUSIVE: ffi::c_int = 8;
/// First lock byte of the wal-index, and its dead-man switch byte.
const SHM_LOCK_BASE: i64 = 120;
const SHM_DMS: i64 = 128;

const SQLITE_IOCAP_POWERSAFE_OVERWRITE: ffi::c_int = 0x1000;

/// `sqlite3_file` subclass of this VFS.
#[repr(C)]
struct UnixFile {
    base: sqlite3_file,
    fd: ffi::c_int,
    lock: ffi::c_int,
    last_errno: ffi::c_int,
    persist_wal: ffi::c_int,
    psow: ffi::c_int,
    /// malloc'd path, NULL for anonymous temp files.
    path: *mut ffi::c_char,
    shm: *mut UnixShm,
}

struct UnixShm {
    fd: ffi::c_int,
    path: CString,
    region_size: usize,
    regions: Vec<*mut ffi::c_void>,
}

fn errno() -> ffi::c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Sets an OFD lock on `[start, start+len)`; returns false when it conflicts.
unsafe fn ofd_lock(
    fd: ffi::c_int,
    ty: ffi::c_int,
    start: i64,
    len: i64,
) -> Result<bool, ffi::c_int> {
    let mut fl: libc::flock = std::mem::zeroed();
    fl.l_type = ty as _;
    fl.l_whence = libc::SEEK_SET as _;
    fl.l_start = start as _;
    fl.l_len = len as _;
    if libc::fcntl(fd, libc::F_OFD_SETLK, &fl) == 0 {
        return Ok(true);
    }
    match errno() {
        libc::EAGAIN | libc::EACCES => Ok(false),
        e => Err(e),
    }
}

unsafe fn file_of(p: *mut sqlite3_file) -> &'static mut UnixFile {
    &mut *(p as *mut UnixFile)
}

unsafe extern "C" fn x_close(p: *mut sqlite3_file) -> ffi::c_int {
    let f = file_of(p);
    x_shm_unmap(p, 0);
    if f.fd >= 0 {
        libc::close(f.fd);
        f.fd = -1;
    }
    if !f.path.is_null() {
        libc::free(f.path as *mut ffi::c_void);
        f.path = std::ptr::null_mut();
    }
    f.base.p_methods = std::ptr::null();
    SQLITE_OK
}

unsafe extern "C" fn x_read(
    p: *mut sqlite3_file,
    buf: *mut ffi::c_void,
    amt: ffi::c_int,
    off: i64,
) -> ffi::c_int {
    let f = file_of(p);
    let want = amt.max(0) as usize;
    let mut got = 0usize;
    while got < want {
        let n = libc::pread(
            f.fd,
            (buf as *mut u8).add(got) as *mut ffi::c_void,
            want - got,
            (off + got as i64) as libc::off_t,
        );
        if n < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            f.last_errno = errno();
            return SQLITE_IOERR_READ;
        }
        if n == 0 {
            break;
        }
        got += n as usize;
    }
    if got < want {
        std::ptr::write_bytes((buf as *mut u8).add(got), 0, want - got);
        return SQLITE_IOERR_SHORT_READ;
    }
    SQLITE_OK
}

unsafe extern "C" fn x_write(
    p: *mut sqlite3_file,
    buf: *const ffi::c_void,
    amt: ffi::c_int,
    off: i64,
) -> ffi::c_int {
    let f = file_of(p);
    let want = amt.max(0) as usize;
    let mut done = 0usize;
    while done < want {
        let n = libc::pwrite(
            f.fd,
            (buf as *const u8).add(done) as *const ffi::c_void,
            want - done,
            (off + done as i64) as libc::off_t,
        );
        if n < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            f.last_errno = errno();
            return if f.last_errno == libc::ENOSPC {
                SQLITE_FULL_CODE
            } else {
                SQLITE_IOERR_WRITE
            };
        }
        if n == 0 {
            return SQLITE_FULL_CODE;
        }
        done += n as usize;
    }
    SQLITE_OK
}

unsafe extern "C" fn x_truncate(p: *mut sqlite3_file, size: i64) -> ffi::c_int {
    let f = file_of(p);
    if libc::ftruncate(f.fd, size as libc::off_t) != 0 {
        f.last_errno = errno();
        return SQLITE_IOERR_TRUNCATE;
    }
    SQLITE_OK
}

unsafe extern "C" fn x_sync(p: *mut sqlite3_file, _flags: ffi::c_int) -> ffi::c_int {
    let f = file_of(p);
    if libc::fsync(f.fd) != 0 {
        f.last_errno = errno();
        return SQLITE_IOERR_FSYNC;
    }
    SQLITE_OK
}

unsafe extern "C" fn x_file_size(p: *mut sqlite3_file, out: *mut i64) -> ffi::c_int {
    let f = file_of(p);
    let mut st: libc::stat = std::mem::zeroed();
    if libc::fstat(f.fd, &mut st) != 0 {
        f.last_errno = errno();
        return SQLITE_IOERR_FSTAT;
    }
    *out = st.st_size as i64;
    SQLITE_OK
}

unsafe extern "C" fn x_lock(p: *mut sqlite3_file, level: ffi::c_int) -> ffi::c_int {
    let f = file_of(p);
    if f.lock >= level {
        return SQLITE_OK;
    }
    let fd = f.fd;
    let busy_or = |r: Result<bool, ffi::c_int>, err: ffi::c_int| match r {
        Ok(true) => SQLITE_OK,
        Ok(false) => SQLITE_BUSY,
        Err(_) => err,
    };
    if level == SHARED_LOCK {
        // PENDING keeps new readers out while a writer waits for EXCLUSIVE.
        let rc = busy_or(
            ofd_lock(fd, libc::F_RDLCK, PENDING_BYTE, 1),
            SQLITE_IOERR_LOCK,
        );
        if rc != SQLITE_OK {
            return rc;
        }
        let rc = busy_or(
            ofd_lock(fd, libc::F_RDLCK, SHARED_FIRST, SHARED_SIZE),
            SQLITE_IOERR_RDLOCK,
        );
        let _ = ofd_lock(fd, libc::F_UNLCK, PENDING_BYTE, 1);
        if rc == SQLITE_OK {
            f.lock = SHARED_LOCK;
        }
        return rc;
    }
    if f.lock == NO_LOCK {
        // SQLite requires SHARED before anything stronger.
        return SQLITE_MISUSE;
    }
    if level == RESERVED_LOCK {
        let rc = busy_or(
            ofd_lock(fd, libc::F_WRLCK, RESERVED_BYTE, 1),
            SQLITE_IOERR_LOCK,
        );
        if rc == SQLITE_OK {
            f.lock = RESERVED_LOCK;
        }
        return rc;
    }
    if f.lock < PENDING_LOCK {
        let rc = busy_or(
            ofd_lock(fd, libc::F_WRLCK, PENDING_BYTE, 1),
            SQLITE_IOERR_LOCK,
        );
        if rc != SQLITE_OK {
            return rc;
        }
        f.lock = PENDING_LOCK;
    }
    if level == EXCLUSIVE_LOCK {
        let rc = busy_or(
            ofd_lock(fd, libc::F_WRLCK, SHARED_FIRST, SHARED_SIZE),
            SQLITE_IOERR_LOCK,
        );
        if rc != SQLITE_OK {
            return rc;
        }
        f.lock = EXCLUSIVE_LOCK;
    }
    SQLITE_OK
}

unsafe extern "C" fn x_unlock(p: *mut sqlite3_file, level: ffi::c_int) -> ffi::c_int {
    let f = file_of(p);
    if f.lock <= level {
        return SQLITE_OK;
    }
    let fd = f.fd;
    if f.lock > SHARED_LOCK {
        if level == SHARED_LOCK
            && ofd_lock(fd, libc::F_RDLCK, SHARED_FIRST, SHARED_SIZE) != Ok(true)
        {
            return SQLITE_IOERR_RDLOCK;
        }
        if ofd_lock(fd, libc::F_UNLCK, PENDING_BYTE, 2).is_err() {
            return SQLITE_IOERR_UNLOCK;
        }
    }
    if level == NO_LOCK && ofd_lock(fd, libc::F_UNLCK, SHARED_FIRST, SHARED_SIZE).is_err() {
        return SQLITE_IOERR_UNLOCK;
    }
    f.lock = level;
    SQLITE_OK
}

unsafe extern "C" fn x_check_reserved_lock(
    p: *mut sqlite3_file,
    out: *mut ffi::c_int,
) -> ffi::c_int {
    let f = file_of(p);
    if f.lock > SHARED_LOCK {
        *out = 1;
        return SQLITE_OK;
    }
    let mut fl: libc::flock = std::mem::zeroed();
    fl.l_type = libc::F_WRLCK as _;
    fl.l_whence = libc::SEEK_SET as _;
    fl.l_start = RESERVED_BYTE as _;
    fl.l_len = 1;
    if libc::fcntl(f.fd, libc::F_OFD_GETLK, &mut fl) != 0 {
        return SQLITE_IOERR_CHECKRESERVEDLOCK;
    }
    *out = (fl.l_type as ffi::c_int != libc::F_UNLCK) as ffi::c_int;
    SQLITE_OK
}

unsafe extern "C" fn x_file_control(
    p: *mut sqlite3_file,
    op: ffi::c_int,
    arg: *mut ffi::c_void,
) -> ffi::c_int {
    let f = file_of(p);
    let ip = arg as *mut ffi::c_int;
    let bool_setting = |slot: &mut ffi::c_int| {
        if *ip < 0 {
            *ip = *slot;
        } else {
            *slot = (*ip != 0) as ffi::c_int;
        }
    };
    match op {
        SQLITE_FCNTL_LOCKSTATE => *ip = f.lock,
        SQLITE_FCNTL_LAST_ERRNO => *ip = f.last_errno,
        SQLITE_FCNTL_SIZE_HINT | SQLITE_FCNTL_CHUNK_SIZE => {}
        SQLITE_FCNTL_PERSIST_WAL => bool_setting(&mut f.persist_wal),
        SQLITE_FCNTL_POWERSAFE_OVERWRITE => bool_setting(&mut f.psow),
        SQLITE_FCNTL_VFSNAME => {
            let p = sqlite3_malloc(5) as *mut u8;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(b"unix\0".as_ptr(), p, 5);
            }
            *(arg as *mut *mut u8) = p;
        }
        SQLITE_FCNTL_HAS_MOVED => {
            let mut moved = 0;
            if !f.path.is_null() {
                let mut a: libc::stat = std::mem::zeroed();
                let mut b: libc::stat = std::mem::zeroed();
                if libc::stat(f.path, &mut a) != 0
                    || libc::fstat(f.fd, &mut b) != 0
                    || a.st_ino != b.st_ino
                    || a.st_dev != b.st_dev
                {
                    moved = 1;
                }
            }
            *ip = moved;
        }
        _ => return SQLITE_NOTFOUND,
    }
    SQLITE_OK
}

unsafe extern "C" fn x_sector_size(_: *mut sqlite3_file) -> ffi::c_int {
    4096
}

unsafe extern "C" fn x_device_characteristics(p: *mut sqlite3_file) -> ffi::c_int {
    if file_of(p).psow != 0 {
        SQLITE_IOCAP_POWERSAFE_OVERWRITE
    } else {
        0
    }
}

unsafe fn shm_open(f: &mut UnixFile) -> ffi::c_int {
    if f.path.is_null() {
        return SQLITE_IOERR_SHMOPEN;
    }
    let mut name = CStr::from_ptr(f.path).to_bytes().to_vec();
    name.extend_from_slice(b"-shm");
    let path = match CString::new(name) {
        Ok(c) => c,
        Err(_) => return SQLITE_IOERR_SHMOPEN,
    };
    let fd = libc::open(
        path.as_ptr(),
        libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC,
        0o644,
    );
    if fd < 0 {
        return SQLITE_IOERR_SHMOPEN;
    }
    // The first opener (nobody holds DMS) starts from an empty index.
    match ofd_lock(fd, libc::F_WRLCK, SHM_DMS, 1) {
        Ok(true) => {
            if libc::ftruncate(fd, 0) != 0 {
                libc::close(fd);
                return SQLITE_IOERR_SHMOPEN;
            }
        }
        Ok(false) => {}
        Err(_) => {
            libc::close(fd);
            return SQLITE_IOERR_SHMOPEN;
        }
    }
    if ofd_lock(fd, libc::F_RDLCK, SHM_DMS, 1) != Ok(true) {
        libc::close(fd);
        return SQLITE_BUSY;
    }
    f.shm = Box::into_raw(Box::new(UnixShm {
        fd,
        path,
        region_size: 0,
        regions: Vec::new(),
    }));
    SQLITE_OK
}

unsafe extern "C" fn x_shm_map(
    p: *mut sqlite3_file,
    region: ffi::c_int,
    size: ffi::c_int,
    extend: ffi::c_int,
    out: *mut *mut ffi::c_void,
) -> ffi::c_int {
    let f = file_of(p);
    *out = std::ptr::null_mut();
    if f.shm.is_null() {
        let rc = shm_open(f);
        if rc != SQLITE_OK {
            return rc;
        }
    }
    let shm = &mut *f.shm;
    let (region, size) = (region.max(0) as usize, size.max(0) as usize);
    if shm.region_size != 0 && shm.region_size != size {
        return SQLITE_IOERR_SHMMAP;
    }
    shm.region_size = size;
    if region >= shm.regions.len() {
        let need = ((region + 1) * size) as i64;
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(shm.fd, &mut st) != 0 {
            return SQLITE_IOERR_SHMSIZE;
        }
        if (st.st_size as i64) < need {
            if extend == 0 {
                return SQLITE_OK;
            }
            if libc::ftruncate(shm.fd, need as libc::off_t) != 0 {
                return SQLITE_IOERR_SHMSIZE;
            }
        }
        while shm.regions.len() <= region {
            let off = shm.regions.len() * size;
            let m = libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                shm.fd,
                off as libc::off_t,
            );
            if m == libc::MAP_FAILED {
                return SQLITE_IOERR_SHMMAP;
            }
            shm.regions.push(m);
        }
    }
    *out = shm.regions[region];
    SQLITE_OK
}

unsafe extern "C" fn x_shm_lock(
    p: *mut sqlite3_file,
    offset: ffi::c_int,
    n: ffi::c_int,
    flags: ffi::c_int,
) -> ffi::c_int {
    let f = file_of(p);
    if f.shm.is_null() {
        return SQLITE_IOERR_SHMOPEN;
    }
    let ty = if flags & SQLITE_SHM_UNLOCK != 0 {
        libc::F_UNLCK
    } else if flags & SQLITE_SHM_EXCLUSIVE != 0 {
        libc::F_WRLCK
    } else if flags & SQLITE_SHM_SHARED != 0 {
        libc::F_RDLCK
    } else {
        return SQLITE_MISUSE;
    };
    match ofd_lock((*f.shm).fd, ty, SHM_LOCK_BASE + offset as i64, n as i64) {
        Ok(true) => SQLITE_OK,
        Ok(false) => SQLITE_BUSY,
        Err(_) => SQLITE_IOERR_LOCK,
    }
}

unsafe extern "C" fn x_shm_barrier(_: *mut sqlite3_file) {
    std::sync::atomic::fence(Ordering::SeqCst);
}

unsafe extern "C" fn x_shm_unmap(p: *mut sqlite3_file, delete: ffi::c_int) -> ffi::c_int {
    let f = file_of(p);
    if f.shm.is_null() {
        return SQLITE_OK;
    }
    let shm = Box::from_raw(f.shm);
    f.shm = std::ptr::null_mut();
    for &r in &shm.regions {
        libc::munmap(r, shm.region_size);
    }
    if delete != 0 {
        libc::unlink(shm.path.as_ptr());
    }
    libc::close(shm.fd);
    SQLITE_OK
}

unsafe extern "C" fn x_fetch(
    _: *mut sqlite3_file,
    _: i64,
    _: ffi::c_int,
    out: *mut *mut ffi::c_void,
) -> ffi::c_int {
    // No memory mapping: callers fall back to xRead.
    *out = std::ptr::null_mut();
    SQLITE_OK
}

unsafe extern "C" fn x_unfetch(_: *mut sqlite3_file, _: i64, _: *mut ffi::c_void) -> ffi::c_int {
    SQLITE_OK
}

static UNIX_IO_METHODS: sqlite3_io_methods = sqlite3_io_methods {
    i_version: 3,
    x_close: Some(x_close),
    x_read: Some(x_read),
    x_write: Some(x_write),
    x_truncate: Some(x_truncate),
    x_sync: Some(x_sync),
    x_file_size: Some(x_file_size),
    x_lock: Some(x_lock),
    x_unlock: Some(x_unlock),
    x_check_reserved_lock: Some(x_check_reserved_lock),
    x_file_control: Some(x_file_control),
    x_sector_size: Some(x_sector_size),
    x_device_characteristics: Some(x_device_characteristics),
    x_shm_map: Some(x_shm_map),
    x_shm_lock: Some(x_shm_lock),
    x_shm_barrier: Some(x_shm_barrier),
    x_shm_unmap: Some(x_shm_unmap),
    x_fetch: Some(x_fetch),
    x_unfetch: Some(x_unfetch),
};

unsafe fn temp_file_name() -> CString {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut rnd = [0u8; 8];
    libc::getrandom(rnd.as_mut_ptr() as *mut ffi::c_void, 8, 0);
    let dir = compat_config::temp_directory_override().unwrap_or_else(std::env::temp_dir);
    let name = dir.join(format!(
        "etilqs_{:x}{:x}{:x}",
        u64::from_ne_bytes(rnd),
        std::process::id(),
        n
    ));
    CString::new(name.to_string_lossy().into_owned()).unwrap_or_default()
}

unsafe extern "C" fn vfs_open(
    _: *mut sqlite3_vfs,
    name: *const ffi::c_char,
    file: *mut sqlite3_file,
    flags: ffi::c_int,
    out_flags: *mut ffi::c_int,
) -> ffi::c_int {
    let f = &mut *(file as *mut UnixFile);
    std::ptr::write_bytes(f as *mut UnixFile, 0, 1);
    f.fd = -1;
    let anon = name.is_null() || *name == 0;
    let temp = if anon { Some(temp_file_name()) } else { None };
    let path = temp.as_ref().map(|c| c.as_ptr()).unwrap_or(name);
    let mut flags = flags;
    let mut oflags = libc::O_CLOEXEC | libc::O_LARGEFILE;
    if anon {
        flags |= SQLITE_OPEN_DELETEONCLOSE | SQLITE_OPEN_CREATE | SQLITE_OPEN_EXCLUSIVE;
    }
    if flags & SQLITE_OPEN_READWRITE != 0 {
        oflags |= libc::O_RDWR;
    } else {
        oflags |= libc::O_RDONLY;
    }
    if flags & SQLITE_OPEN_CREATE != 0 {
        oflags |= libc::O_CREAT;
        if flags & SQLITE_OPEN_EXCLUSIVE != 0 {
            oflags |= libc::O_EXCL;
        }
    }
    let mut fd = libc::open(path, oflags, 0o644);
    if fd < 0 && flags & SQLITE_OPEN_READWRITE != 0 && errno() != libc::EISDIR {
        // As SQLite: fall back to read-only access.
        let e = errno();
        if e == libc::EACCES || e == libc::EROFS || e == libc::EPERM {
            fd = libc::open(path, libc::O_RDONLY | libc::O_CLOEXEC | libc::O_LARGEFILE);
            if fd >= 0 {
                flags =
                    (flags & !(SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE)) | SQLITE_OPEN_READONLY;
            }
        }
    }
    if fd < 0 {
        f.last_errno = errno();
        return SQLITE_CANTOPEN;
    }
    if flags & SQLITE_OPEN_DELETEONCLOSE != 0 {
        libc::unlink(path);
    } else {
        f.path = libc::strdup(path);
    }
    f.fd = fd;
    f.psow = 1;
    f.base.p_methods = &UNIX_IO_METHODS;
    if !out_flags.is_null() {
        *out_flags = flags;
    }
    SQLITE_OK
}

unsafe extern "C" fn vfs_delete(
    _: *mut sqlite3_vfs,
    name: *const ffi::c_char,
    sync_dir: ffi::c_int,
) -> ffi::c_int {
    if libc::unlink(name) != 0 {
        return if errno() == libc::ENOENT {
            SQLITE_IOERR_DELETE_NOENT
        } else {
            SQLITE_IOERR_DELETE
        };
    }
    if sync_dir != 0 {
        let path = std::path::Path::new(std::ffi::OsStr::from_encoded_bytes_unchecked(
            CStr::from_ptr(name).to_bytes(),
        ));
        if let Some(dir) = path.parent() {
            let dir = if dir.as_os_str().is_empty() {
                std::path::Path::new(".")
            } else {
                dir
            };
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
    }
    SQLITE_OK
}

unsafe extern "C" fn vfs_access(
    _: *mut sqlite3_vfs,
    name: *const ffi::c_char,
    flags: ffi::c_int,
    out: *mut ffi::c_int,
) -> ffi::c_int {
    *out = match flags {
        1 => (libc::access(name, libc::R_OK | libc::W_OK) == 0) as ffi::c_int, // READWRITE
        2 => (libc::access(name, libc::R_OK) == 0) as ffi::c_int,              // READ
        _ => {
            // EXISTS: as SQLite, an empty regular file does not count.
            let mut st: libc::stat = std::mem::zeroed();
            (libc::stat(name, &mut st) == 0
                && ((st.st_mode & libc::S_IFMT) != libc::S_IFREG || st.st_size > 0))
                as ffi::c_int
        }
    };
    SQLITE_OK
}

unsafe extern "C" fn vfs_full_pathname(
    _: *mut sqlite3_vfs,
    name: *const ffi::c_char,
    n_out: ffi::c_int,
    out: *mut ffi::c_char,
) -> ffi::c_int {
    let rel = CStr::from_ptr(name).to_bytes();
    let mut full: Vec<u8> = if rel.first() == Some(&b'/') {
        rel.to_vec()
    } else {
        match std::env::current_dir() {
            Ok(d) => {
                let mut v = d.into_os_string().into_encoded_bytes();
                v.push(b'/');
                v.extend_from_slice(rel);
                v
            }
            Err(_) => return SQLITE_CANTOPEN,
        }
    };
    // Drop "." components and duplicate slashes, as SQLite's appendOnePathElement.
    let mut parts: Vec<&[u8]> = Vec::new();
    for c in full.split(|&b| b == b'/') {
        match c {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    let mut norm = Vec::with_capacity(full.len());
    for c in &parts {
        norm.push(b'/');
        norm.extend_from_slice(c);
    }
    if norm.is_empty() {
        norm.push(b'/');
    }
    full = norm;
    if full.len() + 1 > n_out.max(0) as usize {
        return SQLITE_CANTOPEN;
    }
    std::ptr::copy_nonoverlapping(full.as_ptr(), out as *mut u8, full.len());
    *out.add(full.len()) = 0;
    SQLITE_OK
}

unsafe extern "C" fn vfs_dl_open(
    _: *mut sqlite3_vfs,
    name: *const ffi::c_char,
) -> *mut ffi::c_void {
    libc::dlopen(name, libc::RTLD_NOW | libc::RTLD_GLOBAL)
}

unsafe extern "C" fn vfs_dl_error(_: *mut sqlite3_vfs, n: ffi::c_int, out: *mut ffi::c_char) {
    if n <= 0 || out.is_null() {
        return;
    }
    let e = libc::dlerror();
    let msg: &[u8] = if e.is_null() {
        b""
    } else {
        CStr::from_ptr(e).to_bytes()
    };
    let len = msg.len().min(n as usize - 1);
    std::ptr::copy_nonoverlapping(msg.as_ptr(), out as *mut u8, len);
    *out.add(len) = 0;
}

unsafe extern "C" fn vfs_dl_sym(
    _: *mut sqlite3_vfs,
    handle: *mut ffi::c_void,
    sym: *const ffi::c_char,
) -> VfsFn {
    let p = libc::dlsym(handle, sym);
    if p.is_null() {
        None
    } else {
        Some(std::mem::transmute::<
            *mut ffi::c_void,
            unsafe extern "C" fn(),
        >(p))
    }
}

unsafe extern "C" fn vfs_dl_close(_: *mut sqlite3_vfs, handle: *mut ffi::c_void) {
    libc::dlclose(handle);
}

unsafe extern "C" fn vfs_randomness(
    _: *mut sqlite3_vfs,
    n: ffi::c_int,
    out: *mut ffi::c_char,
) -> ffi::c_int {
    let n = n.max(0) as usize;
    let got = libc::getrandom(out as *mut ffi::c_void, n, 0);
    if got < 0 {
        0
    } else {
        got as ffi::c_int
    }
}

unsafe extern "C" fn vfs_sleep(_: *mut sqlite3_vfs, us: ffi::c_int) -> ffi::c_int {
    std::thread::sleep(std::time::Duration::from_micros(us.max(0) as u64));
    us
}

unsafe extern "C" fn vfs_current_time_int64(_: *mut sqlite3_vfs, out: *mut i64) -> ffi::c_int {
    // Julian day number in milliseconds; 2440587.5 is the Unix epoch.
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    *out = 210_866_760_000_000i64 + ms;
    SQLITE_OK
}

unsafe extern "C" fn vfs_current_time(v: *mut sqlite3_vfs, out: *mut f64) -> ffi::c_int {
    let mut t = 0i64;
    vfs_current_time_int64(v, &mut t);
    *out = t as f64 / 86400000.0;
    SQLITE_OK
}

unsafe extern "C" fn vfs_get_last_error(
    _: *mut sqlite3_vfs,
    _: ffi::c_int,
    _: *mut ffi::c_char,
) -> ffi::c_int {
    errno()
}

unsafe extern "C" fn vfs_set_system_call(
    _: *mut sqlite3_vfs,
    _: *const ffi::c_char,
    _: VfsFn,
) -> ffi::c_int {
    SQLITE_NOTFOUND
}

unsafe extern "C" fn vfs_get_system_call(_: *mut sqlite3_vfs, _: *const ffi::c_char) -> VfsFn {
    None
}

unsafe extern "C" fn vfs_next_system_call(
    _: *mut sqlite3_vfs,
    _: *const ffi::c_char,
) -> *const ffi::c_char {
    std::ptr::null()
}

/// Writable because applications may link it into lists or patch fields,
/// as they can with SQLite's own unix VFS object.
static mut UNIX_VFS: sqlite3_vfs = sqlite3_vfs {
    i_version: 3,
    sz_os_file: std::mem::size_of::<UnixFile>() as ffi::c_int,
    mx_pathname: 512,
    p_next: std::ptr::null_mut(),
    z_name: c"unix".as_ptr(),
    p_app_data: std::ptr::null_mut(),
    x_open: Some(vfs_open),
    x_delete: Some(vfs_delete),
    x_access: Some(vfs_access),
    x_full_pathname: Some(vfs_full_pathname),
    x_dl_open: Some(vfs_dl_open),
    x_dl_error: Some(vfs_dl_error),
    x_dl_sym: Some(vfs_dl_sym),
    x_dl_close: Some(vfs_dl_close),
    x_randomness: Some(vfs_randomness),
    x_sleep: Some(vfs_sleep),
    x_current_time: Some(vfs_current_time),
    x_get_last_error: Some(vfs_get_last_error),
    x_current_time_int64: Some(vfs_current_time_int64),
    x_set_system_call: Some(vfs_set_system_call),
    x_get_system_call: Some(vfs_get_system_call),
    x_next_system_call: Some(vfs_next_system_call),
};

/// The built-in "unix" VFS object.
pub(crate) fn unix_vfs() -> *mut sqlite3_vfs {
    std::ptr::addr_of_mut!(UNIX_VFS)
}
