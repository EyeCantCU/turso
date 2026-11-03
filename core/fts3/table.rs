//! The fts3/fts4 virtual table module: table definition, cursor, xBestIndex
//! and the auxiliary functions (`fts3.c`).

use super::eval::Eval;
use super::expr;
use super::segment::SegReader;
use super::tokenizer::{init_tokenizer, Tokenizer};
use super::write::{self, Pending};
use super::{
    corrupt, dequote, is_id_char, next_token, quote_id, quote_str, sql_error, value_int,
    value_text, Sql,
};
use crate::ext::{ModuleImplementation, VTabImpl};
use crate::function::ExternalFunc;
use crate::native_ext::{
    ConnectInfo, ConstraintInfo, ConstraintUsage, FunctionArity, IndexInfo, OrderByInfo,
    ScalarCall, ScalarFunction, TableHook, TableUpdate, VTabKind, VirtualTable, VirtualTableCursor,
    VirtualTableModule,
};
use crate::sync::atomic::{AtomicI32, AtomicU64, AtomicU8, Ordering};
use crate::sync::{Arc, Mutex, RwLock, Weak};
use crate::types::IOResultOr;
use crate::vdbe::Register;
use crate::{Connection, IOResult, LimboError, Result, SymbolTable, Value};
use std::collections::HashMap;
use turso_ext::{ConstraintOp, ResultCode};

pub(crate) const FULLSCAN_SEARCH: i32 = 0;
pub(crate) const DOCID_SEARCH: i32 = 1;
pub(crate) const FULLTEXT_SEARCH: i32 = 2;
const HAVE_LANGID: i32 = 0x0001_0000;
const HAVE_DOCID_GE: i32 = 0x0002_0000;
const HAVE_DOCID_LE: i32 = 0x0004_0000;

/// An FTS3/FTS4 table definition (the immutable part of `Fts3Table`).
pub(crate) struct Fts3Table {
    pub db: String,
    pub name: RwLock<String>,
    pub columns: Vec<String>,
    pub notindexed: Vec<bool>,
    tok_spec: String,
    /// `content=`: `Some("")` for a contentless table.
    pub content: Option<String>,
    pub langid: Option<String>,
    /// Prefix lengths of each index; index 0 is the main index.
    pub prefixes: Vec<i32>,
    pub fts4: bool,
    pub has_docsize: bool,
    /// 0, 1, or 2 for unknown (an FTS3 table may have a `%_stat` table).
    pub has_stat: AtomicU8,
    pub desc: bool,
    pub compress: Option<String>,
    pub uncompress: Option<String>,
    tokenizer: Mutex<Option<Arc<dyn Tokenizer>>>,
    page_size: AtomicI32,
    /// An fts4aux view of the table: one index, ascending, no pending terms.
    pub aux: bool,
}

impl std::fmt::Debug for Fts3Table {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fts3Table")
            .field("name", &*self.name.read())
            .finish_non_exhaustive()
    }
}

/// A table and the connection running a statement against it.
pub(crate) struct Ctx<'a> {
    pub conn: &'a Arc<Connection>,
    pub tab: &'a Fts3Table,
}

impl Fts3Table {
    /// The minimal table an fts4aux cursor reads through.
    pub(crate) fn aux(db: &str, name: &str) -> Self {
        Fts3Table {
            db: db.to_string(),
            name: RwLock::new(name.to_string()),
            columns: Vec::new(),
            notindexed: Vec::new(),
            tok_spec: "simple".to_string(),
            content: None,
            langid: None,
            prefixes: vec![0],
            fts4: false,
            has_docsize: false,
            has_stat: AtomicU8::new(0),
            desc: false,
            compress: None,
            uncompress: None,
            tokenizer: Mutex::new(None),
            page_size: AtomicI32::new(0),
            aux: true,
        }
    }

    pub(crate) fn name(&self) -> String {
        self.name.read().clone()
    }

    /// `"db"."name_suffix"`.
    pub(crate) fn shadow(&self, suffix: &str) -> String {
        format!(
            "{}.{}",
            quote_id(&self.db),
            quote_id(&format!("{}_{}", self.name(), suffix))
        )
    }

    pub(crate) fn ncol(&self) -> usize {
        self.columns.len()
    }

    pub(crate) fn nindex(&self) -> usize {
        self.prefixes.len()
    }

    pub(crate) fn contentless(&self) -> bool {
        matches!(&self.content, Some(c) if c.is_empty())
    }

    /// `zReadExprlist`: docid, the columns and the language id.
    pub(crate) fn read_exprlist(&self) -> String {
        let mut s;
        match &self.content {
            None => {
                s = String::from("docid");
                for (i, c) in self.columns.iter().enumerate() {
                    let col = format!("x.{}", quote_id(&format!("c{i}{c}")));
                    match &self.uncompress {
                        Some(f) => s.push_str(&format!(",{}({col})", quote_id(f))),
                        None => s.push_str(&format!(",{col}")),
                    }
                }
                if self.langid.is_some() {
                    s.push_str(", x.\"langid\"");
                }
                s.push_str(&format!(" FROM {} AS x", self.shadow("content")));
            }
            Some(tbl) => {
                s = String::from("rowid");
                for c in &self.columns {
                    s.push_str(&format!(", x.{}", quote_id(c)));
                }
                if let Some(l) = &self.langid {
                    s.push_str(&format!(", x.{}", quote_id(l)));
                }
                s.push_str(&format!(
                    " FROM {}.{} AS x",
                    quote_id(&self.db),
                    quote_id(tbl)
                ));
            }
        }
        s
    }

