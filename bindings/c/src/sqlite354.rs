//! C API added or extended between SQLite 3.50.4 and 3.54: sqlite3_incomplete,
//! sqlite3_set_errmsg, sqlite3_db_status64, the sqlite3_str family with
//! sqlite3_str_truncate/free and sqlite3_result_str, sqlite3_limit (with the
//! 3.53/3.54 limit ids) and SQLITE_DBCONFIG_FP_DIGITS.

use std::collections::HashMap;
use std::ffi::{self, CStr};
use std::sync::{Mutex, Weak};

use crate::{
    set_db_err_code, set_db_err_msg, sqlite3, sqlite3_result_error_nomem,
    sqlite3_result_error_toobig, SqliteContext, SQLITE_ERROR, SQLITE_MISUSE, SQLITE_NOMEM,
    SQLITE_OK, SQLITE_STATE_BUSY, SQLITE_STATE_OPEN, SQLITE_TOOBIG,
};

/// sqlite3SafetyCheckOk(): an open (not failed or closed) handle.
unsafe fn sqlite3_safety_check_ok(db: *mut sqlite3) -> bool {
    match (*db).inner.lock() {
        Ok(inner) => matches!(inner.e_open_state, SQLITE_STATE_OPEN | SQLITE_STATE_BUSY),
        Err(_) => false,
    }
}

pub const SQLITE_EMPTY: ffi::c_int = 16;

pub const SQLITE_DBSTATUS_MAX: ffi::c_int = 13;
pub const SQLITE_DBCONFIG_FP_DIGITS: ffi::c_int = 1023;
const DEFAULT_FP_DIGITS: ffi::c_int = 17;

pub const SQLITE_COPY: ffi::c_int = 0;
pub const SQLITE_XFER: ffi::c_int = 1;
pub const SQLITE_FINISH: ffi::c_int = 2;

/// SQLITE_LIMIT_LENGTH of `db`, or the hard maximum without a handle.
unsafe fn length_limit(db: *mut sqlite3) -> i64 {
    if db.is_null() || !sqlite3_safety_check_ok(db) {
        return 1_000_000_000;
    }
    misc::length_limit(&*db) as i64
}

/// SQLite's IdChar(): alphanumerics, '_', '$' and bytes >= 0x80.
fn id_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

/// Port of SQLite 3.54's sqlite3_incomplete() state machine (complete.c).
pub(crate) fn incomplete(sql: &[u8]) -> i64 {
    const SEMI: usize = 0;
    const WS: usize = 1;
    const OTHER: usize = 2;
    const EXPLAIN: usize = 3;
    const CREATE: usize = 4;
    const TEMP: usize = 5;
    const TRIGGER: usize = 6;
    const END: usize = 7;
    const TRANS: [[u8; 8]; 8] = [
        [1, 0, 2, 3, 4, 2, 2, 2],
        [1, 1, 2, 3, 4, 2, 2, 2],
        [1, 2, 2, 2, 2, 2, 2, 2],
        [1, 3, 3, 2, 4, 2, 2, 2],
        [1, 4, 2, 2, 2, 4, 5, 2],
        [6, 5, 5, 5, 5, 5, 5, 5],
        [6, 6, 5, 5, 5, 5, 5, 7],
        [1, 7, 5, 5, 5, 5, 5, 5],
    ];
    const STATEMAP: [u64; 8] = [0, 0, 1, 1, 1, 3, 2, 1];

    let at = |i: usize| sql.get(i).copied().unwrap_or(0);
    let mut state: u8 = 0;
    let mut pending: u8 = 0;
    let mut n_paren: i32 = 0;
    let mut i = 0;
    'scan: while at(i) != 0 {
        let token = match at(i) {
            b';' => SEMI,
            b' ' | b'\r' | b'\t' | b'\n' | 0x0c => WS,
            b'/' if at(i + 1) == b'*' => {
                i += 2;
                while at(i) != 0 && (at(i) != b'*' || at(i + 1) != b'/') {
                    i += 1;
                }
                if at(i) == 0 {
                    pending = b'/';
                    break 'scan;
                }
                i += 1;
                WS
            }
            b'-' if at(i + 1) == b'-' => {
                while at(i) != 0 && at(i) != b'\n' {
                    i += 1;
                }
                if at(i) == 0 {
                    pending = b'-';
                    break 'scan;
                }
                WS
            }
            b'[' => {
                i += 1;
                while at(i) != 0 && at(i) != b']' {
                    i += 1;
                }
                if at(i) == 0 {
                    pending = b']';
                    break 'scan;
                }
                OTHER
            }
            q @ (b'`' | b'"' | b'\'') => {
                i += 1;
                while at(i) != 0 && at(i) != q {
                    i += 1;
                }
                if at(i) == 0 {
                    pending = q;
                    break 'scan;
                }
                OTHER
            }
            b'(' => {
                n_paren += 1;
                OTHER
            }
            b')' => {
                n_paren -= 1;
                OTHER
            }
            c if id_char(c) => {
                let mut n = 1;
                while id_char(at(i + n)) {
                    n += 1;
                }
                let word = &sql[i..i + n];
                let tok = if word.eq_ignore_ascii_case(b"create") {
                    CREATE
                } else if word.eq_ignore_ascii_case(b"trigger") {
                    TRIGGER
                } else if word.eq_ignore_ascii_case(b"temp")
                    || word.eq_ignore_ascii_case(b"temporary")
                {
                    TEMP
                } else if word.eq_ignore_ascii_case(b"end") {
                    END
                } else if word.eq_ignore_ascii_case(b"explain") {
                    EXPLAIN
                } else {
                    OTHER
                };
                i += n - 1;
                tok
            }
            _ => OTHER,
        };
        state = TRANS[state as usize][token];
        i += 1;
    }
    if state == 0 && pending == 0 {
        return SQLITE_EMPTY as i64;
    }
    if state == 1 {
        n_paren = 0;
    }
    (((n_paren as u32 as u64) << 32)
        | ((pending as u64) << 16)
        | (STATEMAP[state as usize] << 8)
        | (state > 1 || pending != 0) as u64) as i64
}

