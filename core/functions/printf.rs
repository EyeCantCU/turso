//! SQL printf()/format(), a port of SQLite 3.54's sqlite3_str_vappendf for
//! SQL-function arguments (SQLITE_PRINTF_SQLFUNC).

use crate::functions::fpdecode::fp_decode;
use crate::numeric::{format_float, str_to_i64, Numeric};
use crate::types::Value;
use crate::vdbe::Register;

/// Cap on width/precision so hostile formats cannot exhaust memory.
/// SQLite allows more but fails with SQLITE_TOOBIG past its length limit.
const MAX_WIDTH: i64 = 1_000_000;

// ── Argument access (getIntArg / getDoubleArg / getTextArg) ─────

fn coerce_to_i64(value: &Value) -> i64 {
    match value {
        Value::Numeric(Numeric::Integer(i)) => *i,
        Value::Numeric(Numeric::Float(f)) => f64::from(*f) as i64,
        Value::Text(t) => str_to_i64(t.as_str()).unwrap_or(0),
        Value::Blob(b) => {
            let s = String::from_utf8_lossy(b);
            str_to_i64(s.as_ref()).unwrap_or(0)
        }
        Value::Null => 0,
    }
}

fn coerce_to_f64(value: &Value) -> f64 {
    match value {
        Value::Numeric(Numeric::Float(f)) => f64::from(*f),
        _ => match Option::<Numeric>::from(value) {
            Some(Numeric::Integer(i)) => i as f64,
            Some(Numeric::Float(f)) => f.into(),
            None => 0.0,
        },
    }
}

/// Bytes before the first NUL: SQLite reads text as C strings.
fn until_nul(b: &[u8]) -> &[u8] {
    b.iter().position(|&c| c == 0).map_or(b, |end| &b[..end])
}

/// sqlite3_value_text() as C string bytes; None for NULL.
fn value_text(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Null => None,
        Value::Numeric(Numeric::Integer(i)) => Some(i.to_string().into_bytes()),
        Value::Numeric(Numeric::Float(f)) => Some(format_float(f64::from(*f)).into_bytes()),
        Value::Text(t) => Some(until_nul(t.as_str().as_bytes()).to_vec()),
        Value::Blob(b) => Some(until_nul(b).to_vec()),
    }
}

struct ArgList<'a> {
    args: &'a [&'a Value],
    used: usize,
}

impl ArgList<'_> {
    fn next(&mut self) -> Option<&Value> {
        let v = self.args.get(self.used).copied();
        if v.is_some() {
            self.used += 1;
        }
        v
    }
    fn int(&mut self) -> i64 {
        self.next().map_or(0, coerce_to_i64)
    }
    fn double(&mut self) -> f64 {
        self.next().map_or(0.0, coerce_to_f64)
    }
    fn text(&mut self) -> Option<Vec<u8>> {
        self.next().and_then(value_text)
    }
}

// ── Conversion specifiers ───────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Count {
    None,
    Num(i64),
    Star,
}

/// One parsed `%...` field: flags, width, precision, length and conversion.
struct Spec {
    left_justify: bool,
    prefix: u8,
    alternate: bool,
    altform2: bool,
    zero_pad: bool,
    thousand: bool,
    long: u8,
    width: Count,
    precision: Count,
    conv: u8,
}

fn at(fmt: &[u8], i: usize) -> u8 {
    fmt.get(i).copied().unwrap_or(0)
}

/// Parse the field starting at fmt[i] (just past '%'). Returns the spec and
/// the index of the conversion character, following SQLite's flag loop.
fn parse_spec(fmt: &[u8], mut i: usize) -> (Spec, usize) {
    let mut s = Spec {
        left_justify: false,
        prefix: 0,
        alternate: false,
        altform2: false,
        zero_pad: false,
        thousand: false,
        long: 0,
        width: Count::None,
        precision: Count::None,
        conv: 0,
    };
    let mut c = at(fmt, i);
    loop {
        match c {
            b'-' | b'+' | b' ' | b'#' | b'!' | b'0' | b',' => {
                match c {
                    b'-' => s.left_justify = true,
                    b'+' | b' ' => s.prefix = c,
                    b'#' => s.alternate = true,
                    b'!' => s.altform2 = true,
                    b'0' => s.zero_pad = true,
                    _ => s.thousand = true,
                }
                i += 1;
                c = at(fmt, i);
                if c == 0 {
                    break;
                }
            }
            b'l' => {
                s.long = 1;
                i += 1;
                c = at(fmt, i);
                if c == b'l' {
                    i += 1;
                    c = at(fmt, i);
                    s.long = 2;
                }
                break;
            }
            b'1'..=b'9' => {
                let mut wx = (c - b'0') as u32;
                loop {
                    i += 1;
                    c = at(fmt, i);
                    if !c.is_ascii_digit() {
                        break;
                    }
                    wx = wx.wrapping_mul(10).wrapping_add((c - b'0') as u32);
                }
                s.width = Count::Num((wx & 0x7fff_ffff) as i64);
                if c != b'.' && c != b'l' {
                    break;
                }
            }
            b'*' => {
                s.width = Count::Star;
                let n = at(fmt, i + 1);
                i += 1;
                c = at(fmt, i);
                if n != b'.' && n != b'l' {
                    break;
                }
            }
            b'.' => {
                i += 1;
                c = at(fmt, i);
                if c == b'*' {
                    s.precision = Count::Star;
                    i += 1;
                    c = at(fmt, i);
                } else {
                    let mut px: u32 = 0;
                    while c.is_ascii_digit() {
                        px = px.wrapping_mul(10).wrapping_add((c - b'0') as u32);
                        i += 1;
                        c = at(fmt, i);
                    }
                    s.precision = Count::Num((px & 0x7fff_ffff) as i64);
                }
                if c != b'l' {
                    break;
                }
            }
            _ => break,
        }
    }
    s.conv = c;
    (s, i)
}

const DIGITS_UPPER: &[u8] = b"0123456789ABCDEF";
const DIGITS_LOWER: &[u8] = b"0123456789abcdef";

/// Append `n` copies of `c`.
fn pad(out: &mut Vec<u8>, n: i64, c: u8) {
    if n > 0 {
        out.resize(out.len() + n as usize, c);
    }
}

/// With '!', width counts characters: add one per UTF-8 continuation byte.
fn utf8_width(width: i64, body: &[u8], altform2: bool) -> i64 {
    if altform2 && width > 0 {
        width + body.iter().filter(|&&b| b & 0xc0 == 0x80).count() as i64
    } else {
        width
    }
}

/// Append `body` padded with spaces to `width` bytes.
fn emit(out: &mut Vec<u8>, body: &[u8], width: i64, left_justify: bool) {
    let w = width - body.len() as i64;
    if w > 0 && !left_justify {
        pad(out, w, b' ');
    }
    out.extend_from_slice(body);
    if w > 0 && left_justify {
        pad(out, w, b' ');
    }
}

