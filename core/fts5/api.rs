//! The engine side of SQLite's `fts5_api`: custom tokenizers and auxiliary
//! functions, for the C API (`bindings/c`) and native callers.

pub use super::aux::{ApiError, ApiResult, AuxApi};
pub use super::tokenize::{
    Pattern, TokenCallback, Tokenizer, TokenizerModule, TOKENIZE_AUX, TOKENIZE_DOCUMENT,
    TOKENIZE_PREFIX, TOKENIZE_QUERY, TOKEN_COLOCATED,
};

use crate::sync::Arc;
use crate::{Connection, Result, Value};
use std::sync::OnceLock;

/// An auxiliary function, such as bm25() or a function registered with
/// `fts5_api.xCreateFunction`.
pub trait AuxFunction: Send + Sync {
    fn call(&self, api: &mut dyn AuxApi, args: &[Value]) -> Result<Value>;
}

static FTS5_HOOK: OnceLock<fn(&Arc<Connection>, usize)> = OnceLock::new();

/// Registers (or replaces) a tokenizer on `conn` (xCreateTokenizer).
pub fn register_tokenizer(conn: &Arc<Connection>, name: &str, module: Arc<dyn TokenizerModule>) {
    super::tokenize::register_module(conn, name, module);
}

/// A tokenizer visible to `conn` by name; `None` is the default (unicode61).
pub fn find_tokenizer(
    conn: Option<&Arc<Connection>>,
    name: Option<&str>,
) -> Option<Arc<dyn TokenizerModule>> {
    let _scope = super::registry::LookupScope::enter(conn);
    super::tokenize::find_module(name)
}

/// Registers an auxiliary function and makes `name(table, ...)` callable
/// on `conn`. A later registration replaces an earlier one, including the
/// built-in functions.
pub fn register_aux_function(
    conn: &Arc<Connection>,
    name: &str,
    f: Arc<dyn AuxFunction>,
) -> Result<()> {
    let key = name.to_ascii_lowercase();
    super::registry::with_mut(conn, |r| r.aux.insert(key.clone(), f));
    // As sqlite3_overload_function: a placeholder unless the name exists;
    // tables supply the real function through xFindFunction.
    let mut syms = conn.syms.write();
    if !syms.functions.contains_key(&key) {
        if let Some(f) = super::overload_placeholder(&key) {
            syms.functions.insert(key, f);
        }
    }
    drop(syms);
    conn.bump_prepare_context_generation();
    Ok(())
}

/// An auxiliary function registered on `conn` through fts5_api.
pub(crate) fn find_aux(conn: &Arc<Connection>, name: &str) -> Option<Arc<dyn AuxFunction>> {
    super::registry::with(conn, |r| r.aux.get(&name.to_ascii_lowercase()).cloned()).flatten()
}

/// Sets the handler for the SQL function `fts5(?)`, which the C API uses
/// to hand out its `fts5_api`: it gets the `fts5_api_ptr` pointer argument.
pub fn set_fts5_function_hook(f: fn(&Arc<Connection>, usize)) {
    let _ = FTS5_HOOK.set(f);
}

pub(crate) fn run_fts5_function_hook(conn: &Arc<Connection>, ptr: usize) {
    if let Some(f) = FTS5_HOOK.get() {
        f(conn, ptr);
    }
}
