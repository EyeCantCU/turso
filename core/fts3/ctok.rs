//! The C tokenizer interface (`fts3_tokenizer.h`): application-defined
//! `sqlite3_tokenizer_module`s, and C views of the built-in tokenizers so
//! `fts3_tokenizer('simple')` returns a usable module pointer.

#![allow(non_camel_case_types, non_snake_case, clippy::missing_safety_doc)]

use super::sql_error;
use super::tokenizer::{TokenSink, Tokenizer, TokenizerModule};
use crate::sync::Arc;
use crate::Result;
use std::ffi::{c_char, c_int, CString};

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_NOMEM: c_int = 7;
const SQLITE_DONE: c_int = 101;

#[repr(C)]
pub struct sqlite3_tokenizer_module {
    pub iVersion: c_int,
    pub xCreate: Option<
        unsafe extern "C" fn(c_int, *const *const c_char, *mut *mut sqlite3_tokenizer) -> c_int,
    >,
    pub xDestroy: Option<unsafe extern "C" fn(*mut sqlite3_tokenizer) -> c_int>,
    pub xOpen: Option<
        unsafe extern "C" fn(
            *mut sqlite3_tokenizer,
            *const c_char,
            c_int,
            *mut *mut sqlite3_tokenizer_cursor,
        ) -> c_int,
    >,
    pub xClose: Option<unsafe extern "C" fn(*mut sqlite3_tokenizer_cursor) -> c_int>,
    pub xNext: Option<
        unsafe extern "C" fn(
            *mut sqlite3_tokenizer_cursor,
            *mut *const c_char,
            *mut c_int,
            *mut c_int,
            *mut c_int,
            *mut c_int,
        ) -> c_int,
    >,
    pub xLanguageid: Option<unsafe extern "C" fn(*mut sqlite3_tokenizer_cursor, c_int) -> c_int>,
}

#[repr(C)]
pub struct sqlite3_tokenizer {
    pub pModule: *const sqlite3_tokenizer_module,
}

#[repr(C)]
pub struct sqlite3_tokenizer_cursor {
    pub pTokenizer: *mut sqlite3_tokenizer,
}

/// Address of a `sqlite3_tokenizer_module`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ModulePtr(pub usize);

impl ModulePtr {
    fn get(&self) -> &sqlite3_tokenizer_module {
        // SAFETY: registered through fts3_tokenizer(), which requires a
        // module that outlives its use, as in SQLite.
        unsafe { &*(self.0 as *const sqlite3_tokenizer_module) }
    }
}

/// A tokenizer instance created by a C module.
pub(crate) struct CTokenizer {
    module: ModulePtr,
    tok: *mut sqlite3_tokenizer,
}

// SAFETY: SQLite also uses tokenizer instances from whichever thread runs
// the statement; access is serialized by the connection.
unsafe impl Send for CTokenizer {}
unsafe impl Sync for CTokenizer {}

impl CTokenizer {
    pub(crate) fn create(module: ModulePtr, args: &[Vec<u8>]) -> Option<Self> {
        let m = module.get();
        let x_create = m.xCreate?;
        let cargs: Vec<CString> = args
            .iter()
            .map(|a| CString::new(a.clone()).unwrap_or_default())
            .collect();
        let argv: Vec<*const c_char> = cargs.iter().map(|c| c.as_ptr()).collect();
        let mut tok: *mut sqlite3_tokenizer = std::ptr::null_mut();
        // SAFETY: calling the module's own constructor with valid argv.
        let rc = unsafe { x_create(argv.len() as c_int, argv.as_ptr(), &mut tok) };
        if rc != SQLITE_OK || tok.is_null() {
            return None;
        }
        // SAFETY: tok was just created by the module.
        unsafe { (*tok).pModule = module.0 as *const sqlite3_tokenizer_module };
        Some(Self { module, tok })
    }
}

impl Drop for CTokenizer {
    fn drop(&mut self) {
        if let Some(x) = self.module.get().xDestroy {
            // SAFETY: tok was created by this module and is destroyed once.
            unsafe { x(self.tok) };
        }
    }
}

