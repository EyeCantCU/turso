//! FTS3 tokenizer C interface (fts3_tokenizer.h): fts3_tokenizer(), the
//! built-in modules seen through it, and an application-defined tokenizer.
//! These must pass on turso and on SQLite.

use libc::{c_char, c_int, c_void};
use std::ffi::CStr;
use std::ptr;

use super::{
    sqlite3, sqlite3_bind_blob, sqlite3_bind_text, sqlite3_close, sqlite3_column_blob,
    sqlite3_column_bytes, sqlite3_column_text, sqlite3_column_type, sqlite3_errmsg, sqlite3_exec,
    sqlite3_finalize, sqlite3_open, sqlite3_prepare_v2, sqlite3_step, sqlite3_stmt,
};

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_DBCONFIG_ENABLE_FTS3_TOKENIZER: c_int = 1004;

#[cfg_attr(
    all(not(feature = "sqlite3"), not(target_os = "windows")),
    link(name = "turso_sqlite3")
)]
#[cfg_attr(
    all(
        feature = "sqlite3",
        not(all(target_os = "windows", target_env = "msvc"))
    ),
    link(name = "sqlite3")
)]
#[cfg_attr(
    all(feature = "sqlite3", target_os = "windows", target_env = "msvc"),
    link(name = "sqlite3", kind = "static")
)]
#[cfg_attr(
    all(not(feature = "sqlite3"), target_os = "windows"),
    link(name = "turso_sqlite3", kind = "raw-dylib")
)]
extern "C" {
    fn sqlite3_db_config(db: *mut sqlite3, op: c_int, ...) -> c_int;
}

#[repr(C)]
struct Tokenizer {
    module: *const TokenizerModule,
}

#[repr(C)]
struct TokenizerCursor {
    tokenizer: *mut Tokenizer,
}

#[repr(C)]
struct TokenizerModule {
    version: c_int,
    create: unsafe extern "C" fn(c_int, *const *const c_char, *mut *mut Tokenizer) -> c_int,
    destroy: unsafe extern "C" fn(*mut Tokenizer) -> c_int,
    open: unsafe extern "C" fn(
        *mut Tokenizer,
        *const c_char,
        c_int,
        *mut *mut TokenizerCursor,
    ) -> c_int,
    close: unsafe extern "C" fn(*mut TokenizerCursor) -> c_int,
    next: unsafe extern "C" fn(
        *mut TokenizerCursor,
        *mut *const c_char,
        *mut c_int,
        *mut c_int,
        *mut c_int,
        *mut c_int,
    ) -> c_int,
    languageid: Option<unsafe extern "C" fn(*mut TokenizerCursor, c_int) -> c_int>,
}

/// A tokenizer that splits on spaces and reverses each word.
#[repr(C)]
struct RevCursor {
    base: TokenizerCursor,
    input: Vec<u8>,
    off: usize,
    pos: c_int,
    token: Vec<u8>,
}

unsafe extern "C" fn rev_create(
    _argc: c_int,
    _argv: *const *const c_char,
    pp: *mut *mut Tokenizer,
) -> c_int {
    *pp = Box::into_raw(Box::new(Tokenizer {
        module: ptr::null(),
    }));
    SQLITE_OK
}

unsafe extern "C" fn rev_destroy(p: *mut Tokenizer) -> c_int {
    drop(Box::from_raw(p));
    SQLITE_OK
}

unsafe extern "C" fn rev_open(
    _p: *mut Tokenizer,
    input: *const c_char,
    n: c_int,
    pp: *mut *mut TokenizerCursor,
) -> c_int {
    let input = if n < 0 {
        CStr::from_ptr(input).to_bytes().to_vec()
    } else {
        std::slice::from_raw_parts(input as *const u8, n as usize).to_vec()
    };
    let c = Box::new(RevCursor {
        base: TokenizerCursor {
            tokenizer: ptr::null_mut(),
        },
        input,
        off: 0,
        pos: 0,
        token: Vec::new(),
    });
    *pp = Box::into_raw(c) as *mut TokenizerCursor;
    SQLITE_OK
}

unsafe extern "C" fn rev_close(c: *mut TokenizerCursor) -> c_int {
    drop(Box::from_raw(c as *mut RevCursor));
    SQLITE_OK
}

unsafe extern "C" fn rev_next(
    c: *mut TokenizerCursor,
    tok: *mut *const c_char,
    n: *mut c_int,
    start: *mut c_int,
    end: *mut c_int,
    pos: *mut c_int,
) -> c_int {
    let c = &mut *(c as *mut RevCursor);
    while c.off < c.input.len() && c.input[c.off] == b' ' {
        c.off += 1;
    }
    if c.off >= c.input.len() {
        return SQLITE_DONE;
    }
    let s = c.off;
    while c.off < c.input.len() && c.input[c.off] != b' ' {
        c.off += 1;
    }
    c.token = c.input[s..c.off].iter().rev().copied().collect();
    *tok = c.token.as_ptr() as *const c_char;
    *n = c.token.len() as c_int;
    *start = s as c_int;
    *end = c.off as c_int;
    *pos = c.pos;
    c.pos += 1;
    SQLITE_OK
}

