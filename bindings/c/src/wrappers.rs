//! Thin SQLite C API wrappers and small leaf functions.

use super::*;
use turso_core::{EqpFormat, QueryMode};

const SQLITE_UTF8: ffi::c_int = 1;

/// SQLITE_STATIC is 0, SQLITE_TRANSIENT is -1; any other value is a real
/// destructor that SQLite would invoke.
fn is_real_destructor(d: *mut ffi::c_void) -> bool {
    !d.is_null() && d as isize != -1
}

type Destructor = unsafe extern "C" fn(*mut ffi::c_void);

// ---------------------------------------------------------------------------
// Legacy entry points: forward to the _v2 variants.
// ---------------------------------------------------------------------------

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_function(
    db: *mut sqlite3,
    name: *const ffi::c_char,
    n_args: ffi::c_int,
    enc: ffi::c_int,
    context: *mut ffi::c_void,
    func: Option<unsafe extern "C" fn()>,
    step: Option<unsafe extern "C" fn()>,
    final_: Option<unsafe extern "C" fn()>,
) -> ffi::c_int {
    sqlite3_create_function_v2(db, name, n_args, enc, context, func, step, final_, None)
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_collation(
    db: *mut sqlite3,
    name: *const ffi::c_char,
    enc: ffi::c_int,
    context: *mut ffi::c_void,
    cmp: Option<unsafe extern "C" fn() -> ffi::c_int>,
) -> ffi::c_int {
    sqlite3_create_collation_v2(db, name, enc, context, cmp, None)
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_prepare(
    db: *mut sqlite3,
    sql: *const ffi::c_char,
    n_byte: ffi::c_int,
    out_stmt: *mut *mut sqlite3_stmt,
    tail: *mut *const ffi::c_char,
) -> ffi::c_int {
    sqlite3_prepare_v2(db, sql, n_byte, out_stmt, tail)
}

// ---------------------------------------------------------------------------
// va_list printf variants. The exported symbols are tail-jumping trampolines
// (rustc only exports Rust items from cdylibs); the implementations are the C
// functions in varargs.c, which receive the caller's arguments untouched.
// ---------------------------------------------------------------------------

extern "C" {
    fn turso_sqlite3_vmprintf_va();
    fn turso_sqlite3_vsnprintf_va();
}

/// `char *sqlite3_vmprintf(const char *fmt, va_list ap)`. `ap` is declared as
/// an opaque pointer-sized argument; the real va_list handling is in C.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_vmprintf(
    _fmt: *const ffi::c_char,
    _ap: *mut ffi::c_void,
) -> *mut ffi::c_char {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    core::arch::naked_asm!("jmp {}", sym turso_sqlite3_vmprintf_va);
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    core::arch::naked_asm!("b {}", sym turso_sqlite3_vmprintf_va);
}

/// `char *sqlite3_vsnprintf(int n, char *buf, const char *fmt, va_list ap)`.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_vsnprintf(
    _n: ffi::c_int,
    _buf: *mut ffi::c_char,
    _fmt: *const ffi::c_char,
    _ap: *mut ffi::c_void,
) -> *mut ffi::c_char {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    core::arch::naked_asm!("jmp {}", sym turso_sqlite3_vsnprintf_va);
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    core::arch::naked_asm!("b {}", sym turso_sqlite3_vsnprintf_va);
}

// ---------------------------------------------------------------------------
// 64-bit length variants.
// ---------------------------------------------------------------------------

const SQLITE_UTF16LE: ffi::c_int = 2;
const SQLITE_UTF16BE: ffi::c_int = 3;
const SQLITE_UTF16: ffi::c_int = 4;

/// SQLITE_LIMIT_LENGTH default (SQLITE_MAX_LENGTH).
const MAX_LENGTH: u64 = 1_000_000_000;

fn release(d: Option<Destructor>, p: *const ffi::c_void) {
    if let Some(d) = d {
        if is_real_destructor(d as *mut ffi::c_void) && !p.is_null() {
            // SAFETY: caller-provided destructor for caller-provided pointer.
            unsafe { d(p as *mut ffi::c_void) };
        }
    }
}

