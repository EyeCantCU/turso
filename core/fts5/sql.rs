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

fn prepare(conn: &Arc<Connection>, sql: &str, params: &[Value]) -> Result<crate::Statement> {
    let mut stmt = conn.prepare_internal(sql)?;
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
