//! Auto-vacuum (btree.c): pointer-map upkeep, page relocation and the
//! commit-time / incremental vacuum that shrinks the file.
//!
//! The ptrmap is not updated by every btree mutation. Allocation, free and
//! root creation record what they know, and `av_fixup` brings the entries
//! of every page touched by the transaction up to date from the pages'
//! contents before anything relies on them (relocation, commit).

use super::*;
use crate::storage::btree::PinGuard;
use std::collections::{HashSet, VecDeque};

pub(crate) const PTRMAP_ROOTPAGE: u8 = 1;
pub(crate) const PTRMAP_FREEPAGE: u8 = 2;
pub(crate) const PTRMAP_OVERFLOW1: u8 = 3;
pub(crate) const PTRMAP_OVERFLOW2: u8 = 4;
pub(crate) const PTRMAP_BTREE: u8 = 5;

/// Pointer-map geometry of one database (usable size and pending-byte page).
#[derive(Debug, Clone, Copy)]
pub struct Geom {
    pub usable: u32,
    pub pending: u32,
}

impl Geom {
    /// ptrmapPageno: the ptrmap page holding the entry for `pgno`.
    pub fn ptrmap_pageno(&self, pgno: u32) -> u32 {
        if pgno < 2 {
            return 0;
        }
        let per = self.usable / 5 + 1;
        let ret = ((pgno - 2) / per) * per + 2;
        if ret == self.pending {
            ret + 1
        } else {
            ret
        }
    }

    pub fn is_ptrmap(&self, pgno: u32) -> bool {
        pgno >= 2 && self.ptrmap_pageno(pgno) == pgno
    }

    /// Pages that never hold btree data.
    pub fn skip(&self, pgno: u32) -> bool {
        pgno == self.pending || self.is_ptrmap(pgno)
    }

    pub fn offset(&self, pgno: u32) -> usize {
        5 * (pgno - self.ptrmap_pageno(pgno) - 1) as usize
    }

    /// finalDbSize: database size once `n_free` free pages are vacuumed.
    pub fn final_db_size(&self, n_orig: u32, n_free: u32) -> u32 {
        let n_entry = (self.usable / 5) as i64;
        let n_ptrmap =
            (n_free as i64 - n_orig as i64 + self.ptrmap_pageno(n_orig) as i64 + n_entry) / n_entry;
        let mut n_fin = n_orig as i64 - n_free as i64 - n_ptrmap;
        if n_orig > self.pending && n_fin < self.pending as i64 {
            n_fin -= 1;
        }
        while n_fin > 1 && self.skip(n_fin as u32) {
            n_fin -= 1;
        }
        n_fin.max(1) as u32
    }
}

pub(crate) fn mode_from_header(h: &DatabaseHeader) -> AutoVacuumMode {
    if h.vacuum_mode_largest_root_page.get() == 0 {
        AutoVacuumMode::None
    } else if h.incremental_vacuum_enabled.get() != 0 {
        AutoVacuumMode::Incremental
    } else {
        AutoVacuumMode::Full
    }
}

fn read_entry(page: &PageRef, off: usize) -> (u8, u32) {
    let buf = page.get_contents().as_ptr();
    (buf[off], read_u32(buf, off + 1))
}

/// Pointers held by a btree page: children (BTREE) and first overflow
/// pages (OVERFLOW1), in the order setChildPtrmaps visits them.
fn btree_pointers(page: &PageRef, usable: usize) -> Result<Vec<(u32, u8)>> {
    let contents = page.get_contents();
    let page_type = match contents.page_type() {
        Ok(t) => t,
        Err(_) => {
            return Err(LimboError::Corrupt(format!(
                "page {} is not a btree page",
                contents.id()
            )))
        }
    };
    let interior = matches!(page_type, PageType::TableInterior | PageType::IndexInterior);
    let buf = contents.as_ptr();
    let mut out = Vec::new();
    for i in 0..contents.cell_count() {
        let (start, _len, ovfl) = contents.cell_get_raw_region_and_overflow(i, usable)?;
        if let Some(o) = ovfl {
            out.push((o, PTRMAP_OVERFLOW1));
        }
        if interior {
            if start + 4 > buf.len() {
                return Err(LimboError::Corrupt(format!(
                    "cell {i} out of bounds on page {}",
                    contents.id()
                )));
            }
            out.push((read_u32(buf, start), PTRMAP_BTREE));
        }
    }
    if interior {
        out.push((contents.read_u32(BTREE_RIGHTMOST_PTR), PTRMAP_BTREE));
    }
    Ok(out)
}

/// Pointers held by a page of the given ptrmap type.
fn page_pointers(page: &PageRef, ty: u8, usable: usize) -> Result<Vec<(u32, u8)>> {
    match ty {
        PTRMAP_ROOTPAGE | PTRMAP_BTREE => btree_pointers(page, usable),
        PTRMAP_OVERFLOW1 | PTRMAP_OVERFLOW2 => {
            let next = page.get_contents().read_u32_no_offset(0);
            Ok(if next != 0 {
                vec![(next, PTRMAP_OVERFLOW2)]
            } else {
                vec![]
            })
        }
        _ => Ok(vec![]),
    }
}

