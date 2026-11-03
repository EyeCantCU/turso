//! FTS5 tokenizers: unicode61, ascii, porter and trigram (fts5_tokenize.c).

use super::unicode;
use crate::sync::{Arc, RwLock};
use std::collections::HashMap;
use std::sync::LazyLock;

pub const SQLITE_OK: i32 = 0;
pub const SQLITE_ERROR: i32 = 1;
pub const SQLITE_DONE: i32 = 101;

pub const TOKENIZE_QUERY: i32 = 0x0001;
pub const TOKENIZE_PREFIX: i32 = 0x0002;
pub const TOKENIZE_DOCUMENT: i32 = 0x0004;
pub const TOKENIZE_AUX: i32 = 0x0008;
pub const TOKEN_COLOCATED: i32 = 0x0001;

/// Pattern matching a tokenizer supports for LIKE/GLOB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    None,
    Like,
    Glob,
}

/// Token callback: (tflags, token, start, end) -> SQLite result code.
/// `SQLITE_DONE` stops the tokenizer without an error.
pub type TokenCallback<'a> = dyn FnMut(i32, &[u8], i32, i32) -> i32 + 'a;

pub trait Tokenizer: Send + Sync {
    /// Returns an SQLite result code.
    fn tokenize(
        &self,
        flags: i32,
        text: &[u8],
        locale: Option<&[u8]>,
        cb: &mut TokenCallback,
    ) -> i32;
    fn pattern(&self) -> Pattern {
        Pattern::None
    }
}

pub trait TokenizerModule: Send + Sync {
    /// Creates a tokenizer instance; `Err` holds an SQLite result code.
    fn create(&self, args: &[String]) -> Result<Box<dyn Tokenizer>, i32>;
}

type Registry = HashMap<String, Arc<dyn TokenizerModule>>;

static REGISTRY: LazyLock<RwLock<Registry>> = LazyLock::new(|| {
    let mut map: Registry = HashMap::new();
    map.insert("unicode61".into(), Arc::new(Unicode61Module));
    map.insert("ascii".into(), Arc::new(AsciiModule));
    map.insert("trigram".into(), Arc::new(TrigramModule));
    map.insert("porter".into(), Arc::new(PorterModule));
    RwLock::new(map)
});

/// Finds a tokenizer module by name (case-insensitive); `None` is the default.
pub(crate) fn find_module(name: Option<&str>) -> Option<Arc<dyn TokenizerModule>> {
    let name = name.unwrap_or("unicode61").to_ascii_lowercase();
    REGISTRY.read().get(&name).cloned()
}

/// Registers (or replaces) a tokenizer module.
pub(crate) fn register_module(name: &str, module: Arc<dyn TokenizerModule>) {
    REGISTRY.write().insert(name.to_ascii_lowercase(), module);
}

/// Reads one UTF-8 character the way FTS5_READ_UTF8 does.
pub(crate) fn read_utf8(z: &[u8], pos: &mut usize) -> u32 {
    const TRANS1: [u8; 64] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
        0x0d, 0x0e, 0x0f, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x00, 0x01, 0x02, 0x03,
        0x00, 0x01, 0x00, 0x00,
    ];
    let mut c = z[*pos] as u32;
    *pos += 1;
    if c >= 0xc0 {
        c = TRANS1[(c - 0xc0) as usize] as u32;
        while *pos < z.len() && (z[*pos] & 0xc0) == 0x80 {
            c = (c << 6) + (0x3f & z[*pos] as u32);
            *pos += 1;
        }
        if c < 0x80 || (c & 0xFFFF_F800) == 0xD800 || (c & 0xFFFF_FFFE) == 0xFFFE {
            c = 0xFFFD;
        }
    }
    c
}

pub(crate) fn write_utf8(out: &mut Vec<u8>, c: u32) {
    if c < 0x80 {
        out.push(c as u8);
    } else if c < 0x800 {
        out.push(0xC0 + ((c >> 6) & 0x1F) as u8);
        out.push(0x80 + (c & 0x3F) as u8);
    } else if c < 0x10000 {
        out.push(0xE0 + ((c >> 12) & 0x0F) as u8);
        out.push(0x80 + ((c >> 6) & 0x3F) as u8);
        out.push(0x80 + (c & 0x3F) as u8);
    } else {
        out.push(0xF0 + ((c >> 18) & 0x07) as u8);
        out.push(0x80 + ((c >> 12) & 0x3F) as u8);
        out.push(0x80 + ((c >> 6) & 0x3F) as u8);
        out.push(0x80 + (c & 0x3F) as u8);
    }
}

