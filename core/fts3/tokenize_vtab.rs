//! The fts3tokenize virtual table (`fts3_tokenize_vtab.c`).

use super::tokenizer::{find_module, tokens, Tokenizer};
use super::{dequote, sql_error, value_text};
use crate::ext::{ModuleImplementation, VTabImpl};
use crate::native_ext::{
    ConnectInfo, ConstraintInfo, ConstraintUsage, IndexInfo, OrderByInfo, VTabKind, VirtualTable,
    VirtualTableCursor, VirtualTableModule,
};
use crate::sync::{Arc, Mutex};
use crate::types::IOResultOr;
use crate::{Connection, IOResult, Result, SymbolTable, Value};
use turso_ext::{ConstraintOp, ResultCode};

const SCHEMA: &str = "CREATE TABLE x(input, token, start, end, position)";

#[derive(Debug)]
struct TokModule;

fn dequoted_args(args: &[Value]) -> Vec<Vec<u8>> {
    args.iter()
        .map(|v| {
            let s = value_text(v).unwrap_or_default();
            dequote(&String::from_utf8_lossy(&s)).into_bytes()
        })
        .collect()
}

impl VirtualTableModule for TokModule {
    type Table = TokTable;

    fn schema(&self, _args: &[Value]) -> Result<String> {
        Ok(SCHEMA.to_string())
    }

    fn create(&self, _args: &[Value]) -> Result<TokTable> {
        Err(sql_error("fts3tokenize tables are created through connect"))
    }

    fn connect(&self, info: &ConnectInfo, args: &[Value]) -> Result<(TokTable, String)> {
        let args = dequoted_args(args);
        let t = TokTable {
            args,
            tok: Mutex::new(None),
        };
        if info.connection.is_some() || t.builtin() {
            t.tokenizer(info.connection)?;
        }
        Ok((t, SCHEMA.to_string()))
    }

    fn innocuous(&self) -> bool {
        true
    }
}

struct TokTable {
    args: Vec<Vec<u8>>,
    tok: Mutex<Option<Arc<dyn Tokenizer>>>,
}

impl std::fmt::Debug for TokTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokTable").finish_non_exhaustive()
    }
}

impl TokTable {
    fn name(&self) -> Vec<u8> {
        self.args
            .first()
            .cloned()
            .unwrap_or_else(|| b"simple".to_vec())
    }

    fn builtin(&self) -> bool {
        super::tokenizer::TokenizerModule::builtin(&self.name()).is_some()
    }

    fn tokenizer(&self, conn: Option<&Arc<Connection>>) -> Result<Arc<dyn Tokenizer>> {
        let mut t = self.tok.lock();
        if let Some(t) = t.as_ref() {
            return Ok(t.clone());
        }
        let name = self.name();
        let m = find_module(conn, &name).ok_or_else(|| {
            sql_error(format!(
                "unknown tokenizer: {}",
                String::from_utf8_lossy(&name)
            ))
        })?;
        let rest = if self.args.len() > 1 {
            &self.args[1..]
        } else {
            &[][..]
        };
        let tok = m.create(rest).ok_or_else(|| sql_error("SQL logic error"))?;
        *t = Some(tok.clone());
        Ok(tok)
    }
}

impl VirtualTable for TokTable {
    type Cursor = TokCursor;

    fn open(&self, conn: Arc<Connection>) -> Result<TokCursor> {
        Ok(TokCursor {
            tok: self.tokenizer(Some(&conn))?,
            input: Vec::new(),
            toks: Vec::new(),
            next: 0,
            cur: None,
            rowid: 0,
        })
    }

    fn best_index(
        &self,
        constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> std::result::Result<IndexInfo, ResultCode> {
        let mut usages = vec![
            ConstraintUsage {
                argv_index: None,
                omit: false
            };
            constraints.len()
        ];
        for (i, c) in constraints.iter().enumerate() {
            if c.usable && c.column_index == 0 && c.op == ConstraintOp::Eq {
                usages[i] = ConstraintUsage {
                    argv_index: Some(1),
                    omit: true,
                };
                return Ok(IndexInfo {
                    idx_num: 1,
                    idx_str: None,
                    order_by_consumed: false,
                    estimated_cost: 1.0,
                    estimated_rows: 25,
                    constraint_usages: usages,
                });
            }
        }
        Ok(IndexInfo {
            idx_num: 0,
            idx_str: None,
            order_by_consumed: false,
            estimated_cost: 1e7,
            estimated_rows: 25,
            constraint_usages: usages,
        })
    }
}

struct TokCursor {
    tok: Arc<dyn Tokenizer>,
    input: Vec<u8>,
    toks: Vec<(Vec<u8>, i32, i32, i32)>,
    next: usize,
    cur: Option<(Vec<u8>, i32, i32, i32)>,
    rowid: i64,
}

impl TokCursor {
    fn advance(&mut self) {
        self.rowid += 1;
        self.cur = self.toks.get(self.next).cloned();
        self.next += 1;
        if self.cur.is_none() {
            self.input.clear();
            self.rowid = 0;
        }
    }
}

impl VirtualTableCursor for TokCursor {
    fn filter(&mut self, args: &[Value], _idx_str: Option<&str>, idx_num: i32) -> IOResultOr<bool> {
        self.cur = None;
        self.rowid = 0;
        self.next = 0;
        if idx_num != 1 {
            return Err(sql_error("SQL logic error").into());
        }
        self.input = args.first().and_then(value_text).unwrap_or_default();
        self.toks = tokens(self.tok.as_ref(), 0, &self.input)?;
        self.advance();
        Ok(IOResult::Done(self.cur.is_some()))
    }

    fn next(&mut self) -> IOResultOr<bool> {
        self.advance();
        Ok(IOResult::Done(self.cur.is_some()))
    }

    fn column(&mut self, column: usize) -> IOResultOr<Value> {
        let c = self.cur.as_ref();
        let v = match column {
            0 => Value::build_text(String::from_utf8_lossy(&self.input).into_owned()),
            1 => Value::build_text(
                String::from_utf8_lossy(c.map(|c| c.0.as_slice()).unwrap_or(&[])).into_owned(),
            ),
            2 => Value::from_i64(c.map(|c| c.1).unwrap_or(0) as i64),
            3 => Value::from_i64(c.map(|c| c.2).unwrap_or(0) as i64),
            _ => Value::from_i64(c.map(|c| c.3).unwrap_or(0) as i64),
        };
        Ok(IOResult::Done(v))
    }

    fn rowid(&self) -> i64 {
        self.rowid
    }
}

pub(crate) fn register(syms: &mut SymbolTable) {
    let module = VTabImpl {
        module_kind: VTabKind::VirtualTable,
        implementation: ModuleImplementation::Native(Arc::new(TokModule)),
        eponymous: None,
    };
    syms.vtab_modules
        .insert("fts3tokenize".to_string(), Arc::new(module));
}
