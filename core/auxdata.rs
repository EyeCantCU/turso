//! Function auxiliary data (`sqlite3_get_auxdata` / `sqlite3_set_auxdata`):
//! per-statement values cached for a function argument, keyed like SQLite's
//! `AuxData` by the calling instruction and the argument index.

use std::cell::RefCell;

pub type AuxDestructor = unsafe extern "C" fn(*mut std::ffi::c_void);

struct AuxData {
    op: usize,
    arg: i32,
    ptr: usize,
    destructor: Option<AuxDestructor>,
}

impl Drop for AuxData {
    fn drop(&mut self) {
        if let Some(d) = self.destructor {
            // SAFETY: the destructor was supplied with this pointer.
            unsafe { d(self.ptr as *mut std::ffi::c_void) };
        }
    }
}

/// The auxiliary data of one statement execution.
#[derive(Default)]
pub(crate) struct AuxList(Vec<AuxData>);

impl std::fmt::Debug for AuxList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AuxList({})", self.0.len())
    }
}

impl AuxList {
    /// Runs every destructor (SQLite does this when the VM halts or resets).
    pub(crate) fn clear(&mut self) {
        let old = std::mem::take(&mut self.0);
        drop(old);
    }

    /// `sqlite3VdbeDeleteAuxData` for one instruction: drops entries whose
    /// argument is not constant according to `mask`.
    fn delete_for_op(&mut self, op: usize, mask: u32) {
        let mut dropped = Vec::new();
        let mut i = 0;
        while i < self.0.len() {
            let a = &self.0[i];
            if a.op == op && a.arg >= 0 && (a.arg > 31 || mask & (1u32 << a.arg) == 0) {
                dropped.push(self.0.remove(i));
            } else {
                i += 1;
            }
        }
        drop(dropped);
    }
}

struct Frame {
    list: *mut AuxList,
    op: usize,
    created: bool,
}

thread_local! {
    static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

/// Makes `list` the auxiliary data of the function called by instruction
/// `op` until `finish`.
pub(crate) struct AuxScope {
    list: *mut AuxList,
    op: usize,
    mask: u32,
}

impl AuxScope {
    /// # Safety
    /// `list` must stay valid and otherwise unused until `finish` or drop.
    pub(crate) unsafe fn enter(list: *mut AuxList, op: usize, mask: u32) -> Self {
        FRAMES.with(|f| {
            f.borrow_mut().push(Frame {
                list,
                op,
                created: false,
            })
        });
        AuxScope { list, op, mask }
    }

    /// Ends the call. As in OP_Function, when the call created an entry or
    /// failed, entries for non-constant arguments are dropped.
    pub(crate) fn finish(self, failed: bool) {
        let created = FRAMES
            .try_with(|f| f.borrow_mut().pop().is_some_and(|fr| fr.created))
            .unwrap_or(false);
        if created || failed {
            // SAFETY: guaranteed by `enter`.
            unsafe { (*self.list).delete_for_op(self.op, self.mask) };
        }
        std::mem::forget(self);
    }
}

impl Drop for AuxScope {
    fn drop(&mut self) {
        let _ = FRAMES.try_with(|f| f.borrow_mut().pop());
    }
}

/// `sqlite3_get_auxdata` for the function being called. A negative `arg` is
/// shared by every function of the statement.
pub fn get_auxdata(arg: i32) -> Option<usize> {
    FRAMES.with(|f| {
        let f = f.borrow();
        let fr = f.last()?;
        // SAFETY: the frame's list is valid while the frame is on the stack.
        let list = unsafe { &*fr.list };
        list.0
            .iter()
            .find(|a| a.arg == arg && (a.op == fr.op || arg < 0))
            .map(|a| a.ptr)
    })
}

/// `sqlite3_set_auxdata`. Without a function call in progress the
/// destructor runs at once.
pub fn set_auxdata(arg: i32, ptr: usize, destructor: Option<AuxDestructor>) {
    let old = FRAMES.with(|f| {
        let mut f = f.borrow_mut();
        let Some(fr) = f.last_mut() else {
            return Some(AuxData {
                op: 0,
                arg,
                ptr,
                destructor,
            });
        };
        // SAFETY: the frame's list is valid while the frame is on the stack.
        let list = unsafe { &mut *fr.list };
        let op = fr.op;
        match list
            .0
            .iter_mut()
            .find(|a| a.arg == arg && (a.op == op || arg < 0))
        {
            Some(a) => Some(std::mem::replace(
                a,
                AuxData {
                    op: a.op,
                    arg,
                    ptr,
                    destructor,
                },
            )),
            None => {
                list.0.push(AuxData {
                    op,
                    arg,
                    ptr,
                    destructor,
                });
                fr.created = true;
                None
            }
        }
    });
    // Destructors run after the borrow ends: they may call back into SQLite.
    drop(old);
}
