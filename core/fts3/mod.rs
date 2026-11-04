//! SQLite's FTS3 and FTS4 full-text index, ported from `ext/fts3`.
//!
//! The on-disk format (the `%_content`, `%_segments`, `%_segdir`,
//! `%_docsize` and `%_stat` shadow tables and the segment b-tree inside
//! them) is SQLite's, so tables move freely between the two engines.

mod aux;
mod ctok;
mod eval;
mod expr;
mod porter;
mod segment;
mod snippet;
mod table;
#[cfg(test)]
mod tests;
mod tokenize_vtab;
mod tokenizer;
mod unicode;
mod unicode_data;
mod write;

use crate::sync::{Arc, Mutex};
use crate::types::Value;
use crate::{Connection, LimboError, Result};
use std::collections::HashMap;
use std::num::NonZero;

pub(crate) use table::register;

/// Largest number of segments on one level before they are merged.
pub(crate) const MERGE_COUNT: i32 = 16;
pub(crate) const SEGDIR_MAXLEVEL: i64 = 1024;
pub(crate) const MAX_PENDING_DATA: usize = 1024 * 1024;
pub(crate) const VARINT_MAX: usize = 10;

/// Per-connection FTS3 state: tokenizers registered with `fts3_tokenizer()`
/// and the pending (not yet flushed) terms of each table.
#[derive(Default)]
pub(crate) struct ConnState {
    inner: Mutex<ConnInner>,
}

#[derive(Default)]
struct ConnInner {
    tokenizer_enabled: bool,
    tokenizers: HashMap<Vec<u8>, tokenizer::TokenizerModule>,
    pending: HashMap<String, write::Pending>,
}

impl ConnState {
    pub(crate) fn set_tokenizer_enabled(&self, on: Option<bool>) -> bool {
        let mut inner = self.inner.lock();
        if let Some(on) = on {
            inner.tokenizer_enabled = on;
        }
        inner.tokenizer_enabled
    }
}

impl Connection {
    /// `SQLITE_DBCONFIG_ENABLE_FTS3_TOKENIZER`: `None` queries the setting.
    pub fn fts3_tokenizer_enabled(&self, on: Option<bool>) -> bool {
        self.fts3.set_tokenizer_enabled(on)
    }
}

fn corrupt() -> LimboError {
    LimboError::CorruptVtab("database disk image is malformed".to_string())
}

fn sql_error(msg: impl Into<String>) -> LimboError {
    LimboError::SqlError(msg.into())
}

