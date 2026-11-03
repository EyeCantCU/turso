fn main() {
    println!("cargo:rustc-link-search=native=target/debug");
    println!("cargo:rerun-if-changed=src/varargs.c");
    generate_api_routines();
    // varargs.c exists because some sqlite3 entry points (db_config, and
    // eventually mprintf/snprintf) are C-variadic, which stable Rust cannot
    // define. Faking them with fixed args breaks on Apple arm64, where
    // variadic arguments are passed on the stack but named arguments in
    // registers — the callee would read garbage. The exported sqlite3_*
    // symbols themselves are naked-function trampolines in lib.rs; see the
    // comment there.
    cc::Build::new()
        .file("src/varargs.c")
        .compile("turso_sqlite3_varargs");
    let profile_dir = target_profile_dir();
    println!("cargo:rustc-link-search=native={}", profile_dir.display());

    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        configure_msvc_sqlite3();
    }
}

fn target_profile_dir() -> std::path::PathBuf {
    let out_dir = std::path::PathBuf::from(
        std::env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR for build scripts"),
    );
    out_dir
        .ancestors()
        .find(|path| path.file_name().and_then(|name| name.to_str()) == Some("build"))
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .expect("OUT_DIR must be inside Cargo's target profile build directory")
}

fn configure_msvc_sqlite3() {
    for variable in [
        "VCPKG_ROOT",
        "VCPKGRS_TRIPLET",
        "VCPKGRS_DYNAMIC",
        "VCPKGRS_DISABLE",
        "VCPKGRS_NO_SQLITE3",
        "SQLITE3_NO_VCPKG",
        "NO_VCPKG",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }

    if std::env::var_os("CARGO_FEATURE_SQLITE3").is_none() {
        return;
    }

    let library = vcpkg::find_package("sqlite3").unwrap_or_else(|error| {
        panic!(
            "failed to find static SQLite with vcpkg: {error}; \
             the `sqlite3` feature on MSVC requires \
             `vcpkg install sqlite3:x64-windows-static-md`"
        )
    });
    assert!(
        library.is_static,
        "MSVC SQLite compatibility tests require static vcpkg linkage"
    );
}

/// Generates the `sqlite3_api_routines` table handed to loadable extensions.
/// `src/api_routines.list` lists the struct fields in sqlite3ext.h order as
/// `<field> <symbol>`; a slot points at the exported function when one is
/// defined in `src/*.rs`, and is NULL otherwise (as with SQLITE_OMIT_*).
fn generate_api_routines() {
    use std::collections::HashMap;
    use std::fmt::Write as _;

    println!("cargo:rerun-if-changed=src");
    let mut defined: HashMap<String, String> = HashMap::new();
    let mut files: Vec<_> = std::fs::read_dir("src")
        .expect("read src")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("rs"))
        .collect();
    files.sort();
    for path in files {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("lib");
        let module = if stem == "lib" {
            "crate".to_string()
        } else {
            format!("crate::{stem}")
        };
        let text = std::fs::read_to_string(&path).expect("read source");
        for line in text.lines() {
            let Some(pos) = line.find("extern \"C\" fn ") else {
                continue;
            };
            let rest = &line[pos + "extern \"C\" fn ".len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if name.starts_with("sqlite3_") {
                defined.entry(name).or_insert_with(|| module.clone());
            }
        }
    }
    let table = std::fs::read_to_string("src/api_routines.list").expect("read api_routines.list");
    let fields: Vec<(&str, &str)> = table.lines().filter_map(|l| l.split_once(' ')).collect();
    let mut out = String::new();
    writeln!(out, "pub const API_ROUTINES_LEN: usize = {};", fields.len()).unwrap();
    writeln!(out, "pub static API_ROUTINES: ApiRoutines = ApiRoutines([").unwrap();
    for (field, symbol) in fields {
        match defined.get(symbol) {
            Some(m) => writeln!(out, "    {m}::{symbol} as *const (), // {field}").unwrap(),
            None => writeln!(out, "    std::ptr::null(), // {field}").unwrap(),
        }
    }
    writeln!(out, "]);").unwrap();
    let dest = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"))
        .join("api_routines.rs");
    std::fs::write(dest, out).expect("write api_routines.rs");
}