impl Tokenizer for CTokenizer {
    fn tokenize(&self, langid: i32, text: &[u8], f: &mut TokenSink) -> Result<()> {
        let m = self.module.get();
        let (Some(x_open), Some(x_next), Some(x_close)) = (m.xOpen, m.xNext, m.xClose) else {
            return Err(sql_error("SQL logic error"));
        };
        let mut cur: *mut sqlite3_tokenizer_cursor = std::ptr::null_mut();
        // An empty input still needs a valid pointer.
        let ptr = if text.is_empty() {
            c"".as_ptr()
        } else {
            text.as_ptr() as *const c_char
        };
        // SAFETY: text outlives the cursor, which is closed below.
        let rc = unsafe { x_open(self.tok, ptr, text.len() as c_int, &mut cur) };
        if rc != SQLITE_OK || cur.is_null() {
            return Err(rc_error(rc));
        }
        // SAFETY: cur is a live cursor of this tokenizer.
        unsafe { (*cur).pTokenizer = self.tok };
        let mut rc = SQLITE_OK;
        if m.iVersion >= 1 {
            if let Some(x_lang) = m.xLanguageid {
                // SAFETY: as above.
                rc = unsafe { x_lang(cur, langid) };
            }
        }
        let mut result = Ok(());
        if rc != SQLITE_OK {
            result = Err(rc_error(rc));
        } else {
            loop {
                let mut tok: *const c_char = std::ptr::null();
                let (mut n, mut s, mut e, mut p) = (0, 0, 0, 0);
                // SAFETY: as above; outputs are plain locals.
                let rc = unsafe { x_next(cur, &mut tok, &mut n, &mut s, &mut e, &mut p) };
                if rc == SQLITE_DONE {
                    break;
                }
                if rc != SQLITE_OK {
                    result = Err(rc_error(rc));
                    break;
                }
                let token = if tok.is_null() || n <= 0 {
                    &[][..]
                } else {
                    // SAFETY: the module returns n valid bytes at tok.
                    unsafe { std::slice::from_raw_parts(tok as *const u8, n as usize) }
                };
                match f(token, s, e, p) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(err) => {
                        result = Err(err);
                        break;
                    }
                }
            }
        }
        // SAFETY: closing the cursor opened above.
        unsafe { x_close(cur) };
        result
    }
}

fn rc_error(rc: c_int) -> crate::LimboError {
    match rc {
        SQLITE_NOMEM => crate::LimboError::SqlError("out of memory".to_string()),
        _ => sql_error("SQL logic error"),
    }
}

/// A built-in tokenizer seen through the C interface.
#[repr(C)]
struct BuiltinTok {
    base: sqlite3_tokenizer,
    tok: Arc<dyn Tokenizer>,
}

#[repr(C)]
struct BuiltinCursor {
    base: sqlite3_tokenizer_cursor,
    tokens: Vec<(Vec<u8>, i32, i32, i32)>,
    next: usize,
    langid: i32,
    input: Vec<u8>,
    done: bool,
}

unsafe fn builtin_create(
    which: TokenizerModule,
    argc: c_int,
    argv: *const *const c_char,
    pp: *mut *mut sqlite3_tokenizer,
) -> c_int {
    let mut args = Vec::new();
    for i in 0..argc.max(0) as usize {
        let p = *argv.add(i);
        args.push(if p.is_null() {
            Vec::new()
        } else {
            std::ffi::CStr::from_ptr(p).to_bytes().to_vec()
        });
    }
    let Some(tok) = which.create(&args) else {
        return SQLITE_ERROR;
    };
    let b = Box::new(BuiltinTok {
        base: sqlite3_tokenizer {
            pModule: std::ptr::null(),
        },
        tok,
    });
    *pp = Box::into_raw(b) as *mut sqlite3_tokenizer;
    SQLITE_OK
}

unsafe extern "C" fn simple_create(
    argc: c_int,
    argv: *const *const c_char,
    pp: *mut *mut sqlite3_tokenizer,
) -> c_int {
    builtin_create(TokenizerModule::Simple, argc, argv, pp)
}

unsafe extern "C" fn porter_create(
    argc: c_int,
    argv: *const *const c_char,
    pp: *mut *mut sqlite3_tokenizer,
) -> c_int {
    builtin_create(TokenizerModule::Porter, argc, argv, pp)
}

unsafe extern "C" fn unicode_create(
    argc: c_int,
    argv: *const *const c_char,
    pp: *mut *mut sqlite3_tokenizer,
) -> c_int {
    builtin_create(TokenizerModule::Unicode61, argc, argv, pp)
}

