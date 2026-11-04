//! C API entry points the sqlite3 shell needs beyond the core set: auxdata,
//! client data, mutexes, string matching, transaction state and friends,
//! plus the built-in virtual tables of a SQLite build with DBPAGE_VTAB,
//! BYTECODE_VTAB and STMTVTAB.

use std::collections::HashMap;
use std::ffi::{self, c_char, c_int, c_void, CStr, CString};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::ThreadId;

use turso_core::native_ext::{
    IndexInfo, IndexPlan, IndexRequest, VirtualTable, VirtualTableCursor, VirtualTableModule,
};
use turso_core::{Connection, IOResult, Value};
use turso_ext::{ConstraintInfo, OrderByInfo, ResultCode};

use crate::{sqlite3, sqlite3_stmt, SQLITE_BUSY, SQLITE_MISUSE, SQLITE_OK};

type Destructor = Option<unsafe extern "C" fn(*mut c_void)>;
type XFunc = unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_void);

// Other exported entry points, called through the C ABI.
extern "C" {
    fn sqlite3_result_text(ctx: *mut c_void, z: *const c_char, n: c_int, d: Destructor);
    fn sqlite3_result_blob(ctx: *mut c_void, z: *const c_void, n: c_int, d: Destructor);
    fn sqlite3_result_null(ctx: *mut c_void);
    fn sqlite3_result_error(ctx: *mut c_void, z: *const c_char, n: c_int);
    fn sqlite3_result_error_toobig(ctx: *mut c_void);
    fn sqlite3_user_data(ctx: *mut c_void) -> *mut c_void;
    fn sqlite3_create_function_v2(
        db: *mut c_void,
        name: *const c_char,
        n_arg: c_int,
        enc: c_int,
        app: *mut c_void,
        x_func: Option<XFunc>,
        x_step: Option<XFunc>,
        x_final: Option<unsafe extern "C" fn(*mut c_void)>,
        x_destroy: Destructor,
    ) -> c_int;
    fn sqlite3_value_type(v: *mut c_void) -> c_int;
    fn sqlite3_value_int64(v: *mut c_void) -> i64;
    fn sqlite3_value_double(v: *mut c_void) -> f64;
    fn sqlite3_value_text(v: *mut c_void) -> *const u8;
    fn sqlite3_value_blob(v: *mut c_void) -> *const c_void;
    fn sqlite3_value_bytes(v: *mut c_void) -> c_int;
    fn sqlite3_bind_null(s: *mut c_void, i: c_int) -> c_int;
    fn sqlite3_bind_int64(s: *mut c_void, i: c_int, v: i64) -> c_int;
    fn sqlite3_bind_double(s: *mut c_void, i: c_int, v: f64) -> c_int;
    fn sqlite3_bind_text(
        s: *mut c_void,
        i: c_int,
        z: *const c_char,
        n: c_int,
        d: Destructor,
    ) -> c_int;
    fn sqlite3_bind_blob(
        s: *mut c_void,
        i: c_int,
        z: *const c_void,
        n: c_int,
        d: Destructor,
    ) -> c_int;
    fn sqlite3_next_stmt(db: *mut c_void, s: *mut c_void) -> *mut c_void;
    fn sqlite3_sql(s: *mut c_void) -> *const c_char;
    fn sqlite3_column_count(s: *mut c_void) -> c_int;
    fn sqlite3_stmt_readonly(s: *mut c_void) -> c_int;
    fn sqlite3_stmt_busy(s: *mut c_void) -> c_int;
    fn sqlite3_stmt_status(s: *mut c_void, op: c_int, reset: c_int) -> c_int;
}

const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_TEXT: c_int = 3;
const SQLITE_BLOB: c_int = 4;
const SQLITE_UTF8: c_int = 1;

fn transient() -> Destructor {
    // SAFETY: SQLITE_TRANSIENT is the all-ones pointer; it is never called.
    unsafe { std::mem::transmute::<usize, Destructor>(usize::MAX) }
}

/// Runs a caller destructor unless it is SQLITE_STATIC or SQLITE_TRANSIENT.
unsafe fn run_destructor(d: Destructor, p: *const c_void) {
    if let Some(f) = d {
        if f as usize != usize::MAX {
            f(p as *mut c_void);
        }
    }
}

// ------------------------------------------------------------ init state

/// Whether the library is initialized: sqlite3_shutdown undoes
/// sqlite3_initialize, so sqlite3_config works again afterwards. The
/// one-time setup in `call_once` runs only on the first initialize.
pub(crate) struct InitState {
    once: std::sync::Once,
    active: std::sync::atomic::AtomicBool,
}