fn done(rc: i32) -> i32 {
    if rc == SQLITE_DONE {
        SQLITE_OK
    } else {
        rc
    }
}

const ASCII_TOKEN_CHAR: [u8; 128] = {
    let mut a = [0u8; 128];
    let mut i = 0;
    while i < 128 {
        let c = i as u8;
        if c.is_ascii_alphanumeric() {
            a[i] = 1;
        }
        i += 1;
    }
    a
};

// ---------------------------------------------------------------- ascii

struct AsciiModule;

struct AsciiTokenizer {
    token_char: [u8; 128],
}

impl TokenizerModule for AsciiModule {
    fn create(&self, args: &[String]) -> Result<Box<dyn Tokenizer>, i32> {
        if args.len() % 2 != 0 {
            return Err(SQLITE_ERROR);
        }
        let mut t = AsciiTokenizer {
            token_char: ASCII_TOKEN_CHAR,
        };
        for pair in args.chunks(2) {
            let token = if pair[0].eq_ignore_ascii_case("tokenchars") {
                1
            } else if pair[0].eq_ignore_ascii_case("separators") {
                0
            } else {
                return Err(SQLITE_ERROR);
            };
            for b in pair[1].bytes().filter(|b| b & 0x80 == 0) {
                t.token_char[b as usize] = token;
            }
        }
        Ok(Box::new(t))
    }
}

impl Tokenizer for AsciiTokenizer {
    fn tokenize(&self, _flags: i32, text: &[u8], _l: Option<&[u8]>, cb: &mut TokenCallback) -> i32 {
        let a = &self.token_char;
        let n = text.len();
        let mut is = 0usize;
        let mut rc = SQLITE_OK;
        let mut fold = Vec::new();
        while is < n && rc == SQLITE_OK {
            while is < n && (text[is] & 0x80) == 0 && a[text[is] as usize] == 0 {
                is += 1;
            }
            if is == n {
                break;
            }
            let mut ie = is + 1;
            while ie < n && ((text[ie] & 0x80) != 0 || a[text[ie] as usize] != 0) {
                ie += 1;
            }
            fold.clear();
            fold.extend(text[is..ie].iter().map(|c| c.to_ascii_lowercase()));
            rc = cb(0, &fold, is as i32, ie as i32);
            is = ie + 1;
        }
        done(rc)
    }
}

// ---------------------------------------------------------------- unicode61

struct Unicode61Module;

pub(crate) struct Unicode61Tokenizer {
    token_char: [u8; 128],
    remove_diacritic: i32,
    exceptions: Vec<u32>,
    category: [u8; 32],
}

impl Unicode61Tokenizer {
    fn add_exceptions(&mut self, z: &str, token_chars: bool) {
        let z = z.as_bytes();
        let mut pos = 0;
        while pos < z.len() {
            let code = read_utf8(z, &mut pos);
            if code < 128 {
                self.token_char[code as usize] = token_chars as u8;
            } else {
                let token = self.category[unicode::category(code)] != 0;
                if token != token_chars && !unicode::is_diacritic(code) {
                    let i = self
                        .exceptions
                        .iter()
                        .position(|&e| e > code)
                        .unwrap_or(self.exceptions.len());
                    self.exceptions.insert(i, code);
                }
            }
        }
    }

    fn is_exception(&self, code: u32) -> bool {
        self.exceptions.binary_search(&code).is_ok()
    }

    fn is_alnum(&self, code: u32) -> bool {
        (self.category[unicode::category(code)] != 0) ^ self.is_exception(code)
    }
}

