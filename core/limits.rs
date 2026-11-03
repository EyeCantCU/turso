//! Per-connection limits (`sqlite3_limit`) and the prepare-time checks that
//! enforce them on a parsed statement.

use crate::translate::expr::{walk_expr, WalkControl};
use crate::{LimboError, Result};
use std::sync::atomic::{AtomicI32, Ordering};
use turso_parser::ast;

pub const LIMIT_LENGTH: i32 = 0;
pub const LIMIT_SQL_LENGTH: i32 = 1;
pub const LIMIT_COLUMN: i32 = 2;
pub const LIMIT_EXPR_DEPTH: i32 = 3;
pub const LIMIT_COMPOUND_SELECT: i32 = 4;
pub const LIMIT_VDBE_OP: i32 = 5;
pub const LIMIT_FUNCTION_ARG: i32 = 6;
pub const LIMIT_ATTACHED: i32 = 7;
pub const LIMIT_LIKE_PATTERN_LENGTH: i32 = 8;
pub const LIMIT_VARIABLE_NUMBER: i32 = 9;
pub const LIMIT_TRIGGER_DEPTH: i32 = 10;
pub const LIMIT_WORKER_THREADS: i32 = 11;
const LIMIT_COUNT: usize = 12;
/// SQLite raises a smaller SQLITE_LIMIT_LENGTH to this.
const MIN_LENGTH: i32 = 30;

/// Hard maxima, in `SQLITE_LIMIT_*` order (SQLite 3.54 defaults).
const MAXIMA: [i32; LIMIT_COUNT] = [
    1_000_000_000,
    1_000_000_000,
    2000,
    1000,
    500,
    250_000_000,
    1000,
    10,
    50_000,
    250_000,
    1000,
    8,
];

/// Initial values: the hard maxima, except worker threads (0).
const DEFAULTS: [i32; LIMIT_COUNT] = [
    1_000_000_000,
    1_000_000_000,
    2000,
    1000,
    500,
    250_000_000,
    1000,
    10,
    50_000,
    250_000,
    1000,
    0,
];

pub struct Limits([AtomicI32; LIMIT_COUNT]);

impl Default for Limits {
    fn default() -> Self {
        Self(DEFAULTS.map(AtomicI32::new))
    }
}

impl Limits {
    /// Current value of limit `id`, or `None` for an unknown id.
    pub fn get(&self, id: i32) -> Option<i32> {
        self.0
            .get(usize::try_from(id).ok()?)
            .map(|v| v.load(Ordering::Relaxed))
    }

    /// Sets limit `id`, clamped to its hard maximum, and returns the old
    /// value. A negative `value` only reads. `None` for an unknown id.
    pub fn set(&self, id: i32, value: i32) -> Option<i32> {
        let idx = usize::try_from(id).ok().filter(|&i| i < LIMIT_COUNT)?;
        if value < 0 {
            return Some(self.0[idx].load(Ordering::Relaxed));
        }
        let min = if id == LIMIT_LENGTH { MIN_LENGTH } else { 0 };
        Some(self.0[idx].swap(value.clamp(min, MAXIMA[idx]), Ordering::Relaxed))
    }

    fn at(&self, id: i32) -> usize {
        self.get(id).unwrap_or(0) as usize
    }

    /// Checks a parsed statement against the expression depth, function
    /// argument, compound select and variable number limits.
    pub fn check_cmd(&self, cmd: &ast::Cmd) -> Result<()> {
        let stmt = match cmd {
            ast::Cmd::Explain(s) | ast::Cmd::ExplainQueryPlan { stmt: s, .. } => s,
            ast::Cmd::Stmt(s) => s,
        };
        match stmt {
            ast::Stmt::Select(select) => self.select(select),
            ast::Stmt::Insert {
                with,
                body,
                returning,
                ..
            } => {
                self.with(with)?;
                if let ast::InsertBody::Select(select, _) = body {
                    self.select(select)?;
                }
                self.returning(returning)
            }
            ast::Stmt::Update(update) => {
                self.with(&update.with)?;
                for set in &update.sets {
                    self.expr(&set.expr, 1)?;
                }
                if let Some(from) = &update.from {
                    self.from(from)?;
                }
                self.opt_expr(&update.where_clause)?;
                self.returning(&update.returning)
            }
            ast::Stmt::Delete {
                with,
                where_clause,
                returning,
                ..
            } => {
                self.with(with)?;
                self.opt_expr(where_clause)?;
                self.returning(returning)
            }
            ast::Stmt::CreateTable {
                tbl_name,
                body: ast::CreateTableBody::ColumnsAndConstraints { columns, .. },
                ..
            } if columns.len() > self.at(LIMIT_COLUMN) => Err(LimboError::SqlError(format!(
                "too many columns on {}",
                tbl_name.name.as_str()
            ))),
            _ => Ok(()),
        }
    }

