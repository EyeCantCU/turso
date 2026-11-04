//! SQLite pointer values (`sqlite3_bind_pointer`, `sqlite3_value_pointer`,
//! `sqlite3_result_pointer`): SQL NULLs that carry a typed pointer from a
//! bound parameter or a function result into a function argument.

use crate::sync::Arc;
use std::cell::RefCell;

/// A typed pointer. The destructor runs when the last reference is dropped.
pub struct PointerValue {
    pub ptr: usize,
    pub ty: Vec<u8>,
    pub destructor: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
}

impl std::fmt::Debug for PointerValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PointerValue({:#x}, {:?})", self.ptr, self.ty)
    }
}

impl Drop for PointerValue {
    fn drop(&mut self) {
        if let Some(d) = self.destructor {
            // SAFETY: the destructor was supplied with this pointer by the caller.
            unsafe { d(self.ptr as *mut std::ffi::c_void) };
        }
    }
}

/// Pointer arguments of one function call: `base` is the address of the C
/// argument array (0 for native functions).
struct Frame {
    base: usize,
    args: Vec<Option<Arc<PointerValue>>>,
}

thread_local! {
    static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
    static RESULT: RefCell<Option<Arc<PointerValue>>> = const { RefCell::new(None) };
}

/// Makes `args` visible to the function being called until dropped.
pub(crate) struct CallScope;

impl CallScope {
    pub(crate) fn enter(base: usize, args: &[Option<Arc<PointerValue>>]) -> Self {
        FRAMES.with(|f| {
            f.borrow_mut().push(Frame {
                base,
                args: args.to_vec(),
            })
        });
        CallScope
    }
}

impl Drop for CallScope {
    fn drop(&mut self) {
        let _ = FRAMES.try_with(|f| f.borrow_mut().pop());
    }
}

fn arg_of(frame: &Frame, i: usize, ty: &[u8]) -> Option<usize> {
    frame
        .args
        .get(i)?
        .as_ref()
        .filter(|p| p.ty == ty)
        .map(|p| p.ptr)
}

/// The pointer of argument `i` of the innermost function call, if it has
/// type `ty`.
pub fn arg_pointer(i: usize, ty: &[u8]) -> Option<usize> {
    FRAMES.with(|f| arg_of(f.borrow().last()?, i, ty))
}

/// The pointer carried by the C argument value at `addr`, if it has type
/// `ty` (`sqlite3_value_pointer`).
pub fn arg_pointer_at(addr: usize, ty: &[u8]) -> Option<usize> {
    let size = std::mem::size_of::<turso_ext::Value>();
    FRAMES.with(|f| {
        let f = f.borrow();
        let frame = f.last()?;
        if frame.base == 0 || addr < frame.base || (addr - frame.base) % size != 0 {
            return None;
        }
        arg_of(frame, (addr - frame.base) / size, ty)
    })
}

/// Makes the current function's (NULL) result carry a pointer
/// (`sqlite3_result_pointer`).
pub fn set_result_pointer(
    ptr: usize,
    ty: &[u8],
    destructor: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
) {
    let p = Arc::new(PointerValue {
        ptr,
        ty: ty.to_vec(),
        destructor,
    });
    RESULT.with(|r| *r.borrow_mut() = Some(p));
}

pub(crate) fn take_result() -> Option<Arc<PointerValue>> {
    RESULT.with(|r| r.borrow_mut().take())
}
