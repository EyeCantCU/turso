//! FTS5 table configuration: CREATE VIRTUAL TABLE arguments and the
//! %_config table (fts5_config.c).

use super::tokenize::{self, Pattern, Tokenizer};
use crate::sync::Arc;
use crate::Value;

pub(crate) const DEFAULT_PAGE_SIZE: i32 = 4050;
pub(crate) const DEFAULT_AUTOMERGE: i32 = 4;
pub(crate) const DEFAULT_USERMERGE: i32 = 4;
pub(crate) const DEFAULT_CRISISMERGE: i32 = 16;
pub(crate) const DEFAULT_HASHSIZE: i32 = 1024 * 1024;
pub(crate) const DEFAULT_DELETE_AUTOMERGE: i32 = 10;
pub(crate) const MAX_PAGE_SIZE: i32 = 64 * 1024;
pub(crate) const MAX_SEGMENT: usize = 2000;
pub(crate) const MAX_PREFIX_INDEXES: usize = 31;
pub(crate) const CURRENT_VERSION: i64 = 4;
pub(crate) const CURRENT_VERSION_SECUREDELETE: i64 = 5;
pub(crate) const DEFAULT_RANK: &str = "bm25";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Content {
    Normal,
    None,
    External,
    Unindexed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Detail {
    Full,
    None,
    Columns,
}

/// Parsed CREATE VIRTUAL TABLE arguments. Immutable once built.
pub(crate) struct Fts5Config {
    pub db: String,
    pub name: String,
    pub cols: Vec<String>,
    pub unindexed: Vec<bool>,
    pub prefixes: Vec<usize>,
    pub content: Content,
    pub columnsize: bool,
    pub contentless_delete: bool,
    pub contentless_unindexed: bool,
    pub locale: bool,
    pub tokendata: bool,
    pub detail: Detail,
    /// Quoted content table name (`'main'.'t_content'` style), or None.
    pub content_table: Option<String>,
    pub content_rowid: String,
    pub content_exprlist: String,
    pub tokenizer_args: Vec<String>,
    pub tokenizer: Arc<dyn Tokenizer>,
}

impl std::fmt::Debug for Fts5Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fts5Config")
            .field("name", &self.name)
            .field("cols", &self.cols)
            .finish()
    }
}

/// Settings stored in the %_config table.
#[derive(Debug, Clone)]
pub(crate) struct ConfigValues {
    pub pgsz: i32,
    pub automerge: i32,
    pub usermerge: i32,
    pub crisismerge: i32,
    pub hashsize: i32,
    pub deletemerge: i32,
    pub rank: Option<(String, Option<String>)>,
    pub secure_delete: bool,
    pub insttoken: bool,
    pub version: i64,
    pub cookie: i32,
}

impl Default for ConfigValues {
    fn default() -> Self {
        Self {
            pgsz: DEFAULT_PAGE_SIZE,
            automerge: DEFAULT_AUTOMERGE,
            usermerge: DEFAULT_USERMERGE,
            crisismerge: DEFAULT_CRISISMERGE,
            hashsize: DEFAULT_HASHSIZE,
            deletemerge: DEFAULT_DELETE_AUTOMERGE,
            rank: None,
            secure_delete: false,
            insttoken: false,
            version: CURRENT_VERSION,
            cookie: -1,
        }
    }
}

pub(crate) fn is_bareword(c: u8) -> bool {
    c & 0x80 != 0 || c.is_ascii_alphanumeric() || c == b'_' || c == 0x1a
}

fn skip_ws(z: &[u8], mut i: usize) -> usize {
    while i < z.len() && z[i] == b' ' {
        i += 1;
    }
    i
}

fn skip_bareword(z: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    while j < z.len() && is_bareword(z[j]) {
        j += 1;
    }
    (j > i).then_some(j)
}

