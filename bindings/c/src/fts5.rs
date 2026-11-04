//! SQLite's `fts5_api` C interface (custom tokenizers and auxiliary
//! functions) over turso's FTS5, and the pointer-passing interfaces it
//! needs: sqlite3_bind_pointer, sqlite3_value_pointer, sqlite3_result_pointer.

use crate::{sqlite3, sqlite3_stmt, SqliteContext, SQLITE_ERROR, SQLITE_MISUSE, SQLITE_OK};
use std::collections::HashMap;
use std::ffi::{self, CStr, CString};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use turso_core::fts5_api::{
    find_tokenizer, register_aux_function, register_tokenizer, ApiError, ApiResult, AuxApi,
    AuxFunction, TokenCallback, Tokenizer, TokenizerModule,
};
use turso_core::{Connection, LimboError, Value};
use turso_ext::Value as ExtValue;

const SQLITE_DONE: ffi::c_int = 101;
const SQLITE_NOMEM: ffi::c_int = 7;

// ------------------------------------------------------------------ pointers

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
    install_hook();
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
    let ctx = &mut *(ctx as *mut SqliteContext);
    ctx.result = ExtValue::null();
    let ty = if ty.is_null() {
        &[][..]
    } else {
        CStr::from_ptr(ty).to_bytes()
    };
    turso_core::pointer::set_result_pointer(p as usize, ty, destructor);
}

// ------------------------------------------------------------------ fts5_api

type XCreate = unsafe extern "C" fn(
    *mut ffi::c_void,
    *mut *const ffi::c_char,
    ffi::c_int,
    *mut *mut ffi::c_void,
) -> ffi::c_int;
type XDelete = unsafe extern "C" fn(*mut ffi::c_void);
type XToken = unsafe extern "C" fn(
    *mut ffi::c_void,
    ffi::c_int,
    *const ffi::c_char,
    ffi::c_int,
    ffi::c_int,
    ffi::c_int,
) -> ffi::c_int;
type XTokenizeV1 = unsafe extern "C" fn(
    *mut ffi::c_void,
    *mut ffi::c_void,
    ffi::c_int,
    *const ffi::c_char,
    ffi::c_int,
    XToken,
) -> ffi::c_int;
type XTokenizeV2 = unsafe extern "C" fn(
    *mut ffi::c_void,
    *mut ffi::c_void,
    ffi::c_int,
    *const ffi::c_char,
    ffi::c_int,
    *const ffi::c_char,
    ffi::c_int,
    XToken,
) -> ffi::c_int;
type XDestroy = unsafe extern "C" fn(*mut ffi::c_void);
type XAuxFunc = unsafe extern "C" fn(
    *const Fts5ExtensionApi,
    *mut ffi::c_void,
    *mut ffi::c_void,
    ffi::c_int,
    *mut *mut ffi::c_void,
);

#[repr(C)]
pub struct fts5_tokenizer {
    x_create: Option<XCreate>,
    x_delete: Option<XDelete>,
    x_tokenize: Option<XTokenizeV1>,
}

#[repr(C)]
pub struct fts5_tokenizer_v2 {
    i_version: ffi::c_int,
    x_create: Option<XCreate>,
    x_delete: Option<XDelete>,
    x_tokenize: Option<XTokenizeV2>,
}

#[repr(C)]
pub struct fts5_api {
    i_version: ffi::c_int,
    x_create_tokenizer: unsafe extern "C" fn(
        *mut fts5_api,
        *const ffi::c_char,
        *mut ffi::c_void,
        *mut fts5_tokenizer,
        Option<XDestroy>,
    ) -> ffi::c_int,
    x_find_tokenizer: unsafe extern "C" fn(
        *mut fts5_api,
        *const ffi::c_char,
        *mut *mut ffi::c_void,
        *mut fts5_tokenizer,
    ) -> ffi::c_int,
    x_create_function: unsafe extern "C" fn(
        *mut fts5_api,
        *const ffi::c_char,
        *mut ffi::c_void,
        Option<XAuxFunc>,
        Option<XDestroy>,
    ) -> ffi::c_int,
    x_create_tokenizer_v2: unsafe extern "C" fn(
        *mut fts5_api,
        *const ffi::c_char,
        *mut ffi::c_void,
        *mut fts5_tokenizer_v2,
        Option<XDestroy>,
    ) -> ffi::c_int,
    x_find_tokenizer_v2: unsafe extern "C" fn(
        *mut fts5_api,
        *const ffi::c_char,
        *mut *mut ffi::c_void,
        *mut *mut fts5_tokenizer_v2,
    ) -> ffi::c_int,
}

/// One fts5_api per connection; `api` must stay the first field.
#[repr(C)]
struct ApiHandle {
    api: fts5_api,
    conn: Weak<Connection>,
}

static HANDLES: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();