    pub(crate) fn tokenizer(&self, conn: Option<&Arc<Connection>>) -> Result<Arc<dyn Tokenizer>> {
        let mut t = self.tokenizer.lock();
        if let Some(t) = t.as_ref() {
            return Ok(t.clone());
        }
        let tok = init_tokenizer(conn, &self.tok_spec)?;
        *t = Some(tok.clone());
        Ok(tok)
    }

    /// `nNodeSize`: the page size less 35.
    pub(crate) fn node_size(&self, conn: &Arc<Connection>) -> Result<i32> {
        Ok(self.page_size(conn)? - 35)
    }

    pub(crate) fn page_size(&self, conn: &Arc<Connection>) -> Result<i32> {
        let p = self.page_size.load(Ordering::Relaxed);
        if p > 0 {
            return Ok(p);
        }
        let sql = format!("PRAGMA {}.page_size", quote_id(&self.db));
        let v = Sql::new(conn)
            .row(&sql, &[])?
            .and_then(|r| r.first().map(value_int))
            .unwrap_or(4096) as i32;
        self.page_size.store(v, Ordering::Relaxed);
        Ok(v)
    }

    pub(crate) fn pending_key(&self) -> String {
        format!("{}.{}", self.db, self.name().to_lowercase())
    }
}

impl Ctx<'_> {
    pub(crate) fn sql(&self) -> Sql<'_> {
        Sql::new(self.conn)
    }

    /// `sqlite3Fts3ReadBlock`.
    pub(crate) fn read_block(&self, id: i64) -> Result<Vec<u8>> {
        let sql = format!(
            "SELECT block FROM {} WHERE blockid = ?",
            self.tab.shadow("segments")
        );
        match self.sql().row(&sql, &[Value::from_i64(id)])? {
            Some(r) => Ok(value_text(&r[0]).unwrap_or_default()),
            None => Err(corrupt()),
        }
    }

    /// Segdir rows of an index, oldest first (`sqlite3Fts3AllSegdirs`).
    #[allow(clippy::type_complexity)]
    pub(crate) fn all_segdirs(
        &self,
        langid: i32,
        index: usize,
        level: i32,
    ) -> Result<Vec<(i64, i64, i64, i64, Vec<u8>)>> {
        let tab = self.tab;
        let rows = if level < 0 {
            let sql = format!(
                "SELECT idx, start_block, leaves_end_block, end_block, root FROM {} \
                 WHERE level BETWEEN ? AND ? ORDER BY level DESC, idx ASC",
                tab.shadow("segdir")
            );
            self.sql().rows(
                &sql,
                &[
                    Value::from_i64(write::abs_level(tab, langid, index, 0)),
                    Value::from_i64(write::abs_level(
                        tab,
                        langid,
                        index,
                        super::SEGDIR_MAXLEVEL - 1,
                    )),
                ],
            )?
        } else {
            let sql = format!(
                "SELECT idx, start_block, leaves_end_block, end_block, root FROM {} \
                 WHERE level = ? ORDER BY idx ASC",
                tab.shadow("segdir")
            );
            self.sql().rows(
                &sql,
                &[Value::from_i64(write::abs_level(
                    tab,
                    langid,
                    index,
                    level as i64,
                ))],
            )?
        };
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    value_int(&r[0]),
                    value_int(&r[1]),
                    value_int(&r[2]),
                    value_int(&r[3]),
                    value_text(&r[4]).unwrap_or_default(),
                )
            })
            .collect())
    }

    pub(crate) fn with_pending<R>(&self, f: impl FnOnce(&mut Pending) -> R) -> R {
        let key = self.tab.pending_key();
        let mut inner = self.conn.fts3.inner.lock();
        let p = inner
            .pending
            .entry(key)
            .or_insert_with(|| Pending::new(self.tab.nindex()));
        if p.index.len() != self.tab.nindex() {
            *p = Pending::new(self.tab.nindex());
        }
        f(p)
    }

    pub(crate) fn prev_langid(&self) -> i32 {
        self.with_pending(|p| p.prev_langid)
    }

    /// A reader over this connection's pending terms (`sqlite3Fts3SegReaderPending`).
    pub(crate) fn pending_reader(
        &self,
        index: usize,
        term: Option<&[u8]>,
        prefix: bool,
    ) -> Option<SegReader> {
        let items: Vec<(Vec<u8>, Vec<u8>)> = self.with_pending(|p| {
            let h = &p.index[index];
            if prefix {
                h.iter()
                    .filter(|(k, _)| match term {
                        None => true,
                        Some(t) => t.is_empty() || k.starts_with(t),
                    })
                    .map(|(k, v)| (k.clone(), v.doclist()))
                    .collect()
            } else {
                term.and_then(|t| h.get(t).map(|v| vec![(t.to_vec(), v.doclist())]))
                    .unwrap_or_default()
            }
        });
        if items.is_empty() {
            None
        } else {
            Some(SegReader::pending(items))
        }
    }
}

fn is_special_column(z: &str) -> Option<(&str, String)> {
    let eq = z.find('=')?;
    Some((&z[..eq], dequote(&z[eq + 1..])))
}