/// Skips an SQL literal, as `fts5ConfigSkipLiteral`.
fn skip_literal(z: &[u8], i: usize) -> Option<usize> {
    let at = |k: usize| z.get(k).copied().unwrap_or(0);
    let mut p = i;
    match at(p) {
        b'n' | b'N' => {
            if z.len() >= p + 4 && z[p..p + 4].eq_ignore_ascii_case(b"null") {
                Some(p + 4)
            } else {
                None
            }
        }
        b'x' | b'X' => {
            p += 1;
            if at(p) != b'\'' {
                return None;
            }
            p += 1;
            while at(p).is_ascii_hexdigit() {
                p += 1;
            }
            if at(p) == b'\'' && (p - i) % 2 == 0 {
                Some(p + 1)
            } else {
                None
            }
        }
        b'\'' => {
            p += 1;
            loop {
                if at(p) == b'\'' {
                    p += 1;
                    if at(p) != b'\'' {
                        return Some(p);
                    }
                }
                p += 1;
                if p >= z.len() {
                    return None;
                }
            }
        }
        _ => {
            if at(p) == b'+' || at(p) == b'-' {
                p += 1;
            }
            while at(p).is_ascii_digit() {
                p += 1;
            }
            if at(p) == b'.' && at(p + 1).is_ascii_digit() {
                p += 2;
                while at(p).is_ascii_digit() {
                    p += 1;
                }
            }
            (p != i).then_some(p)
        }
    }
}

/// Removes SQL quotes; returns the text and the bytes consumed from `z`.
pub(crate) fn dequote(z: &[u8]) -> (Vec<u8>, usize) {
    let q = match z.first() {
        Some(b'[') => b']',
        Some(&c @ (b'\'' | b'"' | b'`')) => c,
        _ => return (z.to_vec(), z.len()),
    };
    let mut out = Vec::new();
    let mut i = 1;
    while i < z.len() {
        if z[i] == q {
            if z.get(i + 1) != Some(&q) {
                i += 1;
                break;
            }
            out.push(q);
            i += 2;
        } else {
            out.push(z[i]);
            i += 1;
        }
    }
    (out, i)
}

fn is_open_quote(c: u8) -> bool {
    matches!(c, b'"' | b'\'' | b'[' | b'`')
}

/// Reads one bareword or quoted word: (word, end offset, was quoted).
fn gobble_word(z: &[u8], i: usize) -> Option<(String, usize, bool)> {
    if i < z.len() && is_open_quote(z[i]) {
        let (w, n) = dequote(&z[i..]);
        Some((String::from_utf8_lossy(&w).into_owned(), i + n, true))
    } else {
        let end = skip_bareword(z, i)?;
        Some((String::from_utf8_lossy(&z[i..end]).into_owned(), end, false))
    }
}

/// `%Q`: single-quote a string for SQL.
pub(crate) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `%w`: escape for use inside double quotes.
pub(crate) fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn bool_arg(arg: &str, err: &str) -> Result<bool, String> {
    match arg {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(err.to_string()),
    }
}

/// `sqlite3_strnicmp(full, cmd, strlen(cmd)) == 0`: `cmd` abbreviates `full`.
fn prefix_ci(full: &str, cmd: &str) -> bool {
    cmd.len() <= full.len() && full.as_bytes()[..cmd.len()].eq_ignore_ascii_case(cmd.as_bytes())
}

/// The integer value if `sqlite3_value_numeric_type()` is SQLITE_INTEGER.
pub(crate) fn numeric_int(v: &Value) -> Option<i64> {
    match v {
        Value::Numeric(crate::numeric::Numeric::Integer(i)) => Some(*i),
        Value::Text(t) => {
            let s = t.as_str().trim_matches(|c: char| c.is_ascii_whitespace());
            crate::numeric::str_to_i64_checked(s).ok().filter(|_| {
                let b = s.strip_prefix(['+', '-']).unwrap_or(s);
                !b.is_empty() && b.bytes().all(|c| c.is_ascii_digit())
            })
        }
        _ => None,
    }
}

/// `sqlite3_value_text()` bytes, or None for NULL.
pub(crate) fn value_text(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Null => None,
        Value::Text(t) => Some(t.as_str().as_bytes().to_vec()),
        Value::Blob(b) => Some(b.to_vec()),
        other => Some(other.to_string().into_bytes()),
    }
}

struct Builder {
    cols: Vec<String>,
    unindexed: Vec<bool>,
    prefixes: Vec<usize>,
    tokenize: Option<Vec<String>>,
    content: Content,
    content_arg: Option<String>,
    content_rowid: Option<String>,
    columnsize: bool,
    contentless_delete: bool,
    contentless_unindexed: bool,
    locale: bool,
    tokendata: bool,
    detail: Detail,
}