fn api_for(conn: &Arc<Connection>) -> *mut fts5_api {
    let mut map = HANDLES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    let key = Arc::as_ptr(conn) as usize;
    if let Some(&p) = map.get(&key) {
        let h = p as *mut ApiHandle;
        // The address can be reused by a new connection.
        if unsafe { (*h).conn.upgrade() }.is_some_and(|c| Arc::ptr_eq(&c, conn)) {
            return h as *mut fts5_api;
        }
    }
    let h = Box::into_raw(Box::new(ApiHandle {
        api: fts5_api {
            i_version: 3,
            x_create_tokenizer,
            x_find_tokenizer,
            x_create_function,
            x_create_tokenizer_v2,
            x_find_tokenizer_v2,
        },
        conn: Arc::downgrade(conn),
    }));
    map.insert(key, h as usize);
    h as *mut fts5_api
}

/// `SELECT fts5(?)` writes the fts5_api pointer through a pointer bound
/// with type "fts5_api_ptr".
fn fts5_hook(conn: &Arc<Connection>, pp: usize) {
    if pp != 0 {
        unsafe { *(pp as *mut *mut fts5_api) = api_for(conn) };
    }
}

pub(crate) fn install_hook() {
    turso_core::fts5_api::set_fts5_function_hook(fts5_hook);
}

unsafe fn c_args(az: *mut *const ffi::c_char, n: ffi::c_int) -> Vec<String> {
    (0..n.max(0) as usize)
        .map(|i| {
            let p = *az.add(i);
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        })
        .collect()
}

unsafe fn c_name(z: *const ffi::c_char) -> Option<String> {
    (!z.is_null()).then(|| CStr::from_ptr(z).to_string_lossy().into_owned())
}

/// A tokenizer module implemented in C.
struct CModule {
    user: usize,
    create: XCreate,
    delete: XDelete,
    tokenize: CTokenize,
    destroy: Option<XDestroy>,
}

#[derive(Clone, Copy)]
enum CTokenize {
    V1(XTokenizeV1),
    V2(XTokenizeV2),
}

impl Drop for CModule {
    fn drop(&mut self) {
        if let Some(d) = self.destroy {
            unsafe { d(self.user as *mut ffi::c_void) };
        }
    }
}

struct CTok {
    p: usize,
    delete: XDelete,
    tokenize: CTokenize,
    _module: Arc<CModule>,
}

impl Drop for CTok {
    fn drop(&mut self) {
        unsafe { (self.delete)(self.p as *mut ffi::c_void) };
    }
}

/// Passes tokens from a C tokenizer to the Rust callback.
unsafe extern "C" fn token_trampoline(
    ctx: *mut ffi::c_void,
    tflags: ffi::c_int,
    tok: *const ffi::c_char,
    n: ffi::c_int,
    start: ffi::c_int,
    end: ffi::c_int,
) -> ffi::c_int {
    let cb = &mut *(ctx as *mut &mut TokenCallback);
    let t = if tok.is_null() || n <= 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(tok.cast::<u8>(), n as usize)
    };
    cb(tflags, t, start, end)
}

impl Tokenizer for CTok {
    fn tokenize(
        &self,
        flags: i32,
        text: &[u8],
        locale: Option<&[u8]>,
        cb: &mut TokenCallback,
    ) -> i32 {
        let mut cbref: &mut TokenCallback = cb;
        let ctx = &mut cbref as *mut &mut TokenCallback as *mut ffi::c_void;
        unsafe {
            match self.tokenize {
                CTokenize::V1(f) => f(
                    self.p as *mut ffi::c_void,
                    ctx,
                    flags,
                    text.as_ptr().cast(),
                    text.len() as ffi::c_int,
                    token_trampoline,
                ),
                CTokenize::V2(f) => f(
                    self.p as *mut ffi::c_void,
                    ctx,
                    flags,
                    text.as_ptr().cast(),
                    text.len() as ffi::c_int,
                    locale.map_or(std::ptr::null(), |l| l.as_ptr().cast()),
                    locale.map_or(0, |l| l.len() as ffi::c_int),
                    token_trampoline,
                ),
            }
        }
    }
}

struct CModuleRef(Arc<CModule>);

// SAFETY: the C tokenizer's thread safety is the caller's responsibility, as in SQLite.
unsafe impl Send for CModule {}
unsafe impl Sync for CModule {}
unsafe impl Send for CTok {}
unsafe impl Sync for CTok {}

