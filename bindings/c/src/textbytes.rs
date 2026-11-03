//! Text helpers for the C API.

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
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_encoding(value: *mut std::ffi::c_void) -> std::ffi::c_int {
    if value.is_null() {
        0
    } else {
        1
    }
}
