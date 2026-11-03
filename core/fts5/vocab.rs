//! The fts5vocab virtual table (fts5_vocab.c).

use super::config::{dequote, Detail};
use super::index::{Index, MAIN_PREFIX};
use super::sql::{self, err};
use super::varint::{get_varint32, pos_col, pos_off, poslist_next};
use super::TableInner;
use crate::native_ext::{
    ConnectInfo, ConstraintInfo, ConstraintUsage, IndexInfo, OrderByInfo, VirtualTable,
    VirtualTableCursor, VirtualTableModule,
};
use crate::sync::Arc;
use crate::types::IOResultOr;
use crate::{Connection, IOResult, Result, Value};
use turso_ext::{ConstraintOp, ResultCode};

const TERM_EQ: i32 = 0x0100;
const TERM_GE: i32 = 0x0200;
const TERM_LE: i32 = 0x0400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Col,
    Row,
    Instance,
}

const SCHEMAS: [&str; 3] = [
    "CREATE TABlE vocab(term, col, doc, cnt)",
    "CREATE TABlE vocab(term, doc, cnt)",
    "CREATE TABlE vocab(term, doc, col, offset)",
];

#[derive(Debug)]
pub(crate) struct VocabModule;

/// (fts5 database, fts5 table, kind) from the module arguments.
fn parse_args(schema: &str, args: &[Value]) -> Result<(String, String, Kind)> {
    let args = super::args_text(args);
    let temp = args.len() == 3 && schema.eq_ignore_ascii_case("temp");
    if args.len() != 2 && !temp {
        return Err(err("wrong number of vtable arguments"));
    }
    let deq = |s: &str| String::from_utf8_lossy(&dequote(s.as_bytes()).0).into_owned();
    let (db, tbl, ty) = if temp {
        (deq(&args[0]), deq(&args[1]), deq(&args[2]))
    } else {
        (schema.to_string(), deq(&args[0]), deq(&args[1]))
    };
    let kind = match ty.to_ascii_lowercase().as_str() {
        "col" => Kind::Col,
        "row" => Kind::Row,
        "instance" => Kind::Instance,
        _ => {
            return Err(err(format!(
                "fts5vocab: unknown table type: {}",
                super::config::quote(&ty)
            )))
        }
    };
    Ok((db, tbl, kind))
}

impl VirtualTableModule for VocabModule {
    type Table = VocabTable;

    fn schema(&self, args: &[Value]) -> Result<String> {
        let (_, _, kind) = parse_args("main", args)?;
        Ok(SCHEMAS[kind as usize].to_string())
    }

    fn create(&self, args: &[Value]) -> Result<VocabTable> {
        let (db, tbl, kind) = parse_args("main", args)?;
        Ok(VocabTable { db, tbl, kind })
    }

    fn connect(&self, info: &ConnectInfo, args: &[Value]) -> Result<(VocabTable, String)> {
        let (db, tbl, kind) = parse_args(info.schema_name, args)?;
        Ok((
            VocabTable { db, tbl, kind },
            SCHEMAS[kind as usize].to_string(),
        ))
    }

    fn innocuous(&self) -> bool {
        true
    }
}

#[derive(Debug)]
pub(crate) struct VocabTable {
    db: String,
    tbl: String,
    kind: Kind,
}

impl VirtualTable for VocabTable {
    type Cursor = VocabCursor;

    fn open(&self, connection: Arc<Connection>) -> Result<VocabCursor> {
        let inner = find_fts5_table(&connection, &self.db, &self.tbl)?;
        Ok(VocabCursor {
            conn: connection,
            inner,
            kind: self.kind,
            rows: Vec::new(),
            pos: 0,
        })
    }

