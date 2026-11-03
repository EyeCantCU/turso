//! Byte-level helpers for TEXT values that are not valid UTF-8.
//!
//! SQLite stores TEXT as raw bytes and never validates UTF-8. These helpers
//! reproduce SQLite's byte loops so such values behave exactly as in SQLite.

/// sqlite3Utf8Trans1: payload bits of a lead byte 0xC0..=0xFF.
const UTF8_TRANS1: [u8; 64] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x00, 0x01, 0x02, 0x03, 0x00, 0x01, 0x00, 0x00,
];

#[inline]
fn is_continuation(b: u8) -> bool {
    b & 0xc0 == 0x80
}

/// Decodes one character the way sqlite3Utf8Read does. Returns the code
/// point and the number of bytes consumed (at least 1).
#[inline]
pub(crate) fn sqlite_utf8_read(bytes: &[u8]) -> (u32, usize) {
    let mut c = bytes[0] as u32;
    let mut i = 1;
    if c >= 0xc0 {
        c = UTF8_TRANS1[(c - 0xc0) as usize] as u32;
        while i < bytes.len() && is_continuation(bytes[i]) {
            c = (c << 6).wrapping_add((bytes[i] & 0x3f) as u32);
            i += 1;
        }
        if c < 0x80 || (c & 0xFFFF_F800) == 0xD800 || (c & 0xFFFF_FFFE) == 0xFFFE {
            c = 0xFFFD;
        }
    }
    (c, i)
}

/// Appends the UTF-8 view SQLite would decode from `bytes` (U+FFFD for
/// malformed characters) to `out`.
pub(crate) fn push_lossy(bytes: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] < 0x80 {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let (c, n) = sqlite_utf8_read(&bytes[i..]);
        let ch = char::from_u32(c).unwrap_or('\u{FFFD}');
        let mut tmp = [0u8; 4];
        out.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
        i += n;
    }
}

/// The lossy UTF-8 view of `bytes` as a `String`.
pub(crate) fn lossy_string(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 2);
    push_lossy(bytes, &mut out);
    // SAFETY: push_lossy only writes ASCII bytes and encoded chars.
    unsafe { String::from_utf8_unchecked(out) }
}

/// length() of a TEXT value, as SQLite 3.54 counts it: a byte >= 0x80 and
/// the continuation bytes after it are one character; stops at NUL.
pub(crate) fn sqlite_char_len(bytes: &[u8]) -> usize {
    let mut i = 0;
    let mut n = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0 {
            break;
        }
        i += 1;
        if b >= 0x80 {
            while i < bytes.len() && is_continuation(bytes[i]) {
                i += 1;
            }
        }
        n += 1;
    }
    n
}

/// Characters before the first NUL, counted with SQLITE_SKIP_UTF8 (used by
/// substr() for negative start positions).
fn skip_utf8_len(bytes: &[u8]) -> i64 {
    let mut i = 0;
    let mut n = 0;
    while i < bytes.len() && bytes[i] != 0 {
        let b = bytes[i];
        i += 1;
        if b >= 0xc0 {
            while i < bytes.len() && is_continuation(bytes[i]) {
                i += 1;
            }
        }
        n += 1;
    }
    n
}

/// Advances over `count` characters from `start` with substr()'s loop.
fn substr_advance(bytes: &[u8], mut i: usize, mut count: i64) -> usize {
    while count > 0 && i < bytes.len() {
        let b = bytes[i];
        if b == 0 {
            break;
        }
        i += 1;
        if b >= 0x80 {
            while i < bytes.len() && is_continuation(bytes[i]) {
                i += 1;
            }
        }
        count -= 1;
    }
    i
}

