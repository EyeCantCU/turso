//! FTS3/FTS4 tests. The reference is SQLite (rusqlite's bundled build has
//! FTS3, FTS4 and the enhanced query syntax): tables written by either
//! engine must read the same, byte for byte, in the other.

use super::porter::porter_stem;
use crate::sync::Arc;
use crate::{Connection, Database, OpenOptions, PlatformIO, SqliteDialect, Value};

fn open_turso(path: &std::path::Path) -> (Arc<Database>, Arc<Connection>) {
    let io = Arc::new(PlatformIO::new().unwrap());
    let db = Database::open(
        io,
        path.to_str().unwrap(),
        OpenOptions::new(Arc::new(SqliteDialect)),
    )
    .unwrap();
    let conn = db.connect(crate::ConnectOpts::new()).unwrap();
    (db, conn)
}

fn fmt_turso(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Text(t) => format!("'{}'", t.as_str()),
        Value::Blob(b) => format!(
            "x'{}'",
            b.iter().map(|c| format!("{c:02x}")).collect::<String>()
        ),
        other => other.to_string(),
    }
}

fn fmt_sqlite(v: rusqlite::types::ValueRef) -> String {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => "NULL".into(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => Value::from_f64(f).to_string(),
        ValueRef::Text(t) => format!("'{}'", String::from_utf8_lossy(t)),
        ValueRef::Blob(b) => format!(
            "x'{}'",
            b.iter().map(|c| format!("{c:02x}")).collect::<String>()
        ),
    }
}

/// Runs one statement; rows as strings, or the error message.
fn run_turso(conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    let r = (|| -> crate::Result<Vec<String>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.run_collect_rows()?;
        Ok(rows
            .iter()
            .map(|r| r.iter().map(fmt_turso).collect::<Vec<_>>().join("|"))
            .collect())
    })();
    match r {
        Ok(v) => v,
        Err(e) => vec![format!("ERROR: {e}")],
    }
}

fn run_sqlite(conn: &rusqlite::Connection, sql: &str) -> Vec<String> {
    let r = (|| -> rusqlite::Result<Vec<String>> {
        let mut stmt = conn.prepare(sql)?;
        let n = stmt.column_count();
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(
                (0..n)
                    .map(|i| fmt_sqlite(row.get_ref(i).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|"),
            );
        }
        Ok(out)
    })();
    match r {
        Ok(v) => v,
        Err(e) => vec![format!("ERROR: {e}")],
    }
}

/// Text for a deterministic document; `seed` picks words from a vocabulary.
fn doc(seed: u64, n: usize) -> String {
    const VOCAB: [&str; 24] = [
        "alpha",
        "beta",
        "gamma",
        "delta",
        "epsilon",
        "zeta",
        "eta",
        "theta",
        "iota",
        "kappa",
        "lambda",
        "mu",
        "running",
        "runs",
        "connection",
        "connected",
        "one",
        "two",
        "three",
        "x",
        "ab",
        "abc",
        "abd",
        "Caf\u{e9}",
    ];
    let mut s = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (0..n)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            VOCAB[((s >> 33) % VOCAB.len() as u64) as usize]
        })
        .collect::<Vec<_>>()
        .join(" ")
}

const QUERIES: &[&str] = &[
    "SELECT docid, offsets(t), snippet(t), hex(matchinfo(t)) FROM t WHERE t MATCH 'alpha'",
    "SELECT docid FROM t WHERE t MATCH 'alpha beta'",
    "SELECT docid FROM t WHERE t MATCH 'alpha OR zeta' ORDER BY docid DESC",
    "SELECT docid, offsets(t) FROM t WHERE t MATCH '\"alpha beta\"'",
    "SELECT docid, snippet(t, '[', ']', '...', -1, 4) FROM t WHERE t MATCH 'run* NOT one'",
    "SELECT docid, offsets(t) FROM t WHERE t MATCH 'gamma NEAR/2 delta'",
    "SELECT docid, hex(matchinfo(t, 'pcxyb')) FROM t WHERE t MATCH '(alpha OR beta) gamma'",
    "SELECT docid FROM t WHERE t MATCH 'ab*'",
    "SELECT count(*) FROM t WHERE t MATCH 'connection'",
    "SELECT docid, a FROM t WHERE docid = 7",
    "SELECT docid FROM t WHERE t MATCH 'nomatch'",
    "SELECT docid FROM t WHERE t MATCH 'alpha AND'",
];

