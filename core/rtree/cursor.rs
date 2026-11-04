//! The R*Tree cursor: a best-first search driven by a priority queue of
//! search points, as in rtree.c.

use super::matchfn::{decode_match_arg, MatchConstraint};
use super::shadow::Stmts;
use super::{
    int_equals_float, sql, value_int64, value_numeric, Rtree, RtreeKind, FULLY_WITHIN, NOT_WITHIN,
    PARTLY_WITHIN, RTREE_EQ, RTREE_FALSE, RTREE_GE, RTREE_GT, RTREE_LE, RTREE_LT, RTREE_MATCH,
    RTREE_MAXCELLS, RTREE_MAX_DEPTH, RTREE_QUERY, RTREE_TRUE,
};
use crate::native_ext::VirtualTableCursor;
use crate::numeric::Numeric;
use crate::sync::Arc;
use crate::types::IOResultOr;
use crate::{Connection, IOResult, LimboError, Result, Value};

#[derive(Debug, Clone, Copy, Default)]
struct SearchPoint {
    score: f64,
    id: i64,
    level: u8,
    within: u8,
    cell: u8,
}

pub(crate) struct Constraint {
    pub(crate) coord: usize,
    pub(crate) op: u8,
    pub(crate) value: f64,
    pub(crate) matcher: Option<MatchConstraint>,
}

pub(crate) fn corrupt() -> LimboError {
    LimboError::Corrupt("database disk image is malformed".to_string())
}

pub(crate) fn read_u16(p: &[u8]) -> usize {
    ((p[0] as usize) << 8) + p[1] as usize
}

pub(crate) fn read_i64(p: &[u8]) -> i64 {
    i64::from_be_bytes(p[..8].try_into().expect("8 bytes"))
}

pub(crate) fn read_u32(p: &[u8]) -> u32 {
    u32::from_be_bytes(p[..4].try_into().expect("4 bytes"))
}

/// Decodes a stored coordinate.
pub(crate) fn decode_coord(int: bool, p: &[u8]) -> f64 {
    let u = read_u32(p);
    if int {
        u as i32 as f64
    } else {
        f32::from_bits(u) as f64
    }
}

/// Reads node `id` through `stmts`, checking its size and cell count.
pub(crate) fn read_node(
    t: &Rtree,
    stmts: &mut Stmts<{ sql::COUNT }>,
    node_size: usize,
    id: i64,
) -> Result<Vec<u8>> {
    if id <= 0 {
        return Err(corrupt());
    }
    let row = stmts.query_row(
        sql::READ_NODE,
        t.sql(sql::READ_NODE),
        &[Value::from_i64(id)],
    )?;
    let data = match row.as_ref().and_then(|r| r.first()) {
        Some(Value::Blob(b)) => b.to_vec(),
        Some(Value::Text(s)) => s.as_str().as_bytes().to_vec(),
        _ => return Err(corrupt()),
    };
    if data.len() != node_size {
        return Err(corrupt());
    }
    if id == 1 && read_u16(&data) >= RTREE_MAX_DEPTH {
        return Err(corrupt());
    }
    if read_u16(&data[2..]) > (node_size - 4) / t.bytes_per_cell {
        return Err(corrupt());
    }
    Ok(data)
}

pub(crate) struct RtreeCursor {
    t: Arc<Rtree>,
    stmts: Stmts<{ sql::COUNT }>,
    node_size: usize,
    at_eof: bool,
    constraints: Vec<Constraint>,
    has_point: bool,
    point: SearchPoint,
    queue: Vec<SearchPoint>,
    an_queue: [u32; RTREE_MAX_DEPTH + 2],
    depth: i32,
    /// Nodes read during the current scan, by node number.
    cache: Vec<(i64, Arc<Vec<u8>>)>,
    aux: Option<Vec<Value>>,
    cur_rowid: i64,
}

const CACHE_SIZE: usize = 8;