/// `fts3InitVtab`: parses the module arguments.
fn parse_args(
    conn: Option<&Arc<Connection>>,
    module: &str,
    db: &str,
    name: &str,
    args: &[String],
    lookup_columns: Option<&crate::native_ext::ColumnLookup>,
) -> Result<Fts3Table> {
    let fts4 = module.as_bytes().get(3) == Some(&b'4');
    let mut cols: Vec<String> = Vec::new();
    let mut tok_spec: Option<String> = None;
    let mut no_docsize = false;
    let mut desc = false;
    let mut prefix: Option<String> = None;
    let mut compress: Option<String> = None;
    let mut uncompress: Option<String> = None;
    let mut content: Option<String> = None;
    let mut langid: Option<String> = None;
    let mut notindexed: Vec<String> = Vec::new();
    for z in args {
        let zb = z.as_bytes();
        if tok_spec.is_none()
            && zb.len() > 8
            && zb[..8].eq_ignore_ascii_case(b"tokenize")
            && !is_id_char(zb[8])
        {
            tok_spec = Some(z[9..].to_string());
        } else if let Some((key, val)) = is_special_column(z).filter(|_| fts4) {
            match key.to_ascii_lowercase().as_str() {
                "matchinfo" => {
                    if !val.eq_ignore_ascii_case("fts3") {
                        return Err(sql_error(format!("unrecognized matchinfo: {val}")));
                    }
                    no_docsize = true;
                }
                "prefix" => prefix = Some(val),
                "compress" => compress = Some(val),
                "uncompress" => uncompress = Some(val),
                "order" => {
                    if !val.eq_ignore_ascii_case("asc") && !val.eq_ignore_ascii_case("desc") {
                        return Err(sql_error(format!("unrecognized order: {val}")));
                    }
                    desc = val
                        .as_bytes()
                        .first()
                        .is_some_and(|c| *c == b'd' || *c == b'D');
                }
                "content" => content = Some(val),
                "languageid" => langid = Some(val),
                "notindexed" => notindexed.push(val),
                _ => return Err(sql_error(format!("unrecognized parameter: {z}"))),
            }
        } else {
            cols.push(z.clone());
        }
    }
    let mut columns: Vec<String> = cols
        .iter()
        .map(|c| match next_token(c.as_bytes(), 0) {
            Some((s, n)) => dequote(&c[s..s + n]),
            None => String::new(),
        })
        .collect();
    if let Some(tbl) = &content {
        compress = None;
        uncompress = None;
        if columns.is_empty() && !tbl.is_empty() {
            let mut derived = match (conn, lookup_columns) {
                (Some(conn), _) => content_columns(conn, db, tbl)?,
                (None, Some(f)) => {
                    f(tbl).ok_or_else(|| sql_error(format!("no such table: {db}.{tbl}")))?
                }
                (None, None) => return Err(sql_error(format!("no such table: {db}.{tbl}"))),
            };
            if let Some(l) = &langid {
                if let Some(j) = derived.iter().position(|c| c.eq_ignore_ascii_case(l)) {
                    derived.remove(j);
                }
            }
            columns = derived;
        }
    }
    if columns.is_empty() {
        columns.push("content".to_string());
    }
    let tok_spec = tok_spec.unwrap_or_else(|| "simple".to_string());
    // fts3PrefixParameter
    let mut prefixes = vec![0];
    if let Some(pz) = prefix.as_deref().filter(|p| !p.is_empty()) {
        let b = pz.as_bytes();
        let nindex = 1 + pz.matches(',').count();
        let mut p = 0;
        for _ in 0..nindex {
            let (n, v) = expr::read_int(&b[p.min(b.len())..]);
            if n == 0 {
                return Err(sql_error(format!("error parsing prefix parameter: {pz}")));
            }
            let mut v = v.unwrap_or(-1);
            if v < 0 {
                return Err(sql_error(format!("error parsing prefix parameter: {pz}")));
            }
            if v > 10_000_000 {
                v = 0;
            }
            if v != 0 {
                prefixes.push(v);
            }
            p += n + 1;
        }
    }
    let mut abnot = vec![false; columns.len()];
    for ni in &notindexed {
        let mut found = false;
        for (i, c) in columns.iter().enumerate() {
            if c.len() == ni.len() && c.eq_ignore_ascii_case(ni) {
                abnot[i] = true;
                found = true;
            }
        }
        if !found {
            return Err(sql_error(format!("no such column: {ni}")));
        }
    }
    if compress.is_some() != uncompress.is_some() {
        let miss = if compress.is_none() {
            "compress"
        } else {
            "uncompress"
        };
        return Err(sql_error(format!(
            "missing {miss} parameter in fts4 constructor"
        )));
    }
    Ok(Fts3Table {
        db: db.to_string(),
        name: RwLock::new(name.to_string()),
        columns,
        notindexed: abnot,
        tok_spec,
        content,
        langid,
        prefixes,
        fts4,
        has_docsize: fts4 && !no_docsize,
        has_stat: AtomicU8::new(if fts4 { 1 } else { 2 }),
        desc,
        compress,
        uncompress,
        tokenizer: Mutex::new(None),
        page_size: AtomicI32::new(0),
        aux: false,
    })
}

