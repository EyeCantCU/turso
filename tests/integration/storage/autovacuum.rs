use std::sync::Arc;
use tempfile::TempDir;
use turso_core::SqliteDialect;
use turso_core::{Database, DatabaseOpts, OpenFlags, PlatformIO};

#[test]
fn test_autovacuum_readonly_behavior() {
    // (autovacuum_mode, enable_autovacuum_flag, expected_readonly)
    // Auto-vacuum is always supported now, so the flag no longer matters.
    // TODO: Add encrypted case ("NONE", false, false) after fixing https://github.com/tursodatabase/turso/issues/4519
    let test_cases = [
        ("NONE", false, false),
        ("NONE", true, false),
        ("FULL", false, false),
        ("FULL", true, false),
        ("INCREMENTAL", false, false),
        ("INCREMENTAL", true, false),
    ];

    for (autovacuum_mode, enable_autovacuum_flag, expected_readonly) in test_cases {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.pragma_update(None, "auto_vacuum", autovacuum_mode)
                .unwrap();
        }

        let io = Arc::new(PlatformIO::new().unwrap()) as Arc<dyn turso_core::IO>;
        let opts = DatabaseOpts::new().with_autovacuum(enable_autovacuum_flag);
        let db = Database::open_file_with_flags(
            io,
            db_path.to_str().unwrap(),
            OpenFlags::default(),
            opts,
            None,
            Arc::new(SqliteDialect),
        )
        .unwrap();

        assert_eq!(
            db.is_readonly(),
            expected_readonly,
            "autovacuum={autovacuum_mode}, flag={enable_autovacuum_flag}: expected readonly={expected_readonly}"
        );
    }
}

mod pointer_maps {
    use crate::common::{
        limbo_exec_rows, rusqlite_integrity_check, sqlite_exec_rows, TempDatabase,
    };
    use rusqlite::types::Value;
    use std::sync::Arc;

    fn int(conn: &Arc<turso_core::Connection>, sql: &str) -> i64 {
        match limbo_exec_rows(conn, sql).first().and_then(|r| r.first()) {
            Some(Value::Integer(n)) => *n,
            other => panic!("{sql}: expected an integer, got {other:?}"),
        }
    }

    fn turso_ok(conn: &Arc<turso_core::Connection>) {
        let rows = limbo_exec_rows(conn, "PRAGMA integrity_check");
        assert_eq!(
            rows,
            vec![vec![Value::Text("ok".into())]],
            "turso integrity_check"
        );
    }

    /// Checkpoints so SQLite sees everything, then lets SQLite check the file.
    fn sqlite_ok(db: &TempDatabase, conn: &Arc<turso_core::Connection>) {
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        rusqlite_integrity_check(&db.path).unwrap();
    }

    /// Inserts rows (a third of them with overflow chains) and indexes them.
    fn fill(conn: &Arc<turso_core::Connection>, table: &str, rows: usize) {
        conn.execute(format!(
            "CREATE TABLE {table}(a INTEGER PRIMARY KEY, b TEXT, c BLOB)"
        ))
        .unwrap();
        conn.execute(format!("CREATE INDEX {table}_b ON {table}(b)"))
            .unwrap();
        conn.execute("BEGIN").unwrap();
        for i in 0..rows {
            let big = if i % 3 == 0 { 3000 + i % 5000 } else { 40 };
            conn.execute(format!(
                "INSERT INTO {table}(b, c) VALUES ('k{i:06}' || hex(randomblob(20)), randomblob({big}))"
            ))
            .unwrap();
        }
        conn.execute("COMMIT").unwrap();
    }

    fn open(mode: &str) -> (TempDatabase, Arc<turso_core::Connection>) {
        let db = TempDatabase::new_empty();
        let conn = db.connect_limbo();
        conn.execute(format!("PRAGMA auto_vacuum = {mode}"))
            .unwrap();
        (db, conn)
    }