unsafe extern "C" fn builtin_destroy(p: *mut sqlite3_tokenizer) -> c_int {
    if !p.is_null() {
        drop(Box::from_raw(p as *mut BuiltinTok));
    }
    SQLITE_OK
}

unsafe extern "C" fn builtin_open(
    p: *mut sqlite3_tokenizer,
    input: *const c_char,
    n: c_int,
    pp: *mut *mut sqlite3_tokenizer_cursor,
) -> c_int {
    let input = if input.is_null() {
        Vec::new()
    } else if n < 0 {
        std::ffi::CStr::from_ptr(input).to_bytes().to_vec()
    } else {
        std::slice::from_raw_parts(input as *const u8, n as usize).to_vec()
    };
    let c = Box::new(BuiltinCursor {
        base: sqlite3_tokenizer_cursor { pTokenizer: p },
        tokens: Vec::new(),
        next: 0,
        langid: 0,
        input,
        done: false,
    });
    *pp = Box::into_raw(c) as *mut sqlite3_tokenizer_cursor;
    SQLITE_OK
}

unsafe extern "C" fn builtin_close(c: *mut sqlite3_tokenizer_cursor) -> c_int {
    if !c.is_null() {
        drop(Box::from_raw(c as *mut BuiltinCursor));
    }
    SQLITE_OK
}

unsafe extern "C" fn builtin_next(
    c: *mut sqlite3_tokenizer_cursor,
    tok: *mut *const c_char,
    n: *mut c_int,
    s: *mut c_int,
    e: *mut c_int,
    pos: *mut c_int,
) -> c_int {
    let c = &mut *(c as *mut BuiltinCursor);
    if !c.done {
        let t = &*(c.base.pTokenizer as *mut BuiltinTok);
        match super::tokenizer::tokens(t.tok.as_ref(), c.langid, &c.input) {
            Ok(v) => c.tokens = v,
            Err(_) => return SQLITE_ERROR,
        }
        c.done = true;
    }
    let Some(t) = c.tokens.get(c.next) else {
        return SQLITE_DONE;
    };
    c.next += 1;
    *tok = t.0.as_ptr() as *const c_char;
    *n = t.0.len() as c_int;
    *s = t.1;
    *e = t.2;
    *pos = t.3;
    SQLITE_OK
}

static SIMPLE_MODULE: sqlite3_tokenizer_module = sqlite3_tokenizer_module {
    iVersion: 0,
    xCreate: Some(simple_create),
    xDestroy: Some(builtin_destroy),
    xOpen: Some(builtin_open),
    xClose: Some(builtin_close),
    xNext: Some(builtin_next),
    xLanguageid: None,
};

static PORTER_MODULE: sqlite3_tokenizer_module = sqlite3_tokenizer_module {
    iVersion: 0,
    xCreate: Some(porter_create),
    xDestroy: Some(builtin_destroy),
    xOpen: Some(builtin_open),
    xClose: Some(builtin_close),
    xNext: Some(builtin_next),
    xLanguageid: None,
};

static UNICODE_MODULE: sqlite3_tokenizer_module = sqlite3_tokenizer_module {
    iVersion: 0,
    xCreate: Some(unicode_create),
    xDestroy: Some(builtin_destroy),
    xOpen: Some(builtin_open),
    xClose: Some(builtin_close),
    xNext: Some(builtin_next),
    xLanguageid: None,
};

/// The C module address of a tokenizer module.
pub(crate) fn module_ptr(m: TokenizerModule) -> ModulePtr {
    match m {
        TokenizerModule::Simple => ModulePtr(&SIMPLE_MODULE as *const _ as usize),
        TokenizerModule::Porter => ModulePtr(&PORTER_MODULE as *const _ as usize),
        TokenizerModule::Unicode61 => ModulePtr(&UNICODE_MODULE as *const _ as usize),
        TokenizerModule::C(p) => p,
    }
}

/// Maps a module address back to a built-in tokenizer when it is one.
pub(crate) fn module_from_ptr(p: usize) -> TokenizerModule {
    for m in [
        TokenizerModule::Simple,
        TokenizerModule::Porter,
        TokenizerModule::Unicode61,
    ] {
        if module_ptr(m).0 == p {
            return m;
        }
    }
    TokenizerModule::C(ModulePtr(p))
}
