//! SQLite's R*Tree and Geopoly extensions (ext/rtree/rtree.c, geopoly.c).
//!
//! The on-disk format is SQLite's: the tree lives in the shadow tables
//! `%_node`, `%_parent` and `%_rowid`, each node is a big-endian blob, and
//! the algorithms (R* split, ChooseLeaf, CondenseTree) are ported from
//! rtree.c, so tables can be shared with SQLite in both directions.

mod cursor;
mod funcs;
pub(crate) mod geopoly;
mod matchfn;
mod shadow;
mod tree;

use crate::native_ext::{
    ConnectInfo, ConstraintInfo, ConstraintUsage, IndexInfo, OrderByInfo, TableUpdate,
    VirtualTable, VirtualTableModule,
};
use crate::numeric::{str_to_f64, str_to_i64, Numeric};
use crate::sync::atomic::{AtomicI32, Ordering};
use crate::sync::Arc;
use crate::types::IOResultOr;
use crate::{Connection, IOResult, LimboError, Result, Value};
use turso_ext::ConstraintOp;
use turso_ext::ResultCode;

pub(crate) use funcs::register_functions;
pub use matchfn::{RtreeMatchCall, RtreeMatchFunction, RtreeQueryInfo};

/// The rtree may have between 1 and 5 dimensions.
pub(crate) const RTREE_MAX_DIMENSIONS: usize = 5;
const RTREE_MAX_AUX_COLUMN: usize = 100;
const RTREE_MAXCELLS: usize = 51;
pub(crate) const RTREE_MAX_DEPTH: usize = 40;
const RTREE_DEFAULT_ROWEST: i64 = 1048576;

/// Constraint operators, as they appear in idxStr.
pub(crate) const RTREE_EQ: u8 = 0x41;
pub(crate) const RTREE_LE: u8 = 0x42;
pub(crate) const RTREE_LT: u8 = 0x43;
pub(crate) const RTREE_GE: u8 = 0x44;
pub(crate) const RTREE_GT: u8 = 0x45;
pub(crate) const RTREE_MATCH: u8 = 0x46;
pub(crate) const RTREE_QUERY: u8 = 0x47;
pub(crate) const RTREE_TRUE: u8 = 0x3f;
pub(crate) const RTREE_FALSE: u8 = 0x40;

/// `eWithin` values of sqlite3_rtree_query_info.
pub const NOT_WITHIN: i32 = 0;
pub const PARTLY_WITHIN: i32 = 1;
pub const FULLY_WITHIN: i32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RtreeKind {
    Real32,
    Int32,
    Geopoly,
}

/// Indexes of the shadow-table statements.
pub(crate) mod sql {
    pub const WRITE_NODE: usize = 0;
    pub const DELETE_NODE: usize = 1;
    pub const READ_ROWID: usize = 2;
    pub const WRITE_ROWID: usize = 3;
    pub const DELETE_ROWID: usize = 4;
    pub const READ_PARENT: usize = 5;
    pub const WRITE_PARENT: usize = 6;
    pub const DELETE_PARENT: usize = 7;
    pub const WRITE_AUX: usize = 8;
    pub const READ_NODE: usize = 9;
    pub const READ_AUX: usize = 10;
    pub const COUNT: usize = 11;
}

/// An R*Tree table: its shape and the SQL for its shadow tables.
#[derive(Debug)]
pub(crate) struct Rtree {
    pub(crate) kind: RtreeKind,
    pub(crate) db: String,
    pub(crate) name: String,
    pub(crate) n_dim: usize,
    pub(crate) n_dim2: usize,
    pub(crate) bytes_per_cell: usize,
    pub(crate) n_aux: usize,
    pub(crate) n_aux_not_null: usize,
    /// Declared column names, for constraint error messages.
    pub(crate) columns: Vec<String>,
    /// Node size in bytes; 0 until read from the root node.
    node_size: AtomicI32,
    pub(crate) sql: Vec<String>,
}

impl Rtree {
    pub(crate) fn int_coords(&self) -> bool {
        self.kind == RtreeKind::Int32
    }

