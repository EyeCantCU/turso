//! Every exported C function must carry `#[capi_guard]` (panic safety net),
//! except the naked variadic trampolines. Keeps newly added exports honest.
use std::path::Path;

fn check(path: &Path, bad: &mut Vec<String>) {
    let src = std::fs::read_to_string(path).unwrap();
    let lines: Vec<&str> = src.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        if t != "#[no_mangle]" && t != "#[unsafe(no_mangle)]" {
            continue;
        }
        // Attributes and docs around the export, then the item line.
        let is_attr = |s: &str| {
            let s = s.trim();
            s.starts_with("#[") || s.starts_with("///") || s.starts_with("//")
        };
        let mut start = i;
        while start > 0 && is_attr(lines[start - 1]) {
            start -= 1;
        }
        let mut end = i + 1;
        while end < lines.len() && lines[end].trim().starts_with("#[") {
            end += 1;
        }
        let item = lines[end].trim();
        if !item.starts_with("pub ") || !item.contains("extern \"C\" fn ") {
            continue; // statics, private callbacks
        }
        let attrs = lines[start..end].join("\n");
        if attrs.contains("naked") || attrs.contains("#[capi_guard]") {
            continue;
        }
        bad.push(format!("{}:{}: {item}", path.display(), end + 1));
    }
}

#[test]
fn all_exports_are_guarded() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut bad = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "rs") {
            check(&p, &mut bad);
        }
    }
    assert!(bad.is_empty(), "missing #[capi_guard]:\n{}", bad.join("\n"));
}
