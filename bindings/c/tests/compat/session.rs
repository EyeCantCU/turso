//! Session extension tests. Changesets are compared byte for byte with
//! SQLite 3.54's output, and changesets made by SQLite are applied here, so
//! these also check that changesets interoperate in both directions.

use libc::{c_char, c_int, c_void};
use std::ffi::{CStr, CString};
use std::ptr;

use super::{sqlite3, sqlite3_close, sqlite3_errmsg, sqlite3_exec, sqlite3_free, sqlite3_open};

const SQLITE_OK: c_int = 0;
const SQLITE_ABORT: c_int = 4;
const SQLITE_CORRUPT: c_int = 11;
const SQLITE_SCHEMA: c_int = 17;
const SQLITE_MISUSE: c_int = 21;
const SQLITE_RANGE: c_int = 25;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_INSERT: c_int = 18;
const SQLITE_UPDATE: c_int = 23;
const SQLITE_DELETE: c_int = 9;

const SQLITE_CHANGESET_DATA: c_int = 1;
const SQLITE_CHANGESET_NOTFOUND: c_int = 2;
const SQLITE_CHANGESET_CONFLICT: c_int = 3;
const SQLITE_CHANGESET_CONSTRAINT: c_int = 4;
const SQLITE_CHANGESET_OMIT: c_int = 0;
const SQLITE_CHANGESET_REPLACE: c_int = 1;
const SQLITE_CHANGESET_ABORT: c_int = 2;
const SQLITE_CHANGESETAPPLY_INVERT: c_int = 2;
const SQLITE_CHANGESETSTART_INVERT: c_int = 2;
const SQLITE_SESSION_OBJCONFIG_SIZE: c_int = 1;
const SQLITE_SESSION_OBJCONFIG_ROWID: c_int = 2;
const SQLITE_SESSION_CONFIG_STRMSIZE: c_int = 1;

#[repr(C)]
struct sqlite3_session {
    _p: [u8; 0],
}
#[repr(C)]
struct sqlite3_changeset_iter {
    _p: [u8; 0],
}

