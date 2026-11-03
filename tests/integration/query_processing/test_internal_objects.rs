//! The engine's `__turso_internal_*` objects stay out of what users see, and
//! AUTOINCREMENT keeps its counter in `sqlite_sequence` like SQLite.

use crate::common::{ExecRows, TempDatabase};
use std::sync::Arc;
use turso_core::Connection;

const BACKING: &str = "__turso_internal_seq___turso_internal_autoincrement_t";

fn names(conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    let rows: Vec<(String,)> = conn.exec_rows(sql);
    rows.into_iter().map(|(name,)| name).collect()
}

fn visible_objects(conn: &Arc<Connection>) -> Vec<String> {
    names(conn, "SELECT name FROM sqlite_schema ORDER BY name")
}

fn has_backing_table(conn: &Arc<Connection>) -> bool {
    // A statement that names the table sees it, whatever the listings hide.
    conn.prepare(format!("SELECT 1 FROM {BACKING}")).is_ok()
}

fn next_id(conn: &Arc<Connection>) -> i64 {
    conn.execute("INSERT INTO t(v) VALUES ('n')").unwrap();
    let rows: Vec<(i64,)> = conn.exec_rows("SELECT max(id) FROM t");
    rows[0].0
}

/// WAL mode writes nothing but the table and `sqlite_sequence`, and real SQLite
/// continues the sequence from the file.
#[turso_macros::test]
fn test_autoincrement_wal_file_is_plain_sqlite(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let path = tmp_db.path.clone();
    {
        let conn = tmp_db.connect_limbo();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, v)")?;
        conn.execute("INSERT INTO t(v) VALUES ('a'), ('b'), ('c')")?;
        conn.execute("DELETE FROM t WHERE id = 3")?;
        assert!(!has_backing_table(&conn));
        assert_eq!(visible_objects(&conn), ["sqlite_sequence", "t"]);
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")?;
    }
    drop(tmp_db);

    let sqlite = rusqlite::Connection::open(&path)?;
    let objects: Vec<String> = sqlite
        .prepare("SELECT name FROM sqlite_master ORDER BY name")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    assert_eq!(objects, ["sqlite_sequence", "t"]);
    sqlite.execute("INSERT INTO t(v) VALUES ('d')", [])?;
    let id: i64 = sqlite.query_row("SELECT max(id) FROM t", [], |row| row.get(0))?;
    assert_eq!(id, 4);
    Ok(())
}

/// Replaying the schema and rows of a database, as a dump does, gives the same
/// objects and the same next id.
#[turso_macros::test]
fn test_autoincrement_dump_replay(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, v)")?;
    conn.execute("CREATE INDEX t_v ON t(v)")?;
    conn.execute("INSERT INTO t(v) VALUES ('a'), ('b'), ('c')")?;
    conn.execute("DELETE FROM t WHERE id = 3")?;

    let replica = TempDatabase::new_empty();
    let target = replica.connect_limbo();
    for sql in names(
        &conn,
        "SELECT sql FROM sqlite_schema WHERE sql NOT NULL AND name <> 'sqlite_sequence' ORDER BY rowid",
    ) {
        target.execute(&sql)?;
    }
    let rows: Vec<(i64, String)> = conn.exec_rows("SELECT id, v FROM t ORDER BY id");
    for (id, v) in rows {
        target.execute(format!("INSERT INTO t VALUES({id}, '{v}')"))?;
    }
    let seq: Vec<(String, i64)> = conn.exec_rows("SELECT name, seq FROM sqlite_sequence");
    target.execute("DELETE FROM sqlite_sequence")?;
    for (name, seq) in seq {
        target.execute(format!(
            "INSERT INTO sqlite_sequence VALUES('{name}', {seq})"
        ))?;
    }

    assert_eq!(visible_objects(&target), visible_objects(&conn));
    assert_eq!(next_id(&target), next_id(&conn));
    assert_eq!(next_id(&conn), 5);
    Ok(())
}

/// A table made in WAL mode gets its MVCC counter when the database switches,
/// and the counter continues from `sqlite_sequence`.
#[turso_macros::test]
fn test_autoincrement_wal_to_mvcc_switch(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let path = tmp_db.path.clone();
    let io = tmp_db.io.clone();
    {
        let conn = tmp_db.connect_limbo();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, v)")?;
        conn.execute("INSERT INTO t(v) VALUES ('a'), ('b'), ('c')")?;
        conn.execute("DELETE FROM t WHERE id = 3")?;
        assert!(!has_backing_table(&conn));

        conn.execute("PRAGMA journal_mode = 'mvcc'")?;
        assert!(has_backing_table(&conn));
        assert_eq!(visible_objects(&conn), ["sqlite_sequence", "t"]);
        assert_eq!(next_id(&conn), 4);
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")?;
    }
    drop(tmp_db);

    let db = turso_core::Database::open_file(
        io,
        path.to_str().unwrap(),
        Arc::new(turso_core::SqliteDialect),
    )?;
    let conn = db.connect(turso_core::ConnectOpts::new())?;
    assert_eq!(visible_objects(&conn), ["sqlite_sequence", "t"]);
    assert_eq!(next_id(&conn), 5);
    Ok(())
}

/// A database that holds the counter table, as one written by an older
/// version does, works in WAL mode and shows no internal object.
#[turso_macros::test]
fn test_autoincrement_existing_backing_table_is_hidden(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("PRAGMA journal_mode = 'mvcc'")?;
    conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, v)")?;
    conn.execute("INSERT INTO t(v) VALUES ('a'), ('b')")?;
    conn.execute("PRAGMA journal_mode = 'wal'")?;
    assert!(has_backing_table(&conn));

    assert_eq!(visible_objects(&conn), ["sqlite_sequence", "t"]);
    let listed = names(&conn, "SELECT name FROM pragma_table_list ORDER BY name");
    assert!(listed
        .iter()
        .all(|name| !name.starts_with("__turso_internal_")));
    assert_eq!(next_id(&conn), 3);

    // VACUUM drops the counter table, which WAL mode never reads.
    conn.execute("VACUUM")?;
    assert!(!has_backing_table(&conn));
    assert_eq!(next_id(&conn), 4);
    conn.execute("ALTER TABLE t RENAME TO u")?;
    conn.execute("DROP TABLE u")?;
    conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, v)")?;
    assert_eq!(next_id(&conn), 1);
    Ok(())
}

/// The listings hide the internal objects of an MVCC database too, while the
/// engine still finds them.
#[turso_macros::test]
fn test_autoincrement_mvcc_objects_are_hidden(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("PRAGMA journal_mode = 'mvcc'")?;
    conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, v)")?;
    assert!(has_backing_table(&conn));
    assert_eq!(visible_objects(&conn), ["sqlite_sequence", "t"]);
    let count: Vec<(i64,)> = conn.exec_rows("SELECT count(*) FROM sqlite_master");
    assert_eq!(count, [(2,)]);
    assert_eq!(next_id(&conn), 1);
    assert_eq!(next_id(&conn), 2);
    Ok(())
}