/// substr(X, p1[, p2]) on TEXT bytes, following SQLite's substrFunc. `p2` is
/// None when omitted. Returns the byte range of the result.
pub(crate) fn sqlite_substr(bytes: &[u8], mut p1: i64, p2: Option<i64>) -> (usize, usize) {
    let len = if p1 < 0 { skip_utf8_len(bytes) } else { 0 };
    let mut p2 = p2.unwrap_or(i64::MAX);
    if p1 < 0 {
        p1 += len;
        if p1 < 0 {
            if p2 < 0 {
                p2 = 0;
            } else {
                p2 = p2.saturating_add(p1);
            }
            p1 = 0;
        }
    } else if p1 > 0 {
        p1 -= 1;
    } else if p2 > 0 {
        p2 -= 1;
    }
    if p2 < 0 {
        if p2 < p1.saturating_neg() {
            p2 = p1;
        } else {
            p2 = -p2;
        }
        p1 -= p2;
    }
    let start = substr_advance(bytes, 0, p1);
    let end = substr_advance(bytes, start, p2);
    (start, end)
}

/// Byte-wise ASCII case mapping, as SQLite's built-in upper()/lower().
pub(crate) fn ascii_map(bytes: &[u8], upper: bool) -> Vec<u8> {
    if upper {
        bytes.to_ascii_uppercase()
    } else {
        bytes.to_ascii_lowercase()
    }
}

/// trim()/ltrim()/rtrim() on TEXT bytes, following SQLite's trimFunc.
/// `charset` None trims spaces. Returns the byte range of the result.
pub(crate) fn sqlite_trim(
    bytes: &[u8],
    charset: Option<&[u8]>,
    left: bool,
    right: bool,
) -> (usize, usize) {
    let mut units: Vec<&[u8]> = Vec::new();
    match charset {
        None => units.push(b" "),
        Some(set) => {
            let mut i = 0;
            while i < set.len() && set[i] != 0 {
                let start = i;
                i += 1;
                if set[start] >= 0xc0 {
                    while i < set.len() && is_continuation(set[i]) {
                        i += 1;
                    }
                }
                units.push(&set[start..i]);
            }
        }
    }
    let (mut start, mut end) = (0, bytes.len());
    if units.is_empty() {
        return (start, end);
    }
    if left {
        while start < end {
            let Some(unit) = units.iter().find(|u| bytes[start..end].starts_with(u)) else {
                break;
            };
            start += unit.len();
        }
    }
    if right {
        while start < end {
            let Some(unit) = units.iter().find(|u| bytes[start..end].ends_with(u)) else {
                break;
            };
            end -= unit.len();
        }
    }
    (start, end)
}

/// instr() on TEXT bytes, following SQLite's instrFunc. `needle` is not empty.
pub(crate) fn sqlite_instr(haystack: &[u8], needle: &[u8]) -> usize {
    let mut n = 1;
    let mut i = 0;
    while needle.len() <= haystack.len() - i && !haystack[i..].starts_with(needle) {
        n += 1;
        i += 1;
        while i < haystack.len() && is_continuation(haystack[i]) {
            i += 1;
        }
    }
    if needle.len() > haystack.len() - i {
        0
    } else {
        n
    }
}

/// replace() on TEXT bytes: every non-overlapping `pattern` (not empty), left
/// to right, becomes `replacement`.
pub(crate) fn replace_bytes(source: &[u8], pattern: &[u8], replacement: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(source.len());
    let mut i = 0;
    while i < source.len() {
        if source[i..].starts_with(pattern) {
            out.extend_from_slice(replacement);
            i += pattern.len();
        } else {
            out.push(source[i]);
            i += 1;
        }
    }
    out
}

/// quote() of TEXT bytes: single quotes doubled, cut at the first NUL.
pub(crate) fn quote_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 2);
    out.push(b'\'');
    for &b in bytes {
        if b == 0 {
            break;
        }
        if b == b'\'' {
            out.push(b'\'');
        }
        out.push(b);
    }
    out.push(b'\'');
    out
}

/// Text encoding of the database whose program is running on this thread.
/// Records store TEXT in this encoding; values in registers are always UTF-8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DbEncoding {
    Utf8 = 1,
    Utf16Le = 2,
    Utf16Be = 3,
}

