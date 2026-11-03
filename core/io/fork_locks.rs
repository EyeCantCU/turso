//! Drop inherited OFD locks in a forked child.
//!
//! Open-file-description locks belong to the description, which fork shares
//! with the child. A child that never touches the parent's database would
//! still keep the parent's tshm and interlock locks alive after the parent
//! exits (a reader slot stays pinned, SQLite stays locked out). Descriptors
//! that ever held an OFD lock are recorded here, and a `pthread_atfork` child
//! handler points each one at /dev/null, which drops the child's reference
//! to the description without disturbing the Rust objects that own the fd.

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
extern "C" fn in_child() {
    let mut null = -1;
    for slot in FDS.iter() {
        let fd = slot.swap(EMPTY, Ordering::AcqRel);
        if fd < 0 {
            continue;
        }
        if null < 0 {
            null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            if null < 0 {
                return;
            }
        }
        unsafe { libc::dup2(null, fd) };
    }
    if null >= 0 {
        unsafe { libc::close(null) };
    }
}
