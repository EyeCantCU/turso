//! C ABI conformance checker.
//!
//! Parses SQLite 3.54's `sqlite3.h` / `sqlite3ext.h` (vendored in `tests/abi/`)
//! and compares, parameter by parameter, every exported `sqlite3_*` function and
//! data symbol in `src/**/*.rs`, the `#define` constants in `src/` and in
//! `include/sqlite3.h`, and the declarations in `include/sqlite3.h`.
//!
//! Env: `TURSO_ABI_ROOT=<dir>` checks another `bindings/c` tree (default: this
//! crate); `TURSO_ABI_NOFAIL=1` prints the report without failing.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- types

#[derive(Clone, PartialEq, Debug)]
enum Ty {
    Void,
    /// bits, signedness (None: plain `char`, sign-agnostic)
    Int(u8, Option<bool>),
    /// size_t / usize (signed flag for ssize_t / isize)
    Size(bool),
    F32,
    F64,
    /// pointee, pointee is const
    Ptr(Box<Ty>, bool),
    Fn(Option<Box<Sig>>),
    Array(Box<Ty>),
    Opaque(String),
    Unknown(String),
}

#[derive(Clone, PartialEq, Debug)]
struct Sig {
    ret: Ty,
    params: Vec<Ty>,
    variadic: bool,
}

impl std::fmt::Display for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Ty::Void => write!(f, "void"),
            Ty::Int(b, Some(true)) => write!(f, "i{b}"),
            Ty::Int(b, Some(false)) => write!(f, "u{b}"),
            Ty::Int(b, None) => write!(f, "char{b}"),
            Ty::Size(true) => write!(f, "ssize"),
            Ty::Size(false) => write!(f, "size"),
            Ty::F32 => write!(f, "float"),
            Ty::F64 => write!(f, "double"),
            Ty::Ptr(t, c) => write!(f, "{}{}*", if *c { "const " } else { "" }, t),
            Ty::Fn(None) => write!(f, "fn"),
            Ty::Fn(Some(s)) => write!(f, "{s}"),
            Ty::Array(t) => write!(f, "{t}[]"),
            Ty::Opaque(n) => write!(f, "{n}"),
            Ty::Unknown(n) => write!(f, "?{n}"),
        }
    }
}

impl std::fmt::Display for Sig {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let mut ps: Vec<String> = self.params.iter().map(|p| p.to_string()).collect();
        if self.variadic {
            ps.push("...".into());
        }
        write!(f, "fn({}) -> {}", ps.join(", "), self.ret)
    }
}

// ---------------------------------------------------------------- lexing

#[derive(Clone, Debug, PartialEq)]
struct Tok(String);

