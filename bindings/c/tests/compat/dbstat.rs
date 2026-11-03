//! dbstat virtual table tests. Values are SQLite 3.54's on the same file.

use libc::c_int;
use std::ffi::{CStr, CString};
use std::ptr;

use super::{
    sqlite3, sqlite3_close, sqlite3_column_count, sqlite3_column_name, sqlite3_column_text,
    sqlite3_column_type, sqlite3_errmsg, sqlite3_exec, sqlite3_finalize, sqlite3_open,
    sqlite3_prepare_v2, sqlite3_step,
};

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_NULL: c_int = 5;

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

unsafe fn errmsg(db: *mut sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db))
        .to_string_lossy()
        .into_owned()
}

unsafe fn exec(db: *mut sqlite3, sql: &str) -> c_int {
    sqlite3_exec(db, c(sql).as_ptr(), None, ptr::null_mut(), ptr::null_mut())
}

unsafe fn exec_ok(db: *mut sqlite3, sql: &str) {
    assert_eq!(exec(db, sql), SQLITE_OK, "{sql}: {}", errmsg(db));
}

/// Rows joined with '|', NULL as "NULL".
unsafe fn query(db: *mut sqlite3, sql: &str) -> Vec<String> {
    let mut stmt = ptr::null_mut();
    let rc = sqlite3_prepare_v2(db, c(sql).as_ptr(), -1, &mut stmt, ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "{sql}: {}", errmsg(db));
    let mut rows = Vec::new();
    loop {
        match sqlite3_step(stmt) {
            SQLITE_ROW => {
                let n = sqlite3_column_count(stmt);
                let row: Vec<String> = (0..n)
                    .map(|i| {
                        if sqlite3_column_type(stmt, i) == SQLITE_NULL {
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
            SQLITE_DONE => break,
            rc => panic!("{sql}: step rc={rc}: {}", errmsg(db)),
        }
    }
    sqlite3_finalize(stmt);
    rows
}

unsafe fn column_names(db: *mut sqlite3, sql: &str) -> Vec<String> {
    let mut stmt = ptr::null_mut();
    let rc = sqlite3_prepare_v2(db, c(sql).as_ptr(), -1, &mut stmt, ptr::null_mut());
    assert_eq!(rc, SQLITE_OK, "{sql}: {}", errmsg(db));
    let names = (0..sqlite3_column_count(stmt))
        .map(|i| {
            CStr::from_ptr(sqlite3_column_name(stmt, i))
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    sqlite3_finalize(stmt);
    names
}

/// A 1 KiB-page file with a two-level table, an index and overflow chains.
unsafe fn open_fixture(dir: &tempfile::TempDir) -> *mut sqlite3 {
    let path = dir.path().join("dbstat.db");
    let mut db = ptr::null_mut();
    let p = c(path.to_str().unwrap());
    assert_eq!(sqlite3_open(p.as_ptr(), &mut db), SQLITE_OK);
    exec_ok(db, "PRAGMA page_size=1024");
    exec_ok(db, "CREATE TABLE t(a INTEGER PRIMARY KEY, b BLOB)");
    exec_ok(db, "CREATE INDEX ti ON t(b)");
    exec_ok(
        db,
        "WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i<14) \
         INSERT INTO t SELECT i, zeroblob((i*197)%2500) FROM s",
    );
    db
}

const FIXTURE_PAGES: &[&str] = &[
    "t|/|2|internal|8|0|956|0|1024|1024",
    "t|/000/|6|leaf|2|599|407|398|5120|1024",
    "t|/001/|7|leaf|1|595|416|595|6144|1024",
    "t|/002/|9|leaf|1|792|219|792|8192|1024",
    "t|/003/|11|leaf|1|989|22|989|10240|1024",
    "t|/004/|14|leaf|2|529|469|1383|13312|1024",
    "t|/004/000+000000|13|overflow|0|1020|0|0|12288|1024",
    "t|/004/001+000000|17|overflow|0|1020|0|0|16384|1024",
    "t|/005/|23|leaf|1|560|447|1580|22528|1024",
    "t|/005/000+000000|22|overflow|0|1020|0|0|21504|1024",
    "t|/006/|27|leaf|1|757|250|1777|26624|1024",
    "t|/006/000+000000|26|overflow|0|1020|0|0|25600|1024",
    "t|/007/|31|leaf|1|954|53|1974|30720|1024",
    "t|/007/000+000000|30|overflow|0|1020|0|0|29696|1024",
    "t|/008/|36|leaf|4|786|203|2368|35840|1024",
    "t|/008/000+000000|34|overflow|0|1020|0|0|33792|1024",
    "t|/008/000+000001|35|overflow|0|1020|0|0|34816|1024",
    "t|/008/001+000000|40|overflow|0|1020|0|0|39936|1024",
    "t|/008/001+000001|41|overflow|0|1020|0|0|40960|1024",
    "ti|/|3|internal|1|103|897|990|2048|1024",
    "ti|/000+000000|10|overflow|0|887|133|0|9216|1024",
    "ti|/000/|20|leaf|6|679|298|793|19456|1024",
    "ti|/000/002+000000|42|overflow|0|160|860|0|41984|1024",
    "ti|/000/003+000000|4|overflow|0|296|724|0|3072|1024",
    "ti|/000/004+000000|5|overflow|0|493|527|0|4096|1024",
    "ti|/000/005+000000|8|overflow|0|690|330|0|7168|1024",
    "ti|/001/|21|leaf|7|814|146|2369|20480|1024",
    "ti|/001/000+000000|12|overflow|0|1020|0|0|11264|1024",
    "ti|/001/001+000000|15|overflow|0|1020|0|0|14336|1024",
    "ti|/001/001+000001|16|overflow|0|261|759|0|15360|1024",
    "ti|/001/002+000000|18|overflow|0|1020|0|0|17408|1024",
    "ti|/001/002+000001|19|overflow|0|458|562|0|18432|1024",
    "ti|/001/003+000000|24|overflow|0|1020|0|0|23552|1024",
    "ti|/001/003+000001|25|overflow|0|655|365|0|24576|1024",
    "ti|/001/004+000000|28|overflow|0|1020|0|0|27648|1024",
    "ti|/001/004+000001|29|overflow|0|852|168|0|28672|1024",
    "ti|/001/005+000000|32|overflow|0|1020|0|0|31744|1024",
    "ti|/001/005+000001|33|overflow|0|1020|0|0|32768|1024",
    "ti|/001/006+000000|37|overflow|0|1020|0|0|36864|1024",
    "ti|/001/006+000001|38|overflow|0|1020|0|0|37888|1024",
    "ti|/001/006+000002|39|overflow|0|226|794|0|38912|1024",
];

#[test]
fn test_dbstat_reports_btree_pages() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_fixture(&dir);
        assert_eq!(
            column_names(db, "SELECT * FROM dbstat"),
            [
                "name",
                "path",
                "pageno",
                "pagetype",
                "ncell",
                "payload",
                "unused",
                "mx_payload",
                "pgoffset",
                "pgsize"
            ]
        );
        assert_eq!(
            query(db, "SELECT * FROM dbstat WHERE name<>'sqlite_schema'"),
            FIXTURE_PAGES
        );
        assert_eq!(
            query(
                db,
                "SELECT name, path, pageno, pagetype FROM dbstat WHERE name='sqlite_schema'"
            ),
            ["sqlite_schema|/|1|leaf"]
        );
        // Hidden columns.
        assert_eq!(
            query(db, "SELECT DISTINCT schema, aggregate FROM dbstat"),
            ["main|0"]
        );
        // Each page appears once.
        assert_eq!(
            query(db, "SELECT count(*) = count(DISTINCT pageno) FROM dbstat"),
            ["1"]
        );
        sqlite3_close(db);
    }
}

#[test]
fn test_dbstat_aggregate_and_arguments() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_fixture(&dir);
        let agg = [
            "t|NULL|19|NULL|22|15741|3442|2368|NULL|19456",
            "ti|NULL|22|NULL|14|15754|6563|2369|NULL|22528",
        ];
        assert_eq!(
            query(
                db,
                "SELECT * FROM dbstat('main', 1) WHERE name<>'sqlite_schema'"
            ),
            agg
        );
        assert_eq!(
            query(
                db,
                "SELECT * FROM dbstat WHERE aggregate=1 AND name<>'sqlite_schema'"
            ),
            agg
        );
        assert_eq!(
            query(db, "SELECT * FROM dbstat WHERE name='t' AND aggregate='1'"),
            agg[..1]
        );
        assert_eq!(
            query(
                db,
                "SELECT schema, aggregate FROM dbstat('MAIN', 1) WHERE name='t'"
            ),
            ["main|1"]
        );
        // The aggregate row sums the per-page rows.
        assert_eq!(
            query(
                db,
                "SELECT count(*), sum(ncell), sum(payload), sum(unused), max(mx_payload), \
                 sum(pgsize) FROM dbstat WHERE name='ti'"
            ),
            ["22|14|15754|6563|2369|22528"]
        );
        // An unknown schema yields no rows.
        assert_eq!(query(db, "SELECT count(*) FROM dbstat('nosuch')"), ["0"]);
        assert_eq!(
            query(db, "SELECT count(*) FROM dbstat WHERE schema='nosuch'"),
            ["0"]
        );
        sqlite3_close(db);
    }
}

#[test]
fn test_dbstat_analyzer_queries() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_fixture(&dir);
        // sqlite3_analyzer's per-btree and per-pagetype summaries.
        assert_eq!(
            query(
                db,
                "SELECT name, pagetype, count(*), sum(ncell), sum(payload), sum(unused) \
                 FROM dbstat WHERE name<>'sqlite_schema' GROUP BY name, pagetype \
                 ORDER BY name, pagetype"
            ),
            [
                "t|internal|1|8|0|956",
                "t|leaf|9|14|6561|2486",
                "t|overflow|9|0|9180|0",
                "ti|internal|1|1|103|897",
                "ti|leaf|2|13|1493|444",
                "ti|overflow|19|0|14158|5222",
            ]
        );
        exec_ok(
            db,
            "CREATE TABLE space_used AS SELECT name, path, pageno, pagetype, ncell, payload, \
             unused, mx_payload FROM dbstat ORDER BY name, path",
        );
        assert_eq!(
            query(
                db,
                "SELECT count(*), sum(payload) FROM space_used WHERE name IN ('t', 'ti')"
            ),
            ["41|31495"]
        );
        // dbstat as the inner table of a join.
        assert_eq!(
            query(
                db,
                "SELECT s.name, count(*) FROM sqlite_schema s JOIN dbstat d ON d.name = s.name \
                 WHERE s.type = 'index' GROUP BY s.name"
            ),
            ["ti|22"]
        );
        sqlite3_close(db);
    }
}