impl TokenizerModule for CModuleRef {
    fn create(&self, args: &[String]) -> Result<Box<dyn Tokenizer>, i32> {
        let m = &self.0;
        let cargs: Vec<CString> = args
            .iter()
            .map(|a| CString::new(a.as_str()).unwrap_or_default())
            .collect();
        let mut ptrs: Vec<*const ffi::c_char> = cargs.iter().map(|c| c.as_ptr()).collect();
        let mut out: *mut ffi::c_void = std::ptr::null_mut();
        let rc = unsafe {
            (m.create)(
                m.user as *mut ffi::c_void,
                if ptrs.is_empty() {
                    std::ptr::null_mut()
                } else {
                    ptrs.as_mut_ptr()
                },
                ptrs.len() as ffi::c_int,
                &mut out,
            )
        };
        if rc != SQLITE_OK {
            return Err(rc);
        }
        Ok(Box::new(CTok {
            p: out as usize,
            delete: m.delete,
            tokenize: m.tokenize,
            _module: m.clone(),
        }))
    }
}

unsafe extern "C" fn x_create_tokenizer(
    _api: *mut fts5_api,
    name: *const ffi::c_char,
    user: *mut ffi::c_void,
    tok: *mut fts5_tokenizer,
    destroy: Option<XDestroy>,
) -> ffi::c_int {
    let (Some(name), false) = (c_name(name), tok.is_null()) else {
        return SQLITE_MISUSE;
    };
    let t = &*tok;
    let (Some(create), Some(delete), Some(tokenize)) = (t.x_create, t.x_delete, t.x_tokenize)
    else {
        return SQLITE_MISUSE;
    };
    let m = Arc::new(CModule {
        user: user as usize,
        create,
        delete,
        tokenize: CTokenize::V1(tokenize),
        destroy,
    });
    register_tokenizer(&name, Arc::new(CModuleRef(m)));
    SQLITE_OK
}

unsafe extern "C" fn x_create_tokenizer_v2(
    _api: *mut fts5_api,
    name: *const ffi::c_char,
    user: *mut ffi::c_void,
    tok: *mut fts5_tokenizer_v2,
    destroy: Option<XDestroy>,
) -> ffi::c_int {
    let (Some(name), false) = (c_name(name), tok.is_null()) else {
        return SQLITE_MISUSE;
    };
    let t = &*tok;
    if t.i_version != 2 {
        return SQLITE_ERROR;
    }
    let (Some(create), Some(delete), Some(tokenize)) = (t.x_create, t.x_delete, t.x_tokenize)
    else {
        return SQLITE_MISUSE;
    };
    let m = Arc::new(CModule {
        user: user as usize,
        create,
        delete,
        tokenize: CTokenize::V2(tokenize),
        destroy,
    });
    register_tokenizer(&name, Arc::new(CModuleRef(m)));
    SQLITE_OK
}

/// A Rust tokenizer handed to C through xFindTokenizer.
struct RustTok(Box<dyn Tokenizer>);

unsafe extern "C" fn rust_tok_create(
    user: *mut ffi::c_void,
    az: *mut *const ffi::c_char,
    n: ffi::c_int,
    out: *mut *mut ffi::c_void,
) -> ffi::c_int {
    let module = &*(user as *const Arc<dyn TokenizerModule>);
    match module.create(&c_args(az, n)) {
        Ok(t) => {
            *out = Box::into_raw(Box::new(RustTok(t))) as *mut ffi::c_void;
            SQLITE_OK
        }
        Err(rc) => {
            *out = std::ptr::null_mut();
            rc
        }
    }
}

unsafe extern "C" fn rust_tok_delete(p: *mut ffi::c_void) {
    if !p.is_null() {
        drop(Box::from_raw(p as *mut RustTok));
    }
}

unsafe fn rust_tokenize(
    p: *mut ffi::c_void,
    ctx: *mut ffi::c_void,
    flags: ffi::c_int,
    text: *const ffi::c_char,
    n: ffi::c_int,
    locale: Option<&[u8]>,
    x_token: XToken,
) -> ffi::c_int {
    let tok = &*(p as *const RustTok);
    let text = if text.is_null() || n <= 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(text.cast::<u8>(), n as usize)
    };
    tok.0.tokenize(flags, text, locale, &mut |tf, t, s, e| {
        x_token(ctx, tf, t.as_ptr().cast(), t.len() as ffi::c_int, s, e)
    })
}

unsafe extern "C" fn rust_tok_tokenize_v1(
    p: *mut ffi::c_void,
    ctx: *mut ffi::c_void,
    flags: ffi::c_int,
    text: *const ffi::c_char,
    n: ffi::c_int,
    x_token: XToken,
) -> ffi::c_int {
    rust_tokenize(p, ctx, flags, text, n, None, x_token)
}

unsafe extern "C" fn rust_tok_tokenize_v2(
    p: *mut ffi::c_void,
    ctx: *mut ffi::c_void,
    flags: ffi::c_int,
    text: *const ffi::c_char,
    n: ffi::c_int,
    loc: *const ffi::c_char,
    nloc: ffi::c_int,
    x_token: XToken,
) -> ffi::c_int {
    let locale = (!loc.is_null() && nloc > 0)
        .then(|| std::slice::from_raw_parts(loc.cast::<u8>(), nloc as usize));
    rust_tokenize(p, ctx, flags, text, n, locale, x_token)
}

