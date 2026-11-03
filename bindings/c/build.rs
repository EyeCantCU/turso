fn main() {
    println!("cargo:rustc-link-search=native=target/debug");
    println!("cargo:rerun-if-changed=src/varargs.c");
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
    emit_source_id();
    let profile_dir = target_profile_dir();
    println!("cargo:rustc-link-search=native={}", profile_dir.display());

    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        configure_msvc_sqlite3();
    }
}

/// TURSO_SOURCE_ID for sqlite3_sourceid(): "<commit date UTC> <commit id>",
/// the id zero-padded to SQLite's 64 hex digits. Without git (e.g. a source
/// tarball) the date of the tracked SQLite release and an all-zero id.
fn emit_source_id() {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .env("TZ", "UTC")
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
    println!("cargo:rerun-if-changed=src/varargs.c");
    let id = git(&["rev-parse", "HEAD"]).filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()));
    let date = git(&[
        "log",
        "-1",
        "--format=%cd",
        "--date=format-local:%Y-%m-%d %H:%M:%S",
    ]);
    let source_id = match (date, id) {
        (Some(d), Some(h)) => format!("{d} {h:0<64}"),
        _ => format!("2025-07-30 19:33:53 {:0<64}", ""),
    };
    println!("cargo:rustc-env=TURSO_SOURCE_ID={source_id}");
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
