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