fn render_integer(s: &Spec, args: &mut ArgList, mut precision: i64, width: i64) -> Vec<u8> {
    let conv = s.conv;
    let signed = matches!(conv, b'd' | b'i' | b'r');
    let (base, charset): (u64, &[u8]) = match conv {
        b'o' => (8, DIGITS_UPPER),
        b'x' => (16, DIGITS_LOWER),
        b'X' | b'p' => (16, DIGITS_UPPER),
        _ => (10, DIGITS_UPPER),
    };
    let thousand = s.thousand && matches!(conv, b'd' | b'i' | b'u');
    let mut alternate = s.alternate;
    let (mut value, prefix) = if signed {
        let v = args.int();
        if v < 0 {
            (v.unsigned_abs(), b'-')
        } else {
            (v as u64, s.prefix)
        }
    } else {
        (args.int() as u64, 0)
    };
    if value == 0 {
        alternate = false;
    }
    let has_prefix = (prefix != 0) as i64;
    if s.zero_pad && precision < width - has_prefix {
        precision = width - has_prefix;
    }
    // Built least significant first, then reversed.
    let mut rev: Vec<u8> = Vec::new();
    if conv == b'r' {
        let mut x = (value % 10) as usize;
        if x >= 4 || (value / 10) % 10 == 1 {
            x = 0;
        }
        let ord = b"thstndrd";
        rev.push(ord[x * 2 + 1]);
        rev.push(ord[x * 2]);
    }
    loop {
        rev.push(charset[(value % base) as usize]);
        value /= base;
        if value == 0 {
            break;
        }
    }
    let precision = precision.min(MAX_WIDTH);
    while (rev.len() as i64) < precision {
        rev.push(b'0');
    }
    let mut body: Vec<u8> = rev.into_iter().rev().collect();
    if thousand {
        let len = body.len();
        let mut grouped = Vec::with_capacity(len + len / 3);
        for (k, &d) in body.iter().enumerate() {
            if k > 0 && (len - k) % 3 == 0 {
                grouped.push(b',');
            }
            grouped.push(d);
        }
        body = grouped;
    }
    let mut head: Vec<u8> = Vec::new();
    if alternate {
        match conv {
            b'o' => head.push(b'0'),
            b'x' | b'p' => head.extend_from_slice(b"0x"),
            b'X' => head.extend_from_slice(b"0X"),
            _ => {}
        }
    }
    if prefix != 0 {
        head.push(prefix);
    }
    head.extend_from_slice(&body);
    head
}

/// %f %e %E %g %G. Returns the fully padded field.
fn render_float(s: &Spec, args: &mut ArgList, precision: i64, width: i64) -> Vec<u8> {
    let value = args.double();
    let mut precision = if precision < 0 { 6 } else { precision }.min(MAX_WIDTH);
    let upper = matches!(s.conv, b'E' | b'G');
    let mut xtype = match s.conv {
        b'f' => b'f',
        b'e' | b'E' => b'e',
        _ => b'g',
    };
    let i_round = match xtype {
        b'f' => -(precision as i32),
        b'g' => {
            if precision == 0 {
                precision = 1;
            }
            precision as i32
        }
        _ => precision as i32 + 1,
    };
    let mut d = fp_decode(value, i_round, if s.altform2 { 20 } else { 16 });
    if d.special != 0 {
        let body: Vec<u8> = if d.special == 2 {
            if s.zero_pad {
                b"null".to_vec()
            } else {
                b"NaN".to_vec()
            }
        } else if s.zero_pad {
            d.digits = vec![b'9'];
            d.i_dp = 1000;
            Vec::new()
        } else if d.sign == b'-' {
            b"-Inf".to_vec()
        } else if s.prefix != 0 {
            vec![s.prefix, b'I', b'n', b'f']
        } else {
            b"Inf".to_vec()
        };
        if !body.is_empty() {
            let mut out = Vec::new();
            emit(&mut out, &body, width, s.left_justify);
            return out;
        }
    }
    let prefix = if d.sign == b'-' {
        if s.alternate && s.prefix == 0 && xtype == b'f' && d.i_dp <= i_round {
            0
        } else {
            b'-'
        }
    } else {
        s.prefix
    };
    let exp = d.i_dp - 1;
    let flag_rtz;
    if xtype == b'g' {
        precision -= 1;
        flag_rtz = !s.alternate;
        if exp < -4 || exp as i64 > precision {
            xtype = b'e';
        } else {
            precision -= exp as i64;
            xtype = b'f';
        }
    } else {
        flag_rtz = s.altform2;
    }
    let mut e2: i64 = if xtype == b'e' {
        0
    } else {
        (d.i_dp - 1) as i64
    };
    let digits = &d.digits;
    let n = digits.len();
    let flag_dp = precision > 0 || s.alternate || s.altform2;

    let mut z: Vec<u8> = Vec::new();
    if prefix != 0 {
        z.push(prefix);
    }
    let mut j = 0usize;
    if e2 < 0 {
        z.push(b'0');
    } else if s.thousand {
        while e2 >= 0 {
            z.push(if j < n {
                j += 1;
                digits[j - 1]
            } else {
                b'0'
            });
            if e2 % 3 == 0 && e2 > 1 {
                z.push(b',');
            }
            e2 -= 1;
        }
    } else {
        j = ((e2 + 1) as usize).min(n);
        z.extend_from_slice(&digits[..j]);
        e2 -= j as i64;
        if e2 >= 0 {
            pad(&mut z, e2 + 1, b'0');
            e2 = -1;
        }
    }
    if flag_dp {
        z.push(b'.');
    }
    if e2 < -1 && precision > 0 {
        let nn = (-1 - e2).min(precision);
        pad(&mut z, nn, b'0');
        precision -= nn;
    }
    if precision > 0 {
        let nn = ((n - j) as i64).min(precision);
        if nn > 0 {
            z.extend_from_slice(&digits[j..j + nn as usize]);
            precision -= nn;
        }
        if precision > 0 && !flag_rtz {
            pad(&mut z, precision, b'0');
        }
    }
    if flag_rtz && flag_dp {
        while z.last() == Some(&b'0') {
            z.pop();
        }
        if z.last() == Some(&b'.') {
            if s.altform2 {
                z.push(b'0');
            } else {
                z.pop();
            }
        }
    }
    if xtype == b'e' {
        let mut exp = d.i_dp - 1;
        z.push(if upper { b'E' } else { b'e' });
        if exp < 0 {
            z.push(b'-');
            exp = -exp;
        } else {
            z.push(b'+');
        }
        if exp >= 100 {
            z.push(b'0' + (exp / 100) as u8);
            exp %= 100;
        }
        z.push(b'0' + (exp / 10) as u8);
        z.push(b'0' + (exp % 10) as u8);
    }
    let length = z.len() as i64;
    if length < width {
        let n_pad = (width - length) as usize;
        if s.left_justify {
            pad(&mut z, n_pad as i64, b' ');
        } else if !s.zero_pad {
            z.splice(0..0, std::iter::repeat_n(b' ', n_pad));
        } else {
            let adj = (prefix != 0) as usize;
            z.splice(adj..adj, std::iter::repeat_n(b'0', n_pad));
        }
    }
    z
}