    fn with(&self, with: &Option<ast::With>) -> Result<()> {
        for cte in with.iter().flat_map(|w| &w.ctes) {
            self.select(&cte.select)?;
        }
        Ok(())
    }

    fn opt_expr(&self, e: &Option<Box<ast::Expr>>) -> Result<()> {
        e.as_deref().map_or(Ok(()), |e| self.expr(e, 1))
    }

    fn returning(&self, cols: &[ast::ResultColumn]) -> Result<()> {
        for col in cols {
            if let ast::ResultColumn::Expr(e, _) = col {
                self.expr(e, 1)?;
            }
        }
        Ok(())
    }

    fn select(&self, select: &ast::Select) -> Result<()> {
        self.with(&select.with)?;
        if 1 + select.body.compounds.len() > self.at(LIMIT_COMPOUND_SELECT) {
            return Err(LimboError::SqlError(
                "too many terms in compound SELECT".to_string(),
            ));
        }
        self.one_select(&select.body.select)?;
        for compound in &select.body.compounds {
            self.one_select(&compound.select)?;
        }
        for col in &select.order_by {
            self.expr(&col.expr, 1)?;
        }
        if let Some(limit) = &select.limit {
            self.expr(&limit.expr, 1)?;
            self.opt_expr(&limit.offset)?;
        }
        Ok(())
    }

    fn one_select(&self, one: &ast::OneSelect) -> Result<()> {
        match one {
            ast::OneSelect::Select {
                columns,
                from,
                where_clause,
                group_by,
                ..
            } => {
                self.returning(columns)?;
                if let Some(from) = from {
                    self.from(from)?;
                }
                self.opt_expr(where_clause)?;
                if let Some(group_by) = group_by {
                    for e in &group_by.exprs {
                        self.expr(e, 1)?;
                    }
                    self.opt_expr(&group_by.having)?;
                }
                Ok(())
            }
            ast::OneSelect::Values(rows) => {
                for e in rows.iter().flatten() {
                    self.expr(e, 1)?;
                }
                Ok(())
            }
        }
    }

    fn from(&self, from: &ast::FromClause) -> Result<()> {
        self.select_table(&from.select)?;
        for join in &from.joins {
            self.select_table(&join.table)?;
            if let Some(ast::JoinConstraint::On(e)) = &join.constraint {
                self.expr(e, 1)?;
            }
        }
        Ok(())
    }

    fn select_table(&self, table: &ast::SelectTable) -> Result<()> {
        match table {
            ast::SelectTable::Table(..) => Ok(()),
            ast::SelectTable::TableCall(_, args, _) => {
                args.iter().try_for_each(|e| self.expr(e, 1))
            }
            ast::SelectTable::Select { select, .. } => self.select(select),
            ast::SelectTable::Sub(from, _) => self.from(from),
        }
    }

    /// `depth` is the height of `e` from the root of its expression tree.
    fn expr(&self, e: &ast::Expr, depth: usize) -> Result<()> {
        let max_depth = self.at(LIMIT_EXPR_DEPTH);
        // Parentheses are not part of SQLite's expression tree.
        let is_paren = matches!(e, ast::Expr::Parenthesized(_));
        if !is_paren && depth > max_depth {
            return Err(LimboError::SqlError(format!(
                "Expression tree is too large (maximum depth {max_depth})"
            )));
        }
        match e {
            ast::Expr::FunctionCall { name, args, .. }
                if args.len() > self.at(LIMIT_FUNCTION_ARG) =>
            {
                return Err(LimboError::SqlError(format!(
                    "too many arguments on function {}",
                    name.as_str()
                )));
            }
            ast::Expr::Variable(v) if v.index.get() as usize > self.at(LIMIT_VARIABLE_NUMBER) => {
                return Err(LimboError::SqlError(if v.numbered {
                    format!(
                        "variable number must be between ?1 and ?{}",
                        self.at(LIMIT_VARIABLE_NUMBER)
                    )
                } else {
                    "too many SQL variables".to_string()
                }));
            }
            ast::Expr::Exists(select) | ast::Expr::Subquery(select) => self.select(select)?,
            ast::Expr::InSelect { rhs, .. } => self.select(rhs)?,
            _ => {}
        }
        // Direct children only: the walk stops below each of them.
        let mut children = Vec::new();
        walk_expr(e, &mut |c| {
            if std::ptr::eq(c, e) {
                Ok(WalkControl::Continue)
            } else {
                children.push(c);
                Ok(WalkControl::SkipChildren)
            }
        })?;
        let child_depth = if is_paren { depth } else { depth + 1 };
        children
            .into_iter()
            .try_for_each(|c| self.expr(c, child_depth))
    }
}