type XConflict = unsafe extern "C" fn(*mut c_void, c_int, *mut sqlite3_changeset_iter) -> c_int;
type XFilter = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
type XOutput = unsafe extern "C" fn(*mut c_void, *const c_void, c_int) -> c_int;
type XInput = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_int) -> c_int;

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
    fn sqlite3session_create(
        db: *mut sqlite3,
        z_db: *const c_char,
        pp: *mut *mut sqlite3_session,
    ) -> c_int;
    fn sqlite3session_delete(p: *mut sqlite3_session);
    fn sqlite3session_attach(p: *mut sqlite3_session, z_tab: *const c_char) -> c_int;
    fn sqlite3session_table_filter(p: *mut sqlite3_session, x: Option<XFilter>, ctx: *mut c_void);
    fn sqlite3session_changeset(
        p: *mut sqlite3_session,
        pn: *mut c_int,
        pp: *mut *mut c_void,
    ) -> c_int;
    fn sqlite3session_patchset(
        p: *mut sqlite3_session,
        pn: *mut c_int,
        pp: *mut *mut c_void,
    ) -> c_int;
    fn sqlite3session_changeset_strm(
        p: *mut sqlite3_session,
        x: Option<XOutput>,
        out: *mut c_void,
    ) -> c_int;
    fn sqlite3session_patchset_strm(
        p: *mut sqlite3_session,
        x: Option<XOutput>,
        out: *mut c_void,
    ) -> c_int;
    fn sqlite3session_enable(p: *mut sqlite3_session, b: c_int) -> c_int;
    fn sqlite3session_indirect(p: *mut sqlite3_session, b: c_int) -> c_int;
    fn sqlite3session_isempty(p: *mut sqlite3_session) -> c_int;
    fn sqlite3session_memory_used(p: *mut sqlite3_session) -> i64;
    fn sqlite3session_changeset_size(p: *mut sqlite3_session) -> i64;
    fn sqlite3session_object_config(p: *mut sqlite3_session, op: c_int, arg: *mut c_void) -> c_int;
    fn sqlite3session_config(op: c_int, arg: *mut c_void) -> c_int;
    fn sqlite3session_diff(
        p: *mut sqlite3_session,
        from: *const c_char,
        tbl: *const c_char,
        err: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3changeset_start(
        pp: *mut *mut sqlite3_changeset_iter,
        n: c_int,
        p: *mut c_void,
    ) -> c_int;
    fn sqlite3changeset_start_v2(
        pp: *mut *mut sqlite3_changeset_iter,
        n: c_int,
        p: *mut c_void,
        flags: c_int,
    ) -> c_int;
    fn sqlite3changeset_start_strm(
        pp: *mut *mut sqlite3_changeset_iter,
        x: Option<XInput>,
        p: *mut c_void,
    ) -> c_int;
    fn sqlite3changeset_next(p: *mut sqlite3_changeset_iter) -> c_int;
    fn sqlite3changeset_op(
        p: *mut sqlite3_changeset_iter,
        tab: *mut *const c_char,
        n_col: *mut c_int,
        op: *mut c_int,
        indirect: *mut c_int,
    ) -> c_int;
    fn sqlite3changeset_pk(
        p: *mut sqlite3_changeset_iter,
        pk: *mut *mut u8,
        n: *mut c_int,
    ) -> c_int;
    fn sqlite3changeset_old(p: *mut sqlite3_changeset_iter, i: c_int, v: *mut *mut c_void)
        -> c_int;
    fn sqlite3changeset_new(p: *mut sqlite3_changeset_iter, i: c_int, v: *mut *mut c_void)
        -> c_int;
    fn sqlite3changeset_conflict(
        p: *mut sqlite3_changeset_iter,
        i: c_int,
        v: *mut *mut c_void,
    ) -> c_int;
    fn sqlite3changeset_finalize(p: *mut sqlite3_changeset_iter) -> c_int;
    fn sqlite3changeset_apply(
        db: *mut sqlite3,
        n: c_int,
        p: *mut c_void,
        filter: Option<XFilter>,
        conflict: Option<XConflict>,
        ctx: *mut c_void,
    ) -> c_int;
    fn sqlite3changeset_apply_v2(
        db: *mut sqlite3,
        n: c_int,
        p: *mut c_void,
        filter: Option<XFilter>,
        conflict: Option<XConflict>,
        ctx: *mut c_void,
        rebase: *mut *mut c_void,
        n_rebase: *mut c_int,
        flags: c_int,
    ) -> c_int;
    fn sqlite3changeset_apply_strm(
        db: *mut sqlite3,
        x: Option<XInput>,
        p_in: *mut c_void,
        filter: Option<XFilter>,
        conflict: Option<XConflict>,
        ctx: *mut c_void,
    ) -> c_int;
    fn sqlite3_value_type(v: *mut c_void) -> c_int;
    fn sqlite3_value_int64(v: *mut c_void) -> i64;
    fn sqlite3_value_double(v: *mut c_void) -> f64;
    fn sqlite3_value_bytes(v: *mut c_void) -> c_int;
    fn sqlite3_value_blob(v: *mut c_void) -> *const c_void;
    fn sqlite3_value_text(v: *mut c_void) -> *const u8;
    fn sqlite3_prepare_v2(
        db: *mut sqlite3,
        sql: *const c_char,
        n: c_int,
        stmt: *mut *mut c_void,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_step(stmt: *mut c_void) -> c_int;
    fn sqlite3_finalize(stmt: *mut c_void) -> c_int;
    fn sqlite3_column_count(stmt: *mut c_void) -> c_int;
    fn sqlite3_column_type(stmt: *mut c_void, i: c_int) -> c_int;
    fn sqlite3_column_text(stmt: *mut c_void, i: c_int) -> *const c_char;
}

#[cfg(not(feature = "sqlite3"))]
extern "C" {
    fn turso_enable_experimental();
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

unsafe fn errmsg(db: *mut sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db))
        .to_string_lossy()
        .into_owned()
}