/// %c: first character of the argument's text, repeated `precision` times.
fn render_char(out: &mut Vec<u8>, s: &Spec, args: &mut ArgList, precision: i64, width: i64) {
    let mut buf: Vec<u8> = Vec::with_capacity(4);
    match args.text() {
        Some(t) => {
            let c = at(&t, 0);
            buf.push(c);
            if c & 0xc0 == 0xc0 {
                let mut k = 1;
                while buf.len() < 4 && at(&t, k) & 0xc0 == 0x80 {
                    buf.push(t[k]);
                    k += 1;
                }
            }
        }
        None => buf.push(0),
    }
    let mut width = width;
    if precision > 1 {
        let precision = precision.min(MAX_WIDTH);
        width -= precision - 1;
        if width > 1 && !s.left_justify {
            pad(out, width - 1, b' ');
            width = 0;
        }
        for _ in 0..precision - 1 {
            out.extend_from_slice(&buf);
        }
    }
    let width = utf8_width(width, &buf, true);
    emit(out, &buf, width, s.left_justify);
}

/// %s and %z.
fn render_string(out: &mut Vec<u8>, s: &Spec, args: &mut ArgList, precision: i64, width: i64) {
    let t = args.text().unwrap_or_default();
    let length = if precision >= 0 {
        if s.altform2 {
            let mut k = 0usize;
            let mut p = precision;
            while p > 0 && k < t.len() {
                p -= 1;
                let lead = t[k];
                k += 1;
                if lead >= 0xc0 {
                    while k < t.len() && t[k] & 0xc0 == 0x80 {
                        k += 1;
                    }
                }
            }
            k
        } else {
            (precision as usize).min(t.len())
        }
    } else {
        t.len()
    };
    let body = &t[..length];
    let width = utf8_width(width, body, s.altform2);
    emit(out, body, width, s.left_justify);
}

/// %j and %J: JSON string literal, without or with the quotes.
fn render_json(out: &mut Vec<u8>, s: &Spec, args: &mut ArgList, precision: i64, width: i64) {
    let quoted = s.conv == b'J';
    let start = out.len();
    match args.text() {
        None => {
            if quoted {
                out.extend_from_slice(b"null");
            }
        }
        Some(t) => {
            if quoted {
                out.push(b'"');
            }
            let mut px = if precision < 0 { i64::MAX } else { precision };
            if precision >= 0 && s.altform2 {
                // Precision counts code points: convert it to bytes.
                let mut i = 0i64;
                while i < px && at(&t, i as usize) != 0 {
                    if at(&t, i as usize) & 0xc0 == 0x80 {
                        px += 1;
                    }
                    i += 1;
                }
                if i == px {
                    while at(&t, px as usize) & 0xc0 == 0x80 {
                        px += 1;
                    }
                }
            }
            let end = (px.min(t.len() as i64)) as usize;
            for &ch in &t[..end] {
                if ch <= 0x1f || ch == b'"' || ch == b'\\' {
                    out.push(b'\\');
                    if ch > 0x1f {
                        out.push(ch);
                    } else if (1u32 << ch) & 0x3700 != 0 {
                        out.push(b"btn?fr"[(ch - 8) as usize]);
                    } else {
                        out.extend_from_slice(b"u00");
                        out.push(DIGITS_LOWER[(ch >> 4) as usize]);
                        out.push(DIGITS_LOWER[(ch & 0xf) as usize]);
                    }
                } else {
                    out.push(ch);
                }
            }
            if quoted {
                out.push(b'"');
            }
        }
    }
    if width > 0 {
        let n = (out.len() - start) as i64;
        let mut len = n;
        if s.altform2 && n > 0 {
            len -= out[start..].iter().filter(|&&b| b & 0xc0 == 0x80).count() as i64;
        }
        if width > len {
            let sp = (width - len) as usize;
            if !s.left_justify && n > 0 {
                out.splice(start..start, std::iter::repeat_n(b' ', sp));
            } else {
                pad(out, sp as i64, b' ');
            }
        }
    }
}

/// %q, %Q and %w.
fn render_quoted(out: &mut Vec<u8>, s: &Spec, args: &mut ArgList, precision: i64, width: i64) {
    let conv = s.conv;
    let mut need_quote = 0;
    let esc: Vec<u8> = match args.text() {
        None => {
            if conv == b'Q' {
                b"NULL".to_vec()
            } else {
                b"(NULL)".to_vec()
            }
        }
        Some(t) => {
            if conv == b'Q' {
                need_quote = 1;
            }
            t
        }
    };
    let mut alternate = s.alternate;
    let q = if conv == b'w' {
        alternate = false;
        b'"'
    } else {
        b'\''
    };
    // Bytes of input to use: `precision` bytes, or characters with '!'.
    let mut i = 0usize;
    let mut k = precision;
    while k != 0 && i < esc.len() {
        if s.altform2 && esc[i] & 0xc0 == 0xc0 {
            while i + 1 < esc.len() && esc[i + 1] & 0xc0 == 0x80 {
                i += 1;
            }
        }
        i += 1;
        k -= 1;
    }
    let src = &esc[..i];
    if alternate {
        let n_ctrl = src.iter().filter(|&&b| b <= 0x1f).count();
        if n_ctrl > 0 || conv == b'q' {
            if conv == b'Q' {
                need_quote = 2;
            }
        } else {
            alternate = false;
        }
    }
    let mut body: Vec<u8> = Vec::with_capacity(src.len() + 3);
    match need_quote {
        2 => body.extend_from_slice(b"unistr('"),
        1 => body.push(b'\''),
        _ => {}
    }
    for &ch in src {
        if alternate && ch <= 0x1f {
            body.extend_from_slice(b"\\u00");
            body.push(if ch >= 0x10 { b'1' } else { b'0' });
            body.push(DIGITS_LOWER[(ch & 0xf) as usize]);
            continue;
        }
        body.push(ch);
        if ch == q || (alternate && ch == b'\\') {
            body.push(ch);
        }
    }
    if need_quote != 0 {
        body.push(b'\'');
        if need_quote == 2 {
            body.push(b')');
        }
    }
    let width = utf8_width(width, &body, s.altform2);
    emit(out, &body, width, s.left_justify);
}

