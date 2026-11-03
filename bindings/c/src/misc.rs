//! Connection limits, tracing and profiling, collation_needed, UTF-16 open
//! and the SQL function `load_extension()`.

use super::*;
use std::collections::HashMap;
use std::time::Instant;

pub const SQLITE_TRACE_STMT: ffi::c_uint = 0x01;
pub const SQLITE_TRACE_PROFILE: ffi::c_uint = 0x02;
pub const SQLITE_TRACE_ROW: ffi::c_uint = 0x04;
pub const SQLITE_TRACE_CLOSE: ffi::c_uint = 0x08;
/// Internal bits, as in SQLite: legacy sqlite3_trace and sqlite3_profile.
const TRACE_LEGACY: ffi::c_uint = 0x40;
const TRACE_XPROFILE: ffi::c_uint = 0x80;
const TRACE_V2_MASK: ffi::c_uint = 0x0f;

type TraceV2Fn = unsafe extern "C" fn(
    ffi::c_uint,
    *mut ffi::c_void,
    *mut ffi::c_void,
    *mut ffi::c_void,
) -> ffi::c_int;
type LegacyTraceFn = unsafe extern "C" fn(*mut ffi::c_void, *const ffi::c_char);
type ProfileFn = unsafe extern "C" fn(*mut ffi::c_void, *const ffi::c_char, u64);
type CollationNeededFn =
    unsafe extern "C" fn(*mut ffi::c_void, *mut sqlite3, ffi::c_int, *const ffi::c_void);

/// Per-statement trace bookkeeping.
#[derive(Default)]
pub(crate) struct StmtMisc {
    /// Between the first step of a run and its end (done, error, reset).
    in_run: bool,
    started: Option<Instant>,
}

#[derive(Clone, Copy, Default)]
struct TraceCfg {
    mask: ffi::c_uint,
    v2: Option<TraceV2Fn>,
    legacy: Option<LegacyTraceFn>,
    trace_ctx: usize,
    profile: Option<ProfileFn>,
    profile_ctx: usize,
}

#[derive(Clone, Copy)]
struct NeededCfg {
    cb: CollationNeededFn,
    ctx: usize,
    utf16: bool,
}

#[derive(Default)]
struct DbMisc {
    trace: Mutex<TraceCfg>,
    needed: Mutex<Option<NeededCfg>>,
    /// Statements being stepped, innermost last (SQLite's nVdbeExec).
    running: Mutex<Vec<usize>>,
}

static STATES: Mutex<Option<HashMap<usize, Arc<DbMisc>>>> = Mutex::new(None);

fn lookup(db: *mut sqlite3) -> Option<Arc<DbMisc>> {
    STATES.lock().ok()?.as_ref()?.get(&(db as usize)).cloned()
}

fn state(db: *mut sqlite3) -> Arc<DbMisc> {
    let mut g = STATES.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(HashMap::new)
        .entry(db as usize)
        .or_default()
        .clone()
}

unsafe fn conn_of(db: *mut sqlite3) -> Option<Arc<turso_core::Connection>> {
    let inner = (*db).inner.lock().ok()?;
    Some(inner.conn.clone())
}

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

/// Called right after a handle is created.
pub(crate) unsafe fn on_open(db: *mut sqlite3) {
    if let Some(conn) = conn_of(db) {
        let addr = db as usize;
        conn.set_extension_loader(Some(Arc::new(move |file, entry| {
            // SAFETY: the loader is dropped with the connection the handle owns.
            unsafe { sql_load_extension(addr as *mut sqlite3, file, entry) }
        })));
    }
}

/// Called when the handle is closed: fires SQLITE_TRACE_CLOSE and drops state.
pub(crate) unsafe fn on_close(db: *mut sqlite3) {
    if let Ok(mut g) = BLOBS.lock() {
        if let Some(m) = g.as_mut() {
            m.retain(|_, d| *d != db as usize);
        }
    }
    let Some(st) = lookup(db) else { return };
    let cfg = *st.trace.lock().unwrap_or_else(|e| e.into_inner());
    if cfg.mask & SQLITE_TRACE_CLOSE != 0 {
        if let Some(cb) = cfg.v2 {
            cb(
                SQLITE_TRACE_CLOSE,
                cfg.trace_ctx as *mut ffi::c_void,
                db.cast(),
                std::ptr::null_mut(),
            );
        }
    }
    if let Ok(mut g) = STATES.lock() {
        if let Some(m) = g.as_mut() {
            m.remove(&(db as usize));
        }
    }
}