impl TokenizerModule for Unicode61Module {
    fn create(&self, args: &[String]) -> Result<Box<dyn Tokenizer>, i32> {
        if args.len() % 2 != 0 {
            return Err(SQLITE_ERROR);
        }
        let mut t = Unicode61Tokenizer {
            token_char: [0; 128],
            remove_diacritic: 1,
            exceptions: Vec::new(),
            category: [0; 32],
        };
        let mut cat = "L* N* Co";
        for pair in args.chunks(2) {
            if pair[0].eq_ignore_ascii_case("categories") {
                cat = &pair[1];
            }
        }
        let z = cat.as_bytes();
        let mut i = 0;
        while i < z.len() {
            while i < z.len() && (z[i] == b' ' || z[i] == b'\t') {
                i += 1;
            }
            if i < z.len() && !unicode::cat_parse(&z[i..], &mut t.category) {
                return Err(SQLITE_ERROR);
            }
            while i < z.len() && z[i] != b' ' && z[i] != b'\t' {
                i += 1;
            }
        }
        unicode::ascii_table(&t.category, &mut t.token_char);
        for pair in args.chunks(2) {
            let arg = pair[1].as_bytes();
            if pair[0].eq_ignore_ascii_case("remove_diacritics") {
                if arg.len() != 1 || !(b'0'..=b'2').contains(&arg[0]) {
                    return Err(SQLITE_ERROR);
                }
                t.remove_diacritic = (arg[0] - b'0') as i32;
            } else if pair[0].eq_ignore_ascii_case("tokenchars") {
                t.add_exceptions(&pair[1], true);
            } else if pair[0].eq_ignore_ascii_case("separators") {
                t.add_exceptions(&pair[1], false);
            } else if !pair[0].eq_ignore_ascii_case("categories") {
                return Err(SQLITE_ERROR);
            }
        }
        Ok(Box::new(t))
    }
}

impl Tokenizer for Unicode61Tokenizer {
    fn tokenize(&self, _flags: i32, text: &[u8], _l: Option<&[u8]>, cb: &mut TokenCallback) -> i32 {
        let a = &self.token_char;
        let n = text.len();
        let mut csr = 0usize;
        let mut out: Vec<u8> = Vec::with_capacity(64);
        loop {
            out.clear();
            // Skip separators, stopping at the first token character.
            let is;
            loop {
                if csr >= n {
                    return SQLITE_OK;
                }
                if text[csr] & 0x80 != 0 {
                    let start = csr;
                    let code = read_utf8(text, &mut csr);
                    if self.is_alnum(code) {
                        is = start;
                        let code = unicode::fold(code, self.remove_diacritic);
                        if code != 0 {
                            write_utf8(&mut out, code);
                        }
                        break;
                    }
                } else {
                    if a[text[csr] as usize] != 0 {
                        is = csr;
                        out.push(text[csr].to_ascii_lowercase());
                        csr += 1;
                        break;
                    }
                    csr += 1;
                }
            }
            let mut ie = csr;
            // Gather the rest of the token.
            while csr < n {
                if text[csr] & 0x80 != 0 {
                    let save = csr;
                    let code = read_utf8(text, &mut csr);
                    if self.is_alnum(code) || unicode::is_diacritic(code) {
                        let code = unicode::fold(code, self.remove_diacritic);
                        if code != 0 {
                            write_utf8(&mut out, code);
                        }
                    } else {
                        // The separator is consumed, as in SQLite.
                        let _ = save;
                        break;
                    }
                } else if a[text[csr] as usize] == 0 {
                    break;
                } else {
                    out.push(text[csr].to_ascii_lowercase());
                    csr += 1;
                }
                ie = csr;
            }
            let rc = cb(0, &out, is as i32, ie as i32);
            if rc != SQLITE_OK {
                return done(rc);
            }
        }
    }
}

// ---------------------------------------------------------------- porter

struct PorterModule;

struct PorterTokenizer {
    parent: Box<dyn Tokenizer>,
}

impl TokenizerModule for PorterModule {
    fn create(&self, args: &[String]) -> Result<Box<dyn Tokenizer>, i32> {
        let mut args = args;
        let mut base = "unicode61";
        while let Some(first) = args.first() {
            if first.eq_ignore_ascii_case("porter") {
                args = &args[1..];
            } else {
                base = first;
                break;
            }
        }
        let module = find_module(Some(base)).ok_or(SQLITE_ERROR)?;
        let rest = if args.is_empty() { &[][..] } else { &args[1..] };
        Ok(Box::new(PorterTokenizer {
            parent: module.create(rest)?,
        }))
    }
}

impl Tokenizer for PorterTokenizer {
    fn tokenize(
        &self,
        flags: i32,
        text: &[u8],
        locale: Option<&[u8]>,
        cb: &mut TokenCallback,
    ) -> i32 {
        let mut buf = Vec::with_capacity(128);
        self.parent
            .tokenize(flags, text, locale, &mut |tflags, token, s, e| {
                if token.len() > 64 || token.len() < 3 {
                    return cb(tflags, token, s, e);
                }
                buf.clear();
                buf.extend_from_slice(token);
                porter_stem(&mut buf);
                cb(tflags, &buf, s, e)
            })
    }
}

