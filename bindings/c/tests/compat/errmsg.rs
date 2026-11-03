//! Error text and extended codes. The corpus below was produced by running each
//! statement against SQLite 3.54 (setup first, foreign keys off); every entry
//! must also hold on turso, so the text consumers match on stays SQLite's.
#![allow(non_camel_case_types, non_upper_case_globals)]
use libc::{c_char, c_int, c_void};
use std::ffi::{CStr, CString};
use std::ptr;

type Cmp = unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int;

#[repr(C)]
struct sqlite3 {
    _private: [u8; 0],
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
    fn sqlite3_open(filename: *const c_char, db: *mut *mut sqlite3) -> c_int;
    fn sqlite3_close(db: *mut sqlite3) -> c_int;
    fn sqlite3_exec(
        db: *mut sqlite3,
        sql: *const c_char,
        cb: *const c_void,
        arg: *mut c_void,
        err: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char;
    fn sqlite3_extended_errcode(db: *mut sqlite3) -> c_int;
    fn sqlite3_prepare_v2(
        db: *mut sqlite3,
        sql: *const c_char,
        n: c_int,
        stmt: *mut *mut c_void,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_step(stmt: *mut c_void) -> c_int;
    fn sqlite3_finalize(stmt: *mut c_void) -> c_int;
    fn sqlite3_create_collation_v2(
        db: *mut sqlite3,
        name: *const c_char,
        enc: c_int,
        ctx: *mut c_void,
        cmp: Option<Cmp>,
        destroy: *const c_void,
    ) -> c_int;
    fn sqlite3_create_function_v2(
        db: *mut sqlite3,
        name: *const c_char,
        nargs: c_int,
        enc: c_int,
        ctx: *mut c_void,
        f: Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_void)>,
        step: *const c_void,
        fin: *const c_void,
        destroy: *const c_void,
    ) -> c_int;
    fn sqlite3_result_null(ctx: *mut c_void);
    fn sqlite3_blob_open(
        db: *mut sqlite3,
        schema: *const c_char,
        table: *const c_char,
        column: *const c_char,
        rowid: i64,
        flags: c_int,
        blob: *mut *mut c_void,
    ) -> c_int;
    fn sqlite3_blob_write(blob: *mut c_void, buf: *const c_void, n: c_int, off: c_int) -> c_int;
    fn sqlite3_blob_close(blob: *mut c_void) -> c_int;
}

const SETUP: &str = "
CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT UNIQUE, c INT NOT NULL DEFAULT 1, d INT CHECK(d>0));
CREATE TABLE u(x, y, UNIQUE(x,y));
CREATE TABLE p(id INTEGER PRIMARY KEY);
CREATE TABLE ch(id INT, pid INT REFERENCES p(id));
CREATE INDEX ti ON t(c);
CREATE VIEW v AS SELECT a FROM t;
CREATE TRIGGER trg AFTER INSERT ON u BEGIN SELECT 1; END;
INSERT INTO t VALUES(1,'x',1,1);
INSERT INTO u VALUES(1,2);
PRAGMA foreign_keys=OFF;
";