impl InitState {
    pub(crate) const fn new() -> Self {
        Self {
            once: std::sync::Once::new(),
            active: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(crate) fn call_once(&self, f: impl FnOnce()) {
        self.once.call_once(f);
        self.active.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Read by sqlite3_config (compat/config).
    #[allow(dead_code)]
    pub(crate) fn is_completed(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn reset(&self) {
        self.active
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

// ------------------------------------------------------------ connection

/// Registers the built-in virtual tables on a new handle.
fn stmt_from_pointer(p: usize) -> Option<*const turso_core::Statement> {
    // SAFETY: a "stmt-pointer" is a live sqlite3_stmt the caller bound.
    (p != 0).then(|| unsafe { &(*(p as *const sqlite3_stmt)).stmt as *const _ })
}

pub(crate) fn on_open(db: *mut sqlite3, conn: &Arc<Connection>) {
    turso_core::vdbevtab::set_stmt_pointer_resolver(stmt_from_pointer);
    // SAFETY: db was just created.
    let name = unsafe { (*db).inner.lock().unwrap().filename.as_ptr() as usize };
    db_filenames()
        .lock()
        .unwrap()
        .insert(Arc::as_ptr(conn) as usize, name);
    let _ = conn.register_vtab_module_local(
        turso_core::sqlite_dbpage::DBPAGE_MODULE_NAME,
        turso_core::sqlite_dbpage::DbpageModule,
        true,
        true,
    );
    for tables_used in [false, true] {
        let name = if tables_used {
            turso_core::vdbevtab::TABLES_USED_MODULE_NAME
        } else {
            turso_core::vdbevtab::BYTECODE_MODULE_NAME
        };
        let _ = conn.register_vtab_module_local(
            name,
            turso_core::vdbevtab::BytecodeModule { tables_used },
            false,
            true,
        );
    }
    let _ =
        conn.register_vtab_module_local("sqlite_stmt", StmtModule { db: db as usize }, false, true);
}

/// Disconnects the eponymous virtual tables, as sqlite3_close does before
/// it checks for unfinalized statements: their xDisconnect may finalize
/// statements of their own. They reconnect on next use.
pub(crate) unsafe fn disconnect_vtabs(db: *mut sqlite3) {
    let (conn, busy) = {
        let inner = (*db).inner.lock().unwrap();
        (inner.conn.clone(), !inner.stmt_list.is_null())
    };
    conn.disconnect_eponymous_vtabs();
    // Created tables too, when statements are still open (with none, close
    // drops them anyway).
    if busy
        && !(*db).inner.lock().unwrap().stmt_list.is_null()
        && conn.disconnect_created_vtabs().unwrap_or(false)
        && !(*db).inner.lock().unwrap().stmt_list.is_null()
    {
        // Close will fail: reconnect them for the handle's further use
        // (SQLite reconnects on next use).
        let _ = conn.force_reparse_schema();
    }
}

/// SQLITE_DBCONFIG_WRITABLE_SCHEMA: same switch as PRAGMA writable_schema.
pub(crate) unsafe fn db_config_writable_schema(
    db: *mut sqlite3,
    value: c_int,
    out: *mut c_int,
) -> c_int {
    let conn = (*db).inner.lock().unwrap().conn.clone();
    if value >= 0 {
        conn.set_writable_schema(value > 0);
    }
    if !out.is_null() {
        *out = conn.writable_schema() as c_int;
    }
    SQLITE_OK
}

/// Runs the client-data destructors of a connection that is going away.
pub(crate) fn on_close(conn: &Arc<Connection>) {
    let key = Arc::as_ptr(conn) as usize;
    db_filenames().lock().unwrap().remove(&key);
    let entries = client_data().lock().unwrap().remove(&key);
    // Destructors run outside the lock.
    drop(entries);
}

/// Whether `name` is a table (not a view) of the main database, for
/// sqlite3_table_column_metadata with a NULL column.
pub(crate) fn table_exists(conn: &Arc<Connection>, name: &str) -> bool {
    let text = |v: &Value| v.to_text().unwrap_or_default().to_string();
    conn.pragma_query("table_list").is_ok_and(|rows| {
        rows.iter().any(|r| {
            r.len() > 2
                && text(&r[0]) == "main"
                && text(&r[1]).eq_ignore_ascii_case(name)
                && text(&r[2]) != "view"
        })
    })
}

// ------------------------------------------------------------ client data

struct ClientData {
    name: CString,
    ptr: usize,
    destructor: Destructor,
}

impl Drop for ClientData {
    fn drop(&mut self) {
        // SAFETY: the destructor was supplied with this pointer.
        unsafe { run_destructor(self.destructor, self.ptr as *const c_void) };
    }
}

fn client_data() -> &'static Mutex<HashMap<usize, Vec<ClientData>>> {
    static M: OnceLock<Mutex<HashMap<usize, Vec<ClientData>>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

unsafe fn conn_key(db: *mut sqlite3) -> usize {
    let inner = (*db).inner.lock().unwrap();
    Arc::as_ptr(&inner.conn) as usize
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_get_clientdata(
    db: *mut sqlite3,
    name: *const c_char,
) -> *mut c_void {
    if db.is_null() || name.is_null() {
        return std::ptr::null_mut();
    }
    let key = conn_key(db);
    let name = CStr::from_ptr(name);
    let map = client_data().lock().unwrap();
    map.get(&key)
        .and_then(|v| v.iter().find(|c| c.name.as_c_str() == name))
        .map_or(std::ptr::null_mut(), |c| c.ptr as *mut c_void)
}

/// Replacing or clearing an entry runs the old destructor; a NULL pointer
/// removes the entry.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_set_clientdata(
    db: *mut sqlite3,
    name: *const c_char,
    data: *mut c_void,
    destructor: Destructor,
) -> c_int {
    if db.is_null() || name.is_null() {
        return SQLITE_MISUSE;
    }
    let key = conn_key(db);
    let name = CStr::from_ptr(name).to_owned();
    let old = {
        let mut map = client_data().lock().unwrap();
        let list = map.entry(key).or_default();
        let pos = list.iter().position(|c| c.name == name);
        let old = pos.map(|i| list.remove(i));
        if !data.is_null() {
            list.insert(
                0,
                ClientData {
                    name,
                    ptr: data as usize,
                    destructor,
                },
            );
        }
        old
    };
    drop(old);
    SQLITE_OK
}

// ------------------------------------------------------------ auxdata

#[no_mangle]
pub unsafe extern "C" fn sqlite3_get_auxdata(ctx: *mut c_void, n: c_int) -> *mut c_void {
    if ctx.is_null() {
        return std::ptr::null_mut();
    }
    turso_core::auxdata::get_auxdata(n).map_or(std::ptr::null_mut(), |p| p as *mut c_void)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_set_auxdata(
    ctx: *mut c_void,
    n: c_int,
    data: *mut c_void,
    destructor: Destructor,
) {
    if ctx.is_null() {
        run_destructor(destructor, data);
        return;
    }
    turso_core::auxdata::set_auxdata(n, data as usize, destructor);
}

// ------------------------------------------------------------ results

#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_blob64(
    ctx: *mut c_void,
    z: *const c_void,
    n: u64,
    destructor: Destructor,
) {
    if ctx.is_null() {
        run_destructor(destructor, z);
        return;
    }
    if n > 0x7fff_ffff {
        run_destructor(destructor, z);
        sqlite3_result_error_toobig(ctx);
        return;
    }
    sqlite3_result_blob(ctx, z, n as c_int, destructor);
}

/// SQLite's UTF-16 to UTF-8 translation (without SQLITE_REPLACE_INVALID_UTF:
/// a surrogate takes the next unit, whatever it is).
fn utf16_to_utf8(b: &[u8], big_endian: bool) -> Vec<u8> {
    let unit = |i: usize| -> u32 {
        if big_endian {
            ((b[i] as u32) << 8) | b[i + 1] as u32
        } else {
            b[i] as u32 | ((b[i + 1] as u32) << 8)
        }
    };
    let mut out = Vec::with_capacity(b.len() * 3 / 2);
    let mut i = 0;
    while i + 1 < b.len() {
        let mut c = unit(i);
        i += 2;
        if (0xd800..0xe000).contains(&c) && i + 1 < b.len() {
            let c2 = unit(i);
            i += 2;
            c = (c2 & 0x03ff) + ((c & 0x003f) << 10) + (((c & 0x03c0) + 0x0040) << 10);
        }
        if c < 0x80 {
            out.push(c as u8);
        } else if c < 0x800 {
            out.push(0xc0 | (c >> 6) as u8);
            out.push(0x80 | (c & 0x3f) as u8);
        } else if c < 0x10000 {
            out.push(0xe0 | (c >> 12) as u8);
            out.push(0x80 | ((c >> 6) & 0x3f) as u8);
            out.push(0x80 | (c & 0x3f) as u8);
        } else {
            out.push(0xf0 | ((c >> 18) & 0x07) as u8);
            out.push(0x80 | ((c >> 12) & 0x3f) as u8);
            out.push(0x80 | ((c >> 6) & 0x3f) as u8);
            out.push(0x80 | (c & 0x3f) as u8);
        }
    }
    out
}

/// The bytes of a UTF-16 argument: `n` (rounded down to even), or up to
/// the first zero unit when `n` is negative.
unsafe fn utf16_bytes<'a>(z: *const c_void, n: c_int) -> &'a [u8] {
    let p = z as *const u8;
    let len = if n < 0 {
        let mut k = 0usize;
        while *p.add(k) != 0 || *p.add(k + 1) != 0 {
            k += 2;
        }
        k
    } else {
        (n as usize) & !1
    };
    std::slice::from_raw_parts(p, len)
}

unsafe fn result_text16(
    ctx: *mut c_void,
    z: *const c_void,
    n: c_int,
    destructor: Destructor,
    big_endian: bool,
) {
    if ctx.is_null() {
        run_destructor(destructor, z);
        return;
    }
    if z.is_null() {
        sqlite3_result_null(ctx);
        return;
    }
    let text = utf16_to_utf8(utf16_bytes(z, n), big_endian);
    sqlite3_result_text(ctx, text.as_ptr().cast(), text.len() as c_int, transient());
    run_destructor(destructor, z);
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_text16le(
    ctx: *mut c_void,
    z: *const c_void,
    n: c_int,
    destructor: Destructor,
) {
    result_text16(ctx, z, n, destructor, false);
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_result_text16be(
    ctx: *mut c_void,
    z: *const c_void,
    n: c_int,
    destructor: Destructor,
) {
    result_text16(ctx, z, n, destructor, true);
}

// ------------------------------------------------------------ statements

/// Binds a copy of `value`, as sqlite3_bind_value does.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_value(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
    value: *mut c_void,
) -> c_int {
    if stmt.is_null() {
        return SQLITE_MISUSE;
    }
    if value.is_null() {
        return sqlite3_bind_null(stmt.cast(), idx);
    }
    let stmt: *mut c_void = stmt.cast();
    match sqlite3_value_type(value) {
        SQLITE_INTEGER => sqlite3_bind_int64(stmt, idx, sqlite3_value_int64(value)),
        SQLITE_FLOAT => sqlite3_bind_double(stmt, idx, sqlite3_value_double(value)),
        SQLITE_TEXT => {
            let z = sqlite3_value_text(value);
            let n = sqlite3_value_bytes(value);
            sqlite3_bind_text(stmt, idx, z.cast(), n, transient())
        }
        SQLITE_BLOB => {
            let n = sqlite3_value_bytes(value);
            let z = sqlite3_value_blob(value);
            let empty = [0u8; 1];
            let z = if z.is_null() {
                empty.as_ptr().cast()
            } else {
                z
            };
            sqlite3_bind_blob(stmt, idx, z, n, transient())
        }
        _ => sqlite3_bind_null(stmt, idx),
    }
}

/// Interned schema names; the pointers stay valid for the process.
fn intern(name: String) -> *const c_char {
    static NAMES: OnceLock<Mutex<Vec<Box<CStr>>>> = OnceLock::new();
    let Ok(c) = CString::new(name) else {
        return std::ptr::null();
    };
    let mut names = NAMES.get_or_init(Default::default).lock().unwrap();
    if let Some(n) = names.iter().find(|n| ***n == *c) {
        return n.as_ptr();
    }
    names.push(c.into_boxed_c_str());
    names.last().unwrap().as_ptr()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_database_name(
    stmt: *mut sqlite3_stmt,
    idx: c_int,
) -> *const c_char {
    if stmt.is_null() || idx < 0 {
        return std::ptr::null();
    }
    let stmt = &*stmt;
    if idx as usize >= stmt.stmt.num_columns() {
        return std::ptr::null();
    }
    stmt.stmt
        .column_database_name(idx as usize)
        .map_or(std::ptr::null(), intern)
}

/// Filename pointers handed out by `sqlite3_db_filename`, by connection.
fn db_filenames() -> &'static Mutex<HashMap<usize, usize>> {
    static M: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// `sqlite3_db_filename` hands out the bare database name, which is its own
/// database filename. Other names (those a VFS gets in xOpen) live in
/// SQLite's filename block, whose database name follows four zero bytes:
/// walk back to it, as SQLite's databaseName() does.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_filename_database(f: *const c_char) -> *const c_char {
    if f.is_null() {
        return f;
    }
    if db_filenames()
        .lock()
        .unwrap()
        .values()
        .any(|&p| p == f as usize)
    {
        return f;
    }
    let mut p = f as *const u8;
    while *p.sub(1) != 0 || *p.sub(2) != 0 || *p.sub(3) != 0 || *p.sub(4) != 0 {
        p = p.sub(1);
    }
    p.cast()
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_txn_state(db: *mut sqlite3, schema: *const c_char) -> c_int {
    if db.is_null() {
        return -1;
    }
    let conn = (*db).inner.lock().unwrap().conn.clone();
    let schema = if schema.is_null() {
        None
    } else {
        Some(CStr::from_ptr(schema).to_string_lossy().into_owned())
    };
    conn.txn_state(schema.as_deref()).unwrap_or(-1)
}

unsafe extern "C" fn invalid_function(ctx: *mut c_void, _argc: c_int, _argv: *mut *mut c_void) {
    let name = sqlite3_user_data(ctx) as *const c_char;
    let name = if name.is_null() {
        String::new()
    } else {
        CStr::from_ptr(name).to_string_lossy().into_owned()
    };
    let msg = format!("unable to use function {name} in the requested context");
    sqlite3_result_error(ctx, msg.as_ptr().cast(), msg.len() as c_int);
}

unsafe extern "C" fn free_cstring(p: *mut c_void) {
    if !p.is_null() {
        drop(CString::from_raw(p.cast()));
    }
}

/// Makes sure a function `name` with `n_arg` arguments exists, so a virtual
/// table can overload it; the placeholder fails when called.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_overload_function(
    db: *mut sqlite3,
    name: *const c_char,
    n_arg: c_int,
) -> c_int {
    if db.is_null() || name.is_null() || n_arg < -2 {
        return SQLITE_MISUSE;
    }
    let conn = (*db).inner.lock().unwrap().conn.clone();
    let zname = CStr::from_ptr(name).to_string_lossy().into_owned();
    if conn.function_exists(&zname, n_arg) {
        return SQLITE_OK;
    }
    let copy = CStr::from_ptr(name).to_owned().into_raw();
    sqlite3_create_function_v2(
        db.cast(),
        name,
        n_arg,
        SQLITE_UTF8,
        copy.cast(),
        Some(invalid_function),
        None,
        None,
        Some(free_cstring),
    )
}

// ------------------------------------------------------------ strings

/// `sqlite3Utf8Read`, reading a zero past the end of `s`.
fn utf8_read(s: &[u8], i: &mut usize) -> u32 {
    let at = |k: usize| s.get(k).copied().unwrap_or(0) as u32;
    let mut c = at(*i);
    if c == 0 {
        return 0;
    }
    *i += 1;
    if c >= 0xc0 {
        const TRANS1: [u8; 64] = [
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26, 27, 28, 29, 30, 31, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
            0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 0, 1, 0, 0,
        ];
        c = TRANS1[(c - 0xc0) as usize] as u32;
        while at(*i) & 0xc0 == 0x80 {
            c = (c << 6) + (0x3f & at(*i));
            *i += 1;
        }
        if c < 0x80 || (c & 0xffff_f800) == 0xd800 || (c & 0xffff_fffe) == 0xfffe {
            c = 0xfffd;
        }
    }
    c
}

struct CompareInfo {
    match_all: u32,
    match_one: u32,
    match_set: u32,
    no_case: bool,
}

const GLOB_INFO: CompareInfo = CompareInfo {
    match_all: b'*' as u32,
    match_one: b'?' as u32,
    match_set: b'[' as u32,
    no_case: false,
};
const LIKE_INFO: CompareInfo = CompareInfo {
    match_all: b'%' as u32,
    match_one: b'_' as u32,
    match_set: 0,
    no_case: true,
};

const MATCH: c_int = 0;
const NOMATCH: c_int = 1;
const NOWILDCARDMATCH: c_int = 2;

fn lower(c: u32) -> u32 {
    if (b'A' as u32..=b'Z' as u32).contains(&c) {
        c + 32
    } else {
        c
    }
}

/// SQLite's `patternCompare` (func.c), over NUL-terminated byte strings.
fn pattern_compare(
    pat: &[u8],
    mut ip: usize,
    s: &[u8],
    mut is: usize,
    info: &CompareInfo,
    match_other: u32,
) -> c_int {
    let at = |b: &[u8], k: usize| b.get(k).copied().unwrap_or(0);
    let mut escaped: Option<usize> = None;
    loop {
        let mut c = utf8_read(pat, &mut ip);
        if c == 0 {
            break;
        }
        if c == info.match_all {
            loop {
                c = utf8_read(pat, &mut ip);
                if c == info.match_all || (c == info.match_one && info.match_one != 0) {
                    if c == info.match_one && utf8_read(s, &mut is) == 0 {
                        return NOWILDCARDMATCH;
                    }
                } else {
                    break;
                }
            }
            if c == 0 {
                return MATCH;
            } else if c == match_other {
                if info.match_set == 0 {
                    c = utf8_read(pat, &mut ip);
                    if c == 0 {
                        return NOWILDCARDMATCH;
                    }
                } else {
                    // "[...]" right after "*": try every position.
                    while at(s, is) != 0 {
                        let r = pattern_compare(pat, ip - 1, s, is, info, match_other);
                        if r != NOMATCH {
                            return r;
                        }
                        utf8_read(s, &mut is);
                    }
                    return NOWILDCARDMATCH;
                }
            }
            if c < 0x80 {
                let stop: [u32; 2] = if info.no_case {
                    [
                        (c as u8).to_ascii_uppercase() as u32,
                        (c as u8).to_ascii_lowercase() as u32,
                    ]
                } else {
                    [c, c]
                };
                loop {
                    while at(s, is) != 0 && !stop.contains(&(at(s, is) as u32)) {
                        is += 1;
                    }
                    if at(s, is) == 0 {
                        break;
                    }
                    is += 1;
                    let r = pattern_compare(pat, ip, s, is, info, match_other);
                    if r != NOMATCH {
                        return r;
                    }
                }
            } else {
                loop {
                    let c2 = utf8_read(s, &mut is);
                    if c2 == 0 {
                        break;
                    }
                    if c2 != c {
                        continue;
                    }
                    let r = pattern_compare(pat, ip, s, is, info, match_other);
                    if r != NOMATCH {
                        return r;
                    }
                }
            }
            return NOWILDCARDMATCH;
        }
        if c == match_other {
            if info.match_set == 0 {
                c = utf8_read(pat, &mut ip);
                if c == 0 {
                    return NOMATCH;
                }
                escaped = Some(ip);
            } else {
                let mut prior_c = 0u32;
                let mut seen = false;
                let mut invert = false;
                let c = utf8_read(s, &mut is);
                if c == 0 {
                    return NOMATCH;
                }
                let mut c2 = utf8_read(pat, &mut ip);
                if c2 == b'^' as u32 {
                    invert = true;
                    c2 = utf8_read(pat, &mut ip);
                }
                if c2 == b']' as u32 {
                    if c == b']' as u32 {
                        seen = true;
                    }
                    c2 = utf8_read(pat, &mut ip);
                }
                while c2 != 0 && c2 != b']' as u32 {
                    if c2 == b'-' as u32 && at(pat, ip) != b']' && at(pat, ip) != 0 && prior_c > 0 {
                        c2 = utf8_read(pat, &mut ip);
                        if c >= prior_c && c <= c2 {
                            seen = true;
                        }
                        prior_c = 0;
                    } else {
                        if c == c2 {
                            seen = true;
                        }
                        prior_c = c2;
                    }
                    c2 = utf8_read(pat, &mut ip);
                }
                if c2 == 0 || !(seen ^ invert) {
                    return NOMATCH;
                }
                continue;
            }
        }
        let c2 = utf8_read(s, &mut is);
        if c == c2 {
            continue;
        }
        if info.no_case && lower(c) == lower(c2) && c < 0x80 && c2 < 0x80 {
            continue;
        }
        if c == info.match_one && escaped != Some(ip) && c2 != 0 {
            continue;
        }
        return NOMATCH;
    }
    if at(s, is) == 0 {
        MATCH
    } else {
        NOMATCH
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_strglob(pattern: *const c_char, s: *const c_char) -> c_int {
    if s.is_null() {
        return (!pattern.is_null()) as c_int;
    }
    if pattern.is_null() {
        return 1;
    }
    let p = CStr::from_ptr(pattern).to_bytes();
    let s = CStr::from_ptr(s).to_bytes();
    pattern_compare(p, 0, s, 0, &GLOB_INFO, b'[' as u32)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_strlike(
    pattern: *const c_char,
    s: *const c_char,
    esc: ffi::c_uint,
) -> c_int {
    if s.is_null() {
        return (!pattern.is_null()) as c_int;
    }
    if pattern.is_null() {
        return 1;
    }
    let p = CStr::from_ptr(pattern).to_bytes();
    let s = CStr::from_ptr(s).to_bytes();
    pattern_compare(p, 0, s, 0, &LIKE_INFO, esc)
}

// ------------------------------------------------------------ randomness

/// Fills the buffer with random bytes; N <= 0 or NULL only reseeds in
/// SQLite, which needs nothing here.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_randomness(n: c_int, p: *mut c_void) {
    if n <= 0 || p.is_null() {
        return;
    }
    let buf = std::slice::from_raw_parts_mut(p as *mut u8, n as usize);
    let mut filled = 0;
    while filled < buf.len() {
        let r = libc::getrandom(buf[filled..].as_mut_ptr().cast(), buf.len() - filled, 0);
        if r <= 0 {
            break;
        }
        filled += r as usize;
    }
    if filled < buf.len() {
        // No getrandom: fall back to a time-seeded xorshift.
        let mut x = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x9e37_79b9_7f4a_7c15, |d| d.as_nanos() as u64)
            | 1;
        for b in &mut buf[filled..] {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
    }
}

// ------------------------------------------------------------ mutexes

const SQLITE_MUTEX_FAST: c_int = 0;
const SQLITE_MUTEX_RECURSIVE: c_int = 1;
/// SQLITE_MUTEX_STATIC_MAIN (2) through SQLITE_MUTEX_STATIC_VFS3 (13).
const N_STATIC: usize = 12;

#[derive(Default)]
struct MutexState {
    owner: Option<ThreadId>,
    count: u32,
}

/// `sqlite3_mutex`: a fast or recursive mutex with owner tracking.
pub struct sqlite3_mutex {
    recursive: bool,
    is_static: bool,
    state: Mutex<MutexState>,
    cond: Condvar,
}

impl sqlite3_mutex {
    fn new(recursive: bool, is_static: bool) -> Self {
        Self {
            recursive,
            is_static,
            state: Mutex::new(MutexState::default()),
            cond: Condvar::new(),
        }
    }

    fn try_lock(&self, st: &mut MutexState) -> bool {
        let me = std::thread::current().id();
        match st.owner {
            None => {
                st.owner = Some(me);
                st.count = 1;
                true
            }
            Some(o) if o == me && self.recursive => {
                st.count += 1;
                true
            }
            _ => false,
        }
    }
}

fn static_mutexes() -> &'static [sqlite3_mutex; N_STATIC] {
    static M: OnceLock<[sqlite3_mutex; N_STATIC]> = OnceLock::new();
    M.get_or_init(|| std::array::from_fn(|_| sqlite3_mutex::new(false, true)))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_mutex_alloc(id: c_int) -> *mut sqlite3_mutex {
    match id {
        SQLITE_MUTEX_FAST | SQLITE_MUTEX_RECURSIVE => Box::into_raw(Box::new(sqlite3_mutex::new(
            id == SQLITE_MUTEX_RECURSIVE,
            false,
        ))),
        2..=13 => &static_mutexes()[(id - 2) as usize] as *const sqlite3_mutex as *mut _,
        _ => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_mutex_free(m: *mut sqlite3_mutex) {
    if m.is_null() || (*m).is_static {
        return;
    }
    drop(Box::from_raw(m));
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_mutex_enter(m: *mut sqlite3_mutex) {
    if m.is_null() {
        return;
    }
    let m = &*m;
    let mut st = m.state.lock().unwrap();
    while !m.try_lock(&mut st) {
        st = m.cond.wait(st).unwrap();
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_mutex_try(m: *mut sqlite3_mutex) -> c_int {
    if m.is_null() {
        return SQLITE_OK;
    }
    let m = &*m;
    let mut st = m.state.lock().unwrap();
    if m.try_lock(&mut st) {
        SQLITE_OK
    } else {
        SQLITE_BUSY
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_mutex_leave(m: *mut sqlite3_mutex) {
    if m.is_null() {
        return;
    }
    let m = &*m;
    let mut st = m.state.lock().unwrap();
    if st.owner == Some(std::thread::current().id()) {
        st.count -= 1;
        if st.count == 0 {
            st.owner = None;
            m.cond.notify_one();
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_mutex_held(m: *mut sqlite3_mutex) -> c_int {
    if m.is_null() {
        return 1;
    }
    ((*m).state.lock().unwrap().owner == Some(std::thread::current().id())) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_mutex_notheld(m: *mut sqlite3_mutex) -> c_int {
    if m.is_null() {
        return 1;
    }
    ((*m).state.lock().unwrap().owner != Some(std::thread::current().id())) as c_int
}

// ------------------------------------------------------------ sqlite_stmt

const STMT_SCHEMA: &str = "CREATE TABLE x(sql,ncol,ro,busy,nscan,nsort,naidx,nstep,reprep,run,mem)";
/// SQLITE_STMTSTATUS_* for columns nscan..mem.
const STMT_STATUS_OPS: [c_int; 7] = [1, 2, 3, 4, 5, 6, 99];

/// The `sqlite_stmt` table of ext/misc/stmt.c: one row per prepared
/// statement of the connection.
#[derive(Debug)]
struct StmtModule {
    db: usize,
}

#[derive(Debug)]
struct StmtTable {
    db: usize,
}

impl VirtualTableModule for StmtModule {
    type Table = StmtTable;

    fn schema(&self, _args: &[Value]) -> turso_core::Result<String> {
        Ok(STMT_SCHEMA.to_string())
    }

    fn create(&self, _args: &[Value]) -> turso_core::Result<StmtTable> {
        Ok(StmtTable { db: self.db })
    }
}

impl VirtualTable for StmtTable {
    type Cursor = StmtCursor;

    fn open(&self, _connection: Arc<Connection>) -> turso_core::Result<StmtCursor> {
        Ok(StmtCursor {
            db: self.db,
            rows: Vec::new(),
            pos: 0,
        })
    }

    fn best_index(
        &self,
        _constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        Err(ResultCode::Error)
    }

    fn best_index_plan(&self, req: &IndexRequest) -> turso_core::Result<Option<IndexPlan>> {
        let mut plan = IndexPlan::new(req.constraints.len());
        plan.estimated_cost = 500.0;
        plan.estimated_rows = 500;
        Ok(Some(plan))
    }
}

struct StmtCursor {
    db: usize,
    rows: Vec<(Option<String>, [i64; 10])>,
    pos: usize,
}

// SAFETY: the cursor only holds the handle address; SQLite's threading
// rules for the handle apply.
unsafe impl Send for StmtCursor {}
unsafe impl Sync for StmtCursor {}

impl VirtualTableCursor for StmtCursor {
    fn filter(
        &mut self,
        _args: &[Value],
        _idx_str: Option<&str>,
        _idx_num: i32,
    ) -> turso_core::types::IOResultOr<bool> {
        self.rows.clear();
        self.pos = 0;
        let db = self.db as *mut c_void;
        // SAFETY: the module lives on this handle's connection.
        unsafe {
            let mut p = sqlite3_next_stmt(db, std::ptr::null_mut());
            while !p.is_null() {
                let sql = sqlite3_sql(p);
                let sql =
                    (!sql.is_null()).then(|| CStr::from_ptr(sql).to_string_lossy().into_owned());
                let mut cols = [0i64; 10];
                cols[0] = sqlite3_column_count(p) as i64;
                cols[1] = sqlite3_stmt_readonly(p) as i64;
                cols[2] = sqlite3_stmt_busy(p) as i64;
                for (k, op) in STMT_STATUS_OPS.iter().enumerate() {
                    cols[3 + k] = sqlite3_stmt_status(p, *op, 0) as i64;
                }
                self.rows.push((sql, cols));
                p = sqlite3_next_stmt(db, p);
            }
        }
        Ok(IOResult::Done(!self.rows.is_empty()))
    }

    fn next(&mut self) -> turso_core::types::IOResultOr<bool> {
        self.pos += 1;
        Ok(IOResult::Done(self.pos < self.rows.len()))
    }

    fn column(&mut self, column: usize) -> turso_core::types::IOResultOr<Value> {
        let Some((sql, cols)) = self.rows.get(self.pos) else {
            return Ok(IOResult::Done(Value::Null));
        };
        let v = match column {
            0 => sql.clone().map(Value::from_text).unwrap_or(Value::Null),
            1..=10 => Value::from_i64(cols[column - 1]),
            _ => Value::Null,
        };
        Ok(IOResult::Done(v))
    }

    fn rowid(&self) -> i64 {
        self.pos as i64 + 1
    }
}
