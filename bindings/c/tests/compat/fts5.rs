//! FTS5 C API tests: a tokenizer and an auxiliary function registered
//! through fts5_api. They must pass on turso and on SQLite.

use libc::{c_char, c_int, c_void};
use std::ffi::{CStr, CString};
use std::ptr;

use super::{
    sqlite3, sqlite3_close, sqlite3_column_text, sqlite3_errmsg, sqlite3_exec, sqlite3_finalize,
    sqlite3_open, sqlite3_prepare_v2, sqlite3_step, sqlite3_stmt,
};

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;

type XToken = unsafe extern "C" fn(*mut c_void, c_int, *const c_char, c_int, c_int, c_int) -> c_int;

#[repr(C)]
struct fts5_tokenizer_v2 {
    i_version: c_int,
    x_create: Option<
        unsafe extern "C" fn(*mut c_void, *mut *const c_char, c_int, *mut *mut c_void) -> c_int,
    >,
    x_delete: Option<unsafe extern "C" fn(*mut c_void)>,
    x_tokenize: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            c_int,
            *const c_char,
            c_int,
            *const c_char,
            c_int,
            XToken,
        ) -> c_int,
    >,
}

type XAuxFunc = unsafe extern "C" fn(
    *const Fts5ExtensionApi,
    *mut c_void,
    *mut c_void,
    c_int,
    *mut *mut c_void,
);

