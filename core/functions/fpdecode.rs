//! Port of SQLite 3.54's double <-> decimal helpers (util.c): sqlite3FpDecode,
//! sqlite3Fp2Convert10, sqlite3Fp10Convert2 and powerOfTen.

/// Decimal decode of a double, as SQLite's `FpDecode`.
pub struct FpDecode {
    /// b'+' or b'-'
    pub sign: u8,
    /// 0: finite, 1: infinity, 2: NaN
    pub special: u8,
    /// Significant digits as ASCII (no trailing zeros); empty when special.
    pub digits: Vec<u8>,
    /// Position of the decimal point relative to the first digit.
    pub i_dp: i32,
}

fn mul128(a: u64, b: u64) -> (u64, u64) {
    let r = (a as u128) * (b as u128);
    ((r >> 64) as u64, r as u64)
}

fn mul160(a: u64, a_lo: u32, b: u64) -> (u64, u32) {
    let mut r = (a as u128) * (b as u128);
    r += ((a_lo as u128) * (b as u128)) >> 32;
    ((r >> 64) as u64, ((r >> 32) & 0xffff_ffff) as u32)
}

const POWERSOF10_FIRST: i32 = -348;
const POWERSOF10_LAST: i32 = 347;

const A_BASE: [u64; 27] = [
    0x8000000000000000,
    0xa000000000000000,
    0xc800000000000000,
    0xfa00000000000000,
    0x9c40000000000000,
    0xc350000000000000,
    0xf424000000000000,
    0x9896800000000000,
    0xbebc200000000000,
    0xee6b280000000000,
    0x9502f90000000000,
    0xba43b74000000000,
    0xe8d4a51000000000,
    0x9184e72a00000000,
    0xb5e620f480000000,
    0xe35fa931a0000000,
    0x8e1bc9bf04000000,
    0xb1a2bc2ec5000000,
    0xde0b6b3a76400000,
    0x8ac7230489e80000,
    0xad78ebc5ac620000,
    0xd8d726b7177a8000,
    0x878678326eac9000,
    0xa968163f0a57b400,
    0xd3c21bcecceda100,
    0x84595161401484a0,
    0xa56fa5b99019a5c8,
];

const A_SCALE: [u64; 26] = [
    0x8049a4ac0c5811ae,
    0xcf42894a5dce35ea,
    0xa76c582338ed2621,
    0x873e4f75e2224e68,
    0xda7f5bf590966848,
    0xb080392cc4349dec,
    0x8e938662882af53e,
    0xe65829b3046b0afa,
    0xba121a4650e4ddeb,
    0x964e858c91ba2655,
    0xf2d56790ab41c2a2,
    0xc428d05aa4751e4c,
    0x9e74d1b791e07e48,
    0xcccccccccccccccc,
    0xcecb8f27f4200f3a,
    0xa70c3c40a64e6c51,
    0x86f0ac99b4e8dafd,
    0xda01ee641a708de9,
    0xb01ae745b101e9e4,
    0x8e41ade9fbebc27d,
    0xe5d3ef282a242e81,
    0xb9a74a0637ce2ee1,
    0x95f83d0a1fb69cd9,
    0xf24a01a73cf2dccf,
    0xc3b8358109e84f07,
    0x9e19db92b4e31ba9,
];

const A_SCALE_LO: [u32; 26] = [
    0x205b896d, 0x52064cad, 0xaf2af2b8, 0x5a7744a7, 0xaf39a475, 0xbd8d794e, 0x547eb47b, 0x0cb4a5a3,
    0x92f34d62, 0x3a6a07f9, 0xfae27299, 0xaa97e14c, 0x775ea265, 0xcccccccc, 0x00000000, 0x999090b6,
    0x69a028bb, 0xe80e6f48, 0x5ec05dd0, 0x14588f14, 0x8f1668c9, 0x6d953e2c, 0x4abdaf10, 0xbc633b39,
    0x0a862f81, 0x6c07a2c2,
];