// Not asserted (turso still differs from SQLite): ATTACH/DETACH name checks,
// INSTEAD OF triggers on tables, RAISE outside a trigger, MATCH without fts,
// dangling FOREIGN KEY / PRIMARY KEY columns, `ON CONFLICT(nosuch)`, nullif
// arity, sqlite_compileoption_used arity, lazily checked view column lists,
// PRAGMA argument typing, `SELECT 1 COLLATE nosuch`, `UNION` with no right side,
// "ambiguous column name: main.t.a", sub-select vs row-value text in WHERE.
const CORPUS: &[(&str, i32, &str)] = &[
    ("SELEC 1", 1, "near \"SELEC\": syntax error"),
    ("SELECT FROM", 1, "near \"FROM\": syntax error"),
    ("SELECT * FROM", 1, "incomplete input"),
    ("SELECT 1 +", 1, "incomplete input"),
    ("SELECT (1", 1, "incomplete input"),
    ("SELECT 1)", 1, "near \")\": syntax error"),
    ("CREATE TABLE", 1, "incomplete input"),
    ("CREATE TABLE x(", 1, "incomplete input"),
    ("INSERT INTO", 1, "incomplete input"),
    ("INSERT INTO t VALUES", 1, "incomplete input"),
    ("x", 1, "near \"x\": syntax error"),
    ("SELECT 'abc", 1, "unrecognized token: \"'abc\""),
    ("SELECT \"abc", 1, "unrecognized token: \"\"abc\""),
    ("SELECT 1 FROM t WHERE", 1, "incomplete input"),
    ("SELECT * FROM t ORDER", 1, "incomplete input"),
    ("DELETE t", 1, "near \"t\": syntax error"),
    ("UPDATE t", 1, "incomplete input"),
    ("UPDATE t SET", 1, "incomplete input"),
    ("DROP", 1, "incomplete input"),
    ("ALTER TABLE t", 1, "incomplete input"),
    ("SELECT * FROM nosuch", 1, "no such table: nosuch"),
    ("SELECT nosuch FROM t", 1, "no such column: nosuch"),
    ("SELECT t.nosuch FROM t", 1, "no such column: t.nosuch"),
    ("SELECT nosuch.a FROM t", 1, "no such column: nosuch.a"),
    ("INSERT INTO nosuch VALUES(1)", 1, "no such table: nosuch"),
    ("UPDATE nosuch SET a=1", 1, "no such table: nosuch"),
    ("DELETE FROM nosuch", 1, "no such table: nosuch"),
    ("UPDATE t SET nosuch=1", 1, "no such column: nosuch"),
    (
        "INSERT INTO t(nosuch) VALUES(1)",
        1,
        "table t has no column named nosuch",
    ),
    ("DROP TABLE nosuch", 1, "no such table: nosuch"),
    ("DROP INDEX nosuch", 1, "no such index: nosuch"),
    ("DROP VIEW nosuch", 1, "no such view: nosuch"),
    ("DROP TRIGGER nosuch", 1, "no such trigger: nosuch"),
    ("DROP TABLE v", 1, "use DROP VIEW to delete view v"),
    ("DROP VIEW t", 1, "use DROP TABLE to delete table t"),
    ("CREATE TABLE t(a)", 1, "table t already exists"),
    ("CREATE INDEX ti ON t(c)", 1, "index ti already exists"),
    ("CREATE VIEW v AS SELECT 1", 1, "view v already exists"),
    (
        "CREATE TRIGGER trg AFTER INSERT ON u BEGIN SELECT 1; END",
        1,
        "trigger trg already exists",
    ),
    (
        "CREATE INDEX i2 ON nosuch(a)",
        1,
        "no such table: main.nosuch",
    ),
    ("CREATE INDEX i2 ON t(nosuch)", 1, "no such column: nosuch"),
    ("CREATE TABLE t2(a, a)", 1, "duplicate column name: a"),
    (
        "CREATE TABLE t2(a PRIMARY KEY, b PRIMARY KEY)",
        1,
        "table \"t2\" has more than one primary key",
    ),
    (
        "CREATE TABLE sqlite_x(a)",
        1,
        "object name reserved for internal use: sqlite_x",
    ),
    (
        "CREATE TABLE t2(a) WITHOUT ROWID",
        1,
        "PRIMARY KEY missing on table t2",
    ),
    (
        "INSERT INTO t VALUES(1,'y',1,1)",
        1555,
        "UNIQUE constraint failed: t.a",
    ),
    (
        "INSERT INTO t VALUES(2,'x',1,1)",
        2067,
        "UNIQUE constraint failed: t.b",
    ),
    (
        "INSERT INTO t VALUES(2,'z',NULL,1)",
        1299,
        "NOT NULL constraint failed: t.c",
    ),
    (
        "INSERT INTO t VALUES(2,'z',1,0)",
        275,
        "CHECK constraint failed: d>0",
    ),
    (
        "INSERT INTO u VALUES(1,2)",
        2067,
        "UNIQUE constraint failed: u.x, u.y",
    ),
    (
        "INSERT INTO t(a,b) VALUES(NULL,'q'),(1,'r')",
        1555,
        "UNIQUE constraint failed: t.a",
    ),
    (
        "INSERT INTO t(c) VALUES(NULL)",
        1299,
        "NOT NULL constraint failed: t.c",
    ),
    (
        "INSERT INTO t VALUES(1,2)",
        1,
        "table t has 4 columns but 2 values were supplied",
    ),
    (
        "INSERT INTO t VALUES(1,2,3,4,5)",
        1,
        "table t has 4 columns but 5 values were supplied",
    ),
    ("INSERT INTO t(a,b) VALUES(1)", 1, "1 values for 2 columns"),
    (
        "INSERT INTO t SELECT 1",
        1,
        "table t has 4 columns but 1 values were supplied",
    ),
    (
        "INSERT INTO v VALUES(1)",
        1,
        "cannot modify v because it is a view",
    ),
    (
        "UPDATE v SET a=1",
        1,
        "cannot modify v because it is a view",
    ),
    ("DELETE FROM v", 1, "cannot modify v because it is a view"),
    ("UPDATE t SET a='abc'", 20, "datatype mismatch"),
    ("INSERT INTO t(a) VALUES('abc')", 20, "datatype mismatch"),
    ("INSERT INTO t(a) VALUES(1.5)", 20, "datatype mismatch"),
    (
        "UPDATE t SET c=NULL",
        1299,
        "NOT NULL constraint failed: t.c",
    ),
    ("UPDATE t SET d=-1", 275, "CHECK constraint failed: d>0"),
    (
        "SELECT * FROM t t1, t t2 WHERE a=1",
        1,
        "ambiguous column name: a",
    ),
    ("SELECT a FROM t, t", 1, "ambiguous column name: a"),
    ("SELECT a FROM t t1, t t2", 1, "ambiguous column name: a"),
    ("SELECT nosuchfn(1)", 1, "no such function: nosuchfn"),
    (
        "SELECT abs()",
        1,
        "wrong number of arguments to function abs()",
    ),
    (
        "SELECT abs(1,2)",
        1,
        "wrong number of arguments to function abs()",
    ),
    (
        "SELECT substr(1)",
        1,
        "wrong number of arguments to function substr()",
    ),
    (
        "SELECT count(1,2)",
        1,
        "wrong number of arguments to function count()",
    ),
    (
        "SELECT sum()",
        1,
        "wrong number of arguments to function sum()",
    ),
    (
        "SELECT length()",
        1,
        "wrong number of arguments to function length()",
    ),
    (
        "SELECT * FROM t ORDER BY 5",
        1,
        "1st ORDER BY term out of range - should be between 1 and 4",
    ),
    (
        "SELECT * FROM t GROUP BY 5",
        1,
        "1st GROUP BY term out of range - should be between 1 and 4",
    ),
    (
        "SELECT a FROM t ORDER BY nosuch",
        1,
        "no such column: nosuch",
    ),
    ("SELECT 1 LIMIT 'x'", 20, "datatype mismatch"),
    (
        "SELECT * FROM t LIMIT -1 OFFSET 'a'",
        20,
        "datatype mismatch",
    ),
    (
        "SELECT * FROM t ORDER BY b COLLATE nosuch",
        257,
        "no such collation sequence: nosuch",
    ),
    (
        "CREATE TABLE t2(a COLLATE nosuch)",
        257,
        "no such collation sequence: nosuch",
    ),
    (
        "CREATE INDEX i2 ON t(b COLLATE nosuch)",
        257,
        "no such collation sequence: nosuch",
    ),
    (
        "SELECT ?1, ?2, ?9999999",
        1,
        "variable number must be between ?1 and ?250000",
    ),
    (
        "SELECT ?0",
        1,
        "variable number must be between ?1 and ?250000",
    ),
    (
        "SELECT 1 UNION SELECT 1,2",
        1,
        "SELECTs to the left and right of UNION do not have the same number of result columns",
    ),
    (
        "SELECT 1,2 UNION SELECT 1",
        1,
        "SELECTs to the left and right of UNION do not have the same number of result columns",
    ),
    (
        "SELECT 1 INTERSECT SELECT 1,2",
        1,
        "SELECTs to the left and right of INTERSECT do not have the same number of result columns",
    ),
    (
        "SELECT * FROM (SELECT 1,2) WHERE 1 IN (SELECT 1,2)",
        1,
        "sub-select returns 2 columns - expected 1",
    ),
    ("SELECT (1,2)", 1, "row value misused"),
    ("SELECT (1,2)=(1,2,3)", 1, "row value misused"),
    (
        "SELECT * FROM t WHERE a IN (1,2) AND (a,b) IN (1)",
        1,
        "IN(...) element has 1 term - expected 2",
    ),
    ("COMMIT", 1, "cannot commit - no transaction is active"),
    ("ROLLBACK", 1, "cannot rollback - no transaction is active"),
    ("SAVEPOINT", 1, "incomplete input"),
    ("RELEASE nosuch", 1, "no such savepoint: nosuch"),
    ("ROLLBACK TO nosuch", 1, "no such savepoint: nosuch"),
    (
        "BEGIN; BEGIN",
        1,
        "cannot start a transaction within a transaction",
    ),
    ("END", 1, "cannot commit - no transaction is active"),
    ("ROLLBACK TO x", 1, "no such savepoint: x"),
    ("DETACH nosuch", 1, "no such database: nosuch"),
    ("SELECT * FROM nosuchdb.t", 1, "no such table: nosuchdb.t"),
    ("CREATE TABLE nosuchdb.t(a)", 1, "unknown database nosuchdb"),
    ("ALTER TABLE nosuch RENAME TO x", 1, "no such table: nosuch"),
    (
        "ALTER TABLE t RENAME TO u",
        1,
        "there is already another table or index with this name: u",
    ),
    (
        "ALTER TABLE t RENAME COLUMN nosuch TO x",
        1,
        "no such column: \"nosuch\"",
    ),
    ("ALTER TABLE t ADD COLUMN a", 1, "duplicate column name: a"),
    (
        "ALTER TABLE t ADD COLUMN z NOT NULL",
        1,
        "Cannot add a NOT NULL column with default value NULL",
    ),
    (
        "ALTER TABLE t ADD COLUMN z PRIMARY KEY",
        1,
        "Cannot add a PRIMARY KEY column",
    ),
    (
        "ALTER TABLE t ADD COLUMN z UNIQUE",
        1,
        "Cannot add a UNIQUE column",
    ),
    (
        "ALTER TABLE t DROP COLUMN nosuch",
        1,
        "no such column: \"nosuch\"",
    ),
    (
        "ALTER TABLE t DROP COLUMN a",
        1,
        "cannot drop PRIMARY KEY column: \"a\"",
    ),
    (
        "ALTER TABLE t DROP COLUMN b",
        1,
        "cannot drop UNIQUE column: \"b\"",
    ),
    (
        "ALTER TABLE sqlite_master RENAME TO x",
        1,
        "table sqlite_master may not be altered",
    ),
    (
        "DROP TABLE sqlite_master",
        1,
        "table sqlite_master may not be dropped",
    ),
    (
        "INSERT INTO sqlite_master VALUES(1,2,3,4,5)",
        1,
        "table sqlite_master may not be modified",
    ),
    (
        "UPDATE sqlite_master SET name='x'",
        1,
        "table sqlite_master may not be modified",
    ),
    (
        "DELETE FROM sqlite_master",
        1,
        "table sqlite_master may not be modified",
    ),
    ("CREATE INDEX i2 ON v(a)", 1, "views may not be indexed"),
    (
        "CREATE TRIGGER t2 AFTER INSERT ON nosuch BEGIN SELECT 1; END",
        1,
        "no such table: main.nosuch",
    ),
    (
        "CREATE TRIGGER t2 AFTER INSERT ON v BEGIN SELECT 1; END",
        1,
        "cannot create AFTER trigger on view: v",
    ),
    ("SELECT json('{')", 1, "malformed JSON"),
    ("SELECT json_extract('{}','x')", 1, "bad JSON path: 'x'"),
    ("SELECT * FROM nosuchfn(1)", 1, "no such table: nosuchfn"),
    (
        "SELECT * FROM pragma_nosuch",
        1,
        "no such table: pragma_nosuch",
    ),
    ("SELECT abs(-9223372036854775808)", 1, "integer overflow"),
    ("SELECT zeroblob(2000000000)", 18, "string or blob too big"),
    (
        "SELECT like('a')",
        1,
        "wrong number of arguments to function like()",
    ),
    (
        "SELECT like('a','b','cc')",
        1,
        "ESCAPE expression must be a single character",
    ),
    (
        "SELECT 'a' LIKE 'b' ESCAPE 'xx'",
        1,
        "ESCAPE expression must be a single character",
    ),
    (
        "SELECT * FROM t JOIN u USING(nosuch)",
        1,
        "cannot join using column nosuch - column not present in both tables",
    ),
    (
        "SELECT * FROM t JOIN u ON nosuch=1",
        1,
        "no such column: nosuch",
    ),
    (
        "SELECT * FROM t NATURAL JOIN nosuch",
        1,
        "no such table: nosuch",
    ),
    (
        "SELECT * FROM t INDEXED BY nosuch",
        1,
        "no such index: nosuch",
    ),
    (
        "SELECT * FROM t NOT INDEXED, u INDEXED BY ti",
        1,
        "no such index: ti",
    ),
    (
        "WITH x(a) AS (SELECT 1,2) SELECT * FROM x",
        1,
        "table x has 2 values for 1 columns",
    ),
    (
        "WITH x AS (SELECT * FROM x) SELECT * FROM x",
        1,
        "circular reference: x",
    ),
    ("SELECT * FROM x", 1, "no such table: x"),
    (
        "SELECT sum(sum(a)) FROM t",
        1,
        "misuse of aggregate function sum()",
    ),
    (
        "SELECT a FROM t WHERE sum(a)>1",
        1,
        "misuse of aggregate function sum()",
    ),
    (
        "SELECT count(*) OVER () FROM t WHERE count(*) OVER ()",
        1,
        "misuse of window function count()",
    ),
    (
        "SELECT row_number() FROM t",
        1,
        "misuse of window function row_number()",
    ),
    (
        "SELECT nosuch() OVER () FROM t",
        1,
        "no such function: nosuch",
    ),
    (
        "SELECT abs(a) OVER () FROM t",
        1,
        "abs() may not be used as a window function",
    ),
    (
        "SELECT * FROM t WHERE a = (SELECT a FROM t, nosuch)",
        1,
        "no such table: nosuch",
    ),
    ("SELECT * FROM t AS", 1, "incomplete input"),
    ("SELECT * FROM (SELECT 1) JOIN", 1, "incomplete input"),
    (
        "SELECT 1 FROM t GROUP BY a HAVING nosuch",
        1,
        "no such column: nosuch",
    ),
    (
        "CREATE TABLE t2(a CHECK(nosuch>1))",
        1,
        "no such column: nosuch",
    ),
    (
        "CREATE TABLE t2(a DEFAULT (nosuch))",
        1,
        "default value of column [a] is not constant",
    ),
    (
        "CREATE TABLE t2(a DEFAULT (a))",
        1,
        "default value of column [a] is not constant",
    ),
    (
        "CREATE TABLE t2(a AS (nosuch))",
        1,
        "must have at least one non-generated column",
    ),
    (
        "CREATE TABLE t2(a, b AS (a) DEFAULT 1)",
        1,
        "cannot use DEFAULT on a generated column",
    ),
    (
        "CREATE TABLE t2(a, b AS (a)); INSERT INTO t2(b) VALUES(1)",
        1,
        "cannot INSERT into generated column \"b\"",
    ),
    (
        "CREATE TABLE t2(a PRIMARY KEY AUTOINCREMENT)",
        1,
        "AUTOINCREMENT is only allowed on an INTEGER PRIMARY KEY",
    ),
    (
        "CREATE TABLE t2(a INTEGER PRIMARY KEY DESC AUTOINCREMENT, b)",
        1,
        "AUTOINCREMENT is only allowed on an INTEGER PRIMARY KEY",
    ),
    (
        "CREATE TEMP TABLE main.t2(a)",
        1,
        "temporary table name must be unqualified",
    ),
    (
        "CREATE TABLE t2 AS SELECT * FROM nosuch",
        1,
        "no such table: nosuch",
    ),
    ("CREATE TABLE t2(a) STRICT", 1, "missing datatype for t2.a"),
    (
        "CREATE TABLE t2(a BADTYPE) STRICT",
        1,
        "unknown datatype for t2.a: \"BADTYPE\"",
    ),
    (
        "CREATE TABLE t2(a INT) STRICT; INSERT INTO t2 VALUES('x')",
        3091,
        "cannot store TEXT value in INT column t2.a",
    ),
    (
        "CREATE UNIQUE INDEX i2 ON t(c); INSERT INTO t(a,b,c) VALUES(5,'k',1)",
        2067,
        "UNIQUE constraint failed: t.c",
    ),
    (
        "INSERT OR ROLLBACK INTO t VALUES(1,'y',1,1)",
        1555,
        "UNIQUE constraint failed: t.a",
    ),
    (
        "REPLACE INTO v VALUES(1)",
        1,
        "cannot modify v because it is a view",
    ),
    ("INSERT INTO t(a) SELECT 1,2", 1, "2 values for 1 columns"),
    (
        "REINDEX nosuch",
        1,
        "unable to identify the object to be reindexed",
    ),
    ("ANALYZE nosuch", 1, "no such table: nosuch"),
    ("EXPLAIN", 1, "incomplete input"),
    ("EXPLAIN QUERY PLAN", 1, "incomplete input"),
    (
        "SELECT * FROM t ORDER BY a LIMIT 1 OFFSET",
        1,
        "incomplete input",
    ),
    ("SELECT CASE WHEN", 1, "incomplete input"),
    ("SELECT CASE 1 WHEN 1 THEN", 1, "incomplete input"),
    ("SELECT 1 BETWEEN 1", 1, "incomplete input"),
    ("SELECT 1 IN (", 1, "incomplete input"),
    ("SELECT x'0'", 1, "unrecognized token: \"x'0'\""),
    ("SELECT 1e", 1, "unrecognized token: \"1e\""),
    ("SELECT 0x", 1, "unrecognized token: \"0x\""),
    ("SELECT 1.2.3", 1, "near \".3\": syntax error"),
    ("SELECT 12abc", 1, "unrecognized token: \"12abc\""),
    ("SELECT $", 1, "unrecognized token: \"$\""),
    ("SELECT @", 1, "unrecognized token: \"@\""),
    ("SELECT :", 1, "unrecognized token: \":\""),
    ("SELECT #", 1, "unrecognized token: \"#\""),
    ("SELECT \x01", 1, "unrecognized token: \"\x01\""),
    ("SELECT 1 ! 2", 1, "unrecognized token: \"!\""),
    ("SELECT 1 ~ 2", 1, "near \"~\": syntax error"),
    ("SELECT * FROM t WHERE a = ", 1, "incomplete input"),
    (
        "SELECT * FROM t WHERE a == == 1",
        1,
        "near \"==\": syntax error",
    ),
    ("SELECT a,, b FROM t", 1, "near \",\": syntax error"),
    ("SELECT a, FROM t", 1, "near \"FROM\": syntax error"),
    ("SELECT * FROM t,", 1, "incomplete input"),
    ("INSERT INTO t VALUES(1,", 1, "incomplete input"),
    ("INSERT INTO t VALUES()", 1, "near \")\": syntax error"),
    ("INSERT INTO t() VALUES()", 1, "near \")\": syntax error"),
    ("CREATE TABLE t2()", 1, "near \")\": syntax error"),
    ("CREATE TABLE t2(a,)", 1, "near \")\": syntax error"),
    ("CREATE INDEX i ON t()", 1, "near \")\": syntax error"),
    ("UPDATE t SET a = 1,", 1, "incomplete input"),
    ("DELETE FROM t WHERE", 1, "incomplete input"),
    ("SELECT * FROM t LIMIT", 1, "incomplete input"),
    ("SELECT 1 OFFSET 1", 1, "near \"1\": syntax error"),
    ("SELECT * FROM t JOIN u ON", 1, "incomplete input"),
    (
        "SELECT * FROM t WHERE a IN (SELECT)",
        1,
        "near \")\": syntax error",
    ),
    ("CREATE VIEW v2", 1, "incomplete input"),
    ("CREATE VIEW v2 AS", 1, "incomplete input"),
    ("CREATE TRIGGER", 1, "incomplete input"),
    ("CREATE TRIGGER tr AFTER INSERT ON t", 1, "incomplete input"),
    (
        "SELECT * FROM v WHERE nosuch=1",
        1,
        "no such column: nosuch",
    ),
    ("SELECT json_each('x')", 1, "no such function: json_each"),
    (
        "SELECT group_concat()",
        1,
        "wrong number of arguments to function group_concat()",
    ),
    (
        "SELECT group_concat(a,b,c) FROM t",
        1,
        "wrong number of arguments to function group_concat()",
    ),
    (
        "SELECT total()",
        1,
        "wrong number of arguments to function total()",
    ),
    (
        "SELECT min()",
        1,
        "wrong number of arguments to function min()",
    ),
    (
        "SELECT max()",
        1,
        "wrong number of arguments to function max()",
    ),
    (
        "SELECT coalesce(1)",
        1,
        "wrong number of arguments to function coalesce()",
    ),
    (
        "SELECT ifnull(1)",
        1,
        "wrong number of arguments to function ifnull()",
    ),
    (
        "SELECT replace(1,2)",
        1,
        "wrong number of arguments to function replace()",
    ),
    (
        "SELECT instr(1)",
        1,
        "wrong number of arguments to function instr()",
    ),
    (
        "SELECT hex()",
        1,
        "wrong number of arguments to function hex()",
    ),
    (
        "SELECT typeof()",
        1,
        "wrong number of arguments to function typeof()",
    ),
    (
        "SELECT round(1,2,3)",
        1,
        "wrong number of arguments to function round()",
    ),
    (
        "SELECT lower()",
        1,
        "wrong number of arguments to function lower()",
    ),
    (
        "SELECT trim()",
        1,
        "wrong number of arguments to function trim()",
    ),
    (
        "SELECT unicode()",
        1,
        "wrong number of arguments to function unicode()",
    ),
    (
        "SELECT likelihood(1)",
        1,
        "wrong number of arguments to function likelihood()",
    ),
    (
        "SELECT unlikely()",
        1,
        "wrong number of arguments to function unlikely()",
    ),
    (
        "SELECT random(1)",
        1,
        "wrong number of arguments to function random()",
    ),
    (
        "SELECT changes(1)",
        1,
        "wrong number of arguments to function changes()",
    ),
    (
        "SELECT sqlite_version(1)",
        1,
        "wrong number of arguments to function sqlite_version()",
    ),
    (
        "SELECT * FROM t WHERE nosuchtbl.a = 1",
        1,
        "no such column: nosuchtbl.a",
    ),
    (
        "SELECT main.nosuch.a FROM t",
        1,
        "no such column: main.nosuch.a",
    ),
    (
        "SELECT nosuchdb.t.a FROM t",
        1,
        "no such column: nosuchdb.t.a",
    ),
    ("SELECT t.* FROM u", 1, "no such table: t"),
    ("SELECT nosuch.* FROM t", 1, "no such table: nosuch"),
    (
        "SELECT * FROM t WHERE x.y.z.w=1",
        1,
        "near \".\": syntax error",
    ),
    ("SELECT * FROM t t1 WHERE t.a=1", 1, "no such column: t.a"),
    (
        "SELECT * FROM (SELECT 1) AS x WHERE x.nosuch=1",
        1,
        "no such column: x.nosuch",
    ),
    ("UPDATE t SET (a,b)=(1)", 1, "2 columns assigned 1 values"),
    (
        "UPDATE t SET (a,b)=(1,2,3)",
        1,
        "2 columns assigned 3 values",
    ),
    ("UPDATE t SET nosuch.a=1", 1, "near \".\": syntax error"),
    (
        "DELETE FROM t ORDER BY a",
        1,
        "near \"ORDER\": syntax error",
    ),
    ("DELETE FROM t LIMIT 1", 1, "near \"LIMIT\": syntax error"),
    (
        "UPDATE t SET a=1 LIMIT 1",
        1,
        "near \"LIMIT\": syntax error",
    ),
    (
        "INSERT INTO t VALUES(1,'x',1,1) ON CONFLICT(c) DO NOTHING",
        1,
        "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint",
    ),
    (
        "INSERT INTO t VALUES(1,'x',1,1) ON CONFLICT(a) DO UPDATE SET nosuch=1",
        1,
        "no such column: nosuch",
    ),
    (
        "INSERT INTO t SELECT * FROM t UNION SELECT 1 ON CONFLICT DO NOTHING",
        1,
        "SELECTs to the left and right of UNION do not have the same number of result columns",
    ),
    (
        "INSERT INTO t VALUES(1,'x',1,1) RETURNING nosuch",
        1,
        "no such column: nosuch",
    ),
    (
        "DELETE FROM t RETURNING nosuch",
        1,
        "no such column: nosuch",
    ),
    (
        "UPDATE t SET a=1 RETURNING nosuch",
        1,
        "no such column: nosuch",
    ),
    (
        "PRAGMA nosuchdb.user_version",
        1,
        "unknown database nosuchdb",
    ),
    ("SELECT load_extension('nosuch')", 1, "not authorized"),
    ("SELECT * FROM sqlite_master WHERE", 1, "incomplete input"),
    (
        "SELECT * FROM pragma_table_info('t','a','b')",
        1,
        "too many arguments on pragma_table_info() - max 2",
    ),
    (
        "SELECT * FROM json_each(1,2,3)",
        1,
        "too many arguments on json_each() - max 2",
    ),
    (
        "CREATE VIRTUAL TABLE vt USING nosuch(a)",
        1,
        "no such module: nosuch",
    ),
    ("CREATE VIRTUAL TABLE vt USING", 1, "incomplete input"),
    ("DROP TABLE IF EXISTS", 1, "incomplete input"),
    ("DROP TABLE t; SELECT * FROM t", 1, "no such table: t"),
    ("SELECT 1; SELECT nosuch", 1, "no such column: nosuch"),
    ("SELECT 1; SELEC 2", 1, "near \"SELEC\": syntax error"),
    (
        "CREATE TABLE a1(x); CREATE TABLE a1(x)",
        1,
        "table a1 already exists",
    ),
    ("SELECT * FROM t WHERE a IS NOT", 1, "incomplete input"),
    ("SELECT 1 NOT", 1, "incomplete input"),
    ("SELECT NOT", 1, "incomplete input"),
    ("SELECT 1 IS", 1, "incomplete input"),
    ("SELECT -", 1, "incomplete input"),
    (
        "SELECT * FROM t ORDER BY a ASC DESC",
        1,
        "near \"DESC\": syntax error",
    ),
    ("SELECT DISTINCT", 1, "incomplete input"),
    (
        "SELECT ALL DISTINCT 1",
        1,
        "near \"DISTINCT\": syntax error",
    ),
    ("SELECT 1 AS", 1, "incomplete input"),
    ("SELECT 1 AS 'x' 'y'", 1, "near \"'y'\": syntax error"),
    ("SELECT 1 x y", 1, "near \"y\": syntax error"),
    ("SELECT * FROM t AS x y", 1, "near \"y\": syntax error"),
    ("SELECT max(1,2) FROM", 1, "incomplete input"),
    ("SELECT a FROM t GROUP", 1, "incomplete input"),
    ("SELECT a FROM t HAVING", 1, "incomplete input"),
    (
        "SELECT sum(a) OVER (PARTITION) FROM t",
        1,
        "near \")\": syntax error",
    ),
    ("SELECT sum(a) OVER w FROM t", 1, "no such window: w"),
    (
        "SELECT sum(a) OVER (ROWS BETWEEN 1 FOLLOWING AND 1 PRECEDING) FROM t",
        1,
        "unsupported frame specification",
    ),
    (
        "SELECT sum(a) OVER (ORDER BY a ROWS BETWEEN UNBOUNDED FOLLOWING AND CURRENT ROW) FROM t",
        1,
        "near \"FOLLOWING\": syntax error",
    ),
    (
        "BEGIN IMMEDIATE; BEGIN",
        1,
        "cannot start a transaction within a transaction",
    ),
    (
        "BEGIN DEFERRED; COMMIT; COMMIT",
        1,
        "cannot commit - no transaction is active",
    ),
    ("SAVEPOINT a; RELEASE b", 1, "no such savepoint: b"),
    (
        "PRAGMA foreign_keys=ON; INSERT INTO ch VALUES(1,99)",
        787,
        "FOREIGN KEY constraint failed",
    ),
];