unsafe fn exec(db: *mut sqlite3, sql: &str) {
    let rc = sqlite3_exec(db, c(sql).as_ptr(), None, ptr::null_mut(), ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "{sql}: {}", errmsg(db));
}

/// Rows joined with '|'.
unsafe fn query(db: *mut sqlite3, sql: &str) -> Vec<String> {
    let mut stmt = ptr::null_mut();
    let rc = sqlite3_prepare_v2(db, c(sql).as_ptr(), -1, &mut stmt, ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "{sql}: {}", errmsg(db));
    let mut rows = Vec::new();
    while sqlite3_step(stmt) == SQLITE_ROW {
        let row: Vec<String> = (0..sqlite3_column_count(stmt))
            .map(|i| {
                if sqlite3_column_type(stmt, i) == 5 {
                    "NULL".to_string()
                } else {
                    CStr::from_ptr(sqlite3_column_text(stmt, i))
                        .to_string_lossy()
                        .into_owned()
                }
            })
            .collect();
        rows.push(row.join("|"));
    }
    sqlite3_finalize(stmt);
    rows
}

unsafe fn open(path: &std::path::Path) -> *mut sqlite3 {
    let mut db = ptr::null_mut();
    assert_eq!(
        sqlite3_open(c(path.to_str().unwrap()).as_ptr(), &mut db),
        SQLITE_OK
    );
    db
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// Takes ownership of an sqlite3_malloc'd buffer.
unsafe fn take(n: c_int, p: *mut c_void) -> Vec<u8> {
    let v = if p.is_null() {
        Vec::new()
    } else {
        std::slice::from_raw_parts(p as *const u8, n as usize).to_vec()
    };
    sqlite3_free(p);
    v
}

unsafe fn changeset(s: *mut sqlite3_session) -> Vec<u8> {
    let (mut n, mut p) = (0, ptr::null_mut());
    assert_eq!(sqlite3session_changeset(s, &mut n, &mut p), SQLITE_OK);
    take(n, p)
}

unsafe fn patchset(s: *mut sqlite3_session) -> Vec<u8> {
    let (mut n, mut p) = (0, ptr::null_mut());
    assert_eq!(sqlite3session_patchset(s, &mut n, &mut p), SQLITE_OK);
    take(n, p)
}

unsafe fn val(v: *mut c_void) -> String {
    if v.is_null() {
        return "-".to_string();
    }
    match sqlite3_value_type(v) {
        1 => sqlite3_value_int64(v).to_string(),
        2 => format!("{:?}", sqlite3_value_double(v)),
        3 | 4 => {
            let n = sqlite3_value_bytes(v) as usize;
            let p = if sqlite3_value_type(v) == 3 {
                sqlite3_value_text(v)
            } else {
                sqlite3_value_blob(v) as *const u8
            };
            let b = if n == 0 {
                &[][..]
            } else {
                std::slice::from_raw_parts(p, n)
            };
            let s = String::from_utf8_lossy(b).into_owned();
            if sqlite3_value_type(v) == 3 {
                format!("'{s}'")
            } else {
                format!("x'{}'", hex(b))
            }
        }
        _ => "NULL".to_string(),
    }
}

/// One line per change: "tab OP old=(..) new=(..)".
unsafe fn describe_iter(it: *mut sqlite3_changeset_iter) -> Vec<String> {
    let mut out = Vec::new();
    while sqlite3changeset_next(it) == SQLITE_ROW {
        let (mut tab, mut n, mut op, mut ind) = (ptr::null(), 0, 0, 0);
        sqlite3changeset_op(it, &mut tab, &mut n, &mut op, &mut ind);
        let (mut pk, mut npk) = (ptr::null_mut(), 0);
        sqlite3changeset_pk(it, &mut pk, &mut npk);
        let pk: String = (0..npk)
            .map(|i| (*pk.add(i as usize)).to_string())
            .collect();
        let mut s = format!(
            "{} {} pk={pk}{}",
            CStr::from_ptr(tab).to_string_lossy(),
            match op {
                SQLITE_INSERT => "INSERT",
                SQLITE_DELETE => "DELETE",
                _ => "UPDATE",
            },
            if ind != 0 { " indirect" } else { "" }
        );
        let cols = |f: unsafe extern "C" fn(
            *mut sqlite3_changeset_iter,
            c_int,
            *mut *mut c_void,
        ) -> c_int| {
            (0..n)
                .map(|i| {
                    let mut v = ptr::null_mut();
                    f(it, i, &mut v);
                    val(v)
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        if op != SQLITE_INSERT {
            s.push_str(&format!(" old=({})", cols(sqlite3changeset_old)));
        }
        if op != SQLITE_DELETE {
            s.push_str(&format!(" new=({})", cols(sqlite3changeset_new)));
        }
        out.push(s);
    }
    out
}

unsafe fn describe(cs: &[u8]) -> Vec<String> {
    let mut it = ptr::null_mut();
    assert_eq!(
        sqlite3changeset_start(&mut it, cs.len() as c_int, cs.as_ptr() as *mut c_void),
        SQLITE_OK
    );
    let out = describe_iter(it);
    assert_eq!(sqlite3changeset_finalize(it), SQLITE_OK);
    out
}

const SCHEMA: &str = "CREATE TABLE t1(a INTEGER PRIMARY KEY, b, c);\
    CREATE TABLE t2(x TEXT, y, z REAL, PRIMARY KEY(y, x));\
    CREATE TABLE nopk(a, b);\
    INSERT INTO t1 VALUES(1, 'one', 1.5), (2, 'two', NULL), (3, x'0102', 3);\
    INSERT INTO t2 VALUES('a', 1, 1.0), ('b', 2, 2.0);";

const CHANGES: &str = "INSERT INTO t1 VALUES(4, 'four', 4);\
    UPDATE t1 SET b = 'ONE' WHERE a = 1;\
    UPDATE t1 SET c = 22 WHERE a = 2;\
    DELETE FROM t1 WHERE a = 3;\
    INSERT INTO t1 VALUES(5, 'five', 5); DELETE FROM t1 WHERE a = 5;\
    UPDATE t1 SET b = 'x' WHERE a = 2; UPDATE t1 SET b = 'two' WHERE a = 2;\
    INSERT INTO t2 VALUES('c', 3, 3.5);\
    UPDATE t2 SET z = 9 WHERE x = 'a';\
    DELETE FROM t2 WHERE x = 'b';\
    INSERT INTO nopk VALUES(1, 2);\
    UPDATE t1 SET a = 10 WHERE a = 4;";

/// SQLite 3.54's changeset for SCHEMA + CHANGES.
const CHANGESET: &str = "5403010000743100120001000000000000000a0304666f7572010000000000000004170001000000000000000103036f6e65000003034f4e450017000100000000000000020005000001000000000000001609000100000000000000030402010201000000000000000354030201007432001700030161010000000000000001023ff000000000000000000240220000000000000900030162010000000000000002024000000000000000120003016301000000000000000302400c000000000000";
/// SQLite 3.54's patchset for SCHEMA + CHANGES.
const PATCHSET: &str = "5003010000743100120001000000000000000a0304666f7572010000000000000004170001000000000000000103034f4e45001700010000000000000002000100000000000000160900010000000000000003500302010074320017000301610100000000000000010240220000000000000900030162010000000000000002120003016301000000000000000302400c000000000000";

fn expected_changes() -> Vec<&'static str> {
    vec![
        "t1 INSERT pk=100 new=(10,'four',4)",
        "t1 UPDATE pk=100 old=(1,'one',-) new=(-,'ONE',-)",
        "t1 UPDATE pk=100 old=(2,-,NULL) new=(-,-,22)",
        "t1 DELETE pk=100 old=(3,x'0102',3)",
        "t2 UPDATE pk=210 old=('a',1,1.0) new=(-,-,9.0)",
        "t2 DELETE pk=210 old=('b',2,2.0)",
        "t2 INSERT pk=210 new=('c',3,3.5)",
    ]
}

#[test]
fn test_session_changeset_matches_sqlite() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir.path().join("a.db"));
        exec(db, SCHEMA);
        let mut s = ptr::null_mut();
        assert_eq!(
            sqlite3session_create(db, c("main").as_ptr(), &mut s),
            SQLITE_OK
        );
        let mut size = 1;
        assert_eq!(
            sqlite3session_object_config(
                s,
                SQLITE_SESSION_OBJCONFIG_SIZE,
                &mut size as *mut c_int as *mut c_void
            ),
            SQLITE_OK
        );
        assert_eq!(size, 1);
        assert_eq!(sqlite3session_attach(s, ptr::null()), SQLITE_OK);
        assert_eq!(sqlite3session_isempty(s), 1);
        exec(db, CHANGES);
        assert_eq!(sqlite3session_isempty(s), 0);
        assert!(sqlite3session_memory_used(s) > 0);
        let cs = changeset(s);
        assert_eq!(hex(&cs), CHANGESET);
        assert_eq!(sqlite3session_changeset_size(s), 261);
        assert_eq!(hex(&patchset(s)), PATCHSET);
        assert_eq!(describe(&cs), expected_changes());
        // Configuration is fixed once a table is attached.
        assert_eq!(
            sqlite3session_object_config(
                s,
                SQLITE_SESSION_OBJCONFIG_SIZE,
                &mut size as *mut c_int as *mut c_void
            ),
            SQLITE_MISUSE
        );
        sqlite3session_delete(s);
        sqlite3_close(db);
    }
}

