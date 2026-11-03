//! The "porter" tokenizer (`fts3_porter.c`): ASCII words, Porter-stemmed.

use super::tokenizer::{TokenSink, Tokenizer};
use crate::Result;

const C_TYPE: [u8; 26] = [
    0, 1, 1, 1, 0, 1, 1, 1, 0, 1, 1, 1, 1, 1, 0, 1, 1, 1, 1, 1, 0, 1, 1, 1, 2, 1,
];

/// The word is stored reversed in a NUL-padded buffer, as in SQLite.
struct Rev {
    b: [u8; 28],
}

impl Rev {
    fn at(&self, i: usize) -> u8 {
        self.b.get(i).copied().unwrap_or(0)
    }
    fn is_consonant(&self, i: usize) -> bool {
        let x = self.at(i);
        if x == 0 {
            return false;
        }
        let j = C_TYPE[(x - b'a') as usize];
        if j < 2 {
            return j == 1;
        }
        self.at(i + 1) == 0 || self.is_vowel(i + 1)
    }
    fn is_vowel(&self, i: usize) -> bool {
        let x = self.at(i);
        if x == 0 {
            return false;
        }
        let j = C_TYPE[(x - b'a') as usize];
        if j < 2 {
            return j == 0;
        }
        self.is_consonant(i + 1)
    }
    fn skip_v(&self, mut i: usize) -> usize {
        while self.is_vowel(i) {
            i += 1;
        }
        i
    }
    fn skip_c(&self, mut i: usize) -> usize {
        while self.is_consonant(i) {
            i += 1;
        }
        i
    }
    fn m_gt_0(&self, z: usize) -> bool {
        let z = self.skip_v(z);
        if self.at(z) == 0 {
            return false;
        }
        let z = self.skip_c(z);
        self.at(z) != 0
    }
    fn m_eq_1(&self, z: usize) -> bool {
        let z = self.skip_v(z);
        if self.at(z) == 0 {
            return false;
        }
        let z = self.skip_c(z);
        if self.at(z) == 0 {
            return false;
        }
        let z = self.skip_v(z);
        if self.at(z) == 0 {
            return true;
        }
        let z = self.skip_c(z);
        self.at(z) == 0
    }
    fn m_gt_1(&self, z: usize) -> bool {
        let z = self.skip_v(z);
        if self.at(z) == 0 {
            return false;
        }
        let z = self.skip_c(z);
        if self.at(z) == 0 {
            return false;
        }
        let z = self.skip_v(z);
        if self.at(z) == 0 {
            return false;
        }
        let z = self.skip_c(z);
        self.at(z) != 0
    }
    fn has_vowel(&self, z: usize) -> bool {
        let z = self.skip_c(z);
        self.at(z) != 0
    }
    fn double_consonant(&self, z: usize) -> bool {
        self.is_consonant(z) && self.at(z) == self.at(z + 1)
    }
    fn star_oh(&self, z: usize) -> bool {
        self.is_consonant(z)
            && !matches!(self.at(z), b'w' | b'x' | b'y')
            && self.is_vowel(z + 1)
            && self.is_consonant(z + 2)
    }
    /// Replaces suffix `from` (reversed) with `to` if `cond` holds.
    /// Returns true when `from` matched, whether or not it was replaced.
    fn stem(
        &mut self,
        pz: &mut usize,
        from: &[u8],
        to: &[u8],
        cond: Option<fn(&Rev, usize) -> bool>,
    ) -> bool {
        let mut z = *pz;
        for &c in from {
            if self.at(z) != c {
                return false;
            }
            z += 1;
        }
        if let Some(cond) = cond {
            if !cond(self, z) {
                return true;
            }
        }
        for &c in to {
            z -= 1;
            self.b[z] = c;
        }
        *pz = z;
        true
    }
}

fn copy_stemmer(input: &[u8], out: &mut Vec<u8>) {
    out.clear();
    let mut has_digit = false;
    for &c in input {
        if c.is_ascii_uppercase() {
            out.push(c - b'A' + b'a');
        } else {
            if c.is_ascii_digit() {
                has_digit = true;
            }
            out.push(c);
        }
    }
    let mx = if has_digit { 3 } else { 10 };
    let n = input.len();
    if n > mx * 2 {
        let tail: Vec<u8> = out[n - mx..].to_vec();
        out.truncate(mx);
        out.extend_from_slice(&tail);
    }
}

