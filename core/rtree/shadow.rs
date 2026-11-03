//! Blocking access to the shadow tables of an R*Tree, through helper
//! statements nested in the statement that is using the virtual table.

use crate::sync::atomic::Ordering;
use crate::sync::Arc;
use crate::{Connection, LimboError, Result, Statement, StepResult, Value};

/// Prepares `sql` as a nested statement, as index method backing stores do.
pub(crate) fn prepare(conn: &Arc<Connection>, sql: &str) -> Result<Statement> {
    conn.start_nested();
    let stmt = conn.prepare(sql);
    conn.end_nested();
    let stmt = stmt?;
    // The parent statement's transaction covers this one.
    stmt.program
        .prepared
        .needs_stmt_subtransactions
        .store(false, Ordering::Relaxed);
    Ok(stmt)
}

/// Binds `params` (1-based) and runs `stmt`, calling `row` for each row.
/// The statement is reset afterwards, so it can be run again.
pub(crate) fn run(
    conn: &Arc<Connection>,
    stmt: &mut Statement,
    params: &[Value],
    mut row: impl FnMut(&crate::Row) -> Result<bool>,
) -> Result<()> {
    conn.start_nested();
    let result = (|| {
        for (i, value) in params.iter().enumerate() {
            stmt.bind_at(
                std::num::NonZero::new(i + 1).expect("nonzero"),
                value.clone(),
            )?;
        }
        loop {
            match stmt.step()? {
                StepResult::Done => return Ok(()),
                StepResult::Row => {
                    if !row(stmt.row().expect("row is present"))? {
                        return Ok(());
                    }
                }
                StepResult::IO | StepResult::Yield | StepResult::Sleep { .. } => {
                    stmt._io().step()?
                }
                StepResult::Interrupt => return Err(LimboError::Interrupt),
                StepResult::Busy => return Err(LimboError::Busy),
            }
        }
    })();
    let reset = stmt.reset();
    conn.end_nested();
    result?;
    reset
}

/// Lazily prepared statements, indexed by the caller.
pub(crate) struct Stmts<const N: usize> {
    pub(crate) conn: Arc<Connection>,
    stmts: [Option<Statement>; N],
}

impl<const N: usize> Stmts<N> {
    pub(crate) fn new(conn: Arc<Connection>) -> Self {
        Self {
            conn,
            stmts: std::array::from_fn(|_| None),
        }
    }

    fn stmt(&mut self, idx: usize, sql: &str) -> Result<&mut Statement> {
        if self.stmts[idx].is_none() {
            self.stmts[idx] = Some(prepare(&self.conn, sql)?);
        }
        Ok(self.stmts[idx].as_mut().expect("prepared above"))
    }

    /// Runs statement `idx` and returns its first row.
    pub(crate) fn query_row(
        &mut self,
        idx: usize,
        sql: &str,
        params: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        let conn = self.conn.clone();
        let stmt = self.stmt(idx, sql)?;
        let mut out = None;
        run(&conn, stmt, params, |row| {
            out = Some(row.get_values().cloned().collect());
            Ok(false)
        })?;
        Ok(out)
    }

    /// Runs statement `idx` to completion.
    pub(crate) fn execute(&mut self, idx: usize, sql: &str, params: &[Value]) -> Result<()> {
        let conn = self.conn.clone();
        let stmt = self.stmt(idx, sql)?;
        run(&conn, stmt, params, |_| Ok(true))
    }
}

impl<const N: usize> Drop for Stmts<N> {
    fn drop(&mut self) {
        // A statement's reset reads `is_nested_stmt()`.
        self.conn.start_nested();
        for stmt in self.stmts.iter_mut() {
            *stmt = None;
        }
        self.conn.end_nested();
    }
}

/// Runs each of `sql` (one statement per entry) as a nested statement.
pub(crate) fn execute_batch(conn: &Arc<Connection>, sql: &[String]) -> Result<()> {
    for sql in sql {
        let mut stmt = prepare(conn, sql)?;
        let result = run(conn, &mut stmt, &[], |_| Ok(true));
        conn.start_nested();
        drop(stmt);
        conn.end_nested();
        result?;
    }
    Ok(())
}

/// Runs a one-off query and returns all of its rows.
pub(crate) fn query_all(
    conn: &Arc<Connection>,
    sql: &str,
    params: &[Value],
) -> Result<Vec<Vec<Value>>> {
    let mut stmt = prepare(conn, sql)?;
    let mut rows = Vec::new();
    let result = run(conn, &mut stmt, params, |row| {
        rows.push(row.get_values().cloned().collect());
        Ok(true)
    });
    conn.start_nested();
    drop(stmt);
    conn.end_nested();
    result.map(|_| rows)
}
