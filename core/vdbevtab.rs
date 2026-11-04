//! The `bytecode()` and `tables_used()` table-valued functions of SQLite's
//! `src/vdbevtab.c`, listing turso's VDBE program for a statement.
//!
//! The columns are SQLite's. Their content is turso's program: opcode
//! names, operands and P4 come from turso's EXPLAIN, `comment` is turso's
//! EXPLAIN comment, and `nexec`/`ncycle` are 0 (no STMT_SCANSTATUS).

use crate::introspect::BytecodeRow;
use crate::native_ext::{
    index_op, ConnectInfo, IndexInfo, IndexPlan, IndexRequest, VirtualTable, VirtualTableCursor,
    VirtualTableModule,
};
use crate::sync::Arc;
use crate::types::{IOResult, IOResultOr};
use crate::{Connection, LimboError, Result, Value};
use turso_ext::{ConstraintInfo, ConstraintUsage, OrderByInfo, ResultCode};

pub const BYTECODE_MODULE_NAME: &str = "bytecode";

/// Maps a "stmt-pointer" pointer value to the statement it designates.
pub type StmtPointerResolver = fn(usize) -> Option<*const crate::Statement>;
static STMT_POINTER: std::sync::OnceLock<StmtPointerResolver> = std::sync::OnceLock::new();

/// Installs the C API's mapping from `sqlite3_stmt*` to its statement.
pub fn set_stmt_pointer_resolver(f: StmtPointerResolver) {
    let _ = STMT_POINTER.set(f);
}
pub const TABLES_USED_MODULE_NAME: &str = "tables_used";

const BYTECODE_SCHEMA: &str = "CREATE TABLE x(addr INT,opcode TEXT,p1 INT,p2 INT,p3 INT,p4 TEXT,p5 INT,comment TEXT,subprog TEXT,nexec INT,ncycle INT,stmt HIDDEN);";
const TABLES_USED_SCHEMA: &str =
    "CREATE TABLE x(type TEXT,schema TEXT,name TEXT,wr INT,subprog TEXT,stmt HIDDEN);";

/// `bytecode` (`tables_used == false`) or `tables_used`. Register with
/// `Connection::register_vtab_module_local` as eponymous, not creatable.
#[derive(Debug, Default)]
pub struct BytecodeModule {
    pub tables_used: bool,
}

#[derive(Debug)]
pub struct BytecodeTable {
    tables_used: bool,
}

impl BytecodeModule {
    fn schema_sql(&self) -> &'static str {
        if self.tables_used {
            TABLES_USED_SCHEMA
        } else {
            BYTECODE_SCHEMA
        }
    }
}

impl VirtualTableModule for BytecodeModule {
    type Table = BytecodeTable;

    fn schema(&self, _args: &[Value]) -> Result<String> {
        Ok(self.schema_sql().to_string())
    }

    fn create(&self, _args: &[Value]) -> Result<BytecodeTable> {
        Ok(BytecodeTable {
            tables_used: self.tables_used,
        })
    }

    fn connect(&self, _info: &ConnectInfo, args: &[Value]) -> Result<(BytecodeTable, String)> {
        Ok((self.create(args)?, self.schema_sql().to_string()))
    }
}

impl VirtualTable for BytecodeTable {
    type Cursor = BytecodeCursor;

    fn open(&self, connection: Arc<Connection>) -> Result<BytecodeCursor> {
        Ok(BytecodeCursor {
            conn: connection,
            tables_used: self.tables_used,
            rows: Vec::new(),
            pos: 0,
        })
    }