impl DbEncoding {
    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            2 => Self::Utf16Le,
            3 => Self::Utf16Be,
            _ => Self::Utf8,
        }
    }

    pub(crate) fn from_header(enc: crate::storage::sqlite3_ondisk::TextEncoding) -> Self {
        use crate::storage::sqlite3_ondisk::TextEncoding;
        if enc == TextEncoding::Utf16Le {
            Self::Utf16Le
        } else if enc == TextEncoding::Utf16Be {
            Self::Utf16Be
        } else {
            Self::Utf8
        }
    }

    pub(crate) fn to_header(self) -> crate::storage::sqlite3_ondisk::TextEncoding {
        use crate::storage::sqlite3_ondisk::TextEncoding;
        match self {
            Self::Utf8 => TextEncoding::Utf8,
            Self::Utf16Le => TextEncoding::Utf16Le,
            Self::Utf16Be => TextEncoding::Utf16Be,
        }
    }

    /// Parses a PRAGMA encoding value as SQLite does.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        let le = cfg!(target_endian = "little");
        Some(match name.to_ascii_lowercase().as_str() {
            "utf-8" | "utf8" => Self::Utf8,
            "utf-16le" | "utf16le" => Self::Utf16Le,
            "utf-16be" | "utf16be" => Self::Utf16Be,
            "utf-16" | "utf16" if le => Self::Utf16Le,
            "utf-16" | "utf16" => Self::Utf16Be,
            _ => return None,
        })
    }

    #[inline]
    pub(crate) fn is_utf8(self) -> bool {
        self == Self::Utf8
    }
}

thread_local! {
    static DB_ENCODING: std::cell::Cell<DbEncoding> = const { std::cell::Cell::new(DbEncoding::Utf8) };
}

/// The record text encoding in effect on this thread.
#[inline(always)]
pub(crate) fn db_encoding() -> DbEncoding {
    DB_ENCODING.with(|e| e.get())
}

/// Sets the record text encoding for this thread until dropped.
pub(crate) struct EncodingGuard(DbEncoding);

impl EncodingGuard {
    #[inline]
    pub(crate) fn set(enc: DbEncoding) -> Self {
        Self(DB_ENCODING.with(|e| e.replace(enc)))
    }
}

impl Drop for EncodingGuard {
    #[inline]
    fn drop(&mut self) {
        DB_ENCODING.with(|e| e.set(self.0));
    }
}

/// UTF-16 to UTF-8, as sqlite3VdbeMemTranslate does it (an odd trailing byte
/// is dropped; a surrogate always pairs with the next unit).
pub(crate) fn utf16_to_utf8(bytes: &[u8], big_endian: bool) -> Vec<u8> {
    let n = bytes.len() & !1;
    let mut out = Vec::with_capacity(n + n / 2);
    let unit = |i: usize| -> u32 {
        if big_endian {
            ((bytes[i] as u32) << 8) | bytes[i + 1] as u32
        } else {
            bytes[i] as u32 | ((bytes[i + 1] as u32) << 8)
        }
    };
    let mut i = 0;
    while i < n {
        let mut c = unit(i);
        i += 2;
        if (0xd800..0xe000).contains(&c) && i < n {
            let c2 = unit(i);
            i += 2;
            c = (c2 & 0x03FF) + ((c & 0x003F) << 10) + (((c & 0x03C0) + 0x0040) << 10);
        }
        if c < 0x80 {
            out.push(c as u8);
        } else if c < 0x800 {
            out.extend_from_slice(&[0xC0 + ((c >> 6) & 0x1F) as u8, 0x80 + (c & 0x3F) as u8]);
        } else if c < 0x10000 {
            out.extend_from_slice(&[
                0xE0 + ((c >> 12) & 0x0F) as u8,
                0x80 + ((c >> 6) & 0x3F) as u8,
                0x80 + (c & 0x3F) as u8,
            ]);
        } else {
            out.extend_from_slice(&[
                0xF0 + ((c >> 18) & 0x07) as u8,
                0x80 + ((c >> 12) & 0x3F) as u8,
                0x80 + ((c >> 6) & 0x3F) as u8,
                0x80 + (c & 0x3F) as u8,
            ]);
        }
    }
    out
}