    pub(crate) fn sql(&self, idx: usize) -> &str {
        &self.sql[idx]
    }

    /// `RTREE_MINCELLS`: a third of the cells that fit in a node.
    pub(crate) fn min_cells(&self, node_size: usize) -> usize {
        ((node_size - 4) / self.bytes_per_cell) / 3
    }

    /// The node size, read from the root node the first time.
    pub(crate) fn node_size(&self, conn: &Arc<Connection>) -> Result<usize> {
        let n = self.node_size.load(Ordering::Relaxed);
        if n > 0 {
            return Ok(n as usize);
        }
        let sql = format!(
            "SELECT length(data) FROM {}.{} WHERE nodeno = 1",
            quote_ident(&self.db),
            quote_ident(&format!("{}_node", self.name))
        );
        let rows = shadow::query_all(conn, &sql, &[])?;
        let n = rows
            .first()
            .and_then(|r| r.first())
            .map(value_int64)
            .unwrap_or(0) as i32;
        if n < 512 - 64 {
            return Err(LimboError::Corrupt(format!(
                "undersize RTree blobs in \"{}_node\"",
                self.name
            )));
        }
        self.node_size.store(n, Ordering::Relaxed);
        Ok(n as usize)
    }

    fn new(kind: RtreeKind, db: &str, name: &str, args: &[String]) -> Result<(Self, String)> {
        let mut schema = String::from("CREATE TABLE x(");
        let mut columns = Vec::new();
        let mut n_dim2 = 0usize;
        let mut n_aux = 0usize;
        let mut n_aux_not_null = 0usize;
        if kind == RtreeKind::Geopoly {
            if args.len() + 3 >= RTREE_MAX_AUX_COLUMN + 4 {
                return Err(LimboError::SqlError(
                    "Too many columns for a geopoly table".to_string(),
                ));
            }
            schema.push_str("_shape");
            columns.push("_shape".to_string());
            n_aux = 1;
            n_aux_not_null = 1;
            for arg in args {
                n_aux += 1;
                schema.push(',');
                schema.push_str(arg);
                columns.push(dequote(token(arg)));
            }
            n_dim2 = 4;
        } else {
            let argc = args.len() + 3;
            if !(6..=RTREE_MAX_AUX_COLUMN + 3).contains(&argc) {
                return Err(LimboError::SqlError(
                    if argc < 6 {
                        "Too few columns for an rtree table"
                    } else {
                        "Too many columns for an rtree table"
                    }
                    .to_string(),
                ));
            }
            let first = token(&args[0]);
            schema.push_str(first);
            schema.push_str(" INT");
            columns.push(dequote(first));
            let mut stopped = false;
            for arg in &args[1..] {
                if let Some(aux) = arg.strip_prefix('+') {
                    n_aux += 1;
                    let t = token(aux);
                    schema.push(',');
                    schema.push_str(t);
                    columns.push(dequote(t));
                } else if n_aux > 0 {
                    stopped = true;
                    break;
                } else {
                    n_dim2 += 1;
                    let t = token(arg);
                    schema.push(',');
                    schema.push_str(t);
                    schema.push_str(if kind == RtreeKind::Int32 {
                        " INT"
                    } else {
                        " REAL"
                    });
                    columns.push(dequote(t));
                }
            }
            if stopped {
                return Err(LimboError::SqlError(
                    "Auxiliary rtree columns must be last".to_string(),
                ));
            }
            let msg = if n_dim2 / 2 < 1 {
                Some("Too few columns for an rtree table")
            } else if n_dim2 > RTREE_MAX_DIMENSIONS * 2 {
                Some("Too many columns for an rtree table")
            } else if n_dim2 % 2 != 0 {
                Some("Wrong number of columns for an rtree table")
            } else {
                None
            };
            if let Some(msg) = msg {
                return Err(LimboError::SqlError(msg.to_string()));
            }
        }
        schema.push(')');
        let mut t = Rtree {
            kind,
            db: db.to_string(),
            name: name.to_string(),
            n_dim: n_dim2 / 2,
            n_dim2,
            bytes_per_cell: 8 + n_dim2 * 4,
            n_aux,
            n_aux_not_null,
            columns,
            node_size: AtomicI32::new(0),
            sql: Vec::new(),
        };
        t.sql = t.build_sql();
        Ok((t, schema))
    }