unsafe fn run(sql: &str) -> (c_int, String) {
    let mut db = ptr::null_mut();
    assert_eq!(sqlite3_open(c":memory:".as_ptr(), &mut db), 0);
    let setup = CString::new(SETUP).unwrap();
    assert_eq!(
        sqlite3_exec(
            db,
            setup.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null_mut()
        ),
        0
    );
    let sql = CString::new(sql).unwrap();
    let rc = sqlite3_exec(
        db,
        sql.as_ptr(),
        ptr::null(),
        ptr::null_mut(),
        ptr::null_mut(),
    );
    let got = if rc == 0 {
        (0, String::new())
    } else {
        let msg = CStr::from_ptr(sqlite3_errmsg(db))
            .to_string_lossy()
            .into_owned();
        (sqlite3_extended_errcode(db), msg)
    };
    sqlite3_close(db);
    got
}

#[test]
fn error_text_and_extended_codes_match_sqlite() {
    let mut bad = Vec::new();
    for &(sql, code, msg) in CORPUS {
        let (got_code, got_msg) = unsafe { run(sql) };
        if got_code != code || got_msg != msg {
            bad.push(format!(
                "{sql:?}\n  want {code} {msg:?}\n  got  {got_code} {got_msg:?}"
            ));
        }
    }
    assert!(bad.is_empty(), "{} differ:\n{}", bad.len(), bad.join("\n"));
}

