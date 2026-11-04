//! The engine side of SQLite's `fts5_api`: custom tokenizers and auxiliary
//! functions, for the C API (`bindings/c`) and native callers.

pub use super::aux::{ApiError, ApiResult, AuxApi};
pub use super::tokenize::{
    Pattern, TokenCallback, Tokenizer, TokenizerModule, TOKENIZE_AUX, TOKENIZE_DOCUMENT,
    TOKENIZE_PREFIX, TOKENIZE_QUERY, TOKEN_COLOCATED,
};

use crate::function::ExternalFunc;
use crate::native_ext::FunctionArity;
use crate::sync::{Arc, RwLock};
use crate::{Connection, Result, Value};
use std::collections::HashMap;
use std::sync::{LazyLock, OnceLock};

/// An auxiliary function, such as bm25() or a function registered with
/// `fts5_api.xCreateFunction`.
pub trait AuxFunction: Send + Sync {
    fn call(&self, api: &mut dyn AuxApi, args: &[Value]) -> Result<Value>;
}

type AuxRegistry = HashMap<String, Arc<dyn AuxFunction>>;

static AUX: LazyLock<RwLock<AuxRegistry>> = LazyLock::new(|| RwLock::new(HashMap::new()));
static FTS5_HOOK: OnceLock<fn(&Arc<Connection>, usize)> = OnceLock::new();

/// Registers (or replaces) a tokenizer.
pub fn register_tokenizer(name: &str, module: Arc<dyn TokenizerModule>) {
    super::tokenize::register_module(name, module);
}

/// A tokenizer by name; `None` is the default (unicode61).
pub fn find_tokenizer(name: Option<&str>) -> Option<Arc<dyn TokenizerModule>> {
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
    AUX.write().insert(key.clone(), f);
    let func = ExternalFunc::new_native_scalar(
        key.clone(),
        FunctionArity::Variadic,
        false,
        super::AuxFunction { name: key.clone() },
    )?;
    conn.syms.write().functions.insert(key, Arc::new(func));
    conn.bump_prepare_context_generation();
    Ok(())
}

pub(crate) fn find_aux(name: &str) -> Option<Arc<dyn AuxFunction>> {
    AUX.read().get(&name.to_ascii_lowercase()).cloned()
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
