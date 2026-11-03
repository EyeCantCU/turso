//! Compile-time authorization, modelled on `sqlite3_set_authorizer()`.
//!
//! While a statement is prepared the callback is asked about each action it
//! performs, with SQLite's action codes and arguments. It answers
//! [SQLITE_OK], [SQLITE_DENY] (prepare fails) or [SQLITE_IGNORE] (the action
//! is skipped, or a column read yields NULL).

use crate::sync::Arc;
use crate::translate::emitter::Resolver;
use crate::{LimboError, Result};
use turso_parser::ast;

pub const SQLITE_OK: i32 = 0;
pub const SQLITE_DENY: i32 = 1;
pub const SQLITE_IGNORE: i32 = 2;

pub const SQLITE_CREATE_INDEX: i32 = 1;
pub const SQLITE_CREATE_TABLE: i32 = 2;
pub const SQLITE_CREATE_TEMP_INDEX: i32 = 3;
pub const SQLITE_CREATE_TEMP_TABLE: i32 = 4;
pub const SQLITE_CREATE_TEMP_TRIGGER: i32 = 5;
pub const SQLITE_CREATE_TEMP_VIEW: i32 = 6;
pub const SQLITE_CREATE_TRIGGER: i32 = 7;
pub const SQLITE_CREATE_VIEW: i32 = 8;
pub const SQLITE_DELETE: i32 = 9;
pub const SQLITE_DROP_INDEX: i32 = 10;
pub const SQLITE_DROP_TABLE: i32 = 11;
pub const SQLITE_DROP_TEMP_INDEX: i32 = 12;
pub const SQLITE_DROP_TEMP_TABLE: i32 = 13;
pub const SQLITE_DROP_TEMP_TRIGGER: i32 = 14;
pub const SQLITE_DROP_TEMP_VIEW: i32 = 15;
pub const SQLITE_DROP_TRIGGER: i32 = 16;
pub const SQLITE_DROP_VIEW: i32 = 17;
pub const SQLITE_INSERT: i32 = 18;
pub const SQLITE_PRAGMA: i32 = 19;
pub const SQLITE_READ: i32 = 20;
pub const SQLITE_SELECT: i32 = 21;
pub const SQLITE_TRANSACTION: i32 = 22;
pub const SQLITE_UPDATE: i32 = 23;
pub const SQLITE_ATTACH: i32 = 24;
pub const SQLITE_DETACH: i32 = 25;
pub const SQLITE_ALTER_TABLE: i32 = 26;
pub const SQLITE_REINDEX: i32 = 27;
pub const SQLITE_ANALYZE: i32 = 28;
pub const SQLITE_CREATE_VTABLE: i32 = 29;
pub const SQLITE_DROP_VTABLE: i32 = 30;
pub const SQLITE_FUNCTION: i32 = 31;
pub const SQLITE_SAVEPOINT: i32 = 32;
pub const SQLITE_RECURSIVE: i32 = 33;

/// `(action, arg1, arg2, database, trigger or view)` -> SQLITE_OK / DENY / IGNORE.
pub type AuthorizerCallback =
    Arc<dyn Fn(i32, Option<&str>, Option<&str>, Option<&str>, Option<&str>) -> i32 + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Auth {
    Ok,
    Ignore,
}

fn ask(
    resolver: &Resolver,
    action: i32,
    arg1: Option<&str>,
    arg2: Option<&str>,
    db: Option<&str>,
) -> Result<Option<i32>> {
    let Some(cb) = resolver.authorizer.as_ref() else {
        return Ok(None);
    };
    let rc = cb(action, arg1, arg2, db, None);
    match rc {
        SQLITE_OK | SQLITE_DENY | SQLITE_IGNORE => Ok(Some(rc)),
        _ => Err(LimboError::InvalidArgument(
            "authorizer malfunction".to_string(),
        )),
    }
}

/// sqlite3AuthCheck: DENY fails the prepare with "not authorized".
pub(crate) fn check(
    resolver: &Resolver,
    action: i32,
    arg1: Option<&str>,
    arg2: Option<&str>,
    db: Option<&str>,
) -> Result<Auth> {
    match ask(resolver, action, arg1, arg2, db)? {
        None | Some(SQLITE_OK) => Ok(Auth::Ok),
        Some(SQLITE_IGNORE) => Ok(Auth::Ignore),
        Some(_) => Err(LimboError::AuthDenied("not authorized".to_string())),
    }
}