/// sqlite3_str_vappendf with SQL arguments. Stops at an invalid conversion.
fn vappendf(fmt: &[u8], args: &mut ArgList, out: &mut Vec<u8>) {
    let mut i = 0usize;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            let end = fmt[i..]
                .iter()
                .position(|&b| b == b'%')
                .map_or(fmt.len(), |p| i + p);
            out.extend_from_slice(&fmt[i..end]);
            i = end;
            if i >= fmt.len() {
                break;
            }
        }
        // fmt[i] == '%'
        if at(fmt, i + 1) == 0 {
            out.push(b'%');
            break;
        }
        let (s, ci) = parse_spec(fmt, i + 1);
        i = ci + 1;
        let mut left_justify = s.left_justify;
        let mut width = match s.width {
            Count::None => 0,
            Count::Num(w) => w,
            Count::Star => {
                let w = args.int() as i32 as i64;
                if w < 0 {
                    left_justify = true;
                    if w >= -2147483647 {
                        -w
                    } else {
                        0
                    }
                } else {
                    w
                }
            }
        };
        width = width.min(MAX_WIDTH);
        let precision = match s.precision {
            Count::None => -1,
            Count::Num(p) => p,
            Count::Star => {
                let p = args.int() as i32 as i64;
                if p < 0 {
                    if p >= -2147483647 {
                        -p
                    } else {
                        -1
                    }
                } else {
                    p
                }
            }
        }
        .min(MAX_WIDTH);
        let s = Spec { left_justify, ..s };
        match s.conv {
            b'd' | b'i' | b'u' | b'r' | b'x' | b'X' | b'o' | b'p' => {
                let body = render_integer(&s, args, precision, width);
                emit(out, &body, width, s.left_justify);
            }
            b'f' | b'e' | b'E' | b'g' | b'G' => {
                let body = render_float(&s, args, precision, width);
                out.extend_from_slice(&body);
            }
            b'n' => {}
            b'%' => emit(out, b"%", width, s.left_justify),
            b'c' => render_char(out, &s, args, precision, width),
            b's' | b'z' => render_string(out, &s, args, precision, width),
            b'j' | b'J' => render_json(out, &s, args, precision, width),
            b'q' | b'Q' | b'w' => render_quoted(out, &s, args, precision, width),
            // %T, %S (internal-only) and unknown conversions end the output.
            _ => return,
        }
    }
}

// ── Entry points ────────────────────────────────────────────────

pub fn exec_printf(values: &[Register]) -> crate::Result<Value> {
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let args: Vec<&Value> = values[1..].iter().map(|r| r.get_value()).collect();
    exec_printf_values(values[0].get_value(), &args)
}

/// The C type a sqlite3_mprintf-style caller passes for one argument slot,
/// in order. Derived from the same specifier parser the engine formats
/// with, so the C API's va_list walk cannot drift from the formatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintfCArg {
    Int32,
    /// u/o/x/X without a length modifier: an unsigned int, zero-extended.
    Uint32,
    Int64,
    Double,
    Text,
    /// %z: text whose pointer the caller yields; freed after use.
    OwnedText,
    /// %c: a character code in the C API, rendered as that character.
    CharCode,
    /// %n: an int* the C API consumes; no value is passed to the engine.
    IntPtr,
}

/// The sequence of C argument types `format` consumes. Stops at an invalid
/// conversion, where the engine stops formatting too.
pub fn printf_c_arg_plan(format: &str) -> Vec<PrintfCArg> {
    let fmt = until_nul(format.as_bytes());
    let mut plan = Vec::new();
    let mut i = 0usize;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            i += 1;
            continue;
        }
        if at(fmt, i + 1) == 0 {
            break;
        }
        let (s, ci) = parse_spec(fmt, i + 1);
        i = ci + 1;
        if s.width == Count::Star {
            plan.push(PrintfCArg::Int32);
        }
        if s.precision == Count::Star {
            plan.push(PrintfCArg::Int32);
        }
        let long = s.long >= 1;
        match s.conv {
            b'd' | b'i' | b'r' => plan.push(if long {
                PrintfCArg::Int64
            } else {
                PrintfCArg::Int32
            }),
            b'u' | b'o' | b'x' | b'X' => plan.push(if long {
                PrintfCArg::Int64
            } else {
                PrintfCArg::Uint32
            }),
            b'p' => plan.push(PrintfCArg::Int64),
            b'f' | b'e' | b'E' | b'g' | b'G' => plan.push(PrintfCArg::Double),
            b's' | b'q' | b'Q' | b'w' | b'j' | b'J' => plan.push(PrintfCArg::Text),
            b'z' => plan.push(PrintfCArg::OwnedText),
            b'c' => plan.push(PrintfCArg::CharCode),
            b'n' => plan.push(PrintfCArg::IntPtr),
            b'%' => {}
            _ => break,
        }
    }
    plan
}

