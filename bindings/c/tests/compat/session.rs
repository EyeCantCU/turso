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
    fn sqlite3changeset_invert(
        n: c_int,
        p: *const c_void,
        pn: *mut c_int,
        pp: *mut *mut c_void,
    ) -> c_int;
    fn sqlite3changeset_invert_strm(
        xi: Option<XInput>,
        p_in: *mut c_void,
        xo: Option<XOutput>,
        p_out: *mut c_void,
    ) -> c_int;
    fn sqlite3changeset_concat(
        na: c_int,
        pa: *mut c_void,
        nb: c_int,
        pb: *mut c_void,
        pn: *mut c_int,
        pp: *mut *mut c_void,
    ) -> c_int;
    fn sqlite3changeset_concat_strm(
        xa: Option<XInput>,
        pa: *mut c_void,
        xb: Option<XInput>,
        pb: *mut c_void,
        xo: Option<XOutput>,
        p_out: *mut c_void,
    ) -> c_int;
    fn sqlite3changegroup_new(pp: *mut *mut c_void) -> c_int;
    fn sqlite3changegroup_schema(g: *mut c_void, db: *mut sqlite3, z_db: *const c_char) -> c_int;
    fn sqlite3changegroup_add(g: *mut c_void, n: c_int, p: *mut c_void) -> c_int;
    fn sqlite3changegroup_add_strm(g: *mut c_void, x: Option<XInput>, p: *mut c_void) -> c_int;
    fn sqlite3changegroup_add_change(g: *mut c_void, it: *mut sqlite3_changeset_iter) -> c_int;
    fn sqlite3changegroup_output(g: *mut c_void, pn: *mut c_int, pp: *mut *mut c_void) -> c_int;
    fn sqlite3changegroup_output_strm(g: *mut c_void, x: Option<XOutput>, p: *mut c_void) -> c_int;
    fn sqlite3changegroup_config(g: *mut c_void, op: c_int, arg: *mut c_void) -> c_int;
    fn sqlite3changegroup_delete(g: *mut c_void);
    fn sqlite3changegroup_change_begin(
        g: *mut c_void,
        op: c_int,
        tab: *const c_char,
        indirect: c_int,
        err: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3changegroup_change_int64(g: *mut c_void, new: c_int, col: c_int, v: i64) -> c_int;
    fn sqlite3changegroup_change_null(g: *mut c_void, new: c_int, col: c_int) -> c_int;
    fn sqlite3changegroup_change_double(g: *mut c_void, new: c_int, col: c_int, v: f64) -> c_int;
    fn sqlite3changegroup_change_text(
        g: *mut c_void,
        new: c_int,
        col: c_int,
        v: *const c_char,
        n: c_int,
    ) -> c_int;
    fn sqlite3changegroup_change_blob(
        g: *mut c_void,
        new: c_int,
        col: c_int,
        v: *const c_void,
        n: c_int,
    ) -> c_int;
    fn sqlite3changegroup_change_finish(
        g: *mut c_void,
        discard: c_int,
        err: *mut *mut c_char,
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

#[test]
fn test_session_apply_sqlite_changeset() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let src = open(&dir.path().join("src.db"));
        exec(src, SCHEMA);
        exec(src, CHANGES);
        let dst = open(&dir.path().join("dst.db"));
        exec(dst, SCHEMA);
        let mut st = Conflicts::default();
        let ctx = &mut st as *mut Conflicts as *mut c_void;
        // CHANGESET was made by SQLite.
        let mut cs = unhex(CHANGESET);
        let rc = sqlite3changeset_apply(
            dst,
            cs.len() as c_int,
            cs.as_mut_ptr() as *mut c_void,
            None,
            Some(x_conflict),
            ctx,
        );
        assert_eq!(rc, SQLITE_OK);
        assert!(st.log.is_empty(), "{:?}", st.log);
        assert_eq!(contents(dst), contents(src));
        // Applying it again conflicts on every change.
        let rc = sqlite3changeset_apply(
            dst,
            cs.len() as c_int,
            cs.as_mut_ptr() as *mut c_void,
            None,
            Some(x_conflict),
            ctx,
        );
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(
            st.log,
            [
                "CONFLICT t1 18 (10,'four',4)",
                "DATA t1 23 (1,'ONE',1.5)",
                "DATA t1 23 (2,'two',22)",
                "NOTFOUND t1 9",
                "DATA t2 23 ('a',1,9.0)",
                "NOTFOUND t2 9",
                "CONFLICT t2 18 ('c',3,3.5)",
            ]
        );
        // Inverting it restores the original rows.
        let rc = sqlite3changeset_apply_v2(
            dst,
            cs.len() as c_int,
            cs.as_mut_ptr() as *mut c_void,
            None,
            Some(x_conflict),
            ctx,
            ptr::null_mut(),
            ptr::null_mut(),
            SQLITE_CHANGESETAPPLY_INVERT,
        );
        assert_eq!(rc, SQLITE_OK);
        let orig = open(&dir.path().join("orig.db"));
        exec(orig, SCHEMA);
        assert_eq!(contents(dst), contents(orig));
        // The patchset applies as well, here through a stream.
        let mut input = Buf(unhex(PATCHSET), 0, 7);
        let rc = sqlite3changeset_apply_strm(
            dst,
            Some(x_input),
            &mut input as *mut Buf as *mut c_void,
            None,
            Some(x_conflict),
            ctx,
        );
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(contents(dst), contents(src));
        for db in [src, dst, orig] {
            sqlite3_close(db);
        }
    }
}