unsafe fn open_mem() -> *mut sqlite3 {
    let mut db = ptr::null_mut();
    assert_eq!(sqlite3_open(c":memory:".as_ptr(), &mut db), 0);
    db
}

unsafe fn exec(db: *mut sqlite3, sql: &str) -> c_int {
    let sql = CString::new(sql).unwrap();
    sqlite3_exec(
        db,
        sql.as_ptr(),
        ptr::null(),
        ptr::null_mut(),
        ptr::null_mut(),
    )
}

#[test]
fn second_read_write_blob_handle_inside_a_transaction() {
    unsafe {
        let db = open_mem();
        assert_eq!(exec(db, "CREATE TABLE t(b BLOB); BEGIN"), 0);
        assert_eq!(
            exec(db, "INSERT INTO t VALUES (zeroblob(8)), (zeroblob(8))"),
            0
        );
        let (mut a, mut b) = (ptr::null_mut(), ptr::null_mut());
        let open = |row, out: *mut *mut c_void| {
            sqlite3_blob_open(
                db,
                c"main".as_ptr(),
                c"t".as_ptr(),
                c"b".as_ptr(),
                row,
                1,
                out,
            )
        };
        assert_eq!(open(1, &mut a), 0);
        assert_eq!(sqlite3_blob_write(a, b"xy".as_ptr().cast(), 2, 0), 0);
        assert_eq!(open(2, &mut b), 0);
        assert_eq!(sqlite3_blob_write(b, b"z".as_ptr().cast(), 1, 0), 0);
        assert_eq!(sqlite3_blob_close(b), 0);
        assert_eq!(sqlite3_blob_close(a), 0);
        assert_eq!(exec(db, "COMMIT"), 0);
        sqlite3_close(db);
    }
}

