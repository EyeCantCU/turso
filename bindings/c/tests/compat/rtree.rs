//! R*Tree tests through the C API, including the MATCH geometry callbacks.
//! They must pass on turso and on SQLite.

use libc::{c_char, c_int, c_uint, c_void};
use std::ffi::{CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};

use super::{
    sqlite3, sqlite3_close, sqlite3_column_count, sqlite3_column_text, sqlite3_column_type,
    sqlite3_errcode, sqlite3_errmsg, sqlite3_exec, sqlite3_finalize, sqlite3_last_insert_rowid,
    sqlite3_open, sqlite3_prepare_v2, sqlite3_step, sqlite3_value_int64, sqlite3_value_type,
};

const SQLITE_OK: c_int = 0;
const SQLITE_ERROR: c_int = 1;
const SQLITE_CONSTRAINT: c_int = 19;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_NULL: c_int = 5;
const SQLITE_INTEGER: c_int = 1;
const NOT_WITHIN: c_int = 0;
const PARTLY_WITHIN: c_int = 1;
const FULLY_WITHIN: c_int = 2;

#[repr(C)]
#[allow(non_snake_case)]
struct sqlite3_rtree_query_info {
    pContext: *mut c_void,
    nParam: c_int,
    aParam: *mut f64,
    pUser: *mut c_void,
    xDelUser: Option<unsafe extern "C" fn(*mut c_void)>,
    aCoord: *mut f64,
    anQueue: *mut c_uint,
    nCoord: c_int,
    iLevel: c_int,
    mxLevel: c_int,
    iRowid: i64,
    rParentScore: f64,
    eParentWithin: c_int,
    eWithin: c_int,
    rScore: f64,
    apSqlParam: *mut *mut c_void,
}

type GeomFn =
    unsafe extern "C" fn(*mut sqlite3_rtree_query_info, c_int, *mut f64, *mut c_int) -> c_int;
type QueryFn = unsafe extern "C" fn(*mut sqlite3_rtree_query_info) -> c_int;

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
    fn sqlite3_rtree_geometry_callback(
        db: *mut sqlite3,
        name: *const c_char,
        x_geom: Option<GeomFn>,
        context: *mut c_void,
    ) -> c_int;
    fn sqlite3_rtree_query_callback(
        db: *mut sqlite3,
        name: *const c_char,
        x_query: Option<QueryFn>,
        context: *mut c_void,
        destructor: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

unsafe fn open_mem() -> *mut sqlite3 {
    let mut db = ptr::null_mut();
    assert_eq!(sqlite3_open(c(":memory:").as_ptr(), &mut db), SQLITE_OK);
    db
}

unsafe fn exec(db: *mut sqlite3, sql: &str) -> c_int {
    sqlite3_exec(db, c(sql).as_ptr(), None, ptr::null_mut(), ptr::null_mut())
}

unsafe fn errmsg(db: *mut sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db))
        .to_string_lossy()
        .into_owned()
}

unsafe fn exec_ok(db: *mut sqlite3, sql: &str) {
    let rc = exec(db, sql);
    assert_eq!(rc, SQLITE_OK, "{sql}: {}", errmsg(db));
}

/// Rows of `sql`, each column as text ("NULL" for NULL).
unsafe fn query(db: *mut sqlite3, sql: &str) -> Vec<Vec<String>> {
    let mut stmt = ptr::null_mut();
    let rc = sqlite3_prepare_v2(db, c(sql).as_ptr(), -1, &mut stmt, ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "{sql}: {}", errmsg(db));
    let mut rows = Vec::new();
    loop {
        match sqlite3_step(stmt) {
            SQLITE_ROW => rows.push(
                (0..sqlite3_column_count(stmt))
                    .map(|i| {
                        if sqlite3_column_type(stmt, i) == SQLITE_NULL {
                            "NULL".to_string()
                        } else {
                            CStr::from_ptr(sqlite3_column_text(stmt, i))
                                .to_string_lossy()
                                .into_owned()
                        }
                    })
                    .collect(),
            ),
            SQLITE_DONE => break,
            rc => panic!("{sql}: step rc={rc}: {}", errmsg(db)),
        }
    }
    sqlite3_finalize(stmt);
    rows
}