/// `fts3ContentColumns`: the column names of the content table.
fn content_columns(conn: &Arc<Connection>, db: &str, tbl: &str) -> Result<Vec<String>> {
    let sql = format!("SELECT * FROM {}.{}", quote_id(db), quote_id(tbl));
    let stmt = conn.prepare_internal(&sql)?;
    Ok((0..stmt.num_columns())
        .map(|i| stmt.get_column_name(i).into_owned())
        .collect())
}

/// `fts3DeclareVtab`.
fn declare(tab: &Fts3Table) -> String {
    let mut s = String::from("CREATE TABLE x(");
    for c in &tab.columns {
        s.push_str(&quote_str(c));
        s.push_str(", ");
    }
    let lang = tab.langid.as_deref().unwrap_or("__langid");
    s.push_str(&format!(
        "{} HIDDEN, docid HIDDEN, {} HIDDEN)",
        quote_str(&tab.name()),
        quote_str(lang)
    ));
    s
}

/// `fts3CreateTables`.
fn create_tables(conn: &Arc<Connection>, tab: &Fts3Table) -> Result<()> {
    let sql = Sql::new(conn);
    if tab.content.is_none() {
        let mut cols = String::from("docid INTEGER PRIMARY KEY");
        for (i, c) in tab.columns.iter().enumerate() {
            cols.push_str(&format!(", {}", quote_str(&format!("c{i}{c}"))));
        }
        if tab.langid.is_some() {
            cols.push_str(", langid");
        }
        sql.exec(
            &format!("CREATE TABLE {}({cols})", tab.shadow("content")),
            &[],
        )?;
    }
    sql.exec(
        &format!(
            "CREATE TABLE {}(blockid INTEGER PRIMARY KEY, block BLOB)",
            tab.shadow("segments")
        ),
        &[],
    )?;
    sql.exec(
        &format!(
            "CREATE TABLE {}(level INTEGER,idx INTEGER,start_block INTEGER,\
             leaves_end_block INTEGER,end_block INTEGER,root BLOB,PRIMARY KEY(level, idx))",
            tab.shadow("segdir")
        ),
        &[],
    )?;
    if tab.has_docsize {
        sql.exec(
            &format!(
                "CREATE TABLE {}(docid INTEGER PRIMARY KEY, size BLOB)",
                tab.shadow("docsize")
            ),
            &[],
        )?;
    }
    if tab.has_stat.load(Ordering::Relaxed) == 1 {
        write::create_stat_table(conn, tab)?;
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct Fts3Module {
    name: &'static str,
}

fn args_to_strings(args: &[Value]) -> Vec<String> {
    args.iter()
        .map(|v| match v {
            Value::Text(t) => t.as_str().to_string(),
            other => other.to_string(),
        })
        .collect()
}

impl VirtualTableModule for Fts3Module {
    type Table = Fts3Vtab;

    fn schema(&self, args: &[Value]) -> Result<String> {
        let tab = parse_args(None, self.name, "main", "x", &args_to_strings(args), None).or_else(
            |_| {
                // Columns from a content table are only known at connect time.
                parse_args(None, self.name, "main", "x", &[], None)
            },
        )?;
        Ok(declare(&tab))
    }

    fn create(&self, _args: &[Value]) -> Result<Fts3Vtab> {
        Err(sql_error("fts3 tables are created through connect"))
    }

    fn connect(&self, info: &ConnectInfo, args: &[Value]) -> Result<(Fts3Vtab, String)> {
        let args = args_to_strings(args);
        let tab = parse_args(
            info.connection,
            self.name,
            info.schema_name,
            info.table_name,
            &args,
            info.lookup_columns,
        )?;
        if let Some(conn) = info.connection {
            // Fail CREATE with an unknown tokenizer, as SQLite does.
            tab.tokenizer(Some(conn))?;
            if info.create {
                create_tables(conn, &tab)?;
            }
        } else if super::tokenizer::TokenizerModule::builtin(
            &super::tokenizer::parse_spec(&tab.tok_spec).0,
        )
        .is_some()
        {
            tab.tokenizer(None)?;
        }
        let schema = declare(&tab);
        Ok((Fts3Vtab { tab: Arc::new(tab) }, schema))
    }

    fn innocuous(&self) -> bool {
        true
    }
}

#[derive(Debug)]
pub(crate) struct Fts3Vtab {
    pub tab: Arc<Fts3Table>,
}

impl VirtualTable for Fts3Vtab {
    type Cursor = Fts3Cursor;

    fn open(&self, connection: Arc<Connection>) -> Result<Fts3Cursor> {
        Ok(Fts3Cursor::new(self.tab.clone(), connection))
    }

    fn best_index(
        &self,
        constraints: &[ConstraintInfo],
        order_by: &[OrderByInfo],
    ) -> std::result::Result<IndexInfo, ResultCode> {
        let ncol = self.tab.ncol() as u32;
        let mut idx_num = FULLSCAN_SEARCH;
        let mut cost = 5_000_000.0;
        let mut rows = 25u32;
        let mut icons: Option<usize> = None;
        let mut ilang: Option<usize> = None;
        let mut ige: Option<usize> = None;
        let mut ile: Option<usize> = None;
        for (i, c) in constraints.iter().enumerate() {
            if !c.usable {
                if c.op == ConstraintOp::Match {
                    return Ok(IndexInfo {
                        idx_num: FULLSCAN_SEARCH,
                        idx_str: None,
                        order_by_consumed: false,
                        estimated_cost: 1e50,
                        estimated_rows: u32::MAX,
                        constraint_usages: vec![
                            ConstraintUsage {
                                argv_index: None,
                                omit: false
                            };
                            constraints.len()
                        ],
                    });
                }
                continue;
            }
            let docid = c.column_index == ncol + 1;
            if icons.is_none() && c.op == ConstraintOp::Eq && docid {
                idx_num = DOCID_SEARCH;
                cost = 1.0;
                rows = 1;
                icons = Some(i);
            }
            if c.op == ConstraintOp::Match && c.column_index <= ncol {
                idx_num = FULLTEXT_SEARCH + c.column_index as i32;
                cost = 2.0;
                icons = Some(i);
            }
            if c.op == ConstraintOp::Eq && c.column_index == ncol + 2 {
                ilang = Some(i);
            }
            if docid {
                match c.op {
                    ConstraintOp::Ge | ConstraintOp::Gt => ige = Some(i),
                    ConstraintOp::Le | ConstraintOp::Lt => ile = Some(i),
                    _ => {}
                }
            }
        }
        let mut usages = vec![
            ConstraintUsage {
                argv_index: None,
                omit: false
            };
            constraints.len()
        ];
        let mut next = 1;
        if let Some(i) = icons {
            usages[i] = ConstraintUsage {
                argv_index: Some(next),
                omit: true,
            };
            next += 1;
        }
        if let Some(i) = ilang {
            idx_num |= HAVE_LANGID;
            usages[i].argv_index = Some(next);
            next += 1;
        }
        if let Some(i) = ige {
            idx_num |= HAVE_DOCID_GE;
            usages[i].argv_index = Some(next);
            next += 1;
        }
        if let Some(i) = ile {
            idx_num |= HAVE_DOCID_LE;
            usages[i].argv_index = Some(next);
        }
        let mut idx_str = None;
        let mut consumed = false;
        if order_by.len() == 1
            && (order_by[0].column_index == ncol + 1 || order_by[0].column_index == u32::MAX)
        {
            idx_str = Some(if order_by[0].desc { "DESC" } else { "ASC" }.to_string());
            consumed = true;
        }
        Ok(IndexInfo {
            idx_num,
            idx_str,
            order_by_consumed: consumed,
            estimated_cost: cost,
            estimated_rows: rows,
            constraint_usages: usages,
        })
    }

    fn readonly(&self) -> bool {
        false
    }

    fn unique_scan(&self, idx_num: i32) -> bool {
        idx_num & 0xffff == DOCID_SEARCH
    }

    fn create_update(&mut self, _args: Vec<Value>) -> Result<Box<dyn TableUpdate>> {
        Err(LimboError::ReadOnly)
    }

    fn create_update_conn(
        &mut self,
        conn: &Arc<Connection>,
        args: Vec<Value>,
        on_conflict: u8,
    ) -> Result<Box<dyn TableUpdate>> {
        Ok(Box::new(Fts3Update {
            tab: self.tab.clone(),
            conn: conn.clone(),
            args: Some(args),
            on_conflict,
        }))
    }

    fn hook(&mut self, conn: &Arc<Connection>, hook: TableHook<'_>) -> Result<()> {
        let ctx = Ctx {
            conn,
            tab: &self.tab,
        };
        match hook {
            TableHook::Begin => write::begin(&ctx),
            TableHook::Commit => write::sync(&ctx),
            TableHook::Rollback => {
                write::pending_clear(&ctx);
                Ok(())
            }
            TableHook::Savepoint(level) => write::savepoint(&ctx, level as i32),
            TableHook::Release(level) => {
                ctx.with_pending(|p| p.savepoint = level as i32);
                Ok(())
            }
            TableHook::RollbackTo(level) => {
                if (level as i32) < ctx.with_pending(|p| p.savepoint) {
                    write::pending_clear(&ctx);
                }
                Ok(())
            }
            TableHook::Rename(new) => write::rename(&ctx, new),
            TableHook::Destroy => write::destroy(&ctx),
        }
    }
}

struct Fts3Update {
    tab: Arc<Fts3Table>,
    conn: Arc<Connection>,
    args: Option<Vec<Value>>,
    on_conflict: u8,
}

impl TableUpdate for Fts3Update {
    fn step(&mut self) -> IOResultOr<Option<i64>> {
        let args = self.args.take().unwrap_or_default();
        let ctx = Ctx {
            conn: &self.conn,
            tab: &self.tab,
        };
        let r = write::update(&ctx, &args, self.on_conflict)?;
        Ok(IOResult::Done(r))
    }
}

/// Live cursors, so the auxiliary functions can find theirs.
static CURSORS: std::sync::LazyLock<std::sync::Mutex<HashMap<u64, Weak<Mutex<CursorState>>>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
static NEXT_CURSOR: AtomicU64 = AtomicU64::new(1);
const CURSOR_MAGIC: &[u8; 8] = b"fts3cur\0";

pub(crate) struct CursorState {
    pub tab: Arc<Fts3Table>,
    pub conn: Arc<Connection>,
    pub esearch: i32,
    pub eof: bool,
    pub prev_id: i64,
    pub langid: i32,
    pub bdesc: bool,
    min_docid: i64,
    max_docid: i64,
    /// The content row of `prev_id`, once read.
    row: Option<Vec<Value>>,
    require_seek: bool,
    /// Full-scan rows fetched ahead.
    scan: std::collections::VecDeque<Vec<Value>>,
    scan_done: bool,
    scan_started: bool,
    docid_cons: Option<Value>,
    pub eval: Option<Eval>,
    pub mi: Option<(String, Vec<u32>)>,
}

pub(crate) struct Fts3Cursor {
    id: u64,
    state: Arc<Mutex<CursorState>>,
}

impl Fts3Cursor {
    fn new(tab: Arc<Fts3Table>, conn: Arc<Connection>) -> Self {
        let id = NEXT_CURSOR.fetch_add(1, Ordering::Relaxed);
        let state = Arc::new(Mutex::new(CursorState {
            tab,
            conn,
            esearch: FULLSCAN_SEARCH,
            eof: true,
            prev_id: 0,
            langid: 0,
            bdesc: false,
            min_docid: i64::MIN,
            max_docid: i64::MAX,
            row: None,
            require_seek: false,
            scan: Default::default(),
            scan_done: true,
            scan_started: false,
            docid_cons: None,
            eval: None,
            mi: None,
        }));
        CURSORS.lock().unwrap().insert(id, Arc::downgrade(&state));
        Self { id, state }
    }
}

impl Drop for Fts3Cursor {
    fn drop(&mut self) {
        CURSORS.lock().unwrap().remove(&self.id);
    }
}

fn docid_range(v: Option<&Value>, default: i64) -> i64 {
    match v {
        Some(Value::Numeric(_)) if v.and_then(|v| v.as_int()).is_some() => {
            v.and_then(|v| v.as_int()).unwrap()
        }
        Some(Value::Text(t)) => match t.as_str().trim().parse::<i64>() {
            Ok(i) => i,
            Err(_) => default,
        },
        _ => default,
    }
}

impl CursorState {
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            conn: &self.conn,
            tab: &self.tab,
        }
    }

    /// Fetches the next batch of a full-table scan.
    fn fill_scan(&mut self) -> Result<()> {
        let last = self.prev_id;
        let (lo, hi) = (self.min_docid, self.max_docid);
        let mut sql = format!(
            "SELECT {} WHERE rowid BETWEEN ? AND ?",
            self.tab.read_exprlist()
        );
        let mut params = vec![Value::from_i64(lo), Value::from_i64(hi)];
        if self.scan_started {
            sql.push_str(if self.bdesc {
                " AND rowid < ?"
            } else {
                " AND rowid > ?"
            });
            params.push(Value::from_i64(last));
        }
        sql.push_str(if self.bdesc {
            " ORDER BY rowid DESC LIMIT 256"
        } else {
            " ORDER BY rowid ASC LIMIT 256"
        });
        let rows = Sql::new(&self.conn).rows(&sql, &params)?;
        if rows.len() < 256 {
            self.scan_done = true;
        }
        self.scan.extend(rows);
        Ok(())
    }

    fn next(&mut self) -> Result<()> {
        match self.esearch {
            FULLSCAN_SEARCH => {
                if self.scan.is_empty() && !self.scan_done {
                    self.fill_scan()?;
                }
                match self.scan.pop_front() {
                    Some(r) => {
                        self.prev_id = value_int(&r[0]);
                        self.row = Some(r);
                        self.scan_started = true;
                        self.require_seek = false;
                    }
                    None => self.eof = true,
                }
            }
            DOCID_SEARCH => {
                if let Some(v) = self.docid_cons.take() {
                    let sql = format!("SELECT {} WHERE rowid = ?", self.tab.read_exprlist());
                    match Sql::new(&self.conn).row(&sql, &[v])? {
                        Some(r) => {
                            self.prev_id = value_int(&r[0]);
                            self.row = Some(r);
                            self.require_seek = false;
                        }
                        None => self.eof = true,
                    }
                } else {
                    self.eof = true;
                }
            }
            _ => {
                let ev = self.eval.as_mut().expect("full-text cursor");
                if ev.expr.root.is_none() {
                    self.eof = true;
                    return Ok(());
                }
                ev.next();
                self.eof = ev.is_eof;
                self.prev_id = ev.prev_id;
                self.require_seek = true;
                self.row = None;
            }
        }
        Ok(())
    }

    /// `fts3CursorSeek`: reads the content row of the current docid.
    pub(crate) fn seek(&mut self) -> Result<()> {
        if self.require_seek {
            self.require_seek = false;
            let sql = format!("SELECT {} WHERE rowid = ?", self.tab.read_exprlist());
            match Sql::new(&self.conn).row(&sql, &[Value::from_i64(self.prev_id)])? {
                Some(r) => self.row = Some(r),
                None => {
                    self.row = None;
                    if self.tab.content.is_none() {
                        self.eof = true;
                        return Err(corrupt());
                    }
                }
            }
        }
        Ok(())
    }

    /// Text of column `icol` of the current row.
    pub(crate) fn column_text(&mut self, icol: usize) -> Result<Option<Vec<u8>>> {
        self.seek()?;
        Ok(self
            .row
            .as_ref()
            .and_then(|r| r.get(icol + 1))
            .and_then(value_text))
    }

    fn filter(&mut self, args: &[Value], idx_str: Option<&str>, idx_num: i32) -> Result<()> {
        let esearch = idx_num & 0xffff;
        let mut i = 0;
        let mut take = || {
            let v = args.get(i).cloned();
            i += 1;
            v
        };
        let cons = if esearch != FULLSCAN_SEARCH {
            take()
        } else {
            None
        };
        let lang = if idx_num & HAVE_LANGID != 0 {
            take()
        } else {
            None
        };
        let ge = if idx_num & HAVE_DOCID_GE != 0 {
            take()
        } else {
            None
        };
        let le = if idx_num & HAVE_DOCID_LE != 0 {
            take()
        } else {
            None
        };
        self.eval = None;
        self.mi = None;
        self.row = None;
        self.scan.clear();
        self.scan_done = false;
        self.scan_started = false;
        self.eof = false;
        self.prev_id = 0;
        self.min_docid = docid_range(ge.as_ref(), i64::MIN);
        self.max_docid = docid_range(le.as_ref(), i64::MAX);
        self.bdesc = match idx_str {
            Some(s) => s.starts_with('D'),
            None => self.tab.desc,
        };
        self.esearch = esearch;
        if esearch != DOCID_SEARCH && esearch != FULLSCAN_SEARCH {
            let icol = esearch - FULLTEXT_SEARCH;
            let query = cons.as_ref().and_then(value_text);
            self.langid = lang.as_ref().map(|v| value_int(v) as i32).unwrap_or(0);
            let tok = self.tab.tokenizer(Some(&self.conn))?;
            let e = expr::parse(
                tok.as_ref(),
                self.langid,
                &self.tab.columns,
                self.tab.fts4,
                icol,
                query.as_deref(),
            )?;
            let ctx = self.ctx();
            let ev = Eval::new(
                &ctx,
                e,
                self.langid,
                self.bdesc,
                self.min_docid,
                self.max_docid,
            )?;
            self.eval = Some(ev);
        } else if esearch == DOCID_SEARCH {
            self.docid_cons = cons;
        }
        self.next()
    }
}