/// UTF-16 text to UTF-8; an odd trailing byte is dropped and unpaired
/// surrogates become U+FFFD, as in SQLite.
unsafe fn utf16_to_string(text: *const ffi::c_char, len: u64, enc: ffi::c_int) -> String {
    let n = (len & !1) as usize;
    let bytes = if n == 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(text as *const u8, n)
    };
    let big = enc == SQLITE_UTF16BE || (enc == SQLITE_UTF16 && cfg!(target_endian = "big"));
    let units = bytes.chunks_exact(2).map(|c| {
        if big {
            u16::from_be_bytes([c[0], c[1]])
        } else {
            u16::from_le_bytes([c[0], c[1]])
        }
    });
    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// UTF-8, UTF-16LE/BE and native UTF-16 are accepted (the latter three are
/// transcoded). On failure the destructor still runs, as in SQLite.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_text64(
    stmt: *mut sqlite3_stmt,
    idx: ffi::c_int,
    text: *const ffi::c_char,
    len: u64,
    destructor: Option<Destructor>,
    encoding: ffi::c_uchar,
) -> ffi::c_int {
    if stmt.is_null() {
        release(destructor, text.cast());
        return SQLITE_MISUSE;
    }
    // SQLITE_UTF8_ZT (16) is UTF-8 with a terminator.
    let enc = match encoding as ffi::c_int {
        16 => SQLITE_UTF8,
        e => e,
    };
    if !matches!(
        enc,
        SQLITE_UTF8 | SQLITE_UTF16LE | SQLITE_UTF16BE | SQLITE_UTF16
    ) {
        release(destructor, text.cast());
        return SQLITE_MISUSE;
    }
    if len > MAX_LENGTH {
        release(destructor, text.cast());
        return SQLITE_TOOBIG;
    }
    if enc == SQLITE_UTF8 {
        return sqlite3_bind_text(stmt, idx, text, len as ffi::c_int, destructor);
    }
    if text.is_null() {
        return sqlite3_bind_null(stmt, idx);
    }
    let converted = utf16_to_string(text, len, enc);
    let transient = std::mem::transmute::<usize, Destructor>(usize::MAX);
    let rc = sqlite3_bind_text(
        stmt,
        idx,
        converted.as_ptr().cast(),
        converted.len() as ffi::c_int,
        Some(transient),
    );
    // The converted copy is bound, so the caller's buffer can go now.
    release(destructor, text.cast());
    rc
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_blob64(
    stmt: *mut sqlite3_stmt,
    idx: ffi::c_int,
    blob: *const ffi::c_void,
    len: u64,
    destructor: Option<Destructor>,
) -> ffi::c_int {
    if stmt.is_null() {
        release(destructor, blob);
        return SQLITE_MISUSE;
    }
    if len > MAX_LENGTH {
        release(destructor, blob);
        return SQLITE_TOOBIG;
    }
    sqlite3_bind_blob(stmt, idx, blob, len as ffi::c_int, destructor)
}

/// Same encodings as `sqlite3_bind_text64`. The context copies the text, so
/// the destructor is run before returning.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_text64(
    context: *mut ffi::c_void,
    text: *const ffi::c_char,
    len: u64,
    destroy: Option<Destructor>,
    encoding: ffi::c_uchar,
) {
    // SQLITE_UTF8_ZT (16) is UTF-8 with a terminator.
    let enc = match encoding as ffi::c_int {
        16 => SQLITE_UTF8,
        e => e,
    };
    if context.is_null() {
        release(destroy, text.cast());
        return;
    }
    if !matches!(
        enc,
        SQLITE_UTF8 | SQLITE_UTF16LE | SQLITE_UTF16BE | SQLITE_UTF16
    ) {
        release(destroy, text.cast());
        sqlite3_result_error(context, c"unsupported text encoding".as_ptr(), -1);
        return;
    }
    if len > MAX_LENGTH {
        release(destroy, text.cast());
        sqlite3_result_error_toobig(context);
        return;
    }
    if text.is_null() {
        sqlite3_result_null(context);
        return;
    }
    if enc == SQLITE_UTF8 {
        // sqlite3_result_text releases the buffer itself.
        let destroy = std::mem::transmute::<
            Option<Destructor>,
            Option<extern "C" fn(*mut ffi::c_void)>,
        >(destroy);
        sqlite3_result_text(context, text, len as ffi::c_int, destroy);
        return;
    }
    let ctx = &mut *(context as *mut SqliteContext);
    ctx.result = ExtValue::from_text(utf16_to_string(text, len, enc));
    release(destroy, text.cast());
}

// ---------------------------------------------------------------------------
// Memory.
// ---------------------------------------------------------------------------