static RUST_TOKENIZER_V2: fts5_tokenizer_v2 = fts5_tokenizer_v2 {
    i_version: 2,
    x_create: Some(rust_tok_create),
    x_delete: Some(rust_tok_delete),
    x_tokenize: Some(rust_tok_tokenize_v2),
};

/// User data for a found tokenizer; kept for the life of the process.
fn module_handle(name: Option<String>) -> Option<*mut ffi::c_void> {
    static FOUND: OnceLock<Mutex<HashMap<Option<String>, usize>>> = OnceLock::new();
    let module = find_tokenizer(name.as_deref())?;
    let mut map = FOUND
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    let key = name.map(|n| n.to_ascii_lowercase());
    // A re-registered tokenizer gets a new handle.
    let p = Box::into_raw(Box::new(module)) as usize;
    map.insert(key, p);
    Some(p as *mut ffi::c_void)
}

unsafe extern "C" fn x_find_tokenizer(
    _api: *mut fts5_api,
    name: *const ffi::c_char,
    user: *mut *mut ffi::c_void,
    tok: *mut fts5_tokenizer,
) -> ffi::c_int {
    let Some(h) = module_handle(c_name(name)) else {
        if !user.is_null() {
            *user = std::ptr::null_mut();
        }
        return SQLITE_ERROR;
    };
    if !user.is_null() {
        *user = h;
    }
    if !tok.is_null() {
        *tok = fts5_tokenizer {
            x_create: Some(rust_tok_create),
            x_delete: Some(rust_tok_delete),
            x_tokenize: Some(rust_tok_tokenize_v1),
        };
    }
    SQLITE_OK
}

unsafe extern "C" fn x_find_tokenizer_v2(
    _api: *mut fts5_api,
    name: *const ffi::c_char,
    user: *mut *mut ffi::c_void,
    tok: *mut *mut fts5_tokenizer_v2,
) -> ffi::c_int {
    let Some(h) = module_handle(c_name(name)) else {
        if !user.is_null() {
            *user = std::ptr::null_mut();
        }
        if !tok.is_null() {
            *tok = std::ptr::null_mut();
        }
        return SQLITE_ERROR;
    };
    if !user.is_null() {
        *user = h;
    }
    if !tok.is_null() {
        *tok = &RUST_TOKENIZER_V2 as *const fts5_tokenizer_v2 as *mut fts5_tokenizer_v2;
    }
    SQLITE_OK
}

// ------------------------------------------------------------------ auxiliary functions

/// An auxiliary function implemented in C.
struct CAux {
    user: usize,
    func: XAuxFunc,
    destroy: Option<XDestroy>,
}

// SAFETY: as for scalar functions, thread safety is the caller's responsibility.
unsafe impl Send for CAux {}
unsafe impl Sync for CAux {}

impl Drop for CAux {
    fn drop(&mut self) {
        if let Some(d) = self.destroy {
            unsafe { d(self.user as *mut ffi::c_void) };
        }
    }
}

/// The Fts5Context handed to C: the row being processed.
struct FtsCtx<'a> {
    api: &'a mut dyn AuxApi,
    user: *mut ffi::c_void,
    /// Buffers whose addresses were returned to C; valid for the call.
    bufs: Vec<Vec<u8>>,
    ncol: i32,
}

impl FtsCtx<'_> {
    fn keep(&mut self, b: Vec<u8>) -> (*const ffi::c_char, ffi::c_int) {
        let n = b.len() as ffi::c_int;
        let mut b = b;
        b.push(0);
        self.bufs.push(b);
        (self.bufs.last().unwrap().as_ptr().cast(), n)
    }
}

fn api_rc(e: ApiError) -> ffi::c_int {
    match e {
        ApiError::Code(c) => c,
        ApiError::Err(LimboError::OutOfMemory) => SQLITE_NOMEM,
        ApiError::Err(LimboError::CorruptVtab(_)) | ApiError::Err(LimboError::Corrupt(_)) => 267,
        ApiError::Err(_) => SQLITE_ERROR,
    }
}