fn is_vowel(c: u8, y_is_vowel: bool) -> bool {
    matches!(c, b'a' | b'e' | b'i' | b'o' | b'u') || (y_is_vowel && c == b'y')
}

fn gobble_vc(z: &[u8], prev_cons: bool) -> usize {
    let mut cons = prev_cons;
    let mut i = 0;
    while i < z.len() {
        cons = !is_vowel(z[i], cons);
        if !cons {
            break;
        }
        i += 1;
    }
    i += 1;
    while i < z.len() {
        cons = !is_vowel(z[i], cons);
        if cons {
            return i + 1;
        }
        i += 1;
    }
    0
}

fn m_gt0(z: &[u8]) -> bool {
    gobble_vc(z, false) != 0
}

fn m_gt1(z: &[u8]) -> bool {
    let n = gobble_vc(z, false);
    n != 0 && gobble_vc(&z[n..], true) != 0
}

fn m_eq1(z: &[u8]) -> bool {
    let n = gobble_vc(z, false);
    n != 0 && gobble_vc(&z[n..], true) == 0
}

fn ostar(z: &[u8]) -> bool {
    let last = z[z.len() - 1];
    if last == b'w' || last == b'x' || last == b'y' {
        return false;
    }
    let mut mask = 0;
    let mut cons = false;
    for &c in z {
        cons = !is_vowel(c, cons);
        mask = ((mask << 1) + cons as i32) & 7;
    }
    mask == 5
}

fn m_gt1_and_s_or_t(z: &[u8]) -> bool {
    let last = z[z.len() - 1];
    (last == b's' || last == b't') && m_gt1(z)
}

fn has_vowel(z: &[u8]) -> bool {
    z.iter().enumerate().any(|(i, &c)| is_vowel(c, i > 0))
}

#[derive(Clone, Copy)]
enum Cond {
    None,
    MGt0,
    MGt1,
    MGt1ST,
    Vowel,
}

fn cond(c: Cond, z: &[u8]) -> bool {
    match c {
        Cond::None => true,
        Cond::MGt0 => m_gt0(z),
        Cond::MGt1 => m_gt1(z),
        Cond::MGt1ST => m_gt1_and_s_or_t(z),
        Cond::Vowel => has_vowel(z),
    }
}

/// One generated porter step: rules grouped by the second-to-last byte, as
/// in mkportersteps.tcl. The first rule whose suffix matches decides.
/// Returns true if a rule that sets the step's return value fired.
fn porter_step(buf: &mut Vec<u8>, rules: &[(&[u8], Cond, &[u8], bool)]) -> bool {
    let n = buf.len();
    // aBuf[nBuf-2] can read before the buffer in SQLite; the byte there is 0.
    let key = if n >= 2 { buf[n - 2] } else { 0 };
    for &(suffix, c, out, ret) in rules {
        if suffix[suffix.len() - 2] != key {
            continue;
        }
        if n > suffix.len() && buf[n - suffix.len()..] == *suffix {
            let stem = n - suffix.len();
            if cond(c, &buf[..stem]) {
                buf.truncate(stem);
                buf.extend_from_slice(out);
                return ret;
            }
            return false;
        }
    }
    false
}

const STEP1B: &[(&[u8], Cond, &[u8], bool)] = &[
    (b"eed", Cond::MGt0, b"ee", false),
    (b"ed", Cond::Vowel, b"", true),
    (b"ing", Cond::Vowel, b"", true),
];

const STEP1B2: &[(&[u8], Cond, &[u8], bool)] = &[
    (b"at", Cond::None, b"ate", true),
    (b"bl", Cond::None, b"ble", true),
    (b"iz", Cond::None, b"ize", true),
];