    /// fts5VocabBestIndexMethod
    fn best_index(
        &self,
        constraints: &[ConstraintInfo],
        order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        let (mut eq, mut ge, mut le) = (None, None, None);
        for (i, c) in constraints.iter().enumerate() {
            if !c.usable || c.column_index != 0 {
                continue;
            }
            match c.op {
                ConstraintOp::Eq => eq = Some(i),
                ConstraintOp::Le | ConstraintOp::Lt => le = Some(i),
                ConstraintOp::Ge | ConstraintOp::Gt => ge = Some(i),
                _ => {}
            }
        }
        let mut usages = vec![
            ConstraintUsage {
                argv_index: None,
                omit: false,
            };
            constraints.len()
        ];
        let mut idx_num = 0xFF;
        let mut narg = 0;
        let mut cost = 1_000_000.0;
        if let Some(i) = eq {
            idx_num |= TERM_EQ;
            narg += 1;
            usages[i].argv_index = Some(narg);
            cost = 100.0;
        } else {
            if let Some(i) = ge {
                idx_num |= TERM_GE;
                narg += 1;
                usages[i].argv_index = Some(narg);
                cost /= 2.0;
            }
            if let Some(i) = le {
                idx_num |= TERM_LE;
                narg += 1;
                usages[i].argv_index = Some(narg);
                cost /= 2.0;
            }
        }
        Ok(IndexInfo {
            idx_num,
            idx_str: None,
            order_by_consumed: order_by.len() == 1
                && order_by[0].column_index == 0
                && !order_by[0].desc,
            estimated_cost: cost,
            estimated_rows: u32::MAX,
            constraint_usages: usages,
        })
    }
}

/// Finds an fts5 table through its `*id` special query, as SQLite does.
fn find_fts5_table(conn: &Arc<Connection>, db: &str, tbl: &str) -> Result<Arc<TableInner>> {
    let q = super::config::quote_ident;
    let sql = format!(
        "SELECT t.{} FROM {}.{} AS t WHERE t.{} MATCH '*id'",
        q(tbl),
        q(db),
        q(tbl),
        q(tbl)
    );
    let no_table = || err(format!("no such fts5 table: {db}.{tbl}"));
    let row = sql::query_row(conn, &sql, &[]).map_err(|_| no_table())?;
    let id = row.map(|r| sql::value_i64(&r[0])).ok_or_else(no_table)?;
    super::table_by_special_id(id).ok_or_else(no_table)
}

pub(crate) struct VocabCursor {
    conn: Arc<Connection>,
    inner: Arc<TableInner>,
    kind: Kind,
    rows: Vec<Vec<Value>>,
    pos: usize,
}

/// Counts of zero read as NULL, as in fts5VocabColumnMethod.
fn count(v: i64) -> Value {
    if v > 0 {
        Value::from_i64(v)
    } else {
        Value::Null
    }
}

fn text(b: &[u8]) -> Value {
    Value::build_text(String::from_utf8_lossy(b).into_owned())
}

