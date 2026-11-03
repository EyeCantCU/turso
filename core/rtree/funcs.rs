//! rtreenode(), rtreedepth() and rtreecheck().

use super::cursor::{read_i64, read_u16, read_u32};
use super::shadow;
use super::{quote_ident, value_int64, RTREE_MAX_DEPTH};
use crate::native_ext::{FunctionArity, ScalarCall, ScalarFunction};
use crate::sync::Arc;
use crate::types::IOResultOr;
use crate::ExternalFunc;
use crate::{Connection, IOResult, LimboError, Register, Result, SymbolTable, Value};

/// Formats `x` with SQLite's printf (`%g`, or `%!g` when `alt`).
pub(crate) fn fmt_g(x: f64, alt: bool) -> String {
    let fmt = Value::build_text(if alt { "%!g" } else { "%g" });
    crate::functions::printf::exec_printf_values(&fmt, &[&Value::from_f64(x)])
        .ok()
        .and_then(|v| v.to_text().map(str::to_string))
        .unwrap_or_default()
}

macro_rules! scalar {
    ($name:ident, $call:ident, |$conn:ident, $args:ident| $body:expr) => {
        #[derive(Debug)]
        pub(crate) struct $name;
        pub(crate) struct $call;
        impl ScalarFunction for $name {
            type Call = $call;
            fn create_call(&self) -> Result<Self::Call> {
                Ok($call)
            }
        }
        impl ScalarCall for $call {
            type Output = Value;
            fn step(&mut self, $conn: &Arc<Connection>, $args: &[Register]) -> IOResultOr<Value> {
                let r: Result<Value> = $body;
                Ok(IOResult::Done(r?))
            }
        }
    };
}
pub(crate) use scalar;

scalar!(RtreeNodeFn, RtreeNodeCall, |_conn, args| Ok(rtreenode(
    args[0].get_value(),
    args[1].get_value()
)));

