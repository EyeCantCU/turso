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

/// Deterministic xorshift generator for the differential tests.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
}

const WORDS: &[&str] = &[
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
    "nu",
    "xi",
    "omicron",
    "pi",
    "rho",
    "sigma",
    "tau",
    "upsilon",
    "phi",
    "chi",
    "psi",
    "omega",
    "alphabet",
    "betamax",
    "gammaray",
    "deltoid",
    "running",
    "runs",
    "runner",
    "café",
    "Ünïcode",
    "naïve",
    "über",
    "connection",
    "connected",
    "connects",
];

fn random_doc(rng: &mut Rng, max: usize) -> String {
    let n = rng.below(max) + 1;
    let mut words = Vec::with_capacity(n);
    for _ in 0..n {
        let w = WORDS[rng.below(WORDS.len())];
        words.push(if rng.chance(10) {
            w.to_uppercase()
        } else {
            w.to_string()
        });
    }
    let mut s = words.join(" ");
    if rng.chance(20) {
        s.push_str(". Then ");
        s.push_str(WORDS[rng.below(WORDS.len())]);
    }
    s
}

fn random_term(rng: &mut Rng, prefix_ok: bool) -> String {
    let w = WORDS[rng.below(WORDS.len())].to_lowercase();
    if prefix_ok && rng.chance(20) {
        let n = w.chars().count().min(rng.below(3) + 1);
        let p: String = w.chars().take(n).collect();
        return format!("{p}*");
    }
    if rng.chance(10) {
        return format!("\"{w}\"");
    }
    w
}

fn random_query(rng: &mut Rng, detail: &str, cols: &[&str], depth: usize) -> String {
    let full = detail == "full";
    let leaf = |rng: &mut Rng| -> String {
        let mut t = match rng.below(if full { 4 } else { 1 }) {
            0 => random_term(rng, true),
            1 => format!(
                "\"{} {}\"",
                random_term(rng, false).trim_matches('"'),
                random_term(rng, false).trim_matches('"')
            ),
            2 => format!(
                "NEAR({} {}, {})",
                random_term(rng, false),
                random_term(rng, false),
                rng.below(6)
            ),
            _ => format!("^{}", random_term(rng, false)),
        };
        if detail != "none" && rng.chance(15) {
            t = format!("{}:{}", cols[rng.below(cols.len())], t);
        }
        t
    };
    if depth == 0 || rng.chance(40) {
        return leaf(rng);
    }
    let a = random_query(rng, detail, cols, depth - 1);
    let b = random_query(rng, detail, cols, depth - 1);
    match rng.below(4) {
        0 => format!("({a}) AND ({b})"),
        1 => format!("({a}) OR ({b})"),
        2 => format!("({a}) NOT ({b})"),
        _ => format!("{} {}", leaf(rng), leaf(rng)),
    }
}