// ===== sqlite3_limit =====

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_limit(
    db: *mut sqlite3,
    id: ffi::c_int,
    new_value: ffi::c_int,
) -> ffi::c_int {
    if db.is_null() {
        return -1;
    }
    conn_of(db)
        .and_then(|c| c.set_limit(id, new_value))
        .unwrap_or(-1)
}

/// SQLITE_LIMIT_LENGTH of the connection owning `stmt`.
pub(crate) fn length_limit(db: &sqlite3) -> usize {
    let conn = db.inner.lock().map(|i| i.conn.clone());
    conn.ok()
        .and_then(|c| c.limit(turso_core::limits::LIMIT_LENGTH))
        .unwrap_or(i32::MAX) as usize
}

/// SQLITE_TOOBIG from a bind: the destructor still runs, as in SQLite.
pub(crate) unsafe fn bind_too_big(
    stmt: &mut sqlite3_stmt,
    data: *const ffi::c_void,
    destructor: Option<unsafe extern "C" fn(*mut ffi::c_void)>,
) -> ffi::c_int {
    if let Some(d) = destructor {
        let addr = d as usize;
        if addr != 0 && addr != usize::MAX {
            d(data as *mut ffi::c_void);
        }
    }
    if let Ok(mut inner) = stmt.db.inner.lock() {
        set_db_err_msg(&mut inner, SQLITE_TOOBIG, "string or blob too big");
    }
    SQLITE_TOOBIG
}

// ===== UDF error codes =====

thread_local! {
    /// Code chosen by the last sqlite3_result_error* call of a running UDF.
    static UDF_ERR_CODE: std::cell::Cell<Option<ffi::c_int>> = const { std::cell::Cell::new(None) };
}

/// The first error of a step decides the code: later calls come from
/// cleanup (e.g. an aggregate's xFinal after the error) and must not replace it.
pub(crate) fn set_udf_code(code: ffi::c_int) {
    UDF_ERR_CODE.with(|c| {
        if c.get().is_none() {
            c.set(Some(code));
        }
    });
}

pub(crate) fn take_udf_code() -> Option<ffi::c_int> {
    UDF_ERR_CODE.with(|c| c.take())
}

pub(crate) fn clear_udf_code() {
    UDF_ERR_CODE.with(|c| c.set(None));
}