/// Backend for sqlite3_complete().
pub(crate) unsafe fn complete(sql: *const ffi::c_char) -> ffi::c_int {
    if sql.is_null() {
        return 0;
    }
    (incomplete(CStr::from_ptr(sql).to_bytes()) == 0) as ffi::c_int
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_incomplete(sql: *const ffi::c_char) -> i64 {
    if sql.is_null() {
        return SQLITE_MISUSE as i64;
    }
    incomplete(CStr::from_ptr(sql).to_bytes())
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_complete16(sql: *const ffi::c_void) -> ffi::c_int {
    if sql.is_null() {
        return 0;
    }
    let p = sql as *const u16;
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
    (incomplete(s.as_bytes()) == 0) as ffi::c_int
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_set_errmsg(
    db: *mut sqlite3,
    errcode: ffi::c_int,
    msg: *const ffi::c_char,
) -> ffi::c_int {
    if db.is_null() || !sqlite3_safety_check_ok(db) {
        return SQLITE_MISUSE;
    }
    let mut inner = (*db).inner.lock().unwrap();
    if msg.is_null() {
        set_db_err_code(&mut inner, errcode);
    } else {
        let msg = CStr::from_ptr(msg).to_string_lossy();
        set_db_err_msg(&mut inner, errcode, &msg);
    }
    SQLITE_OK
}

/// Turso keeps no lookaside, cache or statement-memory accounting, so every
/// counter reads zero, as with sqlite3_db_status().
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_status64(
    db: *mut sqlite3,
    op: ffi::c_int,
    p_current: *mut i64,
    p_highwater: *mut i64,
    _reset_flag: ffi::c_int,
) -> ffi::c_int {
    if db.is_null() || p_current.is_null() || p_highwater.is_null() {
        return SQLITE_MISUSE;
    }
    if !(0..=SQLITE_DBSTATUS_MAX).contains(&op) {
        return SQLITE_ERROR;
    }
    *p_current = 0;
    *p_highwater = 0;
    SQLITE_OK
}

/// SQLITE_DBCONFIG_FP_DIGITS: Turso always renders REAL values with SQLite's
/// default of 17 digits, so only that value (or a query) is accepted.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn turso_db_config_fp_digits(
    db: *mut sqlite3,
    value: ffi::c_int,
    p_out: *mut ffi::c_int,
) -> ffi::c_int {
    if db.is_null() {
        return SQLITE_MISUSE;
    }
    if value > 3 && value < 24 && value != DEFAULT_FP_DIGITS {
        return SQLITE_ERROR;
    }
    if !p_out.is_null() {
        *p_out = DEFAULT_FP_DIGITS;
    }
    SQLITE_OK
}

/// sqlite3_str: a growable string with SQLite's length limit and sticky error.
pub struct sqlite3_str {
    buf: Vec<u8>,
    err: ffi::c_int,
    max: i64,
}

impl sqlite3_str {
    fn append(&mut self, bytes: &[u8]) {
        if self.err != SQLITE_OK || bytes.is_empty() {
            return;
        }
        if self.buf.len() as i64 + bytes.len() as i64 + 1 > self.max {
            self.buf = Vec::new();
            self.err = SQLITE_TOOBIG;
            return;
        }
        self.buf.extend_from_slice(bytes);
    }
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_str_new(db: *mut sqlite3) -> *mut sqlite3_str {
    Box::into_raw(Box::new(sqlite3_str {
        buf: Vec::new(),
        err: SQLITE_OK,
        max: length_limit(db),
    }))
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_str_append(
    p: *mut sqlite3_str,
    z: *const ffi::c_char,
    n: ffi::c_int,
) {
    if p.is_null() || z.is_null() || n <= 0 {
        return;
    }
    (*p).append(std::slice::from_raw_parts(z as *const u8, n as usize));
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_str_appendall(p: *mut sqlite3_str, z: *const ffi::c_char) {
    if p.is_null() || z.is_null() {
        return;
    }
    (*p).append(CStr::from_ptr(z).to_bytes());
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_str_appendchar(
    p: *mut sqlite3_str,
    n: ffi::c_int,
    c: ffi::c_char,
) {
    if p.is_null() || n <= 0 {
        return;
    }
    (*p).append(&vec![c as u8; n as usize]);
}

/// Appends (and frees) a string produced by turso_printf_va; used by
/// sqlite3_str_appendf/vappendf in sqlite354.c.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn turso_str_append_printf(p: *mut sqlite3_str, z: *mut ffi::c_char) {
    if z.is_null() {
        return;
    }
    if !p.is_null() {
        (*p).append(CStr::from_ptr(z).to_bytes());
    }
    libc::free(z as *mut ffi::c_void);
}

extern "C" {
    fn turso_sqlite3_str_appendf_va();
    fn turso_sqlite3_str_vappendf_va();
}

/// Variadic; tail-jumps to the C implementation (see sqlite3_db_config).
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_str_appendf(_p: *mut sqlite3_str, _fmt: *const ffi::c_char) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    core::arch::naked_asm!("jmp {}", sym turso_sqlite3_str_appendf_va);
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    core::arch::naked_asm!("b {}", sym turso_sqlite3_str_appendf_va);
}

/// Takes a va_list, which only C can consume; tail-jumps to sqlite354.c.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_str_vappendf(
    _p: *mut sqlite3_str,
    _fmt: *const ffi::c_char,
    _ap: *mut ffi::c_void,
) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    core::arch::naked_asm!("jmp {}", sym turso_sqlite3_str_vappendf_va);
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    core::arch::naked_asm!("b {}", sym turso_sqlite3_str_vappendf_va);
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_str_reset(p: *mut sqlite3_str) {
    if !p.is_null() {
        (*p).buf = Vec::new();
    }
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_str_truncate(p: *mut sqlite3_str, n: ffi::c_int) {
    if !p.is_null() && n >= 0 && (n as usize) < (*p).buf.len() {
        (*p).buf.truncate(n as usize);
    }
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_str_errcode(p: *mut sqlite3_str) -> ffi::c_int {
    if p.is_null() {
        return SQLITE_NOMEM;
    }
    (*p).err
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_str_length(p: *mut sqlite3_str) -> ffi::c_int {
    if p.is_null() {
        return 0;
    }
    (*p).buf.len() as ffi::c_int
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_str_value(p: *mut sqlite3_str) -> *mut ffi::c_char {
    if p.is_null() || (*p).buf.is_empty() {
        return std::ptr::null_mut();
    }
    let buf = &mut (*p).buf;
    buf.push(0);
    buf.pop();
    buf.as_mut_ptr() as *mut ffi::c_char
}

/// Copies the content into sqlite3_malloc'd memory; NULL when empty.
unsafe fn str_to_malloc(p: &sqlite3_str) -> *mut ffi::c_char {
    if p.buf.is_empty() {
        return std::ptr::null_mut();
    }
    let z = libc::malloc(p.buf.len() + 1) as *mut u8;
    if z.is_null() {
        return std::ptr::null_mut();
    }
    std::ptr::copy_nonoverlapping(p.buf.as_ptr(), z, p.buf.len());
    *z.add(p.buf.len()) = 0;
    z as *mut ffi::c_char
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_str_finish(p: *mut sqlite3_str) -> *mut ffi::c_char {
    if p.is_null() {
        return std::ptr::null_mut();
    }
    let s = Box::from_raw(p);
    str_to_malloc(&s)
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_str_free(p: *mut sqlite3_str) {
    if !p.is_null() {
        drop(Box::from_raw(p));
    }
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_str(
    context: *mut ffi::c_void,
    p: *mut sqlite3_str,
    e_own: ffi::c_int,
) {
    if context.is_null() || p.is_null() {
        return;
    }
    match (*p).err {
        SQLITE_OK => {
            let ctx = &mut *(context as *mut SqliteContext);
            let text = String::from_utf8_lossy(&(*p).buf).into_owned();
            ctx.result = turso_ext::Value::from_text(text);
            if e_own != SQLITE_COPY {
                (*p).buf = Vec::new();
            }
        }
        SQLITE_NOMEM => sqlite3_result_error_nomem(context),
        _ => sqlite3_result_error_toobig(context),
    }
    if e_own == SQLITE_FINISH {
        sqlite3_str_free(p);
    }
}

#[cfg(test)]
mod tests {
    use super::incomplete;

    #[test]
    fn incomplete_matches_sqlite() {
        assert_eq!(incomplete(b""), 16);
        assert_eq!(incomplete(b"  -- c\n"), 16);
        assert_eq!(incomplete(b"select 1;"), 0);
        assert_eq!(incomplete(b"select 1"), 0x101);
        assert_eq!(incomplete(b"select 'a"), (b'\'' as i64) << 16 | 0x101);
        assert_eq!(incomplete(b"select (1"), 1 << 32 | 0x101);
        assert_eq!(
            incomplete(b"create trigger t after insert on x begin select 1;"),
            0x201
        );
        assert_eq!(
            incomplete(b"create trigger t after insert on x begin select 1; end;"),
            0
        );
        assert_eq!(incomplete(b"/* x"), (b'/' as i64) << 16 | 1);
    }
}