unsafe extern "C" fn skip_t2(_ctx: *mut c_void, tab: *const c_char) -> c_int {
    (CStr::from_ptr(tab).to_bytes() != b"t2") as c_int
}

#[test]
fn test_session_filter_indirect_enable_rowid() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir.path().join("a.db"));
        exec(db, SCHEMA);
        exec(
            db,
            "CREATE TABLE log(id INTEGER PRIMARY KEY, m);\
             CREATE TRIGGER tr AFTER INSERT ON t1 BEGIN INSERT INTO log(m) VALUES(new.b); END;",
        );
        let mut s = ptr::null_mut();
        sqlite3session_create(db, c("main").as_ptr(), &mut s);
        sqlite3session_table_filter(s, Some(skip_t2), ptr::null_mut());
        exec(
            db,
            "INSERT INTO t1 VALUES(7, 'seven', 7); INSERT INTO t2 VALUES('z', 9, 0);",
        );
        assert_eq!(sqlite3session_indirect(s, 1), 1);
        assert_eq!(sqlite3session_indirect(s, -1), 1);
        exec(db, "UPDATE t1 SET b = 'ind' WHERE a = 1");
        sqlite3session_indirect(s, 0);
        assert_eq!(sqlite3session_enable(s, 0), 0);
        exec(db, "UPDATE t1 SET b = 'off' WHERE a = 2");
        assert_eq!(sqlite3session_enable(s, -1), 0);
        sqlite3session_enable(s, 1);
        assert_eq!(
            describe(&changeset(s)),
            [
                "t1 UPDATE pk=100 indirect old=(1,'one',-) new=(-,'ind',-)",
                "t1 INSERT pk=100 new=(7,'seven',7)",
                "log INSERT pk=10 indirect new=(1,'seven')",
            ]
        );
        sqlite3session_delete(s);

        // Tables without a PRIMARY KEY are tracked by rowid when configured.
        let mut s = ptr::null_mut();
        sqlite3session_create(db, c("main").as_ptr(), &mut s);
        let mut on = 1;
        sqlite3session_object_config(
            s,
            SQLITE_SESSION_OBJCONFIG_ROWID,
            &mut on as *mut c_int as *mut c_void,
        );
        sqlite3session_attach(s, c("nopk").as_ptr());
        exec(db, "INSERT INTO nopk VALUES(3, 4); INSERT INTO nopk VALUES(5, 6); UPDATE nopk SET b = 7 WHERE a = 3;");
        let cs = changeset(s);
        assert_eq!(
            hex(&cs),
            "54030100006e6f706b0012000100000000000000010100000000000000030100000000000000071200010000000000000002010000000000000005010000000000000006"
        );
        assert_eq!(
            describe(&cs),
            [
                "nopk INSERT pk=100 new=(1,3,7)",
                "nopk INSERT pk=100 new=(2,5,6)",
            ]
        );
        sqlite3session_delete(s);
        sqlite3_close(db);
    }
}