static REV_MODULE: TokenizerModule = TokenizerModule {
    version: 0,
    create: rev_create,
    destroy: rev_destroy,
    open: rev_open,
    close: rev_close,
    next: rev_next,
    languageid: None,
};

unsafe fn exec(db: *mut sqlite3, sql: &CStr) -> c_int {
    sqlite3_exec(db, sql.as_ptr(), None, ptr::null_mut(), ptr::null_mut())
}

unsafe fn errmsg(db: *mut sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db))
        .to_string_lossy()
        .into_owned()
}

unsafe fn rows(db: *mut sqlite3, sql: &CStr) -> Vec<String> {
    let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
    assert_eq!(
        sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()),
        SQLITE_OK,
        "{}",
        errmsg(db)
    );
    let mut out = Vec::new();
    while sqlite3_step(stmt) == SQLITE_ROW {
        out.push(
            CStr::from_ptr(sqlite3_column_text(stmt, 0))
                .to_string_lossy()
                .into_owned(),
        );
    }
    sqlite3_finalize(stmt);
    out
}

/// `SELECT fts3_tokenizer(?)` with a bound name: the module address.
unsafe fn query_tokenizer(db: *mut sqlite3, name: &CStr) -> *const TokenizerModule {
    let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
    sqlite3_prepare_v2(
        db,
        c"SELECT fts3_tokenizer(?)".as_ptr(),
        -1,
        &mut stmt,
        ptr::null_mut(),
    );
    sqlite3_bind_text(stmt, 1, name.as_ptr(), -1, None);
    assert_eq!(sqlite3_step(stmt), SQLITE_ROW, "{}", errmsg(db));
    assert_eq!(sqlite3_column_type(stmt, 0), 4);
    assert_eq!(
        sqlite3_column_bytes(stmt, 0) as i32 as usize,
        size_of::<usize>()
    );
    let mut p: *const TokenizerModule = ptr::null();
    ptr::copy_nonoverlapping(
        sqlite3_column_blob(stmt, 0) as *const u8,
        &mut p as *mut _ as *mut u8,
        size_of::<usize>(),
    );
    sqlite3_finalize(stmt);
    p
}

/// `SELECT fts3_tokenizer(?, ?)` with the module address bound as a blob.
unsafe fn register_tokenizer(db: *mut sqlite3, name: &CStr, m: *const TokenizerModule) -> c_int {
    let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
    sqlite3_prepare_v2(
        db,
        c"SELECT fts3_tokenizer(?, ?)".as_ptr(),
        -1,
        &mut stmt,
        ptr::null_mut(),
    );
    sqlite3_bind_text(stmt, 1, name.as_ptr(), -1, None);
    sqlite3_bind_blob(
        stmt,
        2,
        &m as *const _ as *const c_void,
        size_of::<usize>() as c_int,
        None,
    );
    let rc = sqlite3_step(stmt);
    sqlite3_finalize(stmt);
    rc
}

/// Tokenizes `text` with a module through its C interface.
unsafe fn tokenize(m: *const TokenizerModule, text: &str) -> Vec<(String, c_int, c_int, c_int)> {
    let m = &*m;
    let mut t: *mut Tokenizer = ptr::null_mut();
    assert_eq!((m.create)(0, ptr::null(), &mut t), SQLITE_OK);
    (*t).module = m;
    let mut c: *mut TokenizerCursor = ptr::null_mut();
    assert_eq!(
        (m.open)(
            t,
            text.as_ptr() as *const c_char,
            text.len() as c_int,
            &mut c
        ),
        SQLITE_OK
    );
    (*c).tokenizer = t;
    let mut out = Vec::new();
    loop {
        let mut tok: *const c_char = ptr::null();
        let (mut n, mut s, mut e, mut p) = (0, 0, 0, 0);
        if (m.next)(c, &mut tok, &mut n, &mut s, &mut e, &mut p) != SQLITE_OK {
            break;
        }
        let bytes = std::slice::from_raw_parts(tok as *const u8, n as usize);
        out.push((String::from_utf8_lossy(bytes).into_owned(), s, e, p));
    }
    (m.close)(c);
    (m.destroy)(t);
    out
}