unsafe fn new_coll(db: *mut sqlite3, name: *const c_char) -> c_int {
    sqlite3_create_collation_v2(db, name, 1, ptr::null_mut(), Some(cmp_zero), ptr::null())
}

unsafe extern "C" fn cmp_zero(
    _: *mut c_void,
    _: c_int,
    _: *const c_void,
    _: c_int,
    _: *const c_void,
) -> c_int {
    0
}

#[test]
fn replacing_a_collation_while_a_statement_runs_is_busy() {
    unsafe {
        let db = open_mem();
        assert_eq!(
            exec(db, "CREATE TABLE t(x); INSERT INTO t VALUES (1),(2)"),
            0
        );
        let name = c"mycoll".as_ptr();
        assert_eq!(new_coll(db, name), 0);
        let mut st = ptr::null_mut();
        let sql = c"SELECT x FROM t ORDER BY x COLLATE mycoll";
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()),
            0
        );
        assert_eq!(sqlite3_step(st), 100);
        assert_eq!(new_coll(db, name), 5);
        assert_eq!(
            CStr::from_ptr(sqlite3_errmsg(db)).to_str().unwrap(),
            "unable to delete/modify collation sequence due to active statements"
        );
        // A new name is fine while statements run.
        let other = c"other".as_ptr();
        assert_eq!(new_coll(db, other), 0);
        sqlite3_finalize(st);
        assert_eq!(new_coll(db, name), 0);
        sqlite3_close(db);
    }
}

