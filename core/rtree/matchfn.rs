//! Geometry callbacks for `MATCH` on R*Tree tables
//! (`sqlite3_rtree_geometry_callback` and `sqlite3_rtree_query_callback`).
//!
//! Registering a callback creates an SQL function. It returns a blob that
//! names the callback and carries the call's arguments; the R*Tree cursor
//! decodes that blob when it sees the right-hand side of `MATCH`.

use crate::native_ext::{FunctionArity, ScalarCall, ScalarFunction};
use crate::sync::atomic::{AtomicU64, Ordering};
use crate::sync::{Arc, Mutex, Weak};
use crate::types::IOResultOr;
use crate::ExternalFunc;
use crate::{Connection, IOResult, LimboError, Register, Result, Value};
use std::collections::HashMap;
use std::fmt::Debug;

/// The arguments of an `sqlite3_rtree_query_callback` call.
pub struct RtreeQueryInfo<'a> {
    /// The cell's coordinates.
    pub coords: &'a [f64],
    /// 0 for leaf entries, then 1 for leaf nodes, and so on.
    pub level: i32,
    /// The level of the root node, plus one.
    pub max_level: i32,
    /// The rowid of the most recent leaf entry.
    pub rowid: i64,
    pub parent_score: f64,
    pub parent_within: i32,
    /// Output: the cell's score (smaller is visited first).
    pub score: f64,
    /// Output: NOT_WITHIN, PARTLY_WITHIN or FULLY_WITHIN.
    pub within: i32,
    /// Queued search points per level.
    pub queue: &'a [u32],
}

/// One `MATCH` constraint during a scan.
pub trait RtreeMatchCall: Send + Sync {
    /// Old-style callback: whether the box may contain matches.
    fn geometry(&mut self, _coords: &[f64]) -> Result<bool> {
        Err(LimboError::SqlError("SQL logic error".into()))
    }
    /// New-style callback: sets `info.score` and `info.within`.
    fn query(&mut self, _info: &mut RtreeQueryInfo) -> Result<()> {
        Err(LimboError::SqlError("SQL logic error".into()))
    }
}

/// A registered geometry callback.
pub trait RtreeMatchFunction: Send + Sync + Debug {
    /// True for `sqlite3_rtree_query_callback` callbacks.
    fn is_query(&self) -> bool;
    /// Starts a constraint, given the SQL function's arguments.
    fn start(&self, params: &[Value], n_coord: usize) -> Result<Box<dyn RtreeMatchCall>>;
}

pub(crate) struct MatchConstraint {
    pub(crate) n_coord: usize,
    pub(crate) rowid: i64,
    pub(crate) call: Box<dyn RtreeMatchCall>,
    query: bool,
}

impl MatchConstraint {
    pub(crate) fn is_query(&self) -> bool {
        self.query
    }
}

const MAGIC: &[u8; 8] = b"RtMatch\0";

type Registry = HashMap<u64, Weak<dyn RtreeMatchFunction>>;

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: std::sync::OnceLock<Mutex<Registry>> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
struct MatchArgFunction {
    id: u64,
    /// Keeps the callback alive while the SQL function exists.
    _func: Arc<dyn RtreeMatchFunction>,
}

impl Drop for MatchArgFunction {
    fn drop(&mut self) {
        registry().lock().remove(&self.id);
    }
}

impl ScalarFunction for MatchArgFunction {
    type Call = MatchArgCall;

    fn create_call(&self) -> Result<Self::Call> {
        Ok(MatchArgCall { id: self.id })
    }
}

struct MatchArgCall {
    id: u64,
}

impl ScalarCall for MatchArgCall {
    type Output = Value;

    fn step(&mut self, _connection: &Arc<Connection>, args: &[Register]) -> IOResultOr<Value> {
        let mut blob = MAGIC.to_vec();
        blob.extend_from_slice(&self.id.to_le_bytes());
        for arg in args {
            encode_value(&mut blob, arg.get_value());
        }
        Ok(IOResult::Done(Value::from_slice(&blob)?))
    }
}

fn encode_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(0),
        Value::Numeric(crate::Numeric::Integer(i)) => {
            out.push(1);
            out.extend_from_slice(&i.to_le_bytes());
        }
        Value::Numeric(crate::Numeric::Float(f)) => {
            out.push(2);
            out.extend_from_slice(&f64::from(*f).to_le_bytes());
        }
        Value::Text(t) => {
            out.push(3);
            out.extend_from_slice(&(t.as_str().len() as u64).to_le_bytes());
            out.extend_from_slice(t.as_str().as_bytes());
        }
        Value::Blob(b) => {
            out.push(4);
            out.extend_from_slice(&(b.len() as u64).to_le_bytes());
            out.extend_from_slice(b);
        }
    }
}

fn decode_values(mut p: &[u8]) -> Option<Vec<Value>> {
    let mut out = Vec::new();
    let take = |p: &mut &[u8], n: usize| -> Option<Vec<u8>> {
        if p.len() < n {
            return None;
        }
        let (a, b) = p.split_at(n);
        *p = b;
        Some(a.to_vec())
    };
    while let Some((&tag, rest)) = p.split_first() {
        p = rest;
        let v = match tag {
            0 => Value::Null,
            1 => Value::from_i64(i64::from_le_bytes(take(&mut p, 8)?.try_into().ok()?)),
            2 => Value::from_f64(f64::from_le_bytes(take(&mut p, 8)?.try_into().ok()?)),
            3 | 4 => {
                let n = u64::from_le_bytes(take(&mut p, 8)?.try_into().ok()?) as usize;
                let bytes = take(&mut p, n)?;
                if tag == 3 {
                    Value::build_text(String::from_utf8(bytes).ok()?)
                } else {
                    Value::from_slice(&bytes).ok()?
                }
            }
            _ => return None,
        };
        out.push(v);
    }
    Some(out)
}

/// Decodes the right-hand side of `MATCH` and starts its callback.
pub(crate) fn decode_match_arg(
    arg: &Value,
    n_coord: usize,
    _max_level: i32,
) -> Result<MatchConstraint> {
    let logic_error = || LimboError::SqlError("SQL logic error".into());
    let Value::Blob(b) = arg else {
        return Err(logic_error());
    };
    if b.len() < 16 || &b[..8] != MAGIC {
        return Err(logic_error());
    }
    let id = u64::from_le_bytes(b[8..16].try_into().expect("8 bytes"));
    let func = registry()
        .lock()
        .get(&id)
        .and_then(Weak::upgrade)
        .ok_or_else(logic_error)?;
    let params = decode_values(&b[16..]).ok_or_else(logic_error)?;
    Ok(MatchConstraint {
        n_coord,
        rowid: 0,
        query: func.is_query(),
        call: func.start(&params, n_coord)?,
    })
}

impl Connection {
    /// Creates the SQL function `name` for use on the right of `MATCH` on an
    /// R*Tree table, as `sqlite3_rtree_query_callback` does.
    pub fn register_rtree_match_function(
        &self,
        name: &str,
        func: Arc<dyn RtreeMatchFunction>,
    ) -> Result<()> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        registry().lock().insert(id, Arc::downgrade(&func));
        let name = crate::util::normalize_ident(name);
        let ext = ExternalFunc::new_native_scalar(
            name.clone(),
            FunctionArity::Variadic,
            false,
            MatchArgFunction { id, _func: func },
        )?;
        self.syms.write().functions.insert(name, Arc::new(ext));
        self.bump_prepare_context_generation();
        Ok(())
    }
}
