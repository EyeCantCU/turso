//! SQLite's FTS5 full-text search extension, with SQLite's on-disk format.
//!
//! The `fts5` virtual table keeps its index in the shadow tables %_data,
//! %_idx, %_content, %_docsize and %_config exactly as SQLite does, so FTS5
//! tables can move between SQLite and turso in either direction.

pub mod api;
mod aux;
mod config;
mod expr;
mod hash;
mod index;
mod sql;
mod storage;
mod tokenize;
mod unicode;
mod unicode_data;
mod varint;
mod vocab;

pub(crate) use aux::{ApiError, ApiResult, AuxApi};

use crate::connection::SymbolTable;
use crate::function::ExternalFunc;
use crate::native_ext::{
    ConnectInfo, ConstraintInfo, ConstraintUsage, FunctionArity, IndexInfo, OrderByInfo,
    ScalarCall, ScalarFunction, TableUpdate, VTabKind, VirtualTable, VirtualTableCursor,
    VirtualTableModule,
};
use crate::sync::Weak;
use crate::sync::{Arc, Mutex};
use crate::types::IOResultOr;
use crate::{Connection, IOResult, LimboError, Register, Result, Value};
use config::{numeric_int, parse_rank, value_text, ConfigValues, Content, Detail, Fts5Config};
use expr::{Evaluator, Expr};
use index::Index;
use sql::err;
use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::LazyLock;
use storage::{Storage, Writes};
use turso_ext::{ConstraintOp, ResultCode};

const FTS5_SOURCE_ID: &str = "turso fts5";
const SMALLEST_INT64: i64 = i64::MIN;
const LARGEST_INT64: i64 = i64::MAX;
const BI_ORDER_RANK: i32 = 0x01;
const BI_ORDER_ROWID: i32 = 0x02;
const BI_ORDER_DESC: i32 = 0x04;

/// Registers the fts5 and fts5vocab modules and the FTS5 SQL functions.
pub(crate) fn register(syms: &mut SymbolTable) {
    let module = |m: crate::ext::ModuleImplementation| {
        Arc::new(crate::ext::VTabImpl {
            module_kind: VTabKind::VirtualTable,
            implementation: m,
            eponymous: None,
        })
    };
    syms.vtab_modules.insert(
        "fts5".to_string(),
        module(crate::ext::ModuleImplementation::Native(Arc::new(
            Fts5Module,
        ))),
    );
    syms.vtab_modules.insert(
        "fts5vocab".to_string(),
        module(crate::ext::ModuleImplementation::Native(Arc::new(
            vocab::VocabModule,
        ))),
    );
    for name in ["bm25", "highlight", "snippet", "fts5_get_locale"] {
        if let Ok(f) = ExternalFunc::new_native_scalar(
            name.to_string(),
            FunctionArity::Variadic,
            false,
            AuxFunction {
                name: name.to_string(),
            },
        ) {
            syms.functions.insert(name.to_string(), Arc::new(f));
        }
    }
    let simple: [(&'static str, FunctionArity); 4] = [
        ("fts5", FunctionArity::Exact(1)),
        ("fts5_source_id", FunctionArity::Exact(0)),
        ("fts5_locale", FunctionArity::Exact(2)),
        ("fts5_insttoken", FunctionArity::Exact(1)),
    ];
    for (name, arity) in simple {
        if let Ok(f) =
            ExternalFunc::new_native_scalar(name.to_string(), arity, false, SimpleFunction { name })
        {
            syms.functions.insert(name.to_string(), Arc::new(f));
        }
    }
}

/// Module arguments as text, trimmed like SQLite's tokens.
fn args_text(args: &[Value]) -> Vec<String> {
    args.iter()
        .map(|a| match a {
            Value::Text(t) => t.as_str().trim().to_string(),
            other => other.to_string(),
        })
        .collect()
}

#[derive(Debug)]
pub(crate) struct Fts5Module;

impl VirtualTableModule for Fts5Module {
    type Table = Fts5Table;

    fn schema(&self, args: &[Value]) -> Result<String> {
        let cfg = Fts5Config::parse("main", "x", &args_text(args)).map_err(err)?;
        Ok(cfg.declare_vtab())
    }

    fn create(&self, args: &[Value]) -> Result<Fts5Table> {
        let cfg = Fts5Config::parse("main", "x", &args_text(args)).map_err(err)?;
        Ok(Fts5Table::new(cfg))
    }

    fn connect(&self, info: &ConnectInfo, args: &[Value]) -> Result<(Fts5Table, String)> {
        let cfg =
            Fts5Config::parse(info.schema_name, info.table_name, &args_text(args)).map_err(err)?;
        if info.create || cfg.preload_tokenizer() {
            cfg.tokenizer().map_err(err)?;
        }
        if info.create {
            if let Some(conn) = info.connection {
                Storage::create_tables(conn, &cfg)?;
            }
        }
        let schema = cfg.declare_vtab();
        Ok((Fts5Table::new(cfg), schema))
    }

    fn innocuous(&self) -> bool {
        true
    }
}

/// State shared by the table object and its cursors.
pub(crate) struct TableInner {
    pub(crate) cfg: Fts5Config,
    state: Mutex<TableState>,
}

impl std::fmt::Debug for TableInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TableInner")
            .field("cfg", &self.cfg)
            .finish()
    }
}

#[derive(Default)]
struct TableState {
    vals: Option<ConfigValues>,
    /// Pending writes, per connection.
    writes: HashMap<usize, Writes>,
}

fn conn_key(conn: &Arc<Connection>) -> usize {
    Arc::as_ptr(conn) as usize
}

impl TableInner {
    /// The %_config settings, reloaded when the structure cookie changes.
    pub(crate) fn vals(&self, conn: &Arc<Connection>) -> Result<ConfigValues> {
        let store = index::Store::new(conn, &self.cfg);
        let data = store
            .read(index::STRUCTURE_ROWID)?
            .ok_or_else(sql::corrupt)?;
        let cookie = varint::get_u32(&data, 0) as i32;
        {
            let st = self.state.lock();
            if let Some(v) = &st.vals {
                if v.cookie == cookie {
                    return Ok(v.clone());
                }
            }
        }
        if index::Structure::decode(&data).is_none() {
            return Err(sql::corrupt_msg(format!(
                "fts5: corrupt structure record for table \"{}\"",
                self.cfg.name
            )));
        }
        let vals = Storage::load_config(conn, &self.cfg, cookie)?;
        self.state.lock().vals = Some(vals.clone());
        Ok(vals)
    }

