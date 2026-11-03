//! Per-connection functions registered through the SQLite C API.
//!
//! Unlike extension functions they are keyed by (name, nArg, text encoding),
//! shadow built-ins, and carry SQLite's function flags.
use crate::function::ExternalFunc;
use crate::sync::Arc;
use crate::Connection;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

pub const SQLITE_FUNC_UTF8: u8 = 1;
pub const SQLITE_FUNC_DIRECTONLY: u32 = 0x0008_0000;
pub const SQLITE_FUNC_INNOCUOUS: u32 = 0x0020_0000;

#[derive(Clone)]
struct Entry {
    argc: i32,
    enc: u8,
    /// `None` once deleted: SQLite keeps the stub, which still hides a built-in.
    func: Option<Arc<ExternalFunc>>,
}

#[derive(Clone, Default)]
pub struct UserFunctions {
    by_name: HashMap<String, Vec<Entry>>,
}

pub enum UserLookup {
    /// No user function of this name accepts the argument count.
    Missing,
    /// The best match was deleted; a built-in must not take over.
    Deleted,
    Found(Arc<ExternalFunc>),
}

impl UserFunctions {
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    pub fn has_name(&self, name: &str) -> bool {
        self.by_name
            .get(&name.to_ascii_lowercase())
            .is_some_and(|entries| entries.iter().any(|e| e.func.is_some()))
    }

    /// Inserts or replaces the entry; returns the function it replaced.
    pub fn set(
        &mut self,
        name: &str,
        argc: i32,
        enc: u8,
        func: Option<Arc<ExternalFunc>>,
    ) -> Option<Arc<ExternalFunc>> {
        let entries = self.by_name.entry(name.to_ascii_lowercase()).or_default();
        if let Some(entry) = entries.iter_mut().find(|e| e.argc == argc && e.enc == enc) {
            return std::mem::replace(&mut entry.func, func);
        }
        entries.insert(0, Entry { argc, enc, func });
        None
    }

    /// SQLite's sqlite3FindFunction() ranking for a UTF-8 database: an exact
    /// argument count beats nArg = -1, then the encoding breaks ties.
    pub fn lookup(&self, name: &str, argc: usize) -> UserLookup {
        let Some(entries) = self.by_name.get(&name.to_ascii_lowercase()) else {
            return UserLookup::Missing;
        };
        let mut best: Option<(u32, &Entry)> = None;
        for entry in entries {
            let score = if entry.argc as i64 == argc as i64 {
                4
            } else if entry.argc == -1 {
                1
            } else {
                continue;
            } + u32::from(entry.enc == SQLITE_FUNC_UTF8) * 2;
            if best.is_none_or(|(s, _)| score > s) {
                best = Some((score, entry));
            }
        }
        match best {
            None => UserLookup::Missing,
            Some((_, e)) => e
                .func
                .clone()
                .map_or(UserLookup::Deleted, UserLookup::Found),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (i32, &Arc<ExternalFunc>)> {
        self.by_name
            .values()
            .flatten()
            .filter_map(|e| e.func.as_ref().map(|f| (e.argc, f)))
    }

    pub fn extend(&mut self, other: &UserFunctions) {
        for (name, entries) in &other.by_name {
            self.by_name.insert(name.clone(), entries.clone());
        }
    }
}

impl Connection {
    /// Registers (or with `None`, deletes) a user function on this
    /// connection and returns the one it replaced.
    pub fn set_user_function(
        &self,
        name: &str,
        argc: i32,
        enc: u8,
        func: Option<ExternalFunc>,
    ) -> Option<Arc<ExternalFunc>> {
        let old = self
            .syms
            .write()
            .user_functions
            .set(name, argc, enc, func.map(Arc::new));
        // Cached statements must resolve their functions again.
        self.prepare_context_generation
            .fetch_add(1, Ordering::Release);
        old
    }
}

fn reject_direct_only(
    e: &turso_parser::ast::Expr,
    syms: &crate::SymbolTable,
) -> crate::Result<crate::translate::expr::WalkControl> {
    use turso_parser::ast::Expr;
    if let Expr::FunctionCall { name, args, .. } = e {
        if let UserLookup::Found(f) = syms.resolve_user_function(name.as_str(), args.len()) {
            if f.flags & SQLITE_FUNC_DIRECTONLY != 0 {
                crate::bail_parse_error!("unsafe use of {}()", name.as_str());
            }
        }
    }
    Ok(crate::translate::expr::WalkControl::Continue)
}

/// Rejects SQLITE_DIRECTONLY functions in a schema-level expression.
pub fn check_direct_only(
    expr: &turso_parser::ast::Expr,
    syms: &crate::SymbolTable,
) -> crate::Result<()> {
    crate::translate::expr::walk_expr(expr, &mut |e| reject_direct_only(e, syms))?;
    Ok(())
}

/// Rejects SQLITE_DIRECTONLY functions anywhere in a view body.
pub fn check_direct_only_select(
    select: &turso_parser::ast::Select,
    syms: &crate::SymbolTable,
) -> crate::Result<()> {
    crate::util::walk_select_expressions(select, &mut |e| reject_direct_only(e, syms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use turso_ext::{ContextDestructor, Value, ValueDestructor};

    unsafe extern "C" fn nothing(
        _: usize,
        _: i32,
        _: *const Value,
        _: Option<ContextDestructor>,
        _: Option<ValueDestructor>,
    ) -> Value {
        Value::null()
    }

    fn func(tag: usize) -> Option<Arc<ExternalFunc>> {
        let f = ExternalFunc::new_scalar("f".into(), -1, false, tag, nothing, None, None);
        Some(Arc::new(f.unwrap()))
    }

    fn found(funcs: &UserFunctions, argc: usize) -> Option<usize> {
        match funcs.lookup("F", argc) {
            UserLookup::Found(f) => match &f.func {
                crate::function::ExtFunc::Scalar { context, .. } => Some(*context),
                _ => unreachable!(),
            },
            _ => None,
        }
    }

    #[test]
    fn exact_argument_count_beats_variadic_and_encoding_breaks_ties() {
        let mut funcs = UserFunctions::default();
        funcs.set("f", -1, SQLITE_FUNC_UTF8, func(1));
        funcs.set("f", 2, 2, func(2));
        assert_eq!(found(&funcs, 2), Some(2), "exact count over UTF-8 variadic");
        assert_eq!(found(&funcs, 3), Some(1));
        funcs.set("f", 2, SQLITE_FUNC_UTF8, func(3));
        assert_eq!(
            found(&funcs, 2),
            Some(3),
            "UTF-8 over UTF-16 at equal count"
        );
        assert!(matches!(funcs.lookup("g", 1), UserLookup::Missing));
    }

    #[test]
    fn deleted_entry_stays_a_stub() {
        let mut funcs = UserFunctions::default();
        funcs.set("f", 1, SQLITE_FUNC_UTF8, func(1));
        assert!(funcs.set("f", 1, SQLITE_FUNC_UTF8, None).is_some());
        assert!(matches!(funcs.lookup("f", 1), UserLookup::Deleted));
        assert!(matches!(funcs.lookup("f", 2), UserLookup::Missing));
        assert!(!funcs.has_name("f"));
    }
}