/// sqlite3AuthReadCol: IGNORE means the column reads as NULL.
pub(crate) fn check_read(
    resolver: &Resolver,
    table: &str,
    column: &str,
    database_id: usize,
) -> Result<Auth> {
    if resolver.authorizer.is_none() {
        return Ok(Auth::Ok);
    }
    let db = resolver
        .catalog
        .namespace_name(database_id)
        .unwrap_or_else(|| "main".to_string());
    match ask(resolver, SQLITE_READ, Some(table), Some(column), Some(&db))? {
        None | Some(SQLITE_OK) => Ok(Auth::Ok),
        Some(SQLITE_IGNORE) => Ok(Auth::Ignore),
        Some(_) => {
            let name = if database_id != crate::MAIN_DB_ID || resolver.auth_has_attached {
                format!("{db}.{table}.{column}")
            } else {
                format!("{table}.{column}")
            };
            Err(LimboError::AuthDenied(format!(
                "access to {name} is prohibited"
            )))
        }
    }
}

/// SQLITE_READ for a column (`None`: the rowid) of a table in a query.
/// Unless `always`, a column already asked about is not asked again.
pub(crate) fn check_column_read(
    resolver: &Resolver,
    tables: Option<&crate::translate::plan::TableReferences>,
    table_id: ast::TableInternalId,
    column: Option<usize>,
    always: bool,
) -> Result<Auth> {
    if resolver.authorizer.is_none() {
        return Ok(Auth::Ok);
    }
    let Some(tables) = tables else {
        return Ok(Auth::Ok);
    };
    let (table, database_id) = if let Some(t) = tables.find_joined_table_by_internal_id(table_id) {
        (&t.table, t.namespace.database_id)
    } else if let Some(t) = tables.find_outer_query_ref_by_internal_id(table_id) {
        (&t.table, t.namespace.database_id)
    } else {
        return Ok(Auth::Ok);
    };
    let crate::schema::Table::BTree(btree) = table else {
        return Ok(Auth::Ok);
    };
    if !resolver.auth_reads.borrow_mut().insert((table_id, column)) && !always {
        return Ok(Auth::Ok);
    }
    let column = column.or_else(|| btree.get_rowid_alias_column().map(|(i, _)| i));
    let name = column
        .and_then(|i| btree.columns().get(i))
        .and_then(|c| c.name.clone())
        .unwrap_or_else(|| "ROWID".to_string());
    check_read(resolver, &btree.name, &name, database_id)
}

/// SQLITE_READ for each name just bound to a column (`bound`, in order);
/// columns the authorizer ignores read as NULL in `expr`.
pub(crate) fn authorize_column_reads(
    resolver: &Resolver,
    tables: &crate::translate::plan::TableReferences,
    expr: &mut ast::Expr,
    bound: &[(ast::TableInternalId, Option<usize>)],
) -> Result<()> {
    use crate::translate::expr::{walk_expr_mut, WalkControl};
    let mut ignored = Vec::new();
    for &(table, column) in bound {
        if check_column_read(resolver, Some(tables), table, column, true)? == Auth::Ignore {
            ignored.push((table, column));
        }
    }
    if ignored.is_empty() {
        return Ok(());
    }
    walk_expr_mut(expr, &mut |e: &mut ast::Expr| -> Result<WalkControl> {
        let key = match e {
            ast::Expr::Column { table, column, .. } => (*table, Some(*column)),
            ast::Expr::RowId { table, .. } => (*table, None),
            _ => return Ok(WalkControl::Continue),
        };
        if ignored.contains(&key) {
            *e = ast::Expr::Literal(ast::Literal::Null);
        }
        Ok(WalkControl::Continue)
    })?;
    Ok(())
}

/// SQLITE_READ with an empty column for each FROM table no column is read
/// from (e.g. `SELECT count(*) FROM t`).
pub(crate) fn authorize_unread_tables(
    resolver: &Resolver,
    tables: &crate::translate::plan::TableReferences,
) -> Result<()> {
    if resolver.authorizer.is_none() {
        return Ok(());
    }
    for t in tables.joined_tables() {
        let crate::schema::Table::BTree(btree) = &t.table else {
            continue;
        };
        if t.col_used_mask.is_empty() {
            // SQLite passes the database only when the name was qualified.
            check(resolver, SQLITE_READ, Some(&btree.name), Some(""), None)?;
        }
    }
    Ok(())
}