    fn set_vals(&self, vals: ConfigValues) {
        self.state.lock().vals = Some(vals);
    }

    fn take_writes(&self, conn: &Arc<Connection>) -> Writes {
        let key = conn_key(conn);
        self.state
            .lock()
            .writes
            .remove(&key)
            .unwrap_or_else(|| Writes {
                pending: index::Pending::new(self.cfg.detail),
                totals: None,
            })
    }

    fn put_writes(&self, conn: &Arc<Connection>, w: Writes) {
        if w.pending.has_data() || w.totals.is_some() || w.pending.started {
            self.state.lock().writes.insert(conn_key(conn), w);
        }
    }

    /// A copy of this connection's pending writes, for readers.
    fn peek_writes(&self, conn: &Arc<Connection>) -> Option<Writes> {
        self.state.lock().writes.get(&conn_key(conn)).cloned()
    }

    fn discard_all(&self) {
        let mut st = self.state.lock();
        st.writes.clear();
        st.vals = None;
    }

    /// Flushes this connection's pending writes (xSync / savepoints).
    fn sync(&self, conn: &Arc<Connection>) -> Result<()> {
        let mut w = self.take_writes(conn);
        if !w.pending.has_data() && w.totals.is_none() {
            return Ok(());
        }
        let vals = self.vals(conn)?;
        let st = Storage::new(conn, &self.cfg, &vals);
        let r = st.sync(&mut w);
        if st.index.version_bumped.get() {
            self.state.lock().vals = None;
        }
        if r.is_err() {
            self.put_writes(conn, w);
        }
        r
    }
}

#[derive(Debug)]
pub(crate) struct Fts5Table {
    inner: Arc<TableInner>,
}

impl Fts5Table {
    fn new(cfg: Fts5Config) -> Self {
        Self {
            inner: Arc::new(TableInner {
                cfg,
                state: Mutex::new(TableState::default()),
            }),
        }
    }
}

impl VirtualTable for Fts5Table {
    type Cursor = Fts5Cursor;

    fn open(&self, connection: Arc<Connection>) -> Result<Self::Cursor> {
        let id = NEXT_CURSOR_ID.fetch_add(1, Ordering::Relaxed);
        let state = Arc::new(Mutex::new(CursorState::new(
            id,
            self.inner.clone(),
            connection,
        )));
        CURSORS.lock().insert(id, Arc::downgrade(&state));
        Ok(Fts5Cursor { state, id })
    }

    /// fts5BestIndexMethod
    fn best_index(
        &self,
        constraints: &[ConstraintInfo],
        order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let cfg = &self.inner.cfg;
        let ncol = cfg.ncol() as i64;
        let pattern = cfg.pattern();
        let mut usages = vec![
            ConstraintUsage {
                argv_index: None,
                omit: false,
            };
            constraints.len()
        ];
        let mut idx_str = String::new();
        let mut ncons = 0u32;
        let mut seen_match = 0;
        let mut seen_rank = false;
        for (i, c) in constraints.iter().enumerate() {
            let col = c.column_index as i64;
            let is_rowid = c.column_index == u32::MAX;
            if c.op == ConstraintOp::Match || (c.op == ConstraintOp::Eq && !is_rowid && col >= ncol)
            {
                if !c.usable || is_rowid {
                    return Err(ResultCode::ConstraintViolation);
                }
                if col == ncol + 1 {
                    if seen_rank {
                        continue;
                    }
                    idx_str.push('r');
                    seen_rank = true;
                } else {
                    seen_match += 1;
                    idx_str.push('M');
                    idx_str.push_str(&col.to_string());
                }
                ncons += 1;
                usages[i] = ConstraintUsage {
                    argv_index: Some(ncons),
                    omit: true,
                };
            } else if c.usable
                && !is_rowid
                && col < ncol
                && ((pattern == tokenize::Pattern::Glob && c.op == ConstraintOp::Glob)
                    || (pattern == tokenize::Pattern::Like
                        && matches!(c.op, ConstraintOp::Like | ConstraintOp::Glob)))
            {
                idx_str.push(if c.op == ConstraintOp::Like { 'L' } else { 'G' });
                idx_str.push_str(&col.to_string());
                ncons += 1;
                usages[i].argv_index = Some(ncons);
                seen_match += 1;
            }
        }
        let mut seen_eq = false;
        let mut seen_lt = false;
        let mut seen_gt = false;
        for (i, c) in constraints.iter().enumerate() {
            if c.column_index == u32::MAX && c.usable && !seen_eq && c.op == ConstraintOp::Eq {
                idx_str.push('=');
                seen_eq = true;
                ncons += 1;
                usages[i] = ConstraintUsage {
                    argv_index: Some(ncons),
                    omit: true,
                };
            }
        }
        if !seen_eq {
            for (i, c) in constraints.iter().enumerate() {
                if c.column_index != u32::MAX || !c.usable {
                    continue;
                }
                match c.op {
                    ConstraintOp::Lt | ConstraintOp::Le if !seen_lt => {
                        idx_str.push('<');
                        seen_lt = true;
                    }
                    ConstraintOp::Gt | ConstraintOp::Ge if !seen_gt => {
                        idx_str.push('>');
                        seen_gt = true;
                    }
                    _ => continue,
                }
                ncons += 1;
                usages[i].argv_index = Some(ncons);
            }
        }
        let mut idx_num = 0;
        let mut order_by_consumed = false;
        if order_by.len() == 1 {
            let sort = order_by[0].column_index as i64;
            if sort == ncol + 1 && seen_match > 0 {
                idx_num |= BI_ORDER_RANK;
            } else if order_by[0].column_index == u32::MAX && (!order_by[0].desc || !cfg.tokendata)
            {
                idx_num |= BI_ORDER_ROWID;
            }
            if idx_num & (BI_ORDER_RANK | BI_ORDER_ROWID) != 0 {
                order_by_consumed = true;
                if order_by[0].desc {
                    idx_num |= BI_ORDER_DESC;
                }
            }
        }
        let (cost, rows) = if seen_eq {
            (if seen_match > 0 { 25000.0 } else { 25.0 }, 1)
        } else if seen_match > 0 {
            let mut cost: f64 = if seen_lt && seen_gt {
                50000.0
            } else if seen_lt || seen_gt {
                37500.0
            } else {
                50000.0
            };
            let mut rows = (cost / 40.0) as i64;
            for _ in 1..seen_match {
                cost *= 2.5;
                rows /= 2;
            }
            (cost, rows)
        } else {
            let cost: f64 = if seen_lt && seen_gt {
                750000.0
            } else if seen_lt || seen_gt {
                2250000.0
            } else {
                3000000.0
            };
            (cost, (cost / 4.0) as i64)
        };
        Ok(IndexInfo {
            idx_num,
            idx_str: Some(idx_str),
            order_by_consumed,
            estimated_cost: cost,
            estimated_rows: rows.clamp(1, u32::MAX as i64 - 1) as u32,
            constraint_usages: usages,
        })
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
        Ok(Box::new(Fts5Update {
            inner: self.inner.clone(),
            conn: connection.clone(),
            args,
            on_conflict,
        }))
    }