/// Like SQLite: n <= 0 frees the block and returns NULL.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_realloc(ptr: *mut ffi::c_void, n: ffi::c_int) -> *mut ffi::c_void {
    sqlite3_realloc64(ptr, if n < 0 { 0 } else { n as u64 })
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_realloc64(ptr: *mut ffi::c_void, n: u64) -> *mut ffi::c_void {
    if n == 0 {
        sqlite3_free(ptr);
        return std::ptr::null_mut();
    }
    if n > isize::MAX as u64 {
        return std::ptr::null_mut();
    }
    libc::realloc(ptr, n as usize)
}

/// Always reports 0 bytes freed: turso keeps no per-connection page cache that
/// can be shrunk on request.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_release_memory(db: *mut sqlite3) -> ffi::c_int {
    if db.is_null() {
        return SQLITE_MISUSE;
    }
    SQLITE_OK
}

// ---------------------------------------------------------------------------
// Version identification.
// ---------------------------------------------------------------------------

const SQLITE_VERSION_LEN: usize = SQLITE_VERSION.len() + 1;

const fn version_bytes() -> [u8; SQLITE_VERSION_LEN] {
    let src = SQLITE_VERSION.as_bytes();
    let mut out = [0u8; SQLITE_VERSION_LEN];
    let mut i = 0;
    while i < src.len() {
        out[i] = src[i];
        i += 1;
    }
    out
}

/// `const char sqlite3_version[]`: exact size (strlen + 1) matters for copy
/// relocations in non-PIE executables.
#[no_mangle]
#[allow(non_upper_case_globals)]
pub static sqlite3_version: [u8; SQLITE_VERSION_LEN] = version_bytes();

/// "YYYY-MM-DD HH:MM:SS <64 hex>" like SQLite's, but naming the Turso git
/// commit this library was built from (set by build.rs).
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_sourceid() -> *const ffi::c_char {
    concat!(env!("TURSO_SOURCE_ID"), "\0").as_ptr().cast()
}

// ---------------------------------------------------------------------------
// Keywords. SQLite 3.54's keyword table, in SQLite's own index order.
// ---------------------------------------------------------------------------

const SQLITE_KEYWORDS: [&str; 147] = [
    "REINDEX",
    "INDEXED",
    "INDEX",
    "DESC",
    "ESCAPE",
    "EACH",
    "CHECK",
    "KEY",
    "BEFORE",
    "FOREIGN",
    "FOR",
    "IGNORE",
    "REGEXP",
    "EXPLAIN",
    "INSTEAD",
    "ADD",
    "DATABASE",
    "AS",
    "SELECT",
    "TABLE",
    "LEFT",
    "THEN",
    "END",
    "DEFERRABLE",
    "ELSE",
    "EXCLUDE",
    "DELETE",
    "TEMPORARY",
    "TEMP",
    "OR",
    "ISNULL",
    "NULLS",
    "SAVEPOINT",
    "INTERSECT",
    "TIES",
    "NOTNULL",
    "NOT",
    "NO",
    "NULL",
    "LIKE",
    "EXCEPT",
    "TRANSACTION",
    "ACTION",
    "ON",
    "NATURAL",
    "ALTER",
    "RAISE",
    "EXCLUSIVE",
    "EXISTS",
    "CONSTRAINT",
    "INTO",
    "OFFSET",
    "OF",
    "SET",
    "TRIGGER",
    "RANGE",
    "GENERATED",
    "DETACH",
    "HAVING",
    "GLOB",
    "BEGIN",
    "INNER",
    "REFERENCES",
    "UNIQUE",
    "QUERY",
    "WITHOUT",
    "WITH",
    "OUTER",
    "RELEASE",
    "ATTACH",
    "BETWEEN",
    "NOTHING",
    "GROUPS",
    "GROUP",
    "CASCADE",
    "ASC",
    "DEFAULT",
    "CASE",
    "COLLATE",
    "CREATE",
    "CURRENT_DATE",
    "IMMEDIATE",
    "JOIN",
    "INSERT",
    "MATCH",
    "PLAN",
    "ANALYZE",
    "PRAGMA",
    "MATERIALIZED",
    "DEFERRED",
    "DISTINCT",
    "IS",
    "UPDATE",
    "VALUES",
    "VIRTUAL",
    "ALWAYS",
    "WHEN",
    "WHERE",
    "RECURSIVE",
    "ABORT",
    "AFTER",
    "RENAME",
    "AND",
    "DROP",
    "PARTITION",
    "AUTOINCREMENT",
    "TO",
    "IN",
    "CAST",
    "COLUMN",
    "COMMIT",
    "CONFLICT",
    "CROSS",
    "CURRENT_TIMESTAMP",
    "CURRENT_TIME",
    "CURRENT",
    "PRECEDING",
    "FAIL",
    "LAST",
    "FILTER",
    "REPLACE",
    "FIRST",
    "FOLLOWING",
    "FROM",
    "FULL",
    "LIMIT",
    "IF",
    "ORDER",
    "RESTRICT",
    "OTHERS",
    "OVER",
    "RETURNING",
    "RIGHT",
    "ROLLBACK",
    "ROWS",
    "ROW",
    "UNBOUNDED",
    "UNION",
    "USING",
    "VACUUM",
    "VIEW",
    "WINDOW",
    "DO",
    "BY",
    "INITIALLY",
    "ALL",
    "PRIMARY",
];

