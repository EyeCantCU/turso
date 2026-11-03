//! R*Tree writes, ported from rtree.c: ChooseLeaf, AdjustTree, the R* split
//! (splitNodeStartree) and CondenseTree with reinsertion.
//!
//! Nodes are reference counted as in rtree.c, and a node is written back to
//! `%_node` when its last reference is released.

use super::cursor::{corrupt, read_i64, read_node, read_u16, read_u32};
use super::shadow::Stmts;
use super::{sql, value_double, value_int64, Rtree, RtreeKind};
use crate::sync::Arc;
use crate::{Connection, LimboError, Result, Value};
use std::collections::HashMap;

/// A deserialized cell: a rowid or child node number, and raw coordinates.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Cell {
    pub(crate) rowid: i64,
    pub(crate) coord: [u32; 10],
}

fn f(c: u32) -> f32 {
    f32::from_bits(c)
}

fn i(c: u32) -> i32 {
    c as i32
}

struct Node {
    parent: Option<usize>,
    inode: i64,
    nref: i32,
    dirty: bool,
    data: Vec<u8>,
}

pub(crate) struct Writer<'a> {
    t: &'a Rtree,
    pub(crate) stmts: Stmts<{ sql::COUNT }>,
    node_size: usize,
    nodes: Vec<Option<Node>>,
    free: Vec<usize>,
    hash: HashMap<i64, usize>,
    depth: i32,
    deleted: Vec<usize>,
}

impl<'a> Writer<'a> {
    pub(crate) fn new(t: &'a Rtree, conn: &Arc<Connection>) -> Result<Self> {
        let node_size = t.node_size(conn)?;
        Ok(Self {
            t,
            stmts: Stmts::new(conn.clone()),
            node_size,
            nodes: Vec::new(),
            free: Vec::new(),
            hash: HashMap::new(),
            depth: -1,
            deleted: Vec::new(),
        })
    }

    fn n(&self, h: usize) -> &Node {
        self.nodes[h].as_ref().expect("live node")
    }

    fn nm(&mut self, h: usize) -> &mut Node {
        self.nodes[h].as_mut().expect("live node")
    }

    fn alloc(&mut self, node: Node) -> usize {
        if let Some(h) = self.free.pop() {
            self.nodes[h] = Some(node);
            h
        } else {
            self.nodes.push(Some(node));
            self.nodes.len() - 1
        }
    }

    fn ncell(&self, h: usize) -> usize {
        read_u16(&self.n(h).data[2..])
    }

    fn max_cells(&self) -> usize {
        (self.node_size - 4) / self.t.bytes_per_cell
    }

    fn min_cells(&self) -> usize {
        self.t.min_cells(self.node_size)
    }

    fn reference(&mut self, h: Option<usize>) {
        if let Some(h) = h {
            self.nm(h).nref += 1;
        }
    }

    fn node_new(&mut self, parent: Option<usize>) -> usize {
        let node = Node {
            parent,
            inode: 0,
            nref: 1,
            dirty: true,
            data: vec![0; self.node_size],
        };
        self.reference(parent);
        self.alloc(node)
    }

    fn node_acquire(&mut self, inode: i64, parent: Option<usize>) -> Result<usize> {
        if let Some(&h) = self.hash.get(&inode) {
            if parent.is_some() && parent != self.n(h).parent {
                return Err(corrupt());
            }
            self.nm(h).nref += 1;
            return Ok(h);
        }
        let data = read_node(self.t, &mut self.stmts, self.node_size, inode)?;
        if inode == 1 {
            self.depth = read_u16(&data) as i32;
        }
        self.reference(parent);
        let h = self.alloc(Node {
            parent,
            inode,
            nref: 1,
            dirty: false,
            data,
        });
        self.hash.insert(inode, h);
        Ok(h)
    }