impl PartialEq<str> for Tok {
    fn eq(&self, o: &str) -> bool {
        self.0 == o
    }
}
impl PartialEq<&str> for Tok {
    fn eq(&self, o: &&str) -> bool {
        self.0 == *o
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn strip_c_comments(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                if b[i] == '\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i += 2;
            out.push(' ');
        } else if b[i] == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if b[i] == '"' {
            out.push('"');
            i += 1;
            while i < b.len() && b[i] != '"' {
                if b[i] == '\\' {
                    out.push(b[i]);
                    i += 1;
                }
                if i < b.len() {
                    out.push(b[i]);
                }
                i += 1;
            }
            out.push('"');
            i += 1;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

fn lex_c(src: &str) -> Vec<Tok> {
    let b: Vec<char> = src.chars().collect();
    let mut t = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if is_ident_char(c) {
            let s = i;
            while i < b.len() && is_ident_char(b[i]) {
                i += 1;
            }
            t.push(Tok(b[s..i].iter().collect()));
        } else if c == '"' {
            let s = i;
            i += 1;
            while i < b.len() && b[i] != '"' {
                if b[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            t.push(Tok(b[s..i.min(b.len())].iter().collect()));
        } else if c == '.' && b.get(i + 1) == Some(&'.') && b.get(i + 2) == Some(&'.') {
            t.push(Tok("...".into()));
            i += 3;
        } else if c == '<' && b.get(i + 1) == Some(&'<') {
            t.push(Tok("<<".into()));
            i += 2;
        } else if c == '>' && b.get(i + 1) == Some(&'>') {
            t.push(Tok(">>".into()));
            i += 2;
        } else {
            t.push(Tok(c.to_string()));
            i += 1;
        }
    }
    t
}

fn lex_rust(src: &str) -> Vec<Tok> {
    let b: Vec<char> = src.chars().collect();
    let mut t = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if (c == 'r' || c == 'b')
            && matches!(
                (b.get(i + 1), b.get(i + 2)),
                (Some('"'), _) | (Some('#'), Some('"')) | (Some('#'), Some('#'))
            )
            && (i == 0 || !is_ident_char(b[i - 1]))
        {
            // raw / byte string
            let mut j = i + 1;
            let mut hashes = 0;
            while b.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            j += 1;
            loop {
                if j >= b.len() {
                    break;
                }
                if b[j] == '"' && (0..hashes).all(|k| b.get(j + 1 + k) == Some(&'#')) {
                    j += 1 + hashes;
                    break;
                }
                if hashes == 0 && c == 'b' && b[j] == '\\' {
                    j += 1;
                }
                j += 1;
            }
            i = j;
            t.push(Tok("\"str\"".into()));
        } else if c == '"' {
            let s = i + 1;
            i += 1;
            while i < b.len() && b[i] != '"' {
                if b[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            let body: String = b[s..i.min(b.len())].iter().collect();
            i += 1;
            t.push(Tok(format!("\"{body}\"")));
        } else if c == '\'' {
            // char literal or lifetime
            if b.get(i + 1) == Some(&'\\') {
                i += 2;
                while i < b.len() && b[i] != '\'' {
                    i += 1;
                }
                i += 1;
                t.push(Tok("'c'".into()));
            } else if b.get(i + 2) == Some(&'\'') {
                i += 3;
                t.push(Tok("'c'".into()));
            } else {
                i += 1;
                while i < b.len() && is_ident_char(b[i]) {
                    i += 1;
                }
            }
        } else if is_ident_char(c) {
            let s = i;
            while i < b.len()
                && (is_ident_char(b[i])
                    || (b[i] == '.'
                        && s < i
                        && b[s].is_ascii_digit()
                        && b.get(i + 1).is_some_and(|c| c.is_ascii_digit())))
            {
                i += 1;
            }
            t.push(Tok(b[s..i].iter().collect()));
        } else if c == '.' && b.get(i + 1) == Some(&'.') && b.get(i + 2) == Some(&'.') {
            t.push(Tok("...".into()));
            i += 3;
        } else if c == '-' && b.get(i + 1) == Some(&'>') {
            t.push(Tok("->".into()));
            i += 2;
        } else if c == ':' && b.get(i + 1) == Some(&':') {
            t.push(Tok("::".into()));
            i += 2;
        } else if c == '<' && b.get(i + 1) == Some(&'<') {
            t.push(Tok("<<".into()));
            i += 2;
        } else {
            t.push(Tok(c.to_string()));
            i += 1;
        }
    }
    t
}

fn matching(toks: &[Tok], open: usize) -> usize {
    let (o, c) = match toks[open].0.as_str() {
        "(" => ("(", ")"),
        "[" => ("[", "]"),
        "{" => ("{", "}"),
        _ => panic!("not an opener"),
    };
    let mut d = 0;
    for (i, t) in toks.iter().enumerate().skip(open) {
        if t == o {
            d += 1;
        } else if t == c {
            d -= 1;
            if d == 0 {
                return i;
            }
        }
    }
    toks.len() - 1
}

/// Split on top-level commas (tracking ()[]<>; `->` is its own token).
fn split_commas(toks: &[Tok]) -> Vec<Vec<Tok>> {
    let mut out = vec![];
    let mut cur = vec![];
    let mut d = 0i32;
    for t in toks {
        match t.0.as_str() {
            "(" | "[" | "<" | "{" => d += 1,
            ")" | "]" | ">" | "}" => d -= 1,
            "," if d == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(t.clone());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------- constant evaluation

struct ConstEval<'a> {
    toks: &'a [Tok],
    pos: usize,
    defs: &'a BTreeMap<String, Vec<Tok>>,
    depth: u32,
}

fn parse_int(s: &str) -> Option<i128> {
    let s = s.replace('_', "");
    let (digits, radix) = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        (h.to_string(), 16)
    } else if let Some(h) = s.strip_prefix("0b") {
        (h.to_string(), 2)
    } else if s.len() > 1 && s.starts_with('0') && s.chars().all(|c| c.is_ascii_digit()) {
        (s[1..].to_string(), 8)
    } else {
        (s.clone(), 10)
    };
    let end = digits
        .char_indices()
        .find(|(_, c)| !c.is_digit(radix))
        .map(|(i, _)| i)
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let rest = &digits[end..];
    if !(rest.is_empty()
        || rest.chars().all(|c| matches!(c, 'u' | 'U' | 'l' | 'L'))
        || rest.starts_with(['i', 'u']))
    {
        return None;
    }
    i128::from_str_radix(&digits[..end], radix).ok()
}

impl ConstEval<'_> {
    fn peek(&self) -> Option<&str> {
        self.toks.get(self.pos).map(|t| t.0.as_str())
    }
    fn expr(&mut self, min_prec: u8) -> Option<i128> {
        let mut lhs = self.unary()?;
        loop {
            while self.peek() == Some("as") {
                self.pos += 2;
            }
            let (op, prec) = match self.peek().unwrap_or("") {
                "*" => ("*", 10),
                "+" => ("+", 9),
                "-" => ("-", 9),
                "<<" => ("<<", 8),
                ">>" => (">>", 8),
                "&" => ("&", 6),
                "^" => ("^", 5),
                "|" => ("|", 4),
                _ => return Some(lhs),
            };
            if prec < min_prec {
                return Some(lhs);
            }
            self.pos += 1;
            let rhs = self.expr(prec + 1)?;
            lhs = match op {
                "*" => lhs.checked_mul(rhs)?,
                "+" => lhs.checked_add(rhs)?,
                "-" => lhs.checked_sub(rhs)?,
                "<<" => lhs.checked_shl(u32::try_from(rhs).ok()?)?,
                ">>" => lhs.checked_shr(u32::try_from(rhs).ok()?)?,
                "&" => lhs & rhs,
                "^" => lhs ^ rhs,
                _ => lhs | rhs,
            };
        }
    }
    fn unary(&mut self) -> Option<i128> {
        let t = self.peek()?.to_string();
        match t.as_str() {
            "-" => {
                self.pos += 1;
                Some(-self.unary()?)
            }
            "+" => {
                self.pos += 1;
                self.unary()
            }
            "~" => {
                self.pos += 1;
                Some(!self.unary()?)
            }
            "(" => {
                // C cast `(type)operand`: a parenthesised unknown identifier
                // followed by an operand.
                if let (Some(id), Some(")")) = (
                    self.toks.get(self.pos + 1),
                    self.toks.get(self.pos + 2).map(|t| t.0.as_str()),
                ) {
                    let next = self.toks.get(self.pos + 3).map(|t| t.0.as_str());
                    if !self.defs.contains_key(&id.0)
                        && parse_int(&id.0).is_none()
                        && matches!(next, Some(n) if n == "(" || n == "-" || n == "~" || n.chars().next().is_some_and(|c| c.is_ascii_digit()))
                    {
                        self.pos += 3;
                        return self.unary();
                    }
                }
                self.pos += 1;
                let v = self.expr(0)?;
                if self.peek() != Some(")") {
                    return None;
                }
                self.pos += 1;
                Some(v)
            }
            _ => {
                self.pos += 1;
                if let Some(n) = parse_int(&t) {
                    return Some(n);
                }
                if self.depth > 20 {
                    return None;
                }
                let body = self.defs.get(&t)?;
                let mut sub = ConstEval {
                    toks: body,
                    pos: 0,
                    defs: self.defs,
                    depth: self.depth + 1,
                };
                let v = sub.expr(0)?;
                (sub.pos == body.len()).then_some(v)
            }
        }
    }
}

fn eval_const(body: &[Tok], defs: &BTreeMap<String, Vec<Tok>>) -> Option<i128> {
    let mut e = ConstEval {
        toks: body,
        pos: 0,
        defs,
        depth: 0,
    };
    let v = e.expr(0)?;
    (e.pos == body.len()).then_some(v)
}

// ---------------------------------------------------------------- C header parsing

#[derive(Default)]
struct CHeader {
    funcs: BTreeMap<String, Sig>,
    data: BTreeMap<String, Ty>,
    defs: BTreeMap<String, Vec<Tok>>,
    /// `#define sqlite3_x sqlite3_api->y` (sqlite3ext.h)
    ext_map: BTreeMap<String, String>,
    ext_members: BTreeMap<String, Sig>,
    typedefs: BTreeMap<String, Ty>,
}

const C_DROP: &[&str] = &[
    "SQLITE_API",
    "SQLITE_APICALL",
    "SQLITE_CDECL",
    "SQLITE_STDCALL",
    "SQLITE_CALLBACK",
    "SQLITE_SYSAPI",
    "SQLITE_EXTERN",
    "SQLITE_DEPRECATED",
    "SQLITE_EXPERIMENTAL",
    "SQLITE_NOINLINE",
    "extern",
    "volatile",
    "restrict",
    "register",
    "static",
    "inline",
];

/// Remove preprocessor lines (collecting `#define`s) and skipped conditional blocks.
fn c_preprocess(src: &str, hdr: &mut CHeader) -> String {
    let text = strip_c_comments(src)
        .replace("\\\r\n", " ")
        .replace("\\\n", " ");
    let mut out = String::new();
    // stack of "skipping" flags
    let mut stack: Vec<bool> = vec![];
    let skipping = |s: &[bool]| s.iter().any(|x| *x);
    for line in text.lines() {
        let l = line.trim();
        if let Some(d) = l.strip_prefix('#') {
            let d = d.trim_start();
            let (kw, rest) = d.split_once(char::is_whitespace).unwrap_or((d, ""));
            let rest = rest.trim();
            match kw {
                "if" | "ifdef" | "ifndef" => {
                    let skip = rest == "0"
                        || (kw == "ifdef"
                            && (rest == "__cplusplus" || rest == "SQLITE_ENABLE_CEROD"))
                        || (kw == "if" && rest.contains("__cplusplus"));
                    stack.push(skip);
                }
                // every branch of a conditional is scanned (later typedefs win)
                "else" | "elif" => {}
                "endif" => {
                    stack.pop();
                }
                "define" if !skipping(&stack) => {
                    let name_end = rest.find(|c: char| !is_ident_char(c)).unwrap_or(rest.len());
                    let name = &rest[..name_end];
                    let after = &rest[name_end..];
                    if after.starts_with('(') {
                        continue;
                    }
                    let body = lex_c(after.trim());
                    if let Some(m) = body.first() {
                        if m == "sqlite3_api" && body.get(1).is_some_and(|t| t == "-") {
                            if let Some(t) = body.get(3) {
                                hdr.ext_map.insert(name.to_string(), t.0.clone());
                            }
                        }
                    }
                    hdr.defs.entry(name.to_string()).or_insert(body);
                }
                _ => {}
            }
            out.push('\n');
        } else if skipping(&stack) {
            out.push('\n');
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

const C_BASIC_ID: &[&str] = &[
    "int", "char", "short", "long", "unsigned", "signed", "double", "float", "void", "const",
    "struct", "union", "enum",
];

struct TypeCtx<'a> {
    typedefs: &'a BTreeMap<String, Ty>,
}

impl TypeCtx<'_> {
    /// Parse an abstract C type (no declarator name).
    fn c_type(&self, toks: &[Tok]) -> Ty {
        let toks: Vec<&Tok> = toks
            .iter()
            .filter(|t| !C_DROP.contains(&t.0.as_str()))
            .collect();
        if let Some(p) = toks.iter().position(|t| *t == "(") {
            // function pointer: ret ( * name ) ( params )
            let ret = self.c_type(&toks[..p].iter().map(|t| (*t).clone()).collect::<Vec<_>>());
            let owned: Vec<Tok> = toks.iter().map(|t| (*t).clone()).collect();
            let close = matching(&owned, p);
            if let Some(po) =
                (close + 1 < owned.len() && owned[close + 1] == "(").then_some(close + 1)
            {
                let pc = matching(&owned, po);
                let sig = self.c_params(&owned[po + 1..pc], ret);
                return Ty::Fn(Some(Box::new(sig)));
            }
            return Ty::Unknown("fnptr".into());
        }
        let mut stars: Vec<bool> = vec![]; // const-ness of each pointee, innermost first
        let mut base: Vec<&str> = vec![];
        let mut pending_const = false;
        let mut base_const = false;
        let mut after_star = false;
        let mut dims = 0;
        let mut i = 0;
        while i < toks.len() {
            let t = toks[i].0.as_str();
            match t {
                "const" => {
                    if after_star {
                        // `T * const` : constness of the pointer itself, ignore
                    } else {
                        base_const = true;
                        pending_const = true;
                    }
                }
                "*" => {
                    after_star = true;
                    stars.push(if stars.is_empty() {
                        base_const
                    } else {
                        pending_const
                    });
                    pending_const = false;
                }
                "[" => {
                    dims += 1;
                    i = matching(&toks.iter().map(|t| (*t).clone()).collect::<Vec<_>>(), i);
                }
                "struct" | "union" | "enum" => {
                    if let Some(n) = toks.get(i + 1) {
                        base.push("struct");
                        base.push(&n.0);
                        i += 1;
                    }
                }
                _ => base.push(t),
            }
            i += 1;
        }
        let mut ty = self.c_base(&base);
        for (k, _) in stars.iter().enumerate() {
            ty = Ty::Ptr(Box::new(ty), stars[k]);
        }
        for _ in 0..dims {
            ty = Ty::Array(Box::new(ty));
        }
        ty
    }

    fn c_base(&self, base: &[&str]) -> Ty {
        if base.first() == Some(&"struct") {
            return Ty::Opaque(base.get(1).unwrap_or(&"").to_string());
        }
        let has = |w: &str| base.contains(&w);
        let longs = base.iter().filter(|w| **w == "long").count();
        let unsigned = has("unsigned");
        if has("void") {
            return Ty::Void;
        }
        if has("double") {
            return Ty::F64;
        }
        if has("float") {
            return Ty::F32;
        }
        if has("char") {
            return Ty::Int(
                8,
                if unsigned {
                    Some(false)
                } else if has("signed") {
                    Some(true)
                } else {
                    None
                },
            );
        }
        if has("short") {
            return Ty::Int(16, Some(!unsigned));
        }
        if longs >= 2 {
            return Ty::Int(64, Some(!unsigned));
        }
        if longs == 1 {
            // LP64
            return Ty::Int(64, Some(!unsigned));
        }
        if has("int") || unsigned || has("signed") {
            return Ty::Int(32, Some(!unsigned));
        }
        let name = base.last().copied().unwrap_or("");
        match name {
            "size_t" | "uintptr_t" => Ty::Size(false),
            "ssize_t" | "intptr_t" | "ptrdiff_t" => Ty::Size(true),
            "int8_t" => Ty::Int(8, Some(true)),
            "uint8_t" => Ty::Int(8, Some(false)),
            "int16_t" => Ty::Int(16, Some(true)),
            "uint16_t" => Ty::Int(16, Some(false)),
            "int32_t" => Ty::Int(32, Some(true)),
            "uint32_t" => Ty::Int(32, Some(false)),
            "int64_t" => Ty::Int(64, Some(true)),
            "uint64_t" => Ty::Int(64, Some(false)),
            "va_list" | "__builtin_va_list" => Ty::Ptr(Box::new(Ty::Void), false),
            "" => Ty::Unknown("empty".into()),
            n => self
                .typedefs
                .get(n)
                .cloned()
                .unwrap_or_else(|| Ty::Opaque(n.to_string())),
        }
    }

    /// Parse a parameter list (tokens between the parentheses).
    fn c_params(&self, toks: &[Tok], ret: Ty) -> Sig {
        let mut params = vec![];
        let mut variadic = false;
        for p in split_commas(toks) {
            if p.len() == 1 && p[0] == "..." {
                variadic = true;
                continue;
            }
            if p.len() == 1 && p[0] == "void" {
                continue;
            }
            params.push(self.c_param(&p));
        }
        Sig {
            ret,
            params,
            variadic,
        }
    }

    /// One parameter: strip a trailing declarator name, then parse the type.
    fn c_param(&self, p: &[Tok]) -> Ty {
        let p: Vec<Tok> = p
            .iter()
            .filter(|t| !C_DROP.contains(&t.0.as_str()))
            .cloned()
            .collect();
        if p.iter().any(|t| t == "(") {
            // `ret (*name)(params)`: type parser handles it
            return self.c_type(&p);
        }
        // Drop the name: last ident, if it is not part of the type.
        let mut q = p.clone();
        // trailing array dims belong to the declarator
        let mut tail = vec![];
        while q.last().is_some_and(|t| t == "]") {
            let mut d = 0;
            while let Some(t) = q.pop() {
                tail.push(t.clone());
                if t == "]" {
                    d += 1;
                } else if t == "[" {
                    d -= 1;
                    if d == 0 {
                        break;
                    }
                }
            }
        }
        tail.reverse();
        if let Some(last) = q.last() {
            let is_id = last
                .0
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_');
            let before = q.len().checked_sub(2).map(|i| q[i].0.as_str());
            let non_type_word = C_BASIC_ID.contains(&last.0.as_str());
            let typedef_name = self.typedefs.contains_key(&last.0)
                || last.0.starts_with("sqlite")
                || last.0.ends_with("_t");
            // `struct foo` : last word is the tag; `foo` alone could be a typedef
            let after_tag = matches!(before, Some("struct") | Some("union") | Some("enum"));
            if is_id && !non_type_word && !after_tag && q.len() > 1 {
                // multi-token: last is name unless the previous token is a
                // pure modifier (`unsigned foo` is `unsigned` + name `foo`)
                q.pop();
            } else if is_id && !non_type_word && !after_tag && q.len() == 1 && !typedef_name {
                // lone unknown identifier: treat as typedef name
            }
        }
        q.extend(tail);
        self.c_type(&q)
    }
}

fn parse_c_header(src: &str, is_sqlite: bool, base: &BTreeMap<String, Ty>) -> CHeader {
    let mut hdr = CHeader {
        typedefs: base.clone(),
        ..Default::default()
    };
    let code = c_preprocess(src, &mut hdr);
    let toks = lex_c(&code);
    // Split into top-level statements.
    let mut stmts: Vec<Vec<Tok>> = vec![];
    let mut cur: Vec<Tok> = vec![];
    let mut depth = 0;
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        if depth == 0 && t == "{" && cur.len() >= 2 && cur[cur.len() - 1].0 == "\"C\"" {
            // extern "C" {  -- transparent
            cur.clear();
            i += 1;
            continue;
        }
        if depth == 0 && t == "}" {
            // closing of an `extern "C" {`
            i += 1;
            continue;
        }
        if t == "{" {
            depth += 1;
        } else if t == "}" {
            depth -= 1;
        }
        cur.push(t.clone());
        if depth == 0 && (t == ";" || t == "}") {
            if t == "}"
                && toks.get(i + 1).is_some_and(|n| n != ";" && n != "}")
                && cur.iter().any(|x| x == "typedef" || x == "struct")
            {
                // `struct x { ... } name;` continues to `;`
                i += 1;
                continue;
            }
            stmts.push(std::mem::take(&mut cur));
        }
        i += 1;
    }
    // sqlite3ext.h: struct sqlite3_api_routines members
    for s in &stmts {
        if s.iter().any(|t| t == "sqlite3_api_routines")
            && s.iter().any(|t| t == "{")
            && !s.iter().any(|t| t == "typedef")
        {
            let open = s.iter().position(|t| t == "{").unwrap();
            let close = matching(s, open);
            let mut mem = vec![];
            for t in &s[open + 1..close] {
                if t == ";" {
                    let ctx = TypeCtx {
                        typedefs: &hdr.typedefs,
                    };
                    // ret ( * name ) ( params )
                    if let Some(p) = mem.iter().position(|t: &Tok| t == "(") {
                        if mem.get(p + 1).is_some_and(|t| t == "*")
                            && p + 2 < mem.len()
                            && mem[p + 3 - 1].0.chars().all(is_ident_char)
                        {
                            let name = mem[p + 2].0.clone();
                            let ty = ctx.c_type(
                                &mem.iter()
                                    .map(|t| {
                                        if t.0 == name {
                                            Tok(String::new())
                                        } else {
                                            t.clone()
                                        }
                                    })
                                    .filter(|t| !t.0.is_empty())
                                    .collect::<Vec<_>>(),
                            );
                            if let Ty::Fn(Some(sig)) = ty {
                                hdr.ext_members.insert(name, *sig);
                            }
                        }
                    }
                    mem.clear();
                } else {
                    mem.push(t.clone());
                }
            }
        }
    }
    for s in &stmts {
        let s: Vec<Tok> = s
            .iter()
            .filter(|t| !C_DROP.contains(&t.0.as_str()))
            .cloned()
            .collect();
        if s.first().is_some_and(|t| t == "typedef") {
            let body = &s[1..s.len().saturating_sub(1)];
            let ctx = TypeCtx {
                typedefs: &hdr.typedefs,
            };
            if let Some(p) = body.iter().position(|t| t == "(") {
                // function pointer typedef: name inside (*name)
                if body.get(p + 1).is_some_and(|t| t == "*") {
                    if let Some(n) = body.get(p + 2) {
                        let name = n.0.clone();
                        let mut b2 = body.to_vec();
                        b2.remove(p + 2);
                        let ty = ctx.c_type(&b2);
                        hdr.typedefs.insert(name, ty);
                    }
                }
            } else if let Some(open) = body.iter().position(|t| t == "{") {
                let close = matching(body, open);
                if let Some(n) = body.get(close + 1) {
                    hdr.typedefs.insert(n.0.clone(), Ty::Opaque(n.0.clone()));
                }
            } else if body.len() >= 2 {
                let name = body[body.len() - 1].0.clone();
                let ty = if body[0] == "struct" || body[0] == "union" {
                    Ty::Opaque(name.clone())
                } else {
                    ctx.c_type(&body[..body.len() - 1])
                };
                hdr.typedefs.insert(name, ty);
            }
            continue;
        }
        // function or data declaration
        if s.iter().any(|t| t == "{") || s.is_empty() {
            continue;
        }
        // data symbols are collected separately below
        let Some(open) = s.iter().position(|t| t == "(") else {
            continue;
        };
        if open == 0 {
            continue;
        }
        let name = s[open - 1].0.clone();
        if !name.chars().all(is_ident_char) {
            continue;
        }
        let close = matching(&s, open);
        let ctx = TypeCtx {
            typedefs: &hdr.typedefs,
        };
        let ret = ctx.c_type(&s[..open - 1]);
        let sig = ctx.c_params(&s[open + 1..close], ret);
        hdr.funcs.insert(name, sig);
    }
    // data symbols: raw statements that carry SQLITE_API (or any `T name;`/`T name[];`
    // statement in the turso header)
    for s in &stmts {
        if s.iter().any(|t| t == "typedef" || t == "(" || t == "{") {
            continue;
        }
        let is_decl = if is_sqlite {
            s.iter().any(|t| t == "SQLITE_API")
        } else {
            true
        };
        if !is_decl {
            continue;
        }
        let s: Vec<Tok> = s
            .iter()
            .filter(|t| !C_DROP.contains(&t.0.as_str()))
            .cloned()
            .collect();
        let body = &s[..s.len().saturating_sub(1)];
        let mut b2: Vec<Tok> = body.to_vec();
        let mut dims: Vec<Tok> = vec![];
        while b2.last().is_some_and(|t| t == "]") {
            let mut d = 0;
            while let Some(t) = b2.pop() {
                dims.push(t.clone());
                if t == "]" {
                    d += 1;
                } else if t == "[" {
                    d -= 1;
                    if d == 0 {
                        break;
                    }
                }
            }
        }
        let Some(name) = b2.pop() else { continue };
        if !name
            .0
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            || b2.is_empty()
        {
            continue;
        }
        let ctx = TypeCtx {
            typedefs: &hdr.typedefs,
        };
        let mut ty = ctx.c_type(&b2);
        if !dims.is_empty() {
            ty = Ty::Array(Box::new(ty));
        }
        hdr.data.insert(name.0, ty);
    }
    hdr
}

// ---------------------------------------------------------------- Rust parsing

#[derive(Debug)]
struct RustFn {
    file: String,
    sig: Sig,
    naked: bool,
    exported: bool,
    /// declared in an `extern "C" { }` block
    imported: bool,
    /// params contain a type we could not classify
    bad: Vec<String>,
}

#[derive(Default)]
struct RustSrc {
    funcs: BTreeMap<String, Vec<RustFn>>,
    statics: BTreeMap<String, (String, Ty)>,
    consts: BTreeMap<String, Vec<Tok>>,
    aliases: BTreeMap<String, Vec<Tok>>,
}

fn rust_path_end(toks: &[Tok]) -> String {
    toks.iter()
        .rev()
        .find(|t| t.0.chars().next().is_some_and(is_ident_char))
        .map(|t| t.0.clone())
        .unwrap_or_default()
}

fn rust_type(toks: &[Tok], al: &BTreeMap<String, Vec<Tok>>, depth: u32) -> Ty {
    if toks.is_empty() {
        return Ty::Void;
    }
    if depth > 8 {
        return Ty::Unknown("deep alias".into());
    }
    let t0 = toks[0].0.as_str();
    match t0 {
        "*" => {
            let is_const = toks.get(1).is_some_and(|t| t == "const");
            Ty::Ptr(Box::new(rust_type(&toks[2..], al, depth + 1)), is_const)
        }
        "&" => {
            let mut r = &toks[1..];
            let mut is_const = true;
            if r.first().is_some_and(|t| t == "mut") {
                r = &r[1..];
                is_const = false;
            }
            Ty::Ptr(Box::new(rust_type(r, al, depth + 1)), is_const)
        }
        "(" if toks.len() == 2 => Ty::Void,
        "!" => Ty::Void,
        "[" => {
            let close = matching(toks, 0);
            let inner = &toks[1..close];
            let semi = inner.iter().position(|t| t == ";").unwrap_or(inner.len());
            Ty::Array(Box::new(rust_type(&inner[..semi], al, depth + 1)))
        }
        "unsafe" | "extern" | "fn" => {
            let p = toks.iter().position(|t| t == "(").unwrap_or(0);
            if p == 0 {
                return Ty::Unknown("fn".into());
            }
            let close = matching(toks, p);
            let mut params = vec![];
            let mut variadic = false;
            for part in split_commas(&toks[p + 1..close]) {
                if part.len() == 1 && part[0] == "..." || part.iter().any(|t| t == "...") {
                    variadic = true;
                    continue;
                }
                // optional `name:`
                let ty_toks: &[Tok] = if part.len() > 2 && part[1] == ":" && part[2] != ":" {
                    &part[2..]
                } else {
                    &part
                };
                params.push(rust_type(ty_toks, al, depth + 1));
            }
            let ret = if toks.get(close + 1).is_some_and(|t| t == "->") {
                rust_type(&toks[close + 2..], al, depth + 1)
            } else {
                Ty::Void
            };
            Ty::Fn(Some(Box::new(Sig {
                ret,
                params,
                variadic,
            })))
        }
        _ => {
            // path type
            let name_pos = toks.iter().position(|t| t == "<").unwrap_or(toks.len());
            let path = &toks[..name_pos];
            let name = rust_path_end(path);
            if name == "Option" || name == "Box" || name == "NonNull" || name == "ManuallyDrop" {
                let close = toks.iter().rposition(|t| t == ">").unwrap_or(toks.len());
                let inner = rust_type(&toks[name_pos + 1..close], al, depth + 1);
                return match (name.as_str(), inner) {
                    ("NonNull", t) => Ty::Ptr(Box::new(t), false),
                    (_, t) => t,
                };
            }
            match name.as_str() {
                "i8" | "c_schar" => Ty::Int(8, Some(true)),
                "u8" | "c_uchar" => Ty::Int(8, Some(false)),
                "c_char" => Ty::Int(8, None),
                "i16" | "c_short" => Ty::Int(16, Some(true)),
                "u16" | "c_ushort" => Ty::Int(16, Some(false)),
                "i32" | "c_int" => Ty::Int(32, Some(true)),
                "u32" | "c_uint" => Ty::Int(32, Some(false)),
                "i64" | "c_longlong" => Ty::Int(64, Some(true)),
                "u64" | "c_ulonglong" => Ty::Int(64, Some(false)),
                "c_long" | "c_ulong" => Ty::Unknown(format!("{name} (platform dependent width)")),
                "usize" | "size_t" => Ty::Size(false),
                "isize" | "ssize_t" => Ty::Size(true),
                "f32" | "c_float" => Ty::F32,
                "f64" | "c_double" => Ty::F64,
                "c_void" => Ty::Void,
                "bool" => Ty::Unknown("bool".into()),
                "sqlite3_int64" | "sqlite_int64" => Ty::Int(64, Some(true)),
                "sqlite3_uint64" | "sqlite_uint64" => Ty::Int(64, Some(false)),
                n => {
                    if let Some(body) = al.get(n) {
                        rust_type(body, al, depth + 1)
                    } else {
                        Ty::Opaque(n.to_string())
                    }
                }
            }
        }
    }
}

/// Parse `NAME ( params ) [-> ret]` where `name_idx` is the NAME token.
fn parse_fn_sig(
    toks: &[Tok],
    name_idx: usize,
    al: &BTreeMap<String, Vec<Tok>>,
) -> (Sig, Vec<String>) {
    let open = name_idx + 1;
    let close = matching(toks, open);
    let mut params = vec![];
    let mut bad = vec![];
    let mut variadic = false;
    for part in split_commas(&toks[open + 1..close]) {
        if part.iter().any(|t| t == "...") {
            variadic = true;
            continue;
        }
        let colon = part.iter().position(|t| t == ":").unwrap_or(0);
        let ty = rust_type(&part[colon + 1..], al, 0);
        if let Ty::Unknown(w) = &ty {
            bad.push(w.clone());
        }
        params.push(ty);
    }
    let ret = if toks.get(close + 1).is_some_and(|t| t == "->") {
        let end = toks[close + 2..]
            .iter()
            .position(|t| t == "{" || t == "where" || t == ";")
            .map(|p| close + 2 + p)
            .unwrap_or(toks.len());
        rust_type(&toks[close + 2..end], al, 0)
    } else {
        Ty::Void
    };
    if let Ty::Unknown(w) = &ret {
        bad.push(w.clone());
    }
    (
        Sig {
            ret,
            params,
            variadic,
        },
        bad,
    )
}

fn parse_rust(path: &str, src: &str, out: &mut RustSrc) {
    let toks = lex_rust(src);
    // aliases and consts first (aliases are needed to classify types)
    for (i, t) in toks.iter().enumerate() {
        if t == "type" && toks.get(i + 2).is_some_and(|t| t == "=") && i > 0 && toks[i - 1] != "." {
            let name = toks[i + 1].0.clone();
            let end = toks[i..]
                .iter()
                .position(|t| t == ";")
                .map(|p| i + p)
                .unwrap_or(toks.len());
            out.aliases.insert(name, toks[i + 3..end].to_vec());
        }
        if t == "const"
            && toks.get(i + 2).is_some_and(|t| t == ":")
            && toks.get(i + 1).is_some_and(|n| n.0.starts_with("SQLITE"))
        {
            let name = toks[i + 1].0.clone();
            if let Some(eq) = toks[i..].iter().position(|t| t == "=").map(|p| i + p) {
                let end = toks[eq..]
                    .iter()
                    .position(|t| t == ";")
                    .map(|p| eq + p)
                    .unwrap_or(toks.len());
                out.consts.insert(name, toks[eq + 1..end].to_vec());
            }
        }
    }
    let al = out.aliases.clone();
    let attrs_before = |mut j: usize| -> String {
        // j: index of first item token (pub/unsafe/extern/static); walk back over attributes
        let mut text = String::new();
        loop {
            if j >= 1 && toks[j - 1] == "]" {
                let mut d = 0;
                let mut k = j - 1;
                loop {
                    if toks[k] == "]" {
                        d += 1;
                    } else if toks[k] == "[" {
                        d -= 1;
                        if d == 0 {
                            break;
                        }
                    }
                    if k == 0 {
                        return text;
                    }
                    k -= 1;
                }
                if k >= 1 && toks[k - 1] == "#" {
                    for t in &toks[k..j] {
                        text.push_str(&t.0);
                        text.push(' ');
                    }
                    j = k - 1;
                    continue;
                }
            }
            return text;
        }
    };
    let item_start = |mut j: usize| -> usize {
        // walk back over `pub`, `pub(crate)`, `unsafe`, `extern "C"`
        loop {
            if j == 0 {
                return 0;
            }
            let p = &toks[j - 1];
            if p == "pub"
                || p == "unsafe"
                || p == "extern"
                || p.0 == "\"C\""
                || p == "mut"
                || p == "static"
            {
                j -= 1;
            } else if p == ")" {
                // pub(crate)
                let mut k = j - 1;
                while k > 0 && toks[k] != "(" {
                    k -= 1;
                }
                if k > 0 && toks[k - 1] == "pub" {
                    j = k - 1;
                } else {
                    return j;
                }
            } else {
                return j;
            }
        }
    };
    for i in 0..toks.len() {
        // extern "C" fn NAME (
        if toks[i] == "fn"
            && i >= 2
            && toks[i - 1].0 == "\"C\""
            && toks[i - 2] == "extern"
            && toks.get(i + 2).is_some_and(|t| t == "(")
            && toks
                .get(i + 1)
                .is_some_and(|t| t.0.chars().all(is_ident_char))
        {
            let name = toks[i + 1].0.clone();
            let start = item_start(i);
            let attrs = attrs_before(start);
            let (sig, bad) = parse_fn_sig(&toks, i + 1, &al);
            out.funcs.entry(name).or_default().push(RustFn {
                file: path.to_string(),
                sig,
                naked: attrs.contains("naked"),
                exported: attrs.contains("no_mangle") || attrs.contains("export_name"),
                imported: false,
                bad,
            });
        }
        // extern "C" { fn NAME(..); }  (imports, e.g. in the compat tests)
        if toks[i] == "extern"
            && toks.get(i + 1).is_some_and(|t| t.0 == "\"C\"")
            && toks.get(i + 2).is_some_and(|t| t == "{")
        {
            let close = matching(&toks, i + 2);
            let mut j = i + 3;
            while j < close {
                if toks[j] == "fn" && toks.get(j + 2).is_some_and(|t| t == "(") {
                    let (sig, bad) = parse_fn_sig(&toks, j + 1, &al);
                    out.funcs
                        .entry(toks[j + 1].0.clone())
                        .or_default()
                        .push(RustFn {
                            file: path.to_string(),
                            sig,
                            naked: false,
                            exported: false,
                            imported: true,
                            bad,
                        });
                    j = matching(&toks, j + 2);
                }
                j += 1;
            }
        }
        // static NAME: T
        if toks[i] == "static" && i + 2 < toks.len() {
            let mut k = i + 1;
            if toks[k] == "mut" {
                k += 1;
            }
            if toks.get(k + 1).is_some_and(|t| t == ":") && toks[k].0.chars().all(is_ident_char) {
                let start = item_start(i);
                let attrs = attrs_before(start);
                if attrs.contains("no_mangle") || attrs.contains("export_name") {
                    let end = toks[k..]
                        .iter()
                        .position(|t| t == "=")
                        .map(|p| k + p)
                        .unwrap_or(toks.len());
                    let ty = rust_type(&toks[k + 2..end], &al, 0);
                    out.statics
                        .insert(toks[k].0.clone(), (path.to_string(), ty));
                }
            }
        }
    }
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut es: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    es.sort();
    for p in es {
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

// ---------------------------------------------------------------- comparison

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Machine ABI: widths, int vs pointer, fn pointers; const and pointee
    /// opacity ignored.
    Abi,
    /// Declarations must be identical.
    Strict,
}

/// Returns a problem description, or None when compatible. `notes` collects
/// ABI-neutral differences (const-ness).
fn cmp_ty(c: &Ty, r: &Ty, mode: Mode, notes: &mut Vec<String>) -> Option<String> {
    if mode == Mode::Strict {
        return (c != r).then(|| format!("{c} != {r}"));
    }
    match (c, r) {
        (Ty::Void, Ty::Void) => None,
        (Ty::Int(b1, s1), Ty::Int(b2, s2))
            if b1 == b2 && (s1 == s2 || s1.is_none() || s2.is_none()) =>
        {
            None
        }
        (Ty::Size(a), Ty::Size(b)) if a == b => None,
        (Ty::F32, Ty::F32) | (Ty::F64, Ty::F64) => None,
        (Ty::Fn(Some(a)), Ty::Fn(Some(b))) => {
            // A Rust `fn()` placeholder (transmuted later) is not compared.
            if b.params.is_empty() && !a.params.is_empty() {
                notes.push(format!("callback declared as `fn()`: {c}"));
                return None;
            }
            let mut sub = cmp_sig(a, b, false, mode, notes);
            (!sub.is_empty()).then(|| format!("callback {c} vs {r}: {}", sub.remove(0)))
        }
        (Ty::Fn(_), Ty::Fn(_)) => None,
        // pointer-sized integer standing in for a pointer (same ABI)
        (Ty::Fn(_) | Ty::Ptr(..), Ty::Size(_)) | (Ty::Size(_), Ty::Fn(_) | Ty::Ptr(..)) => {
            notes.push(format!("{c} vs {r}: pointer as pointer-sized integer"));
            None
        }
        // callback declared as a plain pointer (same ABI)
        (Ty::Fn(_), Ty::Ptr(p, _)) | (Ty::Ptr(p, _), Ty::Fn(_)) if **p == Ty::Void => {
            notes.push(format!("{c} vs {r}: function pointer as void*"));
            None
        }
        (Ty::Ptr(p1, c1), Ty::Ptr(p2, c2)) => {
            if c1 != c2 {
                notes.push(format!("const mismatch: {c} vs {r}"));
            }
            let wild = |t: &Ty| matches!(t, Ty::Void | Ty::Opaque(_) | Ty::Unknown(_) | Ty::Fn(_));
            if wild(p1) || wild(p2) {
                return None;
            }
            cmp_ty(p1, p2, mode, notes).map(|_| format!("{c} vs {r}"))
        }
        (Ty::Array(a), Ty::Array(b)) => cmp_ty(a, b, mode, notes),
        _ => Some(format!("{c} vs {r}")),
    }
}

fn cmp_sig(c: &Sig, r: &Sig, naked: bool, mode: Mode, notes: &mut Vec<String>) -> Vec<String> {
    let mut p = vec![];
    if mode == Mode::Strict {
        if c != r {
            // locate the differing piece
            if let Some(m) = cmp_ty(&c.ret, &r.ret, mode, notes) {
                p.push(format!("return type: {m}"));
            }
            if c.params.len() != r.params.len() {
                p.push(format!(
                    "param count: C {} vs {}",
                    c.params.len(),
                    r.params.len()
                ));
            }
            for (i, (a, b)) in c.params.iter().zip(&r.params).enumerate() {
                if let Some(m) = cmp_ty(a, b, mode, notes) {
                    p.push(format!("param {}: {m}", i + 1));
                }
            }
            if c.variadic != r.variadic {
                p.push(format!("variadic: C {} vs {}", c.variadic, r.variadic));
            }
        }
        return p;
    }
    if let Some(m) = cmp_ty(&c.ret, &r.ret, mode, notes) {
        p.push(format!("return type: {m}"));
    }
    if c.variadic {
        if !naked {
            p.push("variadic in C but not a naked trampoline in Rust".into());
        }
        if r.params.len() > c.params.len() || r.params.is_empty() && !c.params.is_empty() {
            p.push(format!(
                "variadic fixed params: C {} vs Rust {}",
                c.params.len(),
                r.params.len()
            ));
        }
        for (i, (a, b)) in c.params.iter().zip(&r.params).enumerate() {
            if let Some(m) = cmp_ty(a, b, mode, notes) {
                p.push(format!("param {}: {m}", i + 1));
            }
        }
        if r.params.len() < c.params.len() {
            p.push(format!(
                "variadic fixed params: C {} vs Rust {}",
                c.params.len(),
                r.params.len()
            ));
        }
        return p;
    }
    if c.params.len() != r.params.len() {
        p.push(format!(
            "param count: C {} vs Rust {}",
            c.params.len(),
            r.params.len()
        ));
        return p;
    }
    for (i, (a, b)) in c.params.iter().zip(&r.params).enumerate() {
        if let Some(m) = cmp_ty(a, b, mode, notes) {
            p.push(format!("param {}: {m}", i + 1));
        }
    }
    p
}

// ---------------------------------------------------------------- driver

#[derive(Default)]
struct Report {
    /// ABI problems; the test fails if any
    errors: Vec<String>,
    /// ABI-neutral differences and informational lists
    notes: Vec<String>,
}

/// Exports that are not in SQLite's public headers and are allowed.
const ALLOWED_EXTRA_PREFIXES: &[&str] = &["turso_", "libsql_"];
const ALLOWED_EXTRA: &[&str] = &["sqlite3_search_count"];

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn check(root: &Path, vendored: &Path) -> Report {
    let mut rep = Report::default();
    let sq = parse_c_header(&read(&vendored.join("sqlite3.h")), true, &BTreeMap::new());
    let ext = parse_c_header(&read(&vendored.join("sqlite3ext.h")), true, &sq.typedefs);
    assert!(
        sq.funcs.len() > 300,
        "sqlite3.h parse found only {} functions",
        sq.funcs.len()
    );

    // sanity: sqlite3ext.h members agree with sqlite3.h (validates the parser too)
    let mut ext_funcs = BTreeSet::new();
    for (sname, member) in &ext.ext_map {
        let Some(sig) = ext.ext_members.get(member) else {
            continue;
        };
        ext_funcs.insert(sname.clone());
        if let Some(h) = sq.funcs.get(sname) {
            let mut n = vec![];
            let d = cmp_sig(h, sig, true, Mode::Abi, &mut n);
            if !d.is_empty() {
                rep.notes.push(format!(
                    "sqlite3ext.h vs sqlite3.h disagree on {sname}: {}",
                    d.join("; ")
                ));
            }
        } else if !sname.starts_with("sqlite3_") {
            continue;
        } else {
            rep.notes.push(format!(
                "sqlite3ext.h entry {sname} has no sqlite3.h declaration"
            ));
        }
    }

    // Rust sources
    let mut files = vec![];
    collect_rs(&root.join("src"), &mut files);
    collect_rs(&root.join("tests"), &mut files);
    let mut rs = RustSrc::default();
    // Aliases may be defined in a later file than their first use.
    let mut pre = RustSrc::default();
    for f in &files {
        parse_rust("", &read(f), &mut pre);
    }
    rs.aliases = pre.aliases;
    for f in &files {
        parse_rust(
            &f.strip_prefix(root).unwrap_or(f).display().to_string(),
            &read(f),
            &mut rs,
        );
    }

    let mut exported: BTreeSet<String> = BTreeSet::new();
    for (name, fns) in &rs.funcs {
        for f in fns.iter().filter(|f| f.exported || f.imported) {
            if f.exported {
                exported.insert(name.clone());
            }
            if !name.starts_with("sqlite3") {
                continue;
            }
            let loc = format!("{name} ({})", f.file);
            let Some(csig) = sq.funcs.get(name) else {
                if f.imported {
                    continue;
                }
                if !ALLOWED_EXTRA.contains(&name.as_str()) {
                    rep.errors.push(format!(
                        "{loc}: exported but not declared in SQLite 3.54 sqlite3.h"
                    ));
                }
                continue;
            };
            for w in &f.bad {
                rep.errors
                    .push(format!("{loc}: unclassifiable Rust type `{w}`"));
            }
            let mut notes = vec![];
            for d in cmp_sig(csig, &f.sig, f.naked || f.imported, Mode::Abi, &mut notes) {
                rep.errors
                    .push(format!("{loc}: {d}   [SQLite: {csig}; Rust: {}]", f.sig));
            }
            for n in notes {
                rep.notes.push(format!("{loc}: {n}"));
            }
        }
    }
    for (name, (file, ty)) in &rs.statics {
        exported.insert(name.clone());
        let loc = format!("{name} ({file})");
        match sq.data.get(name) {
            None => {
                if !ALLOWED_EXTRA.contains(&name.as_str()) && name.starts_with("sqlite3") {
                    rep.errors.push(format!(
                        "{loc}: exported data symbol not in SQLite 3.54 sqlite3.h"
                    ));
                }
            }
            Some(cty) => {
                let mut n = vec![];
                if cmp_ty(cty, ty, Mode::Abi, &mut n).is_some() {
                    rep.errors
                        .push(format!("{loc}: data symbol type: SQLite {cty}, Rust {ty}"));
                }
            }
        }
    }
    for name in [
        "sqlite3_version",
        "sqlite3_temp_directory",
        "sqlite3_data_directory",
    ] {
        if !rs.statics.contains_key(name) {
            rep.errors
                .push(format!("data symbol {name} is not exported"));
        }
    }
    let mut missing: Vec<&String> = sq.funcs.keys().filter(|n| !exported.contains(*n)).collect();
    missing.sort();
    rep.notes.push(format!(
        "{} of {} sqlite3.h functions are not exported: {}",
        missing.len(),
        sq.funcs.len(),
        missing
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    ));
    for (name, _) in rs.funcs.iter().filter(|(n, v)| {
        v.iter().any(|f| f.exported)
            && !n.starts_with("sqlite3")
            && !ALLOWED_EXTRA_PREFIXES.iter().any(|p| n.starts_with(p))
    }) {
        rep.errors.push(format!(
            "{name}: exported symbol outside the sqlite3_/turso_/libsql_ namespaces"
        ));
    }

    // Rust constants
    let mut rust_only = vec![];
    for (name, body) in &rs.consts {
        match sq.defs.get(name) {
            None => rust_only.push(name.as_str()),
            Some(cbody) => {
                let rv = eval_const(body, &rs.consts);
                let cv = eval_const(cbody, &sq.defs);
                match (cv, rv) {
                    (Some(a), Some(b)) if a != b => rep
                        .errors
                        .push(format!("Rust const {name} = {b} but SQLite = {a}")),
                    (Some(_), None) => rep
                        .notes
                        .push(format!("Rust const {name}: could not evaluate")),
                    _ => {}
                }
            }
        }
    }
    rep.notes.push(format!(
        "{} Rust SQLITE_* consts not in sqlite3.h: {}",
        rust_only.len(),
        rust_only.join(" ")
    ));

    // turso's own header
    let hp = root.join("include/sqlite3.h");
    if hp.exists() {
        let th = parse_c_header(&read(&hp), false, &BTreeMap::new());
        for (name, sig) in &th.funcs {
            let loc = format!("include/sqlite3.h: {name}");
            if !exported.contains(name) {
                rep.errors
                    .push(format!("{loc}: declared but not exported by src/"));
            }
            match sq.funcs.get(name) {
                Some(csig) => {
                    let mut notes = vec![];
                    for d in cmp_sig(csig, sig, false, Mode::Strict, &mut notes) {
                        rep.errors
                            .push(format!("{loc}: {d}   [SQLite: {csig}; turso: {sig}]"));
                    }
                }
                None => {
                    if !ALLOWED_EXTRA_PREFIXES.iter().any(|p| name.starts_with(p)) {
                        rep.errors.push(format!(
                            "{loc}: not in SQLite 3.54 sqlite3.h (header must be a subset)"
                        ));
                    }
                }
            }
        }
        for (name, ty) in &th.data {
            if !rs.statics.contains_key(name) {
                rep.errors.push(format!(
                    "include/sqlite3.h: data {name} declared but not exported by src/"
                ));
            }
            match sq.data.get(name) {
                Some(cty) if cty != ty => rep.errors.push(format!(
                    "include/sqlite3.h: data {name}: SQLite {cty}, turso {ty}"
                )),
                None => rep.errors.push(format!(
                    "include/sqlite3.h: data {name} not in SQLite sqlite3.h"
                )),
                _ => {}
            }
        }
        for (name, body) in &th.defs {
            if name.starts_with("LIBSQL_")
                || name.starts_with("TURSO_")
                || name == "LIMBO_SQLITE3_H"
            {
                continue;
            }
            match sq.defs.get(name) {
                None => rep.errors.push(format!(
                    "include/sqlite3.h: #define {name} not in SQLite sqlite3.h"
                )),
                Some(cbody) => {
                    let cv = eval_const(cbody, &sq.defs);
                    let tv = eval_const(body, &th.defs);
                    let text =
                        |b: &[Tok]| b.iter().map(|t| t.0.as_str()).collect::<Vec<_>>().join("");
                    match (cv, tv) {
                        (Some(a), Some(b)) if a != b => rep.errors.push(format!(
                            "include/sqlite3.h: #define {name} = {b} but SQLite = {a}"
                        )),
                        (Some(_), Some(_)) => {}
                        _ if text(cbody) != text(body) => rep.errors.push(format!(
                            "include/sqlite3.h: #define {name} `{}` but SQLite `{}`",
                            text(body),
                            text(cbody)
                        )),
                        _ => {}
                    }
                }
            }
        }
        let mut undeclared: Vec<&String> = rs
            .funcs
            .iter()
            .filter(|(n, v)| {
                n.starts_with("sqlite3")
                    && v.iter().any(|f| f.exported)
                    && !th.funcs.contains_key(*n)
            })
            .map(|(n, _)| n)
            .collect();
        undeclared.sort();
        rep.notes.push(format!(
            "{} exported sqlite3_* functions are not declared in include/sqlite3.h: {}",
            undeclared.len(),
            undeclared
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    rep
}

fn crate_root() -> PathBuf {
    match std::env::var_os("TURSO_ABI_ROOT") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")),
    }
}

#[test]
fn exports_match_sqlite_3_54_abi() {
    let vendored = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/abi");
    let rep = check(&crate_root(), &vendored);
    let verbose = std::env::var_os("TURSO_ABI_NOFAIL").is_some() || !rep.errors.is_empty();
    if verbose {
        for n in &rep.notes {
            eprintln!("note: {n}");
        }
    }
    for e in &rep.errors {
        eprintln!("MISMATCH: {e}");
    }
    if std::env::var_os("TURSO_ABI_NOFAIL").is_none() {
        assert!(
            rep.errors.is_empty(),
            "{} ABI mismatches against SQLite 3.54",
            rep.errors.len()
        );
    }
}

// ---------------------------------------------------------------- parser self-tests

#[test]
fn checker_parses_sqlite_header() {
    let vendored = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/abi");
    let sq = parse_c_header(&read(&vendored.join("sqlite3.h")), true, &BTreeMap::new());
    let s = |n: &str| {
        sq.funcs
            .get(n)
            .unwrap_or_else(|| panic!("missing {n}"))
            .to_string()
    };
    assert_eq!(s("sqlite3_bind_int"), "fn(sqlite3_stmt*, i32, i32) -> i32");
    assert_eq!(
        s("sqlite3_bind_int64"),
        "fn(sqlite3_stmt*, i32, i64) -> i32"
    );
    assert_eq!(s("sqlite3_malloc64"), "fn(u64) -> void*");
    assert_eq!(s("sqlite3_value_text"), "fn(sqlite3_value*) -> const u8*");
    assert!(sq.funcs["sqlite3_mprintf"].variadic);
    assert!(sq.funcs["sqlite3_db_config"].variadic);
    assert_eq!(sq.funcs["sqlite3_vmprintf"].params.len(), 2);
    assert_eq!(sq.data["sqlite3_version"].to_string(), "char8[]");
    assert!(sq.data.contains_key("sqlite3_temp_directory"));
    assert!(sq.data.contains_key("sqlite3_data_directory"));
    let v = |n: &str| eval_const(&sq.defs[n], &sq.defs);
    assert_eq!(v("SQLITE_OK"), Some(0));
    assert_eq!(v("SQLITE_IOERR_READ"), Some(10 | (1 << 8)));
    assert_eq!(v("SQLITE_TRANSIENT"), Some(-1));
    assert_eq!(v("SQLITE_STATIC"), Some(0));
}
