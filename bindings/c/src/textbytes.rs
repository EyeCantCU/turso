//! Text helpers for the C API.

use turso_macros::capi_guard;

/// The encoding a collation callback receives, as sqlite3_create_collation
/// resolves `eTextRep`: SQLITE_UTF16 and SQLITE_UTF16_ALIGNED mean native
/// UTF-16. None is SQLITE_MISUSE.
pub(crate) fn collation_encoding(enc: std::ffi::c_int) -> Option<u8> {
    const NATIVE_UTF16: u8 = if cfg!(target_endian = "little") { 2 } else { 3 };
    match enc {
        1..=3 => Some(enc as u8),
        4 | 8 => Some(NATIVE_UTF16),
        _ => None,
    }
}

/// The encoding a value's text is held in. Turso holds every value as UTF-8
/// and converts on demand, so this is always SQLITE_UTF8 (1).
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_encoding(value: *mut std::ffi::c_void) -> std::ffi::c_int {
    if value.is_null() {
        0
    } else {
        1
    }
}

/// The sqlite3_value for a column's UTF-8 text; in a UTF-16 database
/// (`encoding` 2 or 3) it also carries the stored UTF-16 bytes.
pub(crate) fn text_value(text: &str, encoding: u8) -> turso_ext::Value {
    match encoding {
        2 | 3 => {
            let be = encoding == 3;
            let utf16 = turso_core::utf8_to_utf16(text.as_bytes(), be);
            turso_ext::Value::from_text_with_utf16(text.to_string(), utf16, be)
        }
        _ => turso_ext::Value::from_text(text.to_string()),
    }
}