/// modifyPagePointer: make the pointer to `from` on `page` point to `to`.
fn modify_page_pointer(page: &PageRef, from: u32, to: u32, ty: u8, usable: usize) -> Result<()> {
    let contents = page.get_contents();
    let corrupt = || {
        LimboError::Corrupt(format!(
            "page {} holds no pointer to page {from}",
            contents.id()
        ))
    };
    if ty == PTRMAP_OVERFLOW2 {
        if contents.read_u32_no_offset(0) != from {
            return Err(corrupt());
        }
        contents.write_u32_no_offset(0, to);
        return Ok(());
    }
    let interior = matches!(
        contents.page_type().map_err(|_| corrupt())?,
        PageType::TableInterior | PageType::IndexInterior
    );
    for i in 0..contents.cell_count() {
        let (start, len, ovfl) = contents.cell_get_raw_region_and_overflow(i, usable)?;
        if ty == PTRMAP_OVERFLOW1 {
            if ovfl == Some(from) {
                contents.write_u32_no_offset(start + len - 4, to);
                return Ok(());
            }
        } else if interior && read_u32(contents.as_ptr(), start) == from {
            contents.write_u32_no_offset(start, to);
            return Ok(());
        }
    }
    if ty == PTRMAP_BTREE && interior && contents.read_u32(BTREE_RIGHTMOST_PTR) == from {
        contents.write_rightmost_ptr(to);
        return Ok(());
    }
    Err(corrupt())
}

#[derive(Debug)]
struct Fixup {
    /// (page, known ptrmap type or 0 to look it up)
    queue: VecDeque<(u32, u8)>,
    done: HashSet<u32>,
    dirty: RoaringBitmap,
}

#[derive(Debug, Clone, Copy)]
enum Take {
    Exact(u32),
    AtMost(u32),
}

#[derive(Debug)]
enum VacuumPhase {
    Fixup,
    /// Partial incremental vacuum: one page at a time off the end
    /// (incrVacuumStep with bCommit=0), keeping the rest of the freelist.
    Step {
        to: Option<u32>,
    },
    StepFinish,
    LoadFreelist {
        next_trunk: u32,
    },
    Relocate,
    Rewrite {
        idx: usize,
    },
    Finish,
}

#[derive(Debug)]
struct VacuumJob {
    phase: VacuumPhase,
    n_orig: u32,
    n_fin: u32,
    n_free: u32,
    n_vac: u32,
    free: Vec<u32>,
    free_set: HashSet<u32>,
    /// Free pages at or below `n_fin`, largest first so `pop` gives the lowest.
    targets: Vec<u32>,
    cur: u32,
    /// Freelist rebuilt from the free pages that survive: (trunk, leaves).
    trunks: Vec<(u32, Vec<u32>)>,
}

#[derive(Debug, Default, Clone, Copy)]
enum CreatePhase {
    #[default]
    Start,
    Fixup {
        root: u32,
    },
    /// The page at `root` is in use and moves to `to` first.
    Relocate {
        root: u32,
        to: u32,
    },
    TakeFree {
        root: u32,
    },
    Alloc {
        root: u32,
    },
    Grow {
        root: u32,
    },
    Init {
        root: u32,
    },
}

#[derive(Debug, Default, Clone, Copy)]
enum DropPhase {
    #[default]
    Start,
    Relocate {
        root: u32,
        max: u32,
    },
    Free {
        page: u32,
        moved: u32,
        max: u32,
    },
    Header {
        moved: u32,
        max: u32,
    },
}

/// State of the auto-vacuum operations of one pager.
#[derive(Debug, Default)]
pub(crate) struct AvState {
    fixup: Option<Fixup>,
    /// `pages_dirtied_count` when the last fixup finished.
    clean_at: Option<u64>,
    commit_done: bool,
    /// Pages this commit gives back (FULL mode), once decided.
    commit_vac: Option<u32>,
    vacuum: Option<VacuumJob>,
    create: CreatePhase,
    drop: DropPhase,
    /// Freelist search for `av_freelist_take`: (previous trunk, trunk).
    take: Option<(u32, u32)>,
    /// When set, `allocate_page` extends the file instead of using the freelist.
    pub(super) force_grow: bool,
}

impl Pager {
    pub(crate) fn av_geom(&self, h: &DatabaseHeader) -> Geom {
        Geom {
            usable: h.usable_space() as u32,
            pending: self.pending_byte_page_id().unwrap_or(0),
        }
    }

    pub(crate) fn av_reset(&self) {
        *self.av_state.write() = AvState::default();
        self.av_touched.write().0 = false;
    }

    /// Drops any half-done operation after an error.
    fn av_abort(&self) {
        self.av_touched.write().0 = false;
        let mut st = self.av_state.write();
        st.fixup = None;
        st.clean_at = None;
        st.vacuum = None;
        st.create = CreatePhase::Start;
        st.drop = DropPhase::Start;
        st.take = None;
        st.force_grow = false;
    }

    /// Reads `ids` into the cache and pins them; yields until all are loaded.
    fn av_load(&self, ids: &[u32]) -> IOResultOr<Vec<PinGuard>> {
        let mut pages = Vec::with_capacity(ids.len());
        let mut wait = None;
        for &id in ids {
            let (page, c) = return_if_io!(self.read_page(id as i64));
            if !page.is_loaded() {
                wait = Some(c.unwrap_or_else(Completion::new_yield));
            }
            pages.push(PinGuard::new(page));
        }
        if let Some(c) = wait {
            io_yield_one!(c);
        }
        Ok(IOResult::Done(pages))
    }

    /// Loads the ptrmap pages covering `targets`.
    fn av_load_ptrmaps(
        &self,
        geom: Geom,
        targets: impl Iterator<Item = u32>,
    ) -> IOResultOr<HashMap<u32, PinGuard>> {
        let mut ids: Vec<u32> = targets.map(|p| geom.ptrmap_pageno(p)).collect();
        ids.sort_unstable();
        ids.dedup();
        let pages = return_if_io!(self.av_load(&ids));
        Ok(IOResult::Done(ids.into_iter().zip(pages).collect()))
    }

