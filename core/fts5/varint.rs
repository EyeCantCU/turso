//! SQLite varints and the fixed-width integers used by the FTS5 format.

/// Reads an SQLite varint at `a[*off..]`, advancing `*off`. Bytes past the
/// end of `a` read as zero, like the zero padding SQLite adds to records.
pub(crate) fn get_varint(a: &[u8], off: &mut usize) -> u64 {
    let at = |i: usize| a.get(*off + i).copied().unwrap_or(0) as u64;
    let mut v: u64 = 0;
    for i in 0..8 {
        let b = at(i);
        v = (v << 7) | (b & 0x7f);
        if b & 0x80 == 0 {
            *off += i + 1;
            return v;
        }
    }
    v = (v << 8) | at(8);
    *off += 9;
    v
}

/// Reads a varint and keeps the low 32 bits, as `fts5GetVarint32` does.
pub(crate) fn get_varint32(a: &[u8], off: &mut usize) -> u32 {
    get_varint(a, off) as u32
}

pub(crate) fn varint_len(v: u64) -> usize {
    let mut buf = [0u8; 9];
    put_varint_into(&mut buf, v)
}

fn put_varint_into(buf: &mut [u8; 9], v: u64) -> usize {
    if v & (0xff000000u64 << 32) != 0 {
        buf[8] = v as u8;
        let mut v = v >> 8;
        for i in (0..8).rev() {
            buf[i] = ((v & 0x7f) | 0x80) as u8;
            v >>= 7;
        }
        return 9;
    }
    let mut tmp = [0u8; 10];
    let mut n = 0;
    let mut v = v;
    loop {
        tmp[n] = ((v & 0x7f) | 0x80) as u8;
        n += 1;
        v >>= 7;
        if v == 0 {
            break;
        }
    }
    tmp[0] &= 0x7f;
    for i in 0..n {
        buf[i] = tmp[n - 1 - i];
    }
    n
}

pub(crate) fn put_varint(out: &mut Vec<u8>, v: u64) {
    let mut buf = [0u8; 9];
    let n = put_varint_into(&mut buf, v);
    out.extend_from_slice(&buf[..n]);
}

pub(crate) fn get_u16(a: &[u8], off: usize) -> usize {
    ((a.get(off).copied().unwrap_or(0) as usize) << 8)
        | a.get(off + 1).copied().unwrap_or(0) as usize
}

pub(crate) fn put_u16(a: &mut [u8], off: usize, v: u16) {
    a[off] = (v >> 8) as u8;
    a[off + 1] = v as u8;
}

pub(crate) fn get_u32(a: &[u8], off: usize) -> u32 {
    let b = |i: usize| a.get(off + i).copied().unwrap_or(0) as u32;
    (b(0) << 24) | (b(1) << 16) | (b(2) << 8) | b(3)
}

pub(crate) fn put_u32(out: &mut [u8], off: usize, v: u32) {
    out[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

pub(crate) fn get_u64(a: &[u8], off: usize) -> u64 {
    ((get_u32(a, off) as u64) << 32) | get_u32(a, off + 4) as u64
}

/// Position-list entries pack (column << 32) | offset.
pub(crate) fn pos_col(pos: i64) -> i32 {
    ((pos >> 32) & 0x7fff_ffff) as i32
}

pub(crate) fn pos_off(pos: i64) -> i32 {
    (pos & 0x7fff_ffff) as i32
}

/// Iterates over a full-detail position list, as `sqlite3Fts5PoslistNext64`.
/// Returns `None` at the end.
pub(crate) fn poslist_next(a: &[u8], off: &mut usize, pos: &mut i64) -> Option<i64> {
    loop {
        if *off >= a.len() {
            return None;
        }
        let mut v = get_varint32(a, off) as i64;
        if v == 1 {
            if *off >= a.len() {
                return None;
            }
            let col = get_varint32(a, off) as i64;
            *pos = col << 32;
            v = get_varint32(a, off) as i64;
            if v < 2 {
                // A column marker must be followed by an offset.
                return None;
            }
        }
        if v == 0 {
            continue;
        }
        *pos = (*pos & !0x7fff_ffff_i64) | ((*pos & 0x7fff_ffff) + (v - 2)) & 0x7fff_ffff;
        return Some(*pos);
    }
}

/// Collects every position in a full-detail position list.
pub(crate) fn poslist_positions(a: &[u8]) -> Vec<i64> {
    let mut out = Vec::new();
    let mut off = 0;
    let mut pos = 0i64;
    while let Some(p) = poslist_next(a, &mut off, &mut pos) {
        out.push(p);
    }
    out
}

/// Builds a position list, as `sqlite3Fts5PoslistWriterAppend`.
#[derive(Default, Debug, Clone)]
pub(crate) struct PoslistWriter {
    prev: i64,
}

impl PoslistWriter {
    pub(crate) fn append(&mut self, out: &mut Vec<u8>, pos: i64) {
        const COLMASK: i64 = 0x7fff_ffff << 32;
        if pos < self.prev {
            return;
        }
        if (pos & COLMASK) != (self.prev & COLMASK) {
            out.push(1);
            put_varint(out, (pos >> 32) as u64);
            self.prev = pos & COLMASK;
        }
        put_varint(out, ((pos - self.prev) + 2) as u64);
        self.prev = pos;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trip() {
        for v in [
            0u64,
            1,
            127,
            128,
            240,
            2287,
            16383,
            16384,
            1 << 32,
            u64::MAX,
            i64::MAX as u64,
        ] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            assert_eq!(buf.len(), varint_len(v));
            let mut off = 0;
            assert_eq!(get_varint(&buf, &mut off), v);
            assert_eq!(off, buf.len());
        }
    }
}