    fn best_index(
        &self,
        _constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> std::result::Result<IndexInfo, ResultCode> {
        Err(ResultCode::Error)
    }

    /// Needs `stmt = ?`; `subprog IS NULL` (bytecode: `ncycle IS NULL`, as
    /// SQLite's column arithmetic has it) lists only the main program.
    fn best_index_plan(&self, req: &IndexRequest) -> Result<Option<IndexPlan>> {
        let base = if self.tables_used { 4 } else { 10 };
        let mut plan = IndexPlan::new(req.constraints.len());
        plan.estimated_cost = 100.0;
        plan.estimated_rows = 100;
        let mut ok = false;
        for (i, c) in req.constraints.iter().enumerate() {
            if !c.usable {
                continue;
            }
            if c.op == index_op::EQ && c.column == base + 1 {
                ok = true;
                plan.usages[i] = ConstraintUsage {
                    argv_index: Some(1),
                    omit: true,
                };
            }
            if c.op == index_op::ISNULL && c.column == base {
                plan.usages[i].omit = true;
                plan.idx_num = 1;
            }
        }
        Ok(ok.then_some(plan))
    }
}

pub struct BytecodeCursor {
    conn: Arc<Connection>,
    tables_used: bool,
    rows: Vec<BytecodeRow>,
    pos: usize,
}

impl BytecodeCursor {
    fn invalid(&self) -> LimboError {
        LimboError::SqlError(format!(
            "argument to {}() is not a valid SQL statement",
            if self.tables_used {
                "tables_used"
            } else {
                "bytecode"
            }
        ))
    }
}

impl VirtualTableCursor for BytecodeCursor {
    fn filter(&mut self, args: &[Value], _idx_str: Option<&str>, idx_num: i32) -> IOResultOr<bool> {
        self.rows.clear();
        self.pos = 0;
        let mut rows = match args.first() {
            Some(Value::Text(sql)) => {
                let stmt = self.conn.prepare(sql.as_str())?;
                stmt.bytecode(idx_num == 0)?
            }
            _ => {
                // sqlite3_bind_pointer(.., "stmt-pointer"): a statement the
                // caller holds, mapped by the C API's resolver.
                let stmt = crate::pointer::arg_pointer(0, b"stmt-pointer")
                    .and_then(|p| STMT_POINTER.get().and_then(|f| f(p)));
                match stmt {
                    // SAFETY: the caller keeps the statement alive while it
                    // queries bytecode(), as SQLite requires.
                    Some(s) => unsafe { (*s).bytecode(idx_num == 0)? },
                    None => return Err(Box::new(self.invalid())),
                }
            }
        };
        if self.tables_used {
            rows.retain(|r| r.table.is_some());
        }
        self.rows = rows;
        Ok(IOResult::Done(!self.rows.is_empty()))
    }

    fn next(&mut self) -> IOResultOr<bool> {
        self.pos += 1;
        Ok(IOResult::Done(self.pos < self.rows.len()))
    }

    fn column(&mut self, column: usize) -> IOResultOr<Value> {
        let Some(r) = self.rows.get(self.pos) else {
            return Ok(IOResult::Done(Value::Null));
        };
        let text = |s: &Option<String>| s.clone().map(Value::from_text).unwrap_or(Value::Null);
        let v = if self.tables_used {
            let t = r.table.as_ref();
            match column {
                0 => t
                    .and_then(|t| t.0)
                    .map(Value::from_text)
                    .unwrap_or(Value::Null),
                1 => t
                    .map(|t| Value::from_text(t.1.clone()))
                    .unwrap_or(Value::Null),
                2 => text(&t.and_then(|t| t.2.clone())),
                3 => Value::from_i64(t.is_some_and(|t| t.3) as i64),
                4 => text(&r.subprog),
                _ => Value::Null,
            }
        } else {
            match column {
                0 => Value::from_i64(r.addr),
                1 => Value::from_text(r.opcode),
                2 => Value::from_i64(r.p1),
                3 => Value::from_i64(r.p2),
                4 => Value::from_i64(r.p3),
                5 => match &r.p4 {
                    Value::Null => Value::Null,
                    Value::Text(t) => Value::from_text(t.as_str().to_string()),
                    v => Value::from_text(v.to_string()),
                },
                6 => Value::from_i64(r.p5),
                7 if r.comment.is_empty() => Value::Null,
                7 => Value::from_text(r.comment.clone()),
                8 => text(&r.subprog),
                9 | 10 => Value::from_i64(0),
                _ => Value::Null,
            }
        };
        Ok(IOResult::Done(v))
    }

    fn rowid(&self) -> i64 {
        self.rows.get(self.pos).map(|r| r.rowid).unwrap_or(0)
    }
}