unsafe extern "C" fn udf_null(ctx: *mut c_void, _: c_int, _: *mut *mut c_void) {
    sqlite3_result_null(ctx);
}

#[test]
fn deterministic_udf_may_appear_in_a_partial_index() {
    unsafe {
        let db = open_mem();
        assert_eq!(exec(db, "CREATE TABLE t(x)"), 0);
        // SQLITE_UTF8 | SQLITE_DETERMINISTIC
        let rc = sqlite3_create_function_v2(
            db,
            c"det".as_ptr(),
            0,
            1 | 0x800,
            ptr::null_mut(),
            Some(udf_null),
            ptr::null(),
            ptr::null(),
            ptr::null(),
        );
        assert_eq!(rc, 0);
        assert_eq!(
            exec(db, "CREATE INDEX i ON t(x) WHERE det() IS NOT NULL"),
            0
        );
        sqlite3_close(db);
    }
}

static CALLS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

unsafe extern "C" fn udf_count(ctx: *mut c_void, _: c_int, _: *mut *mut c_void) {
    CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    sqlite3_result_null(ctx);
}

#[test]
fn finalizing_a_read_only_statement_does_not_run_it_to_the_end() {
    unsafe {
        let db = open_mem();
        assert_eq!(
            exec(db, "CREATE TABLE t(x); INSERT INTO t VALUES (1),(2),(3)"),
            0
        );
        let rc = sqlite3_create_function_v2(
            db,
            c"cnt".as_ptr(),
            1,
            1,
            ptr::null_mut(),
            Some(udf_count),
            ptr::null(),
            ptr::null(),
            ptr::null(),
        );
        assert_eq!(rc, 0);
        let mut st = ptr::null_mut();
        let sql = c"SELECT cnt(x) FROM t";
        assert_eq!(
            sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut st, ptr::null_mut()),
            0
        );
        assert_eq!(sqlite3_step(st), 100);
        assert_eq!(sqlite3_finalize(st), 0);
        assert_eq!(CALLS.load(std::sync::atomic::Ordering::SeqCst), 1);
        sqlite3_close(db);
    }
}

#[test]
fn vacuum_on_a_memory_database_succeeds() {
    unsafe {
        let db = open_mem();
        assert_eq!(exec(db, "CREATE TABLE t(x); VACUUM"), 0);
        sqlite3_close(db);
    }
}