fn populate(sql: &mut dyn FnMut(&str), create: &str) {
    sql(create);
    sql("BEGIN");
    for i in 1..=300u64 {
        sql(&format!(
            "INSERT INTO t(docid, a, b) VALUES({i}, '{}', '{}')",
            doc(i, (i % 17) as usize + 1),
            doc(i + 1000, (i % 5) as usize)
        ));
    }
    sql("COMMIT");
    for i in (5..300u64).step_by(7) {
        sql(&format!(
            "UPDATE t SET a = '{}' WHERE docid = {i}",
            doc(i + 7, 3)
        ));
    }
    for i in (3..300u64).step_by(11) {
        sql(&format!("DELETE FROM t WHERE docid = {i}"));
    }
    for i in 301..=340u64 {
        sql(&format!(
            "INSERT INTO t(a, b) VALUES('{}', '{}')",
            doc(i, 9),
            doc(i * 3, 2)
        ));
    }
}

const SHADOW: &[&str] = &[
    "SELECT * FROM t_segdir",
    "SELECT * FROM t_segments",
    "SELECT * FROM t_content",
];

fn create_variants() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        ("CREATE VIRTUAL TABLE t USING fts3(a, b)", &[]),
        (
            "CREATE VIRTUAL TABLE t USING fts4(a, b, tokenize=porter)",
            &["SELECT * FROM t_docsize", "SELECT * FROM t_stat"],
        ),
        (
            "CREATE VIRTUAL TABLE t USING fts4(a, b, prefix='1,2', order=DESC)",
            &["SELECT * FROM t_docsize", "SELECT * FROM t_stat"],
        ),
        (
            "CREATE VIRTUAL TABLE t USING fts4(a, b, tokenize=unicode61, notindexed=b)",
            &["SELECT * FROM t_stat"],
        ),
    ]
}

/// The same writes in both engines give byte-identical shadow tables, and
/// the same query results.
#[test]
fn writes_match_sqlite_byte_for_byte() {
    for (create, extra) in create_variants() {
        let dir = tempfile::tempdir().unwrap();
        let (_db, conn) = open_turso(&dir.path().join("t.db"));
        conn.execute("PRAGMA page_size=1024").ok();
        let sqlite = rusqlite::Connection::open(dir.path().join("s.db")).unwrap();
        sqlite.execute_batch("PRAGMA page_size=1024").unwrap();
        populate(&mut |s| conn.execute(s).unwrap(), create);
        populate(&mut |s| sqlite.execute_batch(s).unwrap(), create);
        for q in SHADOW.iter().chain(extra.iter()).chain(QUERIES.iter()) {
            assert_eq!(run_turso(&conn, q), run_sqlite(&sqlite, q), "{create}: {q}");
        }
        for sp in ["merge=4,2", "optimize", "rebuild", "integrity-check"] {
            let q = format!("INSERT INTO t(t) VALUES('{sp}')");
            conn.execute(&q).unwrap();
            sqlite.execute_batch(&q).unwrap();
            for q in SHADOW.iter() {
                assert_eq!(
                    run_turso(&conn, q),
                    run_sqlite(&sqlite, q),
                    "{create}: {sp}: {q}"
                );
            }
        }
    }
}

/// Tables written by SQLite read in turso, and the other way round, with
/// the integrity check passing in both engines.
#[test]
fn tables_cross_engines() {
    for (create, _) in create_variants() {
        // SQLite writes, turso reads and writes, SQLite checks.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.db");
        {
            let sqlite = rusqlite::Connection::open(&path).unwrap();
            populate(&mut |s| sqlite.execute_batch(s).unwrap(), create);
        }
        let expected: Vec<Vec<String>> = {
            let sqlite = rusqlite::Connection::open(&path).unwrap();
            QUERIES.iter().map(|q| run_sqlite(&sqlite, q)).collect()
        };
        {
            let (_db, conn) = open_turso(&path);
            for (q, e) in QUERIES.iter().zip(&expected) {
                assert_eq!(&run_turso(&conn, q), e, "{create}: {q}");
            }
            conn.execute("INSERT INTO t(t) VALUES('integrity-check')")
                .unwrap();
            conn.execute("INSERT INTO t(a, b) VALUES('alpha new', 'zeta')")
                .unwrap();
            conn.execute("DELETE FROM t WHERE docid = 7").unwrap();
            conn.execute("INSERT INTO t(t) VALUES('integrity-check')")
                .unwrap();
        }
        let sqlite = rusqlite::Connection::open(&path).unwrap();
        sqlite
            .execute_batch("INSERT INTO t(t) VALUES('integrity-check')")
            .unwrap();
        assert_eq!(run_sqlite(&sqlite, "PRAGMA integrity_check"), vec!["'ok'"]);
        let after: Vec<Vec<String>> = QUERIES.iter().map(|q| run_sqlite(&sqlite, q)).collect();
        drop(sqlite);
        let (_db, conn) = open_turso(&path);
        for (q, e) in QUERIES.iter().zip(&after) {
            assert_eq!(&run_turso(&conn, q), e, "{create}: {q}");
        }
    }
}