#[test]
fn test_session_apply_conflict_resolution() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let dst = open(&dir.path().join("dst.db"));
        exec(dst, SCHEMA);
        exec(
            dst,
            "DELETE FROM t1; DELETE FROM t2;\
             INSERT INTO t1 VALUES(1,'a',10),(2,'b2',20),(3,'c3',30),(10,'x',99);\
             INSERT INTO t2 VALUES('a',1,8.0),('b',2,2.0);",
        );
        let mut cs = unhex(CHANGESET);
        let mut st = Conflicts {
            answer: SQLITE_CHANGESET_REPLACE,
            ..Default::default()
        };
        let ctx = &mut st as *mut Conflicts as *mut c_void;
        let (mut nrb, mut rb) = (0, ptr::null_mut());
        let rc = sqlite3changeset_apply_v2(
            dst,
            cs.len() as c_int,
            cs.as_mut_ptr() as *mut c_void,
            None,
            Some(x_conflict),
            ctx,
            &mut rb,
            &mut nrb,
            0,
        );
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(
            st.log,
            [
                "CONFLICT t1 18 (10,'x',99)",
                "DATA t1 23 (1,'a',10)",
                "DATA t1 23 (2,'b2',20)",
                "DATA t1 9 (3,'c3',30)",
                "DATA t2 23 ('a',1,8.0)",
            ]
        );
        assert_eq!(
            contents(dst),
            [
                "t1|1|ONE|10",
                "t1|2|b2|22",
                "t1|10|four|4",
                "t2|a|1|9.0",
                "t2|c|3|3.5",
            ]
        );
        assert_eq!(
            hex(&take(nrb, rb)),
            "5403010000743100120101000000000000000a0304666f7572010000000000000004120101000000000000000103034f4e450012010100000000000000020001000000000000001609010100000000000000030402010201000000000000000354030201007432001201030161010000000000000001024022000000000000"
        );

        // ABORT rolls the whole apply back.
        exec(dst, "DELETE FROM t1; INSERT INTO t1 VALUES(2,'zz',20);");
        st.log.clear();
        st.answer = SQLITE_CHANGESET_ABORT;
        let rc = sqlite3changeset_apply(
            dst,
            cs.len() as c_int,
            cs.as_mut_ptr() as *mut c_void,
            None,
            Some(x_conflict),
            ctx,
        );
        assert_eq!(rc, SQLITE_ABORT);
        assert_eq!(query(dst, "SELECT * FROM t1"), ["2|zz|20"]);

        // A table the filter rejects, or whose schema differs, is skipped.
        exec(
            dst,
            "DROP TABLE t2; CREATE TABLE t2(x TEXT PRIMARY KEY, y);",
        );
        st.log.clear();
        st.answer = SQLITE_CHANGESET_OMIT;
        let rc = sqlite3changeset_apply(
            dst,
            cs.len() as c_int,
            cs.as_mut_ptr() as *mut c_void,
            Some(skip_t2),
            Some(x_conflict),
            ctx,
        );
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(query(dst, "SELECT count(*) FROM t2"), ["0"]);
        let rc = sqlite3changeset_apply(
            dst,
            cs.len() as c_int,
            cs.as_mut_ptr() as *mut c_void,
            None,
            Some(x_conflict),
            ctx,
        );
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(query(dst, "SELECT count(*) FROM t2"), ["0"]);
        let _ = SQLITE_SCHEMA;
        sqlite3_close(dst);
    }
}

