//! sqlite3_autovacuum_pages.

use crate::{sqlite3, SQLITE_MISUSE, SQLITE_OK};
use std::ffi;
use std::sync::Arc;

type XAutovacPages = unsafe extern "C" fn(
    *mut ffi::c_void,
    *const ffi::c_char,
    ffi::c_uint,
    ffi::c_uint,
    ffi::c_uint,
) -> ffi::c_uint;
type XDestructor = unsafe extern "C" fn(*mut ffi::c_void);

/// The registered callback; runs the destructor when it is replaced or the
/// connection closes.
struct Registration {
    callback: Option<XAutovacPages>,
    arg: *mut ffi::c_void,
    destructor: Option<XDestructor>,
}

// SAFETY: SQLite hands the application's pointer back on whichever thread
// commits; the application owns its thread safety.
unsafe impl Send for Registration {}
unsafe impl Sync for Registration {}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(destructor) = self.destructor {
            unsafe { destructor(self.arg) };
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_autovacuum_pages(
    db: *mut sqlite3,
    callback: Option<XAutovacPages>,
    arg: *mut ffi::c_void,
    destructor: Option<XDestructor>,
) -> ffi::c_int {
    if db.is_null() {
        return SQLITE_MISUSE;
    }
    let Ok(inner) = (*db).inner.lock() else {
        return SQLITE_MISUSE;
    };
    let registration = Arc::new(Registration {
        callback,
        arg,
        destructor,
    });
    // Without a callback every free page is given back, as when none is set;
    // the registration is kept only so its destructor runs later.
    let hook: Option<turso_core::AutovacuumPagesFn> =
        if registration.callback.is_none() && registration.destructor.is_none() {
            None
        } else {
            Some(Arc::new(move |schema: &str, pages, free, page_size| {
                let Some(callback) = registration.callback else {
                    return free;
                };
                let schema = ffi::CString::new(schema).unwrap_or_default();
                unsafe { callback(registration.arg, schema.as_ptr(), pages, free, page_size) }
            }))
        };
    inner.conn.set_autovacuum_pages(hook);
    SQLITE_OK
}