/// UTF-8 (possibly invalid) to UTF-16, as sqlite3VdbeMemTranslate does it.
pub(crate) fn utf8_to_utf16(bytes: &[u8], big_endian: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() * 2);
    let mut push = |u: u16| {
        let b = if big_endian {
            u.to_be_bytes()
        } else {
            u.to_le_bytes()
        };
        out.extend_from_slice(&b);
    };
    let mut i = 0;
    while i < bytes.len() {
        let (c, n) = if bytes[i] < 0x80 {
            (bytes[i] as u32, 1)
        } else {
            sqlite_utf8_read(&bytes[i..])
        };
        i += n;
        if c <= 0xFFFF {
            push(c as u16);
        } else {
            // WRITE_UTF16 surrogates, including SQLite's handling above U+10FFFF.
            let hi = (((c >> 10) & 0x003F) + (((c.wrapping_sub(0x10000)) >> 10) & 0x00C0))
                | ((0x00D8 + ((c.wrapping_sub(0x10000) >> 18) & 0x03)) << 8);
            let lo = (c & 0x00FF) | ((0x00DC + ((c >> 8) & 0x03)) << 8);
            push(hi as u16);
            push(lo as u16);
        }
    }
    out
}

/// Bytes of UTF-8 text in the record encoding `enc`.
pub(crate) fn encode_for(enc: DbEncoding, utf8: &[u8]) -> Vec<u8> {
    match enc {
        DbEncoding::Utf8 => utf8.to_vec(),
        DbEncoding::Utf16Le => utf8_to_utf16(utf8, false),
        DbEncoding::Utf16Be => utf8_to_utf16(utf8, true),
    }
}

/// UTF-8 text as the bytes of the current record encoding.
#[inline]
pub(crate) fn db_encoded(utf8: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    match db_encoding() {
        DbEncoding::Utf8 => std::borrow::Cow::Borrowed(utf8),
        enc => std::borrow::Cow::Owned(encode_for(enc, utf8)),
    }
}

/// Bytes in the current record encoding (a blob read as text) as UTF-8.
#[inline]
pub(crate) fn db_decoded(bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    match db_encoding() {
        DbEncoding::Utf8 => std::borrow::Cow::Borrowed(bytes),
        enc => std::borrow::Cow::Owned(utf16_to_utf8(bytes, enc == DbEncoding::Utf16Be)),
    }
}

