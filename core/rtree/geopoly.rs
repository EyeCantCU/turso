//! Geopoly (ext/rtree/geopoly.c): polygon functions and the `geopoly`
//! virtual table, an R*Tree over the polygons' bounding boxes.

use super::cursor::Constraint;
use super::funcs::{fmt_g, scalar};
use super::tree::{Cell, Writer};
use super::{sql, value_double, value_int64, Rtree, RTREE_GE, RTREE_LE};
use crate::native_ext::{
    Aggregate, AggregateFunction, ConstraintInfo, ConstraintUsage, FunctionArity, IndexInfo,
    ScalarCall, ScalarFunction,
};
use crate::sync::Arc;
use crate::types::IOResultOr;
use crate::ExternalFunc;
use crate::{Connection, IOResult, LimboError, Register, Result, Value};

/// A polygon: `a` holds x0, y0, x1, y1, ...
#[derive(Debug, Clone)]
pub(crate) struct GeoPoly {
    a: Vec<f32>,
}

impl GeoPoly {
    fn n(&self) -> usize {
        self.a.len() / 2
    }

    fn x(&self, i: usize) -> f32 {
        self.a[2 * i]
    }

    fn y(&self, i: usize) -> f32 {
        self.a[2 * i + 1]
    }

    /// The binary format: a 4-byte header (byte order, vertex count) and
    /// native-endian 32-bit floats.
    fn to_blob(&self) -> Value {
        let n = self.n();
        let mut out = Vec::with_capacity(4 + 8 * n);
        out.push(if cfg!(target_endian = "little") { 1 } else { 0 });
        out.push(((n >> 16) & 0xff) as u8);
        out.push(((n >> 8) & 0xff) as u8);
        out.push((n & 0xff) as u8);
        for v in &self.a {
            out.extend_from_slice(&v.to_ne_bytes());
        }
        Value::from_slice(&out).unwrap_or(Value::Null)
    }
}

/// Byte at `i`, or 0 past the end (the C code reads a NUL-terminated string).
fn at(z: &[u8], i: usize) -> u8 {
    z.get(i).copied().unwrap_or(0)
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

struct GeoParse<'a> {
    z: &'a [u8],
    pos: usize,
}

impl GeoParse<'_> {
    fn skip_space(&mut self) -> u8 {
        while is_space(at(self.z, self.pos)) {
            self.pos += 1;
        }
        at(self.z, self.pos)
    }

    /// `geopolyParseNumber`: 1 on success, 0 if not a number, -1 on a
    /// second exponent (which the caller treats as true, as in C).
    fn parse_number(&mut self, out: Option<&mut f32>) -> i32 {
        let mut c = self.skip_space();
        let z = self.z;
        let p = self.pos;
        let mut j = 0usize;
        let mut seen_dp = false;
        let mut seen_e = false;
        if c == b'-' {
            j = 1;
            c = at(z, p + j);
        }
        if c == b'0' && at(z, p + j + 1).is_ascii_digit() {
            return 0;
        }
        loop {
            c = at(z, p + j);
            if c.is_ascii_digit() {
                j += 1;
                continue;
            }
            if c == b'.' {
                if at(z, p + j - 1) == b'-' {
                    return 0;
                }
                if seen_dp {
                    return 0;
                }
                seen_dp = true;
                j += 1;
                continue;
            }
            if c == b'e' || c == b'E' {
                if at(z, p + j - 1) < b'0' {
                    return 0;
                }
                if seen_e {
                    return -1;
                }
                seen_dp = true;
                seen_e = true;
                c = at(z, p + j + 1);
                if c == b'+' || c == b'-' {
                    j += 1;
                    c = at(z, p + j + 1);
                }
                if !c.is_ascii_digit() {
                    return 0;
                }
                j += 1;
                continue;
            }
            break;
        }
        if j == 0 || at(z, p + j - 1) < b'0' {
            return 0;
        }
        let _ = seen_dp;
        if let Some(out) = out {
            let s = std::str::from_utf8(&z[p..p + j]).unwrap_or("0");
            *out = s.parse::<f64>().unwrap_or(0.0) as f32;
        }
        self.pos += j;
        1
    }
}