pub(crate) fn put_varint(out: &mut Vec<u8>, v: i64) {
    let mut vu = v as u64;
    loop {
        let b = (vu & 0x7f) as u8;
        vu >>= 7;
        if vu == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

pub(crate) fn varint_len(v: u64) -> usize {
    let mut v = v;
    let mut i = 0;
    loop {
        i += 1;
        v >>= 7;
        if v == 0 {
            return i;
        }
    }
}

/// Reads a varint; bytes past the end read as zero, like SQLite's padded buffers.
pub(crate) fn get_varint(buf: &[u8], off: usize) -> (i64, usize) {
    let mut b: u64 = 0;
    let mut shift = 0;
    let mut p = off;
    while shift <= 63 {
        let c = buf.get(p).copied().unwrap_or(0) as u64;
        p += 1;
        b = b.wrapping_add((c & 0x7f) << shift);
        if c & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    (b as i64, p - off)
}

/// `fts3GetVarint32`: the result is masked to 31 bits.
pub(crate) fn get_varint32(buf: &[u8], off: usize) -> (i32, usize) {
    let mut a: u32 = 0;
    for i in 0..4 {
        let c = buf.get(off + i).copied().unwrap_or(0) as u32;
        a |= (c & 0x7f) << (7 * i);
        if c & 0x80 == 0 {
            return (a as i32, i + 1);
        }
    }
    let c = buf.get(off + 4).copied().unwrap_or(0) as u32;
    ((a | ((c & 0x07) << 28)) as i32, 5)
}

/// `sqlite3Fts3Dequote`.
pub(crate) fn dequote(z: &str) -> String {
    let b = z.as_bytes();
    let Some(&q) = b.first() else {
        return String::new();
    };
    let quote = match q {
        b'[' => b']',
        b'\'' | b'"' | b'`' => q,
        _ => return z.to_string(),
    };
    let mut out = Vec::new();
    let mut i = 1;
    while i < b.len() {
        if b[i] == quote {
            if b.get(i + 1) != Some(&quote) {
                break;
            }
            out.push(quote);
            i += 2;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `sqlite3Fts3IsIdChar`.
pub(crate) fn is_id_char(c: u8) -> bool {
    c & 0x80 != 0 || c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

/// `sqlite3Fts3NextToken`: the next identifier or quoted string in `z`
/// starting at `off`, as (start, len).
pub(crate) fn next_token(z: &[u8], off: usize) -> Option<(usize, usize)> {
    let mut z1 = off;
    loop {
        let c = *z.get(z1)?;
        match c {
            b'\'' | b'"' | b'`' => {
                let mut z2 = z1;
                loop {
                    z2 += 1;
                    match z.get(z2) {
                        None => break,
                        Some(&x) if x == c => {
                            z2 += 1;
                            if z.get(z2) != Some(&c) {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                return Some((z1, z2.min(z.len()) - z1));
            }
            b'[' => {
                let mut z2 = z1 + 1;
                while z2 < z.len() && z[z2] != b']' {
                    z2 += 1;
                }
                if z2 < z.len() {
                    z2 += 1;
                }
                return Some((z1, z2 - z1));
            }
            _ if is_id_char(c) => {
                let mut z2 = z1 + 1;
                while z2 < z.len() && is_id_char(z[z2]) {
                    z2 += 1;
                }
                return Some((z1, z2 - z1));
            }
            _ => z1 += 1,
        }
    }
}

/// `%Q`-style SQL string literal.
pub(crate) fn quote_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Double-quoted SQL identifier.
pub(crate) fn quote_id(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `sqlite3_value_text` bytes; `None` for NULL.
pub(crate) fn value_text(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Null => None,
        Value::Text(t) => Some(t.as_str().as_bytes().to_vec()),
        Value::Blob(b) => Some(b.to_vec()),
        other => Some(other.to_string().into_bytes()),
    }
}

/// `sqlite3_value_int64`.
pub(crate) fn value_int(v: &Value) -> i64 {
    match v {
        Value::Null => 0,
        Value::Numeric(_) => match v.as_int() {
            Some(i) => i,
            None => {
                let f = v.as_float();
                if f.is_nan() {
                    0
                } else {
                    f as i64
                }
            }
        },
        Value::Text(t) => parse_int_prefix(t.as_str().as_bytes()),
        Value::Blob(b) => parse_int_prefix(b),
    }
}

fn parse_int_prefix(b: &[u8]) -> i64 {
    let s = std::str::from_utf8(b).unwrap_or("").trim_start();
    let mut end = 0;
    let bytes = s.as_bytes();
    if end < bytes.len() && (bytes[end] == b'-' || bytes[end] == b'+') {
        end += 1;
    }
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if let Ok(i) = s[..end].parse::<i64>() {
        return i;
    }
    s.parse::<f64>().map(|f| f as i64).unwrap_or(0)
}

/// Runs SQL on the connection that owns a table, as a nested statement.
pub(crate) struct Sql<'a> {
    pub conn: &'a Arc<Connection>,
}

impl<'a> Sql<'a> {
    pub fn new(conn: &'a Arc<Connection>) -> Self {
        Self { conn }
    }

    fn prepare(&self, sql: &str, params: &[Value]) -> Result<crate::Statement> {
        let mut stmt = self.conn.prepare_internal(sql)?;
        for (i, p) in params.iter().enumerate() {
            stmt.bind_at(NonZero::new(i + 1).unwrap(), p.clone())?;
        }
        Ok(stmt)
    }

    /// Runs a statement for its side effects.
    pub fn exec(&self, sql: &str, params: &[Value]) -> Result<()> {
        self.each(sql, params, |_| Ok(true))
    }

    /// Calls `f` for each row until it returns `false`.
    pub fn each(
        &self,
        sql: &str,
        params: &[Value],
        mut f: impl FnMut(&[Value]) -> Result<bool>,
    ) -> Result<()> {
        let mut stmt = self.prepare(sql, params)?;
        loop {
            match stmt.step()? {
                crate::vdbe::StepResult::Done => return Ok(()),
                crate::vdbe::StepResult::IO
                | crate::vdbe::StepResult::Yield
                | crate::vdbe::StepResult::Sleep { .. } => self.conn.db.io.step()?,
                crate::vdbe::StepResult::Row => {
                    let row = stmt.row().expect("row should be present");
                    let values: Vec<Value> = row.get_values().cloned().collect();
                    if !f(&values)? {
                        return Ok(());
                    }
                }
                crate::vdbe::StepResult::Interrupt => return Err(LimboError::Interrupt),
                crate::vdbe::StepResult::Busy => return Err(LimboError::Busy),
            }
        }
    }

    pub fn rows(&self, sql: &str, params: &[Value]) -> Result<Vec<Vec<Value>>> {
        let mut out = Vec::new();
        self.each(sql, params, |r| {
            out.push(r.to_vec());
            Ok(true)
        })?;
        Ok(out)
    }

    pub fn row(&self, sql: &str, params: &[Value]) -> Result<Option<Vec<Value>>> {
        let mut out = None;
        self.each(sql, params, |r| {
            out = Some(r.to_vec());
            Ok(false)
        })?;
        Ok(out)
    }
}