impl Builder {
    fn special(&mut self, cmd: &str, arg: &str) -> Result<(), String> {
        let is = |name: &str| prefix_ci(name, cmd);
        if is("prefix") {
            let p = arg.as_bytes();
            let mut i = 0;
            let mut first = true;
            loop {
                while i < p.len() && p[i] == b' ' {
                    i += 1;
                }
                if !first && i < p.len() && p[i] == b',' {
                    i += 1;
                    while i < p.len() && p[i] == b' ' {
                        i += 1;
                    }
                } else if i >= p.len() {
                    break;
                }
                if i >= p.len() || !p[i].is_ascii_digit() {
                    return Err("malformed prefix=... directive".into());
                }
                if self.prefixes.len() == MAX_PREFIX_INDEXES {
                    return Err(format!(
                        "too many prefix indexes (max {MAX_PREFIX_INDEXES})"
                    ));
                }
                let mut pre = 0usize;
                while i < p.len() && p[i].is_ascii_digit() && pre < 1000 {
                    pre = pre * 10 + (p[i] - b'0') as usize;
                    i += 1;
                }
                if pre == 0 || pre >= 1000 {
                    return Err("prefix length out of range (max 999)".into());
                }
                self.prefixes.push(pre);
                first = false;
            }
            return Ok(());
        }
        if is("tokenize") {
            if self.tokenize.is_some() {
                return Err("multiple tokenize=... directives".into());
            }
            let z = arg.as_bytes();
            let mut args = Vec::new();
            let mut i = 0;
            while i < z.len() {
                let start = skip_ws(z, i);
                let end = if z.get(start) == Some(&b'\'') {
                    skip_literal(z, start)
                } else {
                    skip_bareword(z, start)
                };
                let Some(end) = end else {
                    return Err("parse error in tokenize directive".into());
                };
                let (w, _) = dequote(&z[start..end]);
                args.push(String::from_utf8_lossy(&w).into_owned());
                i = skip_ws(z, end);
            }
            self.tokenize = Some(args);
            return Ok(());
        }
        if is("content") {
            if self.content != Content::Normal {
                return Err("multiple content=... directives".into());
            }
            if !arg.is_empty() {
                self.content = Content::External;
                self.content_arg = Some(arg.to_string());
            } else {
                self.content = Content::None;
            }
            return Ok(());
        }
        if is("contentless_delete") {
            self.contentless_delete = bool_arg(arg, "malformed contentless_delete=... directive")?;
            return Ok(());
        }
        if is("contentless_unindexed") {
            self.contentless_unindexed =
                bool_arg(arg, "malformed contentless_delete=... directive")?;
            return Ok(());
        }
        if is("content_rowid") {
            if self.content_rowid.is_some() {
                return Err("multiple content_rowid=... directives".into());
            }
            self.content_rowid = Some(arg.to_string());
            return Ok(());
        }
        if is("columnsize") {
            self.columnsize = bool_arg(arg, "malformed columnsize=... directive")?;
            return Ok(());
        }
        if is("locale") {
            self.locale = bool_arg(arg, "malformed locale=... directive")?;
            return Ok(());
        }
        if is("detail") {
            let mut found = None;
            for (name, d) in [
                ("none", Detail::None),
                ("full", Detail::Full),
                ("columns", Detail::Columns),
            ] {
                if prefix_ci(name, arg) {
                    if found.is_some() {
                        return Err("malformed detail=... directive".into());
                    }
                    found = Some(d);
                }
            }
            self.detail = found.ok_or_else(|| "malformed detail=... directive".to_string())?;
            return Ok(());
        }
        if is("tokendata") {
            self.tokendata = bool_arg(arg, "malformed tokendata=... directive")?;
            return Ok(());
        }
        Err(format!("unrecognized option: \"{cmd}\""))
    }
}