    /// Writes a ptrmap entry, dirtying the ptrmap page only on change.
    fn av_put(
        &self,
        geom: Geom,
        maps: &HashMap<u32, PinGuard>,
        pgno: u32,
        ty: u8,
        parent: u32,
    ) -> Result<()> {
        let page = maps
            .get(&geom.ptrmap_pageno(pgno))
            .expect("ptrmap page loaded")
            .to_page();
        let off = geom.offset(pgno);
        if read_entry(&page, off) == (ty, parent) {
            return Ok(());
        }
        self.add_dirty(&page)?;
        let buf = page.get_contents().as_ptr();
        buf[off] = ty;
        buf[off + 1..off + 5].copy_from_slice(&parent.to_be_bytes());
        Ok(())
    }

    fn av_check_pgno(geom: Geom, db_size: u32, pgno: u32) -> Result<()> {
        if pgno < 2 || pgno > db_size || geom.is_ptrmap(pgno) {
            return Err(LimboError::Corrupt(format!(
                "invalid page number {pgno} in auto-vacuum database of {db_size} pages"
            )));
        }
        Ok(())
    }

    /// Single-entry ptrmap lookup.
    pub(crate) fn av_get(&self, geom: Geom, pgno: u32) -> IOResultOr<(u8, u32)> {
        let maps = return_if_io!(self.av_load_ptrmaps(geom, std::iter::once(pgno)));
        let page = maps.values().next().expect("ptrmap page").to_page();
        Ok(IOResult::Done(read_entry(&page, geom.offset(pgno))))
    }

    /// Brings the ptrmap entries of everything reachable from the pages
    /// dirtied in this transaction up to date.
    pub(crate) fn av_fixup(&self) -> IOResultOr<()> {
        let res = self.av_fixup_inner();
        if res.is_err() {
            self.av_abort();
        }
        res
    }

    fn av_fixup_inner(&self) -> IOResultOr<()> {
        {
            let mut st = self.av_state.write();
            if st.fixup.is_none() {
                if st.clean_at == Some(self.pages_dirtied_count()) {
                    return Ok(IOResult::Done(()));
                }
                let dirty = self.dirty_pages.read().clone();
                // Only pages changed since the last fixup need a visit.
                let seeds = {
                    let mut touched = self.av_touched.write();
                    let seeds = if touched.0 {
                        std::mem::take(&mut touched.1)
                    } else {
                        touched.1.clear();
                        dirty.clone()
                    };
                    touched.0 = true;
                    seeds
                };
                st.fixup = Some(Fixup {
                    queue: seeds.iter().map(|p| (p, 0)).collect(),
                    done: HashSet::new(),
                    dirty,
                });
            }
        }
        let (geom, db_size) =
            return_if_io!(self.with_header(|h| (self.av_geom(h), h.database_size.get())));
        loop {
            let next = {
                let mut st = self.av_state.write();
                let fixup = st.fixup.as_mut().expect("fixup in progress");
                loop {
                    match fixup.queue.front().copied() {
                        Some((pg, _))
                            if pg > db_size || geom.skip(pg) || fixup.done.contains(&pg) =>
                        {
                            fixup.queue.pop_front();
                        }
                        other => break other,
                    }
                }
            };
            let Some((pg, known)) = next else { break };
            return_if_io!(self.av_fixup_page(geom, db_size, pg, known));
            let mut st = self.av_state.write();
            st.fixup.as_mut().expect("fixup").queue.pop_front();
        }
        let mut st = self.av_state.write();
        st.fixup = None;
        st.clean_at = Some(self.pages_dirtied_count());
        Ok(IOResult::Done(()))
    }

    fn av_fixup_page(&self, geom: Geom, db_size: u32, pg: u32, known: u8) -> IOResultOr<()> {
        let ty = if known != 0 {
            known
        } else if pg == 1 {
            PTRMAP_ROOTPAGE
        } else {
            return_if_io!(self.av_get(geom, pg)).0
        };
        if !matches!(
            ty,
            PTRMAP_ROOTPAGE | PTRMAP_BTREE | PTRMAP_OVERFLOW1 | PTRMAP_OVERFLOW2
        ) {
            return Ok(IOResult::Done(()));
        }
        let page = return_if_io!(self.av_load(&[pg])).remove(0);
        let ptrs = page_pointers(&page.to_page(), ty, geom.usable as usize)?;
        for &(c, _) in &ptrs {
            Self::av_check_pgno(geom, db_size, c)?;
        }
        let maps = return_if_io!(self.av_load_ptrmaps(geom, ptrs.iter().map(|p| p.0)));
        for &(c, cty) in &ptrs {
            self.av_put(geom, &maps, c, cty, pg)?;
        }
        let mut st = self.av_state.write();
        let fixup = st.fixup.as_mut().expect("fixup");
        fixup.done.insert(pg);
        for (c, cty) in ptrs {
            if fixup.dirty.contains(c) && !fixup.done.contains(&c) {
                fixup.queue.push_back((c, cty));
            }
        }
        Ok(IOResult::Done(()))
    }

