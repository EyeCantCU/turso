//! `#[capi_guard]`: turn a Rust panic inside an exported C function into an
//! error return (see `bindings/c/src/guard.rs`). Put it above `#[no_mangle]`.
use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, FnArg, ItemFn, Pat, ReturnType};

/// Int-returning functions whose result is a value, not a status code; they
/// return 0 after a panic instead of SQLITE_INTERNAL.
const VALUE_PREFIXES: &[&str] = &[
    "sqlite3_column_",
    "sqlite3_value_",
    "sqlite3_bind_parameter_",
    "sqlite3_stmt_",
    "sqlite3_changes",
    "sqlite3_total_changes",
    "sqlite3_data_count",
    "sqlite3_get_autocommit",
    "sqlite3_db_readonly",
    "sqlite3_complete",
    "sqlite3_threadsafe",
    "sqlite3_libversion_number",
    "sqlite3_stricmp",
    "sqlite3_strnicmp",
    "sqlite3_sleep",
    "sqlite3_limit",
    "sqlite3_blob_bytes",
    "sqlite3_backup_remaining",
    "sqlite3_backup_pagecount",
    "turso_test_panic_value",
];

/// Functions that may free their handle: the handle must not be touched
/// after a panic.
const NO_HANDLE: &[&str] = &["sqlite3_close", "sqlite3_close_v2", "sqlite3_finalize"];

pub(crate) fn is_status(name: &str) -> bool {
    !VALUE_PREFIXES.iter().any(|p| name.starts_with(p))
}

pub(crate) fn capi_guard(_attr: TokenStream, input: TokenStream) -> TokenStream {
    let mut func = parse_macro_input!(input as ItemFn);
    let name = func.sig.ident.to_string();
    let ret = match &func.sig.output {
        ReturnType::Default => quote!(()),
        ReturnType::Type(_, ty) => quote!(#ty),
    };
    let int_ret = ret.to_string().replace(' ', "").ends_with("c_int");

    // Handle used to record the error: only a `*mut sqlite3[_stmt]` first arg.
    let mut ctx = quote!(crate::guard::Ctx::None);
    if !NO_HANDLE.contains(&name.as_str()) {
        if let Some(FnArg::Typed(arg)) = func.sig.inputs.first() {
            let ty = {
                let t = &arg.ty;
                quote!(#t).to_string().replace(' ', "")
            };
            if let Pat::Ident(id) = &*arg.pat {
                let id = &id.ident;
                if ty == "*mutsqlite3" {
                    ctx = quote!(crate::guard::Ctx::Db(#id));
                } else if ty == "*mutsqlite3_stmt" {
                    ctx = quote!(crate::guard::Ctx::Stmt(#id));
                }
            }
        }
    }

    let fallback = if name == "sqlite3_errmsg" {
        // Never hand callers NULL for the message.
        quote!(crate::guard::INTERNAL_MSG.as_ptr())
    } else if int_ret && is_status(&name) {
        quote!(crate::guard::SQLITE_INTERNAL)
    } else {
        quote!(<#ret as crate::guard::PanicRet>::panic_ret())
    };

    let body = &func.block;
    let new_body: syn::Block = syn::parse_quote!({
        crate::guard::run(#name, #ctx, || -> #ret #body, || #fallback)
    });
    *func.block = new_body;
    quote!(#func).into()
}

#[cfg(test)]
mod tests {
    use super::is_status;

    #[test]
    fn status_vs_value() {
        assert!(is_status("sqlite3_step"));
        assert!(is_status("sqlite3_prepare_v2"));
        assert!(is_status("sqlite3_errcode"));
        assert!(!is_status("sqlite3_column_int"));
        assert!(!is_status("sqlite3_changes64"));
        assert!(!is_status("sqlite3_bind_parameter_count"));
        assert!(is_status("sqlite3_bind_int"));
    }
}