/// `geopolyParseJson`.
fn parse_json(z: &[u8]) -> Option<GeoPoly> {
    let mut s = GeoParse { z, pos: 0 };
    let mut a: Vec<f32> = Vec::new();
    let mut n_vertex = 0usize;
    if s.skip_space() != b'[' {
        return None;
    }
    s.pos += 1;
    while s.skip_space() == b'[' {
        let mut ii = 0;
        s.pos += 1;
        if a.len() < (n_vertex + 1) * 2 {
            a.resize((n_vertex + 1) * 2, 0.0);
        }
        loop {
            let mut v = 0.0f32;
            let r = s.parse_number(if ii <= 1 { Some(&mut v) } else { None });
            if r == 0 {
                break;
            }
            if ii <= 1 && r == 1 {
                a[n_vertex * 2 + ii] = v;
            }
            ii += 1;
            if ii == 2 {
                n_vertex += 1;
            }
            let c = s.skip_space();
            s.pos += 1;
            if c == b',' {
                if a.len() < (n_vertex + 1) * 2 {
                    a.resize((n_vertex + 1) * 2, 0.0);
                }
                continue;
            }
            if c == b']' && ii >= 2 {
                break;
            }
            return None;
        }
        if s.skip_space() == b',' {
            s.pos += 1;
            continue;
        }
        break;
    }
    if s.skip_space() == b']'
        && (4..16777216).contains(&n_vertex)
        && a[0] == a[n_vertex * 2 - 2]
        && a[1] == a[n_vertex * 2 - 1]
    {
        s.pos += 1;
        if s.skip_space() == 0 {
            // Drop the closing vertex, which repeats the first.
            a.truncate((n_vertex - 1) * 2);
            return Some(GeoPoly { a });
        }
    }
    None
}

/// `geopolyFuncParam`. `Err(())` is SQLITE_ERROR; `Ok(None)` is a blob
/// that is not a polygon, which SQLite does not treat as an error.
fn func_param(v: &Value) -> std::result::Result<Option<GeoPoly>, ()> {
    match v {
        Value::Blob(b) if b.len() >= 4 + 6 * 4 => {
            let n = ((b[1] as usize) << 16) + ((b[2] as usize) << 8) + b[3] as usize;
            if (b[0] == 0 || b[0] == 1) && n * 8 + 4 == b.len() {
                let big = b[0] == 0;
                let a = b[4..]
                    .chunks_exact(4)
                    .map(|c| {
                        let c: [u8; 4] = c.try_into().expect("4 bytes");
                        if big {
                            f32::from_be_bytes(c)
                        } else {
                            f32::from_le_bytes(c)
                        }
                    })
                    .collect();
                Ok(Some(GeoPoly { a }))
            } else {
                Ok(None)
            }
        }
        Value::Text(t) => {
            let z = t.as_str().as_bytes();
            // Text that does not open with '[' is not an error in SQLite.
            if z.iter().copied().find(|&c| !is_space(c)) != Some(b'[') {
                return Ok(None);
            }
            parse_json(z).map(Some).ok_or(())
        }
        _ => Err(()),
    }
}

fn param(v: &Value) -> Option<GeoPoly> {
    func_param(v).ok().flatten()
}

fn area(p: &GeoPoly) -> f64 {
    let n = p.n();
    let mut r = 0.0f64;
    let mut ii = 0;
    while ii + 1 < n {
        let t: f32 = (p.x(ii) - p.x(ii + 1)) * (p.y(ii) + p.y(ii + 1));
        r += t as f64 * 0.5;
        ii += 1;
    }
    let t: f32 = (p.x(ii) - p.x(0)) * (p.y(ii) + p.y(0));
    r += t as f64 * 0.5;
    r
}

const GEOPOLY_PI: f64 = std::f64::consts::PI;

fn sine(r: f64) -> f64 {
    let mut r = r;
    if r >= 1.5 * GEOPOLY_PI {
        r -= 2.0 * GEOPOLY_PI;
    }
    if r >= 0.5 * GEOPOLY_PI {
        -sine(r - GEOPOLY_PI)
    } else {
        let r2 = r * r;
        let r3 = r2 * r;
        let r5 = r3 * r2;
        0.9996949 * r - 0.1656700 * r3 + 0.0075134 * r5
    }
}