/// Normalizes floats to 9 significant digits so bm25 compares to tolerance.
fn norm_floats(s: &str) -> String {
    s.split('\n')
        .map(|line| {
            line.split('|')
                .map(|f| match f.parse::<f64>() {
                    Ok(v) if f.contains('.') || f.contains('e') => format!("{v:.9e}"),
                    _ => f.to_string(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct Case {
    create: String,
    detail: &'static str,
    pgsz: Option<u32>,
}

fn fuzz_case(seed: u64, case: &Case, nops: usize, nqueries: usize) {
    let dir = tempfile::tempdir().unwrap();
    let tpath = temp_path(&dir, "t.db");
    let spath = temp_path(&dir, "s.db");
    let t = open(&tpath);
    let s = rusqlite::Connection::open(&spath).unwrap();
    let mut both = |sql: &str| {
        let a = tq(&t, sql);
        let b = sq(&s, sql);
        if norm_floats(&a) != norm_floats(&b) {
            // Show the rows that differ, to ease debugging.
            for line in a.lines().chain(b.lines()) {
                if let Some(r) = line.split('|').next().and_then(|r| r.parse::<i64>().ok()) {
                    eprintln!(
                        "row {r}: {}",
                        sq(&s, &format!("SELECT a, b FROM t WHERE rowid={r}"))
                    );
                }
            }
        }
        assert_eq!(
            norm_floats(&a),
            norm_floats(&b),
            "seed {seed} [{}]: {sql}",
            case.create
        );
    };
    both(&case.create);
    if let Some(pgsz) = case.pgsz {
        both(&format!("INSERT INTO t(t, rank) VALUES('pgsz', {pgsz})"));
    }
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut next_rowid = 1;
    let mut in_txn = false;
    for _ in 0..nops {
        match rng.below(20) {
            0..=11 => {
                let a = random_doc(&mut rng, 12);
                let b = random_doc(&mut rng, 30);
                both(&format!(
                    "INSERT INTO t(rowid, a, b) VALUES({next_rowid}, '{a}', '{b}')"
                ));
                next_rowid += 1;
            }
            12 | 13 if next_rowid > 1 && !case.create.contains("content=''") => {
                let r = rng.below(next_rowid - 1) + 1;
                both(&format!("DELETE FROM t WHERE rowid = {r}"));
            }
            14 if next_rowid > 1 && !case.create.contains("content=''") => {
                let r = rng.below(next_rowid - 1) + 1;
                let a = random_doc(&mut rng, 12);
                both(&format!("UPDATE t SET a = '{a}' WHERE rowid = {r}"));
            }
            15 => {
                if in_txn {
                    both("COMMIT");
                } else {
                    both("BEGIN");
                }
                in_txn = !in_txn;
            }
            16 if !in_txn => both("INSERT INTO t(t) VALUES('optimize')"),
            17 if !in_txn => both(&format!(
                "INSERT INTO t(t, rank) VALUES('merge', {})",
                rng.below(200) as i64 - 50
            )),
            _ => {}
        }
    }
    if in_txn {
        both("COMMIT");
    }
    let cols = ["a", "b"];
    let content = !case.create.contains("content=''");
    for _ in 0..nqueries {
        let q = random_query(&mut rng, case.detail, &cols, 3).replace('\'', "''");
        both(&format!(
            "SELECT rowid FROM t WHERE t MATCH '{q}' ORDER BY rowid"
        ));
        both(&format!(
            "SELECT rowid, bm25(t) FROM t WHERE t MATCH '{q}' ORDER BY rowid"
        ));
        if content {
            both(&format!(
                "SELECT rowid, highlight(t, 0, '[', ']'), snippet(t, -1, '<', '>', '...', 5) FROM t WHERE t MATCH '{q}' ORDER BY rowid"
            ));
        }
    }
    both("INSERT INTO t(t) VALUES('integrity-check')");
    for (name, kind) in [("v_row", "row"), ("v_col", "col"), ("v_inst", "instance")] {
        both(&format!(
            "CREATE VIRTUAL TABLE {name} USING fts5vocab(t, {kind})"
        ));
        both(&format!("SELECT * FROM {name}"));
    }
    both("SELECT * FROM v_row WHERE term >= 'c' AND term <= 'p'");
    both("SELECT doc, cnt FROM v_col WHERE term = 'alpha'");
    drop(t);
    drop(s);
    // Exchange files: each engine checks and queries the other's file.
    let t2 = open(&spath);
    let s2 = rusqlite::Connection::open(&tpath).unwrap();
    assert_eq!(
        tq(&t2, "INSERT INTO t(t) VALUES('integrity-check')"),
        "",
        "seed {seed}: turso on sqlite file"
    );
    assert_eq!(
        sq(&s2, "INSERT INTO t(t) VALUES('integrity-check')"),
        "",
        "seed {seed}: sqlite on turso file"
    );
    for _ in 0..nqueries / 2 {
        let q = random_query(&mut rng, case.detail, &cols, 3).replace('\'', "''");
        let sql = format!("SELECT rowid, bm25(t) FROM t WHERE t MATCH '{q}' ORDER BY rowid");
        assert_eq!(
            norm_floats(&tq(&t2, &sql)),
            norm_floats(&sq(&s2, &sql)),
            "seed {seed} exchanged: {sql}"
        );
    }
}

fn fuzz_cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for detail in ["full", "column", "none"] {
        for (extra, pgsz) in [
            ("", None),
            (", prefix='1 2'", Some(64)),
            (", tokenize='porter ascii'", Some(128)),
            (", tokenize='unicode61 remove_diacritics 2'", None),
        ] {
            cases.push(Case {
                create: format!("CREATE VIRTUAL TABLE t USING fts5(a, b, detail={detail}{extra})"),
                detail: if detail == "column" { "column" } else { detail },
                pgsz,
            });
        }
    }
    cases.push(Case {
        create: "CREATE VIRTUAL TABLE t USING fts5(a, b, content='')".into(),
        detail: "full",
        pgsz: Some(64),
    });
    cases.push(Case {
        create: "CREATE VIRTUAL TABLE t USING fts5(a, b, columnsize=0)".into(),
        detail: "full",
        pgsz: None,
    });
    cases
}

#[test]
fn fts5_differential_fuzz() {
    let seeds: u64 = std::env::var("FTS5_FUZZ_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let nops: usize = std::env::var("FTS5_FUZZ_OPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    for (i, case) in fuzz_cases().iter().enumerate() {
        for seed in 0..seeds {
            fuzz_case(seed * 1000 + i as u64 + 1, case, nops, 30);
        }
    }
}