/// SQLITE_FUNCTION: IGNORE means the call yields NULL.
pub(crate) fn check_function(resolver: &Resolver, name: &str) -> Result<Auth> {
    match ask(resolver, SQLITE_FUNCTION, None, Some(name), None)? {
        None | Some(SQLITE_OK) => Ok(Auth::Ok),
        Some(SQLITE_IGNORE) => Ok(Auth::Ignore),
        // SQLite reports this one as SQLITE_ERROR, not SQLITE_AUTH.
        Some(_) => Err(LimboError::InvalidArgument(format!(
            "not authorized to use function: {name}"
        ))),
    }
}

/// Name of the database holding `name`, or of the database CREATE targets.
fn db_of(resolver: &Resolver, name: &ast::QualifiedName, temporary: bool) -> String {
    if let Some(ns) = &name.namespace {
        return crate::util::normalize_ident(ns.as_str());
    }
    if temporary {
        return "temp".to_string();
    }
    resolver
        .catalog
        .find_existing_table(name)
        .ok()
        .flatten()
        .and_then(|ns| resolver.catalog.namespace_name(ns.database_id))
        .unwrap_or_else(|| "main".to_string())
}

fn qualified_db(resolver: &Resolver, name: &ast::QualifiedName, found: Option<usize>) -> String {
    name.namespace
        .as_ref()
        .map(|n| crate::util::normalize_ident(n.as_str()))
        .or_else(|| found.and_then(|id| resolver.catalog.namespace_name(id)))
        .unwrap_or_else(|| "main".to_string())
}

fn schema_table(db: &str) -> &'static str {
    if db.eq_ignore_ascii_case("temp") {
        "sqlite_temp_master"
    } else {
        "sqlite_master"
    }
}

fn is_temp(db: &str) -> bool {
    db.eq_ignore_ascii_case("temp")
}

/// Text SQLite passes for an ATTACH/DETACH/PRAGMA argument.
fn expr_text(expr: &ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::Literal(ast::Literal::String(s)) => {
            Some(crate::translate::expr::sanitize_string(s))
        }
        ast::Expr::Literal(ast::Literal::Numeric(n)) => Some(n.clone()),
        ast::Expr::Literal(ast::Literal::Keyword(k)) => Some(k.clone()),
        ast::Expr::Literal(ast::Literal::True) => Some("true".to_string()),
        ast::Expr::Literal(ast::Literal::False) => Some("false".to_string()),
        ast::Expr::Id(name) | ast::Expr::Name(name) => Some(name.as_str().to_string()),
        ast::Expr::Unary(ast::UnaryOperator::Negative, inner) => {
            expr_text(inner).map(|t| format!("-{t}"))
        }
        _ => None,
    }
}

type Check<'a> = (i32, Option<&'a str>, Option<&'a str>, Option<&'a str>);