    #[test]
    fn full_mode_returns_free_pages_on_commit() {
        let (db, conn) = open("FULL");
        assert_eq!(int(&conn, "PRAGMA auto_vacuum"), 1);
        fill(&conn, "t1", 600);
        fill(&conn, "t2", 300);
        let before = int(&conn, "PRAGMA page_count");
        conn.execute("DELETE FROM t1 WHERE a % 2 = 0").unwrap();
        assert_eq!(int(&conn, "PRAGMA freelist_count"), 0);
        conn.execute("DROP TABLE t2").unwrap();
        assert_eq!(int(&conn, "PRAGMA freelist_count"), 0);
        assert!(int(&conn, "PRAGMA page_count") < before);
        turso_ok(&conn);
        sqlite_ok(&db, &conn);
    }

    #[test]
    fn incremental_mode_shrinks_on_demand() {
        let (db, conn) = open("INCREMENTAL");
        assert_eq!(int(&conn, "PRAGMA auto_vacuum"), 2);
        fill(&conn, "t1", 800);
        conn.execute("DELETE FROM t1 WHERE a % 3 <> 0").unwrap();
        let free = int(&conn, "PRAGMA freelist_count");
        let pages = int(&conn, "PRAGMA page_count");
        assert!(free > 20, "expected free pages, got {free}");
        conn.execute("PRAGMA incremental_vacuum(10)").unwrap();
        assert_eq!(int(&conn, "PRAGMA freelist_count"), free - 10);
        assert!(int(&conn, "PRAGMA page_count") <= pages - 10);
        turso_ok(&conn);
        sqlite_ok(&db, &conn);
        conn.execute("PRAGMA incremental_vacuum").unwrap();
        assert_eq!(int(&conn, "PRAGMA freelist_count"), 0);
        turso_ok(&conn);
        sqlite_ok(&db, &conn);
    }

    #[test]
    fn drop_moves_the_last_root_page() {
        let (db, conn) = open("FULL");
        for t in ["a", "b", "c", "d"] {
            fill(&conn, t, 50);
        }
        conn.execute("DROP TABLE b").unwrap();
        conn.execute("DROP INDEX c_b").unwrap();
        // Every root page sits at the front of the file.
        let max_root = int(&conn, "SELECT max(rootpage) FROM sqlite_schema");
        let roots = int(
            &conn,
            "SELECT count(*) FROM sqlite_schema WHERE rootpage > 0",
        );
        assert!(
            max_root <= roots + 2,
            "max root {max_root} for {roots} roots"
        );
        assert_eq!(int(&conn, "SELECT count(*) FROM d"), 50);
        assert_eq!(int(&conn, "SELECT count(*) FROM a WHERE b > ''"), 50);
        turso_ok(&conn);
        sqlite_ok(&db, &conn);
        // A new connection reads the moved roots from sqlite_schema.
        let conn2 = db.connect_limbo();
        assert_eq!(int(&conn2, "SELECT count(*) FROM d WHERE b > ''"), 50);
    }

    #[test]
    fn savepoint_rollback_keeps_pointer_maps() {
        let (db, conn) = open("INCREMENTAL");
        fill(&conn, "t1", 300);
        conn.execute("DELETE FROM t1 WHERE a % 2 = 0").unwrap();
        conn.execute("BEGIN").unwrap();
        conn.execute("SAVEPOINT s").unwrap();
        conn.execute("PRAGMA incremental_vacuum(5)").unwrap();
        conn.execute("CREATE TABLE t2(x)").unwrap();
        conn.execute("INSERT INTO t2 SELECT c FROM t1").unwrap();
        conn.execute("ROLLBACK TO s").unwrap();
        conn.execute("INSERT INTO t1(b, c) SELECT b || 'x', c FROM t1 WHERE a < 40")
            .unwrap();
        conn.execute("COMMIT").unwrap();
        turso_ok(&conn);
        sqlite_ok(&db, &conn);
    }

