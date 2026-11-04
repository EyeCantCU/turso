//! Pointer values: sqlite3_bind_pointer, sqlite3_value_pointer and
//! sqlite3_result_pointer, on core/pointer.rs.
//! Same as the pointer section of compat/fts5's bindings/c/src/fts5.rs;
//! keep one copy when both are merged.

use std::ffi::{self, CStr};

use crate::{sqlite3_stmt, SQLITE_MISUSE};

extern "C" {
    fn sqlite3_result_null(ctx: *mut ffi::c_void);
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_pointer(
    stmt: *mut sqlite3_stmt,
    idx: ffi::c_int,
    p: *mut ffi::c_void,
    ty: *const ffi::c_char,
    destructor: Option<unsafe extern "C" fn(*mut ffi::c_void)>,
) -> ffi::c_int {
    if stmt.is_null() {
        return SQLITE_MISUSE;
    }
    let s = &mut *stmt;
    let Some(index) = crate::sqlite3_bind_index_in_range(s, idx) else {
        if let Some(d) = destructor {
            d(p);
        }
        return crate::SQLITE_RANGE;
    };
    let ty = if ty.is_null() {
        &[][..]
    } else {
        CStr::from_ptr(ty).to_bytes()
    };
    let result = s.stmt.bind_pointer(index, p as usize, ty, destructor);
    crate::sqlite3_bind_result(s, result)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_pointer(
    value: *mut ffi::c_void,
    ty: *const ffi::c_char,
) -> *mut ffi::c_void {
    if value.is_null() || ty.is_null() {
        return std::ptr::null_mut();
    }
    turso_core::pointer::arg_pointer_at(value as usize, CStr::from_ptr(ty).to_bytes())
        .map_or(std::ptr::null_mut(), |p| p as *mut ffi::c_void)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_pointer(
    ctx: *mut ffi::c_void,
    p: *mut ffi::c_void,
    ty: *const ffi::c_char,
    destructor: Option<unsafe extern "C" fn(*mut ffi::c_void)>,
) {
    if ctx.is_null() {
        return;
    }
    sqlite3_result_null(ctx);
    let ty = if ty.is_null() {
        &[][..]
    } else {
        CStr::from_ptr(ty).to_bytes()
    };
    turso_core::pointer::set_result_pointer(p as usize, ty, destructor);
}