    /// relocatePage: move page `from` (ptrmap `ty`/`parent`) to page `to`.
    fn av_relocate(
        &self,
        geom: Geom,
        db_size: u32,
        from: u32,
        to: u32,
        ty: u8,
        parent: u32,
    ) -> IOResultOr<()> {
        if from < 3 {
            return Err(LimboError::Corrupt(format!("cannot relocate page {from}")).into());
        }
        let mut ids = vec![from, to];
        if ty != PTRMAP_ROOTPAGE {
            if parent < 1 || parent > db_size || geom.is_ptrmap(parent) {
                return Err(LimboError::Corrupt(format!(
                    "page {from} has invalid parent {parent}"
                ))
                .into());
            }
            ids.push(parent);
        }
        let pages = return_if_io!(self.av_load(&ids));
        let (from_page, to_page) = (pages[0].to_page(), pages[1].to_page());
        let ptrs = page_pointers(&from_page, ty, geom.usable as usize)?;
        for &(c, _) in &ptrs {
            Self::av_check_pgno(geom, db_size, c)?;
        }
        let maps = return_if_io!(
            self.av_load_ptrmaps(geom, ptrs.iter().map(|p| p.0).chain(std::iter::once(to)))
        );
        tracing::debug!("autovacuum: move page {from} to {to} (type {ty}, parent {parent})");
        if !from_page.get().overflow_cells.is_empty() {
            return Err(LimboError::InternalError(format!(
                "page {from} has pending overflow cells"
            ))
            .into());
        }
        self.add_dirty(&to_page)?;
        to_page
            .get_contents()
            .as_ptr()
            .copy_from_slice(from_page.get_contents().as_ptr());
        for &(c, cty) in &ptrs {
            self.av_put(geom, &maps, c, cty, to)?;
        }
        if ty == PTRMAP_ROOTPAGE {
            self.av_put(geom, &maps, to, PTRMAP_ROOTPAGE, 0)?;
        } else {
            let parent_page = pages[2].to_page();
            self.add_dirty(&parent_page)?;
            modify_page_pointer(&parent_page, from, to, ty, geom.usable as usize)?;
            self.av_put(geom, &maps, to, ty, parent)?;
        }
        // The ptrmap was kept exact, so a clean ptrmap stays clean.
        self.av_keep_clean();
        Ok(IOResult::Done(()))
    }

    fn av_keep_clean(&self) {
        let count = self.pages_dirtied_count();
        let mut st = self.av_state.write();
        if st.clean_at.is_some() {
            st.clean_at = Some(count);
        }
    }

    /// Drops every page above `n_fin` from the cache and the dirty set,
    /// keeping the pre-images savepoints need.
    fn av_truncate(&self, n_fin: u32) -> Result<()> {
        let doomed: Vec<u32> = self
            .dirty_pages
            .read()
            .iter()
            .filter(|&p| p > n_fin)
            .collect();
        let mut pages = Vec::with_capacity(doomed.len());
        {
            let mut cache = self.page_cache.write();
            for id in doomed {
                if let Some(page) = cache.get(&PageCacheKey::new(id as usize))? {
                    pages.push(page);
                }
            }
        }
        for page in &pages {
            if page.is_loaded() {
                self.subjournal_page_if_required(page)?;
            }
            page.clear_dirty();
            page.try_unpin();
        }
        self.dirty_pages.write().remove_range((n_fin + 1)..);
        self.page_cache.write().truncate(n_fin as usize)?;
        Ok(())
    }

    /// Vacuums `n` free pages (all of them when `n` is `None`): relocates
    /// the pages in use above the final size into free pages below it,
    /// rebuilds the freelist from what is left and shrinks the database.
    /// Returns the number of free pages given back.
    pub(crate) fn av_vacuum(&self, n: Option<u32>) -> IOResultOr<u32> {
        let res = self.av_vacuum_inner(n);
        if res.is_err() {
            self.av_abort();
        }
        res
    }

    fn av_vacuum_inner(&self, n: Option<u32>) -> IOResultOr<u32> {
        loop {
            let phase = {
                let st = self.av_state.read();
                st.vacuum.as_ref().map(|j| match j.phase {
                    VacuumPhase::Fixup => 0,
                    VacuumPhase::LoadFreelist { .. } => 1,
                    VacuumPhase::Relocate => 2,
                    VacuumPhase::Rewrite { .. } => 3,
                    VacuumPhase::Finish => 4,
                    VacuumPhase::Step { .. } => 5,
                    VacuumPhase::StepFinish => 6,
                })
            };
            match phase {
                None => {
                    let (geom, n_orig, n_free) = return_if_io!(self.with_header(|h| (
                        self.av_geom(h),
                        h.database_size.get(),
                        h.freelist_pages.get()
                    )));
                    if n_free == 0 {
                        return Ok(IOResult::Done(0));
                    }
                    if n_free >= n_orig || geom.skip(n_orig) {
                        return Err(LimboError::Corrupt(format!(
                            "freelist of {n_free} pages in a database of {n_orig} pages"
                        ))
                        .into());
                    }
                    let n_vac = n.map_or(n_free, |n| n.min(n_free));
                    let n_fin = geom.final_db_size(n_orig, n_vac);
                    if n_fin > n_orig {
                        return Err(LimboError::Corrupt("bad final database size".into()).into());
                    }
                    self.av_state.write().vacuum = Some(VacuumJob {
                        phase: VacuumPhase::Fixup,
                        n_orig,
                        n_fin,
                        n_free,
                        n_vac,
                        free: Vec::with_capacity(n_free as usize),
                        free_set: HashSet::new(),
                        targets: Vec::new(),
                        cur: n_orig,
                        trunks: Vec::new(),
                    });
                }
                Some(0) => {
                    return_if_io!(self.av_fixup());
                    let trunk = return_if_io!(self.with_header(|h| h.freelist_trunk_page.get()));
                    let mut st = self.av_state.write();
                    let job = st.vacuum.as_mut().expect("vacuum");
                    job.phase = if job.n_vac < job.n_free {
                        VacuumPhase::Step { to: None }
                    } else {
                        VacuumPhase::LoadFreelist { next_trunk: trunk }
                    };
                }
                Some(5) => return_if_io!(self.av_vacuum_partial_step()),
                Some(6) => {
                    let (n_fin, n_vac) = {
                        let st = self.av_state.read();
                        let job = st.vacuum.as_ref().expect("vacuum");
                        (job.n_fin, job.n_vac)
                    };
                    return_if_io!(self.with_header_mut(|h| {
                        h.database_size = n_fin.into();
                    }));
                    self.invalidate_all_cursors();
                    self.av_truncate(n_fin)?;
                    self.av_state.write().vacuum = None;
                    self.av_keep_clean();
                    return Ok(IOResult::Done(n_vac));
                }
                Some(1) => return_if_io!(self.av_vacuum_load_trunk()),
                Some(2) => return_if_io!(self.av_vacuum_relocate_step()),
                Some(3) => return_if_io!(self.av_vacuum_rewrite_step()),
                Some(_) => {
                    let (n_fin, first_trunk, n_left, n_vac) = {
                        let st = self.av_state.read();
                        let job = st.vacuum.as_ref().expect("vacuum");
                        let n_left: usize = job.trunks.iter().map(|(_, l)| l.len() + 1).sum();
                        let first = job.trunks.first().map_or(0, |t| t.0);
                        (job.n_fin, first, n_left, job.n_vac)
                    };
                    return_if_io!(self.with_header_mut(|h| {
                        h.freelist_trunk_page = first_trunk.into();
                        h.freelist_pages = (n_left as u32).into();
                        h.database_size = n_fin.into();
                    }));
                    // Cursors may still pin pages past the new end.
                    self.invalidate_all_cursors();
                    self.av_truncate(n_fin)?;
                    self.av_state.write().vacuum = None;
                    self.av_keep_clean();
                    return Ok(IOResult::Done(n_vac));
                }
            }
        }
    }