impl RtreeCursor {
    pub(crate) fn new(t: Arc<Rtree>, conn: Arc<Connection>) -> Self {
        Self {
            t,
            stmts: Stmts::new(conn),
            node_size: 0,
            at_eof: true,
            constraints: Vec::new(),
            has_point: false,
            point: SearchPoint::default(),
            queue: Vec::new(),
            an_queue: [0; RTREE_MAX_DEPTH + 2],
            depth: 0,
            cache: Vec::new(),
            aux: None,
            cur_rowid: 0,
        }
    }

    fn reset(&mut self) {
        self.constraints.clear();
        self.has_point = false;
        self.queue.clear();
        self.an_queue = [0; RTREE_MAX_DEPTH + 2];
        self.cache.clear();
        self.aux = None;
        self.at_eof = false;
    }

    fn node(&mut self, id: i64) -> Result<Arc<Vec<u8>>> {
        if let Some(pos) = self.cache.iter().position(|(n, _)| *n == id) {
            return Ok(self.cache[pos].1.clone());
        }
        let data = Arc::new(read_node(&self.t, &mut self.stmts, self.node_size, id)?);
        if self.cache.len() >= CACHE_SIZE {
            self.cache.remove(0);
        }
        self.cache.push((id, data.clone()));
        Ok(data)
    }

    fn first(&self) -> Option<SearchPoint> {
        if self.has_point {
            Some(self.point)
        } else {
            self.queue.first().copied()
        }
    }

    fn set_first(&mut self, p: SearchPoint) {
        if self.has_point {
            self.point = p;
        } else {
            self.queue[0] = p;
        }
    }