    fn node_write(&mut self, h: usize) -> Result<()> {
        if !self.n(h).dirty {
            return Ok(());
        }
        let inode = self.n(h).inode;
        let id = if inode != 0 {
            Value::from_i64(inode)
        } else {
            Value::Null
        };
        let data = Value::from_slice(&self.n(h).data)?;
        self.stmts
            .execute(sql::WRITE_NODE, self.t.sql(sql::WRITE_NODE), &[id, data])?;
        self.nm(h).dirty = false;
        if inode == 0 {
            let new = self.stmts.conn.last_insert_rowid();
            if new == 0 {
                return Err(corrupt());
            }
            self.nm(h).inode = new;
            self.hash.insert(new, h);
        }
        Ok(())
    }

    fn node_release(&mut self, h: Option<usize>) -> Result<()> {
        let Some(h) = h else {
            return Ok(());
        };
        self.nm(h).nref -= 1;
        if self.n(h).nref == 0 {
            if self.n(h).inode == 1 {
                self.depth = -1;
            }
            let parent = self.n(h).parent;
            self.node_release(parent)?;
            self.node_write(h)?;
            let inode = self.n(h).inode;
            if inode != 0 && self.hash.get(&inode) == Some(&h) {
                self.hash.remove(&inode);
            }
            self.nodes[h] = None;
            self.free.push(h);
        }
        Ok(())
    }

    fn get_rowid(&self, h: usize, cell: usize) -> i64 {
        read_i64(&self.n(h).data[4 + self.t.bytes_per_cell * cell..])
    }

    fn get_cell(&self, h: usize, cell: usize) -> Cell {
        let base = 4 + self.t.bytes_per_cell * cell;
        let data = &self.n(h).data;
        let mut c = Cell {
            rowid: read_i64(&data[base..]),
            ..Default::default()
        };
        for k in 0..self.t.n_dim2 {
            c.coord[k] = read_u32(&data[base + 8 + 4 * k..]);
        }
        c
    }

    fn overwrite_cell(&mut self, h: usize, cell: &Cell, idx: usize) {
        let base = 4 + self.t.bytes_per_cell * idx;
        let n_dim2 = self.t.n_dim2;
        let node = self.nm(h);
        node.data[base..base + 8].copy_from_slice(&cell.rowid.to_be_bytes());
        for k in 0..n_dim2 {
            node.data[base + 8 + 4 * k..base + 12 + 4 * k]
                .copy_from_slice(&cell.coord[k].to_be_bytes());
        }
        node.dirty = true;
    }

    fn delete_cell_bytes(&mut self, h: usize, idx: usize) {
        let bpc = self.t.bytes_per_cell;
        let n = self.ncell(h);
        let node = self.nm(h);
        let dst = 4 + bpc * idx;
        node.data.copy_within(dst + bpc..4 + bpc * n, dst);
        node.data[2..4].copy_from_slice(&((n - 1) as u16).to_be_bytes());
        node.dirty = true;
    }

    /// Appends a cell; returns true when the node was already full.
    fn insert_cell_bytes(&mut self, h: usize, cell: &Cell) -> bool {
        let n = self.ncell(h);
        let max = self.max_cells();
        if n < max {
            self.overwrite_cell(h, cell, n);
            let node = self.nm(h);
            node.data[2..4].copy_from_slice(&((n + 1) as u16).to_be_bytes());
            node.dirty = true;
        }
        n == max
    }

    fn rowid_index(&self, h: usize, rowid: i64) -> Result<usize> {
        (0..self.ncell(h))
            .find(|&c| self.get_rowid(h, c) == rowid)
            .ok_or_else(corrupt)
    }

    fn parent_index(&self, h: usize) -> Result<Option<usize>> {
        match self.n(h).parent {
            Some(p) => Ok(Some(self.rowid_index(p, self.n(h).inode)?)),
            None => Ok(None),
        }
    }

    fn dcoord(&self, c: u32) -> f64 {
        if self.t.int_coords() {
            i(c) as f64
        } else {
            f(c) as f64
        }
    }