    fn av_vacuum_partial_step(&self) -> IOResultOr<()> {
        let (geom, db_size) =
            return_if_io!(self.with_header(|h| (self.av_geom(h), h.database_size.get())));
        let (cur, n_fin, to) = {
            let mut st = self.av_state.write();
            let job = st.vacuum.as_mut().expect("vacuum");
            while job.cur > job.n_fin && geom.skip(job.cur) {
                job.cur -= 1;
            }
            let VacuumPhase::Step { to } = job.phase else {
                unreachable!()
            };
            (job.cur, job.n_fin, to)
        };
        if cur <= n_fin {
            self.av_state.write().vacuum.as_mut().expect("vacuum").phase = VacuumPhase::StepFinish;
            return Ok(IOResult::Done(()));
        }
        let (ty, parent) = return_if_io!(self.av_get(geom, cur));
        match ty {
            PTRMAP_FREEPAGE => {
                return_if_io!(self.av_freelist_take(Take::Exact(cur)));
            }
            PTRMAP_BTREE | PTRMAP_OVERFLOW1 | PTRMAP_OVERFLOW2 => {
                let to = match to {
                    Some(to) => to,
                    None => {
                        let to = return_if_io!(self.av_freelist_take(Take::AtMost(n_fin)));
                        self.av_state.write().vacuum.as_mut().expect("vacuum").phase =
                            VacuumPhase::Step { to: Some(to) };
                        to
                    }
                };
                return_if_io!(self.av_relocate(geom, db_size, cur, to, ty, parent));
            }
            _ => {
                return Err(LimboError::Corrupt(format!(
                    "page {cur} has ptrmap type {ty} during incremental vacuum"
                ))
                .into())
            }
        }
        let mut st = self.av_state.write();
        let job = st.vacuum.as_mut().expect("vacuum");
        job.cur -= 1;
        job.phase = VacuumPhase::Step { to: None };
        Ok(IOResult::Done(()))
    }

    fn av_vacuum_load_trunk(&self) -> IOResultOr<()> {
        let (trunk, n_orig, n_free, have) = {
            let st = self.av_state.read();
            let job = st.vacuum.as_ref().expect("vacuum");
            let VacuumPhase::LoadFreelist { next_trunk } = job.phase else {
                unreachable!()
            };
            (next_trunk, job.n_orig, job.n_free, job.free.len() as u32)
        };
        let usable = return_if_io!(self.with_header(|h| h.usable_space()));
        if trunk == 0 {
            if have != n_free {
                return Err(LimboError::Corrupt(format!(
                    "freelist holds {have} pages but the header says {n_free}"
                ))
                .into());
            }
            let mut st = self.av_state.write();
            let job = st.vacuum.as_mut().expect("vacuum");
            job.free_set = job.free.iter().copied().collect();
            let n_fin = job.n_fin;
            job.targets = job.free.iter().copied().filter(|&p| p <= n_fin).collect();
            job.targets.sort_unstable_by(|a, b| b.cmp(a));
            job.phase = VacuumPhase::Relocate;
            return Ok(IOResult::Done(()));
        }
        if trunk < 2 || trunk > n_orig || have >= n_free {
            return Err(LimboError::Corrupt(format!("bad freelist trunk page {trunk}")).into());
        }
        let page = return_if_io!(self.av_load(&[trunk])).remove(0).to_page();
        let contents = page.get_contents();
        let next = contents.read_u32_no_offset(FREELIST_TRUNK_OFFSET_NEXT_TRUNK_PTR);
        let count = contents.read_u32_no_offset(FREELIST_TRUNK_OFFSET_LEAF_COUNT) as usize;
        if count > usable / 4 - 2 {
            return Err(LimboError::Corrupt(format!(
                "freelist trunk page {trunk} has {count} leaves"
            ))
            .into());
        }
        let mut leaves = Vec::with_capacity(count);
        for i in 0..count {
            let leaf = contents.read_u32_no_offset(
                FREELIST_TRUNK_OFFSET_FIRST_LEAF_PTR + i * FREELIST_LEAF_PTR_SIZE,
            );
            if leaf < 2 || leaf > n_orig {
                return Err(LimboError::Corrupt(format!("bad freelist leaf page {leaf}")).into());
            }
            leaves.push(leaf);
        }
        let mut st = self.av_state.write();
        let job = st.vacuum.as_mut().expect("vacuum");
        job.free.push(trunk);
        job.free.extend(leaves);
        if job.free.len() as u32 > n_free {
            return Err(LimboError::Corrupt("freelist is longer than its count".into()).into());
        }
        job.phase = VacuumPhase::LoadFreelist { next_trunk: next };
        Ok(IOResult::Done(()))
    }