/// BINARY order of two UTF-8 texts in a database of encoding `enc`: SQLite
/// memcmp()s the stored form, which for UTF-16 differs from UTF-8 order.
pub(crate) fn binary_cmp_utf8_in(enc: DbEncoding, a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let ord = a.cmp(b);
    if enc.is_utf8() || ord.is_eq() {
        return ord;
    }
    // The orders agree unless the first difference involves a non-ASCII byte.
    let i = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    match (a.get(i), b.get(i)) {
        (Some(&x), Some(&y)) if x >= 0x80 || y >= 0x80 => {
            encode_for(enc, a).cmp(&encode_for(enc, b))
        }
        _ => ord,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lossy_view_matches_sqlite_decoding() {
        assert_eq!(lossy_string(b"abc"), "abc");
        assert_eq!(lossy_string(b"a\xfab"), "a\u{FFFD}b");
        // A stray continuation byte decodes to the code point of the same value.
        assert_eq!(lossy_string(b"\x80"), "\u{80}");
        // Overlong encodings collapse into one replacement character.
        assert_eq!(lossy_string(b"\xc0\x80x"), "\u{FFFD}x");
        assert_eq!(lossy_string("é".as_bytes()), "é");
    }

    #[test]
    fn char_len_matches_sqlite() {
        assert_eq!(sqlite_char_len(b"xxx\xfayyy"), 7);
        assert_eq!(sqlite_char_len(b"\x80\x80"), 1);
        assert_eq!(sqlite_char_len(b"\xc3\xa9\x80"), 1);
        assert_eq!(sqlite_char_len(b"ab\0cd"), 2);
        assert_eq!(sqlite_char_len(b""), 0);
    }

    #[test]
    fn substr_matches_sqlite() {
        let s = b"xxx\xfayyy";
        assert_eq!(sqlite_substr(s, 4, Some(1)), (3, 4));
        assert_eq!(sqlite_substr(s, 4, None), (3, 7));
        assert_eq!(sqlite_substr(s, -4, Some(2)), (3, 5));
        assert_eq!(sqlite_substr(s, 0, Some(2)), (0, 1));
        assert_eq!(sqlite_substr(s, 5, Some(-2)), (2, 4));
    }

    #[test]
    fn trim_instr_replace_quote_are_bytewise() {
        assert_eq!(sqlite_trim(b"  \xfa  ", None, true, true), (2, 3));
        assert_eq!(
            sqlite_trim(b"\xfa\xfaab\xfa", Some(b"\xfa"), true, true),
            (2, 4)
        );
        assert_eq!(sqlite_instr(b"xxx\xfayyy", b"y"), 5);
        assert_eq!(sqlite_instr(b"xxx\xfayyy", b"\xfa"), 4);
        assert_eq!(sqlite_instr(b"xxx\xfayyy", b"z"), 0);
        assert_eq!(replace_bytes(b"a\xfab\xfa", b"\xfa", b"--"), b"a--b--");
        assert_eq!(quote_bytes(b"it's\xfa"), b"'it''s\xfa'");
    }

    #[test]
    fn utf16_transcoding_round_trips() {
        for s in ["", "abc", "héllo", "\u{20AC}", "\u{1F600}x", "\u{FFFD}"] {
            for be in [false, true] {
                let u = utf8_to_utf16(s.as_bytes(), be);
                let expected: Vec<u8> = s
                    .encode_utf16()
                    .flat_map(|c| if be { c.to_be_bytes() } else { c.to_le_bytes() })
                    .collect();
                assert_eq!(u, expected);
                assert_eq!(utf16_to_utf8(&u, be), s.as_bytes());
            }
        }
        // Invalid UTF-8 becomes U+FFFD; an odd trailing byte is dropped.
        assert_eq!(utf8_to_utf16(b"\xff", false), [0xFD, 0xFF]);
        assert_eq!(utf16_to_utf8(b"a\0b", false), b"a");
    }

    #[test]
    fn binary_order_follows_the_stored_encoding() {
        use std::cmp::Ordering::*;
        // U+0100 sorts before 'b' in UTF-16LE bytes (00 01 vs 62 00).
        let (a, b) = ("a\u{100}".as_bytes(), "ab".as_bytes());
        assert_eq!(binary_cmp_utf8_in(DbEncoding::Utf8, a, b), Greater);
        assert_eq!(binary_cmp_utf8_in(DbEncoding::Utf16Le, a, b), Less);
        assert_eq!(binary_cmp_utf8_in(DbEncoding::Utf16Be, a, b), Greater);
        // U+FF5E sorts after U+1F600 in UTF-8 but before it in UTF-16BE.
        let (c, d) = ("\u{FF5E}".as_bytes(), "\u{1F600}".as_bytes());
        assert_eq!(binary_cmp_utf8_in(DbEncoding::Utf8, c, d), Less);
        assert_eq!(binary_cmp_utf8_in(DbEncoding::Utf16Be, c, d), Greater);
    }
}

#[cfg(test)]
mod sql_tests {
    use crate::sync::Arc;
    use crate::types::Text;
    use crate::{
        ConnectOpts, Connection, Database, DatabaseOpts, MemoryIO, OpenFlags, SqliteDialect, Value,
        IO,
    };
    use std::num::NonZero;

    fn connect() -> Arc<Connection> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = Database::open_file_with_flags(
            io,
            ":memory:",
            OpenFlags::Create,
            DatabaseOpts::new(),
            None,
            Arc::new(SqliteDialect),
        )
        .unwrap();
        db.connect(ConnectOpts::new()).unwrap()
    }

    fn rows(conn: &Arc<Connection>, sql: &str, params: &[Value]) -> Vec<Vec<Value>> {
        let mut stmt = conn.prepare(sql).unwrap();
        for (i, p) in params.iter().enumerate() {
            stmt.bind_at(NonZero::new(i + 1).unwrap(), p.clone())
                .unwrap();
        }
        stmt.run_collect_rows().unwrap()
    }

    fn one(conn: &Arc<Connection>, sql: &str, params: &[Value]) -> Value {
        rows(conn, sql, params).remove(0).remove(0)
    }

    fn bytes(v: &Value) -> &[u8] {
        match v {
            Value::Text(t) => t.as_bytes(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    fn raw(b: &[u8]) -> Value {
        Value::Text(Text::from_bytes(b.to_vec()))
    }

    #[test]
    fn invalid_utf8_round_trips_through_a_table_and_an_index() {
        let conn = connect();
        conn.execute("CREATE TABLE t(x TEXT)").unwrap();
        conn.execute("CREATE INDEX tx ON t(x)").unwrap();
        for v in [&b"a\xfab"[..], b"\xff", b"\xc3", b"ok"] {
            rows(&conn, "INSERT INTO t VALUES (?)", &[raw(v)]);
        }
        let got: Vec<Vec<u8>> = rows(&conn, "SELECT x FROM t ORDER BY x", &[])
            .iter()
            .map(|r| bytes(&r[0]).to_vec())
            .collect();
        assert_eq!(got, [&b"a\xfab"[..], b"ok", b"\xc3", b"\xff"]);
        // Index lookups compare the exact bytes.
        let hit = one(&conn, "SELECT count(*) FROM t WHERE x = ?", &[raw(b"\xff")]);
        assert_eq!(hit, Value::from_i64(1));
        let miss = one(&conn, "SELECT count(*) FROM t WHERE x = ?", &[raw(b"\xfe")]);
        assert_eq!(miss, Value::from_i64(0));
        let distinct = one(&conn, "SELECT count(DISTINCT x) FROM t", &[]);
        assert_eq!(distinct, Value::from_i64(4));
    }

    #[test]
    fn functions_see_the_exact_bytes() {
        let conn = connect();
        let concat = one(&conn, "SELECT 'xxx' || x'fa' || 'yyy'", &[]);
        assert_eq!(bytes(&concat), b"xxx\xfayyy");
        let v = [raw(b"xxx\xfayyy")];
        let q = |sql: &str| one(&conn, sql, &v);
        assert_eq!(q("SELECT typeof(?1)"), Value::build_text("text"));
        assert_eq!(q("SELECT length(?1)"), Value::from_i64(7));
        assert_eq!(q("SELECT octet_length(?1)"), Value::from_i64(7));
        assert_eq!(q("SELECT hex(?1)"), Value::build_text("787878FA797979"));
        assert_eq!(bytes(&q("SELECT substr(?1, 4, 2)")), b"\xfay");
        assert_eq!(bytes(&q("SELECT substr(?1, -4)")), b"\xfayyy");
        assert_eq!(q("SELECT instr(?1, 'y')"), Value::from_i64(5));
        assert_eq!(bytes(&q("SELECT upper(?1)")), b"XXX\xfaYYY");
        assert_eq!(bytes(&q("SELECT replace(?1, 'x', '')")), b"\xfayyy");
        assert_eq!(bytes(&q("SELECT trim(?1, 'xy')")), b"\xfa");
        assert_eq!(bytes(&q("SELECT quote(?1)")), b"'xxx\xfayyy'");
        assert_eq!(
            q("SELECT CAST(?1 AS BLOB)"),
            Value::Blob(b"xxx\xfayyy".to_vec())
        );
        assert_eq!(bytes(&q("SELECT CAST(x'fa' AS TEXT)")), b"\xfa");
        assert_eq!(q("SELECT ?1 LIKE 'xxx_yyy'"), Value::from_i64(1));
        assert_eq!(q("SELECT ?1 GLOB 'xxx?yyy'"), Value::from_i64(1));
        assert_eq!(q("SELECT ?1 = 'xxx' || x'fa' || 'yyy'"), Value::from_i64(1));
        assert_eq!(q("SELECT ?1 < 'xxx' || x'fb'"), Value::from_i64(1));
        assert_eq!(bytes(&q("SELECT group_concat(?1, x'ff')")), b"xxx\xfayyy");
        // Concatenating halves of a character yields valid UTF-8.
        let e = one(
            &conn,
            "SELECT CAST(x'c3' AS TEXT) || CAST(x'a9' AS TEXT)",
            &[],
        );
        assert_eq!(e, Value::build_text("é"));
    }
}

#[cfg(test)]
mod utf16_tests {
    use crate::sync::Arc;
    use crate::{
        ConnectOpts, Connection, Database, DatabaseOpts, MemoryIO, OpenFlags, SqliteDialect, Value,
        IO,
    };

    fn connect() -> Arc<Connection> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = Database::open_file_with_flags(
            io,
            ":memory:",
            OpenFlags::Create,
            DatabaseOpts::new(),
            None,
            Arc::new(SqliteDialect),
        )
        .unwrap();
        db.connect(ConnectOpts::new()).unwrap()
    }

    fn col(conn: &Arc<Connection>, sql: &str) -> Vec<Value> {
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.run_collect_rows()
            .unwrap()
            .into_iter()
            .map(|mut r| r.remove(0))
            .collect()
    }

    fn text(s: &str) -> Value {
        Value::build_text(s.to_string())
    }

    #[test]
    fn utf16_databases_store_and_order_text_like_sqlite() {
        for (enc, name, e_hex, order) in [
            // memcmp() of the little-endian bytes: 3D D8.., 5E FF, 61 00 00 01, 61 00 62 00.
            (
                "UTF-16le",
                "UTF-16le",
                "E900",
                ["\u{1F600}", "\u{FF5E}", "a\u{100}", "ab"],
            ),
            (
                "UTF-16be",
                "UTF-16be",
                "00E9",
                ["ab", "a\u{100}", "\u{1F600}", "\u{FF5E}"],
            ),
        ] {
            let conn = connect();
            conn.execute(format!("PRAGMA encoding = '{enc}'")).unwrap();
            assert_eq!(col(&conn, "PRAGMA encoding"), [text(name)]);
            conn.execute("CREATE TABLE t(x TEXT, y TEXT COLLATE NOCASE)")
                .unwrap();
            conn.execute("CREATE INDEX tx ON t(x)").unwrap();
            conn.execute("CREATE INDEX ty ON t(y)").unwrap();
            for v in ["ab", "a\u{100}", "\u{FF5E}", "\u{1F600}"] {
                conn.execute(format!("INSERT INTO t VALUES ('{v}', upper('{v}'))"))
                    .unwrap();
            }
            // Once the database has content the encoding no longer changes.
            conn.execute("PRAGMA encoding = 'UTF-8'").unwrap();
            assert_eq!(col(&conn, "PRAGMA encoding"), [text(name)]);
            let got = col(&conn, "SELECT x FROM t ORDER BY x");
            assert_eq!(got, order.map(text));
            let got = col(
                &conn,
                "SELECT x FROM t INDEXED BY tx WHERE x > '' ORDER BY x",
            );
            assert_eq!(got, order.map(text));
            assert_eq!(col(&conn, "SELECT x FROM t WHERE x = 'ab'"), [text("ab")]);
            assert_eq!(col(&conn, "SELECT x FROM t WHERE y = 'AB'"), [text("ab")]);
            assert_eq!(col(&conn, "SELECT hex('\u{e9}')"), [text(e_hex)]);
            assert_eq!(
                col(&conn, "SELECT octet_length('\u{e9}')"),
                [Value::from_i64(2)]
            );
            assert_eq!(
                col(&conn, "SELECT length('\u{1F600}')"),
                [Value::from_i64(1)]
            );
            let blob = if enc == "UTF-16le" {
                "x'6100'"
            } else {
                "x'0061'"
            };
            assert_eq!(
                col(&conn, &format!("SELECT CAST({blob} AS TEXT)")),
                [text("a")]
            );
            assert_eq!(col(&conn, "PRAGMA integrity_check"), [text("ok")]);
        }
    }
}