impl VirtualTableCursor for Fts3Cursor {
    fn filter(&mut self, args: &[Value], idx_str: Option<&str>, idx_num: i32) -> IOResultOr<bool> {
        let mut st = self.state.lock();
        st.filter(args, idx_str, idx_num)?;
        Ok(IOResult::Done(!st.eof))
    }

    fn next(&mut self) -> IOResultOr<bool> {
        let mut st = self.state.lock();
        st.next()?;
        Ok(IOResult::Done(!st.eof))
    }

    fn column(&mut self, column: usize) -> IOResultOr<Value> {
        let mut st = self.state.lock();
        let ncol = st.tab.ncol();
        let v = if column == ncol {
            let mut b = CURSOR_MAGIC.to_vec();
            b.extend_from_slice(&self.id.to_le_bytes());
            Value::from_slice(&b).map_err(|_| LimboError::InternalError("oom".into()))?
        } else if column == ncol + 1 {
            Value::from_i64(st.prev_id)
        } else if column == ncol + 2 && st.eval.is_some() {
            Value::from_i64(st.langid as i64)
        } else if column == ncol + 2 && st.tab.langid.is_none() {
            Value::from_i64(0)
        } else {
            let icol = if column > ncol { ncol } else { column };
            st.seek()?;
            st.row
                .as_ref()
                .and_then(|r| r.get(icol + 1).cloned())
                .unwrap_or(Value::Null)
        };
        Ok(IOResult::Done(v))
    }