unsafe fn ints(db: *mut sqlite3, sql: &str) -> Vec<i64> {
    query(db, sql)
        .into_iter()
        .map(|r| r[0].parse().unwrap())
        .collect()
}

/// A 10x10 grid of unit boxes, ids 1..=100, box (x, y) at [x, x+1] x [y, y+1].
unsafe fn grid(db: *mut sqlite3) {
    exec_ok(
        db,
        "CREATE VIRTUAL TABLE rt USING rtree(id, x0, x1, y0, y1)",
    );
    exec_ok(db, "BEGIN");
    for x in 0..10 {
        for y in 0..10 {
            let id = x * 10 + y + 1;
            exec_ok(
                db,
                &format!(
                    "INSERT INTO rt VALUES({id}, {x}, {}, {y}, {})",
                    x + 1,
                    y + 1
                ),
            );
        }
    }
    exec_ok(db, "COMMIT");
}

#[test]
fn test_rtree_basic_c_api() {
    unsafe {
        let db = open_mem();
        grid(db);
        assert_eq!(
            ints(db, "SELECT count(*) FROM rt"),
            vec![100],
            "every row inserted"
        );
        assert_eq!(
            ints(
                db,
                "SELECT id FROM rt WHERE x0 >= 8 AND y1 <= 2 ORDER BY id"
            ),
            vec![81, 82, 91, 92]
        );
        assert_eq!(
            query(db, "SELECT x0, x1, y0, y1 FROM rt WHERE id = 55"),
            vec![vec!["5.0", "6.0", "4.0", "5.0"]]
        );
        assert_eq!(query(db, "SELECT rtreecheck('rt')"), vec![vec!["ok"]]);
        assert_eq!(
            ints(db, "SELECT rtreedepth(data) FROM rt_node WHERE nodeno = 1"),
            vec![1]
        );

        // A NULL id picks a new rowid, which is last_insert_rowid.
        exec_ok(db, "INSERT INTO rt VALUES(NULL, 0, 1, 0, 1)");
        assert_eq!(sqlite3_last_insert_rowid(db), 101);

        // x0 > x1 fails the rtree constraint.
        assert_eq!(
            exec(db, "INSERT INTO rt VALUES(200, 5, 4, 0, 1)"),
            SQLITE_CONSTRAINT
        );
        assert_eq!(sqlite3_errcode(db), SQLITE_CONSTRAINT);
        assert_eq!(errmsg(db), "rtree constraint failed: rt.(x0<=x1)");
        // A duplicate id fails, unless OR REPLACE.
        assert_eq!(
            exec(db, "INSERT INTO rt VALUES(5, 0, 1, 0, 1)"),
            SQLITE_CONSTRAINT
        );
        assert_eq!(errmsg(db), "UNIQUE constraint failed: rt.id");
        exec_ok(db, "INSERT OR REPLACE INTO rt VALUES(5, 50, 51, 50, 51)");
        assert_eq!(ints(db, "SELECT id FROM rt WHERE x0 > 40"), vec![5]);

        exec_ok(db, "DELETE FROM rt WHERE x1 <= 5");
        assert_eq!(ints(db, "SELECT count(*) FROM rt"), vec![51]);
        exec_ok(db, "UPDATE rt SET x0 = x0 + 100, x1 = x1 + 100");
        assert_eq!(
            ints(db, "SELECT count(*) FROM rt WHERE x0 >= 105"),
            vec![51]
        );
        assert_eq!(query(db, "SELECT rtreecheck('rt')"), vec![vec!["ok"]]);

        exec_ok(db, "DROP TABLE rt");
        assert_eq!(
            ints(
                db,
                "SELECT count(*) FROM sqlite_schema WHERE name LIKE 'rt%'"
            ),
            vec![0]
        );
        sqlite3_close(db);
    }
}

