//! Statements run from inside a callback (UDF, collation, hook) while
//! another statement on the same handle is mid-step.
//!
//! SQLite lets such a nested statement read, and write other tables, inside
//! the outer statement's transaction. Core rejects a second writer on a
//! connection, except for nested statements, which join the transaction
//! that is already open and leave commit and rollback to the outer
//! statement. So a step, reset or finalize that happens inside another
//! step on the same handle, while a writer is active, runs as nested.

use super::sqlite3_stmt;
use std::cell::RefCell;
use std::sync::Arc;

thread_local! {
    /// Handles with a sqlite3_step in progress on this thread, innermost last.
    static STEPPING: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

pub(crate) struct StepScope {
    db: usize,
    pushed: bool,
    nested: Option<Arc<turso_core::Connection>>,
}

impl StepScope {
    /// Scope for one sqlite3_step call on `stmt`.
    pub(crate) fn step(stmt: &sqlite3_stmt) -> Self {
        Self::enter(stmt, true)
    }

    /// Scope for a reset or finalize of `stmt`.
    pub(crate) fn cleanup(stmt: &sqlite3_stmt) -> Self {
        Self::enter(stmt, false)
    }

    fn enter(stmt: &sqlite3_stmt, push: bool) -> Self {
        let db = Arc::as_ptr(&stmt.db) as usize;
        // TLS may already be destroyed when called from an atexit handler.
        let inside_step = STEPPING
            .try_with(|s| s.borrow().contains(&db))
            .unwrap_or(false);
        let conn = &stmt.stmt.get_program().connection;
        let nested = (inside_step && conn.has_active_write()).then(|| {
            conn.start_nested();
            conn.clone()
        });
        if push {
            let _ = STEPPING.try_with(|s| s.borrow_mut().push(db));
        }
        Self {
            db,
            pushed: push,
            nested,
        }
    }
}

impl Drop for StepScope {
    fn drop(&mut self) {
        if self.pushed {
            let _ = STEPPING.try_with(|s| {
                let mut s = s.borrow_mut();
                if let Some(i) = s.iter().rposition(|&d| d == self.db) {
                    s.remove(i);
                }
            });
        }
        if let Some(conn) = self.nested.take() {
            conn.end_nested();
        }
    }
}