/// Value-level entry point, shared by SQL printf()/format() and the C API's
/// sqlite3_mprintf/sqlite3_snprintf bridge.
pub fn exec_printf_values(format: &Value, args: &[&Value]) -> crate::Result<Value> {
    let Some(fmt) = value_text(format) else {
        return Ok(Value::Null);
    };
    let mut list = ArgList { args, used: 0 };
    let mut out = Vec::new();
    vappendf(&fmt, &mut list, &mut out);
    let text = match String::from_utf8(out) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    Ok(Value::build_text(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Register {
        Register::Value(Value::build_text(value.to_string()))
    }

    fn integer(value: i64) -> Register {
        Register::Value(Value::from_i64(value))
    }

    fn float(value: f64) -> Register {
        Register::Value(Value::from_f64(value))
    }

    #[test]
    fn test_printf_no_args() {
        assert_eq!(exec_printf(&[]).unwrap(), Value::Null);
    }

    #[test]
    fn test_printf_basic_string() {
        assert_eq!(
            exec_printf(&[text("Hello World")]).unwrap(),
            *text("Hello World").get_value()
        );
    }

    #[test]
    fn test_printf_string_formatting() {
        let test_cases = vec![
            (
                vec![text("Hello, %s!"), text("World")],
                text("Hello, World!"),
            ),
            (
                vec![text("%s %s!"), text("Hello"), text("World")],
                text("Hello World!"),
            ),
            (
                vec![text("Hello, %s!"), Register::Value(Value::Null)],
                text("Hello, !"),
            ),
            (vec![text("Value: %s"), integer(42)], text("Value: 42")),
            (vec![text("100%% complete")], text("100% complete")),
        ];
        for (input, output) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *output.get_value());
        }
    }

    #[test]
    fn test_printf_integer_formatting() {
        let test_cases = vec![
            (vec![text("Number: %d"), integer(42)], text("Number: 42")),
            (vec![text("Number: %d"), integer(-42)], text("Number: -42")),
            (
                vec![text("%d + %d = %d"), integer(2), integer(3), integer(5)],
                text("2 + 3 = 5"),
            ),
            (
                vec![text("Number: %d"), text("not a number")],
                text("Number: 0"),
            ),
            (
                vec![text("Truncated float: %d"), float(3.9)],
                text("Truncated float: 3"),
            ),
            (vec![text("Number: %i"), integer(42)], text("Number: 42")),
        ];
        for (input, output) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *output.get_value());
        }
    }

    #[test]
    fn test_printf_unsigned_integer_formatting() {
        let test_cases = vec![
            (vec![text("Number: %u"), integer(42)], text("Number: 42")),
            (
                vec![text("Negative: %u"), integer(-1)],
                text("Negative: 18446744073709551615"),
            ),
            (vec![text("NaN: %u"), text("not a number")], text("NaN: 0")),
        ];
        for (input, output) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *output.get_value());
        }
    }

    #[test]
    fn test_printf_float_formatting() {
        let test_cases = vec![
            (
                vec![text("Number: %f"), float(42.5)],
                text("Number: 42.500000"),
            ),
            (
                vec![text("Number: %f"), float(-42.5)],
                text("Number: -42.500000"),
            ),
            (
                vec![text("Number: %f"), integer(42)],
                text("Number: 42.000000"),
            ),
            (
                vec![text("Number: %f"), text("not a number")],
                text("Number: 0.000000"),
            ),
        ];

        // Huge finite float must not overflow rounding to produce "inf"
        let huge = exec_printf(&[text("%f"), float(1e308)]).unwrap();
        let huge_str = match &huge {
            Value::Text(t) => t.as_str().to_string(),
            _ => panic!("expected text"),
        };
        assert!(huge_str.starts_with("1000000000000000"));
        assert!(huge_str.ends_with(".000000"));
        assert!(!huge_str.contains("inf"));
        for (input, expected) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *expected.get_value());
        }
    }

    #[test]
    fn test_printf_width_precision() {
        let test_cases = vec![
            (vec![text("%.2f"), float(4.002)], text("4.00")),
            (vec![text("%05d"), integer(42)], text("00042")),
            (vec![text("%.5d"), integer(42)], text("00042")),
            (vec![text("%+d"), integer(42)], text("+42")),
            (vec![text("%.3s"), text("hello")], text("hel")),
            (vec![text("%08x"), integer(255)], text("000000ff")),
            (vec![text("%#x"), integer(255)], text("0xff")),
        ];
        for (input, expected) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *expected.get_value());
        }
    }

    #[test]
    fn test_printf_dynamic_width() {
        assert_eq!(
            exec_printf(&[text("%.*f"), integer(2), float(3.14258)]).unwrap(),
            *text("3.14").get_value()
        );
    }

    #[test]
    fn test_printf_character_formatting() {
        let test_cases = vec![
            (vec![text("character: %c"), text("a")], text("character: a")),
            (
                vec![text("character: %c"), text("this is a test")],
                text("character: t"),
            ),
            (
                vec![text("character: %c"), integer(123)],
                text("character: 1"),
            ),
            (
                vec![text("character: %c"), float(42.5)],
                text("character: 4"),
            ),
            // Empty string → NUL char → no output (matches SQLite)
            // Empty or NULL text emits a NUL byte, as SQLite does.
            (vec![text("character: %c"), text("")], text("character: \0")),
            (
                vec![text("character: %c"), Register::Value(Value::Null)],
                text("character: \0"),
            ),
        ];
        for (input, expected) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *expected.get_value());
        }
    }

    #[test]
    fn test_printf_exponential_formatting() {
        let test_cases = vec![
            (
                vec![text("Exp: %e"), float(23000000.0)],
                text("Exp: 2.300000e+07"),
            ),
            (
                vec![text("Exp: %e"), float(-23000000.0)],
                text("Exp: -2.300000e+07"),
            ),
            (vec![text("Exp: %e"), float(0.0)], text("Exp: 0.000000e+00")),
        ];
        for (input, expected) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *expected.get_value());
        }
    }

    #[test]
    fn test_printf_general_formatting() {
        let test_cases = vec![
            (vec![text("%g"), float(100.0)], text("100")),
            (vec![text("%g"), float(0.00123)], text("0.00123")),
            (vec![text("%g"), float(1.0)], text("1")),
            (vec![text("%g"), float(1.5)], text("1.5")),
            (vec![text("%g"), float(0.0)], text("0")),
            (vec![text("%g"), integer(42)], text("42")),
            // Comma separator applies to %G decimal notation
            (vec![text("%,G"), integer(1000)], text("1,000")),
            (
                vec![text("%,.20G"), float(1234567.89)],
                text("1,234,567.89"),
            ),
        ];
        for (input, expected) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *expected.get_value());
        }
    }

    #[test]
    fn test_printf_sql_quoting() {
        assert_eq!(
            exec_printf(&[text("%q"), text("it's")]).unwrap(),
            *text("it''s").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%Q"), text("it's")]).unwrap(),
            *text("'it''s'").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%Q"), Register::Value(Value::Null)]).unwrap(),
            *text("NULL").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%q"), Register::Value(Value::Null)]).unwrap(),
            *text("(NULL)").get_value()
        );
    }

    #[test]
    fn test_printf_comma_separator() {
        assert_eq!(
            exec_printf(&[text("%,d"), integer(1234567)]).unwrap(),
            *text("1,234,567").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%,d"), integer(-1234567)]).unwrap(),
            *text("-1,234,567").get_value()
        );
    }

    #[test]
    fn test_printf_edge_cases() {
        let test_cases = vec![
            (vec![text("%%%%")], text("%%")),
            (vec![text("No substitutions")], text("No substitutions")),
            (
                vec![text("%d%d%d"), integer(1), integer(2), integer(3)],
                text("123"),
            ),
            // Trailing % is preserved
            (vec![text("test%")], text("test%")),
            // Unknown specifier: NULL if nothing processed before, else accumulated text
            (vec![text("%d%j"), integer(42)], text("42")),
            (vec![text("hello%j")], text("hello")),
            // Negative zero should not show minus sign
            (vec![text("%f"), float(-0.0)], text("0.000000")),
        ];
        for (input, expected) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *expected.get_value());
        }
        // SQLite 3.54 returns '' (not NULL) for empty output.
        assert_eq!(exec_printf(&[text("")]).unwrap(), *text("").get_value());
        assert_eq!(
            exec_printf(&[text("%s"), text("")]).unwrap(),
            *text("").get_value()
        );
    }

    #[test]
    fn test_printf_hexadecimal_formatting() {
        let test_cases = vec![
            (vec![text("hex: %x"), integer(4)], text("hex: 4")),
            (
                vec![text("hex: %X"), integer(15565303546)],
                text("hex: 39FC3AEFA"),
            ),
            (
                vec![text("hex: %x"), integer(-15565303546)],
                text("hex: fffffffc603c5106"),
            ),
            (vec![text("hex: %x"), float(42.5)], text("hex: 2a")),
            (vec![text("hex: %x"), text("42")], text("hex: 2a")),
            (vec![text("hex: %x"), text("")], text("hex: 0")),
        ];
        for (input, expected) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *expected.get_value());
        }
    }

    #[test]
    fn test_printf_octal_formatting() {
        let test_cases = vec![
            (vec![text("octal: %o"), integer(4)], text("octal: 4")),
            (vec![text("octal: %o"), float(42.5)], text("octal: 52")),
            (vec![text("octal: %o"), text("42")], text("octal: 52")),
            // # flag always adds "0" prefix when value is non-zero
            (vec![text("%#o"), integer(8)], text("010")),
            (vec![text("%#o"), integer(0)], text("0")),
            // # flag with precision: "0" prefix added even if precision pads with zeros
            (vec![text("%#.5o"), integer(8)], text("000010")),
            (
                vec![text("%#.20o"), integer(1000)],
                text("000000000000000001750"),
            ),
        ];
        for (input, expected) in test_cases {
            assert_eq!(exec_printf(&input).unwrap(), *expected.get_value());
        }
    }

    // ── Bug fix regression tests ────────────────────────────────────

    #[test]
    fn test_rounding_half_away_from_zero() {
        // Bug 1: SQLite uses half-away-from-zero, not half-to-even
        assert_eq!(
            exec_printf(&[text("%.0f"), float(0.5)]).unwrap(),
            *text("1").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%.0f"), float(2.5)]).unwrap(),
            *text("3").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%.0f"), float(-0.5)]).unwrap(),
            *text("-1").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%.0e"), float(2.5)]).unwrap(),
            *text("3e+00").get_value()
        );
    }

    #[test]
    fn test_alt_hex_zero_pad_width() {
        // Bug 2: # flag with 0 flag - prefix not counted in width
        assert_eq!(
            exec_printf(&[text("%#08x"), integer(255)]).unwrap(),
            *text("0x000000ff").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%#04x"), integer(255)]).unwrap(),
            *text("0x00ff").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%#08o"), integer(8)]).unwrap(),
            *text("000000010").get_value()
        );
    }

    #[test]
    fn test_alt_flag_forces_decimal_point() {
        // Bug 3: # flag forces decimal point on %e and %g
        assert_eq!(
            exec_printf(&[text("%#.0e"), float(1.0)]).unwrap(),
            *text("1.e+00").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%#.0g"), float(1.0)]).unwrap(),
            *text("1.").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%#g"), float(100000.0)]).unwrap(),
            *text("100000.").get_value()
        );
    }

    #[test]
    fn test_g_threshold_rounding() {
        // Bug 4: %g pre-rounding changes the exponent threshold
        assert_eq!(
            exec_printf(&[text("%g"), float(999999.5)]).unwrap(),
            *text("1e+06").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%.1g"), float(9.5)]).unwrap(),
            *text("1e+01").get_value()
        );
    }

    #[test]
    fn test_zero_pad_ignored_for_strings() {
        // Bug 5: 0 flag should be ignored for %s and %c
        assert_eq!(
            exec_printf(&[text("%05s"), text("hi")]).unwrap(),
            *text("   hi").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%05c"), text("A")]).unwrap(),
            *text("    A").get_value()
        );
    }

    #[test]
    fn test_q_width_precision() {
        // Bug 6: %q/%Q/%w should respect width and precision
        assert_eq!(
            exec_printf(&[text("%.2q"), text("hello")]).unwrap(),
            *text("he").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%10q"), text("hi")]).unwrap(),
            *text("        hi").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%10Q"), text("hi")]).unwrap(),
            *text("      'hi'").get_value()
        );
    }

    #[test]
    fn test_infinity_handling() {
        // SQLite source (sqlite3.c:32502): infinity + flag_zeropad → 9-fill
        // infinity without flag_zeropad → "Inf"
        let inf_f = exec_printf(&[text("%020f"), float(f64::INFINITY)]).unwrap();
        let inf_str = match &inf_f {
            Value::Text(t) => t.as_str().to_string(),
            _ => panic!("expected text"),
        };
        assert!(inf_str.starts_with("9000"));
        assert_eq!(inf_str.len(), 1007); // 9 + 999 zeros + ".000000"

        assert_eq!(
            exec_printf(&[text("%020e"), float(f64::INFINITY)]).unwrap(),
            *text("00000009.000000e+999").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%020g"), float(f64::INFINITY)]).unwrap(),
            *text("000000000000009e+999").get_value()
        );
        // Without zero-pad → "Inf" (not 9-fill)
        assert_eq!(
            exec_printf(&[text("%e"), float(f64::INFINITY)]).unwrap(),
            *text("Inf").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%G"), float(f64::INFINITY)]).unwrap(),
            *text("Inf").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%f"), float(f64::INFINITY)]).unwrap(),
            *text("Inf").get_value()
        );
        // With zero-pad but no width still triggers 9-fill
        assert_eq!(
            exec_printf(&[text("%0G"), float(f64::INFINITY)]).unwrap(),
            *text("9E+999").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%0,G"), float(f64::INFINITY)]).unwrap(),
            *text("9E+999").get_value()
        );
        // Negative infinity
        assert_eq!(
            exec_printf(&[text("%e"), float(f64::NEG_INFINITY)]).unwrap(),
            *text("-Inf").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%020e"), float(f64::NEG_INFINITY)]).unwrap(),
            *text("-0000009.000000e+999").get_value()
        );
        // # flag with %g infinity: RTZ disabled, so trailing zeros remain
        assert_eq!(
            exec_printf(&[text("%#0g"), float(f64::INFINITY)]).unwrap(),
            *text("9.00000e+999").get_value()
        );
        // ! flag with %e infinity: RTZ enabled, strips to .0
        assert_eq!(
            exec_printf(&[text("%!0e"), float(f64::INFINITY)]).unwrap(),
            *text("9.0e+999").get_value()
        );
        // ! flag with %f infinity: strips trailing fractional zeros
        let inf_bang_f = exec_printf(&[text("%!0f"), float(f64::INFINITY)]).unwrap();
        let inf_bang_str = match &inf_bang_f {
            Value::Text(t) => t.as_str().to_string(),
            _ => panic!("expected text"),
        };
        assert!(
            inf_bang_str.ends_with(".0"),
            "Infinity with %!0f should end with .0, got: ...{}",
            &inf_bang_str[inf_bang_str.len().saturating_sub(10)..]
        );
    }

    #[test]
    fn test_significant_digits_limiting() {
        // Default: 16 significant digits (hide IEEE noise)
        assert_eq!(
            exec_printf(&[text("%.20f"), float(1.0 / 3.0)]).unwrap(),
            *text("0.33333333333333330000").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%.20e"), float(1.0 / 3.0)]).unwrap(),
            *text("3.33333333333333300000e-01").get_value()
        );
        // ! flag: at most 18 decoded digits (SQLite 3.54), trailing zeros stripped.
        assert_eq!(
            exec_printf(&[text("%!.20f"), float(1.0 / 3.0)]).unwrap(),
            *text("0.333333333333333315").get_value()
        );
    }

    #[test]
    fn test_nan_handling() {
        // Value::from_f64(NaN) returns Value::Null (NonNan rejects NaN),
        // so NaN is treated as NULL which coerces to 0.0 for float formats.
        // The NaN-specific formatting code (NaN/null output) is defense-in-depth
        // that can't be triggered through the Value system.
        assert_eq!(
            exec_printf(&[text("%f"), float(f64::NAN)]).unwrap(),
            *text("0.000000").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%e"), float(f64::NAN)]).unwrap(),
            *text("0.000000e+00").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%g"), float(f64::NAN)]).unwrap(),
            *text("0").get_value()
        );
    }

    #[test]
    fn test_blob_nul_truncation() {
        // Bug 9: %s on blobs truncates at first NUL byte
        let blob_val = Register::Value(
            Value::from_slice(&[0x48, 0x00, 0x4C]).expect(crate::alloc::ALLOC_ERR_MSG),
        ); // H\0L
        let result = exec_printf(&[text("%s"), blob_val]).unwrap();
        assert_eq!(result, *text("H").get_value());

        let blob_hello =
            Register::Value(Value::from_slice(b"Hello").expect(crate::alloc::ALLOC_ERR_MSG));
        assert_eq!(
            exec_printf(&[text("%s"), blob_hello]).unwrap(),
            *text("Hello").get_value()
        );
    }

    #[test]
    fn test_i32_star_precision_wrapping() {
        // i32::MIN as * precision wraps back to itself after negation → treated as 0
        assert_eq!(
            exec_printf(&[text("%.*d"), integer(-2147483648), integer(42)]).unwrap(),
            *text("42").get_value()
        );
        // 4294967295 as i64 → -1 as i32 → wrapping_neg → 1
        assert_eq!(
            exec_printf(&[text("%.*d"), integer(4294967295), integer(42)]).unwrap(),
            *text("42").get_value()
        );
    }

    #[test]
    fn test_comma_zero_pad_interaction() {
        // When comma + zero_pad: zero-pad digits to width, then insert commas
        // Width 15 = 15 digit positions, commas added on top
        assert_eq!(
            exec_printf(&[text("%0,15d"), integer(42)]).unwrap(),
            *text("000,000,000,000,042").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%0,15u"), integer(42)]).unwrap(),
            *text("000,000,000,000,042").get_value()
        );
        // Left-justify is ignored when comma + zero-pad are both set
        assert_eq!(
            exec_printf(&[text("%-0,15d"), integer(42)]).unwrap(),
            *text("000,000,000,000,042").get_value()
        );
    }

    #[test]
    fn test_ordinal_format() {
        assert_eq!(
            exec_printf(&[text("%r"), integer(1)]).unwrap(),
            *text("1st").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%r"), integer(2)]).unwrap(),
            *text("2nd").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%r"), integer(3)]).unwrap(),
            *text("3rd").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%r"), integer(11)]).unwrap(),
            *text("11th").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%r"), integer(112)]).unwrap(),
            *text("112th").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%.5r"), integer(-39)]).unwrap(),
            *text("-039th").get_value()
        );
        assert_eq!(
            exec_printf(&[text("% r"), integer(42)]).unwrap(),
            *text(" 42nd").get_value()
        );
        // Zero-pad pads the digits before the suffix
        assert_eq!(
            exec_printf(&[text("%010r"), integer(0)]).unwrap(),
            *text("00000000th").get_value()
        );
    }

    #[test]
    fn test_q_null_precision_truncation() {
        // Precision truncates the NULL literal representation
        assert_eq!(
            exec_printf(&[text("%.0q"), Register::Value(Value::Null)]).unwrap(),
            *text("").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%.3q"), Register::Value(Value::Null)]).unwrap(),
            *text("(NU").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%.0Q"), Register::Value(Value::Null)]).unwrap(),
            *text("").get_value()
        );
    }

    #[test]
    fn test_q_null_width_padding() {
        // Width applies to the NULL representation
        assert_eq!(
            exec_printf(&[text("%-10q"), Register::Value(Value::Null)]).unwrap(),
            *text("(NULL)    ").get_value()
        );
    }

    #[test]
    fn test_unknown_specifier_returns_early() {
        // Unknown specifier as first thing → '' (SQLite 3.54; 3.50 gave NULL)
        assert_eq!(
            exec_printf(&[text("%b"), integer(42)]).unwrap(),
            *text("").get_value(),
        );
        // Unknown specifier after literal text → accumulated text
        assert_eq!(
            exec_printf(&[text("hello%b"), integer(42)]).unwrap(),
            *text("hello").get_value()
        );
        // Unknown specifier after %n → "" (StrAccum was allocated by %n processing)
        assert_eq!(
            exec_printf(&[text("%n%b"), integer(42)]).unwrap(),
            *text("").get_value(),
        );
    }

    #[test]
    fn test_control_char_escaping_with_hash_q() {
        // %#q escapes control characters as \uXXXX and doubles backslashes
        assert_eq!(
            exec_printf(&[text("%#q"), text("a\nb")]).unwrap(),
            *text("a\\u000ab").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%#q"), text("a\tb")]).unwrap(),
            *text("a\\u0009b").get_value()
        );
        // Backslash is doubled in escape mode
        assert_eq!(
            exec_printf(&[text("%#q"), text("a\\b")]).unwrap(),
            *text("a\\\\b").get_value()
        );
    }

    #[test]
    fn test_hash_q_upper_unistr_wrapping() {
        // %#Q wraps with unistr('...') when control chars are present
        assert_eq!(
            exec_printf(&[text("%#Q"), text("a\nb")]).unwrap(),
            *text("unistr('a\\u000ab')").get_value()
        );
        // %#Q without control chars — no unistr wrapping
        assert_eq!(
            exec_printf(&[text("%#Q"), text("hello")]).unwrap(),
            *text("'hello'").get_value()
        );
        // %Q without # — no unistr wrapping even with control chars
        assert_eq!(
            exec_printf(&[text("%Q"), text("a\nb")]).unwrap(),
            *text("'a\nb'").get_value()
        );
    }

    #[test]
    fn test_nul_ends_format_and_string_args() {
        // SQLite stops the format string and string arguments at the first NUL.
        assert_eq!(
            exec_printf(&[text("a\0b%d"), integer(5)]).unwrap(),
            *text("a").get_value()
        );
        assert_eq!(
            exec_printf(&[text("%s-%d"), text("a\0b"), integer(1)]).unwrap(),
            *text("a-1").get_value()
        );
        assert_eq!(
            exec_printf(&[
                text("%5s|%q|%Q|%w"),
                text("a\0b"),
                text("c\0d"),
                text("e\0f"),
                text("g\0h")
            ])
            .unwrap(),
            *text("    a|c|'e'|g").get_value()
        );
        // A format string that is empty up to the NUL gives '', like ''.
        assert_eq!(exec_printf(&[text("\0x")]).unwrap(), *text("").get_value());
    }

    #[test]
    fn test_very_small_float_no_nan() {
        // 1e-300 with %G should not produce NaN — round_half_away_e must handle
        // subnormal scale values from 10^(-309+) without dividing by ~0.
        let result = exec_printf(&[text("%.*G"), integer(10), float(1e-300)]).unwrap();
        assert_eq!(result, *text("1E-300").get_value());

        // Also test with %e
        let result = exec_printf(&[text("%.10e"), float(1e-300)]).unwrap();
        assert!(
            !result.to_string().contains("NaN"),
            "1e-300 with %e should not produce NaN"
        );

        // And %g
        let result = exec_printf(&[text("%.10g"), float(1e-300)]).unwrap();
        assert!(
            !result.to_string().contains("NaN"),
            "1e-300 with %g should not produce NaN"
        );
    }

    #[test]
    fn test_large_float_f_format() {
        // 1e308 with %f: SQLite 3.54's exact decode gives "1000..."
        // (3.50's double-double decode gave "9999...").
        let result = exec_printf(&[text("%.0f"), float(1e308)]).unwrap();
        let s = result.to_string();
        assert!(
            s.starts_with("10000000000000000"),
            "1e308 with %.0f should start with 1000..., got: {}",
            &s[..s.len().min(40)]
        );

        // With commas too
        let result = exec_printf(&[text("%,f"), float(1e308)]).unwrap();
        let s = result.to_string();
        assert!(
            s.starts_with("100,000,000,000,000,000"),
            "1e308 with %,f should start with 100,000..., got: {}",
            &s[..s.len().min(40)]
        );
    }

    #[test]
    fn test_negative_zero_suppression() {
        // SQLite 3.51+ (sqlite3.c:32520-32532): With # flag (no + or space),
        // %f suppresses minus sign when displayed value rounds to zero.

        // -0.0000001 with %#f displays as 0.000000 — suppress minus
        let result = exec_printf(&[text("%#f"), float(-0.0000001)]).unwrap();
        assert_eq!(result.to_string(), "0.000000");

        // Same without # flag — keep minus
        let result = exec_printf(&[text("%f"), float(-0.0000001)]).unwrap();
        assert_eq!(result.to_string(), "-0.000000");

        // With + flag, # doesn't suppress (flag_prefix is set)
        let result = exec_printf(&[text("%#+f"), float(-0.0000001)]).unwrap();
        assert_eq!(result.to_string(), "-0.000000");

        // -0.5 rounds to -1, not zero — keep minus
        let result = exec_printf(&[text("%#.0f"), float(-0.5)]).unwrap();
        assert_eq!(result.to_string(), "-1.");

        // -0.4 rounds to 0 — suppress
        let result = exec_printf(&[text("%#.0f"), float(-0.4)]).unwrap();
        assert_eq!(result.to_string(), "0.");

        // -0.0000001 with comma separator
        let result = exec_printf(&[text("%#,f"), float(-0.0000001)]).unwrap();
        assert_eq!(result.to_string(), "0.000000");
    }

    // Expected values below were produced by SQLite 3.54.1.
    #[test]
    fn test_sqlite_354_json_escapes() {
        let cases = vec![
            (vec![text("%J"), text("a\"b\n")], "\"a\\\"b\\n\""),
            (vec![text("%j"), text("a\"b")], "a\\\"b"),
            (
                vec![
                    text("%J|%j|"),
                    Register::Value(Value::Null),
                    Register::Value(Value::Null),
                ],
                "null||",
            ),
            (
                vec![text("%5J|%-5J|"), text("ab"), text("ab")],
                " \"ab\"|\"ab\" |",
            ),
            (vec![text("%.2J"), text("abcd")], "\"ab\""),
            (
                vec![text("%!.1J|%!5J|"), text("éa"), text("é")],
                "\"é\"|  \"é\"|",
            ),
            (
                vec![text("%J"), text("\u{1}\t\u{1f}")],
                "\"\\u0001\\t\\u001f\"",
            ),
            (vec![text("%J"), integer(12)], "\"12\""),
        ];
        for (input, expected) in cases {
            assert_eq!(exec_printf(&input).unwrap(), *text(expected).get_value());
        }
    }

    #[test]
    fn test_sqlite_354_float_decode() {
        let cases = vec![
            (vec![text("%!.17g"), float(49.47)], "49.47"),
            (vec![text("%!.17g"), float(0.1)], "0.1"),
            (
                vec![text("%!.17g"), float(1.0 / 3.0)],
                "0.33333333333333332",
            ),
            (vec![text("%!.17g"), float(1e23)], "1.0e+23"),
            (vec![text("%!.20e"), float(0.1)], "1.000000000000000056e-01"),
            (vec![text("%#.2f"), float(-0.001)], "0.00"),
            (
                vec![text("%+f|% f"), float(f64::INFINITY), float(f64::INFINITY)],
                "+Inf| Inf",
            ),
            (vec![text("%010.1f"), float(f64::NEG_INFINITY)], "-9"),
        ];
        for (input, expected) in cases {
            let got = exec_printf(&input).unwrap().to_string();
            assert!(got.starts_with(expected), "{got} vs {expected}");
        }
    }

    #[test]
    fn test_sqlite_354_invalid_conversions() {
        // 'h' is not a length modifier; %T and %S are internal-only.
        for f in ["%hd", "%T", "%S", "%5", "%-"] {
            assert_eq!(
                exec_printf(&[text(f), integer(5)]).unwrap(),
                *text("").get_value()
            );
        }
        assert_eq!(
            exec_printf(&[text("a%Tb"), integer(5)]).unwrap(),
            *text("a").get_value()
        );
        assert_eq!(
            printf_c_arg_plan("%d %j %n %5.*f %hd %s"),
            vec![
                PrintfCArg::Int32,
                PrintfCArg::Text,
                PrintfCArg::IntPtr,
                PrintfCArg::Int32,
                PrintfCArg::Double
            ]
        );
    }
}