    fn av_vacuum_relocate_step(&self) -> IOResultOr<()> {
        let (geom, db_size) =
            return_if_io!(self.with_header(|h| (self.av_geom(h), h.database_size.get())));
        let (cur, n_fin) = {
            let mut st = self.av_state.write();
            let job = st.vacuum.as_mut().expect("vacuum");
            while job.cur > job.n_fin && (geom.skip(job.cur) || job.free_set.contains(&job.cur)) {
                job.cur -= 1;
            }
            (job.cur, job.n_fin)
        };
        if cur <= n_fin {
            let mut st = self.av_state.write();
            let job = st.vacuum.as_mut().expect("vacuum");
            // What is left of the freelist, all of it below n_fin.
            let mut left = job.targets.clone();
            left.sort_unstable();
            let per = geom.usable as usize / 4 - 8;
            job.trunks = left
                .chunks(per + 1)
                .map(|c| (c[0], c[1..].to_vec()))
                .collect();
            job.phase = VacuumPhase::Rewrite { idx: 0 };
            return Ok(IOResult::Done(()));
        }
        let (ty, parent) = return_if_io!(self.av_get(geom, cur));
        if ty == PTRMAP_ROOTPAGE || ty == PTRMAP_FREEPAGE || ty == 0 || ty > PTRMAP_BTREE {
            return Err(LimboError::Corrupt(format!(
                "page {cur} has ptrmap type {ty} during auto-vacuum"
            ))
            .into());
        }
        let to = {
            let st = self.av_state.read();
            match st.vacuum.as_ref().expect("vacuum").targets.last() {
                Some(&t) => t,
                None => {
                    return Err(LimboError::Corrupt("no free page to relocate into".into()).into())
                }
            }
        };
        return_if_io!(self.av_relocate(geom, db_size, cur, to, ty, parent));
        let mut st = self.av_state.write();
        let job = st.vacuum.as_mut().expect("vacuum");
        job.targets.pop();
        job.free_set.remove(&to);
        job.cur -= 1;
        Ok(IOResult::Done(()))
    }

    fn av_vacuum_rewrite_step(&self) -> IOResultOr<()> {
        let (idx, entry, next) = {
            let st = self.av_state.read();
            let job = st.vacuum.as_ref().expect("vacuum");
            let VacuumPhase::Rewrite { idx } = job.phase else {
                unreachable!()
            };
            (
                idx,
                job.trunks.get(idx).cloned(),
                job.trunks.get(idx + 1).map_or(0, |t| t.0),
            )
        };
        let Some((trunk, leaves)) = entry else {
            self.av_state.write().vacuum.as_mut().expect("vacuum").phase = VacuumPhase::Finish;
            return Ok(IOResult::Done(()));
        };
        let page = return_if_io!(self.av_load(&[trunk])).remove(0).to_page();
        self.add_dirty(&page)?;
        let contents = page.get_contents();
        contents.write_u32_no_offset(FREELIST_TRUNK_OFFSET_NEXT_TRUNK_PTR, next);
        contents.write_u32_no_offset(FREELIST_TRUNK_OFFSET_LEAF_COUNT, leaves.len() as u32);
        for (i, leaf) in leaves.iter().enumerate() {
            contents.write_u32_no_offset(
                FREELIST_TRUNK_OFFSET_FIRST_LEAF_PTR + i * FREELIST_LEAF_PTR_SIZE,
                *leaf,
            );
        }
        self.av_state.write().vacuum.as_mut().expect("vacuum").phase =
            VacuumPhase::Rewrite { idx: idx + 1 };
        Ok(IOResult::Done(()))
    }

    /// autoVacuumCommit: fix the ptrmap and, in FULL mode, give every free
    /// page back to the file system. Runs once per commit.
    pub(crate) fn av_commit(&self, connection: &crate::Connection) -> IOResultOr<()> {
        if self.av_state.read().commit_done || self.dirty_pages.read().is_empty() {
            return Ok(IOResult::Done(()));
        }
        let mode = return_if_io!(self.with_header(mode_from_header));
        self.set_auto_vacuum_mode(mode);
        match mode {
            AutoVacuumMode::None => {}
            AutoVacuumMode::Incremental => return_if_io!(self.av_fixup()),
            AutoVacuumMode::Full => {
                return_if_io!(self.av_fixup());
                // xAutovacPages decides how much to give back, once per commit.
                let decided = self.av_state.read().commit_vac;
                let n_vac = match decided {
                    Some(n) => n,
                    None => {
                        let (n_pages, n_free, page_size) = return_if_io!(self.with_header(|h| (
                            h.database_size.get(),
                            h.freelist_pages.get(),
                            h.page_size.get()
                        )));
                        let n = connection
                            .autovacuum_pages_for(self, n_pages, n_free, page_size)
                            .map_or(n_free, |n| n.min(n_free));
                        self.av_state.write().commit_vac = Some(n);
                        n
                    }
                };
                if n_vac > 0 {
                    return_if_io!(self.av_vacuum(Some(n_vac)));
                }
            }
        }
        self.av_state.write().commit_done = true;
        Ok(IOResult::Done(()))
    }

    /// Called once the commit is durable.
    pub(crate) fn av_commit_end(&self) {
        *self.av_touched.write() = (true, RoaringBitmap::new());
        let mut st = self.av_state.write();
        st.commit_done = false;
        st.commit_vac = None;
        st.clean_at = None;
    }

