//! Panic safety net for exported C functions, applied by `#[capi_guard]`.
//! A panic would otherwise abort the host (`extern "C"` aborts on unwind), so
//! it becomes an error code and, where a handle exists, an errmsg.
//! Needs `panic = "unwind"`; with `panic = "abort"` it is inert.
use super::*;
use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use turso_macros::capi_guard;

pub(crate) const SQLITE_INTERNAL: ffi::c_int = super::SQLITE_INTERNAL;
pub(crate) static INTERNAL_MSG: &CStr = c"internal error";

/// Handle a function can record the error on.
pub(crate) enum Ctx {
    None,
    Db(*mut sqlite3),
    Stmt(*mut sqlite3_stmt),
}

/// Value returned after a panic when it is not a status code.
pub(crate) trait PanicRet {
    fn panic_ret() -> Self;
}

impl PanicRet for () {
    fn panic_ret() {}
}
impl PanicRet for ffi::c_int {
    fn panic_ret() -> Self {
        0
    }
}
impl PanicRet for i64 {
    fn panic_ret() -> Self {
        0
    }
}
impl PanicRet for f64 {
    fn panic_ret() -> Self {
        0.0
    }
}
impl<T> PanicRet for *const T {
    fn panic_ret() -> Self {
        std::ptr::null()
    }
}
impl<T> PanicRet for *mut T {
    fn panic_ret() -> Self {
        std::ptr::null_mut()
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    let what = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    format!("internal error: {what}")
}

/// Record the panic on the handle. Tolerates a poisoned lock; the handle
/// may be NULL or SICK.
unsafe fn record(ctx: &Ctx, msg: &str) {
    let db: Option<&sqlite3> = match *ctx {
        Ctx::None => None,
        Ctx::Db(p) => p.as_ref(),
        Ctx::Stmt(p) => p.as_ref().map(|s| &*s.db),
    };
    if let Some(db) = db {
        let mut inner = db.inner.lock().unwrap_or_else(|e| e.into_inner());
        let msg = CString::new(msg.replace('\0', " ")).unwrap_or_default();
        inner.p_err = Some(msg);
        inner.err_code = SQLITE_INTERNAL;
    }
}

/// Run `f`; on panic record it and return `fallback()`.
#[inline(always)]
pub(crate) fn run<R>(
    name: &str,
    ctx: Ctx,
    f: impl FnOnce() -> R,
    fallback: impl FnOnce() -> R,
) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            let msg = panic_message(&*payload);
            tracing::error!("panic in {name}: {msg}");
            // SAFETY: the C caller passed this handle to the same function.
            unsafe { record(&ctx, &msg) };
            fallback()
        }
    }
}

/// Test hook: panics, to exercise the guard on a handle.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn turso_test_panic(db: *mut sqlite3) -> ffi::c_int {
    let _ = db;
    panic!("injected panic");
}

/// Test hook: panics, on a statement.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn turso_test_panic_stmt(stmt: *mut sqlite3_stmt) -> ffi::c_int {
    let _ = stmt;
    panic!("injected panic");
}

/// Test hook: panics, in a pointer-returning function with no handle.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn turso_test_panic_ptr() -> *const ffi::c_char {
    panic!("injected panic");
}

/// Test hook: panics in an int function that returns a value.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn turso_test_panic_value(stmt: *mut sqlite3_stmt) -> ffi::c_int {
    let _ = stmt;
    panic!("injected panic");
}