impl AuxFunction for CAux {
    fn call(&self, api: &mut dyn AuxApi, args: &[Value]) -> turso_core::Result<Value> {
        let ncol = api.column_count();
        let mut fctx = FtsCtx {
            api,
            user: self.user as *mut ffi::c_void,
            bufs: Vec::new(),
            ncol,
        };
        let mut ext: Vec<ExtValue> = args.iter().map(|v| v.to_ffi()).collect();
        let mut argv: Vec<*mut ffi::c_void> = ext
            .iter_mut()
            .map(|v| v as *mut ExtValue as *mut ffi::c_void)
            .collect();
        let mut ctx = SqliteContext {
            result: ExtValue::null(),
            p_app: self.user as *mut ffi::c_void,
            db: std::ptr::null_mut::<sqlite3>(),
        };
        unsafe {
            (self.func)(
                &EXTENSION_API,
                &mut fctx as *mut FtsCtx as *mut ffi::c_void,
                &mut ctx as *mut SqliteContext as *mut ffi::c_void,
                argv.len() as ffi::c_int,
                if argv.is_empty() {
                    std::ptr::null_mut()
                } else {
                    argv.as_mut_ptr()
                },
            );
        }
        for v in ext {
            unsafe { v.__free_internal_type() };
        }
        let result = std::mem::replace(&mut ctx.result, ExtValue::null());
        if result.value_type() == turso_ext::ValueType::Error {
            let msg = result
                .to_error_details()
                .and_then(|(_, m)| m)
                .unwrap_or_default();
            unsafe { result.__free_internal_type() };
            return Err(LimboError::SqlError(msg));
        }
        Value::from_ffi(result)
    }
}

unsafe extern "C" fn x_create_function(
    api: *mut fts5_api,
    name: *const ffi::c_char,
    user: *mut ffi::c_void,
    func: Option<XAuxFunc>,
    destroy: Option<XDestroy>,
) -> ffi::c_int {
    let (Some(name), Some(func), false) = (c_name(name), func, api.is_null()) else {
        return SQLITE_MISUSE;
    };
    let h = &*(api as *const ApiHandle);
    let Some(conn) = h.conn.upgrade() else {
        return SQLITE_MISUSE;
    };
    let f = Arc::new(CAux {
        user: user as usize,
        func,
        destroy,
    });
    match register_aux_function(&conn, &name, f) {
        Ok(()) => SQLITE_OK,
        Err(_) => SQLITE_ERROR,
    }
}

#[repr(C)]
pub struct Fts5PhraseIter {
    a: *const u8,
    b: *const u8,
}

type CtxP = *mut ffi::c_void;

unsafe fn fctx<'a>(p: CtxP) -> &'a mut FtsCtx<'a> {
    &mut *(p as *mut FtsCtx)
}

unsafe extern "C" fn x_user_data(p: CtxP) -> *mut ffi::c_void {
    fctx(p).user
}

unsafe extern "C" fn x_column_count(p: CtxP) -> ffi::c_int {
    fctx(p).api.column_count()
}

unsafe extern "C" fn x_row_count(p: CtxP, n: *mut i64) -> ffi::c_int {
    match fctx(p).api.row_count() {
        Ok(v) => {
            *n = v;
            SQLITE_OK
        }
        Err(e) => api_rc(e),
    }
}

unsafe extern "C" fn x_column_total_size(p: CtxP, col: ffi::c_int, n: *mut i64) -> ffi::c_int {
    match fctx(p).api.column_total_size(col) {
        Ok(v) => {
            *n = v;
            SQLITE_OK
        }
        Err(e) => {
            *n = 0;
            api_rc(e)
        }
    }
}

unsafe fn do_tokenize(
    p: CtxP,
    text: *const ffi::c_char,
    n: ffi::c_int,
    locale: Option<&[u8]>,
    ctx: *mut ffi::c_void,
    x_token: Option<XToken>,
) -> ffi::c_int {
    let Some(x_token) = x_token else {
        return SQLITE_MISUSE;
    };
    let text = if text.is_null() || n <= 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(text.cast::<u8>(), n as usize)
    };
    match fctx(p).api.tokenize(text, locale, &mut |tf, t, s, e| {
        x_token(ctx, tf, t.as_ptr().cast(), t.len() as ffi::c_int, s, e)
    }) {
        Ok(()) => SQLITE_OK,
        Err(e) => api_rc(e),
    }
}

unsafe extern "C" fn x_tokenize(
    p: CtxP,
    text: *const ffi::c_char,
    n: ffi::c_int,
    ctx: *mut ffi::c_void,
    x_token: Option<XToken>,
) -> ffi::c_int {
    do_tokenize(p, text, n, None, ctx, x_token)
}

unsafe extern "C" fn x_tokenize_v2(
    p: CtxP,
    text: *const ffi::c_char,
    n: ffi::c_int,
    loc: *const ffi::c_char,
    nloc: ffi::c_int,
    ctx: *mut ffi::c_void,
    x_token: Option<XToken>,
) -> ffi::c_int {
    let locale = (!loc.is_null() && nloc > 0)
        .then(|| std::slice::from_raw_parts(loc.cast::<u8>(), nloc as usize));
    do_tokenize(p, text, n, locale, ctx, x_token)
}

unsafe extern "C" fn x_phrase_count(p: CtxP) -> ffi::c_int {
    fctx(p).api.phrase_count()
}

