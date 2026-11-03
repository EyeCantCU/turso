//! Tokenizer interface, the "simple" tokenizer and tokenizer lookup
//! (`fts3_tokenizer.c`, `fts3_tokenizer1.c`).

use super::{ctok, dequote, next_token, sql_error};
use crate::sync::Arc;
use crate::{Connection, Result};

/// Receives (token, start byte, end byte, position); returns false to stop.
pub(crate) type TokenSink<'a> = dyn FnMut(&[u8], i32, i32, i32) -> Result<bool> + 'a;

pub(crate) trait Tokenizer: Send + Sync {
    fn tokenize(&self, langid: i32, text: &[u8], f: &mut TokenSink) -> Result<()>;
}

/// A tokenizer module: a built-in one or a C `sqlite3_tokenizer_module`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenizerModule {
    Simple,
    Porter,
    Unicode61,
    C(ctok::ModulePtr),
}

impl TokenizerModule {
    pub(crate) fn builtin(name: &[u8]) -> Option<Self> {
        match name {
            b"simple" => Some(Self::Simple),
            b"porter" => Some(Self::Porter),
            b"unicode61" => Some(Self::Unicode61),
            _ => None,
        }
    }

    /// `xCreate`. Errors are reported by the caller as "unknown tokenizer".
    pub(crate) fn create(&self, args: &[Vec<u8>]) -> Option<Arc<dyn Tokenizer>> {
        match self {
            Self::Simple => Simple::new(args).map(|t| Arc::new(t) as Arc<dyn Tokenizer>),
            Self::Porter => Some(Arc::new(super::porter::Porter)),
            Self::Unicode61 => {
                super::unicode::Unicode61::new(args).map(|t| Arc::new(t) as Arc<dyn Tokenizer>)
            }
            Self::C(m) => ctok::CTokenizer::create(*m, args).map(|t| Arc::new(t) as _),
        }
    }
}

/// Finds a tokenizer module by name: those registered on this connection
/// with `fts3_tokenizer()` first, then the built-in ones.
pub(crate) fn find_module(conn: Option<&Arc<Connection>>, name: &[u8]) -> Option<TokenizerModule> {
    if let Some(conn) = conn {
        let inner = conn.fts3.inner.lock();
        if let Some(m) = inner.tokenizers.get(name) {
            return Some(*m);
        }
    }
    TokenizerModule::builtin(name)
}

/// Splits a `tokenize=` argument into a name and arguments, dequoted.
pub(crate) fn parse_spec(spec: &str) -> (Vec<u8>, Vec<Vec<u8>>) {
    let z = spec.as_bytes();
    let (name, mut off) = match next_token(z, 0) {
        Some((s, n)) => (&z[s..s + n], s + n + 1),
        None => (z, z.len()),
    };
    let name = dequote(&String::from_utf8_lossy(name)).into_bytes();
    let mut args = Vec::new();
    while off < z.len() {
        let Some((s, n)) = next_token(z, off) else {
            break;
        };
        args.push(dequote(&String::from_utf8_lossy(&z[s..s + n])).into_bytes());
        off = s + n + 1;
    }
    (name, args)
}

/// `sqlite3Fts3InitTokenizer`.
pub(crate) fn init_tokenizer(
    conn: Option<&Arc<Connection>>,
    spec: &str,
) -> Result<Arc<dyn Tokenizer>> {
    let (name, args) = parse_spec(spec);
    let module = find_module(conn, &name).ok_or_else(|| {
        sql_error(format!(
            "unknown tokenizer: {}",
            String::from_utf8_lossy(&name)
        ))
    })?;
    // SQLite passes the arguments that follow the name as argv[0..].
    module
        .create(&args)
        .ok_or_else(|| sql_error("unknown tokenizer"))
}

/// The "simple" tokenizer: ASCII alphanumeric runs, folded to lower case.
#[derive(Debug)]
pub(crate) struct Simple {
    delim: [bool; 128],
}

impl Simple {
    pub(crate) fn new(args: &[Vec<u8>]) -> Option<Self> {
        let mut delim = [false; 128];
        if args.len() > 1 {
            for &ch in &args[1] {
                if ch >= 0x80 {
                    return None;
                }
                delim[ch as usize] = true;
            }
        } else {
            for (i, d) in delim.iter_mut().enumerate().skip(1) {
                *d = !(i as u8).is_ascii_alphanumeric();
            }
        }
        Some(Self { delim })
    }

    fn is_delim(&self, c: u8) -> bool {
        c < 0x80 && self.delim[c as usize]
    }
}

impl Tokenizer for Simple {
    fn tokenize(&self, _langid: i32, p: &[u8], f: &mut TokenSink) -> Result<()> {
        let mut off = 0;
        let mut pos = 0;
        let mut tok = Vec::new();
        while off < p.len() {
            while off < p.len() && self.is_delim(p[off]) {
                off += 1;
            }
            let start = off;
            while off < p.len() && !self.is_delim(p[off]) {
                off += 1;
            }
            if off > start {
                tok.clear();
                tok.extend(p[start..off].iter().map(|c| c.to_ascii_lowercase()));
                if !f(&tok, start as i32, off as i32, pos)? {
                    return Ok(());
                }
                pos += 1;
            }
        }
        Ok(())
    }
}

/// Collects all tokens of `text`.
pub(crate) fn tokens(
    tok: &dyn Tokenizer,
    langid: i32,
    text: &[u8],
) -> Result<Vec<(Vec<u8>, i32, i32, i32)>> {
    let mut out = Vec::new();
    tok.tokenize(langid, text, &mut |t, s, e, p| {
        out.push((t.to_vec(), s, e, p));
        Ok(true)
    })?;
    Ok(out)
}