struct Buf(Vec<u8>, usize, usize);

unsafe extern "C" fn x_output(ctx: *mut c_void, data: *const c_void, n: c_int) -> c_int {
    let b = &mut *(ctx as *mut Buf);
    b.0.extend_from_slice(std::slice::from_raw_parts(data as *const u8, n as usize));
    b.1 += 1;
    SQLITE_OK
}

unsafe extern "C" fn x_input(ctx: *mut c_void, data: *mut c_void, pn: *mut c_int) -> c_int {
    let b = &mut *(ctx as *mut Buf);
    let n = (b.0.len() - b.1).min(*pn as usize).min(b.2);
    ptr::copy_nonoverlapping(b.0.as_ptr().add(b.1), data as *mut u8, n);
    b.1 += n;
    *pn = n as c_int;
    SQLITE_OK
}

#[test]
fn test_session_streams() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir.path().join("a.db"));
        exec(db, SCHEMA);
        let mut s = ptr::null_mut();
        sqlite3session_create(db, c("main").as_ptr(), &mut s);
        sqlite3session_attach(s, ptr::null());
        exec(db, CHANGES);
        let mut chunk = 64;
        assert_eq!(
            sqlite3session_config(
                SQLITE_SESSION_CONFIG_STRMSIZE,
                &mut chunk as *mut c_int as *mut c_void
            ),
            SQLITE_OK
        );
        let mut out = Buf(Vec::new(), 0, 0);
        assert_eq!(
            sqlite3session_changeset_strm(s, Some(x_output), &mut out as *mut Buf as *mut c_void),
            SQLITE_OK
        );
        assert_eq!(hex(&out.0), CHANGESET);
        assert!(out.1 > 1, "output came in chunks");
        let mut pout = Buf(Vec::new(), 0, 0);
        sqlite3session_patchset_strm(s, Some(x_output), &mut pout as *mut Buf as *mut c_void);
        assert_eq!(hex(&pout.0), PATCHSET);
        // Read it back 5 bytes at a time.
        let mut input = Buf(out.0.clone(), 0, 5);
        let mut it = ptr::null_mut();
        sqlite3changeset_start_strm(
            &mut it,
            Some(x_input),
            &mut input as *mut Buf as *mut c_void,
        );
        assert_eq!(describe_iter(it), expected_changes());
        sqlite3changeset_finalize(it);
        let mut chunk = 1024;
        sqlite3session_config(
            SQLITE_SESSION_CONFIG_STRMSIZE,
            &mut chunk as *mut c_int as *mut c_void,
        );
        sqlite3session_delete(s);
        sqlite3_close(db);
    }
}

