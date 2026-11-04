//! The "unicode61" tokenizer (`fts3_unicode.c`, `fts3_unicode2.c`).

use super::tokenizer::{TokenSink, Tokenizer};
use super::unicode_data::*;
use crate::Result;

pub(crate) fn is_alnum(c: i32) -> bool {
    let c = c as u32;
    if c < 128 {
        return ALNUM_ASCII[(c >> 5) as usize] & (1u32 << (c & 0x1f)) == 0;
    }
    if c < (1 << 22) {
        let key = (c << 10) | 0x3ff;
        let mut res = 0usize;
        let (mut lo, mut hi) = (0i32, ALNUM_ENTRY.len() as i32 - 1);
        while hi >= lo {
            let t = (hi + lo) / 2;
            if key >= ALNUM_ENTRY[t as usize] {
                res = t as usize;
                lo = t + 1;
            } else {
                hi = t - 1;
            }
        }
        let e = ALNUM_ENTRY[res];
        return c >= (e >> 10) + (e & 0x3ff);
    }
    true
}

fn remove_diacritic(c: i32, complex: bool) -> i32 {
    let key = ((c as u32) << 3) | 7;
    let mut res = 0usize;
    let (mut lo, mut hi) = (0i32, DIA.len() as i32 - 1);
    while hi >= lo {
        let t = (hi + lo) / 2;
        if key >= DIA[t as usize] as u32 {
            res = t as usize;
            lo = t + 1;
        } else {
            hi = t - 1;
        }
    }
    if !complex && DIA_CHAR[res] & 0x80 != 0 {
        return c;
    }
    let d = DIA[res] as i32;
    if c > (d >> 3) + (d & 7) {
        c
    } else {
        (DIA_CHAR[res] & 0x7f) as i32
    }
}

pub(crate) fn is_diacritic(c: i32) -> bool {
    let mask0: u32 = 0x08029FDF;
    let mask1: u32 = 0x000361F8;
    if !(768..=817).contains(&c) {
        return false;
    }
    if c < 768 + 32 {
        mask0 & (1u32 << (c - 768)) != 0
    } else {
        mask1 & (1u32 << (c - 768 - 32)) != 0
    }
}

pub(crate) fn fold(c: i32, remove_diacritics: i32) -> i32 {
    let mut ret = c;
    if c < 128 {
        if (b'A' as i32..=b'Z' as i32).contains(&c) {
            ret = c + 32;
        }
    } else if c < 65536 {
        let mut res = -1i32;
        let (mut lo, mut hi) = (0i32, FOLD.len() as i32 - 1);
        while hi >= lo {
            let t = (hi + lo) / 2;
            if c - FOLD[t as usize].0 as i32 >= 0 {
                res = t;
                lo = t + 1;
            } else {
                hi = t - 1;
            }
        }
        if res >= 0 {
            let (code, flags, n) = FOLD[res as usize];
            let (code, flags, n) = (code as i32, flags as i32, n as i32);
            if c < code + n && 0 == (0x01 & flags & (code ^ c)) {
                ret = (c + FOLD_OFF[(flags >> 1) as usize] as i32) & 0xffff;
            }
        }
        if remove_diacritics != 0 {
            ret = remove_diacritic(ret, remove_diacritics == 2);
        }
    } else if (66560..66600).contains(&c) {
        ret = c + 40;
    }
    ret
}

const UTF8_TRANS1: [u8; 64] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x00, 0x01, 0x02, 0x03, 0x00, 0x01, 0x00, 0x00,
];

/// `FTS3_READ_UTF8`: decodes one character at `*i`.
pub(crate) fn read_utf8(z: &[u8], i: &mut usize) -> u32 {
    let mut c = z[*i] as u32;
    *i += 1;
    if c >= 0xc0 {
        c = UTF8_TRANS1[(c - 0xc0) as usize] as u32;
        while *i < z.len() && (z[*i] & 0xc0) == 0x80 {
            c = (c << 6) + (0x3f & z[*i] as u32);
            *i += 1;
        }
        if c < 0x80 || (c & 0xFFFFF800) == 0xD800 || (c & 0xFFFFFFFE) == 0xFFFE {
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

#[derive(Debug)]
pub(crate) struct Unicode61 {
    remove_diacritic: i32,
    exceptions: Vec<i32>,
}

impl Unicode61 {
    /// `unicodeCreate`; `None` for an unrecognized argument.
    pub(crate) fn new(args: &[Vec<u8>]) -> Option<Self> {
        let mut t = Self {
            remove_diacritic: 1,
            exceptions: Vec::new(),
        };
        for z in args {
            match z.as_slice() {
                b"remove_diacritics=1" => t.remove_diacritic = 1,
                b"remove_diacritics=0" => t.remove_diacritic = 0,
                b"remove_diacritics=2" => t.remove_diacritic = 2,
                _ if z.starts_with(b"tokenchars=") => t.add_exceptions(true, &z[11..]),
                _ if z.starts_with(b"separators=") => t.add_exceptions(false, &z[11..]),
                _ => return None,
            }
        }
        Some(t)
    }

    fn add_exceptions(&mut self, alnum: bool, z: &[u8]) {
        let mut i = 0;
        while i < z.len() {
            let c = read_utf8(z, &mut i) as i32;
            if is_alnum(c) != alnum && !is_diacritic(c) {
                let pos = self.exceptions.partition_point(|&e| e < c);
                self.exceptions.insert(pos, c);
            }
        }
    }

    fn is_alnum(&self, c: i32) -> bool {
        is_alnum(c) ^ self.exceptions.binary_search(&c).is_ok()
    }
}

impl Tokenizer for Unicode61 {
    fn tokenize(&self, _langid: i32, input: &[u8], f: &mut TokenSink) -> Result<()> {
        let term = input.len();
        let mut off = 0usize;
        let mut pos = 0;
        let mut tok = Vec::new();
        loop {
            let mut z = off;
            let mut start = z;
            let mut code = 0u32;
            while z < term {
                code = read_utf8(input, &mut z);
                if self.is_alnum(code as i32) {
                    break;
                }
                start = z;
            }
            if start >= term {
                return Ok(());
            }
            tok.clear();
            let mut end;
            loop {
                end = z;
                let out = fold(code as i32, self.remove_diacritic);
                if out != 0 {
                    write_utf8(&mut tok, out as u32);
                }
                if z >= term {
                    break;
                }
                code = read_utf8(input, &mut z);
                if !(self.is_alnum(code as i32) || is_diacritic(code as i32)) {
                    break;
                }
            }
            off = z;
            if !f(&tok, start as i32, end as i32, pos)? {
                return Ok(());
            }
            pos += 1;
        }
    }
}