    /// Takes a page off the freelist (allocateBtreePage with BTALLOC_EXACT or
    /// BTALLOC_LE) and returns its number.
    fn av_freelist_take(&self, want: Take) -> IOResultOr<u32> {
        let matches = |p: u32| match want {
            Take::Exact(x) => p == x,
            Take::AtMost(x) => p <= x,
        };
        loop {
            let (prev, trunk) = {
                let st = self.av_state.write();
                match st.take {
                    Some(t) => t,
                    None => {
                        drop(st);
                        let first =
                            return_if_io!(self.with_header(|h| h.freelist_trunk_page.get()));
                        self.av_state.write().take = Some((0, first));
                        continue;
                    }
                }
            };
            if trunk == 0 {
                self.av_state.write().take = None;
                return Err(
                    LimboError::Corrupt(format!("no freelist page matches {want:?}")).into(),
                );
            }
            let mut ids = vec![trunk];
            if prev != 0 {
                ids.push(prev);
            }
            let pages = return_if_io!(self.av_load(&ids));
            let tp = pages[0].to_page();
            let tc = tp.get_contents();
            let next = tc.read_u32_no_offset(FREELIST_TRUNK_OFFSET_NEXT_TRUNK_PTR);
            let count = tc.read_u32_no_offset(FREELIST_TRUNK_OFFSET_LEAF_COUNT) as usize;
            let leaf_at = |i: usize| {
                tc.read_u32_no_offset(
                    FREELIST_TRUNK_OFFSET_FIRST_LEAF_PTR + i * FREELIST_LEAF_PTR_SIZE,
                )
            };
            // A leaf is cheaper to take than the trunk itself.
            if let Some(i) = (0..count).find(|&i| matches(leaf_at(i))) {
                let taken = leaf_at(i);
                self.add_dirty(&tp)?;
                let last = leaf_at(count - 1);
                tc.write_u32_no_offset(
                    FREELIST_TRUNK_OFFSET_FIRST_LEAF_PTR + i * FREELIST_LEAF_PTR_SIZE,
                    last,
                );
                tc.write_u32_no_offset(FREELIST_TRUNK_OFFSET_LEAF_COUNT, count as u32 - 1);
                return_if_io!(self.with_header_mut(|h| {
                    h.freelist_pages = (h.freelist_pages.get() - 1).into();
                }));
                self.av_state.write().take = None;
                return Ok(IOResult::Done(taken));
            }
            if matches(trunk) {
                // The trunk itself: its first leaf, if any, becomes the trunk.
                let replacement = if count > 0 {
                    let new_trunk = leaf_at(0);
                    let np = return_if_io!(self.av_load(&[new_trunk]))
                        .remove(0)
                        .to_page();
                    self.add_dirty(&np)?;
                    let nc = np.get_contents();
                    nc.write_u32_no_offset(FREELIST_TRUNK_OFFSET_NEXT_TRUNK_PTR, next);
                    nc.write_u32_no_offset(FREELIST_TRUNK_OFFSET_LEAF_COUNT, count as u32 - 1);
                    for i in 1..count {
                        nc.write_u32_no_offset(
                            FREELIST_TRUNK_OFFSET_FIRST_LEAF_PTR + (i - 1) * FREELIST_LEAF_PTR_SIZE,
                            leaf_at(i),
                        );
                    }
                    new_trunk
                } else {
                    next
                };
                if prev == 0 {
                    return_if_io!(self.with_header_mut(|h| {
                        h.freelist_trunk_page = replacement.into();
                        h.freelist_pages = (h.freelist_pages.get() - 1).into();
                    }));
                } else {
                    let pp = pages[1].to_page();
                    self.add_dirty(&pp)?;
                    pp.get_contents()
                        .write_u32_no_offset(FREELIST_TRUNK_OFFSET_NEXT_TRUNK_PTR, replacement);
                    return_if_io!(self.with_header_mut(|h| {
                        h.freelist_pages = (h.freelist_pages.get() - 1).into();
                    }));
                }
                self.av_state.write().take = None;
                return Ok(IOResult::Done(trunk));
            }
            self.av_state.write().take = Some((trunk, next));
        }
    }

    /// btreeCreateTable for auto-vacuum databases: the new root goes right
    /// after the largest root page, moving whatever lives there.
    pub(crate) fn av_btree_create(&self, page_type: PageType) -> IOResultOr<u32> {
        let res = self.av_btree_create_inner(page_type);
        if res.is_err() {
            self.av_abort();
        }
        res
    }