impl Fts5Config {
    /// Parses the module arguments (`args` excludes module, db and table names).
    pub(crate) fn parse(db: &str, name: &str, args: &[String]) -> Result<Self, String> {
        if name.eq_ignore_ascii_case("rank") {
            return Err(format!("reserved fts5 table name: {name}"));
        }
        let mut b = Builder {
            cols: Vec::new(),
            unindexed: Vec::new(),
            prefixes: Vec::new(),
            tokenize: None,
            content: Content::Normal,
            content_arg: None,
            content_rowid: None,
            columnsize: true,
            contentless_delete: false,
            contentless_unindexed: false,
            locale: false,
            tokendata: false,
            detail: Detail::Full,
        };
        let mut any_unindexed = false;
        for orig in args {
            let z = orig.as_bytes();
            let parse_err = || format!("parse error in \"{orig}\"");
            let Some((one, mut i, must_be_col)) = gobble_word(z, 0) else {
                return Err(parse_err());
            };
            i = skip_ws(z, i);
            let mut option = false;
            if z.get(i) == Some(&b'=') {
                if must_be_col {
                    return Err(parse_err());
                }
                option = true;
                i += 1;
            }
            i = skip_ws(z, i);
            let mut two = None;
            if i < z.len() {
                let Some((w, end, _)) = gobble_word(z, i) else {
                    return Err(parse_err());
                };
                if end < z.len() {
                    return Err(parse_err());
                }
                two = Some(w);
            }
            if option {
                b.special(&one, two.as_deref().unwrap_or(""))?;
            } else {
                if one.eq_ignore_ascii_case("rank") || one.eq_ignore_ascii_case("rowid") {
                    return Err(format!("reserved fts5 column name: {one}"));
                }
                let mut unindexed = false;
                if let Some(arg) = two {
                    if arg.eq_ignore_ascii_case("unindexed") {
                        unindexed = true;
                        any_unindexed = true;
                    } else {
                        return Err(format!("unrecognized column option: {arg}"));
                    }
                }
                b.cols.push(one);
                b.unindexed.push(unindexed);
            }
        }
        if b.contentless_delete && b.content != Content::None {
            return Err("contentless_delete=1 requires a contentless table".into());
        }
        if b.contentless_delete && !b.columnsize {
            return Err("contentless_delete=1 is incompatible with columnsize=0".into());
        }
        if b.contentless_unindexed && b.content != Content::None {
            return Err("contentless_unindexed=1 requires a contentless table".into());
        }
        let mut content = b.content;
        let content_table = match (&b.content_arg, content) {
            (Some(arg), _) => Some(format!("{}.{}", quote(db), quote(arg))),
            (None, Content::Normal) => Some(format!(
                "{}.{}",
                quote(db),
                quote(&format!("{name}_content"))
            )),
            (None, _) => {
                if any_unindexed && b.contentless_unindexed {
                    content = Content::Unindexed;
                    Some(format!(
                        "{}.{}",
                        quote(db),
                        quote(&format!("{name}_content"))
                    ))
                } else if b.columnsize {
                    Some(format!(
                        "{}.{}",
                        quote(db),
                        quote(&format!("{name}_docsize"))
                    ))
                } else {
                    None
                }
            }
        };
        let content_rowid = b.content_rowid.unwrap_or_else(|| "rowid".to_string());
        let mut exprlist = format!("T.{}", quote(&content_rowid));
        if content != Content::None {
            for (i, col) in b.cols.iter().enumerate() {
                if content == Content::External {
                    exprlist.push_str(&format!(", T.{}", quote(col)));
                } else if content == Content::Normal || b.unindexed[i] {
                    exprlist.push_str(&format!(", T.c{i}"));
                } else {
                    exprlist.push_str(", NULL");
                }
            }
        }
        if content == Content::Normal && b.locale {
            for i in 0..b.cols.len() {
                if !b.unindexed[i] {
                    exprlist.push_str(&format!(", T.l{i}"));
                } else {
                    exprlist.push_str(", NULL");
                }
            }
        }
        let tokenizer_args = b.tokenize.unwrap_or_default();
        let module = tokenize::find_module(tokenizer_args.first().map(|s| s.as_str()))
            .ok_or_else(|| format!("no such tokenizer: {}", tokenizer_args[0]))?;
        let rest = if tokenizer_args.is_empty() {
            &[][..]
        } else {
            &tokenizer_args[1..]
        };
        let tokenizer: Arc<dyn Tokenizer> = Arc::from(
            module
                .create(rest)
                .map_err(|_| "error in tokenizer constructor".to_string())?,
        );
        Ok(Self {
            db: db.to_string(),
            name: name.to_string(),
            cols: b.cols,
            unindexed: b.unindexed,
            prefixes: b.prefixes,
            content,
            columnsize: b.columnsize,
            contentless_delete: b.contentless_delete,
            contentless_unindexed: b.contentless_unindexed,
            locale: b.locale,
            tokendata: b.tokendata,
            detail: b.detail,
            content_table,
            content_rowid,
            content_exprlist: exprlist,
            tokenizer_args,
            tokenizer,
        })
    }