/// The bounding box as [minX, maxX, minY, maxY].
fn bbox_of(p: &GeoPoly) -> [f32; 4] {
    let (mut mnx, mut mxx, mut mny, mut mxy) = (p.x(0), p.x(0), p.y(0), p.y(0));
    for ii in 1..p.n() {
        let r = p.x(ii) as f64;
        if r < mnx as f64 {
            mnx = r as f32;
        } else if r > mxx as f64 {
            mxx = r as f32;
        }
        let r = p.y(ii) as f64;
        if r < mny as f64 {
            mny = r as f32;
        } else if r > mxy as f64 {
            mxy = r as f32;
        }
    }
    [mnx, mxx, mny, mxy]
}

fn bbox_poly(b: [f32; 4]) -> GeoPoly {
    GeoPoly {
        a: vec![b[0], b[2], b[1], b[2], b[1], b[3], b[0], b[3]],
    }
}

/// `geopolyBBox` into coordinates: zeros for a blob that is not a polygon.
fn bbox_coords(v: &Value) -> std::result::Result<[f32; 4], ()> {
    Ok(func_param(v)?.map_or([0.0; 4], |p| bbox_of(&p)))
}

/// `pointBeneathLine`.
fn point_beneath_line(x0: f64, y0: f64, x1: f64, y1: f64, x2: f64, y2: f64) -> i32 {
    if x0 == x1 && y0 == y1 {
        return 2;
    }
    if x1 < x2 {
        if x0 <= x1 || x0 > x2 {
            return 0;
        }
    } else if x1 > x2 {
        if x0 <= x2 || x0 > x1 {
            return 0;
        }
    } else {
        if x0 != x1 {
            return 0;
        }
        if y0 < y1 && y0 < y2 {
            return 0;
        }
        if y0 > y1 && y0 > y2 {
            return 0;
        }
        return 2;
    }
    let y = y1 + (y2 - y1) * (x0 - x1) / (x2 - x1);
    if y0 == y {
        return 2;
    }
    if y0 < y {
        return 1;
    }
    0
}

#[derive(Clone, Copy)]
struct Event {
    x: f64,
    remove: bool,
    seg: usize,
    next: Option<usize>,
}

#[derive(Clone, Copy)]
struct Segment {
    c: f64,
    b: f64,
    y: f64,
    y0: f32,
    side: u8,
    next: Option<usize>,
}

struct Overlap {
    events: Vec<Event>,
    segs: Vec<Segment>,
}

impl Overlap {
    fn add_one_segment(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, side: u8) {
        if x0 == x1 {
            return;
        }
        let (x0, y0, x1, y1) = if x0 > x1 {
            (x1, y1, x0, y0)
        } else {
            (x0, y0, x1, y1)
        };
        // C divides in float and widens the slope to double.
        let c = ((y1 - y0) / (x1 - x0)) as f64;
        let seg = Segment {
            c,
            b: y1 as f64 - x1 as f64 * c,
            y: 0.0,
            y0,
            side,
            next: None,
        };
        let idx = self.segs.len();
        self.segs.push(seg);
        self.events.push(Event {
            x: x0 as f64,
            remove: false,
            seg: idx,
            next: None,
        });
        self.events.push(Event {
            x: x1 as f64,
            remove: true,
            seg: idx,
            next: None,
        });
    }

    fn add_segments(&mut self, p: &GeoPoly, side: u8) {
        let n = p.n();
        for i in 0..n - 1 {
            self.add_one_segment(p.x(i), p.y(i), p.x(i + 1), p.y(i + 1), side);
        }
        self.add_one_segment(p.x(n - 1), p.y(n - 1), p.a[0], p.a[1], side);
    }

    fn event_merge(&mut self, mut l: Option<usize>, mut r: Option<usize>) -> Option<usize> {
        let mut head: Option<usize> = None;
        let mut last: Option<usize> = None;
        let link = |s: &mut Self, n: usize, head: &mut Option<usize>, last: &mut Option<usize>| {
            match *last {
                Some(l) => s.events[l].next = Some(n),
                None => *head = Some(n),
            }
            *last = Some(n);
        };
        while let (Some(ri), Some(li)) = (r, l) {
            if self.events[ri].x <= self.events[li].x {
                r = self.events[ri].next;
                link(self, ri, &mut head, &mut last);
            } else {
                l = self.events[li].next;
                link(self, li, &mut head, &mut last);
            }
        }
        let rest = if r.is_some() { r } else { l };
        match last {
            Some(last) => self.events[last].next = rest,
            None => head = rest,
        }
        head
    }