/// Changes made after SCHEMA + CHANGES.
const CHANGES2: &str = "UPDATE t1 SET b = 'TEN' WHERE a = 10;\
    UPDATE t1 SET b = 'one' WHERE a = 1;\
    INSERT INTO t1 VALUES(3, 'three', 3);\
    DELETE FROM t1 WHERE a = 2;\
    UPDATE t2 SET z = 10 WHERE x = 'a';\
    DELETE FROM t2 WHERE x = 'c';\
    INSERT INTO t1 VALUES(30, 'x', 0); DELETE FROM t1 WHERE a = 30;";

/// The changeset for CHANGES2, as made by this engine.
unsafe fn second_changeset(dir: &std::path::Path) -> Vec<u8> {
    let db = open(&dir.join("second.db"));
    exec(db, SCHEMA);
    exec(db, CHANGES);
    let mut s = ptr::null_mut();
    sqlite3session_create(db, c("main").as_ptr(), &mut s);
    sqlite3session_attach(s, ptr::null());
    exec(db, CHANGES2);
    let cs = changeset(s);
    sqlite3session_delete(s);
    sqlite3_close(db);
    cs
}

const CHANGESET2: &str = "5403010000743100170001000000000000000a0304666f75720000030354454e00170001000000000000000103034f4e45000003036f6e65000900010000000000000002030374776f010000000000000016120001000000000000000303057468726565010000000000000003540302010074320017000301610100000000000000010240220000000000000000024024000000000000090003016301000000000000000302400c000000000000";
const INVERTED: &str = "5403010000743100090001000000000000000a0304666f7572010000000000000004170001000000000000000103034f4e45000003036f6e6500170001000000000000000200010000000000000016000005120001000000000000000304020102010000000000000003540302010074320017000301610100000000000000010240220000000000000000023ff00000000000001200030162010000000000000002024000000000000000090003016301000000000000000302400c000000000000";
const CONCAT: &str = "5403010000743100120001000000000000000a030354454e0100000000000000040900010000000000000002030374776f051700010000000000000003040201020000030574687265650054030201007432001700030161010000000000000001023ff000000000000000000240240000000000000900030162010000000000000002024000000000000000";

unsafe fn out_buf(n: c_int, p: *mut c_void, rc: c_int) -> Vec<u8> {
    assert_eq!(rc, SQLITE_OK);
    take(n, p)
}