/// Most significant 64 bits of 10^p, plus the next 32 bits.
fn power_of_ten(p: i32) -> (u64, u32) {
    debug_assert!((POWERSOF10_FIRST..=POWERSOF10_LAST).contains(&p));
    let (g, n);
    if p < 0 {
        if p == -1 {
            return (A_SCALE[13], A_SCALE_LO[13]);
        }
        let mut gg = p / 27;
        let mut nn = p % 27;
        if nn != 0 {
            gg -= 1;
            nn += 27;
        }
        g = gg;
        n = nn;
    } else if p < 27 {
        return (A_BASE[p as usize], 0);
    } else {
        g = p / 27;
        n = p % 27;
    }
    let idx = (g + 13) as usize;
    let s = A_SCALE[idx];
    if n == 0 {
        return (s, A_SCALE_LO[idx]);
    }
    let (mut x, mut lo) = mul160(s, A_SCALE_LO[idx], A_BASE[n as usize]);
    if x & (1u64 << 63) == 0 {
        x = (x << 1) | ((lo >> 31) & 1) as u64;
        lo = (lo << 1) | 1;
    }
    (x, lo)
}

fn pwr10to2(p: i32) -> i32 {
    (p * 108853) >> 15
}

fn pwr2to10(p: i32) -> i32 {
    (p * 78913) >> 18
}

/// r == m*2^e  ->  (d, p) with r ~= d*10^p and at least n digits in d.
fn fp2_convert10(m: u64, e: i32, n: i32) -> (u64, i32) {
    let p = n - 1 - pwr2to10(e + 63);
    let (h, _) = mul128(m, power_of_ten(p).0);
    let d = if n == 18 {
        let h = h >> -(e + pwr10to2(p) + 2);
        (h + ((h << 1) & 2)) >> 1
    } else {
        h >> -(e + pwr10to2(p) + 1)
    };
    (d, -p)
}

/// Closest double to d*10^p.
pub fn fp10_convert2(d: u64, p: i32) -> f64 {
    if p < POWERSOF10_FIRST {
        return 0.0;
    }
    if p > POWERSOF10_LAST {
        return f64::INFINITY;
    }
    let b = 64 - d.leading_zeros() as i32;
    let lp = pwr10to2(p);
    let mut e = 53 - b - lp;
    if e > 1074 {
        if e >= 1130 {
            return 0.0;
        }
        e = 1074;
    }
    let s = -(e - (64 - b) + lp + 3);
    let (mut pwr10h, mut pwr10l) = power_of_ten(p);
    if pwr10l != 0 {
        pwr10h = pwr10h.wrapping_add(1);
        pwr10l = !pwr10l;
    }
    let x = d.wrapping_shl((64 - b) as u32);
    let (mut hi, lo) = mul128(x, pwr10h);
    let mid1 = (lo >> 32) as u32;
    let mut sticky = 1u64;
    if hi & (1u64.wrapping_shl(s as u32).wrapping_sub(1)) == 0 {
        let mid2 = (mul128(x, (pwr10l as u64) << 32).0 >> 32) as u32;
        sticky = (mid1.wrapping_sub(mid2) > 1) as u64;
        hi = hi.wrapping_sub((mid1 < mid2) as u64);
    }
    let mut u = hi.wrapping_shr(s as u32) | sticky;
    let adj = (u >= (1u64 << 55) - 2) as i32;
    if adj != 0 {
        u = (u >> adj) | (u & 1);
        e -= adj;
    }
    let mut m = (u + 1 + ((u >> 2) & 1)) >> 2;
    if e <= -972 {
        return f64::INFINITY;
    }
    if m & (1u64 << 52) != 0 {
        m = (m & !(1u64 << 52)) | (((1075 - e) as u64) << 52);
    }
    f64::from_bits(m)
}