    fn compare(a: &SearchPoint, b: &SearchPoint) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        if a.score < b.score {
            return Less;
        }
        if a.score > b.score {
            return Greater;
        }
        a.level.cmp(&b.level)
    }

    /// Pushes onto the heap; returns the index of the new point.
    fn enqueue(&mut self, score: f64, level: u8) -> usize {
        let mut i = self.queue.len();
        self.queue.push(SearchPoint {
            score,
            level,
            ..Default::default()
        });
        while i > 0 {
            let j = (i - 1) / 2;
            if Self::compare(&self.queue[i], &self.queue[j]) != std::cmp::Ordering::Less {
                break;
            }
            self.queue.swap(i, j);
            i = j;
        }
        i
    }

    /// `rtreeSearchPointNew`; the caller fills in the returned point.
    fn search_point_new(&mut self, score: f64, level: u8) -> PointRef {
        let first = self.first();
        self.an_queue[level as usize] += 1;
        let goes_first = match first {
            None => true,
            Some(f) => f.score > score || (f.score == score && f.level > level),
        };
        if goes_first {
            if self.has_point {
                let i = self.enqueue(score, level);
                self.queue[i] = self.point;
            }
            self.point.score = score;
            self.point.level = level;
            self.has_point = true;
            PointRef::Point
        } else {
            PointRef::Queue(self.enqueue(score, level))
        }
    }

    fn point_mut(&mut self, r: PointRef) -> &mut SearchPoint {
        match r {
            PointRef::Point => &mut self.point,
            PointRef::Queue(i) => &mut self.queue[i],
        }
    }

    fn pop(&mut self) {
        if self.has_point {
            self.an_queue[self.point.level as usize] -= 1;
            self.has_point = false;
        } else if !self.queue.is_empty() {
            self.an_queue[self.queue[0].level as usize] -= 1;
            let last = self.queue.pop().expect("nonempty");
            let n = self.queue.len();
            if n == 0 {
                return;
            }
            self.queue[0] = last;
            let mut i = 0;
            loop {
                let j = i * 2 + 1;
                if j >= n {
                    break;
                }
                let k = j + 1;
                use std::cmp::Ordering::Less;
                if k < n && Self::compare(&self.queue[k], &self.queue[j]) == Less {
                    if Self::compare(&self.queue[k], &self.queue[i]) == Less {
                        self.queue.swap(i, k);
                        i = k;
                    } else {
                        break;
                    }
                } else if Self::compare(&self.queue[j], &self.queue[i]) == Less {
                    self.queue.swap(i, j);
                    i = j;
                } else {
                    break;
                }
            }
        }
    }

    /// `rtreeStepToLeaf`.
    fn step_to_leaf(&mut self) -> Result<()> {
        let int = self.t.int_coords();
        let bpc = self.t.bytes_per_cell;
        while let Some(mut p) = self.first().filter(|p| p.level > 0) {
            let node = self.node(p.id)?;
            let n_cell = read_u16(&node[2..]);
            if n_cell > RTREE_MAXCELLS {
                return Err(corrupt());
            }
            let mut pushed = false;
            while (p.cell as usize) < n_cell {
                let cell = &node[4 + bpc * p.cell as usize..4 + bpc * (p.cell as usize + 1)];
                let mut score = -1.0f64;
                let mut within = FULLY_WITHIN;
                for ii in 0..self.constraints.len() {
                    let c = &mut self.constraints[ii];
                    if c.op >= RTREE_MATCH {
                        callback_constraint(
                            c,
                            int,
                            cell,
                            &p,
                            self.depth + 1,
                            &self.an_queue,
                            &mut score,
                            &mut within,
                        )?;
                    } else if p.level == 1 {
                        leaf_constraint(c, int, cell, &mut within);
                    } else {
                        nonleaf_constraint(c, int, cell, &mut within);
                    }
                    if within == NOT_WITHIN {
                        break;
                    }
                }
                if within == NOT_WITHIN {
                    p.cell += 1;
                    continue;
                }
                p.cell += 1;
                let level = p.level - 1;
                let (id, icell) = if level > 0 {
                    let id = read_i64(cell);
                    if self.queue.iter().any(|q| q.id == id) {
                        return Err(corrupt());
                    }
                    (id, 0)
                } else {
                    (p.id, p.cell - 1)
                };
                self.set_first(p);
                if p.cell as usize >= n_cell {
                    self.pop();
                }
                if score < 0.0 {
                    score = 0.0;
                }
                let r = self.search_point_new(score, level);
                let np = self.point_mut(r);
                np.within = within as u8;
                np.id = id;
                np.cell = icell;
                pushed = true;
                break;
            }
            if !pushed {
                self.set_first(p);
                if p.cell as usize >= n_cell {
                    self.pop();
                }
            }
        }
        self.at_eof = self.first().is_none();
        self.cur_rowid = match self.current()? {
            Some((node, cell)) => read_i64(&node[4 + bpc * cell..]),
            None => 0,
        };
        Ok(())
    }

    /// The node and cell index of the current row.
    fn current(&mut self) -> Result<Option<(Arc<Vec<u8>>, usize)>> {
        let Some(p) = self.first() else {
            return Ok(None);
        };
        let node = self.node(p.id)?;
        if p.cell as usize >= read_u16(&node[2..]) {
            return Err(LimboError::SqlError("query aborted".to_string()));
        }
        Ok(Some((node, p.cell as usize)))
    }

    fn filter_inner(&mut self, args: &[Value], idx_str: Option<&str>, idx_num: i32) -> Result<()> {
        self.reset();
        self.node_size = self.t.node_size(&self.stmts.conn)?;
        if idx_num == 1 {
            // A rowid lookup.
            let arg = &args[0];
            let rowid = value_int64(arg);
            let found = match value_numeric(arg) {
                Value::Numeric(Numeric::Integer(_)) => true,
                Value::Numeric(Numeric::Float(f)) => int_equals_float(rowid, f64::from(f)),
                _ => false,
            };
            let leaf = if found {
                self.stmts
                    .query_row(
                        sql::READ_ROWID,
                        self.t.sql(sql::READ_ROWID),
                        &[Value::from_i64(rowid)],
                    )?
                    .and_then(|r| r.into_iter().next())
                    .map(|v| value_int64(&v))
            } else {
                None
            };
            if let Some(node_id) = leaf {
                let node = self.node(node_id)?;
                let bpc = self.t.bytes_per_cell;
                let n = read_u16(&node[2..]);
                let cell = (0..n)
                    .find(|&i| read_i64(&node[4 + bpc * i..]) == rowid)
                    .ok_or_else(corrupt)?;
                let r = self.search_point_new(0.0, 0);
                let p = self.point_mut(r);
                p.id = node_id;
                p.within = PARTLY_WITHIN as u8;
                p.cell = cell as u8;
                self.cur_rowid = rowid;
            } else {
                self.at_eof = true;
            }
            return Ok(());
        }
        let root = self.node(1)?;
        self.depth = read_u16(&root) as i32;
        if self.t.kind == RtreeKind::Geopoly {
            if idx_num <= 3 {
                self.constraints = super::geopoly::filter_constraints(idx_num, &args[0])?;
            }
        } else if !args.is_empty() {
            let idx = idx_str.unwrap_or("").as_bytes();
            if idx.len() != args.len() * 2 {
                return Err(LimboError::InternalError("rtree: bad idxStr".into()));
            }
            for (ii, arg) in args.iter().enumerate() {
                let mut c = Constraint {
                    op: idx[ii * 2],
                    coord: (idx[ii * 2 + 1] as i32 - b'0' as i32).max(0) as usize,
                    value: 0.0,
                    matcher: None,
                };
                if c.op >= RTREE_MATCH {
                    let m = decode_match_arg(arg, self.t.n_dim2, self.depth + 1)?;
                    if m.is_query() {
                        c.op = RTREE_QUERY;
                    }
                    c.matcher = Some(m);
                } else {
                    match value_numeric(arg) {
                        Value::Numeric(Numeric::Integer(i)) => {
                            c.value = i as f64;
                            if i >= 1i64 << 48 || i <= -(1i64 << 48) {
                                if c.op == RTREE_LT {
                                    c.op = RTREE_LE;
                                }
                                if c.op == RTREE_GT {
                                    c.op = RTREE_GE;
                                }
                            }
                        }
                        Value::Numeric(Numeric::Float(f)) => c.value = f64::from(f),
                        Value::Null => c.op = RTREE_FALSE,
                        _ => {
                            c.op = if c.op == RTREE_LT || c.op == RTREE_LE {
                                RTREE_TRUE
                            } else {
                                RTREE_FALSE
                            }
                        }
                    }
                }
                self.constraints.push(c);
            }
        }
        let r = self.search_point_new(0.0, (self.depth + 1) as u8);
        let p = self.point_mut(r);
        p.id = 1;
        p.cell = 0;
        p.within = PARTLY_WITHIN as u8;
        self.step_to_leaf()
    }

    fn column_inner(&mut self, i: usize) -> Result<Value> {
        let Some((node, cell)) = self.current()? else {
            return Ok(Value::Null);
        };
        let t = self.t.clone();
        let base = 4 + t.bytes_per_cell * cell;
        let rowid = read_i64(&node[base..]);
        let geopoly = t.kind == RtreeKind::Geopoly;
        if !geopoly && i == 0 {
            return Ok(Value::from_i64(rowid));
        }
        if !geopoly && i <= t.n_dim2 {
            let u = read_u32(&node[base + 8 + 4 * (i - 1)..]);
            return Ok(if t.int_coords() {
                Value::from_i64(u as i32 as i64)
            } else {
                Value::from_f64(f32::from_bits(u) as f64)
            });
        }
        if geopoly && i > t.n_aux {
            return Ok(Value::Null);
        }
        if self.aux.is_none() {
            let row = self.stmts.query_row(
                sql::READ_AUX,
                t.sql(sql::READ_AUX),
                &[Value::from_i64(rowid)],
            )?;
            match row {
                Some(row) => self.aux = Some(row),
                None => return Ok(Value::Null),
            }
        }
        let col = if geopoly { i + 2 } else { i - t.n_dim2 + 1 };
        Ok(self
            .aux
            .as_ref()
            .and_then(|r| r.get(col).cloned())
            .unwrap_or(Value::Null))
    }
}