    /// A copy of this table under a new name.
    fn renamed(&self, name: &str) -> Self {
        let mut t = Rtree {
            kind: self.kind,
            db: self.db.clone(),
            name: name.to_string(),
            n_dim: self.n_dim,
            n_dim2: self.n_dim2,
            bytes_per_cell: self.bytes_per_cell,
            n_aux: self.n_aux,
            n_aux_not_null: self.n_aux_not_null,
            columns: self.columns.clone(),
            node_size: AtomicI32::new(self.node_size.load(Ordering::Relaxed)),
            sql: Vec::new(),
        };
        t.sql = t.build_sql();
        t
    }

    fn table(&self, suffix: &str) -> String {
        format!(
            "{}.{}",
            quote_ident(&self.db),
            quote_ident(&format!("{}{suffix}", self.name))
        )
    }

    fn build_sql(&self) -> Vec<String> {
        let node = self.table("_node");
        let rowid = self.table("_rowid");
        let parent = self.table("_parent");
        let mut v = vec![String::new(); sql::COUNT];
        v[sql::WRITE_NODE] = format!("INSERT OR REPLACE INTO {node} VALUES(?1, ?2)");
        v[sql::DELETE_NODE] = format!("DELETE FROM {node} WHERE nodeno = ?1");
        v[sql::READ_ROWID] = format!("SELECT nodeno FROM {rowid} WHERE rowid = ?1");
        v[sql::WRITE_ROWID] = if self.n_aux == 0 {
            format!("INSERT OR REPLACE INTO {rowid} VALUES(?1, ?2)")
        } else {
            // An upsert keeps the auxiliary columns.
            format!(
                "INSERT INTO {rowid}(rowid,nodeno)VALUES(?1,?2)\
                 ON CONFLICT(rowid)DO UPDATE SET nodeno=excluded.nodeno"
            )
        };
        v[sql::DELETE_ROWID] = format!("DELETE FROM {rowid} WHERE rowid = ?1");
        v[sql::READ_PARENT] = format!("SELECT parentnode FROM {parent} WHERE nodeno = ?1");
        v[sql::WRITE_PARENT] = format!("INSERT OR REPLACE INTO {parent} VALUES(?1, ?2)");
        v[sql::DELETE_PARENT] = format!("DELETE FROM {parent} WHERE nodeno = ?1");
        if self.n_aux > 0 {
            let sets = (0..self.n_aux)
                .map(|ii| {
                    if ii < self.n_aux_not_null {
                        format!("a{ii}=coalesce(?{},a{ii})", ii + 2)
                    } else {
                        format!("a{ii}=?{}", ii + 2)
                    }
                })
                .collect::<Vec<_>>()
                .join(",");
            v[sql::WRITE_AUX] = format!("UPDATE {rowid}SET {sets} WHERE rowid=?1");
            v[sql::READ_AUX] = format!("SELECT * FROM {rowid} WHERE rowid=?1");
        }
        v[sql::READ_NODE] = format!("SELECT data FROM {node} WHERE nodeno = ?1");
        v
    }