/// sqlite3FpDecode: iRound<=0 rounds to -iRound digits after the point,
/// iRound>0 to min(iRound, mx_round) significant digits.
pub fn fp_decode(r: f64, i_round: i32, mx_round: i32) -> FpDecode {
    let mut r = r;
    let sign;
    if r < 0.0 {
        sign = b'-';
        r = -r;
    } else if r == 0.0 {
        return FpDecode {
            sign: b'+',
            special: 0,
            digits: vec![b'0'],
            i_dp: 1,
        };
    } else {
        sign = b'+';
    }
    let bits = r.to_bits();
    let mut e = ((bits >> 52) & 0x7ff) as i32;
    if e == 0x7ff {
        return FpDecode {
            sign,
            special: 1 + (bits != 0x7ff0_0000_0000_0000) as u8,
            digits: Vec::new(),
            i_dp: 0,
        };
    }
    let mut v = bits & 0x000f_ffff_ffff_ffff;
    if e == 0 {
        let nn = v.leading_zeros() as i32;
        v <<= nn;
        e = -1074 - nn;
    } else {
        v = (v << 11) | (1u64 << 63);
        e -= 1086;
    }
    let want = if i_round <= 0 || i_round >= 18 {
        18
    } else {
        i_round + 1
    };
    let (v, exp) = fp2_convert10(v, e, want);

    // z holds the digits; z[0] is the first significant digit.
    let mut z: Vec<u8> = v.to_string().into_bytes();
    let mut n = z.len() as i32;
    let mut i_dp = n + exp;
    let mut i_round = i_round;
    if i_round <= 0 {
        i_round = i_dp - i_round;
        if i_round == 0 && z[0] >= b'5' {
            i_round = 1;
            z.insert(0, b'0');
            n += 1;
            i_dp += 1;
        }
    }
    let dig = |z: &[u8], i: i32| -> u8 { z.get(i as usize).copied().unwrap_or(b'0') };
    if i_round > 0 && (i_round < n || n > mx_round) {
        if i_round > mx_round {
            i_round = mx_round;
        }
        if i_round == 17 {
            // Use fewer digits when they still round-trip (49.47, not 49.469999999999999).
            if dig(&z, 15) == b'9' && dig(&z, 14) == b'9' {
                let mut jj = 14;
                while jj > 0 && dig(&z, jj - 1) == b'9' {
                    jj -= 1;
                }
                let v2 = if jj == 0 {
                    1
                } else {
                    let mut v2 = (z[0] - b'0') as u64;
                    for kk in 1..jj {
                        v2 = v2 * 10 + (dig(&z, kk) - b'0') as u64;
                    }
                    v2 + 1
                };
                if r == fp10_convert2(v2, exp + n - jj) {
                    i_round = jj + 1;
                }
            } else if i_dp >= n
                || (dig(&z, 15) == b'0' && dig(&z, 14) == b'0' && dig(&z, 13) == b'0')
            {
                let mut jj = 13;
                while dig(&z, jj - 1) == b'0' {
                    jj -= 1;
                }
                let mut v2 = (z[0] - b'0') as u64;
                for kk in 1..jj {
                    v2 = v2 * 10 + (dig(&z, kk) - b'0') as u64;
                }
                if r == fp10_convert2(v2, exp + n - jj) {
                    i_round = jj + 1;
                }
            }
        }
        n = i_round;
        if dig(&z, i_round) >= b'5' {
            let mut j = (i_round - 1) as usize;
            loop {
                z[j] += 1;
                if z[j] <= b'9' {
                    break;
                }
                z[j] = b'0';
                if j == 0 {
                    z.insert(0, b'1');
                    n += 1;
                    i_dp += 1;
                    break;
                }
                j -= 1;
            }
        }
    }
    z.truncate(n as usize);
    while z.len() > 1 && *z.last().unwrap() == b'0' {
        z.pop();
    }
    FpDecode {
        sign,
        special: 0,
        digits: z,
        i_dp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(r: f64, i_round: i32, mx: i32) -> (String, i32) {
        let d = fp_decode(r, i_round, mx);
        (String::from_utf8(d.digits).unwrap(), d.i_dp)
    }

    #[test]
    fn decode_basic() {
        assert_eq!(dec(0.1, 17, 20), ("1".into(), 0));
        assert_eq!(dec(49.47, 17, 20), ("4947".into(), 2));
        assert_eq!(dec(1.0 / 3.0, 17, 20), ("33333333333333332".into(), 0));
        assert_eq!(dec(0.1, 20, 20), ("1000000000000000056".into(), 0));
        assert_eq!(dec(2.5, 0, 16), ("3".into(), 1));
        // iRound<=0 that is below the value keeps every decoded digit.
        assert_eq!(dec(0.004, -2, 16), ("400000000000000008".into(), -2));
    }

    #[test]
    fn convert2_round_trips() {
        assert_eq!(fp10_convert2(4947, -2), 49.47);
        assert_eq!(fp10_convert2(1, 23), 1e23);
        assert_eq!(fp10_convert2(5, -324), 5e-324);
    }
}