    fn av_btree_create_inner(&self, page_type: PageType) -> IOResultOr<u32> {
        loop {
            let phase = self.av_state.read().create;
            match phase {
                CreatePhase::Start => {
                    let (geom, largest, db_size) = return_if_io!(self.with_header(|h| (
                        self.av_geom(h),
                        h.vacuum_mode_largest_root_page.get(),
                        h.database_size.get()
                    )));
                    if largest > db_size {
                        return Err(LimboError::Corrupt(format!(
                            "largest root page {largest} beyond database size {db_size}"
                        ))
                        .into());
                    }
                    let mut root = largest + 1;
                    while geom.skip(root) {
                        root += 1;
                    }
                    self.av_state.write().create = if root > db_size {
                        CreatePhase::Grow { root }
                    } else {
                        CreatePhase::Fixup { root }
                    };
                }
                CreatePhase::Fixup { root } => {
                    return_if_io!(self.av_fixup());
                    let geom = return_if_io!(self.with_header(|h| self.av_geom(h)));
                    let (ty, _) = return_if_io!(self.av_get(geom, root));
                    let next = match ty {
                        PTRMAP_FREEPAGE => CreatePhase::TakeFree { root },
                        PTRMAP_ROOTPAGE => {
                            self.av_state.write().create = CreatePhase::Start;
                            return Err(LimboError::Corrupt(format!(
                                "page {root} is already a root page"
                            ))
                            .into());
                        }
                        _ => CreatePhase::Alloc { root },
                    };
                    self.av_state.write().create = next;
                }
                CreatePhase::Alloc { root } => {
                    // allocate_page takes from the freelist before it grows the file.
                    let page = return_if_io!(self.allocate_page());
                    self.av_state.write().create = CreatePhase::Relocate {
                        root,
                        to: page.get().id() as u32,
                    };
                }
                CreatePhase::Relocate { root, to } => {
                    let (geom, db_size) = return_if_io!(
                        self.with_header(|h| (self.av_geom(h), h.database_size.get()))
                    );
                    let (ty, parent) = return_if_io!(self.av_get(geom, root));
                    return_if_io!(self.av_relocate(geom, db_size, root, to, ty, parent));
                    self.invalidate_all_cursors();
                    self.av_state.write().create = CreatePhase::Init { root };
                }
                CreatePhase::TakeFree { root } => {
                    return_if_io!(self.av_freelist_take(Take::Exact(root)));
                    self.av_state.write().create = CreatePhase::Init { root };
                }
                CreatePhase::Grow { root } => {
                    self.av_state.write().force_grow = true;
                    let res = self.allocate_page();
                    self.av_state.write().force_grow = false;
                    let page = return_if_io!(res);
                    let id = page.get().id() as u32;
                    if id != root {
                        self.av_state.write().create = CreatePhase::Start;
                        return Err(LimboError::InternalError(format!(
                            "new root page {id} is not the expected page {root}"
                        ))
                        .into());
                    }
                    self.av_state.write().create = CreatePhase::Init { root };
                }
                CreatePhase::Init { root } => {
                    let geom = return_if_io!(self.with_header(|h| self.av_geom(h)));
                    let page = return_if_io!(self.av_load(&[root])).remove(0).to_page();
                    let maps = return_if_io!(self.av_load_ptrmaps(geom, std::iter::once(root)));
                    self.add_dirty(&page)?;
                    page.get().overflow_cells.clear();
                    page.get_contents().as_ptr().fill(0);
                    btree_init_page(&page, page_type, 0, self.usable_space());
                    self.av_put(geom, &maps, root, PTRMAP_ROOTPAGE, 0)?;
                    return_if_io!(self.with_header_mut(|h| {
                        h.vacuum_mode_largest_root_page = root.into();
                    }));
                    self.av_state.write().create = CreatePhase::Start;
                    return Ok(IOResult::Done(root));
                }
            }
        }
    }

    /// btreeDropTable for auto-vacuum databases, run after the tree at
    /// `root` was cleared (its root page kept). Moves the largest root page
    /// into the hole and returns its old number, or 0 if nothing moved.
    pub(crate) fn av_drop_root(&self, root: u32) -> IOResultOr<u32> {
        let res = self.av_drop_root_inner(root);
        if res.is_err() {
            self.av_abort();
        }
        res
    }

    fn av_drop_root_inner(&self, root: u32) -> IOResultOr<u32> {
        loop {
            let phase = self.av_state.read().drop;
            match phase {
                DropPhase::Start => {
                    let max =
                        return_if_io!(self.with_header(|h| h.vacuum_mode_largest_root_page.get()));
                    self.av_state.write().drop = if root == max {
                        DropPhase::Free {
                            page: root,
                            moved: 0,
                            max,
                        }
                    } else {
                        DropPhase::Relocate { root, max }
                    };
                }
                DropPhase::Relocate { root, max } => {
                    let (geom, db_size) = return_if_io!(
                        self.with_header(|h| (self.av_geom(h), h.database_size.get()))
                    );
                    return_if_io!(self.av_relocate(geom, db_size, max, root, PTRMAP_ROOTPAGE, 0));
                    self.invalidate_all_cursors();
                    self.av_state.write().drop = DropPhase::Free {
                        page: max,
                        moved: max,
                        max,
                    };
                }
                DropPhase::Free { page, moved, max } => {
                    return_if_io!(self.free_page(None, page as usize));
                    self.av_state.write().drop = DropPhase::Header { moved, max };
                }
                DropPhase::Header { moved, max } => {
                    let geom = return_if_io!(self.with_header(|h| self.av_geom(h)));
                    let mut new_max = max - 1;
                    while new_max > 1 && geom.skip(new_max) {
                        new_max -= 1;
                    }
                    return_if_io!(self.with_header_mut(|h| {
                        h.vacuum_mode_largest_root_page = new_max.into();
                    }));
                    self.av_state.write().drop = DropPhase::Start;
                    return Ok(IOResult::Done(moved));
                }
            }
        }
    }

    /// Records a freshly freed page in the ptrmap.
    pub(crate) fn av_mark_free(&self, pgno: u32) -> IOResultOr<()> {
        let geom = return_if_io!(self.with_header(|h| self.av_geom(h)));
        let maps = return_if_io!(self.av_load_ptrmaps(geom, std::iter::once(pgno)));
        self.av_put(geom, &maps, pgno, PTRMAP_FREEPAGE, 0)?;
        Ok(IOResult::Done(()))
    }
}

impl Pager {
    /// Whether page `id`, about to be created at the end of the file, existed
    /// when the newest savepoint was opened (the file was truncated since).
    pub(super) fn av_needs_preimage(&self, id: u32) -> bool {
        self.subjournal.read().is_some()
            && Self::savepoints_require_page(&self.savepoints.read(), id)
    }

    pub(super) fn av_force_grow(&self) -> bool {
        self.av_state.read().force_grow
    }
}