    /// The shadow tables, created by xCreate.
    fn create_shadow_tables(&self, conn: &Arc<Connection>, node_size: usize) -> Result<()> {
        // Like SQLite, the stored schema of a main-database table has no qualifier.
        let prefix = if self.db.eq_ignore_ascii_case("main") {
            String::new()
        } else {
            format!("{}.", quote_ident(&self.db))
        };
        let name =
            |suffix: &str| format!("{prefix}{}", quote_ident(&format!("{}{suffix}", self.name)));
        let mut rowid = format!(
            "CREATE TABLE {}(rowid INTEGER PRIMARY KEY,nodeno",
            name("_rowid")
        );
        for ii in 0..self.n_aux {
            rowid.push_str(&format!(",a{ii}"));
        }
        rowid.push(')');
        shadow::execute_batch(
            conn,
            &[
                rowid,
                format!(
                    "CREATE TABLE {}(nodeno INTEGER PRIMARY KEY,data)",
                    name("_node")
                ),
                format!(
                    "CREATE TABLE {}(nodeno INTEGER PRIMARY KEY,parentnode)",
                    name("_parent")
                ),
                format!(
                    "INSERT INTO {}VALUES(1,zeroblob({node_size}))",
                    name("_node")
                ),
            ],
        )
    }

    /// The error for a failed constraint: the rowid when `col` is 0, else
    /// the `col <= col+1` bound.
    pub(crate) fn constraint_error(&self, col: usize) -> LimboError {
        let col_name = |i: usize| self.columns.get(i).cloned().unwrap_or_default();
        if col == 0 {
            LimboError::Constraint(format!(
                "UNIQUE constraint failed: {}.{}",
                self.name,
                col_name(0)
            ))
        } else {
            LimboError::Constraint(format!(
                "rtree constraint failed: {}.({}<={})",
                self.name,
                col_name(col),
                col_name(col + 1)
            ))
        }
    }
}

/// A virtual table module for `rtree`, `rtree_i32` or `geopoly`.
#[derive(Debug)]
pub(crate) struct RtreeModule {
    kind: RtreeKind,
}

impl RtreeModule {
    pub(crate) fn new(kind: RtreeKind) -> Self {
        Self { kind }
    }
}

fn arg_strings(args: &[Value]) -> Vec<String> {
    args.iter()
        .map(|v| v.to_text().unwrap_or_default().trim().to_string())
        .collect()
}

impl VirtualTableModule for RtreeModule {
    type Table = RtreeTable;

    fn schema(&self, args: &[Value]) -> Result<String> {
        Ok(Rtree::new(self.kind, "main", "x", &arg_strings(args))?.1)
    }

    fn create(&self, _args: &[Value]) -> Result<Self::Table> {
        Err(LimboError::InternalError(
            "rtree tables need connect".into(),
        ))
    }

    fn connect(&self, info: &ConnectInfo, args: &[Value]) -> Result<(Self::Table, String)> {
        let (t, schema) = Rtree::new(
            self.kind,
            info.schema_name,
            info.table_name,
            &arg_strings(args),
        )?;
        if info.create {
            let conn = info.connection.ok_or_else(|| {
                LimboError::InternalError("CREATE VIRTUAL TABLE without a connection".into())
            })?;
            let page_size = shadow::query_all(
                conn,
                &format!("PRAGMA {}.page_size", quote_ident(&t.db)),
                &[],
            )?
            .first()
            .and_then(|r| r.first())
            .map(value_int64)
            .unwrap_or(4096) as usize;
            let node_size = (page_size - 64).min(4 + t.bytes_per_cell * RTREE_MAXCELLS);
            t.create_shadow_tables(conn, node_size)?;
            t.node_size.store(node_size as i32, Ordering::Relaxed);
        }
        Ok((RtreeTable { t: Arc::new(t) }, schema))
    }

    fn innocuous(&self) -> bool {
        true
    }
}

#[derive(Debug)]
pub(crate) struct RtreeTable {
    t: Arc<Rtree>,
}

impl VirtualTable for RtreeTable {
    type Cursor = cursor::RtreeCursor;

    fn open(&self, connection: Arc<Connection>) -> Result<Self::Cursor> {
        Ok(cursor::RtreeCursor::new(self.t.clone(), connection))
    }