fn rtreenode(dim: &Value, data: &Value) -> Value {
    let n_dim = value_int64(dim);
    if !(1..=5).contains(&n_dim) {
        return Value::Null;
    }
    let n_dim = n_dim as usize;
    let bpc = 8 + 8 * n_dim;
    let data: &[u8] = match data {
        Value::Blob(b) => b,
        Value::Text(t) => t.as_str().as_bytes(),
        Value::Null => return Value::Null,
        _ => return Value::Null,
    };
    if data.len() < 4 {
        return Value::Null;
    }
    let n = read_u16(&data[2..]);
    if data.len() < 4 + n * bpc {
        return Value::Null;
    }
    let mut out = String::new();
    for ii in 0..n {
        let base = 4 + ii * bpc;
        if ii > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{{{}", read_i64(&data[base..])));
        for jj in 0..n_dim * 2 {
            let f = f32::from_bits(read_u32(&data[base + 8 + 4 * jj..]));
            out.push(' ');
            out.push_str(&fmt_g(f as f64, false));
        }
        out.push('}');
    }
    if out.is_empty() {
        Value::Null
    } else {
        Value::build_text(out)
    }
}

scalar!(RtreeDepthFn, RtreeDepthCall, |_conn, args| {
    match args[0].get_value() {
        Value::Blob(b) if b.len() >= 2 => Ok(Value::from_i64(read_u16(b) as i64)),
        _ => Err(LimboError::SqlError(
            "Invalid argument to rtreedepth()".to_string(),
        )),
    }
});

scalar!(RtreeCheckFn, RtreeCheckCall, |conn, args| {
    if args.len() != 1 && args.len() != 2 {
        return Err(LimboError::SqlError(
            "wrong number of arguments to function rtreecheck()".to_string(),
        )
        .into());
    }
    let text = |v: &Value| v.cast_text().unwrap_or_default();
    let (db, tab) = if args.len() == 1 {
        ("main".to_string(), text(args[0].get_value()))
    } else {
        (text(args[0].get_value()), text(args[1].get_value()))
    };
    check_table(conn, &db, &tab)
        .map(|report| Value::build_text(report.unwrap_or_else(|| "ok".into())))
});

const RTREE_CHECK_MAX_ERROR: usize = 100;

struct Check<'a> {
    conn: &'a Arc<Connection>,
    db: String,
    tab: String,
    int: bool,
    n_dim: usize,
    n_leaf: i64,
    n_nonleaf: i64,
    report: Vec<String>,
    n_err: usize,
}

impl Check<'_> {
    fn msg(&mut self, m: String) {
        if self.n_err < RTREE_CHECK_MAX_ERROR {
            self.report.push(m);
        }
        self.n_err += 1;
    }

    fn table(&self, suffix: &str) -> String {
        format!(
            "{}.{}",
            quote_ident(&self.db),
            quote_ident(&format!("{}{suffix}", self.tab))
        )
    }

    fn get_node(&mut self, inode: i64) -> Result<Option<Vec<u8>>> {
        let sql = format!("SELECT data FROM {} WHERE nodeno=?", self.table("_node"));
        let rows = shadow::query_all(self.conn, &sql, &[Value::from_i64(inode)])?;
        let node = rows.into_iter().next().and_then(|r| r.into_iter().next());
        match node {
            Some(Value::Blob(b)) => Ok(Some(b.to_vec())),
            Some(Value::Text(t)) => Ok(Some(t.as_str().as_bytes().to_vec())),
            Some(Value::Null) => Ok(Some(Vec::new())),
            Some(v) => Ok(Some(v.cast_text().unwrap_or_default().into_bytes())),
            None => {
                self.msg(format!("Node {inode} missing from database"));
                Ok(None)
            }
        }
    }

    fn check_mapping(&mut self, leaf: bool, key: i64, val: i64) -> Result<()> {
        let sql = if leaf {
            format!("SELECT nodeno FROM {} WHERE rowid=?1", self.table("_rowid"))
        } else {
            format!(
                "SELECT parentnode FROM {} WHERE nodeno=?1",
                self.table("_parent")
            )
        };
        let rows = shadow::query_all(self.conn, &sql, &[Value::from_i64(key)])?;
        let name = if leaf { "%_rowid" } else { "%_parent" };
        match rows.first().and_then(|r| r.first()) {
            None => self.msg(format!(
                "Mapping ({key} -> {val}) missing from {name} table"
            )),
            Some(v) => {
                let ii = value_int64(v);
                if ii != val {
                    self.msg(format!(
                        "Found ({key} -> {ii}) in {name} table, expected ({key} -> {val})"
                    ));
                }
            }
        }
        Ok(())
    }

    fn check_cell_coord(&mut self, inode: i64, icell: usize, cell: &[u8], parent: Option<&[u8]>) {
        let int = self.int;
        let gt = |a: u32, b: u32| {
            if int {
                (a as i32) > (b as i32)
            } else {
                f32::from_bits(a) > f32::from_bits(b)
            }
        };
        for i in 0..self.n_dim {
            let c1 = read_u32(&cell[8 * i..]);
            let c2 = read_u32(&cell[8 * i + 4..]);
            if gt(c1, c2) {
                self.msg(format!(
                    "Dimension {i} of cell {icell} on node {inode} is corrupt"
                ));
            }
            if let Some(p) = parent {
                let p1 = read_u32(&p[8 * i..]);
                let p2 = read_u32(&p[8 * i + 4..]);
                if gt(p1, c1) || gt(c2, p2) {
                    self.msg(format!(
                        "Dimension {i} of cell {icell} on node {inode} is corrupt relative to parent"
                    ));
                }
            }
        }
    }

    fn check_node(&mut self, depth: i32, parent: Option<&[u8]>, inode: i64) -> Result<()> {
        let Some(node) = self.get_node(inode)? else {
            return Ok(());
        };
        if node.len() < 4 {
            self.msg(format!("Node {inode} is too small ({} bytes)", node.len()));
            return Ok(());
        }
        let mut depth = depth;
        if parent.is_none() {
            depth = read_u16(&node) as i32;
            if depth > RTREE_MAX_DEPTH as i32 {
                self.msg(format!("Rtree depth out of range ({depth})"));
                return Ok(());
            }
        }
        let n_cell = read_u16(&node[2..]);
        let bpc = 8 + self.n_dim * 2 * 4;
        if 4 + n_cell * bpc > node.len() {
            self.msg(format!(
                "Node {inode} is too small for cell count of {n_cell} ({} bytes)",
                node.len()
            ));
            return Ok(());
        }
        for i in 0..n_cell {
            let cell = &node[4 + i * bpc..4 + (i + 1) * bpc];
            let val = read_i64(cell);
            self.check_cell_coord(inode, i, &cell[8..], parent);
            if depth > 0 {
                self.check_mapping(false, val, inode)?;
                self.check_node(depth - 1, Some(&cell[8..]), val)?;
                self.n_nonleaf += 1;
            } else {
                self.check_mapping(true, val, inode)?;
                self.n_leaf += 1;
            }
        }
        Ok(())
    }

    fn check_count(&mut self, suffix: &str, expect: i64) -> Result<()> {
        let sql = format!("SELECT count(*) FROM {}", self.table(suffix));
        let rows = shadow::query_all(self.conn, &sql, &[])?;
        if let Some(v) = rows.first().and_then(|r| r.first()) {
            let actual = value_int64(v);
            if actual != expect {
                self.msg(format!(
                    "Wrong number of entries in %{suffix} table - expected {expect}, actual {actual}"
                ));
            }
        }
        Ok(())
    }
}