    fn cell_area(&self, p: &Cell) -> f64 {
        let mut area = 1.0f64;
        for d in (0..self.t.n_dim).rev() {
            let diff = if self.t.int_coords() {
                (i(p.coord[2 * d + 1]) as i64 - i(p.coord[2 * d]) as i64) as f64
            } else {
                (f(p.coord[2 * d + 1]) - f(p.coord[2 * d])) as f64
            };
            area *= diff;
        }
        area
    }

    fn cell_margin(&self, p: &Cell) -> f64 {
        let mut margin = 0.0;
        let mut ii = self.t.n_dim2 as isize - 2;
        while ii >= 0 {
            let k = ii as usize;
            margin += self.dcoord(p.coord[k + 1]) - self.dcoord(p.coord[k]);
            ii -= 2;
        }
        margin
    }

    fn cell_union(&self, p1: &mut Cell, p2: &Cell) {
        for k in (0..self.t.n_dim2).step_by(2) {
            if self.t.int_coords() {
                let (a, b) = (i(p1.coord[k]), i(p2.coord[k]));
                p1.coord[k] = if a > b { b } else { a } as u32;
                let (a, b) = (i(p1.coord[k + 1]), i(p2.coord[k + 1]));
                p1.coord[k + 1] = if a < b { b } else { a } as u32;
            } else {
                let (a, b) = (f(p1.coord[k]), f(p2.coord[k]));
                p1.coord[k] = if a > b { b } else { a }.to_bits();
                let (a, b) = (f(p1.coord[k + 1]), f(p2.coord[k + 1]));
                p1.coord[k + 1] = if a < b { b } else { a }.to_bits();
            }
        }
    }

    fn cell_contains(&self, p1: &Cell, p2: &Cell) -> bool {
        for k in (0..self.t.n_dim2).step_by(2) {
            if self.t.int_coords() {
                if i(p2.coord[k]) < i(p1.coord[k]) || i(p2.coord[k + 1]) > i(p1.coord[k + 1]) {
                    return false;
                }
            } else if f(p2.coord[k]) < f(p1.coord[k]) || f(p2.coord[k + 1]) > f(p1.coord[k + 1]) {
                return false;
            }
        }
        true
    }

    fn cell_overlap(&self, p: &Cell, cells: &[Cell]) -> f64 {
        let mut overlap = 0.0;
        for c in cells {
            let mut o = 1.0;
            for k in (0..self.t.n_dim2).step_by(2) {
                let (a, b) = (self.dcoord(p.coord[k]), self.dcoord(c.coord[k]));
                let x1 = if a < b { b } else { a };
                let (a, b) = (self.dcoord(p.coord[k + 1]), self.dcoord(c.coord[k + 1]));
                let x2 = if a > b { b } else { a };
                if x2 < x1 {
                    o = 0.0;
                    break;
                }
                o *= x2 - x1;
            }
            overlap += o;
        }
        overlap
    }

    /// ChooseSubTree: the node at height `height` to insert `cell` into.
    fn choose_leaf(&mut self, cell: &Cell, height: i32) -> Result<usize> {
        let mut node = self.node_acquire(1, None)?;
        let mut ii = 0;
        while ii < self.depth - height {
            let n = self.ncell(node);
            let mut best = 0i64;
            let mut found = false;
            let mut min_growth = 0.0;
            let mut min_area = 0.0;
            for c in 0..n {
                let cc = self.get_cell(node, c);
                if self.cell_contains(&cc, cell) {
                    let area = self.cell_area(&cc);
                    if !found || area < min_area {
                        best = cc.rowid;
                        min_area = area;
                        found = true;
                    }
                }
            }
            if !found {
                for c in 0..n {
                    let mut cc = self.get_cell(node, c);
                    let area = self.cell_area(&cc);
                    self.cell_union(&mut cc, cell);
                    let growth = self.cell_area(&cc) - area;
                    if c == 0 || growth < min_growth || (growth == min_growth && area < min_area) {
                        min_growth = growth;
                        min_area = area;
                        best = cc.rowid;
                    }
                }
            }
            let child = self.node_acquire(best, Some(node));
            self.node_release(Some(node))?;
            node = child?;
            ii += 1;
        }
        Ok(node)
    }