    fn sort_events(&mut self) -> Option<usize> {
        let mut a: Vec<Option<usize>> = vec![None; 50];
        let mut mx = 0;
        for i in 0..self.events.len() {
            self.events[i].next = None;
            let mut p = Some(i);
            let mut j = 0;
            while j < mx && a[j].is_some() {
                p = self.event_merge(a[j], p);
                a[j] = None;
                j += 1;
            }
            a[j] = p;
            if j >= mx {
                mx = j + 1;
            }
        }
        let mut p = None;
        for slot in a.iter().take(mx) {
            p = self.event_merge(*slot, p);
        }
        p
    }

    fn seg_merge(&mut self, mut l: Option<usize>, mut r: Option<usize>) -> Option<usize> {
        let mut head: Option<usize> = None;
        let mut last: Option<usize> = None;
        let link = |s: &mut Self, n: usize, head: &mut Option<usize>, last: &mut Option<usize>| {
            match *last {
                Some(l) => s.segs[l].next = Some(n),
                None => *head = Some(n),
            }
            *last = Some(n);
        };
        while let (Some(ri), Some(li)) = (r, l) {
            let mut d = self.segs[ri].y - self.segs[li].y;
            if d == 0.0 {
                d = self.segs[ri].c - self.segs[li].c;
            }
            if d < 0.0 {
                r = self.segs[ri].next;
                link(self, ri, &mut head, &mut last);
            } else {
                l = self.segs[li].next;
                link(self, li, &mut head, &mut last);
            }
        }
        let rest = if r.is_some() { r } else { l };
        match last {
            Some(last) => self.segs[last].next = rest,
            None => head = rest,
        }
        head
    }

    fn sort_segments(&mut self, mut list: Option<usize>) -> Option<usize> {
        let mut a: Vec<Option<usize>> = vec![None; 50];
        let mut mx = 0;
        while let Some(p0) = list {
            list = self.segs[p0].next;
            self.segs[p0].next = None;
            let mut p = Some(p0);
            let mut i = 0;
            while i < mx && a[i].is_some() {
                p = self.seg_merge(a[i], p);
                a[i] = None;
                i += 1;
            }
            a[i] = p;
            if i >= mx {
                mx = i + 1;
            }
        }
        let mut p = None;
        for slot in a.iter().take(mx) {
            p = self.seg_merge(*slot, p);
        }
        p
    }
}

/// `geopolyOverlap`: 0 disjoint, 1 overlap, 2 p1 within p2, 3 p2 within
/// p1, 4 the same polygon.
fn overlap(p1: &GeoPoly, p2: &GeoPoly) -> i32 {
    let mut o = Overlap {
        events: Vec::new(),
        segs: Vec::new(),
    };
    o.add_segments(p1, 1);
    o.add_segments(p2, 2);
    let mut this = o.sort_events();
    let mut rx = match this {
        Some(e) if o.events[e].x == 0.0 => -1.0,
        _ => 0.0,
    };
    let mut a_overlap = [false; 4];
    let mut need_sort = false;
    let mut active: Option<usize> = None;
    while let Some(ev) = this {
        if o.events[ev].x != rx {
            rx = o.events[ev].x;
            if need_sort {
                active = o.sort_segments(active);
                need_sort = false;
            }
            let mut mask = 0usize;
            let mut prev: Option<usize> = None;
            let mut s = active;
            while let Some(si) = s {
                if let Some(pi) = prev {
                    if o.segs[pi].y != o.segs[si].y {
                        a_overlap[mask] = true;
                    }
                }
                mask ^= o.segs[si].side as usize;
                prev = Some(si);
                s = o.segs[si].next;
            }
            prev = None;
            s = active;
            while let Some(si) = s {
                let y = o.segs[si].c * rx + o.segs[si].b;
                o.segs[si].y = y;
                if let Some(pi) = prev {
                    if o.segs[pi].y > o.segs[si].y && o.segs[pi].side != o.segs[si].side {
                        return 1;
                    } else if o.segs[pi].y != o.segs[si].y {
                        a_overlap[mask] = true;
                    }
                }
                mask ^= o.segs[si].side as usize;
                prev = Some(si);
                s = o.segs[si].next;
            }
        }
        let seg = o.events[ev].seg;
        if !o.events[ev].remove {
            o.segs[seg].y = o.segs[seg].y0 as f64;
            o.segs[seg].next = active;
            active = Some(seg);
            need_sort = true;
        } else if active == Some(seg) {
            active = o.segs[seg].next;
        } else {
            let mut s = active;
            while let Some(si) = s {
                if o.segs[si].next == Some(seg) {
                    o.segs[si].next = o.segs[seg].next;
                    break;
                }
                s = o.segs[si].next;
            }
        }
        this = o.events[ev].next;
    }
    if !a_overlap[3] {
        0
    } else if a_overlap[1] && !a_overlap[2] {
        3
    } else if !a_overlap[1] && a_overlap[2] {
        2
    } else if !a_overlap[1] && !a_overlap[2] {
        4
    } else {
        1
    }
}

