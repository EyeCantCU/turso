//! Running helper SQL on the connection that owns an FTS5 operation.

use crate::sync::Arc;
use crate::{Connection, LimboError, Result, Value};
use std::num::NonZero;

/// Errors from FTS5's own logic (SQLITE_ERROR with a bare message).
pub(crate) fn err(msg: impl Into<String>) -> LimboError {
    LimboError::SqlError(msg.into())
}

/// SQLITE_CORRUPT_VTAB.
pub(crate) fn corrupt() -> LimboError {
    LimboError::CorruptVtab("database disk image is malformed".into())
}

pub(crate) fn corrupt_msg(msg: impl Into<String>) -> LimboError {
    LimboError::CorruptVtab(msg.into())
}

type Compiled = (
    crate::vdbe::Program,
    Arc<crate::storage::pager::Pager>,
    crate::vdbe::builder::QueryMode,
);

/// Compiled helper statements, reused for the life of a cursor or a
/// transaction's writes. Holds programs, never live statements, so the
/// connection is only marked nested while a statement runs.
#[derive(Default)]
pub(crate) struct SqlCache(crate::sync::Mutex<std::collections::HashMap<String, Compiled>>);

impl std::fmt::Debug for SqlCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SqlCache")
    }
}

thread_local! {
    static CACHE: std::cell::Cell<*const SqlCache> = const { std::cell::Cell::new(std::ptr::null()) };
}

/// Makes `cache` the statement cache of this thread until the guard drops.
pub(crate) struct CacheScope(*const SqlCache);

pub(crate) fn cache_scope(cache: &SqlCache) -> CacheScope {
    CacheScope(CACHE.with(|c| c.replace(cache as *const SqlCache)))
}

impl Drop for CacheScope {
    fn drop(&mut self) {
        CACHE.with(|c| c.set(self.0));
    }
}

fn statement(conn: &Arc<Connection>, sql: &str) -> Result<crate::Statement> {
    let cache = CACHE.with(|c| c.get());
    if cache.is_null() {
        return conn.prepare_internal(sql);
    }
    // SAFETY: the scope guard keeps the cache alive while it is installed.
    let cache = unsafe { &*cache };
    let key = format!("{:p}{sql}", Arc::as_ptr(conn));
    if let Some((program, pager, mode)) = cache.0.lock().get(&key).cloned() {
        conn.start_nested();
        return Ok(crate::Statement::new_with_origin(
            program,
            pager,
            mode,
            0,
            crate::statement::StatementOrigin::InternalHelper,
            true,
        ));
    }
    let stmt = conn.prepare_internal(sql)?;
    let compiled = (
        stmt.get_program().clone(),
        stmt.get_pager().clone(),
        stmt.get_query_mode(),
    );
    cache.0.lock().insert(key, compiled);
    Ok(stmt)
}

fn prepare(conn: &Arc<Connection>, sql: &str, params: &[Value]) -> Result<crate::Statement> {
    let mut stmt = statement(conn, sql)?;
    for (i, v) in params.iter().enumerate() {
        stmt.bind_at(NonZero::new(i + 1).expect("nonzero"), v.clone())?;
    }
    Ok(stmt)
}

/// Runs a statement and discards its rows.
pub(crate) fn exec(conn: &Arc<Connection>, sql: &str, params: &[Value]) -> Result<()> {
    prepare(conn, sql, params)?.run_ignore_rows()
}

/// Runs a statement and returns all rows.
pub(crate) fn query(
    conn: &Arc<Connection>,
    sql: &str,
    params: &[Value],
) -> Result<Vec<Vec<Value>>> {
    prepare(conn, sql, params)?.run_collect_rows()
}

/// Runs a statement and returns its first row, if any.
pub(crate) fn query_row(
    conn: &Arc<Connection>,
    sql: &str,
    params: &[Value],
) -> Result<Option<Vec<Value>>> {
    let mut stmt = prepare(conn, sql, params)?;
    let row = stmt
        .run_one_step_blocking(|| Ok(()), || Ok(()))?
        .map(|row| row.get_values().cloned().collect());
    Ok(row)
}

pub(crate) fn blob(data: &[u8]) -> Value {
    Value::from_slice(data).unwrap_or(Value::Null)
}

pub(crate) fn int(v: i64) -> Value {
    Value::from_i64(v)
}

pub(crate) fn text(s: &str) -> Value {
    Value::build_text(s.to_string())
}

/// Bytes of a BLOB or TEXT value; other types read as empty.
pub(crate) fn value_bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::Blob(b) => b.to_vec(),
        Value::Text(t) => t.as_str().as_bytes().to_vec(),
        Value::Null => Vec::new(),
        other => other.to_string().into_bytes(),
    }
}

pub(crate) fn value_i64(v: &Value) -> i64 {
    match v {
        Value::Numeric(crate::numeric::Numeric::Integer(i)) => *i,
        Value::Numeric(crate::numeric::Numeric::Float(f)) => f64::from(*f) as i64,
        Value::Text(t) => crate::numeric::str_to_i64(t.as_str()).unwrap_or(0),
        _ => 0,
    }
}