    /// Grows the ancestors' boxes to cover `cell`, just inserted into `node`.
    fn adjust_tree(&mut self, node: usize, cell: &Cell) -> Result<()> {
        let mut p = node;
        let mut cnt = 0;
        while let Some(parent) = self.n(p).parent {
            cnt += 1;
            if cnt > 100 {
                return Err(corrupt());
            }
            let idx = self.parent_index(p)?.ok_or_else(corrupt)?;
            let mut c = self.get_cell(parent, idx);
            if !self.cell_contains(&c, cell) {
                self.cell_union(&mut c, cell);
                self.overwrite_cell(parent, &c, idx);
            }
            p = parent;
        }
        Ok(())
    }

    fn rowid_write(&mut self, rowid: i64, node: i64) -> Result<()> {
        self.stmts.execute(
            sql::WRITE_ROWID,
            self.t.sql(sql::WRITE_ROWID),
            &[Value::from_i64(rowid), Value::from_i64(node)],
        )
    }

    fn parent_write(&mut self, node: i64, parent: i64) -> Result<()> {
        self.stmts.execute(
            sql::WRITE_PARENT,
            self.t.sql(sql::WRITE_PARENT),
            &[Value::from_i64(node), Value::from_i64(parent)],
        )
    }

    fn sort_by_dimension(
        &self,
        idx: &mut [usize],
        dim: usize,
        cells: &[Cell],
        spare: &mut [usize],
    ) {
        let n = idx.len();
        if n <= 1 {
            return;
        }
        let n_left = n / 2;
        let n_right = n - n_left;
        {
            let (l, r) = idx.split_at_mut(n_left);
            self.sort_by_dimension(l, dim, cells, spare);
            self.sort_by_dimension(r, dim, cells, spare);
        }
        spare[..n_left].copy_from_slice(&idx[..n_left]);
        let (mut il, mut ir) = (0, 0);
        while il < n_left || ir < n_right {
            let take_left = il != n_left && {
                if ir == n_right {
                    true
                } else {
                    let l = &cells[spare[il]];
                    let r = &cells[idx[n_left + ir]];
                    let (l1, l2) = (
                        self.dcoord(l.coord[dim * 2]),
                        self.dcoord(l.coord[dim * 2 + 1]),
                    );
                    let (r1, r2) = (
                        self.dcoord(r.coord[dim * 2]),
                        self.dcoord(r.coord[dim * 2 + 1]),
                    );
                    l1 < r1 || (l1 == r1 && l2 < r2)
                }
            };
            if take_left {
                idx[il + ir] = spare[il];
                il += 1;
            } else {
                idx[il + ir] = idx[n_left + ir];
                ir += 1;
            }
        }
    }

    /// The R*-tree split of Beckmann [1990].
    fn split_startree(&mut self, cells: &[Cell], left: usize, right: usize) -> (Cell, Cell) {
        let n_cell = cells.len();
        let n_dim = self.t.n_dim;
        let min = self.min_cells();
        let mut sorted: Vec<Vec<usize>> = Vec::with_capacity(n_dim);
        let mut spare = vec![0usize; n_cell];
        for d in 0..n_dim {
            let mut v: Vec<usize> = (0..n_cell).collect();
            self.sort_by_dimension(&mut v, d, cells, &mut spare);
            sorted.push(v);
        }
        let mut best_dim = 0;
        let mut best_split = 0;
        let mut best_margin = 0.0;
        for (d, s) in sorted.iter().enumerate() {
            let mut margin = 0.0;
            let mut best_overlap = 0.0;
            let mut best_area = 0.0;
            let mut best_left = 0;
            let mut n_left = min;
            while n_left + min <= n_cell {
                let mut l = cells[s[0]];
                let mut r = cells[s[n_cell - 1]];
                for (kk, &ci) in s.iter().enumerate().take(n_cell - 1).skip(1) {
                    if kk < n_left {
                        self.cell_union(&mut l, &cells[ci]);
                    } else {
                        self.cell_union(&mut r, &cells[ci]);
                    }
                }
                margin += self.cell_margin(&l);
                margin += self.cell_margin(&r);
                let overlap = self.cell_overlap(&l, std::slice::from_ref(&r));
                let area = self.cell_area(&l) + self.cell_area(&r);
                if n_left == min
                    || overlap < best_overlap
                    || (overlap == best_overlap && area < best_area)
                {
                    best_left = n_left;
                    best_overlap = overlap;
                    best_area = area;
                }
                n_left += 1;
            }
            if d == 0 || margin < best_margin {
                best_dim = d;
                best_margin = margin;
                best_split = best_left;
            }
        }
        let s = &sorted[best_dim];
        let mut bbox_left = cells[s[0]];
        let mut bbox_right = cells[s[best_split]];
        for (ii, &ci) in s.iter().enumerate() {
            let c = &cells[ci];
            if ii < best_split {
                self.insert_cell_bytes(left, c);
                self.cell_union(&mut bbox_left, c);
            } else {
                self.insert_cell_bytes(right, c);
                self.cell_union(&mut bbox_right, c);
            }
        }
        (bbox_left, bbox_right)
    }