#[test]
fn test_fts3_builtin_tokenizers_through_c_interface() {
    unsafe {
        let mut db: *mut sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(c":memory:".as_ptr(), &mut db), SQLITE_OK);
        let simple = query_tokenizer(db, c"simple");
        assert_eq!(
            tokenize(simple, "Hello, World"),
            vec![
                ("hello".to_string(), 0, 5, 0),
                ("world".to_string(), 7, 12, 1)
            ]
        );
        let porter = query_tokenizer(db, c"porter");
        assert_eq!(
            tokenize(porter, "running connections")
                .into_iter()
                .map(|t| t.0)
                .collect::<Vec<_>>(),
            vec!["run", "connect"]
        );
        let unicode = query_tokenizer(db, c"unicode61");
        assert_eq!(
            tokenize(unicode, "Caf\u{e9} \u{dc}BER")
                .into_iter()
                .map(|t| t.0)
                .collect::<Vec<_>>(),
            vec!["cafe", "uber"]
        );
        // The same module address every time.
        assert_eq!(query_tokenizer(db, c"simple"), simple);
        // An unknown name is an error.
        assert_ne!(
            exec(db, c"SELECT fts3_tokenizer('no_such_tokenizer')"),
            SQLITE_OK
        );
        assert_eq!(errmsg(db), "unknown tokenizer: no_such_tokenizer");
        sqlite3_close(db);
    }
}

#[test]
fn test_fts3_application_tokenizer() {
    unsafe {
        let mut db: *mut sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(c":memory:".as_ptr(), &mut db), SQLITE_OK);
        // Registering through bound parameters works without the db config.
        assert_eq!(register_tokenizer(db, c"rev", &REV_MODULE), SQLITE_ROW);
        assert_eq!(query_tokenizer(db, c"rev"), &REV_MODULE as *const _);
        assert_eq!(
            exec(
                db,
                c"CREATE VIRTUAL TABLE t USING fts4(x, tokenize=rev);
                  INSERT INTO t VALUES('abc def');
                  INSERT INTO t VALUES('xyz cba');"
            ),
            SQLITE_OK,
            "{}",
            errmsg(db)
        );
        // Queries are tokenized the same way: 'abc' looks up "cba".
        assert_eq!(
            rows(db, c"SELECT rowid FROM t WHERE t MATCH 'abc'"),
            vec!["1"]
        );
        assert_eq!(
            rows(db, c"SELECT offsets(t) FROM t WHERE t MATCH 'cba'"),
            vec!["0 0 4 3"]
        );
        assert_eq!(
            rows(db, c"SELECT snippet(t) FROM t WHERE t MATCH 'def'"),
            vec!["abc <b>def</b>"]
        );
        assert_eq!(
            exec(db, c"INSERT INTO t(t) VALUES('integrity-check')"),
            SQLITE_OK
        );
        // The term index holds the reversed words.
        assert_eq!(
            exec(db, c"CREATE VIRTUAL TABLE a USING fts4aux(t)"),
            SQLITE_OK
        );
        assert_eq!(
            rows(db, c"SELECT term FROM a WHERE col = '*'"),
            vec!["abc", "cba", "fed", "zyx"]
        );
        // fts3tokenize reaches it too.
        assert_eq!(
            exec(db, c"CREATE VIRTUAL TABLE tk USING fts3tokenize(rev)"),
            SQLITE_OK
        );
        assert_eq!(
            rows(db, c"SELECT token FROM tk WHERE input = 'hello there'"),
            vec!["olleh", "ereht"]
        );
        sqlite3_close(db);
    }
}

#[test]
fn test_fts3_tokenizer_needs_db_config_for_literals() {
    unsafe {
        let mut db: *mut sqlite3 = ptr::null_mut();
        assert_eq!(sqlite3_open(c":memory:".as_ptr(), &mut db), SQLITE_OK);
        let mut on: c_int = -1;
        assert_eq!(
            sqlite3_db_config(db, SQLITE_DBCONFIG_ENABLE_FTS3_TOKENIZER, -1, &mut on),
            SQLITE_OK
        );
        assert_eq!(on, 0);
        // A literal pointer is refused while disabled.
        assert_eq!(
            exec(db, c"SELECT fts3_tokenizer('x', x'0102030405060708')"),
            SQLITE_ERROR
        );
        assert_eq!(errmsg(db), "fts3tokenize disabled");
        // Looking a tokenizer up with a literal name returns NULL.
        assert_eq!(
            rows(db, c"SELECT quote(fts3_tokenizer('simple'))"),
            vec!["NULL"]
        );
        assert_eq!(
            sqlite3_db_config(db, SQLITE_DBCONFIG_ENABLE_FTS3_TOKENIZER, 1, &mut on),
            SQLITE_OK
        );
        assert_eq!(on, 1);
        assert_eq!(
            rows(db, c"SELECT length(fts3_tokenizer('simple'))"),
            vec![size_of::<usize>().to_string()]
        );
        // A pointer of the wrong size is a type mismatch.
        assert_eq!(exec(db, c"SELECT fts3_tokenizer('x', x'01')"), SQLITE_ERROR);
        assert_eq!(errmsg(db), "argument type mismatch");
        sqlite3_close(db);
    }
}