#[derive(Clone, Copy)]
enum PointRef {
    Point,
    Queue(usize),
}

/// Checks a MATCH constraint against a cell.
#[allow(clippy::too_many_arguments)]
fn callback_constraint(
    c: &mut Constraint,
    int: bool,
    cell: &[u8],
    p: &SearchPoint,
    max_level: i32,
    an_queue: &[u32],
    score: &mut f64,
    within: &mut i32,
) -> Result<()> {
    let m = c.matcher.as_mut().expect("MATCH constraint has a callback");
    let n_coord = m.n_coord;
    let mut coords = [0.0f64; 10];
    for (k, coord) in coords.iter_mut().enumerate().take(n_coord) {
        *coord = decode_coord(int, &cell[8 + 4 * k..]);
    }
    if c.op == RTREE_QUERY && p.level == 1 {
        m.rowid = read_i64(cell);
    }
    if c.op == RTREE_MATCH {
        if !m.call.geometry(&coords[..n_coord])? {
            *within = NOT_WITHIN;
        }
        *score = 0.0;
    } else {
        let mut info = super::RtreeQueryInfo {
            coords: &coords[..n_coord],
            level: p.level as i32 - 1,
            max_level,
            rowid: m.rowid,
            parent_score: p.score,
            parent_within: p.within as i32,
            score: p.score,
            within: p.within as i32,
            queue: an_queue,
        };
        m.call.query(&mut info)?;
        if info.within < *within {
            *within = info.within;
        }
        if info.score < *score || *score < 0.0 {
            *score = info.score;
        }
    }
    Ok(())
}