#[capi_guard]
#[no_mangle]
pub extern "C" fn sqlite3_keyword_count() -> ffi::c_int {
    SQLITE_KEYWORDS.len() as ffi::c_int
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_keyword_name(
    i: ffi::c_int,
    name: *mut *const ffi::c_char,
    len: *mut ffi::c_int,
) -> ffi::c_int {
    if i < 0 || i as usize >= SQLITE_KEYWORDS.len() {
        return SQLITE_ERROR;
    }
    if name.is_null() || len.is_null() {
        return SQLITE_MISUSE;
    }
    let k = SQLITE_KEYWORDS[i as usize];
    *name = k.as_ptr() as *const ffi::c_char;
    *len = k.len() as ffi::c_int;
    SQLITE_OK
}

/// `sqlite3_keyword_check` is a sibling that consumers use alongside the
/// above; case-insensitive membership test.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_keyword_check(
    word: *const ffi::c_char,
    n: ffi::c_int,
) -> ffi::c_int {
    if word.is_null() || n < 0 {
        return 0;
    }
    let bytes = std::slice::from_raw_parts(word as *const u8, n as usize);
    SQLITE_KEYWORDS
        .iter()
        .any(|k| k.as_bytes().eq_ignore_ascii_case(bytes)) as ffi::c_int
}

// ---------------------------------------------------------------------------
// sqlite3_result_* / sqlite3_value_* additions.
// ---------------------------------------------------------------------------

/// Copies any sqlite3_value (function argument or column value) as the result.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_value(context: *mut ffi::c_void, value: *mut ffi::c_void) {
    if context.is_null() {
        return;
    }
    let copy = sqlite3_value_dup(value);
    if copy.is_null() {
        sqlite3_result_null(context);
        return;
    }
    let ctx = &mut *(context as *mut SqliteContext);
    ctx.result = *Box::from_raw(copy as *mut ExtValue);
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_zeroblob(context: *mut ffi::c_void, n: ffi::c_int) {
    if context.is_null() {
        return;
    }
    if n as u64 > MAX_LENGTH && n > 0 {
        sqlite3_result_error_toobig(context);
        return;
    }
    let ctx = &mut *(context as *mut SqliteContext);
    ctx.result = ExtValue::from_blob(vec![0u8; n.max(0) as usize]);
}

/// SQLITE_TOOBIG (and a too-big error result) above the length limit.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_zeroblob64(
    context: *mut ffi::c_void,
    n: u64,
) -> ffi::c_int {
    if context.is_null() {
        return SQLITE_MISUSE;
    }
    if n > MAX_LENGTH {
        sqlite3_result_error_toobig(context);
        return SQLITE_TOOBIG;
    }
    sqlite3_result_zeroblob(context, n as ffi::c_int);
    SQLITE_OK
}

/// Applies NUMERIC affinity to a TEXT value in place, as SQLite does: a
/// complete numeric literal becomes INTEGER or FLOAT; anything else keeps
/// its type.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_numeric_type(value: *mut ffi::c_void) -> ffi::c_int {
    let t = sqlite3_value_type(value);
    if t != SQLITE_TEXT || value.is_null() {
        return t;
    }
    let v = &mut *(value as *mut ExtValue);
    let Some(s) = v.to_text().filter(|_| v.is_text_utf8()) else {
        return t;
    };
    let s = s.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'));
    let numeric = if let Ok(i) = s.parse::<i64>() {
        Some(ExtValue::from_integer(i))
    } else if s.bytes().any(|b| b.is_ascii_digit())
        // Rust's f64 parser also accepts inf/nan, which SQLite does not.
        && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'+' | b'-' | b'.' | b'e' | b'E'))
    {
        s.parse::<f64>().ok().map(ExtValue::from_float)
    } else {
        None
    };
    match numeric {
        Some(n) => {
            *v = n;
            sqlite3_value_type(value)
        }
        None => t,
    }
}

