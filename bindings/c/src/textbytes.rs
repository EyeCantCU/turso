//! Byte-exact strings through APIs that work on `&str`.
//!
//! [`escape`] maps bytes that are not part of valid UTF-8, and characters in
//! U+10FF00..=U+10FFFF, to one code point U+10FF00 + byte each; [`unescape`]
//! maps them back. Formatting code that only copies such characters (printf's
//! %s, %q, %Q, %w) therefore round-trips arbitrary bytes.

const BYTE_BASE: u32 = 0x10FF00;

fn push_byte(out: &mut String, b: u8) {
    out.push(char::from_u32(BYTE_BASE + b as u32).expect("valid code point"));
}

/// Returns the escaped string and whether anything was escaped.
pub(crate) fn escape(bytes: &[u8]) -> (String, bool) {
    let mut out = String::with_capacity(bytes.len());
    let mut escaped = false;
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if (c as u32) >= BYTE_BASE {
                let mut buf = [0u8; 4];
                for &b in c.encode_utf8(&mut buf).as_bytes() {
                    push_byte(&mut out, b);
                }
                escaped = true;
            } else {
                out.push(c);
            }
        }
        for &b in chunk.invalid() {
            push_byte(&mut out, b);
            escaped = true;
        }
    }
    (out, escaped)
}

pub(crate) fn unescape(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        let v = c as u32;
        if v >= BYTE_BASE {
            out.push((v - BYTE_BASE) as u8);
        } else {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_round_trips() {
        for raw in [&b"plain"[..], b"a\xfab", "\u{10FF41}".as_bytes(), b"\xc3"] {
            let (s, _) = escape(raw);
            assert_eq!(unescape(&s), raw);
        }
        assert!(!escape(b"plain").1);
    }
}