    fn update_mapping(&mut self, rowid: i64, node: usize, height: i32) -> Result<()> {
        if height > 0 {
            let child = self.hash.get(&rowid).copied();
            let mut p = Some(node);
            while let Some(h) = p {
                if Some(h) == child {
                    return Err(corrupt());
                }
                p = self.n(h).parent;
            }
            if let Some(child) = child {
                let old = self.n(child).parent;
                self.node_release(old)?;
                self.reference(Some(node));
                self.nm(child).parent = Some(node);
            }
        }
        let inode = self.n(node).inode;
        if height == 0 {
            self.rowid_write(rowid, inode)
        } else {
            self.parent_write(rowid, inode)
        }
    }

    fn split_node(&mut self, node: usize, cell: &Cell, height: i32) -> Result<()> {
        let n = self.ncell(node);
        let mut cells: Vec<Cell> = (0..n).map(|c| self.get_cell(node, c)).collect();
        {
            let node_size = self.node_size;
            let nd = self.nm(node);
            nd.data[2..node_size].fill(0);
            nd.dirty = true;
        }
        cells.push(*cell);
        let is_root = self.n(node).inode == 1;
        let (left, right) = if is_root {
            let right = self.node_new(Some(node));
            let left = self.node_new(Some(node));
            self.depth += 1;
            let depth = self.depth as u16;
            let nd = self.nm(node);
            nd.dirty = true;
            nd.data[0..2].copy_from_slice(&depth.to_be_bytes());
            (left, right)
        } else {
            let parent = self.n(node).parent;
            let right = self.node_new(parent);
            self.nm(node).nref += 1;
            (node, right)
        };
        let result = self.split_node_inner(node, cell, height, &cells, left, right, is_root);
        let r1 = self.node_release(Some(right));
        let r2 = self.node_release(Some(left));
        result.and(r1).and(r2)
    }

    #[allow(clippy::too_many_arguments)]
    fn split_node_inner(
        &mut self,
        node: usize,
        cell: &Cell,
        height: i32,
        cells: &[Cell],
        left: usize,
        right: usize,
        is_root: bool,
    ) -> Result<()> {
        self.nm(left).data.fill(0);
        self.nm(right).data.fill(0);
        let (mut leftbbox, mut rightbbox) = self.split_startree(cells, left, right);
        self.node_write(right)?;
        if self.n(left).inode == 0 {
            self.node_write(left)?;
        }
        rightbbox.rowid = self.n(right).inode;
        leftbbox.rowid = self.n(left).inode;
        if is_root {
            let parent = self.n(left).parent.ok_or_else(corrupt)?;
            self.insert_cell(parent, &leftbbox, height + 1)?;
        } else {
            let parent = self.n(left).parent.ok_or_else(corrupt)?;
            let idx = self.parent_index(left)?.ok_or_else(corrupt)?;
            self.overwrite_cell(parent, &leftbbox, idx);
            self.adjust_tree(parent, &leftbbox)?;
        }
        let rparent = self.n(right).parent.ok_or_else(corrupt)?;
        self.insert_cell(rparent, &rightbbox, height + 1)?;
        let mut new_cell_is_right = false;
        let mut c = 0;
        while c < self.ncell(right) {
            let rowid = self.get_rowid(right, c);
            self.update_mapping(rowid, right, height)?;
            if rowid == cell.rowid {
                new_cell_is_right = true;
            }
            c += 1;
        }
        if self.n(node).inode == 1 {
            let mut c = 0;
            while c < self.ncell(left) {
                let rowid = self.get_rowid(left, c);
                self.update_mapping(rowid, left, height)?;
                c += 1;
            }
        } else if !new_cell_is_right {
            self.update_mapping(cell.rowid, left, height)?;
        }
        Ok(())
    }