    fn rowid(&self) -> i64 {
        self.state.lock().prev_id
    }
}

/// The cursor behind an auxiliary function's first argument.
fn cursor_arg(v: &Value) -> Option<Arc<Mutex<CursorState>>> {
    let b = v.to_blob()?;
    if b.len() != 16 || &b[..8] != CURSOR_MAGIC {
        return None;
    }
    let id = u64::from_le_bytes(b[8..16].try_into().ok()?);
    CURSORS.lock().unwrap().get(&id)?.upgrade()
}

fn arg_error(f: &str) -> LimboError {
    sql_error(format!(
        "unable to use function {f} in the requested context"
    ))
}

#[derive(Debug, Clone, Copy)]
enum Aux {
    Snippet,
    Offsets,
    Matchinfo,
    Optimize,
}

#[derive(Debug)]
struct AuxFunc(Aux);

impl ScalarFunction for AuxFunc {
    type Call = AuxCall;
    fn create_call(&self) -> Result<AuxCall> {
        Ok(AuxCall(self.0))
    }
}

struct AuxCall(Aux);

impl ScalarCall for AuxCall {
    type Output = Value;

    fn step(&mut self, _conn: &Arc<Connection>, args: &[Register]) -> IOResultOr<Value> {
        let vals: Vec<Value> = args.iter().map(|r| r.get_value().clone()).collect();
        let name = match self.0 {
            Aux::Snippet => "snippet",
            Aux::Offsets => "offsets",
            Aux::Matchinfo => "matchinfo",
            Aux::Optimize => "optimize",
        };
        let Some(state) = vals.first().and_then(cursor_arg) else {
            return Err(arg_error(name).into());
        };
        let mut st = state.lock();
        let v = match self.0 {
            Aux::Snippet => {
                if vals.len() > 6 {
                    return Err(sql_error("wrong number of arguments to function snippet()").into());
                }
                let text = |i: usize, d: &str| -> Vec<u8> {
                    vals.get(i)
                        .map(|v| value_text(v).unwrap_or_default())
                        .unwrap_or_else(|| d.as_bytes().to_vec())
                };
                let start = text(1, "<b>");
                let end = text(2, "</b>");
                let ellipsis = text(3, "<b>...</b>");
                let icol = vals.get(4).map(value_int).unwrap_or(-1);
                let ntoken = vals.get(5).map(value_int).unwrap_or(15);
                if ntoken == 0 || icol >= st.tab.ncol() as i64 {
                    Value::build_text("")
                } else {
                    st.seek()?;
                    let icol = if icol < 0 { -1 } else { icol };
                    let ntoken = ntoken.clamp(-64, 64);
                    let s = super::snippet::snippet(
                        &mut st,
                        &start,
                        &end,
                        &ellipsis,
                        icol as i32,
                        ntoken as i32,
                    )?;
                    Value::build_text(String::from_utf8_lossy(&s).into_owned())
                }
            }
            Aux::Offsets => {
                st.seek()?;
                let s = super::snippet::offsets(&mut st)?;
                Value::build_text(s)
            }
            Aux::Matchinfo => {
                let fmt = vals.get(1).map(|v| value_text(v).unwrap_or_default());
                let b = super::snippet::matchinfo(&mut st, fmt.as_deref())?;
                Value::from_slice(&b).map_err(|_| LimboError::InternalError("oom".into()))?
            }
            Aux::Optimize => {
                let ctx = st.ctx();
                if write::optimize_fn(&ctx)? {
                    Value::build_text("Index optimized")
                } else {
                    Value::build_text("Index already optimal")
                }
            }
        };
        Ok(IOResult::Done(v))
    }
}