/// Makes the UDF fail with `code` and its standard message.
pub(crate) unsafe fn result_error_code_impl(context: *mut ffi::c_void, code: ffi::c_int) {
    if context.is_null() {
        return;
    }
    let ctx = &mut *(context as *mut SqliteContext);
    let msg = CStr::from_ptr(sqlite3_errstr(code))
        .to_string_lossy()
        .into_owned();
    ctx.result = ExtValue::error_with_message(msg);
    set_udf_code(code);
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_error_code(context: *mut ffi::c_void, code: ffi::c_int) {
    result_error_code_impl(context, code);
}

// ===== trace and profile =====

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_trace_v2(
    db: *mut sqlite3,
    mask: ffi::c_uint,
    callback: Option<TraceV2Fn>,
    context: *mut ffi::c_void,
) -> ffi::c_int {
    if db.is_null() {
        return SQLITE_MISUSE;
    }
    let (mask, callback) = match callback {
        Some(cb) if mask != 0 => (mask, Some(cb)),
        _ => (0, None),
    };
    let st = state(db);
    {
        let mut cfg = st.trace.lock().unwrap_or_else(|e| e.into_inner());
        cfg.mask = mask;
        cfg.v2 = callback;
        cfg.trace_ctx = context as usize;
    }
    refresh_trigger_hook(db, &st);
    SQLITE_OK
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_trace(
    db: *mut sqlite3,
    callback: Option<LegacyTraceFn>,
    context: *mut ffi::c_void,
) -> *mut ffi::c_void {
    if db.is_null() {
        return std::ptr::null_mut();
    }
    let st = state(db);
    let old = {
        let mut cfg = st.trace.lock().unwrap_or_else(|e| e.into_inner());
        let old = cfg.trace_ctx;
        cfg.mask = if callback.is_some() { TRACE_LEGACY } else { 0 };
        cfg.legacy = callback;
        cfg.v2 = None;
        cfg.trace_ctx = context as usize;
        old
    };
    refresh_trigger_hook(db, &st);
    old as *mut ffi::c_void
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_profile(
    db: *mut sqlite3,
    callback: Option<ProfileFn>,
    context: *mut ffi::c_void,
) -> *mut ffi::c_void {
    if db.is_null() {
        return std::ptr::null_mut();
    }
    let st = state(db);
    let old = {
        let mut cfg = st.trace.lock().unwrap_or_else(|e| e.into_inner());
        let old = cfg.profile_ctx;
        cfg.profile = callback;
        cfg.profile_ctx = context as usize;
        cfg.mask &= TRACE_V2_MASK;
        if callback.is_some() {
            cfg.mask |= TRACE_XPROFILE;
        }
        old
    };
    refresh_trigger_hook(db, &st);
    old as *mut ffi::c_void
}

/// Installs or removes the core hook that reports trigger programs.
unsafe fn refresh_trigger_hook(db: *mut sqlite3, st: &Arc<DbMisc>) {
    let want = st.trace.lock().map(|c| c.mask).unwrap_or(0) & (SQLITE_TRACE_STMT | TRACE_LEGACY);
    let Some(conn) = conn_of(db) else { return };
    if want == 0 {
        conn.set_trigger_trace(None);
        return;
    }
    let st = st.clone();
    let addr = db as usize;
    conn.set_trigger_trace(Some(Arc::new(move |line| {
        let top = st.running.lock().map(|r| r.last().copied()).unwrap_or(None);
        let Some(stmt) = top else { return };
        let cfg = *st.trace.lock().unwrap_or_else(|e| e.into_inner());
        let text = cstring(line);
        // SAFETY: the statement is mid-step on this thread.
        unsafe {
            emit_stmt(
                &cfg,
                addr as *mut sqlite3,
                stmt as *mut sqlite3_stmt,
                &text,
                false,
            )
        };
    })));
}

/// Fires SQLITE_TRACE_STMT or the legacy trace callback. `text` is the
/// statement SQL or a "-- TRIGGER" comment.
unsafe fn emit_stmt(
    cfg: &TraceCfg,
    _db: *mut sqlite3,
    stmt: *mut sqlite3_stmt,
    text: &CStr,
    nested: bool,
) {
    if cfg.mask & TRACE_LEGACY != 0 {
        if let Some(cb) = cfg.legacy {
            let sql = text.to_string_lossy();
            let out = if sql.starts_with("-- ") {
                sql.into_owned()
            } else {
                let expanded = (*stmt).stmt.expanded_sql();
                if nested {
                    comment_lines(&expanded)
                } else {
                    expanded
                }
            };
            cb(cfg.trace_ctx as *mut ffi::c_void, cstring(&out).as_ptr());
        }
    } else if cfg.mask & SQLITE_TRACE_STMT != 0 {
        if let Some(cb) = cfg.v2 {
            let shown;
            let ptr = if nested && !text.to_bytes().starts_with(b"--") {
                shown = cstring(&format!("-- {}", text.to_string_lossy()));
                shown.as_ptr()
            } else {
                text.as_ptr()
            };
            cb(
                SQLITE_TRACE_STMT,
                cfg.trace_ctx as *mut ffi::c_void,
                stmt.cast(),
                ptr as *mut ffi::c_void,
            );
        }
    }
}

/// Prefixes every line with "-- ", as SQLite does for statements run from
/// inside another statement.
fn comment_lines(sql: &str) -> String {
    sql.split_inclusive('\n')
        .map(|l| format!("-- {l}"))
        .collect()
}

unsafe fn emit_profile(cfg: &TraceCfg, stmt: *mut sqlite3_stmt, started: Instant) {
    let ns = started.elapsed().as_nanos().min(i64::MAX as u128) as i64;
    if cfg.mask & SQLITE_TRACE_PROFILE != 0 {
        if let Some(cb) = cfg.v2 {
            let mut ns = ns;
            cb(
                SQLITE_TRACE_PROFILE,
                cfg.trace_ctx as *mut ffi::c_void,
                stmt.cast(),
                (&mut ns as *mut i64).cast(),
            );
        }
    }
    if cfg.mask & TRACE_XPROFILE != 0 {
        if let Some(cb) = cfg.profile {
            cb(
                cfg.profile_ctx as *mut ffi::c_void,
                (*stmt).sql.as_ptr(),
                ns as u64,
            );
        }
    }
}

/// A run ended by reset or finalize: reports the profile.
pub(crate) unsafe fn stmt_end(stmt: *mut sqlite3_stmt) {
    if stmt.is_null() || !(*stmt).misc.in_run {
        return;
    }
    (*stmt).misc.in_run = false;
    let db = Arc::as_ptr(&(*stmt).db) as *mut sqlite3;
    if let (Some(st), Some(started)) = (lookup(db), (*stmt).misc.started.take()) {
        let cfg = *st.trace.lock().unwrap_or_else(|e| e.into_inner());
        emit_profile(&cfg, stmt, started);
    }
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_step(stmt: *mut sqlite3_stmt) -> ffi::c_int {
    if stmt.is_null() {
        return SQLITE_MISUSE;
    }
    let db = Arc::as_ptr(&(*stmt).db) as *mut sqlite3;
    let Some(st) = lookup(db) else {
        return step_impl(stmt);
    };
    let cfg = *st.trace.lock().unwrap_or_else(|e| e.into_inner());
    if cfg.mask == 0 {
        return step_impl(stmt);
    }
    let nested = {
        let mut running = st.running.lock().unwrap_or_else(|e| e.into_inner());
        running.push(stmt as usize);
        running.len() > 1
    };
    if !(*stmt).misc.in_run {
        (*stmt).misc.in_run = true;
        (*stmt).misc.started = Some(Instant::now());
        let sql = (*stmt).sql.clone();
        emit_stmt(&cfg, db, stmt, &sql, nested);
    }
    let rc = step_impl(stmt);
    if let Ok(mut running) = st.running.lock() {
        running.pop();
    }
    // Callbacks may have changed the configuration.
    let cfg = *st.trace.lock().unwrap_or_else(|e| e.into_inner());
    if rc == SQLITE_ROW {
        if cfg.mask & SQLITE_TRACE_ROW != 0 {
            if let Some(cb) = cfg.v2 {
                cb(
                    SQLITE_TRACE_ROW,
                    cfg.trace_ctx as *mut ffi::c_void,
                    stmt.cast(),
                    std::ptr::null_mut(),
                );
            }
        }
    } else {
        (*stmt).misc.in_run = false;
        if let Some(started) = (*stmt).misc.started.take() {
            emit_profile(&cfg, stmt, started);
        }
    }
    rc
}

// ===== prepare: SQL length limit and collation_needed =====

unsafe fn fail_prepare(
    db: *mut sqlite3,
    out_stmt: *mut *mut sqlite3_stmt,
    code: ffi::c_int,
    msg: &str,
) -> ffi::c_int {
    *out_stmt = std::ptr::null_mut();
    if let Ok(mut inner) = (*db).inner.lock() {
        set_db_err_msg(&mut inner, code, msg);
    }
    code
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_prepare_v2(
    db: *mut sqlite3,
    sql: *const ffi::c_char,
    n_byte: ffi::c_int,
    out_stmt: *mut *mut sqlite3_stmt,
    tail: *mut *const ffi::c_char,
) -> ffi::c_int {
    if !out_stmt.is_null() {
        *out_stmt = std::ptr::null_mut();
    }
    if db.is_null() || sql.is_null() || out_stmt.is_null() {
        return SQLITE_MISUSE;
    }
    let max_len = conn_of(db)
        .and_then(|c| c.limit(turso_core::limits::LIMIT_SQL_LENGTH))
        .unwrap_or(i32::MAX) as usize;
    if n_byte >= 0 && n_byte as usize > max_len && *sql.add(n_byte as usize - 1) != 0 {
        return fail_prepare(db, out_stmt, SQLITE_TOOBIG, "statement too long");
    }
    let mut asked = Vec::new();
    let mut rc;
    let mut end: *const ffi::c_char = std::ptr::null();
    loop {
        rc = prepare_v2_impl(db, sql, n_byte, out_stmt, &mut end);
        if rc != SQLITE_ERROR {
            break;
        }
        let Some(needed) = lookup(db).and_then(|s| *s.needed.lock().ok()?) else {
            break;
        };
        let msg = (*db)
            .inner
            .lock()
            .ok()
            .and_then(|i| i.p_err.as_ref().map(|m| m.to_string_lossy().into_owned()));
        let name = msg.as_deref().and_then(|m| {
            m.split_once("no such collation sequence: ")
                .map(|(_, n)| n.to_string())
        });
        let Some(name) = name else { break };
        if asked.contains(&name) {
            break;
        }
        call_collation_needed(db, &needed, &name);
        asked.push(name);
    }
    if rc == SQLITE_OK && !end.is_null() {
        end = skip_trailing_blanks(sql, n_byte, end);
    }
    if !tail.is_null() && !end.is_null() {
        *tail = end;
    }
    if rc == SQLITE_OK && !(*out_stmt).is_null() {
        let len = end.offset_from(sql) as usize;
        if len > max_len {
            sqlite3_finalize(*out_stmt);
            return fail_prepare(db, out_stmt, SQLITE_TOOBIG, "string or blob too big");
        }
        // SQLite's statement text runs from the start of the input to its tail.
        let text = std::slice::from_raw_parts(sql.cast::<u8>(), len);
        (**out_stmt).sql = CString::new(text.to_vec()).unwrap_or_default();
    }
    rc
}

/// A statement that does not end in ';' runs to the end of the input, so its
/// tail (and sqlite3_sql text) include trailing blanks and comments.
unsafe fn skip_trailing_blanks(
    sql: *const ffi::c_char,
    n_byte: ffi::c_int,
    end: *const ffi::c_char,
) -> *const ffi::c_char {
    let start = end.offset_from(sql) as usize;
    let total = if n_byte < 0 {
        CStr::from_ptr(sql).to_bytes().len()
    } else {
        let bounded = std::slice::from_raw_parts(sql.cast::<u8>(), n_byte as usize);
        bounded
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(bounded.len())
    };
    let bytes = std::slice::from_raw_parts(sql.cast::<u8>(), total);
    if start == 0 || start > total || bytes[..start].last() == Some(&b';') {
        return end;
    }
    let mut i = start;
    while i < total {
        match bytes[i] {
            b if b.is_ascii_whitespace() => i += 1,
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i = bytes[i..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(total, |p| i + p + 1);
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = match bytes[i + 2..].windows(2).position(|w| w == b"*/") {
                    Some(p) => i + 2 + p + 2,
                    None => total,
                };
            }
            // Another statement follows the one already compiled.
            _ => return end,
        }
    }
    sql.add(total)
}

// ===== collation_needed =====

unsafe fn call_collation_needed(db: *mut sqlite3, cfg: &NeededCfg, name: &str) {
    if cfg.utf16 {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        (cfg.cb)(
            cfg.ctx as *mut ffi::c_void,
            db,
            2, // SQLITE_UTF16LE
            wide.as_ptr().cast(),
        );
    } else {
        let c = cstring(name);
        (cfg.cb)(cfg.ctx as *mut ffi::c_void, db, 1, c.as_ptr().cast());
    }
}

unsafe fn set_needed(
    db: *mut sqlite3,
    cb: Option<CollationNeededFn>,
    ctx: *mut ffi::c_void,
    utf16: bool,
) -> ffi::c_int {
    if db.is_null() {
        return SQLITE_MISUSE;
    }
    let st = state(db);
    *st.needed.lock().unwrap_or_else(|e| e.into_inner()) = cb.map(|cb| NeededCfg {
        cb,
        ctx: ctx as usize,
        utf16,
    });
    SQLITE_OK
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_collation_needed(
    db: *mut sqlite3,
    context: *mut ffi::c_void,
    callback: Option<CollationNeededFn>,
) -> ffi::c_int {
    set_needed(db, callback, context, false)
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_collation_needed16(
    db: *mut sqlite3,
    context: *mut ffi::c_void,
    callback: Option<CollationNeededFn>,
) -> ffi::c_int {
    set_needed(db, callback, context, true)
}

// ===== sqlite3_open16 =====

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_open16(
    filename: *const ffi::c_void,
    db_out: *mut *mut sqlite3,
) -> ffi::c_int {
    if db_out.is_null() {
        return SQLITE_MISUSE;
    }
    *db_out = std::ptr::null_mut();
    if filename.is_null() {
        return sqlite3_open_v2(c"".as_ptr(), db_out, OPEN_RWC, std::ptr::null());
    }
    let mut units = Vec::new();
    let mut p = filename.cast::<u16>();
    while p.read_unaligned() != 0 {
        units.push(p.read_unaligned());
        p = p.add(1);
    }
    let name = cstring(&String::from_utf16_lossy(&units));
    sqlite3_open_v2(name.as_ptr(), db_out, OPEN_RWC, std::ptr::null())
}

/// What sqlite3_open() does: SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE.
const OPEN_RWC: ffi::c_int = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE;

// ===== SQL function load_extension() =====

unsafe fn sql_load_extension(
    db: *mut sqlite3,
    file: &str,
    entry: Option<&str>,
) -> Result<(), String> {
    let file = cstring(file);
    let entry = entry.map(cstring);
    let mut err: *mut ffi::c_char = std::ptr::null_mut();
    let rc = extension::load_extension_internal(
        db,
        file.as_ptr(),
        entry.as_ref().map_or(std::ptr::null(), |e| e.as_ptr()),
        &mut err,
    );
    if rc == SQLITE_OK {
        return Ok(());
    }
    let msg = if err.is_null() {
        "unable to load extension".to_string()
    } else {
        CStr::from_ptr(err).to_string_lossy().into_owned()
    };
    sqlite3_free(err.cast());
    Err(msg)
}

// ===== blob handles =====

/// Open blob handles and the connection each belongs to.
static BLOBS: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None);

pub(crate) fn blob_opened(blob: *mut ffi::c_void, db: *mut sqlite3) {
    if let Ok(mut g) = BLOBS.lock() {
        g.get_or_insert_with(HashMap::new)
            .insert(blob as usize, db as usize);
    }
}

pub(crate) fn blob_closed(blob: *mut ffi::c_void) {
    if let Ok(mut g) = BLOBS.lock() {
        if let Some(m) = g.as_mut() {
            m.remove(&(blob as usize));
        }
    }
}

/// Records the outcome of a blob call on its connection, as SQLite does, and
/// returns the result code.
pub(crate) fn blob_result(blob: *mut ffi::c_void, result: Result<(), LimboError>) -> ffi::c_int {
    let code = match &result {
        Ok(()) => SQLITE_OK,
        Err(err) => limbo_err_code(err),
    };
    let db = BLOBS
        .lock()
        .ok()
        .and_then(|g| g.as_ref()?.get(&(blob as usize)).copied());
    if let Some(db) = db {
        // SAFETY: handles of a closed connection are dropped in on_close.
        if let Ok(mut inner) = unsafe { &*(db as *mut sqlite3) }.inner.lock() {
            set_db_err_code(&mut inner, code);
        }
    }
    code
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_reopen(blob: *mut ffi::c_void, rowid: i64) -> ffi::c_int {
    if blob.is_null() {
        return SQLITE_MISUSE;
    }
    // SAFETY: a live handle from sqlite3_blob_open, per the API contract.
    let handle = &mut *(blob as *mut turso_core::Blob);
    blob_result(blob, handle.reopen(rowid))
}