unsafe extern "C" fn x_phrase_size(p: CtxP, i: ffi::c_int) -> ffi::c_int {
    fctx(p).api.phrase_size(i)
}

unsafe extern "C" fn x_inst_count(p: CtxP, n: *mut ffi::c_int) -> ffi::c_int {
    match fctx(p).api.inst_count() {
        Ok(v) => {
            *n = v;
            SQLITE_OK
        }
        Err(e) => api_rc(e),
    }
}

unsafe extern "C" fn x_inst(
    p: CtxP,
    i: ffi::c_int,
    ip: *mut ffi::c_int,
    ic: *mut ffi::c_int,
    io: *mut ffi::c_int,
) -> ffi::c_int {
    match fctx(p).api.inst(i) {
        Ok((a, b, c)) => {
            *ip = a;
            *ic = b;
            *io = c;
            SQLITE_OK
        }
        Err(e) => api_rc(e),
    }
}

unsafe extern "C" fn x_rowid(p: CtxP) -> i64 {
    fctx(p).api.rowid()
}

unsafe extern "C" fn x_column_text(
    p: CtxP,
    col: ffi::c_int,
    pz: *mut *const ffi::c_char,
    pn: *mut ffi::c_int,
) -> ffi::c_int {
    let c = fctx(p);
    match c.api.column_text(col) {
        Ok(Some(t)) => {
            let (z, n) = c.keep(t);
            *pz = z;
            *pn = n;
            SQLITE_OK
        }
        Ok(None) => {
            *pz = std::ptr::null();
            *pn = 0;
            SQLITE_OK
        }
        Err(e) => api_rc(e),
    }
}

unsafe extern "C" fn x_column_size(p: CtxP, col: ffi::c_int, n: *mut ffi::c_int) -> ffi::c_int {
    match fctx(p).api.column_size(col) {
        Ok(v) => {
            *n = v;
            SQLITE_OK
        }
        Err(e) => {
            *n = 0;
            api_rc(e)
        }
    }
}

type XQueryCb = unsafe extern "C" fn(*const Fts5ExtensionApi, CtxP, *mut ffi::c_void) -> ffi::c_int;

unsafe extern "C" fn x_query_phrase(
    p: CtxP,
    phrase: ffi::c_int,
    user: *mut ffi::c_void,
    cb: Option<XQueryCb>,
) -> ffi::c_int {
    let Some(cb) = cb else {
        return SQLITE_MISUSE;
    };
    let c = fctx(p);
    let outer_user = c.user;
    let r = c.api.query_phrase(phrase, &mut |sub: &mut dyn AuxApi| {
        let ncol = sub.column_count();
        let mut sctx = FtsCtx {
            api: sub,
            user: outer_user,
            bufs: Vec::new(),
            ncol,
        };
        let rc = cb(&EXTENSION_API, &mut sctx as *mut FtsCtx as CtxP, user);
        match rc {
            SQLITE_OK => Ok(true),
            SQLITE_DONE => Ok(false),
            rc => Err(ApiError::Code(rc)),
        }
    });
    match r {
        Ok(()) => SQLITE_OK,
        Err(e) => api_rc(e),
    }
}

/// C auxdata: a pointer with its destructor.
struct CAuxdata {
    p: usize,
    del: Option<XDelete>,
}

impl Drop for CAuxdata {
    fn drop(&mut self) {
        if let Some(d) = self.del {
            unsafe { d(self.p as *mut ffi::c_void) };
        }
    }
}

unsafe extern "C" fn x_set_auxdata(
    p: CtxP,
    aux: *mut ffi::c_void,
    del: Option<XDelete>,
) -> ffi::c_int {
    fctx(p).api.set_auxdata(Box::new(CAuxdata {
        p: aux as usize,
        del,
    }));
    SQLITE_OK
}

unsafe extern "C" fn x_get_auxdata(p: CtxP, clear: ffi::c_int) -> *mut ffi::c_void {
    let c = fctx(p);
    if clear != 0 {
        let Some(d) = c.api.take_auxdata() else {
            return std::ptr::null_mut();
        };
        match d.downcast::<CAuxdata>() {
            Ok(mut d) => {
                // Clearing hands ownership back without running the destructor.
                d.del = None;
                d.p as *mut ffi::c_void
            }
            Err(_) => std::ptr::null_mut(),
        }
    } else {
        c.api
            .auxdata()
            .and_then(|d| d.downcast_ref::<CAuxdata>())
            .map_or(std::ptr::null_mut(), |d| d.p as *mut ffi::c_void)
    }
}