// ---------------------------------------------------------------------------
// Statements and connections.
// ---------------------------------------------------------------------------

/// Name of the table column behind a result column; NULL for expressions.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_origin_name(
    stmt: *mut sqlite3_stmt,
    idx: ffi::c_int,
) -> *const ffi::c_char {
    if stmt.is_null() || idx < 0 || idx >= sqlite3_column_count(stmt) {
        return std::ptr::null();
    }
    let stmt = &mut *stmt;
    let Some(name) = stmt.stmt.get_column_origin_name(idx as usize) else {
        return std::ptr::null();
    };
    let Ok(c) = CString::new(name) else {
        return std::ptr::null();
    };
    // Interned in param_name_cache under a negative key (parameter indexes
    // are positive) so the pointer stays valid until finalize and repeated
    // calls do not accumulate copies.
    let key = -1 - idx;
    if let Some((_, existing)) = stmt
        .param_name_cache
        .iter()
        .find(|(k, s)| *k == key && *s == c)
    {
        return existing.as_ptr();
    }
    stmt.param_name_cache.push((key, c));
    stmt.param_name_cache.last().unwrap().1.as_ptr()
}

/// 0 for an ordinary statement, 1 for EXPLAIN, 2 for EXPLAIN QUERY PLAN.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_stmt_isexplain(stmt: *mut sqlite3_stmt) -> ffi::c_int {
    if stmt.is_null() {
        return 0;
    }
    match (*stmt).stmt.get_query_mode() {
        QueryMode::Normal => 0,
        QueryMode::Explain => 1,
        QueryMode::ExplainQueryPlan { .. } => 2,
    }
}

/// Converts a statement between normal, EXPLAIN and EXPLAIN QUERY PLAN mode.
/// Like SQLite: SQLITE_ERROR for a bad mode, SQLITE_BUSY once stepping has
/// begun (reset first).
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_stmt_explain(
    stmt: *mut sqlite3_stmt,
    e_mode: ffi::c_int,
) -> ffi::c_int {
    if stmt.is_null() {
        return SQLITE_MISUSE;
    }
    if e_mode == sqlite3_stmt_isexplain(stmt) {
        return SQLITE_OK;
    }
    let mode = match e_mode {
        0 => QueryMode::Normal,
        1 => QueryMode::Explain,
        2 => QueryMode::ExplainQueryPlan {
            format: EqpFormat::Text,
        },
        _ => return SQLITE_ERROR,
    };
    let stmt = &mut *stmt;
    if stmt.stmt.has_started() {
        return SQLITE_BUSY;
    }
    if stmt.stmt.set_query_mode(mode).is_err() {
        return SQLITE_ERROR;
    }
    let n_cols = stmt.stmt.num_columns();
    stmt.text_cache = vec![vec![]; n_cols];
    stmt.value_cache = (0..n_cols).map(|_| None).collect();
    stmt.param_name_cache.retain(|(k, _)| *k >= 0);
    SQLITE_OK
}

/// Turso does not track the byte offset of errors; -1 means "not available",
/// which SQLite also returns when the last error had no offset.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_error_offset(_db: *mut sqlite3) -> ffi::c_int {
    -1
}

/// Turso does not record the OS errno of failed I/O; 0 means "none".
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_system_errno(_db: *mut sqlite3) -> ffi::c_int {
    0
}

static SHARED_CACHE_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Shared cache is deprecated in SQLite and Turso has no equivalent: every
/// connection has its own page cache. The call succeeds for both 0 and 1 (SQLite
/// itself returns SQLITE_OK) and the request is recorded but has no effect on
/// behavior; connections still see each other's committed data through the
/// file as usual. Failing would break callers (e.g. Python 3.10/3.11's
/// sqlite3.enable_shared_cache) that use it purely as an optimization.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_enable_shared_cache(enable: ffi::c_int) -> ffi::c_int {
    SHARED_CACHE_REQUESTED.store(enable != 0, Ordering::Relaxed);
    SQLITE_OK
}