fn v(args: &[Register], i: usize) -> &Value {
    args[i].get_value()
}

scalar!(AreaFn, AreaCall, |_c, args| Ok(
    param(v(args, 0)).map_or(Value::Null, |p| Value::from_f64(area(&p)))
));

scalar!(BlobFn, BlobCall, |_c, args| Ok(
    param(v(args, 0)).map_or(Value::Null, |p| p.to_blob())
));

scalar!(JsonFn, JsonCall, |_c, args| Ok(param(v(args, 0)).map_or(
    Value::Null,
    |p| {
        let mut s = String::from("[");
        for i in 0..p.n() {
            s.push_str(&format!(
                "[{},{}],",
                fmt_g(p.x(i) as f64, true),
                fmt_g(p.y(i) as f64, true)
            ));
        }
        s.push_str(&format!(
            "[{},{}]]",
            fmt_g(p.x(0) as f64, true),
            fmt_g(p.y(0) as f64, true)
        ));
        Value::build_text(s)
    }
)));

scalar!(SvgFn, SvgCall, |_c, args| {
    if args.is_empty() {
        return Ok(IOResult::Done(Value::Null));
    }
    Ok(param(v(args, 0)).map_or(Value::Null, |p| {
        let mut s = String::from("<polyline points=");
        let mut sep = '\'';
        for i in 0..p.n() {
            s.push_str(&format!(
                "{sep}{},{}",
                fmt_g(p.x(i) as f64, false),
                fmt_g(p.y(i) as f64, false)
            ));
            sep = ' ';
        }
        s.push_str(&format!(
            " {},{}'",
            fmt_g(p.x(0) as f64, false),
            fmt_g(p.y(0) as f64, false)
        ));
        for a in &args[1..] {
            if let Some(z) = a.get_value().cast_text() {
                if !z.is_empty() {
                    s.push(' ');
                    s.push_str(&z);
                }
            }
        }
        s.push_str("></polyline>");
        Value::build_text(s)
    }))
});

scalar!(XformFn, XformCall, |_c, args| {
    let k: Vec<f64> = (1..7).map(|i| value_double(v(args, i))).collect();
    Ok(param(v(args, 0)).map_or(Value::Null, |mut p| {
        for ii in 0..p.n() {
            let (x0, y0) = (p.x(ii) as f64, p.y(ii) as f64);
            p.a[2 * ii] = (k[0] * x0 + k[1] * y0 + k[4]) as f32;
            p.a[2 * ii + 1] = (k[2] * x0 + k[3] * y0 + k[5]) as f32;
        }
        p.to_blob()
    }))
});

scalar!(CcwFn, CcwCall, |_c, args| Ok(param(v(args, 0)).map_or(
    Value::Null,
    |mut p| {
        if area(&p) < 0.0 {
            let (mut ii, mut jj) = (1, p.n() - 1);
            while ii < jj {
                p.a.swap(2 * ii, 2 * jj);
                p.a.swap(2 * ii + 1, 2 * jj + 1);
                ii += 1;
                jj -= 1;
            }
        }
        p.to_blob()
    }
)));

scalar!(RegularFn, RegularCall, |_c, args| {
    let x = value_double(v(args, 0));
    let y = value_double(v(args, 1));
    let r = value_double(v(args, 2));
    let mut n = value_int64(v(args, 3));
    if n < 3 || r <= 0.0 || r >= 1e300 {
        return Ok(IOResult::Done(Value::Null));
    }
    n = n.min(1000);
    let mut a = Vec::with_capacity(2 * n as usize);
    for i in 0..n {
        let angle = 2.0 * GEOPOLY_PI * i as f64 / n as f64;
        a.push((x - r * sine(angle - 0.5 * GEOPOLY_PI)) as f32);
        a.push((y + r * sine(angle)) as f32);
    }
    Ok(GeoPoly { a }.to_blob())
});