/// `fts3_tokenizer(name)` / `fts3_tokenizer(name, ptr)`.
#[derive(Debug)]
struct TokenizerFunc;

impl ScalarFunction for TokenizerFunc {
    type Call = TokenizerCall;
    fn create_call(&self) -> Result<TokenizerCall> {
        Ok(TokenizerCall)
    }
}

struct TokenizerCall;

impl ScalarCall for TokenizerCall {
    type Output = Value;

    fn step(&mut self, conn: &Arc<Connection>, args: &[Register]) -> IOResultOr<Value> {
        use super::ctok::{module_from_ptr, module_ptr};
        // The translator appends a mask of the arguments that are bound parameters.
        let (args, frombind) = match args.split_last() {
            Some((m, rest)) if !rest.is_empty() => (rest, value_int(m.get_value())),
            _ => (args, 0),
        };
        let name = value_text(args[0].get_value());
        let enabled = conn.fts3.set_tokenizer_enabled(None);
        let ptr = if args.len() == 2 {
            let v = args[1].get_value();
            if !enabled && frombind & 2 == 0 {
                return Err(sql_error("fts3tokenize disabled").into());
            }
            let b = match v {
                Value::Blob(b) => b.to_vec(),
                Value::Null => Vec::new(),
                other => value_text(other).unwrap_or_default(),
            };
            let Some(name) = name.as_ref().filter(|_| b.len() == 8) else {
                return Err(sql_error("argument type mismatch").into());
            };
            let p = usize::from_ne_bytes(b[..8].try_into().unwrap());
            conn.fts3
                .inner
                .lock()
                .tokenizers
                .insert(name.clone(), module_from_ptr(p));
            p
        } else {
            let m = name
                .as_ref()
                .and_then(|n| super::tokenizer::find_module(Some(conn), n));
            let Some(m) = m else {
                return Err(sql_error(format!(
                    "unknown tokenizer: {}",
                    String::from_utf8_lossy(&name.unwrap_or_default())
                ))
                .into());
            };
            module_ptr(m).0
        };
        if enabled || frombind & 1 != 0 {
            Ok(IOResult::Done(
                Value::from_slice(&ptr.to_ne_bytes())
                    .map_err(|_| LimboError::InternalError("oom".into()))?,
            ))
        } else {
            Ok(IOResult::Done(Value::Null))
        }
    }
}

