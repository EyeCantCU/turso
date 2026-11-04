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
        // turso prefixes parse errors; SQLite does not.
        Err(e) => format!("ERR: {}", e.to_string().trim_start_matches("Parse error: ")),
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
    let both = |sql: &str| {
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
        // Plans the module orders itself: descending rowid, the rank sorter,
        // rowid constraints and LIMIT.
        let hl = if content {
            "highlight(t, 1, '[', ']')"
        } else {
            "NULL"
        };
        let lo = rng.below(80);
        for sql in [
            format!("SELECT rowid, bm25(t), {hl} FROM t WHERE t MATCH '{q}' ORDER BY rowid DESC"),
            format!("SELECT rowid, rank, {hl} FROM t WHERE t MATCH '{q}' ORDER BY rank"),
            format!("SELECT rowid, rank FROM t WHERE t MATCH '{q}' ORDER BY rank DESC LIMIT 5"),
            format!("SELECT rowid, bm25(t) FROM t WHERE t MATCH '{q}' AND rowid > {lo} ORDER BY rowid DESC LIMIT 7"),
            format!("SELECT rowid FROM t WHERE t MATCH '{q}' AND rowid IN ({lo}, {}, {})", lo + 3, lo + 11),
            format!("SELECT rowid, {hl} FROM t WHERE t MATCH '{q}' AND rowid = {lo}"),
        ] {
            both(&sql);
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

/// Runs each statement on both engines and compares results and errors.
fn compare_script(stmts: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let t = open(&temp_path(&dir, "c.db"));
    let s = rusqlite::Connection::open_in_memory().unwrap();
    for sql in stmts {
        assert_eq!(
            norm_floats(&tq(&t, sql)),
            norm_floats(&sq(&s, sql)),
            "{sql}"
        );
    }
}

// A prefix scan with a column filter skips rows where the filtered list is
// empty; otherwise a stale nomatch row clears sibling phrase poslists.
#[test]
fn fts5_prefix_colset_skips_empty_rows() {
    compare_script(&[
        "CREATE VIRTUAL TABLE t USING fts5(a, b, detail=column)",
        "INSERT INTO t(rowid, a, b) VALUES(10, 'betamax', 'phi'), (12, 'x', 'phi nu'), (14, 'y', 'beta phi')",
        "SELECT rowid, highlight(t, 1, '[', ']'), bm25(t) FROM t WHERE t MATCH '((b:bet* OR b:nu) AND phi) NOT zzzz' ORDER BY rowid",
        "SELECT rowid FROM t WHERE t MATCH 'b:bet*' ORDER BY rowid",
    ]);
}

// Secure-delete edits leaf pages in place exactly as SQLite does, so the
// shadow tables stay byte-identical in every detail mode.
#[test]
fn fts5_secure_delete_bytes_match() {
    for detail in ["full", "column", "none"] {
        let mut stmts = vec![
            format!("CREATE VIRTUAL TABLE t USING fts5(a, b, detail={detail})"),
            "INSERT INTO t(t, rank) VALUES('pgsz', 64)".to_string(),
            "INSERT INTO t(t, rank) VALUES('secure-delete', 1)".to_string(),
        ];
        let mut rng = Rng(77);
        for i in 1..=120 {
            let doc = |r: &mut Rng| {
                (0..1 + r.below(12))
                    .map(|_| WORDS[r.below(WORDS.len())])
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let (a, b) = (doc(&mut rng), doc(&mut rng));
            stmts.push(format!(
                "INSERT INTO t(rowid, a, b) VALUES({i}, '{a}', '{b}')"
            ));
            if i % 3 == 0 {
                let r = 1 + rng.below(i);
                stmts.push(format!("DELETE FROM t WHERE rowid = {r}"));
            }
            if i % 5 == 0 {
                let r = 1 + rng.below(i);
                let a = doc(&mut rng);
                stmts.push(format!("UPDATE t SET a = '{a}' WHERE rowid = {r}"));
            }
        }
        stmts.push("SELECT id, hex(block) FROM t_data ORDER BY id".to_string());
        stmts.push("SELECT segid, hex(term), pgno FROM t_idx ORDER BY 1, 2".to_string());
        stmts.push("INSERT INTO t(t) VALUES('integrity-check')".to_string());
        let refs: Vec<&str> = stmts.iter().map(|s| s.as_str()).collect();
        compare_script(&refs);
    }
}

#[test]
fn fts5_external_content() {
    compare_script(&[
        "CREATE TABLE docs(id INTEGER PRIMARY KEY, title, body)",
        "INSERT INTO docs VALUES(1, 'red apple', 'a sweet fruit'), (2, 'green pear', 'also a fruit'), (3, 'blue sky', 'not a fruit')",
        "CREATE VIRTUAL TABLE t USING fts5(title, body, content='docs', content_rowid='id')",
        "INSERT INTO t(t) VALUES('rebuild')",
        "SELECT rowid, title FROM t WHERE t MATCH 'fruit' ORDER BY rowid",
        "SELECT rowid, highlight(t, 1, '[', ']') FROM t WHERE t MATCH 'fruit NOT not'",
        "INSERT INTO t(t, rowid, title, body) VALUES('delete', 3, 'blue sky', 'not a fruit')",
        "DELETE FROM docs WHERE id = 3",
        "SELECT rowid FROM t WHERE t MATCH 'sky'",
        "INSERT INTO t(t) VALUES('integrity-check')",
        "INSERT INTO t(t, rank) VALUES('integrity-check', 1)",
        "SELECT * FROM t",
        "INSERT INTO t(t) VALUES('delete-all')",
        "SELECT count(*) FROM t WHERE t MATCH 'fruit'",
        "CREATE VIRTUAL TABLE u USING fts5(a, content='')",
        "INSERT INTO u(rowid, a) VALUES(1, 'one two'), (2, 'two three')",
        "SELECT rowid, a FROM u WHERE u MATCH 'two'",
        "DELETE FROM u WHERE rowid = 1",
        "UPDATE u SET a = 'x' WHERE rowid = 2",
        "INSERT INTO u(u, rowid, a) VALUES('delete', 1, 'one two')",
        "SELECT rowid FROM u WHERE u MATCH 'two'",
        "INSERT INTO u(u) VALUES('rebuild')",
        "SELECT * FROM u",
    ]);
}

#[test]
fn fts5_contentless_delete_and_secure_delete() {
    compare_script(&[
        "CREATE VIRTUAL TABLE t USING fts5(a, b, content='', contentless_delete=1)",
        "INSERT INTO t(rowid, a, b) VALUES(1, 'alpha beta', 'gamma'), (2, 'beta', 'delta'), (3, 'alpha', 'beta')",
        "DELETE FROM t WHERE rowid = 2",
        "SELECT rowid FROM t WHERE t MATCH 'beta' ORDER BY rowid",
        "INSERT INTO t(t) VALUES('integrity-check')",
        "UPDATE t SET a = 'zeta', b = 'eta' WHERE rowid = 3",
        "SELECT rowid FROM t WHERE t MATCH 'zeta OR eta' ORDER BY rowid",
        "INSERT INTO t(t) VALUES('optimize')",
        "SELECT rowid FROM t WHERE t MATCH 'alpha OR zeta' ORDER BY rowid",
        "INSERT INTO t(t, rowid, a, b) VALUES('delete', 1, 'x', 'y')",
        "CREATE VIRTUAL TABLE s USING fts5(a)",
        "INSERT INTO s(s, rank) VALUES('secure-delete', 1)",
        "INSERT INTO s(rowid, a) VALUES(1, 'one two three'), (2, 'two three four'), (3, 'three four five')",
        "DELETE FROM s WHERE rowid = 2",
        "SELECT rowid FROM s WHERE s MATCH 'three' ORDER BY rowid",
        "SELECT * FROM s_config ORDER BY k",
        "INSERT INTO s(s) VALUES('integrity-check')",
        "CREATE VIRTUAL TABLE v USING fts5vocab(s, row)",
        "SELECT * FROM v",
    ]);
}

#[test]
fn fts5_options_and_errors() {
    compare_script(&[
        "CREATE VIRTUAL TABLE e1 USING fts5(a, rank)",
        "CREATE VIRTUAL TABLE e2 USING fts5(a, detail=bogus)",
        "CREATE VIRTUAL TABLE e3 USING fts5(a, tokenize='nosuch')",
        "CREATE VIRTUAL TABLE e4 USING fts5(a, prefix='0')",
        "CREATE VIRTUAL TABLE e5 USING fts5(a, contentless_delete=1)",
        "CREATE VIRTUAL TABLE e6 USING fts5(a b c)",
        "CREATE VIRTUAL TABLE e7 USING fts5(a, unknown=1)",
        "CREATE VIRTUAL TABLE t USING fts5(a, b UNINDEXED, tokenize = \"unicode61 tokenchars '-' separators 'x'\")",
        "INSERT INTO t VALUES('well-known taxi', 'skip me')",
        "SELECT * FROM t WHERE t MATCH 'well-known'",
        "SELECT * FROM t WHERE t MATCH 'ta'",
        "SELECT * FROM t WHERE t MATCH 'skip'",
        "SELECT * FROM t WHERE t MATCH 'a:(well-known OR taxi)'",
        "SELECT * FROM t WHERE t MATCH 'b:skip'",
        "SELECT * FROM t WHERE t MATCH 'nosuchcol:x'",
        "SELECT * FROM t WHERE t MATCH '\"unterminated'",
        "SELECT * FROM t WHERE t MATCH 'x AND'",
        "SELECT * FROM t WHERE t MATCH ''",
        "SELECT * FROM t WHERE t MATCH 'NEAR(a b, x)'",
        "SELECT * FROM t WHERE t MATCH '*bogus'",
        "SELECT rowid FROM t WHERE t NOT MATCH 'taxi'",
        "INSERT INTO t(t, rank) VALUES('pgsz', 10)",
        "INSERT INTO t(t, rank) VALUES('nosuch', 1)",
        "INSERT INTO t(t, rank) VALUES('automerge', 8)",
        "INSERT INTO t(t, rank) VALUES('rank', 'bm25(10.0, 1.0)')",
        "SELECT k, v FROM t_config ORDER BY k",
        "SELECT rowid, rank FROM t WHERE t MATCH 'taxi'",
        "SELECT rowid, rank FROM t WHERE t MATCH 'taxi' AND rank MATCH 'bm25(1.0)'",
        "INSERT INTO t(t) VALUES('delete-all')",
        "CREATE VIRTUAL TABLE tri USING fts5(x, tokenize='trigram')",
        "INSERT INTO tri VALUES('the quick brown fox'), ('jumped over'), ('ABCDEF')",
        "SELECT rowid FROM tri WHERE tri MATCH 'uic'",
        "SELECT rowid FROM tri WHERE x LIKE '%BROWN%'",
        "SELECT rowid FROM tri WHERE x GLOB '*own*'",
        "SELECT rowid FROM tri WHERE tri MATCH 'cde'",
        "CREATE VIRTUAL TABLE tri2 USING fts5(x, tokenize='trigram case_sensitive 1')",
        "INSERT INTO tri2 VALUES('ABCDEF')",
        "SELECT rowid FROM tri2 WHERE tri2 MATCH 'cde'",
        "SELECT rowid FROM tri2 WHERE tri2 MATCH 'CDE'",
        "CREATE VIRTUAL TABLE asc USING fts5(x, tokenize='ascii')",
        "INSERT INTO asc VALUES('Héllo wörld')",
        "SELECT rowid FROM asc WHERE asc MATCH 'héllo'",
        "SELECT rowid FROM asc WHERE asc MATCH 'hello'",
        "CREATE VIRTUAL TABLE loc USING fts5(a, locale=1)",
        "INSERT INTO loc VALUES(fts5_locale('en', 'hello world'))",
        "INSERT INTO loc VALUES('plain text')",
        "SELECT rowid, a, fts5_get_locale(loc, 0) FROM loc",
        "SELECT rowid FROM loc WHERE loc MATCH 'world'",
        "INSERT INTO t VALUES(fts5_locale('en', 'x'), 'y')",
        "SELECT fts5_locale('', 'text'), typeof(fts5_locale('de', 'text'))",
        "DROP TABLE t",
        "SELECT name FROM sqlite_schema WHERE name LIKE 't_%' ORDER BY name",
        "ALTER TABLE tri RENAME TO tri3",
        "SELECT name FROM sqlite_schema WHERE name LIKE 'tri3%' ORDER BY name",
        "SELECT rowid FROM tri3 WHERE tri3 MATCH 'uic'",
    ]);
}

/// Runs sqlite/conformance/turso-sqltests/fts5.sqltest on turso (its
/// expectations were produced with SQLite 3.54).
#[test]
fn fts5_sqltest_file() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../sqlite/conformance/turso-sqltests/fts5.sqltest"
    ))
    .unwrap();
    let block = |s: &str, start: usize| -> (String, usize) {
        let open = s[start..].find("{\n").unwrap() + start + 2;
        let close = s[open..].find("\n}").unwrap() + open;
        (s[open..close].to_string(), close + 2)
    };
    let mut setups = std::collections::HashMap::new();
    let mut pos = 0;
    let mut pending_setup: Option<String> = None;
    let mut ntests = 0;
    while pos < src.len() {
        let line_end = src[pos..].find('\n').map_or(src.len(), |i| pos + i);
        let line = src[pos..line_end].trim();
        if let Some(name) = line.strip_prefix("setup ") {
            let (body, next) = block(&src, pos);
            setups.insert(name.trim_end_matches(" {").to_string(), body);
            pos = next;
            continue;
        }
        if let Some(name) = line.strip_prefix("@setup ") {
            pending_setup = Some(name.to_string());
        }
        if let Some(name) = line.strip_prefix("test ") {
            let (body, next) = block(&src, pos);
            let is_error = src[next..].trim_start().starts_with("expect error");
            let (expect, next) = block(&src, next);
            let dir = tempfile::tempdir().unwrap();
            let conn = open(&temp_path(&dir, "s.db"));
            let mut sql = String::new();
            if let Some(s) = pending_setup.take() {
                sql.push_str(&setups[&s]);
                sql.push('\n');
            }
            sql.push_str(&body);
            let mut out = Vec::new();
            let mut error = None;
            for stmt in sql.split(";\n").map(str::trim).filter(|s| !s.is_empty()) {
                let r = tq(&conn, stmt.trim_end_matches(';'));
                if let Some(e) = r.strip_prefix("ERR: ") {
                    error = Some(e.to_string());
                    break;
                }
                if !r.is_empty() {
                    out.push(r);
                }
            }
            let expect: Vec<&str> = expect.lines().map(str::trim).collect();
            if is_error {
                let e = error.unwrap_or_default();
                assert!(expect.iter().all(|x| e.contains(x)), "{name}: {e}");
            } else {
                assert_eq!(error, None, "{name}");
                let got = out.join("\n");
                let got: Vec<&str> = got.lines().collect();
                assert_eq!(got, expect, "{name}");
            }
            ntests += 1;
            pos = next;
            continue;
        }
        pos = line_end + 1;
    }
    assert!(ntests >= 10);
}