#[repr(C)]
struct fts5_api {
    i_version: c_int,
    x_create_tokenizer: *const c_void,
    x_find_tokenizer: *const c_void,
    x_create_function: unsafe extern "C" fn(
        *mut fts5_api,
        *const c_char,
        *mut c_void,
        Option<XAuxFunc>,
        Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int,
    x_create_tokenizer_v2: unsafe extern "C" fn(
        *mut fts5_api,
        *const c_char,
        *mut c_void,
        *mut fts5_tokenizer_v2,
        Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int,
    x_find_tokenizer_v2: unsafe extern "C" fn(
        *mut fts5_api,
        *const c_char,
        *mut *mut c_void,
        *mut *mut fts5_tokenizer_v2,
    ) -> c_int,
}

#[repr(C)]
struct Fts5PhraseIter {
    a: *const u8,
    b: *const u8,
}

type Ctx = *mut c_void;

#[repr(C)]
struct Fts5ExtensionApi {
    i_version: c_int,
    x_user_data: unsafe extern "C" fn(Ctx) -> *mut c_void,
    x_column_count: unsafe extern "C" fn(Ctx) -> c_int,
    x_row_count: unsafe extern "C" fn(Ctx, *mut i64) -> c_int,
    x_column_total_size: unsafe extern "C" fn(Ctx, c_int, *mut i64) -> c_int,
    x_tokenize:
        unsafe extern "C" fn(Ctx, *const c_char, c_int, *mut c_void, Option<XToken>) -> c_int,
    x_phrase_count: unsafe extern "C" fn(Ctx) -> c_int,
    x_phrase_size: unsafe extern "C" fn(Ctx, c_int) -> c_int,
    x_inst_count: unsafe extern "C" fn(Ctx, *mut c_int) -> c_int,
    x_inst: unsafe extern "C" fn(Ctx, c_int, *mut c_int, *mut c_int, *mut c_int) -> c_int,
    x_rowid: unsafe extern "C" fn(Ctx) -> i64,
    x_column_text: unsafe extern "C" fn(Ctx, c_int, *mut *const c_char, *mut c_int) -> c_int,
    x_column_size: unsafe extern "C" fn(Ctx, c_int, *mut c_int) -> c_int,
    x_query_phrase: unsafe extern "C" fn(
        Ctx,
        c_int,
        *mut c_void,
        Option<unsafe extern "C" fn(*const Fts5ExtensionApi, Ctx, *mut c_void) -> c_int>,
    ) -> c_int,
    x_set_auxdata:
        unsafe extern "C" fn(Ctx, *mut c_void, Option<unsafe extern "C" fn(*mut c_void)>) -> c_int,
    x_get_auxdata: unsafe extern "C" fn(Ctx, c_int) -> *mut c_void,
    x_phrase_first:
        unsafe extern "C" fn(Ctx, c_int, *mut Fts5PhraseIter, *mut c_int, *mut c_int) -> c_int,
    x_phrase_next: unsafe extern "C" fn(Ctx, *mut Fts5PhraseIter, *mut c_int, *mut c_int),
    x_phrase_first_column:
        unsafe extern "C" fn(Ctx, c_int, *mut Fts5PhraseIter, *mut c_int) -> c_int,
    x_phrase_next_column: unsafe extern "C" fn(Ctx, *mut Fts5PhraseIter, *mut c_int),
    x_query_token: unsafe extern "C" fn(Ctx, c_int, c_int, *mut *const c_char, *mut c_int) -> c_int,
    x_inst_token: *const c_void,
    x_column_locale: *const c_void,
    x_tokenize_v2: *const c_void,
}

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
    fn sqlite3_bind_pointer(
        stmt: *mut sqlite3_stmt,
        idx: c_int,
        p: *mut c_void,
        ty: *const c_char,
        d: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    fn sqlite3_result_text(ctx: *mut c_void, s: *const c_char, n: c_int, d: isize);
    fn sqlite3_result_error(ctx: *mut c_void, s: *const c_char, n: c_int);
}

const SQLITE_TRANSIENT: isize = -1;

unsafe fn fts5_api_of(db: *mut sqlite3) -> *mut fts5_api {
    let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
    let sql = CString::new("SELECT fts5(?1)").unwrap();
    assert_eq!(
        sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut()),
        SQLITE_OK
    );
    let mut api: *mut fts5_api = ptr::null_mut();
    let ty = CString::new("fts5_api_ptr").unwrap();
    assert_eq!(
        sqlite3_bind_pointer(
            stmt,
            1,
            &mut api as *mut *mut fts5_api as *mut c_void,
            ty.as_ptr(),
            None
        ),
        SQLITE_OK
    );
    assert_eq!(sqlite3_step(stmt), SQLITE_ROW);
    sqlite3_finalize(stmt);
    api
}

// A tokenizer that reverses the tokens of the unicode61 tokenizer and adds
// the original token as a colocated synonym.
struct RevTok {
    base: *mut fts5_tokenizer_v2,
    inner: *mut c_void,
}

static mut BASE_USER: *mut c_void = ptr::null_mut();
static mut BASE_TOK: *mut fts5_tokenizer_v2 = ptr::null_mut();

unsafe extern "C" fn rev_create(
    _user: *mut c_void,
    _az: *mut *const c_char,
    _n: c_int,
    out: *mut *mut c_void,
) -> c_int {
    let base = BASE_TOK;
    let mut inner: *mut c_void = ptr::null_mut();
    let rc = ((*base).x_create.unwrap())(BASE_USER, ptr::null_mut(), 0, &mut inner);
    if rc != SQLITE_OK {
        return rc;
    }
    *out = Box::into_raw(Box::new(RevTok { base, inner })) as *mut c_void;
    SQLITE_OK
}

unsafe extern "C" fn rev_delete(p: *mut c_void) {
    let t = Box::from_raw(p as *mut RevTok);
    ((*t.base).x_delete.unwrap())(t.inner);
}

struct RevCtx {
    ctx: *mut c_void,
    x_token: XToken,
}

unsafe extern "C" fn rev_cb(
    p: *mut c_void,
    tflags: c_int,
    tok: *const c_char,
    n: c_int,
    s: c_int,
    e: c_int,
) -> c_int {
    let c = &*(p as *const RevCtx);
    let bytes = std::slice::from_raw_parts(tok as *const u8, n as usize);
    let rev: Vec<u8> = bytes.iter().rev().copied().collect();
    let rc = (c.x_token)(c.ctx, tflags, rev.as_ptr().cast(), n, s, e);
    if rc != SQLITE_OK || rev == bytes {
        return rc;
    }
    (c.x_token)(c.ctx, 1 /* FTS5_TOKEN_COLOCATED */, tok, n, s, e)
}

unsafe extern "C" fn rev_tokenize(
    p: *mut c_void,
    ctx: *mut c_void,
    flags: c_int,
    text: *const c_char,
    n: c_int,
    loc: *const c_char,
    nloc: c_int,
    x_token: XToken,
) -> c_int {
    let t = &*(p as *const RevTok);
    let mut rc = RevCtx { ctx, x_token };
    ((*t.base).x_tokenize.unwrap())(
        t.inner,
        &mut rc as *mut RevCtx as *mut c_void,
        flags,
        text,
        n,
        loc,
        nloc,
        rev_cb,
    )
}

static mut REV_TOKENIZER: fts5_tokenizer_v2 = fts5_tokenizer_v2 {
    i_version: 2,
    x_create: Some(rev_create),
    x_delete: Some(rev_delete),
    x_tokenize: Some(rev_tokenize),
};

static USER_VALUE: c_int = 42;

unsafe extern "C" fn count_cb(
    _api: *const Fts5ExtensionApi,
    _ctx: Ctx,
    user: *mut c_void,
) -> c_int {
    *(user as *mut i64) += 1;
    SQLITE_OK
}

unsafe extern "C" fn free_box(p: *mut c_void) {
    drop(Box::from_raw(p as *mut i64));
}

// Reports what the extension API says about the current row.
unsafe extern "C" fn rowinfo(
    api: *const Fts5ExtensionApi,
    ctx: Ctx,
    sctx: *mut c_void,
    nval: c_int,
    _vals: *mut *mut c_void,
) {
    let a = &*api;
    let mut out = String::new();
    let user = (a.x_user_data)(ctx) as *const c_int;
    out += &format!(
        "user={} nval={} cols={} ",
        *user,
        nval,
        (a.x_column_count)(ctx)
    );
    let mut nrow = 0i64;
    (a.x_row_count)(ctx, &mut nrow);
    let mut ntok = 0i64;
    (a.x_column_total_size)(ctx, -1, &mut ntok);
    out += &format!("rows={nrow} tokens={ntok} rowid={} ", (a.x_rowid)(ctx));
    let nphrase = (a.x_phrase_count)(ctx);
    for i in 0..nphrase {
        let mut z: *const c_char = ptr::null();
        let mut n = 0;
        (a.x_query_token)(ctx, i, 0, &mut z, &mut n);
        let tok = String::from_utf8_lossy(std::slice::from_raw_parts(z as *const u8, n as usize))
            .into_owned();
        let mut hits = 0i64;
        (a.x_query_phrase)(ctx, i, &mut hits as *mut i64 as *mut c_void, Some(count_cb));
        out += &format!("p{i}[{} {tok} docs={hits}] ", (a.x_phrase_size)(ctx, i));
        let mut it = Fts5PhraseIter {
            a: ptr::null(),
            b: ptr::null(),
        };
        let (mut col, mut off) = (0, 0);
        (a.x_phrase_first)(ctx, i, &mut it, &mut col, &mut off);
        while col >= 0 {
            out += &format!("{col}.{off} ");
            (a.x_phrase_next)(ctx, &mut it, &mut col, &mut off);
        }
        let mut col = 0;
        (a.x_phrase_first_column)(ctx, i, &mut it, &mut col);
        while col >= 0 {
            out += &format!("c{col} ");
            (a.x_phrase_next_column)(ctx, &mut it, &mut col);
        }
    }
    let mut ninst = 0;
    (a.x_inst_count)(ctx, &mut ninst);
    for i in 0..ninst {
        let (mut ip, mut ic, mut io) = (0, 0, 0);
        (a.x_inst)(ctx, i, &mut ip, &mut ic, &mut io);
        out += &format!("i{ip}:{ic}:{io} ");
    }
    let mut z: *const c_char = ptr::null();
    let mut n = 0;
    (a.x_column_text)(ctx, 0, &mut z, &mut n);
    let mut sz = 0;
    (a.x_column_size)(ctx, 0, &mut sz);
    out += &format!(
        "text={:?} size={sz} ",
        String::from_utf8_lossy(std::slice::from_raw_parts(z as *const u8, n as usize))
    );
    // Auxdata persists across the rows of one query.
    let calls = (a.x_get_auxdata)(ctx, 0) as *mut i64;
    let calls = if calls.is_null() {
        let b = Box::into_raw(Box::new(0i64));
        (a.x_set_auxdata)(ctx, b as *mut c_void, Some(free_box));
        b
    } else {
        calls
    };
    *calls += 1;
    out += &format!("call={}", *calls);
    let c = CString::new(out).unwrap();
    sqlite3_result_text(sctx, c.as_ptr(), -1, SQLITE_TRANSIENT);
}

unsafe extern "C" fn failing(
    _api: *const Fts5ExtensionApi,
    _ctx: Ctx,
    sctx: *mut c_void,
    _n: c_int,
    _v: *mut *mut c_void,
) {
    let msg = CString::new("aux failed").unwrap();
    sqlite3_result_error(sctx, msg.as_ptr(), -1);
}

unsafe fn rows(db: *mut sqlite3, sql: &str) -> Result<Vec<String>, String> {
    let c = CString::new(sql).unwrap();
    let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
    if sqlite3_prepare_v2(db, c.as_ptr(), -1, &mut stmt, ptr::null_mut()) != SQLITE_OK {
        return Err(CStr::from_ptr(sqlite3_errmsg(db))
            .to_string_lossy()
            .into_owned());
    }
    let mut out = Vec::new();
    loop {
        match sqlite3_step(stmt) {
            SQLITE_ROW => {
                let t = sqlite3_column_text(stmt, 0);
                out.push(if t.is_null() {
                    "NULL".to_string()
                } else {
                    CStr::from_ptr(t as *const c_char)
                        .to_string_lossy()
                        .into_owned()
                });
            }
            SQLITE_DONE => break,
            _ => {
                let e = CStr::from_ptr(sqlite3_errmsg(db))
                    .to_string_lossy()
                    .into_owned();
                sqlite3_finalize(stmt);
                return Err(e);
            }
        }
    }
    sqlite3_finalize(stmt);
    Ok(out)
}

unsafe fn exec(db: *mut sqlite3, sql: &str) {
    let c = CString::new(sql).unwrap();
    let rc = sqlite3_exec(db, c.as_ptr(), None, ptr::null_mut(), ptr::null_mut());
    assert_eq!(
        rc,
        SQLITE_OK,
        "{sql}: {}",
        CStr::from_ptr(sqlite3_errmsg(db)).to_string_lossy()
    );
}

#[test]
fn test_fts5_api_tokenizer_and_auxiliary_function() {
    unsafe {
        let mut db: *mut sqlite3 = ptr::null_mut();
        let name = CString::new(":memory:").unwrap();
        assert_eq!(sqlite3_open(name.as_ptr(), &mut db), SQLITE_OK);
        let api = fts5_api_of(db);
        assert!(!api.is_null());
        assert!((*api).i_version >= 3);

        // A missing tokenizer is SQLITE_ERROR; unicode61 is found.
        let mut user: *mut c_void = ptr::null_mut();
        let mut tok: *mut fts5_tokenizer_v2 = ptr::null_mut();
        let nosuch = CString::new("nosuch").unwrap();
        assert_ne!(
            ((*api).x_find_tokenizer_v2)(api, nosuch.as_ptr(), &mut user, &mut tok),
            SQLITE_OK
        );
        let u61 = CString::new("unicode61").unwrap();
        assert_eq!(
            ((*api).x_find_tokenizer_v2)(api, u61.as_ptr(), &mut user, &mut tok),
            SQLITE_OK
        );
        BASE_USER = user;
        BASE_TOK = tok;

        let rev = CString::new("rev").unwrap();
        let tokenizer = &raw mut REV_TOKENIZER;
        assert_eq!(
            ((*api).x_create_tokenizer_v2)(api, rev.as_ptr(), ptr::null_mut(), tokenizer, None),
            SQLITE_OK
        );
        let f = CString::new("rowinfo").unwrap();
        assert_eq!(
            ((*api).x_create_function)(
                api,
                f.as_ptr(),
                &USER_VALUE as *const c_int as *mut c_void,
                Some(rowinfo),
                None
            ),
            SQLITE_OK
        );
        let f2 = CString::new("failing").unwrap();
        assert_eq!(
            ((*api).x_create_function)(api, f2.as_ptr(), ptr::null_mut(), Some(failing), None),
            SQLITE_OK
        );

        exec(
            db,
            "CREATE VIRTUAL TABLE t USING fts5(a, b, tokenize='rev')",
        );
        exec(db, "INSERT INTO t VALUES('Hello world', 'the world is round'), ('round robin', 'hello again')");
        exec(db, "CREATE VIRTUAL TABLE v USING fts5vocab(t, row)");
        assert_eq!(
            rows(db, "SELECT term FROM v").unwrap(),
            [
                "again", "dlrow", "dnuor", "eht", "hello", "is", "niaga", "nibor", "olleh",
                "robin", "round", "si", "the", "world"
            ]
        );
        assert_eq!(
            rows(db, "SELECT rowid FROM t WHERE t MATCH 'world'").unwrap(),
            ["1"]
        );
        assert_eq!(
            rows(db, "SELECT rowid FROM t WHERE t MATCH 'dlrow'").unwrap(),
            ["1"]
        );
        assert_eq!(
            rows(db, "SELECT rowinfo(t, 1, 2) FROM t WHERE t MATCH 'hello OR round' ORDER BY rowid").unwrap(),
            [
                "user=42 nval=2 cols=2 rows=2 tokens=10 rowid=1 p0[1 olleh docs=2] 0.0 c0 p1[1 dnuor docs=2] 1.3 c1 i0:0:0 i1:1:3 text=\"Hello world\" size=2 call=1",
                "user=42 nval=2 cols=2 rows=2 tokens=10 rowid=2 p0[1 olleh docs=2] 1.0 c1 p1[1 dnuor docs=2] 0.0 c0 i1:0:0 i0:1:0 text=\"round robin\" size=2 call=2",
            ]
        );
        assert_eq!(
            rows(db, "SELECT failing(t) FROM t WHERE t MATCH 'hello'").unwrap_err(),
            "aux failed"
        );
        // An fts5_api function used as the rank function.
        exec(db, "INSERT INTO t(t, rank) VALUES('rank', 'rowinfo()')");
        assert_eq!(
            rows(db, "SELECT rank FROM t WHERE t MATCH 'robin'")
                .unwrap()
                .len(),
            1
        );
        sqlite3_close(db);
    }
}