    /// Loads the ancestors of `leaf` and links its parent chain.
    fn fix_leaf_parent(&mut self, leaf: usize) -> Result<()> {
        let mut child = leaf;
        while self.n(child).inode != 1 && self.n(child).parent.is_none() {
            let inode = self.n(child).inode;
            let row = self.stmts.query_row(
                sql::READ_PARENT,
                self.t.sql(sql::READ_PARENT),
                &[Value::from_i64(inode)],
            )?;
            if let Some(v) = row.and_then(|r| r.into_iter().next()) {
                let parent_no = value_int64(&v);
                // Refuse to create a reference loop.
                let mut test = Some(leaf);
                while let Some(h) = test {
                    if self.n(h).inode == parent_no {
                        break;
                    }
                    test = self.n(h).parent;
                }
                if test.is_none() {
                    let p = self.node_acquire(parent_no, None)?;
                    self.nm(child).parent = Some(p);
                }
            }
            match self.n(child).parent {
                Some(p) => child = p,
                None => return Err(corrupt()),
            }
        }
        Ok(())
    }

    fn remove_node(&mut self, node: usize, height: i32) -> Result<()> {
        let idx = self.parent_index(node)?.ok_or_else(corrupt)?;
        let parent = self.n(node).parent;
        self.nm(node).parent = None;
        let r = self.delete_cell(parent.expect("has parent"), idx, height + 1);
        let r2 = self.node_release(parent);
        r?;
        r2?;
        let inode = self.n(node).inode;
        self.stmts.execute(
            sql::DELETE_NODE,
            self.t.sql(sql::DELETE_NODE),
            &[Value::from_i64(inode)],
        )?;
        self.stmts.execute(
            sql::DELETE_PARENT,
            self.t.sql(sql::DELETE_PARENT),
            &[Value::from_i64(inode)],
        )?;
        if inode != 0 && self.hash.get(&inode) == Some(&node) {
            self.hash.remove(&inode);
        }
        // The node is reinserted later; its number now holds its height.
        let nd = self.nm(node);
        nd.inode = height as i64;
        nd.nref += 1;
        self.deleted.push(node);
        Ok(())
    }

    fn fix_bounding_box(&mut self, node: usize) -> Result<()> {
        let Some(parent) = self.n(node).parent else {
            return Ok(());
        };
        let n = self.ncell(node);
        let mut bbox = self.get_cell(node, 0);
        for c in 1..n {
            let cc = self.get_cell(node, c);
            self.cell_union(&mut bbox, &cc);
        }
        bbox.rowid = self.n(node).inode;
        let idx = self.parent_index(node)?.ok_or_else(corrupt)?;
        self.overwrite_cell(parent, &bbox, idx);
        self.fix_bounding_box(parent)
    }

    fn delete_cell(&mut self, node: usize, idx: usize, height: i32) -> Result<()> {
        self.fix_leaf_parent(node)?;
        self.delete_cell_bytes(node, idx);
        if self.n(node).parent.is_some() {
            if self.ncell(node) < self.min_cells() {
                self.remove_node(node, height)?;
            } else {
                self.fix_bounding_box(node)?;
            }
        }
        Ok(())
    }