    fn sync(&mut self, connection: &Arc<Connection>) -> Result<()> {
        self.inner.sync(connection)
    }

    fn savepoint_with_connection(
        &mut self,
        connection: &Arc<Connection>,
        _level: usize,
    ) -> Result<()> {
        self.inner.sync(connection)
    }

    fn rollback_to(&mut self, _level: usize) -> Result<()> {
        self.inner.discard_all();
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        self.inner.discard_all();
        Ok(())
    }

    fn destroy_with_connection(&mut self, connection: &Arc<Connection>) -> Result<()> {
        self.inner.discard_all();
        Storage::drop_tables(connection, &self.inner.cfg)
    }

    fn rename_with_connection(&mut self, connection: &Arc<Connection>, name: &str) -> Result<()> {
        self.inner.sync(connection)?;
        Storage::rename_tables(connection, &self.inner.cfg, name)
    }
}

// ------------------------------------------------------------------ writes

struct Fts5Update {
    inner: Arc<TableInner>,
    conn: Arc<Connection>,
    args: Vec<Value>,
    on_conflict: u8,
}

const OE_REPLACE: u8 = 5;

impl TableUpdate for Fts5Update {
    fn step(&mut self) -> IOResultOr<Option<i64>> {
        let mut w = self.inner.take_writes(&self.conn);
        let r = self.run(&mut w);
        self.inner.put_writes(&self.conn, w);
        Ok(IOResult::Done(r?))
    }
}

impl Fts5Update {
    /// fts5UpdateMethod
    fn run(&self, w: &mut Writes) -> Result<Option<i64>> {
        let inner = &self.inner;
        let cfg = &inner.cfg;
        let vals = inner.vals(&self.conn)?;
        let st = Storage::new(&self.conn, cfg, &vals);
        let args = &self.args;
        let ncol = cfg.ncol();
        let is_delete = args.len() == 2;
        if !is_delete && args.len() != ncol + 4 {
            return Err(err("wrong number of arguments to fts5 xUpdate"));
        }
        if !is_delete && args[0] == Value::Null && args[2 + ncol] != Value::Null {
            let cmd = value_text(&args[2 + ncol]).unwrap_or_default();
            let cmd = String::from_utf8_lossy(&cmd).into_owned();
            if cfg.content != Content::Normal && cmd.eq_ignore_ascii_case("delete") {
                if cfg.contentless_delete {
                    return Err(err(
                        "'delete' may not be used with a contentless_delete=1 table",
                    ));
                }
                if let Value::Numeric(crate::numeric::Numeric::Integer(rowid)) = args[1] {
                    st.delete(w, rowid, Some(&args[2..2 + ncol]))?;
                }
                return Ok(Some(0));
            }
            special_insert(inner, &st, w, &cmd, &args[3 + ncol])?;
            if st.index.version_bumped.get() {
                inner.state.lock().vals = None;
            }
            return Ok(Some(0));
        }
        let conflict_replace = (cfg.content == Content::Normal || cfg.contentless_delete)
            && self.on_conflict == OE_REPLACE;
        if is_delete {
            if cfg.is_contentless(true) && !cfg.contentless_delete {
                return Err(err(format!(
                    "cannot DELETE from contentless fts5 table: {}",
                    cfg.name
                )));
            }
            let rowid = sql::value_i64(&args[0]);
            st.delete(w, rowid, None)?;
            return Ok(None);
        }
        if !cfg.locale && args[2..2 + ncol].iter().any(storage::is_locale_value) {
            return Err(LimboError::SqlError(
                "fts5_locale() requires locale=1".into(),
            ));
        }
        let new_int = numeric_int(&args[1]);
        if args[0] == Value::Null {
            // INSERT
            if conflict_replace {
                if let Some(r) = new_int {
                    st.delete(w, r, None)?;
                }
            }
            let rowid = st.content_insert(false, args, None)?;
            st.index_insert(w, args, rowid)?;
            return Ok(Some(rowid));
        }
        // UPDATE
        let old = sql::value_i64(&args[0]);
        let Some(new) = new_int else {
            return Err(LimboError::SqlError("datatype mismatch".into()));
        };
        let mut content_only = false;
        if cfg.is_contentless(true) {
            // Turso passes every column, so all indexed columns count as modified.
            let changed_indexed = cfg.unindexed.iter().any(|u| !u);
            if !changed_indexed && old == new {
                content_only = true;
            } else if !cfg.contentless_delete {
                return Err(err(format!(
                    "cannot UPDATE contentless fts5 table: {}",
                    cfg.name
                )));
            }
        }
        if old != new {
            if conflict_replace {
                st.delete(w, old, None)?;
                st.delete(w, new, None)?;
                let rowid = st.content_insert(false, args, None)?;
                st.index_insert(w, args, rowid)?;
            } else {
                let rowid = st.content_insert(false, args, None)?;
                st.delete(w, old, None)?;
                st.index_insert(w, args, rowid)?;
            }
        } else if content_only {
            st.content_insert(true, args, None)?;
        } else {
            st.delete(w, old, None)?;
            let rowid = st.content_insert(false, args, None)?;
            st.index_insert(w, args, rowid)?;
        }
        Ok(None)
    }
}