const STEP2: &[(&[u8], Cond, &[u8], bool)] = &[
    (b"ational", Cond::MGt0, b"ate", false),
    (b"tional", Cond::MGt0, b"tion", false),
    (b"enci", Cond::MGt0, b"ence", false),
    (b"anci", Cond::MGt0, b"ance", false),
    (b"izer", Cond::MGt0, b"ize", false),
    (b"logi", Cond::MGt0, b"log", false),
    (b"bli", Cond::MGt0, b"ble", false),
    (b"alli", Cond::MGt0, b"al", false),
    (b"entli", Cond::MGt0, b"ent", false),
    (b"eli", Cond::MGt0, b"e", false),
    (b"ousli", Cond::MGt0, b"ous", false),
    (b"ization", Cond::MGt0, b"ize", false),
    (b"ation", Cond::MGt0, b"ate", false),
    (b"ator", Cond::MGt0, b"ate", false),
    (b"alism", Cond::MGt0, b"al", false),
    (b"iveness", Cond::MGt0, b"ive", false),
    (b"fulness", Cond::MGt0, b"ful", false),
    (b"ousness", Cond::MGt0, b"ous", false),
    (b"aliti", Cond::MGt0, b"al", false),
    (b"iviti", Cond::MGt0, b"ive", false),
    (b"biliti", Cond::MGt0, b"ble", false),
];

const STEP3: &[(&[u8], Cond, &[u8], bool)] = &[
    (b"ical", Cond::MGt0, b"ic", false),
    (b"ness", Cond::MGt0, b"", false),
    (b"icate", Cond::MGt0, b"ic", false),
    (b"iciti", Cond::MGt0, b"ic", false),
    (b"ful", Cond::MGt0, b"", false),
    (b"ative", Cond::MGt0, b"", false),
    (b"alize", Cond::MGt0, b"al", false),
];

const STEP4: &[(&[u8], Cond, &[u8], bool)] = &[
    (b"al", Cond::MGt1, b"", false),
    (b"ance", Cond::MGt1, b"", false),
    (b"ence", Cond::MGt1, b"", false),
    (b"er", Cond::MGt1, b"", false),
    (b"ic", Cond::MGt1, b"", false),
    (b"able", Cond::MGt1, b"", false),
    (b"ible", Cond::MGt1, b"", false),
    (b"ant", Cond::MGt1, b"", false),
    (b"ement", Cond::MGt1, b"", false),
    (b"ment", Cond::MGt1, b"", false),
    (b"ent", Cond::MGt1, b"", false),
    (b"ion", Cond::MGt1ST, b"", false),
    (b"ou", Cond::MGt1, b"", false),
    (b"ism", Cond::MGt1, b"", false),
    (b"ate", Cond::MGt1, b"", false),
    (b"iti", Cond::MGt1, b"", false),
    (b"ous", Cond::MGt1, b"", false),
    (b"ive", Cond::MGt1, b"", false),
    (b"ize", Cond::MGt1, b"", false),
];

fn porter_stem(buf: &mut Vec<u8>) {
    // Step 1a.
    let n = buf.len();
    if buf[n - 1] == b's' {
        if buf[n - 2] == b'e' {
            if (n > 4 && buf[n - 4] == b's' && buf[n - 3] == b's') || (n > 3 && buf[n - 3] == b'i')
            {
                buf.truncate(n - 2);
            } else {
                buf.truncate(n - 1);
            }
        } else if buf[n - 2] != b's' {
            buf.truncate(n - 1);
        }
    }
    // Step 1b.
    if porter_step(buf, STEP1B) && !porter_step(buf, STEP1B2) {
        let n = buf.len();
        let c = buf[n - 1];
        let prev = if n >= 2 { buf[n - 2] } else { 0 };
        if !is_vowel(c, false) && c != b'l' && c != b's' && c != b'z' && c == prev {
            buf.truncate(n - 1);
        } else if m_eq1(buf) && ostar(buf) {
            buf.push(b'e');
        }
    }
    // Step 1c.
    let n = buf.len();
    if buf[n - 1] == b'y' && has_vowel(&buf[..n - 1]) {
        buf[n - 1] = b'i';
    }
    porter_step(buf, STEP2);
    porter_step(buf, STEP3);
    porter_step(buf, STEP4);
    // Step 5a.
    let n = buf.len();
    if buf[n - 1] == b'e'
        && (m_gt1(&buf[..n - 1]) || (m_eq1(&buf[..n - 1]) && !ostar(&buf[..n - 1])))
    {
        buf.truncate(n - 1);
    }
    // Step 5b.
    let n = buf.len();
    if n > 1 && buf[n - 1] == b'l' && buf[n - 2] == b'l' && m_gt1(&buf[..n - 1]) {
        buf.truncate(n - 1);
    }
}