    fn insert_cell(&mut self, node: usize, cell: &Cell, height: i32) -> Result<()> {
        if height > 0 {
            if let Some(&child) = self.hash.get(&cell.rowid) {
                let old = self.n(child).parent;
                self.node_release(old)?;
                self.reference(Some(node));
                self.nm(child).parent = Some(node);
            }
        }
        if self.insert_cell_bytes(node, cell) {
            self.split_node(node, cell, height)
        } else {
            self.adjust_tree(node, cell)?;
            let inode = self.n(node).inode;
            if height == 0 {
                self.rowid_write(cell.rowid, inode)
            } else {
                self.parent_write(cell.rowid, inode)
            }
        }
    }

    fn reinsert_node_content(&mut self, node: usize) -> Result<()> {
        let height = self.n(node).inode as i32;
        for c in 0..self.ncell(node) {
            let cell = self.get_cell(node, c);
            let insert = self.choose_leaf(&cell, height)?;
            let r = self.insert_cell(insert, &cell, height);
            let r2 = self.node_release(Some(insert));
            r?;
            r2?;
        }
        Ok(())
    }

    fn new_rowid(&mut self) -> Result<i64> {
        self.stmts.execute(
            sql::WRITE_ROWID,
            self.t.sql(sql::WRITE_ROWID),
            &[Value::Null, Value::Null],
        )?;
        Ok(self.stmts.conn.last_insert_rowid())
    }

    /// Whether `rowid` is in the table.
    pub(crate) fn rowid_exists(&mut self, rowid: i64) -> Result<bool> {
        Ok(self
            .stmts
            .query_row(
                sql::READ_ROWID,
                self.t.sql(sql::READ_ROWID),
                &[Value::from_i64(rowid)],
            )?
            .is_some())
    }

    /// `rtreeDeleteRowid`.
    pub(crate) fn delete_rowid(&mut self, rowid: i64) -> Result<()> {
        let root = self.node_acquire(1, None)?;
        let leaf = self
            .stmts
            .query_row(
                sql::READ_ROWID,
                self.t.sql(sql::READ_ROWID),
                &[Value::from_i64(rowid)],
            )?
            .and_then(|r| r.into_iter().next())
            .map(|v| value_int64(&v));
        if let Some(leaf_no) = leaf {
            let leaf = self.node_acquire(leaf_no, None)?;
            let r = self
                .rowid_index(leaf, rowid)
                .and_then(|idx| self.delete_cell(leaf, idx, 0));
            let r2 = self.node_release(Some(leaf));
            r?;
            r2?;
        }
        self.stmts.execute(
            sql::DELETE_ROWID,
            self.t.sql(sql::DELETE_ROWID),
            &[Value::from_i64(rowid)],
        )?;
        // A root with a single child: pull the child's contents up.
        if self.depth > 0 && self.ncell(root) == 1 {
            let child_no = self.get_rowid(root, 0);
            let child = self.node_acquire(child_no, Some(root))?;
            let depth = self.depth;
            let r = self.remove_node(child, depth - 1);
            let r2 = self.node_release(Some(child));
            r?;
            r2?;
            self.depth -= 1;
            let depth = self.depth as u16;
            let nd = self.nm(root);
            nd.data[0..2].copy_from_slice(&depth.to_be_bytes());
            nd.dirty = true;
        }
        while let Some(node) = self.deleted.pop() {
            self.reinsert_node_content(node)?;
            self.nodes[node] = None;
            self.free.push(node);
        }
        self.node_release(Some(root))
    }

    /// Inserts `cell` as a new leaf entry, choosing a rowid when it has none.
    pub(crate) fn insert(&mut self, cell: &mut Cell, have_rowid: bool) -> Result<i64> {
        if !have_rowid {
            cell.rowid = self.new_rowid()?;
        }
        let leaf = self.choose_leaf(cell, 0)?;
        let r = self.insert_cell(leaf, cell, 0);
        let r2 = self.node_release(Some(leaf));
        r?;
        r2?;
        Ok(cell.rowid)
    }
}