    pub(crate) fn ncol(&self) -> usize {
        self.cols.len()
    }

    pub(crate) fn pattern(&self) -> Pattern {
        self.tokenizer.pattern()
    }

    /// `CREATE TABLE x(...)` declared to the core.
    pub(crate) fn declare_vtab(&self) -> String {
        let mut sql = String::from("CREATE TABLE x(");
        for (i, c) in self.cols.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&quote(c));
        }
        sql.push_str(&format!(", {} HIDDEN, rank HIDDEN)", quote(&self.name)));
        sql
    }

    /// `'db'.'name_suffix'`
    pub(crate) fn shadow(&self, suffix: &str) -> String {
        format!(
            "{}.{}",
            quote(&self.db),
            quote(&format!("{}_{}", self.name, suffix))
        )
    }

    /// Whether the table has no stored copy of the indexed columns.
    pub(crate) fn is_contentless(&self, include_unindexed: bool) -> bool {
        self.content == Content::None || (include_unindexed && self.content == Content::Unindexed)
    }
}

/// Parses a rank function spec: `name(args...)`.
pub(crate) fn parse_rank(z: &str) -> Option<(String, Option<String>)> {
    let z = z.as_bytes();
    let p = skip_ws(z, 0);
    let end = skip_bareword(z, p)?;
    let name = String::from_utf8_lossy(&z[p..end]).into_owned();
    let mut p = skip_ws(z, end);
    if z.get(p) != Some(&b'(') {
        return None;
    }
    p = skip_ws(z, p + 1);
    let args_start = p;
    let mut args = None;
    if z.get(p) != Some(&b')') {
        loop {
            p = skip_ws(z, p);
            p = skip_literal(z, p)?;
            p = skip_ws(z, p);
            match z.get(p) {
                Some(b')') => break,
                Some(b',') => p += 1,
                _ => return None,
            }
        }
        args = Some(String::from_utf8_lossy(&z[args_start..p]).into_owned());
    }
    Some((name, args))
}

impl ConfigValues {
    /// Applies one %_config entry. Returns false for a bad key or value.
    pub(crate) fn set(&mut self, key: &str, val: &Value) -> bool {
        let int = || numeric_int(val);
        match key.to_ascii_lowercase().as_str() {
            "pgsz" => match int() {
                Some(v) if (32..=MAX_PAGE_SIZE as i64).contains(&v) => self.pgsz = v as i32,
                _ => return false,
            },
            "hashsize" => match int() {
                Some(v) if v > 0 => self.hashsize = v as i32,
                _ => return false,
            },
            "automerge" => match int() {
                Some(v) if (0..=64).contains(&v) => {
                    self.automerge = if v == 1 { DEFAULT_AUTOMERGE } else { v as i32 }
                }
                _ => return false,
            },
            "usermerge" => match int() {
                Some(v) if (2..=16).contains(&v) => self.usermerge = v as i32,
                _ => return false,
            },
            "crisismerge" => match int() {
                Some(v) if v >= 0 => {
                    let mut v = v as i32;
                    if v <= 1 {
                        v = DEFAULT_CRISISMERGE;
                    }
                    if v >= MAX_SEGMENT as i32 {
                        v = MAX_SEGMENT as i32 - 1;
                    }
                    self.crisismerge = v;
                }
                _ => return false,
            },
            "deletemerge" => {
                let mut ok = true;
                let mut v = match int() {
                    Some(v) => v,
                    None => {
                        ok = false;
                        -1
                    }
                };
                if v < 0 {
                    v = DEFAULT_DELETE_AUTOMERGE as i64;
                }
                if v > 100 {
                    v = 0;
                }
                self.deletemerge = v as i32;
                return ok;
            }
            "rank" => {
                let text = value_text(val).unwrap_or_default();
                match parse_rank(&String::from_utf8_lossy(&text)) {
                    Some(r) => self.rank = Some(r),
                    None => return false,
                }
            }
            "secure-delete" => match int() {
                Some(v) if v >= 0 => self.secure_delete = v != 0,
                _ => return false,
            },
            "insttoken" => match int() {
                Some(v) if v >= 0 => self.insttoken = v != 0,
                _ => return false,
            },
            _ => return false,
        }
        true
    }
}
