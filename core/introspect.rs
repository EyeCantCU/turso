//! Statement and connection details behind SQLite C API calls that look
//! inside the engine: `bytecode()`/`tables_used()`, `sqlite3_txn_state`,
//! `sqlite3_column_database_name` and `sqlite3_overload_function`.

use crate::connection::TransactionState;
use crate::sync::Arc;
use crate::vdbe::builder::CursorType;
use crate::vdbe::explain::insn_to_row_with_comment;
use crate::vdbe::insn::{Insn, RegisterOrLiteral};
use crate::vdbe::PreparedProgram;
use crate::{Connection, Result, Statement, Value};

/// One row of `bytecode()`.
#[derive(Debug, Clone)]
pub struct BytecodeRow {
    /// Row number across the main program and its subprograms.
    pub rowid: i64,
    pub addr: i64,
    pub opcode: &'static str,
    pub p1: i64,
    pub p2: i64,
    pub p3: i64,
    pub p4: Value,
    pub p5: i64,
    pub comment: String,
    /// None for the main program, else "TRIGGER name" or "(FK)".
    pub subprog: Option<String>,
    /// For OpenRead/OpenWrite: ("table"|"index", schema, name, is_write).
    pub table: Option<(Option<&'static str>, String, Option<String>, bool)>,
}

impl Statement {
    /// The program as `bytecode()` lists it; with `subprograms`, trigger and
    /// foreign-key programs follow the main one in first-use order.
    pub fn bytecode(&self, subprograms: bool) -> Result<Vec<BytecodeRow>> {
        let conn = &self.program.connection;
        let dbs = conn.list_all_databases();
        let mut programs: Vec<Arc<PreparedProgram>> = vec![self.program.prepared.clone()];
        let mut rows = Vec::new();
        let mut rowid = 0;
        let mut k = 0;
        while k < programs.len() {
            let prog = programs[k].clone();
            let subprog = (k > 0).then(|| match &prog.trigger {
                Some(t) => format!("TRIGGER {}", t.name),
                None => "(FK)".to_string(),
            });
            for (addr, (insn, _)) in prog.insns.iter().enumerate() {
                if let Insn::Program { program, .. } = insn {
                    if subprograms {
                        let sub = program.prepared_program()?;
                        if !programs.iter().any(|p| Arc::ptr_eq(p, &sub)) {
                            programs.push(sub);
                        }
                    }
                }
                rowid += 1;
                let comment = prog.explain.comment_at(addr as u32);
                let (opcode, p1, p2, p3, p4, p5, comment) =
                    insn_to_row_with_comment(&prog, insn, comment);
                let table = match insn {
                    Insn::OpenRead { cursor_id, db, .. } => {
                        Some(table_used(&prog, &dbs, *cursor_id, *db, false))
                    }
                    Insn::OpenWrite {
                        cursor_id,
                        db,
                        root_page: RegisterOrLiteral::Literal(_),
                    } => Some(table_used(&prog, &dbs, *cursor_id, *db, true)),
                    _ => None,
                };
                rows.push(BytecodeRow {
                    rowid,
                    addr: addr as i64,
                    opcode,
                    p1,
                    p2,
                    p3,
                    p4,
                    p5,
                    comment,
                    subprog: subprog.clone(),
                    table,
                });
            }
            k += 1;
        }
        Ok(rows)
    }