/// Rounds toward negative infinity to a 32-bit float.
pub(crate) fn round_down(d: f64) -> f32 {
    let mut f = d as f32;
    if (f as f64) > d {
        let x = f.to_bits();
        f = f32::from_bits(if x & 0x8000_0000 != 0 { x + 1 } else { x - 1 });
    }
    f
}

/// Rounds toward positive infinity to a 32-bit float.
pub(crate) fn round_up(d: f64) -> f32 {
    let mut f = d as f32;
    if (f as f64) < d {
        let x = f.to_bits();
        f = f32::from_bits(if x & 0x8000_0000 != 0 { x - 1 } else { x + 1 });
    }
    f
}

fn value_int32(v: &Value) -> i32 {
    value_int64(v).clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

const ON_CONFLICT_IGNORE: u8 = 4;
const ON_CONFLICT_REPLACE: u8 = 5;

/// xUpdate of rtree.c. `args` is turso's layout: old rowid, new rowid,
/// then the columns; a DELETE has just the old rowid and NULL.
pub(crate) fn update(
    t: &Rtree,
    conn: &Arc<Connection>,
    args: &[Value],
    on_conflict: u8,
) -> Result<Option<i64>> {
    debug_assert!(t.kind != RtreeKind::Geopoly);
    let n_data = if args.len() == 2 { 1 } else { args.len() };
    let mut w = Writer::new(t, conn)?;
    let mut cell = Cell::default();
    let mut have_rowid = false;
    let result = (|| -> Result<Option<i64>> {
        if n_data > 1 {
            let nn = (n_data - 4).min(t.n_dim2);
            let mut ii = 0;
            while ii < nn {
                if t.int_coords() {
                    cell.coord[ii] = value_int32(&args[ii + 3]) as u32;
                    cell.coord[ii + 1] = value_int32(&args[ii + 4]) as u32;
                    if (cell.coord[ii] as i32) > (cell.coord[ii + 1] as i32) {
                        return Err(t.constraint_error(ii + 1));
                    }
                } else {
                    let lo = round_down(value_double(&args[ii + 3]));
                    let hi = round_up(value_double(&args[ii + 4]));
                    cell.coord[ii] = lo.to_bits();
                    cell.coord[ii + 1] = hi.to_bits();
                    if lo > hi {
                        return Err(t.constraint_error(ii + 1));
                    }
                }
                ii += 2;
            }
            if !matches!(args[2], Value::Null) {
                cell.rowid = value_int64(&args[2]);
                if matches!(args[0], Value::Null) || value_int64(&args[0]) != cell.rowid {
                    if w.rowid_exists(cell.rowid)? {
                        if on_conflict == ON_CONFLICT_REPLACE {
                            w.delete_rowid(cell.rowid)?;
                        } else {
                            return Err(t.constraint_error(0));
                        }
                    }
                }
                have_rowid = true;
            }
        }
        if !matches!(args[0], Value::Null) {
            w.delete_rowid(value_int64(&args[0]))?;
        }
        if n_data > 1 {
            let rowid = w.insert(&mut cell, have_rowid)?;
            if t.n_aux > 0 {
                let mut params = vec![Value::from_i64(rowid)];
                for jj in 0..t.n_aux {
                    params.push(args[t.n_dim2 + 3 + jj].clone());
                }
                w.stmts
                    .execute(sql::WRITE_AUX, t.sql(sql::WRITE_AUX), &params)?;
            }
            return Ok(Some(rowid));
        }
        Ok(None)
    })();
    match result {
        // With OR IGNORE, SQLite skips a row that fails a constraint.
        Err(LimboError::Constraint(_)) if on_conflict == ON_CONFLICT_IGNORE => Ok(None),
        r => r,
    }
}

/// The error SQLite reports for a write that would disturb an open scan.
#[allow(dead_code)]
pub(crate) fn locked() -> LimboError {
    LimboError::TableLocked
}