#[test]
fn test_session_invert_concat() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let mut cs1 = unhex(CHANGESET);
        let mut cs2 = second_changeset(dir.path());
        assert_eq!(hex(&cs2), CHANGESET2);
        let (mut n, mut p) = (0, ptr::null_mut());
        let rc = sqlite3changeset_invert(
            cs1.len() as c_int,
            cs1.as_ptr() as *const c_void,
            &mut n,
            &mut p,
        );
        let inv = out_buf(n, p, rc);
        assert_eq!(hex(&inv), INVERTED);
        let mut input = Buf(cs1.clone(), 0, 9);
        let mut out = Buf(Vec::new(), 0, 0);
        let rc = sqlite3changeset_invert_strm(
            Some(x_input),
            &mut input as *mut Buf as *mut c_void,
            Some(x_output),
            &mut out as *mut Buf as *mut c_void,
        );
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(out.0, inv);
        // A patchset cannot be inverted.
        let ps = unhex(PATCHSET);
        let rc = sqlite3changeset_invert(
            ps.len() as c_int,
            ps.as_ptr() as *const c_void,
            &mut n,
            &mut p,
        );
        assert_eq!(rc, SQLITE_CORRUPT);

        let rc = sqlite3changeset_concat(
            cs1.len() as c_int,
            cs1.as_mut_ptr() as *mut c_void,
            cs2.len() as c_int,
            cs2.as_mut_ptr() as *mut c_void,
            &mut n,
            &mut p,
        );
        let cat = out_buf(n, p, rc);
        assert_eq!(hex(&cat), CONCAT);
        let (mut a, mut b) = (Buf(cs1.clone(), 0, 4), Buf(cs2.clone(), 0, 6));
        let mut out = Buf(Vec::new(), 0, 0);
        let rc = sqlite3changeset_concat_strm(
            Some(x_input),
            &mut a as *mut Buf as *mut c_void,
            Some(x_input),
            &mut b as *mut Buf as *mut c_void,
            Some(x_output),
            &mut out as *mut Buf as *mut c_void,
        );
        assert_eq!(rc, SQLITE_OK);
        assert_eq!(out.0, cat);
        // A changeset and a patchset do not mix.
        let mut ps = unhex(PATCHSET);
        let rc = sqlite3changeset_concat(
            cs1.len() as c_int,
            cs1.as_mut_ptr() as *mut c_void,
            ps.len() as c_int,
            ps.as_mut_ptr() as *mut c_void,
            &mut n,
            &mut p,
        );
        assert_eq!(rc, 1);

        // Applying the concatenation equals applying both changesets.
        let dst = open(&dir.path().join("dst.db"));
        exec(dst, SCHEMA);
        let mut st = Conflicts::default();
        let ctx = &mut st as *mut Conflicts as *mut c_void;
        let mut cat = cat;
        let rc = sqlite3changeset_apply(
            dst,
            cat.len() as c_int,
            cat.as_mut_ptr() as *mut c_void,
            None,
            Some(x_conflict),
            ctx,
        );
        assert_eq!(rc, SQLITE_OK);
        assert!(st.log.is_empty(), "{:?}", st.log);
        let src = open(&dir.path().join("second.db"));
        assert_eq!(contents(dst), contents(src));
        sqlite3_close(src);
        sqlite3_close(dst);
    }
}

const GROUP_ADD_CHANGE: &str = "5403010000743100170001000000000000000103036f6e65000003034f4e450017000100000000000000020005000001000000000000001654030201007432001700030161010000000000000001023ff00000000000000000024022000000000000";
const GROUP_SCHEMA: &str = "54030100007700120001000000000000000103036f6e650301431200010000000000000002030374776f030464666c7412000100000000000000030305746872656503026333";
const GROUP_CHANGES: &str = "54030100007700120001000000000000000103036f6e65030143120001000000000000000204020102030464666c7412000100000000000000030305746872656503026333120101000000000000003203056669667479024004000000000000";