#[test]
fn test_session_iterator_api() {
    unsafe {
        let cs = unhex(CHANGESET);
        let mut it = ptr::null_mut();
        sqlite3changeset_start_v2(
            &mut it,
            cs.len() as c_int,
            cs.as_ptr() as *mut c_void,
            SQLITE_CHANGESETSTART_INVERT,
        );
        assert_eq!(sqlite3changeset_next(it), SQLITE_ROW);
        let (mut tab, mut n, mut op, mut ind) = (ptr::null(), 0, 0, 0);
        sqlite3changeset_op(it, &mut tab, &mut n, &mut op, &mut ind);
        assert_eq!((op, n), (SQLITE_DELETE, 3));
        let mut v = ptr::null_mut();
        assert_eq!(sqlite3changeset_new(it, 0, &mut v), SQLITE_MISUSE);
        assert_eq!(sqlite3changeset_old(it, 3, &mut v), SQLITE_RANGE);
        assert_eq!(sqlite3changeset_old(it, 0, &mut v), SQLITE_OK);
        assert_eq!(sqlite3_value_int64(v), 10);
        assert_eq!(sqlite3changeset_conflict(it, 0, &mut v), SQLITE_MISUSE);
        sqlite3changeset_finalize(it);

        // A truncated changeset is corrupt.
        let mut it = ptr::null_mut();
        sqlite3changeset_start(&mut it, 20, cs.as_ptr() as *mut c_void);
        while sqlite3changeset_next(it) == SQLITE_ROW {}
        assert_eq!(sqlite3changeset_finalize(it), SQLITE_CORRUPT);
        let mut it = ptr::null_mut();
        sqlite3changeset_start(&mut it, 0, ptr::null_mut());
        assert_eq!(sqlite3changeset_next(it), SQLITE_DONE);
        assert_eq!(sqlite3changeset_finalize(it), SQLITE_OK);
    }
}