    fn best_index(
        &self,
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> std::result::Result<IndexInfo, ResultCode> {
        if self.t.kind == RtreeKind::Geopoly {
            return Ok(geopoly::best_index(constraints));
        }
        Ok(best_index(&self.t, constraints))
    }

    fn readonly(&self) -> bool {
        false
    }

    fn create_update_with_connection(
        &mut self,
        connection: &Arc<Connection>,
        args: Vec<Value>,
        on_conflict: u8,
    ) -> Result<Box<dyn TableUpdate>> {
        let rowid = if self.t.kind == RtreeKind::Geopoly {
            geopoly::update(&self.t, connection, &args, on_conflict)?
        } else {
            tree::update(&self.t, connection, &args, on_conflict)?
        };
        Ok(Box::new(DoneUpdate(rowid)))
    }

    fn destroy_with_connection(&mut self, connection: &Arc<Connection>) -> Result<()> {
        shadow::execute_batch(
            connection,
            &[
                format!("DROP TABLE {}", self.t.table("_node")),
                format!("DROP TABLE {}", self.t.table("_rowid")),
                format!("DROP TABLE {}", self.t.table("_parent")),
            ],
        )
    }

    fn rename_with_connection(&mut self, connection: &Arc<Connection>, name: &str) -> Result<()> {
        let rename = |suffix: &str| {
            format!(
                "ALTER TABLE {} RENAME TO {}",
                self.t.table(suffix),
                quote_ident(&format!("{name}{suffix}"))
            )
        };
        shadow::execute_batch(
            connection,
            &[rename("_node"), rename("_parent"), rename("_rowid")],
        )?;
        self.t = Arc::new(self.t.renamed(name));
        Ok(())
    }
}

struct DoneUpdate(Option<i64>);

impl TableUpdate for DoneUpdate {
    fn step(&mut self) -> IOResultOr<Option<i64>> {
        Ok(IOResult::Done(self.0))
    }
}

/// xBestIndex of rtree.c.
fn best_index(t: &Rtree, constraints: &[ConstraintInfo]) -> IndexInfo {
    let mut usages = vec![
        ConstraintUsage {
            argv_index: None,
            omit: false,
        };
        constraints.len()
    ];
    // A MATCH, even an unusable one, rules out the rowid plan.
    let has_match = constraints.iter().any(|c| c.op == ConstraintOp::Match);
    let mut idx_str = Vec::new();
    for (ii, p) in constraints.iter().enumerate() {
        if idx_str.len() >= RTREE_MAX_DIMENSIONS * 8 {
            break;
        }
        if !has_match && p.usable && p.column_index == 0 && p.op == ConstraintOp::Eq {
            // An equality constraint on the rowid: a direct lookup.
            for u in usages.iter_mut() {
                *u = ConstraintUsage {
                    argv_index: None,
                    omit: false,
                };
            }
            usages[ii] = ConstraintUsage {
                argv_index: Some(1),
                omit: true,
            };
            return IndexInfo {
                idx_num: 1,
                idx_str: None,
                order_by_consumed: false,
                estimated_cost: 30.0,
                estimated_rows: 1,
                constraint_usages: usages,
            };
        }
        let col = p.column_index as usize;
        if p.usable && ((col > 0 && col <= t.n_dim2) || p.op == ConstraintOp::Match) {
            let (op, omit) = match p.op {
                ConstraintOp::Eq => (RTREE_EQ, false),
                ConstraintOp::Gt => (RTREE_GT, false),
                ConstraintOp::Le => (RTREE_LE, true),
                ConstraintOp::Lt => (RTREE_LT, false),
                ConstraintOp::Ge => (RTREE_GE, true),
                ConstraintOp::Match => (RTREE_MATCH, true),
                _ => (0, true),
            };
            if op != 0 {
                idx_str.push(op);
                idx_str.push((col as i64 - 1 + b'0' as i64) as u8);
                usages[ii] = ConstraintUsage {
                    argv_index: Some((idx_str.len() / 2) as u32),
                    omit,
                };
            }
        }
    }
    let n_row = RTREE_DEFAULT_ROWEST >> (idx_str.len() / 2);
    IndexInfo {
        idx_num: 2,
        idx_str: (!idx_str.is_empty()).then(|| idx_str.iter().map(|&b| b as char).collect()),
        order_by_consumed: false,
        estimated_cost: 6.0 * n_row as f64,
        estimated_rows: n_row.min(u32::MAX as i64) as u32,
        constraint_usages: usages,
    }
}

/// Double-quotes an identifier.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The first SQL token of `z`, as `sqlite3GetToken` reads it.
fn token(z: &str) -> &str {
    let b = z.as_bytes();
    if b.is_empty() {
        return z;
    }
    let end = match b[0] {
        q @ (b'"' | b'\'' | b'`') => {
            let mut i = 1;
            loop {
                if i >= b.len() {
                    break b.len();
                }
                if b[i] == q {
                    if b.get(i + 1) == Some(&q) {
                        i += 2;
                        continue;
                    }
                    break i + 1;
                }
                i += 1;
            }
        }
        b'[' => b.iter().position(|&c| c == b']').map_or(b.len(), |i| i + 1),
        c if c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80 => b
            .iter()
            .position(|&c| !(c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80))
            .unwrap_or(b.len()),
        c if c.is_ascii_whitespace() => b
            .iter()
            .position(|c| !c.is_ascii_whitespace())
            .unwrap_or(b.len()),
        _ => 1,
    };
    &z[..end]
}

/// Removes SQL quoting from an identifier.
fn dequote(z: &str) -> String {
    let b = z.as_bytes();
    match b.first() {
        Some(&q @ (b'"' | b'\'' | b'`')) if z.len() >= 2 && b[b.len() - 1] == q => {
            let q = q as char;
            z[1..z.len() - 1].replace(&format!("{q}{q}"), &q.to_string())
        }
        Some(b'[') if z.ends_with(']') => z[1..z.len() - 1].to_string(),
        _ => z.to_string(),
    }
}

/// `sqlite3_value_int64`.
pub(crate) fn value_int64(v: &Value) -> i64 {
    match v {
        Value::Null => 0,
        Value::Numeric(Numeric::Integer(i)) => *i,
        Value::Numeric(Numeric::Float(f)) => f64::from(*f) as i64,
        Value::Text(t) => str_to_i64(t.as_str()).unwrap_or(0),
        Value::Blob(b) => str_to_i64(String::from_utf8_lossy(b)).unwrap_or(0),
    }
}

/// `sqlite3_value_double`.
pub(crate) fn value_double(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Numeric(n) => n.to_f64(),
        Value::Text(t) => str_to_f64(t.as_str()).map(f64::from).unwrap_or(0.0),
        Value::Blob(b) => str_to_f64(String::from_utf8_lossy(b))
            .map(f64::from)
            .unwrap_or(0.0),
    }
}

