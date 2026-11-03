//! FTS5 tests. Expected outputs come from SQLite, by running the same SQL
//! through rusqlite's bundled SQLite.

use crate::dialect::sqlite::SqliteDialect;
use crate::io::PlatformIO;
use crate::sync::Arc;
use crate::{ConnectOpts, Connection, Database, Value, IO};

fn open(path: &str) -> Arc<Connection> {
    let io: Arc<dyn IO> = Arc::new(PlatformIO::new().unwrap());
    let db = Database::open_file(io, path, Arc::new(SqliteDialect)).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

fn fmt(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Blob(b) => format!(
            "x'{}'",
            b.iter().map(|c| format!("{c:02x}")).collect::<String>()
        ),
        other => other.to_string(),
    }
}

/// Runs `sql` on turso; columns joined by '|' and rows by '\n', or "ERR: msg".
fn tq(conn: &Arc<Connection>, sql: &str) -> String {
    let mut out = Vec::new();
    let r = (|| -> crate::Result<()> {
        let mut stmt = conn.prepare(sql)?;
        for row in stmt.run_collect_rows()? {
            out.push(row.iter().map(fmt).collect::<Vec<_>>().join("|"));
        }
        Ok(())
    })();
    match r {
        Ok(()) => out.join("\n"),
        Err(e) => format!("ERR: {e}"),
    }
}

fn texec(conn: &Arc<Connection>, sql: &str) {
    if let Err(e) = conn.execute(sql) {
        panic!("turso: {sql}: {e}");
    }
}

fn rfmt(v: rusqlite::types::ValueRef) -> String {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => "NULL".into(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => Value::from_f64(f).to_string(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(b) => format!(
            "x'{}'",
            b.iter().map(|c| format!("{c:02x}")).collect::<String>()
        ),
    }
}

/// Runs `sql` on SQLite with the same output format as `tq`.
fn sq(conn: &rusqlite::Connection, sql: &str) -> String {
    let r = (|| -> rusqlite::Result<Vec<String>> {
        let mut stmt = conn.prepare(sql)?;
        let n = stmt.column_count();
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(
                (0..n)
                    .map(|i| rfmt(row.get_ref(i).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|"),
            );
        }
        Ok(out)
    })();
    match r {
        Ok(v) => v.join("\n"),
        Err(e) => format!("ERR: {e}"),
    }
}

fn sexec(conn: &rusqlite::Connection, sql: &str) {
    conn.execute_batch(sql)
        .unwrap_or_else(|e| panic!("sqlite: {sql}: {e}"));
}

const DOCS: &[(&str, &str)] = &[
    ("the quick brown fox", "jumps over the lazy dog"),
    (
        "a stitch in time saves nine",
        "the early bird catches the worm",
    ),
    ("all that glitters is not gold", "fortune favours the brave"),
    ("brown bread and butter", "quick quick slow"),
    ("Héllo wörld", "naïve café résumé"),
];

fn insert_docs(sql: &mut dyn FnMut(&str)) {
    for (i, (a, b)) in DOCS.iter().enumerate() {
        sql(&format!(
            "INSERT INTO t(rowid, a, b) VALUES({}, '{}', '{}')",
            i + 1,
            a,
            b
        ));
    }
}

const QUERIES: &[&str] = &[
    "SELECT rowid FROM t WHERE t MATCH 'quick'",
    "SELECT rowid FROM t WHERE t MATCH 'brown OR gold' ORDER BY rowid",
    "SELECT rowid FROM t WHERE t MATCH 'the NOT quick'",
    "SELECT rowid FROM t WHERE t MATCH '\"quick brown\"'",
    "SELECT rowid FROM t WHERE t MATCH 'qu*'",
    "SELECT rowid FROM t WHERE t MATCH 'b:quick'",
    "SELECT rowid FROM t WHERE t MATCH 'NEAR(quick dog, 5)'",
    "SELECT rowid FROM t WHERE t MATCH 'hello'",
    "SELECT rowid FROM t WHERE t MATCH 'cafe'",
    "SELECT rowid, a FROM t WHERE t MATCH '^brown'",
    "SELECT rowid, b FROM t WHERE b MATCH 'the'",
    "SELECT rowid, bm25(t) FROM t WHERE t MATCH 'the' ORDER BY rowid",
    "SELECT rowid, bm25(t, 2.0, 1.0) FROM t WHERE t MATCH 'quick OR brown' ORDER BY rowid",
    "SELECT rowid, highlight(t, 0, '[', ']') FROM t WHERE t MATCH 'quick OR brown' ORDER BY rowid",
    "SELECT rowid, snippet(t, 1, '<', '>', '...', 3) FROM t WHERE t MATCH 'the' ORDER BY rowid",
    "SELECT rowid, rank FROM t WHERE t MATCH 'the' ORDER BY rank",
    "SELECT * FROM t WHERE rowid = 3",
    "SELECT count(*) FROM t",
    "SELECT rowid FROM t WHERE t MATCH 'qu* AND (bro* OR slow)'",
    "SELECT rowid FROM t('fox')",
    "SELECT rowid FROM t WHERE t = 'gold'",
];

fn temp_path(dir: &tempfile::TempDir, name: &str) -> String {
    dir.path().join(name).to_str().unwrap().to_string()
}

#[test]
fn fts5_turso_create_insert_query() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(&temp_path(&dir, "a.db"));
    texec(&conn, "CREATE VIRTUAL TABLE t USING fts5(a, b)");
    insert_docs(&mut |s| texec(&conn, s));
    let sconn = rusqlite::Connection::open_in_memory().unwrap();
    sexec(&sconn, "CREATE VIRTUAL TABLE t USING fts5(a, b)");
    insert_docs(&mut |s| sexec(&sconn, s));
    for q in QUERIES {
        assert_eq!(tq(&conn, q), sq(&sconn, q), "{q}");
    }
    assert_eq!(tq(&conn, "INSERT INTO t(t) VALUES('integrity-check')"), "");
}

#[test]
fn fts5_reads_sqlite_files_and_sqlite_reads_ours() {
    let dir = tempfile::tempdir().unwrap();
    // SQLite -> turso
    let p1 = temp_path(&dir, "s.db");
    {
        let s = rusqlite::Connection::open(&p1).unwrap();
        sexec(&s, "CREATE VIRTUAL TABLE t USING fts5(a, b)");
        insert_docs(&mut |q| sexec(&s, q));
    }
    let sref = rusqlite::Connection::open(&p1).unwrap();
    let conn = open(&p1);
    for q in QUERIES {
        assert_eq!(tq(&conn, q), sq(&sref, q), "{q}");
    }
    drop(conn);
    // turso -> SQLite
    let p2 = temp_path(&dir, "t.db");
    {
        let conn = open(&p2);
        texec(&conn, "CREATE VIRTUAL TABLE t USING fts5(a, b)");
        insert_docs(&mut |s| texec(&conn, s));
    }
    let s = rusqlite::Connection::open(&p2).unwrap();
    for q in QUERIES {
        assert_eq!(sq(&s, q), sq(&sref, q), "{q}");
    }
    assert_eq!(sq(&s, "INSERT INTO t(t) VALUES('integrity-check')"), "");
}