/// Checks a constraint against a cell of an interior node.
fn nonleaf_constraint(c: &Constraint, int: bool, cell: &[u8], within: &mut i32) {
    let p = &cell[8 + 4 * (c.coord & 0xfe)..];
    match c.op {
        RTREE_TRUE => return,
        RTREE_FALSE => {}
        RTREE_EQ => {
            let lo = decode_coord(int, p);
            if c.value >= lo {
                let hi = decode_coord(int, &p[4..]);
                if c.value <= hi {
                    return;
                }
            }
        }
        RTREE_LE | RTREE_LT => {
            if c.value >= decode_coord(int, p) {
                return;
            }
        }
        _ => {
            if c.value <= decode_coord(int, &p[4..]) {
                return;
            }
        }
    }
    *within = NOT_WITHIN;
}

/// Checks a constraint against a leaf cell.
fn leaf_constraint(c: &Constraint, int: bool, cell: &[u8], within: &mut i32) {
    let x = decode_coord(int, &cell[8 + c.coord * 4..]);
    let ok = match c.op {
        RTREE_TRUE => true,
        RTREE_FALSE => false,
        RTREE_LE => x <= c.value,
        RTREE_LT => x < c.value,
        RTREE_GE => x >= c.value,
        RTREE_GT => x > c.value,
        _ => x == c.value,
    };
    if !ok {
        *within = NOT_WITHIN;
    }
}

impl VirtualTableCursor for RtreeCursor {
    fn filter(&mut self, args: &[Value], idx_str: Option<&str>, idx_num: i32) -> IOResultOr<bool> {
        self.filter_inner(args, idx_str, idx_num)?;
        Ok(IOResult::Done(!self.at_eof))
    }

    fn next(&mut self) -> IOResultOr<bool> {
        self.aux = None;
        self.pop();
        self.step_to_leaf()?;
        Ok(IOResult::Done(!self.at_eof))
    }

    fn column(&mut self, column: usize) -> IOResultOr<Value> {
        Ok(IOResult::Done(self.column_inner(column)?))
    }

    fn rowid(&self) -> i64 {
        self.cur_rowid
    }
}