#[test]
fn test_session_changegroup() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let mut cs1 = unhex(CHANGESET);
        let mut cs2 = second_changeset(dir.path());
        let mut g = ptr::null_mut();
        assert_eq!(sqlite3changegroup_new(&mut g), SQLITE_OK);
        assert_eq!(
            sqlite3changegroup_add(g, cs1.len() as c_int, cs1.as_mut_ptr() as *mut c_void),
            SQLITE_OK
        );
        let mut input = Buf(cs2.clone(), 0, 3);
        assert_eq!(
            sqlite3changegroup_add_strm(g, Some(x_input), &mut input as *mut Buf as *mut c_void),
            SQLITE_OK
        );
        let (mut n, mut p) = (0, ptr::null_mut());
        let rc = sqlite3changegroup_output(g, &mut n, &mut p);
        assert_eq!(hex(&out_buf(n, p, rc)), CONCAT);
        let mut out = Buf(Vec::new(), 0, 0);
        sqlite3changegroup_output_strm(g, Some(x_output), &mut out as *mut Buf as *mut c_void);
        assert_eq!(hex(&out.0), CONCAT);
        let mut patch = 1;
        sqlite3changegroup_config(g, 1, &mut patch as *mut c_int as *mut c_void);
        assert_eq!(patch, 0, "fixed once changes are added");
        let mut ps = unhex(PATCHSET);
        assert_eq!(
            sqlite3changegroup_add(g, ps.len() as c_int, ps.as_mut_ptr() as *mut c_void),
            1
        );
        sqlite3changegroup_delete(g);

        // add_change: only the UPDATEs of the first changeset.
        sqlite3changegroup_new(&mut g);
        let mut it = ptr::null_mut();
        sqlite3changeset_start(&mut it, cs1.len() as c_int, cs1.as_mut_ptr() as *mut c_void);
        assert_eq!(sqlite3changegroup_add_change(g, it), 1, "no current change");
        while sqlite3changeset_next(it) == SQLITE_ROW {
            let (mut tab, mut nc, mut op, mut ind) = (ptr::null(), 0, 0, 0);
            sqlite3changeset_op(it, &mut tab, &mut nc, &mut op, &mut ind);
            if op == SQLITE_UPDATE {
                assert_eq!(sqlite3changegroup_add_change(g, it), SQLITE_OK);
            }
        }
        sqlite3changeset_finalize(it);
        let rc = sqlite3changegroup_output(g, &mut n, &mut p);
        assert_eq!(hex(&out_buf(n, p, rc)), GROUP_ADD_CHANGE);
        sqlite3changegroup_delete(g);
        let _ = &mut cs2;

        // With a schema, changes recorded before ALTER TABLE ADD COLUMN are
        // padded with the new columns' defaults.
        let db = open(&dir.path().join("w.db"));
        exec(db, "CREATE TABLE w(a INTEGER PRIMARY KEY, b)");
        let mut s = ptr::null_mut();
        sqlite3session_create(db, c("main").as_ptr(), &mut s);
        sqlite3session_attach(s, ptr::null());
        exec(
            db,
            "INSERT INTO w VALUES(1, 'one'); INSERT INTO w VALUES(2, 'two');",
        );
        let mut old = changeset(s);
        sqlite3session_delete(s);
        exec(db, "ALTER TABLE w ADD COLUMN c DEFAULT 'dflt'");
        sqlite3session_create(db, c("main").as_ptr(), &mut s);
        sqlite3session_attach(s, ptr::null());
        exec(
            db,
            "UPDATE w SET c = 'C' WHERE a = 1; INSERT INTO w VALUES(3, 'three', 'c3');",
        );
        let mut new = changeset(s);
        sqlite3session_delete(s);
        sqlite3changegroup_new(&mut g);
        assert_eq!(
            sqlite3changegroup_add(g, old.len() as c_int, old.as_mut_ptr() as *mut c_void),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3changegroup_add(g, new.len() as c_int, new.as_mut_ptr() as *mut c_void),
            SQLITE_SCHEMA
        );
        sqlite3changegroup_delete(g);
        sqlite3changegroup_new(&mut g);
        assert_eq!(
            sqlite3changegroup_schema(g, db, c("main").as_ptr()),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3changegroup_schema(g, db, c("main").as_ptr()),
            SQLITE_MISUSE
        );
        assert_eq!(
            sqlite3changegroup_add(g, old.len() as c_int, old.as_mut_ptr() as *mut c_void),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3changegroup_add(g, new.len() as c_int, new.as_mut_ptr() as *mut c_void),
            SQLITE_OK
        );
        let rc = sqlite3changegroup_output(g, &mut n, &mut p);
        assert_eq!(hex(&out_buf(n, p, rc)), GROUP_SCHEMA);

        // Changes built value by value.
        let mut err = ptr::null_mut();
        assert_eq!(sqlite3changegroup_change_int64(g, 1, 0, 5), SQLITE_MISUSE);
        assert_eq!(
            sqlite3changegroup_change_begin(g, 99, c("w").as_ptr(), 0, &mut err),
            1
        );
        assert_eq!(
            sqlite3changegroup_change_begin(g, SQLITE_INSERT, c("nosuch").as_ptr(), 0, &mut err),
            1
        );
        assert_eq!(
            CStr::from_ptr(err).to_str().unwrap(),
            "no such table: nosuch"
        );
        sqlite3_free(err as *mut c_void);
        assert_eq!(
            sqlite3changegroup_change_begin(g, SQLITE_INSERT, c("w").as_ptr(), 1, &mut err),
            SQLITE_OK
        );
        assert_eq!(
            sqlite3changegroup_change_begin(g, SQLITE_INSERT, c("w").as_ptr(), 0, &mut err),
            SQLITE_MISUSE
        );
        assert_eq!(sqlite3changegroup_change_int64(g, 1, 3, 1), SQLITE_RANGE);
        assert_eq!(sqlite3changegroup_change_int64(g, 0, 0, 1), 1);
        sqlite3changegroup_change_int64(g, 1, 0, 50);
        sqlite3changegroup_change_text(g, 1, 1, c("fifty").as_ptr(), 3);
        assert_eq!(sqlite3changegroup_change_finish(g, 0, &mut err), 1);
        assert_eq!(
            CStr::from_ptr(err).to_str().unwrap(),
            "invalid change: column 2 is undefined"
        );
        sqlite3_free(err as *mut c_void);
        sqlite3changegroup_change_begin(g, SQLITE_INSERT, c("w").as_ptr(), 1, ptr::null_mut());
        sqlite3changegroup_change_int64(g, 1, 0, 50);
        sqlite3changegroup_change_text(g, 1, 1, c("fifty").as_ptr(), -1);
        sqlite3changegroup_change_double(g, 1, 2, 2.5);
        assert_eq!(sqlite3changegroup_change_finish(g, 0, &mut err), SQLITE_OK);
        assert!(err.is_null());
        sqlite3changegroup_change_begin(g, SQLITE_UPDATE, c("w").as_ptr(), 0, ptr::null_mut());
        sqlite3changegroup_change_int64(g, 0, 0, 2);
        sqlite3changegroup_change_text(g, 0, 1, c("two").as_ptr(), -1);
        sqlite3changegroup_change_blob(g, 1, 1, b"\x01\x02".as_ptr() as *const c_void, 2);
        assert_eq!(
            sqlite3changegroup_change_finish(g, 0, ptr::null_mut()),
            SQLITE_OK
        );
        sqlite3changegroup_change_begin(g, SQLITE_UPDATE, c("w").as_ptr(), 0, ptr::null_mut());
        sqlite3changegroup_change_null(g, 0, 0);
        assert_eq!(sqlite3changegroup_change_finish(g, 0, &mut err), 1);
        assert_eq!(
            CStr::from_ptr(err).to_str().unwrap(),
            "invalid change: null value in PK of old.* record"
        );
        sqlite3_free(err as *mut c_void);
        sqlite3changegroup_change_begin(g, SQLITE_DELETE, c("w").as_ptr(), 0, ptr::null_mut());
        sqlite3changegroup_change_int64(g, 0, 0, 1);
        assert_eq!(
            sqlite3changegroup_change_finish(g, 1, ptr::null_mut()),
            SQLITE_OK
        );
        let rc = sqlite3changegroup_output(g, &mut n, &mut p);
        assert_eq!(hex(&out_buf(n, p, rc)), GROUP_CHANGES);
        sqlite3changegroup_delete(g);
        sqlite3_close(db);
    }
}