/// fts5SpecialInsert: the INSERT INTO t(t) VALUES('command') commands.
fn special_insert(
    inner: &TableInner,
    st: &Storage,
    w: &mut Writes,
    cmd: &str,
    val: &Value,
) -> Result<()> {
    let cfg = &inner.cfg;
    let c = cmd.to_ascii_lowercase();
    match c.as_str() {
        "delete-all" => {
            if cfg.content == Content::Normal {
                return Err(err(
                    "'delete-all' may only be used with a contentless or external content fts5 table",
                ));
            }
            st.delete_all(w)?;
            inner.state.lock().vals = None;
        }
        "rebuild" => {
            if cfg.is_contentless(true) {
                return Err(err(
                    "'rebuild' may not be used with a contentless fts5 table",
                ));
            }
            st.rebuild(w)?;
            inner.state.lock().vals = None;
        }
        "optimize" => {
            st.sync(w)?;
            st.index.optimize(&mut w.pending)?;
        }
        "merge" => {
            st.sync(w)?;
            let n = sql::value_i64(val).clamp(i32::MIN as i64, i32::MAX as i64);
            st.index.user_merge(&mut w.pending, n)?;
        }
        "integrity-check" => {
            st.integrity(Some(w), sql::value_i64(val))?;
        }
        "flush" => {
            st.sync(w)?;
        }
        _ => {
            st.sync(w)?;
            let mut vals = inner.vals(st.conn)?;
            if !vals.set(cmd, val) {
                return Err(err("SQL logic error"));
            }
            let cookie = st.config_value(cmd, val)?;
            vals.cookie = cookie;
            inner.set_vals(vals);
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ cursors

static NEXT_CURSOR_ID: AtomicI64 = AtomicI64::new(1);
/// Tables found through their `*id` special query (used by fts5vocab).
static TABLES_BY_ID: LazyLock<Mutex<HashMap<i64, Weak<TableInner>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn table_by_special_id(id: i64) -> Option<Arc<TableInner>> {
    TABLES_BY_ID.lock().get(&id).and_then(|w| w.upgrade())
}
static CURSORS: LazyLock<Mutex<HashMap<i64, Weak<Mutex<CursorState>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    None,
    Match,
    Scan,
    Rowid,
    Special,
}

pub(crate) struct CursorState {
    id: i64,
    inner: Arc<TableInner>,
    conn: Arc<Connection>,
    plan: Plan,
    rows: Vec<i64>,
    pos: usize,
    desc: bool,
    expr: Option<Expr>,
    eval: Option<Evaluator>,
    special: i64,
    rank: Option<(String, Vec<Value>)>,
    vals: Option<ConfigValues>,
    content: Option<Option<Vec<Value>>>,
    docsize: Option<Vec<i64>>,
    poslists: Option<Vec<Vec<u8>>>,
    insts: Option<Vec<(i32, i32, i32)>>,
    auxdata: HashMap<String, Box<dyn Any + Send>>,
    current_aux: String,
    scan_cache: HashMap<i64, Vec<Value>>,
}

impl CursorState {
    fn new(id: i64, inner: Arc<TableInner>, conn: Arc<Connection>) -> Self {
        CursorState {
            id,
            inner,
            conn,
            plan: Plan::None,
            rows: Vec::new(),
            pos: 0,
            desc: false,
            expr: None,
            eval: None,
            special: 0,
            rank: None,
            vals: None,
            content: None,
            docsize: None,
            poslists: None,
            insts: None,
            auxdata: HashMap::new(),
            current_aux: String::new(),
            scan_cache: HashMap::new(),
        }
    }

    fn cfg(&self) -> &Fts5Config {
        &self.inner.cfg
    }

    fn new_row(&mut self) {
        self.content = None;
        self.docsize = None;
        self.poslists = None;
        self.insts = None;
    }

    fn eof(&self) -> bool {
        match (&self.plan, &self.eval) {
            (Plan::Match, Some(ev)) => ev.eof(),
            _ => self.pos >= self.rows.len(),
        }
    }

    fn rowid(&self) -> i64 {
        match (&self.plan, &self.eval) {
            (Plan::Special, _) => 0,
            (Plan::Match, Some(ev)) => ev.rowid(),
            _ => self.rows.get(self.pos).copied().unwrap_or(0),
        }
    }

    fn vals(&mut self) -> Result<ConfigValues> {
        if self.vals.is_none() {
            self.vals = Some(self.inner.vals(&self.conn)?);
        }
        Ok(self.vals.clone().unwrap())
    }

    /// fts5FilterMethod
    fn filter(&mut self, args: &[Value], idx_str: Option<&str>, idx_num: i32) -> Result<bool> {
        self.plan = Plan::None;
        self.rows.clear();
        self.pos = 0;
        self.expr = None;
        self.eval = None;
        self.rank = None;
        self.vals = None;
        self.auxdata.clear();
        self.scan_cache.clear();
        self.new_row();
        let idx = idx_str.unwrap_or("").as_bytes();
        let mut k = 0usize;
        let mut rank_arg: Option<Value> = None;
        let (mut eq, mut le, mut ge) = (None, None, None);
        let vals = self.vals()?;
        let cfg = &self.inner.cfg;
        let mut expr: Option<Expr> = None;
        let read_col = |k: &mut usize| {
            let mut col = 0usize;
            while *k < idx.len() && idx[*k].is_ascii_digit() {
                col = col * 10 + (idx[*k] - b'0') as usize;
                *k += 1;
            }
            col
        };
        for v in args {
            let c = idx.get(k).copied().unwrap_or(b'>');
            k += 1;
            match c {
                b'r' => rank_arg = Some(v.clone()),
                b'M' => {
                    let (text, locale) = match storage::decode_locale(v) {
                        Some((loc, text)) if cfg.locale => (text, Some(loc)),
                        _ => (value_text(v).unwrap_or_default(), None),
                    };
                    let col = read_col(&mut k);
                    if text.first() == Some(&b'*') {
                        return self.special_match(&text[1..]);
                    }
                    let e = Expr::parse(cfg, false, col, &text, locale.as_deref()).map_err(err)?;
                    expr = Some(match expr {
                        None => e,
                        Some(prev) => prev.and(e),
                    });
                }
                b'L' | b'G' => {
                    let col = read_col(&mut k);
                    if let Some(text) = value_text(v) {
                        if let Some(e) =
                            expr::pattern_expr(cfg, c == b'G', col, &text).map_err(err)?
                        {
                            expr = Some(match expr {
                                None => e,
                                Some(prev) => prev.and(e),
                            });
                        }
                    }
                }
                b'=' => eq = Some(v.clone()),
                b'<' => le = Some(v.clone()),
                _ => ge = Some(v.clone()),
            }
        }
        self.desc = idx_num & BI_ORDER_DESC != 0;
        if eq.is_some() {
            le.clone_from(&eq);
            ge.clone_from(&eq);
        }
        let limit = |v: &Option<Value>, d: i64| v.as_ref().and_then(numeric_int).unwrap_or(d);
        let first = limit(&ge, SMALLEST_INT64);
        let last = limit(&le, LARGEST_INT64);
        if let Some(e) = expr {
            // Rank function: rank MATCH arg, config, or bm25.
            let rank = match &rank_arg {
                Some(Value::Null) => return Err(err("parse error in rank function: ")),
                Some(r) => {
                    let t =
                        String::from_utf8_lossy(&value_text(r).unwrap_or_default()).into_owned();
                    Some(
                        parse_rank(&t)
                            .ok_or_else(|| err(format!("parse error in rank function: {t}")))?,
                    )
                }
                None => vals.rank.clone(),
            };
            let (name, rargs) = rank.unwrap_or_else(|| (config::DEFAULT_RANK.to_string(), None));
            let rank_vals = match rargs {
                Some(a) => {
                    sql::query_row(&self.conn, &format!("SELECT {a}"), &[])?.unwrap_or_default()
                }
                None => Vec::new(),
            };
            self.rank = Some((name, rank_vals));
            let w = self.inner.peek_writes(&self.conn);
            let index = Index::new(&self.conn, cfg, &vals);
            let (s, _) = index.store.read_structure()?;
            let ev = Evaluator::new(
                &e,
                &index,
                &s,
                w.as_ref().map(|w| &w.pending),
                (first, last),
            )?;
            self.eval = Some(ev);
            self.expr = Some(e);
            self.plan = Plan::Match;
        } else if cfg.content_table.is_none() {
            return Err(err(format!(
                "{}: table does not support scanning",
                cfg.name
            )));
        } else {
            self.plan = if eq.is_some() {
                Plan::Rowid
            } else {
                Plan::Scan
            };
            let table = cfg.content_table.clone().unwrap();
            let rowid_col = config::quote(&cfg.content_rowid);
            let rows = if let Some(eqv) = &eq {
                sql::query(
                    &self.conn,
                    &format!(
                        "SELECT {} FROM {} T WHERE T.{}=?",
                        cfg.content_exprlist, table, rowid_col
                    ),
                    &[eqv.clone()],
                )?
            } else {
                sql::query(
                    &self.conn,
                    &format!(
                        "SELECT {} FROM {} T WHERE T.{rowid_col} >= ? AND T.{rowid_col} <= ? ORDER BY T.{rowid_col} {}",
                        cfg.content_exprlist,
                        table,
                        if self.desc { "DESC" } else { "ASC" }
                    ),
                    &[sql::int(first), sql::int(last)],
                )?
            };
            for r in rows {
                let rowid = sql::value_i64(&r[0]);
                self.rows.push(rowid);
                self.scan_cache.insert(rowid, r);
            }
        }
        Ok(!self.eof())
    }

    fn special_match(&mut self, z: &[u8]) -> Result<bool> {
        let s = String::from_utf8_lossy(z);
        let s = s.trim_start_matches(' ');
        let word = s.split(' ').next().unwrap_or("");
        self.plan = Plan::Special;
        if word.eq_ignore_ascii_case("reads") {
            self.special = 0;
        } else if word.eq_ignore_ascii_case("id") {
            self.special = self.id;
            let mut map = TABLES_BY_ID.lock();
            map.retain(|_, w| w.strong_count() > 0);
            map.insert(self.id, Arc::downgrade(&self.inner));
        } else {
            return Err(err(format!("unknown special query: {word}")));
        }
        self.rows = vec![0];
        Ok(true)
    }

    fn next(&mut self) -> bool {
        if self.plan == Plan::Special {
            self.pos = self.rows.len();
            return false;
        }
        if let (Plan::Match, Some(ev)) = (&self.plan, &mut self.eval) {
            ev.next();
        } else {
            self.pos += 1;
        }
        self.new_row();
        !self.eof()
    }

    /// The content row: (rowid, cols..., locales...); None if contentless.
    fn content_row(&mut self) -> Result<Option<Vec<Value>>> {
        if let Some(c) = &self.content {
            return Ok(c.clone());
        }
        let rowid = self.rowid();
        let row = if let Some(r) = self.scan_cache.get(&rowid) {
            Some(r.clone())
        } else if self.cfg().content == Content::None {
            None
        } else {
            let vals = self.vals()?;
            let st = Storage::new(&self.conn, &self.inner.cfg, &vals);
            match st.lookup(rowid)? {
                Some(r) => Some(r),
                None => {
                    return Err(sql::corrupt_msg(format!(
                        "fts5: missing row {} from content table {}",
                        rowid,
                        self.cfg().content_table.clone().unwrap_or_default()
                    )))
                }
            }
        };
        self.content = Some(row.clone());
        Ok(row)
    }

    fn column(&mut self, col: usize) -> Result<Value> {
        let ncol = self.cfg().ncol();
        if self.plan == Plan::Special {
            return Ok(if col == ncol {
                Value::from_i64(self.special)
            } else {
                Value::Null
            });
        }
        if col == ncol {
            return Ok(Value::from_i64(self.id));
        }
        if col == ncol + 1 {
            if self.plan != Plan::Match {
                return Ok(Value::Null);
            }
            let (name, args) = self.rank.clone().unwrap_or_default();
            return self.call_aux(&name, &args);
        }
        if self.cfg().content == Content::None {
            return Ok(Value::Null);
        }
        let Some(row) = self.content_row()? else {
            return Ok(Value::Null);
        };
        let v = row.get(col + 1).cloned().unwrap_or(Value::Null);
        if self.cfg().locale && self.cfg().content == Content::External {
            if let Some((_, text)) = storage::decode_locale(&v) {
                return Ok(Value::build_text(
                    String::from_utf8_lossy(&text).into_owned(),
                ));
            }
        }
        Ok(v)
    }

    /// Runs an auxiliary function on the current row.
    fn call_aux(&mut self, name: &str, args: &[Value]) -> Result<Value> {
        let prev = std::mem::replace(&mut self.current_aux, name.to_ascii_lowercase());
        // Functions registered through fts5_api take precedence, as in SQLite.
        let r = if let Some(f) = api::find_aux(name) {
            f.call(self, args)
        } else {
            match aux::call_builtin(name, self, args) {
                Some(r) => r,
                None => Err(err(format!("no such function: {name}"))),
            }
        };
        self.current_aux = prev;
        r
    }

    fn poslists(&mut self) -> Result<Vec<Vec<u8>>> {
        if let Some(p) = &self.poslists {
            return Ok(p.clone());
        }
        let p: Vec<Option<Vec<u8>>> = match (&self.expr, &self.eval) {
            (Some(e), Some(ev)) => (0..e.phrase_count())
                .map(|i| {
                    ev.phrase_poslist(i)
                        .filter(|p| !p.is_empty())
                        .map(|p| p.to_vec())
                })
                .collect(),
            _ => Vec::new(),
        };
        let p = if self.cfg().detail != Detail::Full {
            self.populate_poslists(&p)?
        } else {
            p.into_iter().map(Option::unwrap_or_default).collect()
        };
        self.poslists = Some(p.clone());
        Ok(p)
    }

    /// For detail!=full, positions come from re-tokenizing the row
    /// (sqlite3Fts5ExprPopulatePoslists). `matched` marks the phrases that
    /// matched this row.
    fn populate_poslists(&mut self, matched: &[Option<Vec<u8>>]) -> Result<Vec<Vec<u8>>> {
        let nphrase = matched.len();
        if self.cfg().is_contentless(true) {
            return Ok(vec![Vec::new(); nphrase]);
        }
        let Some(row) = self.content_row()? else {
            return Ok(vec![Vec::new(); nphrase]);
        };
        let expr = self.expr.clone().expect("expr");
        let cfg = &self.inner.cfg;
        let ncol = cfg.ncol();
        let mut out: Vec<Vec<u8>> = vec![Vec::new(); nphrase];
        let mut writers = vec![varint::PoslistWriter::default(); nphrase];
        for icol in 0..ncol {
            let v = row.get(icol + 1).cloned().unwrap_or(Value::Null);
            let loc_col = (cfg.locale && cfg.content == Content::Normal)
                .then(|| row.get(1 + ncol + icol).cloned())
                .flatten();
            let (text, loc) = match storage::decode_locale(&v) {
                Some((l, t)) if cfg.locale => (t, Some(l)),
                _ => (
                    value_text(&v).unwrap_or_default(),
                    loc_col.and_then(|l| value_text(&l)),
                ),
            };
            let ok: Vec<bool> = (0..nphrase)
                .map(|i| {
                    matched[i].is_some()
                        && expr
                            .phrase_nearset(i)
                            .and_then(|n| n.colset.as_ref())
                            .is_none_or(|c| c.contains(&icol))
                })
                .collect();
            let mut ioff: i64 = ((icol as i64) << 32) - 1;
            let rc = cfg.tokenizer().map_err(err)?.tokenize(
                tokenize::TOKENIZE_DOCUMENT,
                &text,
                loc.as_deref(),
                &mut |tflags, tok, _, _| {
                    let tok = &tok[..tok.len().min(storage::MAX_TOKEN_SIZE)];
                    if tflags & tokenize::TOKEN_COLOCATED == 0 {
                        ioff += 1;
                    }
                    for i in 0..nphrase {
                        if !ok[i] {
                            continue;
                        }
                        let Some(t) = expr.phrases[i].terms.first() else {
                            continue;
                        };
                        let hit = t.alts.iter().any(|a| {
                            (a.len() == tok.len() || (a.len() < tok.len() && t.prefix))
                                && tok.starts_with(a)
                        });
                        if hit {
                            writers[i].append(&mut out[i], ioff);
                        }
                    }
                    0
                },
            );
            if rc != 0 {
                return Err(err(format!("tokenizer error {rc}")));
            }
        }
        Ok(out)
    }

    fn insts(&mut self) -> Result<Vec<(i32, i32, i32)>> {
        if let Some(i) = &self.insts {
            return Ok(i.clone());
        }
        let pls = self.poslists()?;
        let lists: Vec<Vec<i64>> = pls.iter().map(|p| varint::poslist_positions(p)).collect();
        let mut idx = vec![0usize; lists.len()];
        let mut out = Vec::new();
        loop {
            let mut best: Option<usize> = None;
            for i in 0..lists.len() {
                if idx[i] < lists[i].len()
                    && best.is_none_or(|b| lists[i][idx[i]] < lists[b][idx[b]])
                {
                    best = Some(i);
                }
            }
            let Some(b) = best else { break };
            let p = lists[b][idx[b]];
            let col = varint::pos_col(p);
            if col as usize >= self.cfg().ncol() {
                return Err(sql::corrupt());
            }
            out.push((b as i32, col, varint::pos_off(p)));
            idx[b] += 1;
        }
        self.insts = Some(out.clone());
        Ok(out)
    }

    fn row_docsize(&mut self) -> Result<Vec<i64>> {
        if let Some(d) = &self.docsize {
            return Ok(d.clone());
        }
        let cfg = &self.inner.cfg;
        let ncol = cfg.ncol();
        let sizes = if cfg.columnsize {
            let vals = self.vals()?;
            let st = Storage::new(&self.conn, &self.inner.cfg, &vals);
            st.docsize(self.rowid())?
        } else if cfg.content_table.is_none() || cfg.content == Content::Unindexed {
            (0..ncol)
                .map(|i| if cfg.unindexed[i] { 0 } else { -1 })
                .collect()
        } else {
            let row = self.content_row()?.unwrap_or_default();
            let mut sizes = vec![0i64; ncol];
            for (i, size) in sizes.iter_mut().enumerate() {
                if self.inner.cfg.unindexed[i] {
                    continue;
                }
                let text = row.get(i + 1).and_then(value_text).unwrap_or_default();
                let mut n = 0i64;
                self.inner.cfg.tokenizer().map_err(err)?.tokenize(
                    tokenize::TOKENIZE_AUX,
                    &text,
                    None,
                    &mut |tflags, _, _, _| {
                        if tflags & tokenize::TOKEN_COLOCATED == 0 {
                            n += 1;
                        }
                        0
                    },
                );
                *size = n;
            }
            sizes
        };
        self.docsize = Some(sizes.clone());
        Ok(sizes)
    }
}

impl AuxApi for CursorState {
    fn column_count(&self) -> i32 {
        self.cfg().ncol() as i32
    }

    fn row_count(&mut self) -> ApiResult<i64> {
        let vals = self.vals()?;
        let st = Storage::new(&self.conn, &self.inner.cfg, &vals);
        let w = self.inner.peek_writes(&self.conn);
        let (n, _) = st.totals(w.as_ref())?;
        if n <= 0 {
            return Err(ApiError::Err(sql::corrupt()));
        }
        Ok(n)
    }

    fn column_total_size(&mut self, col: i32) -> ApiResult<i64> {
        let vals = self.vals()?;
        let st = Storage::new(&self.conn, &self.inner.cfg, &vals);
        let w = self.inner.peek_writes(&self.conn);
        let (_, sizes) = st.totals(w.as_ref())?;
        if col < 0 {
            Ok(sizes.iter().sum())
        } else if (col as usize) < sizes.len() {
            Ok(sizes[col as usize])
        } else {
            Err(ApiError::Code(aux::RANGE))
        }
    }

    fn tokenize(
        &mut self,
        text: &[u8],
        locale: Option<&[u8]>,
        cb: &mut tokenize::TokenCallback,
    ) -> ApiResult<()> {
        let tok = self
            .inner
            .cfg
            .tokenizer()
            .map_err(|e| ApiError::Err(err(e)))?;
        let rc = tok.tokenize(tokenize::TOKENIZE_AUX, text, locale, cb);
        if rc != 0 {
            return Err(ApiError::Code(rc));
        }
        Ok(())
    }

    fn phrase_count(&self) -> i32 {
        self.expr.as_ref().map_or(0, |e| e.phrase_count() as i32)
    }

    fn phrase_size(&self, phrase: i32) -> i32 {
        if phrase < 0 {
            return 0;
        }
        self.expr
            .as_ref()
            .map_or(0, |e| e.phrase_size(phrase as usize) as i32)
    }

    fn inst_count(&mut self) -> ApiResult<i32> {
        Ok(self.insts()?.len() as i32)
    }

    fn inst(&mut self, i: i32) -> ApiResult<(i32, i32, i32)> {
        let insts = self.insts()?;
        if i < 0 || i as usize >= insts.len() {
            return Err(ApiError::Code(aux::RANGE));
        }
        Ok(insts[i as usize])
    }

    fn rowid(&self) -> i64 {
        CursorState::rowid(self)
    }

    fn column_text(&mut self, col: i32) -> ApiResult<Option<Vec<u8>>> {
        if col < 0 || col as usize >= self.cfg().ncol() {
            return Err(ApiError::Code(aux::RANGE));
        }
        if self.cfg().is_contentless(false) {
            return Ok(None);
        }
        let Some(row) = self.content_row()? else {
            return Ok(None);
        };
        let v = row.get(col as usize + 1).cloned().unwrap_or(Value::Null);
        if let Some((_, text)) = storage::decode_locale(&v) {
            if self.cfg().locale && self.cfg().content == Content::External {
                return Ok(Some(text));
            }
        }
        Ok(Some(value_text(&v).unwrap_or_default()))
    }

    fn column_size(&mut self, col: i32) -> ApiResult<i32> {
        let sizes = self.row_docsize()?;
        if col < 0 {
            Ok(sizes.iter().sum::<i64>() as i32)
        } else if (col as usize) < sizes.len() {
            Ok(sizes[col as usize] as i32)
        } else {
            Err(ApiError::Code(aux::RANGE))
        }
    }

    fn query_phrase_count(&mut self, phrase: i32) -> ApiResult<i64> {
        let Some(expr) = &self.expr else {
            return Err(ApiError::Code(aux::RANGE));
        };
        if phrase < 0 || phrase as usize >= expr.phrase_count() {
            return Err(ApiError::Code(aux::RANGE));
        }
        let single = expr.single_phrase(phrase as usize);
        let vals = self.vals()?;
        let index = Index::new(&self.conn, &self.inner.cfg, &vals);
        let (s, _) = index.store.read_structure()?;
        let w = self.inner.peek_writes(&self.conn);
        let mut ev = Evaluator::new(
            &single,
            &index,
            &s,
            w.as_ref().map(|w| &w.pending),
            (SMALLEST_INT64, LARGEST_INT64),
        )?;
        let mut n = 0;
        while !ev.eof() {
            n += 1;
            ev.next();
        }
        Ok(n)
    }

    fn column_locale(&mut self, col: i32) -> ApiResult<Option<Vec<u8>>> {
        if col < 0 || col as usize >= self.cfg().ncol() {
            return Err(ApiError::Code(aux::RANGE));
        }
        let cfg = &self.inner.cfg;
        if !cfg.locale || cfg.unindexed[col as usize] || cfg.is_contentless(false) {
            return Ok(None);
        }
        let ncol = cfg.ncol();
        let content = cfg.content;
        let Some(row) = self.content_row()? else {
            return Ok(None);
        };
        if content == Content::External {
            let v = row.get(col as usize + 1).cloned().unwrap_or(Value::Null);
            return Ok(storage::decode_locale(&v)
                .map(|(l, _)| l)
                .filter(|l| !l.is_empty()));
        }
        let v = row
            .get(1 + ncol + col as usize)
            .cloned()
            .unwrap_or(Value::Null);
        Ok(value_text(&v).filter(|l| !l.is_empty()))
    }

    fn auxdata(&mut self) -> Option<&mut Box<dyn Any + Send>> {
        let key = self.current_aux.clone();
        self.auxdata.get_mut(&key)
    }

    fn set_auxdata(&mut self, data: Box<dyn Any + Send>) {
        self.auxdata.insert(self.current_aux.clone(), data);
    }

    fn take_auxdata(&mut self) -> Option<Box<dyn Any + Send>> {
        let key = self.current_aux.clone();
        self.auxdata.remove(&key)
    }

    fn phrase_poslist(&mut self, phrase: i32) -> ApiResult<Vec<u8>> {
        if phrase < 0 || phrase >= self.phrase_count() {
            return Err(ApiError::Code(aux::RANGE));
        }
        Ok(self
            .poslists()?
            .get(phrase as usize)
            .cloned()
            .unwrap_or_default())
    }

    fn phrase_collist(&mut self, phrase: i32) -> ApiResult<Vec<u8>> {
        if phrase < 0 || phrase >= self.phrase_count() {
            return Err(ApiError::Code(aux::RANGE));
        }
        if self.cfg().detail != Detail::Columns {
            return self.phrase_poslist(phrase);
        }
        Ok(self
            .eval
            .as_ref()
            .and_then(|ev| ev.phrase_collist(phrase as usize))
            .unwrap_or_default())
    }

    fn query_phrase(
        &mut self,
        phrase: i32,
        cb: &mut dyn FnMut(&mut dyn AuxApi) -> ApiResult<bool>,
    ) -> ApiResult<()> {
        let Some(expr) = &self.expr else {
            return Err(ApiError::Code(aux::RANGE));
        };
        if phrase < 0 || phrase as usize >= expr.phrase_count() {
            return Err(ApiError::Code(aux::RANGE));
        }
        let single = expr.single_phrase(phrase as usize);
        let vals = self.vals()?;
        let index = Index::new(&self.conn, &self.inner.cfg, &vals);
        let (s, _) = index.store.read_structure()?;
        let w = self.inner.peek_writes(&self.conn);
        let ev = Evaluator::new(
            &single,
            &index,
            &s,
            w.as_ref().map(|w| &w.pending),
            (SMALLEST_INT64, LARGEST_INT64),
        )?;
        let mut sub = CursorState::new(0, self.inner.clone(), self.conn.clone());
        sub.plan = Plan::Match;
        sub.expr = Some(single);
        sub.eval = Some(ev);
        sub.vals = Some(vals);
        while !sub.eof() {
            if !cb(&mut sub)? {
                break;
            }
            sub.next();
        }
        Ok(())
    }

    fn query_token(&mut self, phrase: i32, token: i32) -> ApiResult<Vec<u8>> {
        let Some(expr) = &self.expr else {
            return Err(ApiError::Code(aux::RANGE));
        };
        expr.phrases
            .get(phrase.max(0) as usize)
            .filter(|_| phrase >= 0)
            .and_then(|p| p.terms.get(token.max(0) as usize).filter(|_| token >= 0))
            .map(|t| t.alts[0].clone())
            .ok_or(ApiError::Code(aux::RANGE))
    }

    fn detail(&self) -> i32 {
        match self.cfg().detail {
            Detail::Full => 0,
            Detail::None => 1,
            Detail::Columns => 2,
        }
    }
}

pub(crate) struct Fts5Cursor {
    state: Arc<Mutex<CursorState>>,
    id: i64,
}

impl Drop for Fts5Cursor {
    fn drop(&mut self) {
        CURSORS.lock().remove(&self.id);
    }
}

impl VirtualTableCursor for Fts5Cursor {
    fn filter(&mut self, args: &[Value], idx_str: Option<&str>, idx_num: i32) -> IOResultOr<bool> {
        Ok(IOResult::Done(
            self.state.lock().filter(args, idx_str, idx_num)?,
        ))
    }

    fn next(&mut self) -> IOResultOr<bool> {
        Ok(IOResult::Done(self.state.lock().next()))
    }

    fn column(&mut self, column: usize) -> IOResultOr<Value> {
        Ok(IOResult::Done(self.state.lock().column(column)?))
    }

    fn rowid(&self) -> i64 {
        self.state.lock().rowid()
    }
}

// ------------------------------------------------------------------ SQL functions

/// An auxiliary function called as an SQL function; the first argument is
/// the value of the table's hidden column, the cursor id.
#[derive(Debug)]
pub(crate) struct AuxFunction {
    pub(crate) name: String,
}

impl ScalarFunction for AuxFunction {
    type Call = AuxCall;

    fn create_call(&self) -> Result<Self::Call> {
        Ok(AuxCall {
            name: self.name.clone(),
        })
    }
}

pub(crate) struct AuxCall {
    name: String,
}

impl ScalarCall for AuxCall {
    type Output = Value;

    fn step(&mut self, _connection: &Arc<Connection>, args: &[Register]) -> IOResultOr<Value> {
        let unusable = || {
            LimboError::SqlError(format!(
                "unable to use function {} in the requested context",
                self.name
            ))
        };
        let Some(first) = args.first() else {
            return Err(unusable().into());
        };
        let Value::Numeric(crate::numeric::Numeric::Integer(id)) = first.get_value() else {
            return Err(unusable().into());
        };
        let state = CURSORS.lock().get(id).and_then(|w| w.upgrade());
        let Some(state) = state else {
            return Err(LimboError::SqlError(format!("no such cursor: {id}")).into());
        };
        let mut st = state.lock();
        if matches!(st.plan, Plan::None | Plan::Special) {
            return Err(LimboError::SqlError(format!("no such cursor: {id}")).into());
        }
        let rest: Vec<Value> = args[1..].iter().map(|r| r.get_value().clone()).collect();
        Ok(IOResult::Done(st.call_aux(&self.name, &rest)?))
    }
}

#[derive(Debug)]
struct SimpleFunction {
    name: &'static str,
}

impl ScalarFunction for SimpleFunction {
    type Call = SimpleCall;

    fn create_call(&self) -> Result<Self::Call> {
        Ok(SimpleCall { name: self.name })
    }
}

struct SimpleCall {
    name: &'static str,
}

impl ScalarCall for SimpleCall {
    type Output = Value;

    fn step(&mut self, connection: &Arc<Connection>, args: &[Register]) -> IOResultOr<Value> {
        let v = match self.name {
            "fts5" => {
                api::run_fts5_function_hook(connection, args[0].get_value());
                Value::Null
            }
            "fts5_source_id" => Value::build_text(FTS5_SOURCE_ID),
            "fts5_locale" => {
                let loc = value_text(args[0].get_value()).unwrap_or_default();
                let text = value_text(args[1].get_value());
                if loc.is_empty() {
                    match text {
                        Some(t) => Value::build_text(String::from_utf8_lossy(&t).into_owned()),
                        None => Value::Null,
                    }
                } else {
                    sql::blob(&storage::make_locale_value(&loc, &text.unwrap_or_default()))
                }
            }
            _ => args[0].get_value().clone(),
        };
        Ok(IOResult::Done(v))
    }
}

#[cfg(test)]
mod tests;