impl VocabCursor {
    fn filter_rows(&mut self, args: &[Value], idx_num: i32) -> Result<()> {
        self.rows.clear();
        self.pos = 0;
        let mut k = 0;
        let mut arg = |bit: i32| -> Option<Vec<u8>> {
            if idx_num & bit == 0 {
                return None;
            }
            let v = args.get(k).cloned();
            k += 1;
            match v {
                Some(Value::Text(t)) => Some(t.as_str().as_bytes().to_vec()),
                _ => None,
            }
        };
        let eq = arg(TERM_EQ);
        let ge = arg(TERM_GE);
        let le = arg(TERM_LE);
        let cfg = &self.inner.cfg;
        let vals = self.inner.vals(&self.conn)?;
        let index = Index::new(&self.conn, cfg, &vals);
        let (s, _) = index.store.read_structure()?;
        let writes = self.inner.peek_writes(&self.conn);
        let pending = writes.as_ref().map(|w| &w.pending);
        let ncol = cfg.ncol();
        let detail = cfg.detail;
        // (term, rowid, poslist) in term order.
        let entries: Vec<(Vec<u8>, i64, Vec<u8>)> = if let Some(term) = &eq {
            let mut key = vec![MAIN_PREFIX];
            key.extend_from_slice(term);
            index
                .query_term(&s, pending, &key)?
                .into_iter()
                .map(|(r, p)| (term.clone(), r, p))
                .collect()
        } else {
            let mut key = vec![MAIN_PREFIX];
            key.extend_from_slice(ge.as_deref().unwrap_or(&[]));
            let mut out = Vec::new();
            let mut m = index.iter_all(&s, pending, true)?;
            while let Some(e) = m.next()? {
                if e.term.first() != Some(&MAIN_PREFIX) || e.term < key {
                    continue;
                }
                out.push((e.term[1..].to_vec(), e.rowid, e.poslist));
            }
            out
        };
        let mut rowid = 0i64;
        let mut i = 0;
        while i < entries.len() {
            let term = entries[i].0.clone();
            if le.as_ref().is_some_and(|l| term > *l) {
                break;
            }
            let mut j = i;
            while j < entries.len() && entries[j].0 == term {
                j += 1;
            }
            let group = &entries[i..j];
            let tval = text(&term);
            match self.kind {
                Kind::Row => {
                    let mut cnt = 0;
                    if detail == Detail::Full {
                        for (_, _, pl) in group {
                            let mut off = 0;
                            while off < pl.len() {
                                if get_varint32(pl, &mut off) == 1 {
                                    get_varint32(pl, &mut off);
                                } else {
                                    cnt += 1;
                                }
                            }
                        }
                    }
                    rowid += 1;
                    self.rows.push(vec![
                        Value::from_i64(rowid),
                        tval,
                        count(group.len() as i64),
                        count(cnt),
                    ]);
                }
                Kind::Col => {
                    let n = ncol.max(1);
                    let mut doc = vec![0i64; n];
                    let mut cnt = vec![0i64; n];
                    for (_, _, pl) in group {
                        match detail {
                            Detail::Full => {
                                let mut off = 0;
                                let mut pos = 0i64;
                                let mut last = -1;
                                while let Some(p) = poslist_next(pl, &mut off, &mut pos) {
                                    let c = pos_col(p);
                                    if c as usize >= ncol {
                                        return Err(sql::corrupt());
                                    }
                                    if c != last {
                                        doc[c as usize] += 1;
                                        last = c;
                                    }
                                    cnt[c as usize] += 1;
                                }
                            }
                            Detail::Columns => {
                                let mut off = 0;
                                let mut c = 0i64;
                                while off < pl.len() {
                                    c += get_varint32(pl, &mut off) as i64 - 2;
                                    if c < 0 || c as usize >= ncol {
                                        return Err(sql::corrupt());
                                    }
                                    doc[c as usize] += 1;
                                }
                            }
                            Detail::None => doc[0] += 1,
                        }
                    }
                    for c in 0..n {
                        if doc[c] == 0 {
                            continue;
                        }
                        rowid += 1;
                        let col = if detail == Detail::None {
                            Value::Null
                        } else {
                            Value::build_text(cfg.cols[c].clone())
                        };
                        self.rows.push(vec![
                            Value::from_i64(rowid),
                            tval.clone(),
                            col,
                            count(doc[c]),
                            count(cnt[c]),
                        ]);
                    }
                }
                Kind::Instance => {
                    let colname = |c: usize| {
                        cfg.cols
                            .get(c)
                            .map_or(Value::Null, |n| Value::build_text(n.clone()))
                    };
                    for (_, r, pl) in group {
                        let mut emit = |col: Value, off: Value| {
                            rowid += 1;
                            self.rows.push(vec![
                                Value::from_i64(rowid),
                                tval.clone(),
                                Value::from_i64(*r),
                                col,
                                off,
                            ]);
                        };
                        match detail {
                            Detail::None => emit(Value::Null, Value::Null),
                            Detail::Full => {
                                let mut off = 0;
                                let mut pos = 0i64;
                                while let Some(p) = poslist_next(pl, &mut off, &mut pos) {
                                    emit(
                                        colname(pos_col(p) as usize),
                                        Value::from_i64(pos_off(p) as i64),
                                    );
                                }
                            }
                            Detail::Columns => {
                                let mut off = 0;
                                let mut c = 0i64;
                                while off < pl.len() {
                                    c += get_varint32(pl, &mut off) as i64 - 2;
                                    emit(colname(c as usize), Value::Null);
                                }
                            }
                        }
                    }
                }
            }
            i = j;
        }
        Ok(())
    }
}

impl VirtualTableCursor for VocabCursor {
    fn filter(&mut self, args: &[Value], _idx_str: Option<&str>, idx_num: i32) -> IOResultOr<bool> {
        self.filter_rows(args, idx_num)?;
        Ok(IOResult::Done(!self.rows.is_empty()))
    }

    fn next(&mut self) -> IOResultOr<bool> {
        self.pos += 1;
        Ok(IOResult::Done(self.pos < self.rows.len()))
    }

    fn column(&mut self, column: usize) -> IOResultOr<Value> {
        Ok(IOResult::Done(
            self.rows
                .get(self.pos)
                .and_then(|r| r.get(column + 1))
                .cloned()
                .unwrap_or(Value::Null),
        ))
    }

    fn rowid(&self) -> i64 {
        self.rows.get(self.pos).map_or(0, |r| sql::value_i64(&r[0]))
    }
}