pub(crate) fn porter_stem(input: &[u8], out: &mut Vec<u8>) {
    let n = input.len();
    if !(3..21).contains(&n) {
        copy_stemmer(input, out);
        return;
    }
    let mut r = Rev { b: [0; 28] };
    let mut j = 22usize;
    for &c in input {
        if c.is_ascii_uppercase() {
            r.b[j] = c + b'a' - b'A';
        } else if c.is_ascii_lowercase() {
            r.b[j] = c;
        } else {
            copy_stemmer(input, out);
            return;
        }
        j -= 1;
    }
    let mut z = j + 1;
    let gt0: Option<fn(&Rev, usize) -> bool> = Some(Rev::m_gt_0);
    let gt1: Option<fn(&Rev, usize) -> bool> = Some(Rev::m_gt_1);

    // Step 1a
    if r.at(z) == b's'
        && !r.stem(&mut z, b"sess", b"ss", None)
        && !r.stem(&mut z, b"sei", b"i", None)
        && !r.stem(&mut z, b"ss", b"ss", None)
    {
        z += 1;
    }

    // Step 1b
    let z2 = z;
    if r.stem(&mut z, b"dee", b"ee", gt0) {
        // nothing
    } else if (r.stem(&mut z, b"gni", b"", Some(Rev::has_vowel))
        || r.stem(&mut z, b"de", b"", Some(Rev::has_vowel)))
        && z != z2
    {
        if r.stem(&mut z, b"ta", b"ate", None)
            || r.stem(&mut z, b"lb", b"ble", None)
            || r.stem(&mut z, b"zi", b"ize", None)
        {
        } else if r.double_consonant(z) && !matches!(r.at(z), b'l' | b's' | b'z') {
            z += 1;
        } else if r.m_eq_1(z) && r.star_oh(z) {
            z -= 1;
            r.b[z] = b'e';
        }
    }

    // Step 1c
    if r.at(z) == b'y' && r.has_vowel(z + 1) {
        r.b[z] = b'i';
    }

    // Step 2
    match r.at(z + 1) {
        b'a' => {
            if !r.stem(&mut z, b"lanoita", b"ate", gt0) {
                r.stem(&mut z, b"lanoit", b"tion", gt0);
            }
        }
        b'c' => {
            if !r.stem(&mut z, b"icne", b"ence", gt0) {
                r.stem(&mut z, b"icna", b"ance", gt0);
            }
        }
        b'e' => {
            r.stem(&mut z, b"rezi", b"ize", gt0);
        }
        b'g' => {
            r.stem(&mut z, b"igol", b"log", gt0);
        }
        b'l' => {
            if !r.stem(&mut z, b"ilb", b"ble", gt0)
                && !r.stem(&mut z, b"illa", b"al", gt0)
                && !r.stem(&mut z, b"iltne", b"ent", gt0)
                && !r.stem(&mut z, b"ile", b"e", gt0)
            {
                r.stem(&mut z, b"ilsuo", b"ous", gt0);
            }
        }
        b'o' => {
            if !r.stem(&mut z, b"noitazi", b"ize", gt0) && !r.stem(&mut z, b"noita", b"ate", gt0) {
                r.stem(&mut z, b"rota", b"ate", gt0);
            }
        }
        b's' => {
            if !r.stem(&mut z, b"msila", b"al", gt0)
                && !r.stem(&mut z, b"ssenevi", b"ive", gt0)
                && !r.stem(&mut z, b"ssenluf", b"ful", gt0)
            {
                r.stem(&mut z, b"ssensuo", b"ous", gt0);
            }
        }
        b't' => {
            if !r.stem(&mut z, b"itila", b"al", gt0) && !r.stem(&mut z, b"itivi", b"ive", gt0) {
                r.stem(&mut z, b"itilib", b"ble", gt0);
            }
        }
        _ => {}
    }

    // Step 3
    match r.at(z) {
        b'e' => {
            if !r.stem(&mut z, b"etaci", b"ic", gt0) && !r.stem(&mut z, b"evita", b"", gt0) {
                r.stem(&mut z, b"ezila", b"al", gt0);
            }
        }
        b'i' => {
            r.stem(&mut z, b"itici", b"ic", gt0);
        }
        b'l' => {
            if !r.stem(&mut z, b"laci", b"ic", gt0) {
                r.stem(&mut z, b"luf", b"", gt0);
            }
        }
        b's' => {
            r.stem(&mut z, b"ssen", b"", gt0);
        }
        _ => {}
    }

    // Step 4
    match r.at(z + 1) {
        b'a' => {
            if r.at(z) == b'l' && r.m_gt_1(z + 2) {
                z += 2;
            }
        }
        b'c' => {
            if r.at(z) == b'e'
                && r.at(z + 2) == b'n'
                && matches!(r.at(z + 3), b'a' | b'e')
                && r.m_gt_1(z + 4)
            {
                z += 4;
            }
        }
        b'e' => {
            if r.at(z) == b'r' && r.m_gt_1(z + 2) {
                z += 2;
            }
        }
        b'i' => {
            if r.at(z) == b'c' && r.m_gt_1(z + 2) {
                z += 2;
            }
        }
        b'l' => {
            if r.at(z) == b'e'
                && r.at(z + 2) == b'b'
                && matches!(r.at(z + 3), b'a' | b'i')
                && r.m_gt_1(z + 4)
            {
                z += 4;
            }
        }
        b'n' => {
            if r.at(z) == b't' {
                if r.at(z + 2) == b'a' {
                    if r.m_gt_1(z + 3) {
                        z += 3;
                    }
                } else if r.at(z + 2) == b'e'
                    && !r.stem(&mut z, b"tneme", b"", gt1)
                    && !r.stem(&mut z, b"tnem", b"", gt1)
                {
                    r.stem(&mut z, b"tne", b"", gt1);
                }
            }
        }
        b'o' => {
            if r.at(z) == b'u' {
                if r.m_gt_1(z + 2) {
                    z += 2;
                }
            } else if matches!(r.at(z + 3), b's' | b't') {
                r.stem(&mut z, b"noi", b"", gt1);
            }
        }
        b's' => {
            if r.at(z) == b'm' && r.at(z + 2) == b'i' && r.m_gt_1(z + 3) {
                z += 3;
            }
        }
        b't' => {
            if !r.stem(&mut z, b"eta", b"", gt1) {
                r.stem(&mut z, b"iti", b"", gt1);
            }
        }
        b'u' => {
            if r.at(z) == b's' && r.at(z + 2) == b'o' && r.m_gt_1(z + 3) {
                z += 3;
            }
        }
        b'v' | b'z' => {
            if r.at(z) == b'e' && r.at(z + 2) == b'i' && r.m_gt_1(z + 3) {
                z += 3;
            }
        }
        _ => {}
    }

    // Step 5a
    if r.at(z) == b'e' && (r.m_gt_1(z + 1) || (r.m_eq_1(z + 1) && !r.star_oh(z + 1))) {
        z += 1;
    }

    // Step 5b
    if r.m_gt_1(z) && r.at(z) == b'l' && r.at(z + 1) == b'l' {
        z += 1;
    }

    out.clear();
    let mut end = z;
    while r.at(end) != 0 {
        end += 1;
    }
    out.extend(r.b[z..end].iter().rev());
}

fn is_delim(c: u8) -> bool {
    c & 0x80 == 0 && !(c.is_ascii_alphanumeric() || c == b'_')
}

#[derive(Debug, Default)]
pub(crate) struct Porter;

impl Tokenizer for Porter {
    fn tokenize(&self, _langid: i32, z: &[u8], f: &mut TokenSink) -> Result<()> {
        let mut off = 0;
        let mut pos = 0;
        let mut buf = Vec::new();
        while off < z.len() {
            while off < z.len() && is_delim(z[off]) {
                off += 1;
            }
            let start = off;
            while off < z.len() && !is_delim(z[off]) {
                off += 1;
            }
            if off > start {
                porter_stem(&z[start..off], &mut buf);
                if !f(&buf, start as i32, off as i32, pos)? {
                    return Ok(());
                }
                pos += 1;
            }
        }
        Ok(())
    }
}