/// fts5ApiPhraseNext
unsafe extern "C" fn x_phrase_next(
    p: CtxP,
    it: *mut Fts5PhraseIter,
    col: *mut ffi::c_int,
    off: *mut ffi::c_int,
) {
    let it = &mut *it;
    if it.a.is_null() || it.a >= it.b {
        *col = -1;
        *off = -1;
        return;
    }
    let ncol = fctx(p).ncol;
    let mut v = read_varint32(&mut it.a);
    if v == 1 {
        v = read_varint32(&mut it.a);
        *col = if v as i32 >= ncol { ncol - 1 } else { v as i32 };
        *off = 0;
        v = read_varint32(&mut it.a);
    }
    *off += v as i32 - 2;
}

unsafe fn read_varint32(a: &mut *const u8) -> u32 {
    let mut v: u64 = 0;
    for i in 0..9 {
        let b = **a as u64;
        *a = a.add(1);
        if i == 8 {
            v = (v << 8) | b;
            break;
        }
        v = (v << 7) | (b & 0x7f);
        if b & 0x80 == 0 {
            break;
        }
    }
    v as u32
}

unsafe extern "C" fn x_phrase_first(
    p: CtxP,
    phrase: ffi::c_int,
    it: *mut Fts5PhraseIter,
    col: *mut ffi::c_int,
    off: *mut ffi::c_int,
) -> ffi::c_int {
    let c = fctx(p);
    match c.api.phrase_poslist(phrase) {
        Ok(pl) => {
            let n = pl.len();
            let (z, _) = c.keep(pl);
            (*it).a = z.cast();
            (*it).b = (z as *const u8).add(n);
            *col = 0;
            *off = 0;
            x_phrase_next(p, it, col, off);
            SQLITE_OK
        }
        Err(e) => api_rc(e),
    }
}

unsafe extern "C" fn x_phrase_next_column(p: CtxP, it: *mut Fts5PhraseIter, col: *mut ffi::c_int) {
    let it = &mut *it;
    if fctx(p).api.detail() == 2 {
        if it.a.is_null() || it.a >= it.b {
            *col = -1;
        } else {
            let incr = read_varint32(&mut it.a);
            *col += incr as i32 - 2;
        }
    } else {
        loop {
            if it.a.is_null() || it.a >= it.b {
                *col = -1;
                return;
            }
            if *it.a == 0x01 {
                break;
            }
            read_varint32(&mut it.a);
        }
        it.a = it.a.add(1);
        *col = read_varint32(&mut it.a) as i32;
    }
}

unsafe extern "C" fn x_phrase_first_column(
    p: CtxP,
    phrase: ffi::c_int,
    it: *mut Fts5PhraseIter,
    col: *mut ffi::c_int,
) -> ffi::c_int {
    let c = fctx(p);
    let columns = c.api.detail() == 2;
    let r = if columns {
        c.api.phrase_collist(phrase)
    } else {
        c.api.phrase_poslist(phrase)
    };
    match r {
        Ok(pl) => {
            let n = pl.len();
            let (z, _) = c.keep(pl);
            (*it).a = z.cast();
            (*it).b = (z as *const u8).add(n);
            if columns {
                *col = 0;
                x_phrase_next_column(p, it, col);
            } else if n == 0 {
                *col = -1;
            } else if *(*it).a == 0x01 {
                (*it).a = (*it).a.add(1);
                *col = read_varint32(&mut (*it).a) as i32;
            } else {
                *col = 0;
            }
            SQLITE_OK
        }
        Err(e) => api_rc(e),
    }
}

unsafe extern "C" fn x_query_token(
    p: CtxP,
    phrase: ffi::c_int,
    token: ffi::c_int,
    pz: *mut *const ffi::c_char,
    pn: *mut ffi::c_int,
) -> ffi::c_int {
    let c = fctx(p);
    match c.api.query_token(phrase, token) {
        Ok(t) => {
            let (z, n) = c.keep(t);
            *pz = z;
            *pn = n;
            SQLITE_OK
        }
        Err(e) => api_rc(e),
    }
}

unsafe extern "C" fn x_inst_token(
    p: CtxP,
    inst: ffi::c_int,
    token: ffi::c_int,
    pz: *mut *const ffi::c_char,
    pn: *mut ffi::c_int,
) -> ffi::c_int {
    let c = fctx(p);
    match c.api.inst_token(inst, token) {
        Ok(Some(t)) => {
            let (z, n) = c.keep(t);
            *pz = z;
            *pn = n;
            SQLITE_OK
        }
        Ok(None) => SQLITE_OK,
        Err(e) => api_rc(e),
    }
}

unsafe extern "C" fn x_column_locale(
    p: CtxP,
    col: ffi::c_int,
    pz: *mut *const ffi::c_char,
    pn: *mut ffi::c_int,
) -> ffi::c_int {
    let c = fctx(p);
    *pz = std::ptr::null();
    *pn = 0;
    match c.api.column_locale(col) {
        Ok(Some(l)) => {
            let (z, n) = c.keep(l);
            *pz = z;
            *pn = n;
            SQLITE_OK
        }
        Ok(None) => SQLITE_OK,
        Err(e) => api_rc(e),
    }
}