#[derive(Default)]
struct Conflicts {
    log: Vec<String>,
    answer: c_int,
}

unsafe extern "C" fn x_conflict(
    ctx: *mut c_void,
    e: c_int,
    it: *mut sqlite3_changeset_iter,
) -> c_int {
    let st = &mut *(ctx as *mut Conflicts);
    let (mut tab, mut n, mut op, mut ind) = (ptr::null(), 0, 0, 0);
    sqlite3changeset_op(it, &mut tab, &mut n, &mut op, &mut ind);
    let kind = match e {
        SQLITE_CHANGESET_DATA => "DATA",
        SQLITE_CHANGESET_NOTFOUND => "NOTFOUND",
        SQLITE_CHANGESET_CONFLICT => "CONFLICT",
        SQLITE_CHANGESET_CONSTRAINT => "CONSTRAINT",
        _ => "FOREIGN_KEY",
    };
    let mut s = format!("{kind} {} {op}", CStr::from_ptr(tab).to_string_lossy());
    if e == SQLITE_CHANGESET_DATA || e == SQLITE_CHANGESET_CONFLICT {
        let row: Vec<String> = (0..n)
            .map(|i| {
                let mut v = ptr::null_mut();
                sqlite3changeset_conflict(it, i, &mut v);
                val(v)
            })
            .collect();
        s.push_str(&format!(" ({})", row.join(",")));
    }
    st.log.push(s);
    if st.answer == SQLITE_CHANGESET_REPLACE
        && (e == SQLITE_CHANGESET_NOTFOUND || e == SQLITE_CHANGESET_CONSTRAINT)
    {
        return SQLITE_CHANGESET_OMIT;
    }
    st.answer
}

/// Rows of t1 and t2.
unsafe fn contents(db: *mut sqlite3) -> Vec<String> {
    let mut v = query(db, "SELECT 't1', * FROM t1 ORDER BY a");
    v.extend(query(db, "SELECT 't2', * FROM t2 ORDER BY x"));
    v
}