    /// The schema of the table behind result column `idx`
    /// (`sqlite3_column_database_name`), or None for expressions.
    pub fn column_database_name(&self, idx: usize) -> Option<String> {
        if self.get_query_mode() != crate::QueryMode::Normal {
            return None;
        }
        let column = self.program.result_columns.get(idx)?;
        let turso_parser::ast::Expr::Column { table, .. } = &column.expr else {
            return None;
        };
        let refs = &self.program.table_references;
        let db = refs
            .joined_tables()
            .iter()
            .find(|t| t.internal_id == *table)
            .map(|t| t.namespace.database_id)
            .or_else(|| {
                refs.outer_query_refs()
                    .iter()
                    .find(|t| t.internal_id == *table)
                    .map(|t| t.namespace.database_id)
            })?;
        database_name(&self.program.connection, db)
    }
}

fn database_name(conn: &Connection, db: usize) -> Option<String> {
    conn.list_all_databases()
        .into_iter()
        .find(|(id, _, _)| *id == db)
        .map(|(_, name, _)| name)
}

fn table_used(
    prog: &PreparedProgram,
    dbs: &[(usize, String, String)],
    cursor_id: usize,
    db: usize,
    wr: bool,
) -> (Option<&'static str>, String, Option<String>, bool) {
    let schema = dbs
        .iter()
        .find(|(id, _, _)| *id == db)
        .map(|(_, n, _)| n.clone())
        .unwrap_or_else(|| "main".to_string());
    let (kind, name) = match prog.cursor_ref.get(cursor_id).map(|c| &c.1) {
        Some(CursorType::BTreeTable(t)) => (Some("table"), Some(t.name.clone())),
        Some(CursorType::BTreeIndex(i)) => (Some("index"), Some(i.name.clone())),
        _ => (None, None),
    };
    (kind, schema, name, wr)
}

/// `SQLITE_TXN_NONE`, `SQLITE_TXN_READ`, `SQLITE_TXN_WRITE`.
pub const TXN_NONE: i32 = 0;
pub const TXN_READ: i32 = 1;
pub const TXN_WRITE: i32 = 2;

impl Connection {
    /// `sqlite3_txn_state`: the highest state over all schemas, or that of
    /// `schema`; None when there is no such schema.
    pub fn txn_state(&self, schema: Option<&str>) -> Option<i32> {
        let dbs = self.list_all_databases();
        let state_of = |id: usize| -> i32 {
            if id == crate::MAIN_DB_ID {
                return match self.get_tx_state() {
                    TransactionState::Write { .. } => TXN_WRITE,
                    TransactionState::Read
                    | TransactionState::PendingUpgrade { has_read_txn: true } => TXN_READ,
                    _ => TXN_NONE,
                };
            }
            match self.get_pager_from_database_index(&id) {
                Ok(p) if p.holds_write_lock() => TXN_WRITE,
                Ok(p) if p.holds_read_lock() => TXN_READ,
                _ => TXN_NONE,
            }
        };
        match schema {
            Some(name) => dbs
                .iter()
                .find(|(_, n, _)| n.eq_ignore_ascii_case(name))
                .map(|(id, _, _)| state_of(*id)),
            None => Some(
                dbs.iter()
                    .map(|(id, _, _)| state_of(*id))
                    .max()
                    .unwrap_or(0),
            ),
        }
    }

    /// Disconnects the eponymous virtual tables (sqlite3_close's
    /// disconnectAllVtab); they reconnect on next use.
    pub fn disconnect_eponymous_vtabs(&self) {
        let tables = self.syms.read().take_eponymous_tables();
        // Dropped outside the lock: xDisconnect may call back in.
        drop(tables);
    }

    /// Whether a function `name` taking `n_arg` arguments exists, as
    /// `sqlite3FindFunction` decides for `sqlite3_overload_function`
    /// (-1 matches only variadic functions).
    pub fn function_exists(&self, name: &str, n_arg: i32) -> bool {
        let user = self
            .get_syms_functions()
            .into_iter()
            .any(|(n, _, argc, _)| n.eq_ignore_ascii_case(name) && argc == n_arg);
        if user {
            return true;
        }
        let builtin = |n: usize| {
            matches!(
                crate::function::Func::resolve_function(name, n),
                Ok(Some(_))
            )
        };
        if n_arg >= 0 {
            builtin(n_arg as usize)
        } else {
            // A built-in that accepts any count is variadic.
            builtin(1) && builtin(7)
        }
    }
}