/// The Fts5ExtensionApi handed to auxiliary functions (version 4).
#[repr(C)]
pub struct Fts5ExtensionApi {
    i_version: ffi::c_int,
    x_user_data: unsafe extern "C" fn(CtxP) -> *mut ffi::c_void,
    x_column_count: unsafe extern "C" fn(CtxP) -> ffi::c_int,
    x_row_count: unsafe extern "C" fn(CtxP, *mut i64) -> ffi::c_int,
    x_column_total_size: unsafe extern "C" fn(CtxP, ffi::c_int, *mut i64) -> ffi::c_int,
    x_tokenize: unsafe extern "C" fn(
        CtxP,
        *const ffi::c_char,
        ffi::c_int,
        *mut ffi::c_void,
        Option<XToken>,
    ) -> ffi::c_int,
    x_phrase_count: unsafe extern "C" fn(CtxP) -> ffi::c_int,
    x_phrase_size: unsafe extern "C" fn(CtxP, ffi::c_int) -> ffi::c_int,
    x_inst_count: unsafe extern "C" fn(CtxP, *mut ffi::c_int) -> ffi::c_int,
    x_inst: unsafe extern "C" fn(
        CtxP,
        ffi::c_int,
        *mut ffi::c_int,
        *mut ffi::c_int,
        *mut ffi::c_int,
    ) -> ffi::c_int,
    x_rowid: unsafe extern "C" fn(CtxP) -> i64,
    x_column_text: unsafe extern "C" fn(
        CtxP,
        ffi::c_int,
        *mut *const ffi::c_char,
        *mut ffi::c_int,
    ) -> ffi::c_int,
    x_column_size: unsafe extern "C" fn(CtxP, ffi::c_int, *mut ffi::c_int) -> ffi::c_int,
    x_query_phrase:
        unsafe extern "C" fn(CtxP, ffi::c_int, *mut ffi::c_void, Option<XQueryCb>) -> ffi::c_int,
    x_set_auxdata: unsafe extern "C" fn(CtxP, *mut ffi::c_void, Option<XDelete>) -> ffi::c_int,
    x_get_auxdata: unsafe extern "C" fn(CtxP, ffi::c_int) -> *mut ffi::c_void,
    x_phrase_first: unsafe extern "C" fn(
        CtxP,
        ffi::c_int,
        *mut Fts5PhraseIter,
        *mut ffi::c_int,
        *mut ffi::c_int,
    ) -> ffi::c_int,
    x_phrase_next:
        unsafe extern "C" fn(CtxP, *mut Fts5PhraseIter, *mut ffi::c_int, *mut ffi::c_int),
    x_phrase_first_column:
        unsafe extern "C" fn(CtxP, ffi::c_int, *mut Fts5PhraseIter, *mut ffi::c_int) -> ffi::c_int,
    x_phrase_next_column: unsafe extern "C" fn(CtxP, *mut Fts5PhraseIter, *mut ffi::c_int),
    x_query_token: unsafe extern "C" fn(
        CtxP,
        ffi::c_int,
        ffi::c_int,
        *mut *const ffi::c_char,
        *mut ffi::c_int,
    ) -> ffi::c_int,
    x_inst_token: unsafe extern "C" fn(
        CtxP,
        ffi::c_int,
        ffi::c_int,
        *mut *const ffi::c_char,
        *mut ffi::c_int,
    ) -> ffi::c_int,
    x_column_locale: unsafe extern "C" fn(
        CtxP,
        ffi::c_int,
        *mut *const ffi::c_char,
        *mut ffi::c_int,
    ) -> ffi::c_int,
    x_tokenize_v2: unsafe extern "C" fn(
        CtxP,
        *const ffi::c_char,
        ffi::c_int,
        *const ffi::c_char,
        ffi::c_int,
        *mut ffi::c_void,
        Option<XToken>,
    ) -> ffi::c_int,
}

static EXTENSION_API: Fts5ExtensionApi = Fts5ExtensionApi {
    i_version: 4,
    x_user_data,
    x_column_count,
    x_row_count,
    x_column_total_size,
    x_tokenize,
    x_phrase_count,
    x_phrase_size,
    x_inst_count,
    x_inst,
    x_rowid,
    x_column_text,
    x_column_size,
    x_query_phrase,
    x_set_auxdata,
    x_get_auxdata,
    x_phrase_first,
    x_phrase_next,
    x_phrase_first_column,
    x_phrase_next_column,
    x_query_token,
    x_inst_token,
    x_column_locale,
    x_tokenize_v2,
};

#[allow(dead_code)]
fn _api_result(_: ApiResult<()>) {}