/// Registers fts3, fts4, fts4aux, fts3tokenize and the auxiliary functions.
pub(crate) fn register(syms: &mut SymbolTable) -> Result<()> {
    for name in ["fts3", "fts4"] {
        let module = VTabImpl {
            module_kind: VTabKind::VirtualTable,
            implementation: ModuleImplementation::Native(Arc::new(Fts3Module { name })),
            eponymous: None,
        };
        syms.vtab_modules.insert(name.to_string(), Arc::new(module));
    }
    super::aux::register(syms);
    super::tokenize_vtab::register(syms);
    let funcs: [(&str, FunctionArity, Aux); 4] = [
        ("snippet", FunctionArity::Variadic, Aux::Snippet),
        ("offsets", FunctionArity::Exact(1), Aux::Offsets),
        ("matchinfo", FunctionArity::OneOf(&[1, 2]), Aux::Matchinfo),
        ("optimize", FunctionArity::Exact(1), Aux::Optimize),
    ];
    for (name, arity, aux) in funcs {
        syms.functions.insert(
            name.to_string(),
            Arc::new(ExternalFunc::new_native_scalar(
                name.to_string(),
                arity,
                false,
                AuxFunc(aux),
            )?),
        );
    }
    syms.functions.insert(
        "fts3_tokenizer".to_string(),
        Arc::new(ExternalFunc::new_native_scalar(
            "fts3_tokenizer".to_string(),
            FunctionArity::OneOf(&[1, 2]),
            false,
            TokenizerFunc,
        )?),
    );
    Ok(())
}