scalar!(BboxFn, BboxCall, |_c, args| Ok(
    param(v(args, 0)).map_or(Value::Null, |p| bbox_poly(bbox_of(&p)).to_blob())
));

scalar!(ContainsPointFn, ContainsPointCall, |_c, args| {
    let x0 = value_double(v(args, 1));
    let y0 = value_double(v(args, 2));
    let Some(p) = param(v(args, 0)) else {
        return Ok(IOResult::Done(Value::Null));
    };
    let n = p.n();
    let mut res = 0;
    let mut cnt = 0;
    let mut ii = 0;
    while ii + 1 < n {
        res = point_beneath_line(
            x0,
            y0,
            p.x(ii) as f64,
            p.y(ii) as f64,
            p.x(ii + 1) as f64,
            p.y(ii + 1) as f64,
        );
        if res == 2 {
            break;
        }
        cnt += res;
        ii += 1;
    }
    if res != 2 {
        res = point_beneath_line(
            x0,
            y0,
            p.x(ii) as f64,
            p.y(ii) as f64,
            p.x(0) as f64,
            p.y(0) as f64,
        );
    }
    Ok(Value::from_i64(if res == 2 {
        1
    } else if (res + cnt) & 1 == 0 {
        0
    } else {
        2
    }))
});

scalar!(WithinFn, WithinCall, |_c, args| {
    match (param(v(args, 0)), param(v(args, 1))) {
        (Some(p1), Some(p2)) => {
            let x = overlap(&p1, &p2);
            Ok(Value::from_i64(match x {
                2 => 1,
                4 => 2,
                _ => 0,
            }))
        }
        _ => Ok(Value::Null),
    }
});

scalar!(OverlapFn, OverlapCall, |_c, args| {
    match (param(v(args, 0)), param(v(args, 1))) {
        (Some(p1), Some(p2)) => Ok(Value::from_i64(overlap(&p1, &p2) as i64)),
        _ => Ok(Value::Null),
    }
});

scalar!(DebugFn, DebugCall, |_c, _args| Ok(Value::Null));

#[derive(Debug)]
struct GroupBboxFn;

#[derive(Debug, Default)]
struct GroupBbox {
    a: Option<[f32; 4]>,
}

impl AggregateFunction for GroupBboxFn {
    type Accumulator = GroupBbox;

    fn create_accumulator(&self) -> Result<Self::Accumulator> {
        Ok(GroupBbox::default())
    }
}

impl Aggregate for GroupBbox {
    fn step(&mut self, args: &[Value]) -> IOResultOr<()> {
        if let Ok(a) = bbox_coords(&args[0]) {
            self.a = Some(match self.a {
                None => a,
                Some(mut b) => {
                    if a[0] < b[0] {
                        b[0] = a[0];
                    }
                    if a[1] > b[1] {
                        b[1] = a[1];
                    }
                    if a[2] < b[2] {
                        b[2] = a[2];
                    }
                    if a[3] > b[3] {
                        b[3] = a[3];
                    }
                    b
                }
            });
        }
        Ok(IOResult::Done(()))
    }

    fn finalize(&mut self) -> IOResultOr<Value> {
        Ok(IOResult::Done(
            self.a.map_or(Value::Null, |b| bbox_poly(b).to_blob()),
        ))
    }
}

pub(crate) fn functions() -> Result<Vec<ExternalFunc>> {
    fn f(
        name: &str,
        arity: FunctionArity,
        func: impl ScalarFunction + 'static,
    ) -> Result<ExternalFunc> {
        ExternalFunc::new_native_scalar(name.to_string(), arity, true, func)
    }
    use FunctionArity::{Exact, Variadic};
    Ok(vec![
        f("geopoly_area", Exact(1), AreaFn)?,
        f("geopoly_blob", Exact(1), BlobFn)?,
        f("geopoly_json", Exact(1), JsonFn)?,
        f("geopoly_svg", Variadic, SvgFn)?,
        f("geopoly_within", Exact(2), WithinFn)?,
        f("geopoly_contains_point", Exact(3), ContainsPointFn)?,
        f("geopoly_overlap", Exact(2), OverlapFn)?,
        f("geopoly_debug", Exact(1), DebugFn)?,
        f("geopoly_bbox", Exact(1), BboxFn)?,
        f("geopoly_xform", Exact(7), XformFn)?,
        f("geopoly_regular", Exact(4), RegularFn)?,
        f("geopoly_ccw", Exact(1), CcwFn)?,
        ExternalFunc::new_native_aggregate("geopoly_group_bbox".to_string(), 1, GroupBboxFn)?,
    ])
}