/// Column count and first row of `sql`, or `None` if it does not prepare.
fn probe(conn: &Arc<Connection>, sql: &str) -> Result<Option<(usize, Option<Vec<Value>>)>> {
    let mut stmt = match shadow::prepare(conn, sql) {
        Ok(stmt) => stmt,
        Err(_) => return Ok(None),
    };
    let n = stmt.num_columns();
    let mut first = None;
    let r = shadow::run(conn, &mut stmt, &[], |row| {
        first = Some(row.get_values().cloned().collect());
        Ok(false)
    });
    conn.start_nested();
    drop(stmt);
    conn.end_nested();
    // As in rtreeCheckTable, corruption found here is reported by the checks.
    match r {
        Err(LimboError::Corrupt(_)) => Ok(Some((n, None))),
        r => r.map(|_| Some((n, first))),
    }
}

/// `rtreeCheckTable`: the report, or `None` when the table is sound.
pub(crate) fn check_table(conn: &Arc<Connection>, db: &str, tab: &str) -> Result<Option<String>> {
    let mut check = Check {
        conn,
        db: db.to_string(),
        tab: tab.to_string(),
        int: false,
        n_dim: 0,
        n_leaf: 0,
        n_nonleaf: 0,
        report: Vec::new(),
        n_err: 0,
    };
    let logic_error = || LimboError::SqlError("SQL logic error".to_string());
    let n_aux = match probe(conn, &format!("SELECT * FROM {}", check.table("_rowid")))? {
        Some((n, _)) => n as i64 - 2,
        None => 0,
    };
    let sql = format!("SELECT * FROM {}.{}", quote_ident(db), quote_ident(tab));
    let Some((n_col, first)) = probe(conn, &sql)? else {
        return Err(logic_error());
    };
    let n_dim = (n_col as i64 - 1 - n_aux) / 2;
    if n_dim < 1 {
        check.msg("Schema corrupt or not an rtree".to_string());
    } else {
        check.n_dim = n_dim as usize;
        if let Some(row) = first {
            check.int = matches!(row.get(1), Some(Value::Numeric(crate::Numeric::Integer(_))));
        }
        check.check_node(0, None, 1)?;
        let n_leaf = check.n_leaf;
        let n_nonleaf = check.n_nonleaf;
        check.check_count("_rowid", n_leaf)?;
        check.check_count("_parent", n_nonleaf)?;
    }
    Ok((!check.report.is_empty()).then(|| check.report.join("\n")))
}

fn native(
    name: &str,
    arity: FunctionArity,
    f: impl ScalarFunction + 'static,
) -> Result<ExternalFunc> {
    ExternalFunc::new_native_scalar(name.to_string(), arity, false, f)
}

scalar!(VtabIntegrityFn, VtabIntegrityCall, |conn, args| {
    let name = args[0].get_value().cast_text().unwrap_or_default();
    let quick = super::value_int64(args[1].get_value()) != 0;
    let table = conn
        .schema
        .read()
        .get_table(crate::NamespaceId::ROOT, &name);
    let report = match table.as_deref() {
        Some(crate::schema::Table::Virtual(vtab)) => vtab.integrity(conn, quick)?,
        _ => None,
    };
    Ok(report.map_or(Value::Null, Value::build_text))
});

/// The function PRAGMA integrity_check calls for each virtual table.
pub(crate) const VTAB_INTEGRITY_FUNCTION: &str = "__turso_internal_vtab_integrity";

pub(crate) fn register_functions(syms: &mut SymbolTable) -> Result<()> {
    let mut funcs = vec![
        native(
            VTAB_INTEGRITY_FUNCTION,
            FunctionArity::Exact(2),
            VtabIntegrityFn,
        )?,
        native("rtreenode", FunctionArity::Exact(2), RtreeNodeFn)?,
        native("rtreedepth", FunctionArity::Exact(1), RtreeDepthFn)?,
        native("rtreecheck", FunctionArity::Variadic, RtreeCheckFn)?,
    ];
    funcs.extend(super::geopoly::functions()?);
    for f in funcs {
        syms.functions.insert(f.name.clone(), Arc::new(f));
    }
    Ok(())
}