#[test]
fn test_rtreecheck_reports_corruption() {
    unsafe {
        let db = open_mem();
        exec_ok(db, "CREATE VIRTUAL TABLE rt USING rtree(id, x0, x1)");
        exec_ok(db, "INSERT INTO rt VALUES(1, 0, 1), (2, 2, 3)");
        // Shadow tables are writable outside defensive mode.
        exec_ok(db, "DELETE FROM rt_rowid WHERE rowid = 2");
        exec_ok(db, "INSERT INTO rt_parent VALUES(99, 1)");
        assert_eq!(
            query(db, "SELECT rtreecheck('rt')"),
            vec![vec![
                "Mapping (2 -> 1) missing from %_rowid table\n\
                 Wrong number of entries in %_rowid table - expected 2, actual 1\n\
                 Wrong number of entries in %_parent table - expected 0, actual 1"
            ]]
        );
        sqlite3_close(db);
    }
}

static GEOM_USER_FREED: AtomicI32 = AtomicI32::new(0);

unsafe extern "C" fn free_user(p: *mut c_void) {
    drop(Box::from_raw(p as *mut i64));
    GEOM_USER_FREED.fetch_add(1, Ordering::SeqCst);
}

/// circle(x, y, r): boxes that overlap the circle.
unsafe extern "C" fn circle_geom(
    p: *mut sqlite3_rtree_query_info,
    n_coord: c_int,
    a: *mut f64,
    res: *mut c_int,
) -> c_int {
    let p = &mut *p;
    if p.nParam != 3 || n_coord != 4 {
        return SQLITE_ERROR;
    }
    assert_eq!(*(p.pContext as *const i32), 42);
    if p.pUser.is_null() {
        p.pUser = Box::into_raw(Box::new(0i64)) as *mut c_void;
        p.xDelUser = Some(free_user);
    }
    *(p.pUser as *mut i64) += 1;
    let prm = std::slice::from_raw_parts(p.aParam, 3);
    let a = std::slice::from_raw_parts(a, 4);
    let dx = (prm[0] - prm[0].clamp(a[0], a[1])).abs();
    let dy = (prm[1] - prm[1].clamp(a[2], a[3])).abs();
    *res = (dx * dx + dy * dy <= prm[2] * prm[2]) as c_int;
    SQLITE_OK
}

#[test]
fn test_rtree_geometry_callback() {
    unsafe {
        static CTX: i32 = 42;
        let db = open_mem();
        grid(db);
        let rc = sqlite3_rtree_geometry_callback(
            db,
            c("circle").as_ptr(),
            Some(circle_geom),
            &CTX as *const i32 as *mut c_void,
        );
        assert_eq!(rc, SQLITE_OK);
        let freed = GEOM_USER_FREED.load(Ordering::SeqCst);
        // Boxes within distance 1 of (5, 5): the four around the point and
        // their edge neighbours.
        let got = ints(
            db,
            "SELECT id FROM rt WHERE id MATCH circle(5, 5, 1) ORDER BY id",
        );
        let want: Vec<i64> = (1..=100)
            .filter(|&id: &i64| {
                let (x, y) = (((id - 1) / 10) as f64, ((id - 1) % 10) as f64);
                let dx = 5.0 - 5f64.clamp(x, x + 1.0);
                let dy = 5.0 - 5f64.clamp(y, y + 1.0);
                dx * dx + dy * dy <= 1.0
            })
            .collect();
        assert_eq!(got, want);
        // The constraint's pUser was freed when the scan ended.
        assert_eq!(GEOM_USER_FREED.load(Ordering::SeqCst), freed + 1);
        // Combined with a range constraint.
        assert_eq!(
            ints(
                db,
                "SELECT id FROM rt WHERE id MATCH circle(5, 5, 1) AND x0 >= 5 ORDER BY id"
            ),
            want.iter()
                .copied()
                .filter(|id| (id - 1) / 10 >= 5)
                .collect::<Vec<_>>()
        );
        // A callback error fails the statement.
        let mut stmt = ptr::null_mut();
        let rc = sqlite3_prepare_v2(
            db,
            c("SELECT id FROM rt WHERE id MATCH circle(1, 2)").as_ptr(),
            -1,
            &mut stmt,
            ptr::null_mut(),
        );
        assert_eq!(rc, SQLITE_OK, "{}", errmsg(db));
        assert_eq!(sqlite3_step(stmt), SQLITE_ERROR);
        sqlite3_finalize(stmt);
        sqlite3_close(db);
    }
}