/// Run `checks` in order; the first IGNORE wins.
fn check_all(resolver: &Resolver, checks: &[Check<'_>]) -> Result<Auth> {
    for &(action, a1, a2, db) in checks {
        if check(resolver, action, a1, a2, db)? == Auth::Ignore {
            return Ok(Auth::Ignore);
        }
    }
    Ok(Auth::Ok)
}

/// Statement-level checks, run before the statement is translated. Returns
/// `Auth::Ignore` when the whole statement must compile to a no-op.
pub(crate) fn authorize_statement(resolver: &mut Resolver, stmt: &mut ast::Stmt) -> Result<Auth> {
    use ast::Stmt;
    if resolver.authorizer.is_none() {
        return Ok(Auth::Ok);
    }
    let r: &Resolver = resolver;
    let all = |checks: &[Check<'_>]| -> Result<Auth> { check_all(r, checks) };
    let mut no_truncate = false;
    let mut ignored_sets: Vec<usize> = Vec::new();
    let outcome = match &*stmt {
        Stmt::Begin { .. } => all(&[(SQLITE_TRANSACTION, Some("BEGIN"), None, None)])?,
        Stmt::Commit { .. } => all(&[(SQLITE_TRANSACTION, Some("COMMIT"), None, None)])?,
        Stmt::Rollback {
            savepoint_name: None,
            ..
        } => all(&[(SQLITE_TRANSACTION, Some("ROLLBACK"), None, None)])?,
        Stmt::Rollback {
            savepoint_name: Some(name),
            ..
        } => all(&[(
            SQLITE_SAVEPOINT,
            Some("ROLLBACK"),
            Some(name.as_str()),
            None,
        )])?,
        Stmt::Savepoint { name } => {
            all(&[(SQLITE_SAVEPOINT, Some("BEGIN"), Some(name.as_str()), None)])?
        }
        Stmt::Release { name } => {
            all(&[(SQLITE_SAVEPOINT, Some("RELEASE"), Some(name.as_str()), None)])?
        }
        Stmt::Pragma { name, body } => {
            let value = body.as_ref().and_then(|b| match b {
                ast::PragmaBody::Equals(v) | ast::PragmaBody::Call(v) => expr_text(v),
            });
            let db = name
                .namespace
                .as_ref()
                .map(|n| crate::util::normalize_ident(n.as_str()));
            all(&[(
                SQLITE_PRAGMA,
                Some(name.name.as_str()),
                value.as_deref(),
                db.as_deref(),
            )])?
        }
        Stmt::Attach { expr, .. } => {
            let file = expr_text(expr);
            all(&[(SQLITE_ATTACH, file.as_deref(), None, None)])?
        }
        Stmt::Detach { name } => {
            let db = expr_text(name);
            all(&[(SQLITE_DETACH, db.as_deref(), None, None)])?
        }
        Stmt::CreateTable {
            temporary,
            tbl_name,
            ..
        } => {
            let db = db_of(r, tbl_name, *temporary);
            let code = if is_temp(&db) {
                SQLITE_CREATE_TEMP_TABLE
            } else {
                SQLITE_CREATE_TABLE
            };
            all(&[
                (SQLITE_INSERT, Some(schema_table(&db)), None, Some(&db)),
                (code, Some(tbl_name.name.as_str()), None, Some(&db)),
            ])?
        }
        Stmt::CreateView {
            temporary,
            view_name,
            ..
        } => {
            let db = db_of(r, view_name, *temporary);
            let code = if is_temp(&db) {
                SQLITE_CREATE_TEMP_VIEW
            } else {
                SQLITE_CREATE_VIEW
            };
            all(&[
                (SQLITE_INSERT, Some(schema_table(&db)), None, Some(&db)),
                (code, Some(view_name.name.as_str()), None, Some(&db)),
            ])?
        }
        Stmt::CreateVirtualTable(vtab) => {
            let db = db_of(r, &vtab.tbl_name, false);
            all(&[
                (SQLITE_INSERT, Some(schema_table(&db)), None, Some(&db)),
                (
                    SQLITE_CREATE_VTABLE,
                    Some(vtab.tbl_name.name.as_str()),
                    Some(vtab.module_name.as_str()),
                    Some(&db),
                ),
            ])?
        }
        Stmt::CreateIndex {
            idx_name, tbl_name, ..
        } => {
            let table = ast::QualifiedName {
                namespace: idx_name.namespace.clone(),
                name: tbl_name.clone(),
                alias: None,
            };
            let db = db_of(r, &table, false);
            let code = if is_temp(&db) {
                SQLITE_CREATE_TEMP_INDEX
            } else {
                SQLITE_CREATE_INDEX
            };
            all(&[
                (SQLITE_INSERT, Some(schema_table(&db)), None, Some(&db)),
                (
                    code,
                    Some(idx_name.name.as_str()),
                    Some(tbl_name.as_str()),
                    Some(&db),
                ),
            ])?
        }
        Stmt::CreateTrigger {
            temporary,
            trigger_name,
            tbl_name,
            ..
        } => {
            let db = if *temporary {
                "temp".to_string()
            } else {
                db_of(r, trigger_name, false)
            };
            let code = if is_temp(&db) {
                SQLITE_CREATE_TEMP_TRIGGER
            } else {
                SQLITE_CREATE_TRIGGER
            };
            all(&[
                (
                    code,
                    Some(trigger_name.name.as_str()),
                    Some(tbl_name.name.as_str()),
                    Some(&db),
                ),
                (SQLITE_INSERT, Some(schema_table(&db)), None, Some(&db)),
            ])?
        }
        Stmt::DropTable { tbl_name, .. }
        | Stmt::DropView {
            view_name: tbl_name,
            ..
        } => {
            let is_view = matches!(&*stmt, Stmt::DropView { .. });
            let db = db_of(r, tbl_name, false);
            let name = tbl_name.name.as_str();
            let code = match (is_view, is_temp(&db)) {
                (true, true) => SQLITE_DROP_TEMP_VIEW,
                (true, false) => SQLITE_DROP_VIEW,
                (false, true) => SQLITE_DROP_TEMP_TABLE,
                (false, false) => SQLITE_DROP_TABLE,
            };
            all(&[
                (SQLITE_DELETE, Some(schema_table(&db)), None, Some(&db)),
                (code, Some(name), None, Some(&db)),
                (SQLITE_DELETE, Some(name), None, Some(&db)),
            ])?
        }
        Stmt::DropIndex { idx_name, .. } => {
            let ns = r.catalog.find_existing_index(idx_name).ok().flatten();
            let index_name = crate::util::normalize_ident(idx_name.name.as_str());
            let table = ns.and_then(|ns| {
                r.with_schema(ns.database_id, |s| {
                    s.indexes(ns.id)
                        .find(|i| i.name.eq_ignore_ascii_case(&index_name))
                        .map(|i| i.table_name.clone())
                })
            });
            let db = qualified_db(r, idx_name, ns.map(|ns| ns.database_id));
            let code = if is_temp(&db) {
                SQLITE_DROP_TEMP_INDEX
            } else {
                SQLITE_DROP_INDEX
            };
            all(&[
                (SQLITE_DELETE, Some(schema_table(&db)), None, Some(&db)),
                (
                    code,
                    Some(idx_name.name.as_str()),
                    table.as_deref(),
                    Some(&db),
                ),
            ])?
        }
        Stmt::DropTrigger { trigger_name, .. } => {
            let ns = r.catalog.resolve_existing_trigger(trigger_name).ok();
            let table = ns.and_then(|ns| {
                r.with_schema(ns.database_id, |s| {
                    s.get_trigger(ns.id, trigger_name.name.as_str())
                        .map(|t| t.table_name.clone())
                })
            });
            let db = qualified_db(r, trigger_name, ns.map(|ns| ns.database_id));
            let code = if is_temp(&db) {
                SQLITE_DROP_TEMP_TRIGGER
            } else {
                SQLITE_DROP_TRIGGER
            };
            all(&[
                (
                    code,
                    Some(trigger_name.name.as_str()),
                    table.as_deref(),
                    Some(&db),
                ),
                (SQLITE_DELETE, Some(schema_table(&db)), None, Some(&db)),
            ])?
        }
        Stmt::AlterTable(alter) => {
            let db = db_of(r, &alter.name, false);
            let column = match &alter.body {
                ast::AlterTableBody::DropColumn(col)
                | ast::AlterTableBody::AlterColumn { old: col, .. } => Some(col.as_str()),
                _ => None,
            };
            all(&[(
                SQLITE_ALTER_TABLE,
                Some(&db),
                Some(alter.name.name.as_str()),
                column,
            )])?
        }
        Stmt::Analyze { name: Some(name) } => {
            let db = db_of(r, name, false);
            all(&[(SQLITE_ANALYZE, Some(name.name.as_str()), None, Some(&db))])?
        }
        Stmt::Reindex { name: Some(name) } => {
            let db = db_of(r, name, false);
            all(&[(SQLITE_REINDEX, Some(name.name.as_str()), None, Some(&db))])?
        }
        Stmt::Insert { tbl_name, .. } => {
            let db = db_of(r, tbl_name, false);
            all(&[(SQLITE_INSERT, Some(tbl_name.name.as_str()), None, Some(&db))])?
        }
        Stmt::Delete { tbl_name, .. } => {
            let db = db_of(r, tbl_name, false);
            // IGNORE still deletes, but row by row (no truncate optimization).
            no_truncate = check(
                r,
                SQLITE_DELETE,
                Some(tbl_name.name.as_str()),
                None,
                Some(&db),
            )? == Auth::Ignore;
            Auth::Ok
        }
        Stmt::Update(update) => {
            let db = db_of(r, &update.tbl_name, false);
            let table = update.tbl_name.name.as_str();
            for (i, set) in update.sets.iter().enumerate() {
                for col in &set.col_names {
                    if check(r, SQLITE_UPDATE, Some(table), Some(col.as_str()), Some(&db))?
                        == Auth::Ignore
                    {
                        ignored_sets.push(i);
                    }
                }
            }
            Auth::Ok
        }
        _ => Auth::Ok,
    };
    if no_truncate {
        resolver.auth_no_truncate = true;
    }
    // Other statements are authorized as a whole; their internal queries
    // (schema lookups and rewrites) are not reported, as in SQLite.
    if !matches!(
        stmt,
        Stmt::Select(..)
            | Stmt::Insert { .. }
            | Stmt::Update(..)
            | Stmt::Delete { .. }
            | Stmt::CreateTable {
                body: ast::CreateTableBody::AsSelect(..),
                ..
            }
    ) {
        resolver.authorizer = None;
    }
    if !ignored_sets.is_empty() {
        // An ignored column is left unchanged.
        if let Stmt::Update(update) = stmt {
            let mut i = 0;
            update.sets.retain(|_| {
                let keep = !ignored_sets.contains(&i);
                i += 1;
                keep
            });
            if update.sets.is_empty() {
                return Ok(Auth::Ignore);
            }
        }
    }
    Ok(outcome)
}
