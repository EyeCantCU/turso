//! Drop inherited OFD locks in a forked child.
//!
//! Open-file-description locks belong to the description, which fork shares
//! with the child. A child that never touches the parent's database would
//! still keep the parent's tshm and interlock locks alive after the parent
//! exits (a reader slot stays pinned, SQLite stays locked out). Descriptors
//! that ever held an OFD lock are recorded here, and a `pthread_atfork` child
//! handler points each one at a fresh open of the same file (or /dev/null),
//! which drops the child's reference to the description without disturbing
//! the Rust objects that own the fd.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Once;

const SLOTS: usize = 1024;
const EMPTY: i32 = -1;

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY_SLOT: AtomicI32 = AtomicI32::new(EMPTY);
static FDS: [AtomicI32; SLOTS] = [EMPTY_SLOT; SLOTS];
static INSTALL: Once = Once::new();

/// Record that `fd` holds OFD locks.
pub(crate) fn remember(fd: i32) {
    INSTALL.call_once(|| unsafe {
        libc::pthread_atfork(None, None, Some(in_child));
    });
    if FDS.iter().any(|slot| slot.load(Ordering::Acquire) == fd) {
        return;
    }
    for slot in FDS.iter() {
        if slot
            .compare_exchange(EMPTY, fd, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return;
        }
    }
}

/// Forget `fd` before it is closed.
pub(crate) fn forget(fd: i32) {
    for slot in FDS.iter() {
        let _ = slot.compare_exchange(fd, EMPTY, Ordering::AcqRel, Ordering::Acquire);
    }
}

/// Runs in the child right after fork: only async-signal-safe calls.
///
/// Each descriptor is pointed at a fresh open of the same file, a new open
/// file description without the parent's locks, so a child that keeps using
/// the connection (some daemons fork workers after opening) still reads the
/// database. Files that cannot be reopened get /dev/null.
extern "C" fn in_child() {
    for slot in FDS.iter() {
        let fd = slot.swap(EMPTY, Ordering::AcqRel);
        if fd < 0 {
            continue;
        }
        unsafe {
            let fd_flags = libc::fcntl(fd, libc::F_GETFD);
            let fl = libc::fcntl(fd, libc::F_GETFL);
            let mut link = [0u8; 64];
            let mut path = [0u8; 4096];
            let fresh = if fl >= 0 && proc_fd_path(fd, &mut link) {
                let n = libc::readlink(
                    link.as_ptr() as *const libc::c_char,
                    path.as_mut_ptr() as *mut libc::c_char,
                    path.len() - 1,
                );
                if n > 0 {
                    path[n as usize] = 0;
                    libc::open(
                        path.as_ptr() as *const libc::c_char,
                        (fl & libc::O_ACCMODE) | libc::O_CLOEXEC,
                    )
                } else {
                    -1
                }
            } else {
                -1
            };
            let fresh = if fresh >= 0 {
                fresh
            } else {
                libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC)
            };
            if fresh < 0 {
                continue;
            }
            libc::dup2(fresh, fd);
            libc::close(fresh);
            if fd_flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, fd_flags);
            }
        }
    }
}

/// "/proc/self/fd/<fd>" into `buf`, NUL-terminated, without allocating.
fn proc_fd_path(fd: i32, buf: &mut [u8; 64]) -> bool {
    let prefix = b"/proc/self/fd/";
    buf[..prefix.len()].copy_from_slice(prefix);
    let mut digits = [0u8; 12];
    let mut n = 0;
    let mut v = fd as u32;
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let mut at = prefix.len();
    for i in (0..n).rev() {
        buf[at] = digits[i];
        at += 1;
    }
    buf[at] = 0;
    true
}