/// xBestIndex of geopoly.c. Turso offers neither rowid nor function
/// constraints, so this is a full scan unless one shows up.
pub(crate) fn best_index(constraints: &[ConstraintInfo]) -> IndexInfo {
    IndexInfo {
        idx_num: 4,
        idx_str: Some("fullscan".to_string()),
        order_by_consumed: false,
        estimated_cost: 3000000.0,
        estimated_rows: 100000,
        constraint_usages: vec![
            ConstraintUsage {
                argv_index: None,
                omit: false,
            };
            constraints.len()
        ],
    }
}

/// The constraints of an overlap (2) or within (3) query for polygon `arg`.
pub(crate) fn filter_constraints(idx_num: i32, arg: &Value) -> Result<Vec<Constraint>> {
    let b = bbox_coords(arg).map_err(|_| LimboError::SqlError("SQL logic error".into()))?;
    let c = |op, coord, value: f32| Constraint {
        op,
        coord,
        value: value as f64,
        matcher: None,
    };
    Ok(if idx_num == 2 {
        vec![
            c(RTREE_LE, 0, b[1]),
            c(RTREE_GE, 1, b[0]),
            c(RTREE_LE, 2, b[3]),
            c(RTREE_GE, 3, b[2]),
        ]
    } else {
        vec![
            c(RTREE_GE, 0, b[0]),
            c(RTREE_LE, 1, b[1]),
            c(RTREE_GE, 2, b[2]),
            c(RTREE_LE, 3, b[3]),
        ]
    })
}

/// xUpdate of geopoly.c, with turso's argument layout.
pub(crate) fn update(
    t: &Rtree,
    w: &mut Writer,
    args: &[Value],
    on_conflict: u8,
) -> Result<Option<i64>> {
    let n_data = if args.len() == 2 { 1 } else { args.len() };
    let old_valid = !matches!(args[0], Value::Null);
    let old_rowid = if old_valid { value_int64(&args[0]) } else { 0 };
    let new_valid = n_data > 1 && !matches!(args[1], Value::Null);
    let new_rowid = if new_valid { value_int64(&args[1]) } else { 0 };
    let mut cell = Cell {
        rowid: new_rowid,
        ..Default::default()
    };
    let mut coord_change = false;
    let result = (|| -> Result<Option<i64>> {
        if n_data > 1 {
            // Turso always supplies unchanged columns, so the shape is
            // always re-read.
            let b = bbox_coords(&args[2]).map_err(|_| {
                LimboError::SqlError("_shape does not contain a valid polygon".into())
            })?;
            for (k, v) in b.iter().enumerate() {
                cell.coord[k] = v.to_bits();
            }
            coord_change = true;
            if new_valid && (!old_valid || old_rowid != new_rowid) && w.rowid_exists(cell.rowid)? {
                if on_conflict == 5 {
                    w.delete_rowid(cell.rowid)?;
                } else {
                    return Err(t.constraint_error(0));
                }
            }
        }
        if n_data == 1 || (coord_change && old_valid) {
            w.delete_rowid(old_rowid)?;
        }
        let mut rowid = None;
        if n_data > 1 && coord_change {
            rowid = Some(w.insert(&mut cell, new_valid)?);
        }
        if n_data > 1 {
            let shape = match &args[2] {
                Value::Text(_) => match func_param(&args[2]) {
                    Ok(Some(p)) => p.to_blob(),
                    _ => args[2].clone(),
                },
                v => v.clone(),
            };
            let mut params = vec![Value::from_i64(cell.rowid), shape];
            params.extend(args[3..].iter().cloned());
            w.stmts
                .execute(sql::WRITE_AUX, t.sql(sql::WRITE_AUX), &params)?;
        }
        Ok(if old_valid { None } else { rowid })
    })();
    match result {
        Err(LimboError::Constraint(_)) if on_conflict == 4 => Err(LimboError::RaiseIgnore),
        r => r,
    }
}