#[test]
fn fts5_two_connections() {
    let dir = tempfile::tempdir().unwrap();
    let path = temp_path(&dir, "m.db");
    let io: Arc<dyn IO> = Arc::new(PlatformIO::new().unwrap());
    let db = Database::open_file(io, &path, Arc::new(SqliteDialect)).unwrap();
    let a = db.connect(ConnectOpts::new()).unwrap();
    let b = db.connect(ConnectOpts::new()).unwrap();
    texec(&a, "CREATE VIRTUAL TABLE t USING fts5(x)");
    texec(&a, "INSERT INTO t VALUES('alpha beta')");
    texec(&a, "BEGIN");
    texec(&a, "INSERT INTO t VALUES('alpha gamma')");
    // A sees its pending rows; B sees only committed ones.
    assert_eq!(tq(&a, "SELECT count(*) FROM t WHERE t MATCH 'alpha'"), "2");
    assert_eq!(tq(&b, "SELECT count(*) FROM t WHERE t MATCH 'alpha'"), "1");
    texec(&a, "COMMIT");
    assert_eq!(tq(&b, "SELECT count(*) FROM t WHERE t MATCH 'alpha'"), "2");
    // A config change made by one connection is seen by the other.
    texec(&b, "INSERT INTO t(t, rank) VALUES('rank', 'bm25(2.0)')");
    texec(&a, "INSERT INTO t VALUES('gamma delta')");
    assert_eq!(
        tq(&a, "SELECT k, v FROM t_config WHERE k = 'rank'"),
        "rank|bm25(2.0)"
    );
    texec(&a, "BEGIN");
    texec(&a, "INSERT INTO t VALUES('alpha alpha')");
    texec(&a, "ROLLBACK");
    assert_eq!(tq(&b, "SELECT count(*) FROM t WHERE t MATCH 'alpha'"), "2");
    assert_eq!(tq(&a, "SELECT count(*) FROM t WHERE t MATCH 'alpha'"), "2");
    assert_eq!(tq(&a, "INSERT INTO t(t) VALUES('integrity-check')"), "");
    texec(&a, "BEGIN");
    texec(&a, "INSERT INTO t VALUES('epsilon')");
    texec(&a, "SAVEPOINT s1");
    texec(&a, "INSERT INTO t VALUES('epsilon zeta')");
    texec(&a, "ROLLBACK TO s1");
    texec(&a, "COMMIT");
    assert_eq!(
        tq(&b, "SELECT count(*) FROM t WHERE t MATCH 'epsilon'"),
        "1"
    );
    assert_eq!(tq(&b, "INSERT INTO t(t) VALUES('integrity-check')"), "");
}

#[test]
#[ignore]
fn fts5_bulk_timing() {
    let dir = tempfile::tempdir().unwrap();
    let t = open(&temp_path(&dir, "b.db"));
    texec(&t, "CREATE VIRTUAL TABLE t USING fts5(a, b)");
    let mut rng = Rng(7);
    let start = std::time::Instant::now();
    texec(&t, "BEGIN");
    for i in 0..5000 {
        let a = random_doc(&mut rng, 10);
        let b = random_doc(&mut rng, 40);
        texec(
            &t,
            &format!("INSERT INTO t(rowid, a, b) VALUES({}, '{a}', '{b}')", i + 1),
        );
    }
    texec(&t, "COMMIT");
    eprintln!("insert 5000: {:?}", start.elapsed());
    let start = std::time::Instant::now();
    for _ in 0..100 {
        tq(
            &t,
            "SELECT rowid, bm25(t) FROM t WHERE t MATCH 'alpha AND beta' ORDER BY rank LIMIT 10",
        );
    }
    eprintln!("100 queries: {:?}", start.elapsed());
}