/// `sqlite3_value_numeric_type`: the type after numeric affinity.
pub(crate) fn value_numeric(v: &Value) -> Value {
    match v {
        Value::Text(_) => match Numeric::from_value_strict(v) {
            Some(n) => Value::Numeric(n),
            None => v.clone(),
        },
        _ => v.clone(),
    }
}

/// `sqlite3IntFloatCompare(i, r) == 0`.
pub(crate) fn int_equals_float(i: i64, r: f64) -> bool {
    if r.is_nan() {
        return false;
    }
    if r < -9223372036854775808.0 || r >= 9223372036854775808.0 {
        return false;
    }
    let y = r as i64;
    i == y && (i as f64) == r
}

/// Registers the rtree, rtree_i32 and geopoly modules and their functions.
pub(crate) fn register(syms: &mut crate::SymbolTable) -> Result<()> {
    use crate::ext::{ModuleImplementation, VTabImpl};
    for (name, kind) in [
        ("rtree", RtreeKind::Real32),
        ("rtree_i32", RtreeKind::Int32),
        ("geopoly", RtreeKind::Geopoly),
    ] {
        syms.vtab_modules.insert(
            name.to_string(),
            Arc::new(VTabImpl {
                module_kind: turso_ext::VTabKind::VirtualTable,
                implementation: ModuleImplementation::Native(Arc::new(RtreeModule::new(kind))),
                eponymous: None,
            }),
        );
    }
    register_functions(syms)
}