// ---------------------------------------------------------------- trigram

struct TrigramModule;

struct TrigramTokenizer {
    fold: bool,
    fold_param: i32,
}

impl TokenizerModule for TrigramModule {
    fn create(&self, args: &[String]) -> Result<Box<dyn Tokenizer>, i32> {
        if args.len() % 2 != 0 {
            return Err(SQLITE_ERROR);
        }
        let mut t = TrigramTokenizer {
            fold: true,
            fold_param: 0,
        };
        for pair in args.chunks(2) {
            let arg = pair[1].as_bytes();
            if pair[0].eq_ignore_ascii_case("case_sensitive") {
                if arg.len() != 1 || (arg[0] != b'0' && arg[0] != b'1') {
                    return Err(SQLITE_ERROR);
                }
                t.fold = arg[0] == b'0';
            } else if pair[0].eq_ignore_ascii_case("remove_diacritics") {
                if arg.len() != 1 || !(b'0'..=b'2').contains(&arg[0]) {
                    return Err(SQLITE_ERROR);
                }
                t.fold_param = if arg[0] != b'0' { 2 } else { 0 };
            } else {
                return Err(SQLITE_ERROR);
            }
        }
        if t.fold_param != 0 && !t.fold {
            return Err(SQLITE_ERROR);
        }
        Ok(Box::new(t))
    }
}

impl Tokenizer for TrigramTokenizer {
    fn tokenize(&self, _flags: i32, text: &[u8], _l: Option<&[u8]>, cb: &mut TokenCallback) -> i32 {
        let n = text.len();
        let mut pos = 0usize;
        let mut buf: Vec<u8> = Vec::with_capacity(32);
        let mut starts = [0usize; 3];
        let read = |pos: &mut usize| -> u32 {
            let mut c = read_utf8(text, pos);
            if self.fold {
                c = unicode::fold(c, self.fold_param);
            }
            c
        };
        for start in starts.iter_mut() {
            loop {
                *start = pos;
                if pos >= n {
                    return SQLITE_OK;
                }
                let c = read(&mut pos);
                if c != 0 {
                    write_utf8(&mut buf, c);
                    break;
                }
            }
        }
        loop {
            let mut next;
            let mut code;
            loop {
                next = pos;
                if pos >= n {
                    code = 0;
                    break;
                }
                code = read(&mut pos);
                if code != 0 {
                    break;
                }
            }
            let rc = cb(0, &buf, starts[0] as i32, next as i32);
            if code == 0 || rc != SQLITE_OK {
                return done(rc);
            }
            // Drop the first character and append the new one.
            let mut skip = 1;
            if buf[0] >= 0xc0 {
                while skip < buf.len() && (buf[skip] & 0xc0) == 0x80 {
                    skip += 1;
                }
            }
            buf.drain(..skip);
            write_utf8(&mut buf, code);
            starts = [starts[1], starts[2], next];
        }
    }

    fn pattern(&self) -> Pattern {
        if self.fold_param == 0 {
            if self.fold {
                Pattern::Like
            } else {
                Pattern::Glob
            }
        } else {
            Pattern::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(args: &[&str], text: &str) -> Vec<(String, i32, i32)> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let module = find_module(args.first().map(|s| s.as_str())).unwrap();
        let tok = module
            .create(if args.is_empty() { &[] } else { &args[1..] })
            .unwrap();
        let mut out = Vec::new();
        tok.tokenize(
            TOKENIZE_DOCUMENT,
            text.as_bytes(),
            None,
            &mut |_, t, s, e| {
                out.push((String::from_utf8_lossy(t).into_owned(), s, e));
                SQLITE_OK
            },
        );
        out
    }

    #[test]
    fn unicode61_folds_and_removes_diacritics() {
        let t = tokens(&[], "Héllo, WÖRLD! naïve");
        let words: Vec<_> = t.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(words, ["hello", "world", "naive"]);
        assert_eq!((t[0].1, t[0].2), (0, 6));
    }

    #[test]
    fn porter_stems() {
        let t = tokens(&["porter"], "connections running happily relational");
        let words: Vec<_> = t.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(words, ["connect", "run", "happili", "relat"]);
    }

    #[test]
    fn trigram_splits() {
        let t = tokens(&["trigram"], "abcd");
        let words: Vec<_> = t.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(words, ["abc", "bcd"]);
    }
}
