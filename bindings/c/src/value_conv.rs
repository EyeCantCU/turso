//! `sqlite3_value_*` accessors with SQLite's storage-class conversions
//! (https://sqlite.org/c3ref/column_blob.html). The void* is a
//! `turso_ext::Value`, as handed to a UDF.

use super::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use turso_ext::ValueType as T;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Enc {
    Utf8,
    Utf16,
    Utf16Le,
    Utf16Be,
}

thread_local! {
    /// Converted text of values, NUL-terminated, valid until the outermost
    /// UDF call returns. Keyed by the value's address and revalidated by
    /// content, since arguments of later calls reuse addresses.
    static BUFS: RefCell<HashMap<(usize, Enc), Vec<u8>>> = RefCell::new(HashMap::new());
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Scope of one UDF invocation; the buffers are released when the outermost ends.
pub(crate) struct UdfScope;

impl UdfScope {
    pub(crate) fn enter() -> Self {
        DEPTH.with(|d| d.set(d.get() + 1));
        UdfScope
    }
}

impl Drop for UdfScope {
    fn drop(&mut self) {
        let outermost = DEPTH.with(|d| {
            d.set(d.get().saturating_sub(1));
            d.get() == 0
        });
        if outermost {
            BUFS.with(|b| b.borrow_mut().clear());
        }
    }
}

fn forget(addr: usize) {
    BUFS.with(|b| b.borrow_mut().retain(|(a, _), _| *a != addr));
}

/// The bytes of the value as text or blob, or None for NULL.
fn raw_bytes(v: &ExtValue) -> Option<Vec<u8>> {
    match v.value_type() {
        T::Integer => Some(v.to_integer()?.to_string().into_bytes()),
        T::Float => Some(turso_core::format_float(v.to_float()?).into_bytes()),
        T::Text => v.to_text().map(|s| s.as_bytes().to_vec()),
        T::Blob => Some(v.blob_ref().map(<[u8]>::to_vec).unwrap_or_default()),
        T::Null | T::Error => None,
    }
}

fn encode(bytes: &[u8], enc: Enc) -> Vec<u8> {
    let mut out = match enc {
        Enc::Utf8 => bytes.to_vec(),
        _ => {
            let text = String::from_utf8_lossy(bytes);
            text.encode_utf16()
                .flat_map(|u| match enc {
                    Enc::Utf16Be => u.to_be_bytes(),
                    Enc::Utf16Le => u.to_le_bytes(),
                    _ => u.to_ne_bytes(),
                })
                .collect()
        }
    };
    let nul = if enc == Enc::Utf8 { 1 } else { 2 };
    out.resize(out.len() + nul, 0);
    out
}

/// Pointer to the NUL-terminated conversion and its length without the NUL.
unsafe fn converted(value: *mut ffi::c_void, enc: Enc) -> Option<(*const u8, usize)> {
    let v = &*(value as *const ExtValue);
    let fresh = encode(&raw_bytes(v)?, enc);
    let addr = value as usize;
    BUFS.with(|b| {
        let mut b = b.borrow_mut();
        let slot = b.entry((addr, enc)).or_default();
        if *slot != fresh {
            *slot = fresh;
        }
        let nul = if enc == Enc::Utf8 { 1 } else { 2 };
        Some((slot.as_ptr(), slot.len() - nul))
    })
}

fn as_text_value(bytes: &[u8]) -> Value {
    Value::build_text(String::from_utf8_lossy(bytes).into_owned())
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_int64(value: *mut ffi::c_void) -> i64 {
    if value.is_null() {
        return 0;
    }
    let v = &*(value as *const ExtValue);
    match v.value_type() {
        T::Integer => v.to_integer().unwrap_or(0),
        T::Float => v.to_float().unwrap_or(0.0) as i64,
        T::Text | T::Blob => {
            let Some(bytes) = raw_bytes(v) else { return 0 };
            match as_text_value(&bytes).exec_cast("INTEGER") {
                Ok(Value::Numeric(turso_core::Numeric::Integer(i))) => i,
                _ => 0,
            }
        }
        T::Null | T::Error => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_int(value: *mut ffi::c_void) -> ffi::c_int {
    sqlite3_value_int64(value) as ffi::c_int
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_double(value: *mut ffi::c_void) -> f64 {
    if value.is_null() {
        return 0.0;
    }
    let v = &*(value as *const ExtValue);
    match v.value_type() {
        T::Integer => v.to_integer().unwrap_or(0) as f64,
        T::Float => v.to_float().unwrap_or(0.0),
        T::Text | T::Blob => {
            let Some(bytes) = raw_bytes(v) else {
                return 0.0;
            };
            match as_text_value(&bytes).exec_cast("REAL") {
                Ok(Value::Numeric(turso_core::Numeric::Float(f))) => f64::from(f),
                Ok(Value::Numeric(turso_core::Numeric::Integer(i))) => i as f64,
                _ => 0.0,
            }
        }
        T::Null | T::Error => 0.0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_text(value: *mut ffi::c_void) -> *const ffi::c_uchar {
    if value.is_null() {
        return std::ptr::null();
    }
    converted(value, Enc::Utf8).map_or(std::ptr::null(), |(p, _)| p)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_text16(value: *mut ffi::c_void) -> *const ffi::c_void {
    if value.is_null() {
        return std::ptr::null();
    }
    converted(value, Enc::Utf16).map_or(std::ptr::null(), |(p, _)| p.cast())
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_text16le(value: *mut ffi::c_void) -> *const ffi::c_void {
    if value.is_null() {
        return std::ptr::null();
    }
    converted(value, Enc::Utf16Le).map_or(std::ptr::null(), |(p, _)| p.cast())
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_text16be(value: *mut ffi::c_void) -> *const ffi::c_void {
    if value.is_null() {
        return std::ptr::null();
    }
    converted(value, Enc::Utf16Be).map_or(std::ptr::null(), |(p, _)| p.cast())
}

/// The bytes of a text or blob value; a zero-length one reads as NULL.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_blob(value: *mut ffi::c_void) -> *const ffi::c_void {
    if value.is_null() {
        return std::ptr::null();
    }
    match converted(value, Enc::Utf8) {
        Some((p, n)) if n > 0 => p.cast(),
        _ => std::ptr::null(),
    }
}

/// Length in bytes of the UTF-8 text or blob form.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_bytes(value: *mut ffi::c_void) -> ffi::c_int {
    if value.is_null() {
        return 0;
    }
    converted(value, Enc::Utf8).map_or(0, |(_, n)| n) as ffi::c_int
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_bytes16(value: *mut ffi::c_void) -> ffi::c_int {
    if value.is_null() {
        return 0;
    }
    converted(value, Enc::Utf16).map_or(0, |(_, n)| n) as ffi::c_int
}

/// Frees a value allocated by `sqlite3_value_dup`.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_free(value: *mut ffi::c_void) {
    if value.is_null() {
        return;
    }
    forget(value as usize);
    let _ = Box::from_raw(value as *mut ExtValue);
}