#[test]
fn test_dbstat_create_virtual_table() {
    unsafe {
        let dir = tempfile::tempdir().unwrap();
        let db = open_fixture(&dir);
        exec_ok(db, "CREATE VIRTUAL TABLE st USING dbstat(main)");
        assert_eq!(
            query(
                db,
                "SELECT name, pageno, ncell FROM st WHERE name='t' AND path='/'"
            ),
            ["t|2|8"]
        );
        // The eponymous table has a schema and cannot be dropped.
        assert_eq!(
            query(
                db,
                "SELECT group_concat(name) FROM pragma_table_info('dbstat')"
            ),
            ["name,path,pageno,pagetype,ncell,payload,unused,mx_payload,pgoffset,pgsize"]
        );
        assert_ne!(exec(db, "DROP TABLE dbstat"), SQLITE_OK);
        assert!(errmsg(db).ends_with("table dbstat may not be dropped"));
        assert_ne!(
            exec(db, "CREATE VIRTUAL TABLE st2 USING dbstat(nosuch)"),
            SQLITE_OK
        );
        assert_eq!(errmsg(db), "no such database: nosuch");
        // An empty database has no pages.
        let mut mem = ptr::null_mut();
        assert_eq!(sqlite3_open(c(":memory:").as_ptr(), &mut mem), SQLITE_OK);
        assert_eq!(query(mem, "SELECT count(*) FROM dbstat"), ["0"]);
        sqlite3_close(mem);
        sqlite3_close(db);
    }
}
