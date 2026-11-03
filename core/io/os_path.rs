//! Lossless file paths in `&str`.
//!
//! Core passes database paths around as `&str`, but a Unix path is any byte
//! string without NUL. A path that is not valid UTF-8 is carried in a `String`
//! by [`encode`]: a leading NUL (which no real path contains) marks it, and
//! every byte that is not part of a valid UTF-8 character becomes one code
//! point in U+10FF00..=U+10FFFF. [`to_path`] restores the exact bytes where the
//! path reaches the OS. UTF-8 paths are unchanged, so appending suffixes such
//! as `-wal` works on both forms.

use std::borrow::Cow;
use std::path::Path;

const MARK: char = '\0';
const BYTE_BASE: u32 = 0x10FF00;

fn push_byte(out: &mut String, b: u8) {
    out.push(char::from_u32(BYTE_BASE + b as u32).expect("valid code point"));
}

/// The `&str` form of a path given as raw bytes.
pub fn encode(bytes: &[u8]) -> Cow<'_, str> {
    if let Ok(s) = std::str::from_utf8(bytes) {
        if !s.starts_with(MARK) {
            return Cow::Borrowed(s);
        }
    }
    let mut out = String::with_capacity(bytes.len() + 8);
    out.push(MARK);
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if (c as u32) >= BYTE_BASE {
                // Escape characters that would read back as raw bytes.
                let mut buf = [0u8; 4];
                for &b in c.encode_utf8(&mut buf).as_bytes() {
                    push_byte(&mut out, b);
                }
            } else {
                out.push(c);
            }
        }
        for &b in chunk.invalid() {
            push_byte(&mut out, b);
        }
    }
    Cow::Owned(out)
}

/// The raw bytes of a path produced by [`encode`] (or any plain path).
pub fn decode(path: &str) -> Cow<'_, [u8]> {
    let Some(rest) = path.strip_prefix(MARK) else {
        return Cow::Borrowed(path.as_bytes());
    };
    let mut out = Vec::with_capacity(rest.len());
    for c in rest.chars() {
        let v = c as u32;
        if v >= BYTE_BASE {
            out.push((v - BYTE_BASE) as u8);
        } else {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    Cow::Owned(out)
}

/// The OS path for a core path string.
pub fn to_path(path: &str) -> Cow<'_, Path> {
    if !path.starts_with(MARK) {
        return Cow::Borrowed(Path::new(path));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = decode(path).into_owned();
        Cow::Owned(std::ffi::OsStr::from_bytes(&bytes).into())
    }
    #[cfg(not(unix))]
    {
        Cow::Owned(String::from_utf8_lossy(&decode(path)).into_owned().into())
    }
}

/// The core path string for an OS path.
pub fn from_path(path: &Path) -> Cow<'_, str> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        encode(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_paths_are_unchanged() {
        assert!(matches!(encode(b"/tmp/a.db"), Cow::Borrowed("/tmp/a.db")));
        assert_eq!(decode("/tmp/a.db"), &b"/tmp/a.db"[..]);
    }

    #[test]
    fn raw_paths_round_trip_with_suffixes() {
        for raw in [
            &b"/tmp/\xff\xfe.db"[..],
            "/tmp/\u{10FF41}x".as_bytes(),
            b"\x00weird",
            b"/a/\xc3",
        ] {
            let encoded = encode(raw);
            assert_eq!(decode(&encoded), raw);
            let wal = format!("{encoded}-wal");
            let mut expected = raw.to_vec();
            expected.extend_from_slice(b"-wal");
            assert_eq!(decode(&wal), expected);
        }
    }
}