static QUERY_DESTROYED: AtomicI32 = AtomicI32::new(0);
static MAX_LEVEL_SEEN: AtomicI64 = AtomicI64::new(-1);

unsafe extern "C" fn query_destroy(p: *mut c_void) {
    assert_eq!(*(p as *const i32), 7);
    QUERY_DESTROYED.fetch_add(1, Ordering::SeqCst);
}

/// nearest(x, y, tag): every box, nearest to (x, y) first; `tag` must be an integer.
unsafe extern "C" fn nearest_query(p: *mut sqlite3_rtree_query_info) -> c_int {
    let p = &mut *p;
    if p.nParam != 3 || p.nCoord != 4 {
        return SQLITE_ERROR;
    }
    let args = std::slice::from_raw_parts(p.apSqlParam, 3);
    if sqlite3_value_type(args[2]) != SQLITE_INTEGER || sqlite3_value_int64(args[2]) != 9 {
        return SQLITE_ERROR;
    }
    MAX_LEVEL_SEEN.fetch_max(p.mxLevel as i64, Ordering::SeqCst);
    let prm = std::slice::from_raw_parts(p.aParam, 2);
    let a = std::slice::from_raw_parts(p.aCoord, 4);
    let dx = prm[0] - prm[0].clamp(a[0], a[1]);
    let dy = prm[1] - prm[1].clamp(a[2], a[3]);
    p.rScore = dx * dx + dy * dy;
    p.eWithin = if p.iLevel == 0 {
        FULLY_WITHIN
    } else {
        PARTLY_WITHIN
    };
    if p.rScore > 8.0 {
        p.eWithin = NOT_WITHIN;
    }
    SQLITE_OK
}

#[test]
fn test_rtree_query_callback() {
    unsafe {
        static CTX: i32 = 7;
        let db = open_mem();
        grid(db);
        let rc = sqlite3_rtree_query_callback(
            db,
            c("nearest").as_ptr(),
            Some(nearest_query),
            &CTX as *const i32 as *mut c_void,
            Some(query_destroy),
        );
        assert_eq!(rc, SQLITE_OK);
        // Rows come back in score order: distance from (0.5, 9.5).
        let got = ints(db, "SELECT id FROM rt WHERE id MATCH nearest(0.5, 9.5, 9)");
        let mut want: Vec<(f64, i64)> = (1..=100)
            .map(|id: i64| {
                let (x, y) = (((id - 1) / 10) as f64, ((id - 1) % 10) as f64);
                let dx = 0.5 - 0.5f64.clamp(x, x + 1.0);
                let dy = 9.5 - 9.5f64.clamp(y, y + 1.0);
                (dx * dx + dy * dy, id)
            })
            .filter(|(d, _)| *d <= 8.0)
            .collect();
        want.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let scores: Vec<f64> = got
            .iter()
            .map(|id| want.iter().find(|w| w.1 == *id).unwrap().0)
            .collect();
        assert_eq!(got.len(), want.len());
        assert!(scores.windows(2).all(|w| w[0] <= w[1]), "{got:?}");
        assert_eq!(MAX_LEVEL_SEEN.load(Ordering::SeqCst), 2);
        // The wrong argument type is a callback error.
        let mut stmt = ptr::null_mut();
        sqlite3_prepare_v2(
            db,
            c("SELECT id FROM rt WHERE id MATCH nearest(0, 0, 'x')").as_ptr(),
            -1,
            &mut stmt,
            ptr::null_mut(),
        );
        assert_eq!(sqlite3_step(stmt), SQLITE_ERROR);
        sqlite3_finalize(stmt);
        assert_eq!(QUERY_DESTROYED.load(Ordering::SeqCst), 0);
        sqlite3_close(db);
        // The destructor runs when the function goes away with the connection.
        assert_eq!(QUERY_DESTROYED.load(Ordering::SeqCst), 1);
    }
}