    #[test]
    fn sqlite_written_database_is_maintained() {
        for mode in ["FULL", "INCREMENTAL"] {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("sqlite.db");
            {
                let c = rusqlite::Connection::open(&path).unwrap();
                c.execute_batch(&format!(
                    "PRAGMA auto_vacuum={mode};
                     CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT, c BLOB);
                     CREATE INDEX tb ON t(b);
                     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<500)
                     INSERT INTO t(b, c) SELECT hex(randomblob(16)), randomblob(CASE WHEN i%4=0 THEN 5000 ELSE 50 END) FROM n;
                     CREATE TABLE u(x);
                     INSERT INTO u SELECT c FROM t WHERE a % 5 = 0;
                     DELETE FROM t WHERE a % 3 = 0;"
                ))
                .unwrap();
            }
            let db = TempDatabase::new_with_existent(&path);
            let conn = db.connect_limbo();
            conn.execute("INSERT INTO t(b, c) SELECT b || 'y', c FROM t WHERE a < 100")
                .unwrap();
            conn.execute("DROP TABLE u").unwrap();
            conn.execute("CREATE TABLE v(y)").unwrap();
            conn.execute("INSERT INTO v SELECT c FROM t WHERE a % 7 = 0")
                .unwrap();
            conn.execute("DELETE FROM t WHERE a % 2 = 0").unwrap();
            conn.execute("PRAGMA incremental_vacuum").unwrap();
            assert_eq!(int(&conn, "PRAGMA freelist_count"), 0, "{mode}");
            turso_ok(&conn);
            sqlite_ok(&db, &conn);
            let c = rusqlite::Connection::open(&path).unwrap();
            let rows = sqlite_exec_rows(&c, "SELECT count(*) FROM v");
            assert!(matches!(rows[0][0], Value::Integer(n) if n > 0));
        }
    }

    #[test]
    fn auto_vacuum_pragma_follows_sqlite() {
        // NONE <-> FULL needs VACUUM once the database has a page 1.
        let db = TempDatabase::new_empty();
        let conn = db.connect_limbo();
        conn.execute("CREATE TABLE t(x)").unwrap();
        conn.execute("PRAGMA auto_vacuum = FULL").unwrap();
        assert_eq!(int(&conn, "PRAGMA auto_vacuum"), 0);
        conn.execute("VACUUM").unwrap();
        assert_eq!(int(&conn, "PRAGMA auto_vacuum"), 1);
        // FULL <-> INCREMENTAL is a header change.
        conn.execute("PRAGMA auto_vacuum = INCREMENTAL").unwrap();
        assert_eq!(int(&conn, "PRAGMA auto_vacuum"), 2);
        conn.execute("PRAGMA auto_vacuum = 0").unwrap();
        assert_eq!(int(&conn, "PRAGMA auto_vacuum"), 2);
        conn.execute("VACUUM").unwrap();
        assert_eq!(int(&conn, "PRAGMA auto_vacuum"), 0);
        turso_ok(&conn);
        sqlite_ok(&db, &conn);
    }

    #[test]
    fn vacuum_to_incremental_keeps_data() {
        let db = TempDatabase::new_empty();
        let conn = db.connect_limbo();
        fill(&conn, "t1", 400);
        conn.execute("PRAGMA auto_vacuum = INCREMENTAL").unwrap();
        conn.execute("VACUUM").unwrap();
        assert_eq!(int(&conn, "PRAGMA auto_vacuum"), 2);
        assert_eq!(int(&conn, "SELECT count(*) FROM t1"), 400);
        conn.execute("DELETE FROM t1 WHERE a > 100").unwrap();
        conn.execute("PRAGMA incremental_vacuum").unwrap();
        assert_eq!(int(&conn, "PRAGMA freelist_count"), 0);
        turso_ok(&conn);
        sqlite_ok(&db, &conn);
    }
}