/// Pending terms are visible inside a transaction and dropped on rollback.
#[test]
fn pending_terms_follow_the_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let (_db, conn) = open_turso(&dir.path().join("p.db"));
    conn.execute("CREATE VIRTUAL TABLE t USING fts4(a)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES('kept words')").unwrap();
    conn.execute("BEGIN").unwrap();
    conn.execute("INSERT INTO t VALUES('pending words')")
        .unwrap();
    assert_eq!(
        run_turso(&conn, "SELECT a FROM t WHERE t MATCH 'words'"),
        vec!["'kept words'", "'pending words'"]
    );
    assert!(run_turso(&conn, "SELECT count(*) FROM t_segdir") == vec!["1"]);
    conn.execute("ROLLBACK").unwrap();
    assert_eq!(
        run_turso(&conn, "SELECT a FROM t WHERE t MATCH 'words'"),
        vec!["'kept words'"]
    );
    conn.execute("INSERT INTO t(t) VALUES('integrity-check')")
        .unwrap();
}

#[test]
fn porter_stems_like_sqlite() {
    for (word, stem) in [
        ("running", "run"),
        ("connection", "connect"),
        ("relational", "relat"),
        ("caresses", "caress"),
        ("ponies", "poni"),
        ("agreed", "agre"),
        ("happy", "happi"),
        ("generalizations", "gener"),
        ("abcdefghijklmnopqrstuvwxyz", "abcdefghijqrstuvwxyz"),
        ("x1234567", "x12567"),
    ] {
        let mut out = Vec::new();
        porter_stem(word.as_bytes(), &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), stem, "{word}");
    }
}

#[test]
fn malformed_match_expressions() {
    let dir = tempfile::tempdir().unwrap();
    let (_db, conn) = open_turso(&dir.path().join("m.db"));
    let sqlite = rusqlite::Connection::open(dir.path().join("s.db")).unwrap();
    let setup = "CREATE VIRTUAL TABLE t USING fts3(a); INSERT INTO t VALUES('a b c')";
    sqlite.execute_batch(setup).unwrap();
    for s in setup.split("; ") {
        conn.execute(s).unwrap();
    }
    for q in [
        "SELECT * FROM t WHERE t MATCH 'a AND'",
        "SELECT * FROM t WHERE t MATCH '(a'",
        "SELECT * FROM t WHERE t MATCH '\"a'",
        "SELECT * FROM t WHERE t MATCH 'OR a'",
        "SELECT * FROM t WHERE t MATCH 'a NEAR (b OR c)'",
        "SELECT * FROM t WHERE t MATCH 'a OR b OR c OR a OR b OR c OR a OR b OR c OR a OR b OR c OR a OR b OR c OR a OR b OR c OR a OR b OR c OR a OR b OR c OR a OR b OR c OR a OR b OR c OR a OR b OR c OR a OR b OR c OR a OR b OR c'",
    ] {
        assert_eq!(run_turso(&conn, q), run_sqlite(&sqlite, q), "{q}");
    }
}

/// INSERT ... SELECT into a virtual table, and an external content table
/// whose columns are taken from the content table on reload.
#[test]
fn insert_select_and_content_columns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.db");
    let setup = [
        "CREATE TABLE src(a, b)",
        "INSERT INTO src VALUES('red apple', 'fruit'), ('green leaf', 'plant')",
        "CREATE VIRTUAL TABLE t USING fts4(content=src)",
        "INSERT INTO t(docid, a, b) SELECT rowid, a, b FROM src",
        "CREATE VIRTUAL TABLE cl USING fts4(x, content='')",
        "INSERT INTO cl(docid, x) VALUES(3, 'alpha beta')",
    ];
    let queries = [
        "SELECT docid, a, b FROM t WHERE t MATCH 'apple OR plant'",
        "SELECT * FROM t_segdir",
        "SELECT docid FROM cl WHERE cl MATCH 'beta'",
        "SELECT x FROM cl WHERE cl MATCH 'beta'",
        "DELETE FROM cl WHERE docid = 3",
    ];
    {
        let (_db, conn) = open_turso(&path);
        for s in setup {
            conn.execute(s).unwrap();
        }
    }
    let sqlite = rusqlite::Connection::open(dir.path().join("s.db")).unwrap();
    sqlite.execute_batch(&setup.join(";")).unwrap();
    // Reopen: the content table's columns come from the loaded schema.
    let (_db, conn) = open_turso(&path);
    for q in queries {
        assert_eq!(run_turso(&conn, q), run_sqlite(&sqlite, q), "{q}");
    }
}
