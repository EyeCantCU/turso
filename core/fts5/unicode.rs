//! Unicode tables and helpers of the unicode61 tokenizer (fts5_unicode2.c).

use super::unicode_data::*;

fn remove_diacritic(c: u32, complex: bool) -> u32 {
    let key = (c << 3) | 7;
    let mut res = 0usize;
    let (mut lo, mut hi) = (0i32, DIA.len() as i32 - 1);
    while hi >= lo {
        let test = (hi + lo) / 2;
        if key >= DIA[test as usize] as u32 {
            res = test as usize;
            lo = test + 1;
        } else {
            hi = test - 1;
        }
    }
    if !complex && (DIA_CHAR[res] & 0x80) != 0 {
        return c;
    }
    let entry = DIA[res] as u32;
    if c > (entry >> 3) + (entry & 7) {
        c
    } else {
        (DIA_CHAR[res] & 0x7f) as u32
    }
}

/// Whether `c` is a combining diacritical mark.
pub(crate) fn is_diacritic(c: u32) -> bool {
    const MASK0: u32 = 0x08029FDF;
    const MASK1: u32 = 0x000361F8;
    if !(768..=817).contains(&c) {
        return false;
    }
    if c < 768 + 32 {
        MASK0 & (1 << (c - 768)) != 0
    } else {
        MASK1 & (1 << (c - 768 - 32)) != 0
    }
}

/// Folds `c` to lower case, optionally removing diacritics (1 simple, 2 complex).
pub(crate) fn fold(c: u32, remove_diacritic_mode: i32) -> u32 {
    let mut ret = c;
    if c < 128 {
        if (b'A' as u32..=b'Z' as u32).contains(&c) {
            ret = c + 32;
        }
    } else if c < 65536 {
        let (mut lo, mut hi) = (0i32, FOLD.len() as i32 - 1);
        let mut res = -1i32;
        while hi >= lo {
            let test = (hi + lo) / 2;
            if c as i64 - FOLD[test as usize].0 as i64 >= 0 {
                res = test;
                lo = test + 1;
            } else {
                hi = test - 1;
            }
        }
        if res >= 0 {
            let (code, flags, range) = FOLD[res as usize];
            let code = code as u32;
            if c < code + range as u32 && (1 & flags as u32 & (code ^ c)) == 0 {
                ret = (c + FOLD_OFF[(flags >> 1) as usize] as u32) & 0xffff;
            }
        }
        if remove_diacritic_mode != 0 {
            ret = remove_diacritic(ret, remove_diacritic_mode == 2);
        }
    } else if (66560..66600).contains(&c) {
        ret = c + 40;
    }
    ret
}

/// Parses one category name (e.g. "Lu", "L*") into `cats`. Returns false on error.
pub(crate) fn cat_parse(z: &[u8], cats: &mut [u8; 32]) -> bool {
    cats[0] = 1;
    let c1 = z.get(1).copied().unwrap_or(0);
    let set: &[(u8, usize)] = match z.first().copied().unwrap_or(0) {
        b'C' => &[(b'c', 1), (b'f', 2), (b'n', 3), (b's', 4), (b'o', 31)],
        b'L' => &[
            (b'l', 5),
            (b'm', 6),
            (b'o', 7),
            (b't', 8),
            (b'u', 9),
            (b'C', 30),
        ],
        b'M' => &[(b'c', 10), (b'e', 11), (b'n', 12)],
        b'N' => &[(b'd', 13), (b'l', 14), (b'o', 15)],
        b'P' => &[
            (b'c', 16),
            (b'd', 17),
            (b'e', 18),
            (b'f', 19),
            (b'i', 20),
            (b'o', 21),
            (b's', 22),
        ],
        b'S' => &[(b'c', 23), (b'k', 24), (b'm', 25), (b'o', 26)],
        b'Z' => &[(b'l', 27), (b'p', 28), (b's', 29)],
        _ => return false,
    };
    if c1 == b'*' {
        for &(_, i) in set {
            cats[i] = 1;
        }
        return true;
    }
    match set.iter().find(|(ch, _)| *ch == c1) {
        Some(&(_, i)) => {
            cats[i] = 1;
            true
        }
        None => false,
    }
}

/// The unicode category index (0..31) of `code`.
pub(crate) fn category(code: u32) -> usize {
    if code >= (1 << 20) {
        return 0;
    }
    let mut lo = CAT_BLOCK[(code >> 16) as usize] as i32;
    let mut hi = CAT_BLOCK[1 + (code >> 16) as usize] as i32;
    let key = (code & 0xffff) as u16;
    let mut res = -1i32;
    while hi > lo {
        let test = (hi + lo) / 2;
        if key >= CAT_MAP[test as usize] {
            res = test;
            lo = test + 1;
        } else {
            hi = test;
        }
    }
    if res < 0 {
        return 0;
    }
    let res = res as usize;
    if key as u32 >= CAT_MAP[res] as u32 + (CAT_DATA[res] >> 5) as u32 {
        return 0;
    }
    let ret = (CAT_DATA[res] & 0x1f) as usize;
    if ret != 30 {
        return ret;
    }
    if (key - CAT_MAP[res]) & 1 != 0 {
        5
    } else {
        9
    }
}

/// Fills the ASCII token-character table from a category set.
pub(crate) fn ascii_table(cats: &[u8; 32], ascii: &mut [u8; 128]) {
    let mut i = 0usize;
    let mut tbl = 0usize;
    while i < 128 {
        let token = cats[(CAT_DATA[tbl] & 0x1f) as usize];
        let n = (CAT_DATA[tbl] >> 5) as usize + i;
        while i < 128 && i < n {
            ascii[i] = token;
            i += 1;
        }
        tbl += 1;
    }
    ascii[0] = 0;
}
