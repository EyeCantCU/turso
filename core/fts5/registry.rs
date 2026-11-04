//! Per-connection FTS5 registries: tokenizers and auxiliary functions added
//! through `fts5_api` belong to the connection that registered them, as in
//! SQLite. The built-in tokenizers and functions are shared.

use super::api::AuxFunction;
use super::tokenize::TokenizerModule;
use crate::sync::{Arc, Mutex, Weak};
use crate::Connection;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Default)]
pub(crate) struct ConnRegistry {
    pub tokenizers: HashMap<String, Arc<dyn TokenizerModule>>,
    pub aux: HashMap<String, Arc<dyn AuxFunction>>,
}

/// Keyed by connection address; the Weak keeps the address from being reused
/// while its entry exists.
type Map = HashMap<usize, (Weak<Connection>, ConnRegistry)>;

static REGISTRIES: LazyLock<Mutex<Map>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn key(conn: &Arc<Connection>) -> usize {
    Arc::as_ptr(conn) as usize
}

/// Runs `f` on `conn`'s registry, creating it; drops entries of closed
/// connections.
pub(crate) fn with_mut<R>(conn: &Arc<Connection>, f: impl FnOnce(&mut ConnRegistry) -> R) -> R {
    let mut map = REGISTRIES.lock();
    map.retain(|_, (w, _)| w.strong_count() > 0);
    let entry = map
        .entry(key(conn))
        .or_insert_with(|| (Arc::downgrade(conn), ConnRegistry::default()));
    f(&mut entry.1)
}

/// Runs `f` on `conn`'s registry, if it has one.
pub(crate) fn with<R>(conn: &Arc<Connection>, f: impl FnOnce(&ConnRegistry) -> R) -> Option<R> {
    let map = REGISTRIES.lock();
    map.get(&key(conn)).map(|(_, r)| f(r))
}

/// Whether any open connection registered auxiliary function `name`.
pub(crate) fn any_aux(name: &str) -> bool {
    REGISTRIES
        .lock()
        .values()
        .any(|(w, r)| w.strong_count() > 0 && r.aux.contains_key(name))
}

thread_local! {
    /// The connection whose registry tokenizer lookups use (a tokenizer
    /// such as porter looks up its parent while being created).
    static LOOKUP: RefCell<Vec<Weak<Connection>>> = const { RefCell::new(Vec::new()) };
}

/// Makes `conn` the connection for nested lookups until dropped.
pub(crate) struct LookupScope;

impl LookupScope {
    pub(crate) fn enter(conn: Option<&Arc<Connection>>) -> Self {
        let w = conn.map(Arc::downgrade).unwrap_or_default();
        LOOKUP.with(|l| l.borrow_mut().push(w));
        LookupScope
    }
}

impl Drop for LookupScope {
    fn drop(&mut self) {
        let _ = LOOKUP.try_with(|l| l.borrow_mut().pop());
    }
}

pub(crate) fn current_conn() -> Option<Arc<Connection>> {
    LOOKUP.with(|l| l.borrow().last().and_then(Weak::upgrade))
}
