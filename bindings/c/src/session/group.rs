//! sqlite3changeset_invert, sqlite3changeset_concat and sqlite3_changegroup.

use super::iter::{ChangesetIter, NextOut, SessionInput};
use super::*;

const SQLITE_CHANGEGROUP_CONFIG_PATCHSET: i32 = 1;

/// Bytes `[start, start+len)` of `b`, clamped to its end.
fn sl(b: &[u8], start: usize, len: usize) -> &[u8] {
    let s = start.min(b.len());
    &b[s..(start + len).min(b.len())]
}

fn byte(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

fn slen(b: &[u8], o: usize) -> usize {
    serial_len(b.get(o..).unwrap_or(&[]))
}

/// sessionSkipRecord: offset just past a record of `n_col` fields.
pub(crate) fn skip_record(b: &[u8], mut o: usize, n_col: usize) -> usize {
    for _ in 0..n_col {
        o += slen(b, o);
    }
    o
}

/// sessionMergeRecord: right's value where defined, else left's.
fn merge_record(out: &mut Vec<u8>, n_col: usize, left: &[u8], right: &[u8]) {
    let (mut a1, mut a2) = (0, 0);
    for _ in 0..n_col {
        let n1 = slen(left, a1);
        let n2 = slen(right, a2);
        if byte(right, a2) != 0 {
            out.extend_from_slice(sl(right, a2, n2));
        } else {
            out.extend_from_slice(sl(left, a1, n1));
        }
        a1 += n1;
        a2 += n2;
    }
}

/// A cursor into a record.
struct Cur<'a> {
    b: &'a [u8],
    o: usize,
}

/// sessionMergeValue: `two`'s value if defined, else `one`'s.
fn merge_value<'a>(one: &mut Cur<'a>, two: Option<&mut Cur<'a>>) -> &'a [u8] {
    let mut ret = None;
    if let Some(t) = two {
        let n2 = slen(t.b, t.o);
        if byte(t.b, t.o) != 0 {
            ret = Some(sl(t.b, t.o, n2));
        }
        t.o += n2;
    }
    let n1 = slen(one.b, one.o);
    let r = ret.unwrap_or_else(|| sl(one.b, one.o, n1));
    one.o += n1;
    r
}

/// sessionMergeUpdate: false when the merged UPDATE changes nothing.
fn merge_update(
    out: &mut Vec<u8>,
    t: &SessionTable,
    patchset: bool,
    old1: &[u8],
    old2: Option<&[u8]>,
    new1: &[u8],
    new2: Option<&[u8]>,
) -> bool {
    let start = out.len();
    if !patchset {
        let mut required = false;
        let (mut o1, mut o2) = (Cur { b: old1, o: 0 }, old2.map(|b| Cur { b, o: 0 }));
        let (mut n1, mut n2) = (Cur { b: new1, o: 0 }, new2.map(|b| Cur { b, o: 0 }));
        for i in 0..t.n_col {
            let a_old = merge_value(&mut o1, o2.as_mut());
            let a_new = merge_value(&mut n1, n2.as_mut());
            if t.pk[i] != 0 || a_old != a_new {
                if t.pk[i] == 0 {
                    required = true;
                }
                out.extend_from_slice(a_old);
            } else {
                out.push(0);
            }
        }
        if !required {
            out.truncate(start);
            return false;
        }
    }
    let (mut o1, mut o2) = (Cur { b: old1, o: 0 }, old2.map(|b| Cur { b, o: 0 }));
    let (mut n1, mut n2) = (Cur { b: new1, o: 0 }, new2.map(|b| Cur { b, o: 0 }));
    for i in 0..t.n_col {
        let a_old = merge_value(&mut o1, o2.as_mut());
        let a_new = merge_value(&mut n1, n2.as_mut());
        if !patchset && (t.pk[i] != 0 || a_old == a_new) {
            out.push(0);
        } else {
            out.extend_from_slice(a_new);
        }
    }
    true
}

/// sessionChangeEqual: whether two records have the same PK.
pub(crate) fn change_equal(
    t: &SessionTable,
    left_pk_only: bool,
    a: &[u8],
    right_pk_only: bool,
    b: &[u8],
) -> bool {
    let (mut o1, mut o2) = (0, 0);
    for i in 0..t.n_col {
        if t.pk[i] != 0 {
            let n1 = slen(a, o1);
            let n2 = slen(b, o2);
            if n1 != n2 || sl(a, o1, n1) != sl(b, o2, n2) {
                return false;
            }
            o1 += n1;
            o2 += n2;
        } else {
            if !left_pk_only {
                o1 += slen(a, o1);
            }
            if !right_pk_only {
                o2 += slen(b, o2);
            }
        }
    }
    true
}

/// sessionChangeMerge: the change for one row after adding `rec` to `exist`.
fn change_merge(
    t: &SessionTable,
    rebase: bool,
    patchset: bool,
    exist: Option<SessionChange>,
    op2: u8,
    indirect: bool,
    rec: &[u8],
) -> Option<SessionChange> {
    let Some(exist) = exist else {
        let mut record = Vec::with_capacity(rec.len());
        if !indirect || !rebase {
            record.extend_from_slice(rec);
        } else {
            let mut o = 0;
            for i in 0..t.n_col {
                let n = slen(rec, o);
                if byte(rec, o) == 0 {
                    record.push(0);
                } else if t.pk[i] == 0 {
                    record.push(0xFF);
                } else {
                    record.extend_from_slice(sl(rec, o, n));
                }
                o += n;
            }
        }
        return Some(SessionChange {
            op: op2,
            indirect,
            record,
            ..Default::default()
        });
    };
    if rebase {
        if exist.op == SQLITE_DELETE && exist.indirect {
            return Some(exist);
        }
        let mut record = Vec::new();
        let (mut a1, mut a2) = (0, 0);
        let e = &exist.record;
        for i in 0..t.n_col {
            let n1 = slen(e, a1);
            let n2 = slen(rec, a2);
            if byte(e, a1) == 0xFF || (t.pk[i] == 0 && indirect) {
                record.push(0xFF);
            } else if byte(rec, a2) == 0 {
                record.extend_from_slice(sl(e, a1, n1));
            } else {
                record.extend_from_slice(sl(rec, a2, n2));
            }
            a1 += n1;
            a2 += n2;
        }
        return Some(SessionChange {
            op: op2,
            indirect: indirect || exist.indirect,
            record,
            ..Default::default()
        });
    }
    let op1 = exist.op;
    if (op1 == SQLITE_INSERT && op2 == SQLITE_INSERT)
        || (op1 == SQLITE_UPDATE && op2 == SQLITE_INSERT)
        || (op1 == SQLITE_DELETE && op2 == SQLITE_UPDATE)
        || (op1 == SQLITE_DELETE && op2 == SQLITE_DELETE)
    {
        // Unsupported combinations: the second change is discarded.
        return Some(exist);
    }
    if op1 == SQLITE_INSERT && op2 == SQLITE_DELETE {
        return None;
    }
    let a_exist = &exist.record;
    let mut out = Vec::with_capacity(a_exist.len() + rec.len());
    let new_op;
    if op1 == SQLITE_INSERT {
        // INSERT + UPDATE
        new_op = SQLITE_INSERT;
        let a1 = if patchset {
            0
        } else {
            skip_record(rec, 0, t.n_col)
        };
        merge_record(&mut out, t.n_col, a_exist, sl(rec, a1, rec.len()));
    } else if op1 == SQLITE_DELETE {
        // DELETE + INSERT
        new_op = SQLITE_UPDATE;
        if patchset {
            out.extend_from_slice(rec);
        } else if !merge_update(&mut out, t, patchset, a_exist, None, rec, None) {
            return None;
        }
    } else if op2 == SQLITE_UPDATE {
        // UPDATE + UPDATE
        new_op = SQLITE_UPDATE;
        let (a1, a2) = if patchset {
            (0, 0)
        } else {
            (
                skip_record(a_exist, 0, t.n_col),
                skip_record(rec, 0, t.n_col),
            )
        };
        if !merge_update(
            &mut out,
            t,
            patchset,
            rec,
            Some(a_exist),
            sl(a_exist, a1, a_exist.len()),
            Some(sl(rec, a2, rec.len())),
        ) {
            return None;
        }
    } else {
        // UPDATE + DELETE
        new_op = SQLITE_DELETE;
        if patchset {
            out.extend_from_slice(rec);
        } else {
            merge_record(&mut out, t.n_col, rec, a_exist);
        }
    }
    Some(SessionChange {
        op: new_op,
        indirect: indirect && exist.indirect,
        record: out,
        ..Default::default()
    })
}

/// Data for the sqlite3changegroup_change_* APIs.
#[derive(Default)]
struct ChangeData {
    tab: Option<usize>,
    indirect: bool,
    op: u8,
    bufs: Vec<Vec<u8>>,
}

/// sqlite3_changegroup.
#[derive(Default)]
pub struct Changegroup {
    pub(crate) patch: bool,
    pub(crate) tables: Vec<SessionTable>,
    db: Option<*mut sqlite3>,
    zdb: String,
    cd: ChangeData,
}

impl Changegroup {
    /// sessionChangesetCheckCompat.
    fn check_compat(t: &SessionTable, n_col: usize, pk: &[u8]) -> bool {
        if !t.cols.is_empty() && n_col < t.n_col {
            return (0..t.n_col).all(|ii| {
                let b = if ii < n_col { pk[ii] } else { 0 };
                t.pk.get(ii).copied().unwrap_or(0) == b
            });
        }
        t.n_col == n_col && t.pk.get(..n_col) == pk.get(..n_col)
    }

    /// sessionChangesetFindTable. `pk` is the iterator's PK array.
    unsafe fn find_table(&mut self, name: &str, pk: Option<&[u8]>) -> Result<usize, i32> {
        let found = self
            .tables
            .iter()
            .position(|t| t.name.eq_ignore_ascii_case(name));
        if pk.is_none() && found.is_none() && self.db.is_none() {
            return Err(SQLITE_ERROR);
        }
        let ti = match found {
            Some(i) => i,
            None => {
                let mut t = SessionTable::new(name);
                if let Some(pk) = pk {
                    t.n_col = pk.len();
                    t.pk = pk.to_vec();
                }
                if let Some(db) = self.db {
                    let info = table_info(db, &self.zdb, name, true)?;
                    if info.n_col == 0 {
                        return Err(SQLITE_ERROR);
                    }
                    t.n_col = info.n_col;
                    t.n_total_col = info.n_total_col;
                    t.cols = info.cols;
                    t.dflt = info.dflt;
                    t.idx = info.idx;
                    t.rowid = info.rowid;
                    t.pk = info.pk;
                    t.stat1 = name.eq_ignore_ascii_case("sqlite_stat1");
                }
                self.tables.push(t);
                self.tables.len() - 1
            }
        };
        if let Some(pk) = pk {
            if !Self::check_compat(&self.tables[ti], pk.len(), pk) {
                return Err(SQLITE_SCHEMA);
            }
        }
        Ok(ti)
    }

    /// sessionChangesetExtendRecord: pad a record from a table that has
    /// since gained columns.
    unsafe fn extend_record(
        &mut self,
        ti: usize,
        n_col: usize,
        op: u8,
        rec: &[u8],
    ) -> Result<Vec<u8>, i32> {
        let patch = self.patch;
        let db = self.db.unwrap_or(ptr::null_mut());
        let t = &mut self.tables[ti];
        let mut out = Vec::new();
        if op == SQLITE_INSERT || (op == SQLITE_DELETE && !patch) {
            out.extend_from_slice(rec);
            if t.dflt_stmt.is_none() {
                let stmt = prepare_dflt_stmt(db, t)?;
                if stmt.step() != SQLITE_ROW {
                    return Err(crate::sqlite3_errcode(db));
                }
                t.dflt_stmt = Some(stmt);
            }
            let stmt = t.dflt_stmt.as_ref().unwrap();
            for ii in n_col..t.n_col {
                let v = stmt.col(ii as i32);
                Val::serialize(Some(&v), &mut out);
            }
        } else if op == SQLITE_UPDATE {
            let mut off = 0;
            if !patch {
                off = skip_record(rec, 0, n_col);
                out.extend_from_slice(sl(rec, 0, off));
                out.extend(std::iter::repeat_n(0u8, t.n_col - n_col));
            }
            out.extend_from_slice(sl(rec, off, rec.len()));
            out.extend(std::iter::repeat_n(0u8, t.n_col - n_col));
        } else {
            out.extend_from_slice(rec);
        }
        Ok(out)
    }

    /// sessionOneChangeToHash.
    unsafe fn one_change_to_hash(
        &mut self,
        ti: usize,
        op: u8,
        indirect: bool,
        n_col: usize,
        rec: &[u8],
        rebase: bool,
    ) -> i32 {
        let ext;
        let rec = if n_col < self.tables[ti].n_col {
            match self.extend_record(ti, n_col, op, rec) {
                Ok(r) => {
                    ext = r;
                    &ext[..]
                }
                Err(e) => return e,
            }
        } else {
            rec
        };
        let patch = self.patch;
        let t = &mut self.tables[ti];
        t.grow_hash(patch);
        let h = t.change_hash(patch && op == SQLITE_DELETE, rec, t.n_change());
        let mut exist = None;
        let chain = &t.buckets[h];
        for pos in (0..chain.len()).rev() {
            let (pk1, pk2) = if patch {
                (chain[pos].op == SQLITE_DELETE, op == SQLITE_DELETE)
            } else {
                (false, false)
            };
            if change_equal(t, pk1, &chain[pos].record, pk2, rec) {
                exist = Some(t.buckets[h].remove(pos));
                t.n_entry -= 1;
                break;
            }
        }
        if let Some(c) = change_merge(t, rebase, patch, exist, op, indirect, rec) {
            t.buckets[h].push(c);
            t.n_entry += 1;
        }
        SQLITE_OK
    }

    /// sessionOneChangeIterToHash.
    unsafe fn one_change_iter_to_hash(&mut self, it: &ChangesetIter, rebase: bool) -> i32 {
        let input = &it.input;
        let data = input.data();
        let rec = sl(
            data,
            input.i_current + 2,
            input.i_next.saturating_sub(input.i_current + 2),
        );
        if self.tables.is_empty() {
            self.patch = it.patchset;
        } else if it.patchset != self.patch {
            return SQLITE_ERROR;
        }
        let name = it.tab_name();
        let ti = match self.find_table(&name, Some(&it.pk)) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let rc = self.one_change_to_hash(ti, it.op, it.indirect, it.n_col, rec, rebase);
        if rc != SQLITE_OK {
            return rc;
        }
        it.rc
    }

    /// sessionChangesetToHash.
    pub(crate) unsafe fn add_iter(&mut self, it: &mut ChangesetIter, rebase: bool) -> i32 {
        it.input.no_discard = true;
        let mut rc = SQLITE_OK;
        loop {
            let mut out = NextOut::default();
            if it.next(true, &mut out) != SQLITE_ROW {
                break;
            }
            rc = if rebase && it.patchset {
                // A patchset cannot be used to rebase.
                SQLITE_ERROR
            } else {
                self.one_change_iter_to_hash(it, rebase)
            };
            if rc != SQLITE_OK {
                break;
            }
        }
        if rc == SQLITE_OK {
            rc = it.rc;
        }
        rc
    }

    /// sessionChangegroupOutput.
    unsafe fn output(&self, mut out: Option<(XOutput, *mut ffi::c_void)>) -> Result<Vec<u8>, i32> {
        let mut buf = Vec::new();
        let chunk = strm_chunk_size() as usize;
        for t in &self.tables {
            if t.n_entry == 0 {
                continue;
            }
            append_table_hdr(&mut buf, self.patch, t);
            for p in t.changes() {
                buf.push(p.op);
                buf.push(p.indirect as u8);
                buf.extend_from_slice(&p.record);
                if let Some((x, ctx)) = out {
                    if buf.len() >= chunk {
                        let rc = x(ctx, buf.as_ptr() as *const ffi::c_void, buf.len() as i32);
                        buf.clear();
                        if rc != SQLITE_OK {
                            return Err(rc);
                        }
                    }
                }
            }
        }
        if let Some((x, ctx)) = out.take() {
            if !buf.is_empty() {
                let rc = x(ctx, buf.as_ptr() as *const ffi::c_void, buf.len() as i32);
                if rc != SQLITE_OK {
                    return Err(rc);
                }
            }
            buf.clear();
        }
        Ok(buf)
    }
}

/// sessionChangesetInvert.
unsafe fn invert(
    input: &mut SessionInput,
    out: Option<(XOutput, *mut ffi::c_void)>,
) -> Result<Vec<u8>, i32> {
    let mut s_out: Vec<u8> = Vec::new();
    let mut n_col = 0usize;
    let mut pk: Vec<u8> = Vec::new();
    let chunk = strm_chunk_size() as usize;
    loop {
        let rc = input.buffer(2);
        if rc != SQLITE_OK {
            return Err(rc);
        }
        if input.i_next + 1 >= input.n_data {
            if input.i_next != input.n_data {
                return Err(CORRUPT);
            }
            break;
        }
        let e = input.byte(input.i_next);
        match e {
            b'T' => {
                input.i_next += 1;
                let n_byte = input.buffer_tblhdr()?;
                let data = input.data();
                let (nc, nv) = get_varint(&data[input.i_next..]);
                n_col = nc as usize;
                pk = sl(data, input.i_next + nv, n_col).to_vec();
                s_out.push(e);
                s_out.extend_from_slice(sl(data, input.i_next, n_byte));
                input.i_next += n_byte;
            }
            SQLITE_INSERT | SQLITE_DELETE => {
                let indirect = input.byte(input.i_next + 1);
                let e2 = if e == SQLITE_DELETE {
                    SQLITE_INSERT
                } else {
                    SQLITE_DELETE
                };
                input.i_next += 2;
                let n_byte = input.buffer_record(n_col)?;
                s_out.push(e2);
                s_out.push(indirect);
                s_out.extend_from_slice(sl(input.data(), input.i_next, n_byte));
                input.i_next += n_byte;
            }
            SQLITE_UPDATE => {
                s_out.push(e);
                s_out.push(input.byte(input.i_next + 1));
                input.i_next += 2;
                let mut vals: Vec<Option<Val>> = vec![None; n_col * 2];
                let mut rc = input.read_record(n_col, None, &mut vals[..n_col], None);
                if rc == SQLITE_OK {
                    rc = input.read_record(n_col, None, &mut vals[n_col..], None);
                }
                if rc != SQLITE_OK {
                    return Err(rc);
                }
                let is_pk = |i: usize| pk.get(i).copied().unwrap_or(0) != 0;
                // New old.*: the PK from old.*, other values from new.*.
                for i in 0..n_col {
                    let v = &vals[i + if is_pk(i) { 0 } else { n_col }];
                    Val::serialize(v.as_ref(), &mut s_out);
                }
                // New new.*: old.* values, PK undefined.
                for i in 0..n_col {
                    let v = if is_pk(i) { None } else { vals[i].as_ref() };
                    Val::serialize(v, &mut s_out);
                }
            }
            _ => return Err(CORRUPT),
        }
        if let Some((x, ctx)) = out {
            if s_out.len() >= chunk {
                let rc = x(
                    ctx,
                    s_out.as_ptr() as *const ffi::c_void,
                    s_out.len() as i32,
                );
                s_out.clear();
                if rc != SQLITE_OK {
                    return Err(rc);
                }
            }
        }
    }
    if let Some((x, ctx)) = out {
        if !s_out.is_empty() {
            let rc = x(
                ctx,
                s_out.as_ptr() as *const ffi::c_void,
                s_out.len() as i32,
            );
            if rc != SQLITE_OK {
                return Err(rc);
            }
        }
        s_out.clear();
    }
    Ok(s_out)
}

/// Stores a result buffer in (*pn, *pp), or zeroes them on error.
unsafe fn give(
    res: Result<Vec<u8>, i32>,
    pn: *mut ffi::c_int,
    pp: *mut *mut ffi::c_void,
) -> ffi::c_int {
    if !pn.is_null() {
        *pn = 0;
    }
    if !pp.is_null() {
        *pp = ptr::null_mut();
    }
    match res {
        Ok(buf) => {
            if !pn.is_null() {
                *pn = buf.len() as i32;
            }
            if !pp.is_null() {
                *pp = to_sqlite_buf(&buf);
            }
            SQLITE_OK
        }
        Err(rc) => rc,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_invert(
    n_in: ffi::c_int,
    p_in: *const ffi::c_void,
    pn_out: *mut ffi::c_int,
    pp_out: *mut *mut ffi::c_void,
) -> ffi::c_int {
    let mut input = SessionInput::from_buffer(p_in as *const u8, n_in.max(0) as usize);
    give(invert(&mut input, None), pn_out, pp_out)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_invert_strm(
    x_input: Option<XInput>,
    p_in: *mut ffi::c_void,
    x_output: Option<XOutput>,
    p_out: *mut ffi::c_void,
) -> ffi::c_int {
    let (Some(xi), Some(xo)) = (x_input, x_output) else {
        return SQLITE_MISUSE;
    };
    let mut input = SessionInput::from_stream(xi, p_in);
    invert(&mut input, Some((xo, p_out)))
        .err()
        .unwrap_or(SQLITE_OK)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_new(pp: *mut *mut Changegroup) -> ffi::c_int {
    if pp.is_null() {
        return SQLITE_MISUSE;
    }
    *pp = Box::into_raw(Box::default());
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_config(
    p_grp: *mut Changegroup,
    op: ffi::c_int,
    p_arg: *mut ffi::c_void,
) -> ffi::c_int {
    if p_grp.is_null() {
        return SQLITE_MISUSE;
    }
    let g = &mut *p_grp;
    match op {
        SQLITE_CHANGEGROUP_CONFIG_PATCHSET => {
            if p_arg.is_null() {
                return SQLITE_MISUSE;
            }
            let arg = *(p_arg as *mut ffi::c_int);
            if g.tables.is_empty() && arg >= 0 {
                g.patch = arg > 0;
            }
            *(p_arg as *mut ffi::c_int) = g.patch as ffi::c_int;
            SQLITE_OK
        }
        _ => SQLITE_MISUSE,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_schema(
    p_grp: *mut Changegroup,
    db: *mut sqlite3,
    z_db: *const ffi::c_char,
) -> ffi::c_int {
    if p_grp.is_null() {
        return SQLITE_MISUSE;
    }
    let g = &mut *p_grp;
    if !g.tables.is_empty() || g.db.is_some() {
        // Only once, and before any change is added.
        return SQLITE_MISUSE;
    }
    g.zdb = cstr(z_db);
    g.db = Some(db);
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_add(
    p_grp: *mut Changegroup,
    n_data: ffi::c_int,
    p_data: *mut ffi::c_void,
) -> ffi::c_int {
    if p_grp.is_null() {
        return SQLITE_MISUSE;
    }
    let mut it = ChangesetIter::new(
        SessionInput::from_buffer(p_data as *const u8, n_data.max(0) as usize),
        false,
        false,
    );
    (*p_grp).add_iter(&mut it, false)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_add_strm(
    p_grp: *mut Changegroup,
    x_input: Option<XInput>,
    p_in: *mut ffi::c_void,
) -> ffi::c_int {
    let (Some(x), false) = (x_input, p_grp.is_null()) else {
        return SQLITE_MISUSE;
    };
    let mut it = ChangesetIter::new(SessionInput::from_stream(x, p_in), false, false);
    (*p_grp).add_iter(&mut it, false)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_add_change(
    p_grp: *mut Changegroup,
    p_iter: *mut ChangesetIter,
) -> ffi::c_int {
    if p_grp.is_null() || p_iter.is_null() {
        return SQLITE_MISUSE;
    }
    let it = &mut *p_iter;
    if it.input.i_current == it.input.i_next || it.rc != SQLITE_OK || it.invert {
        // No current change, or an inverting iterator.
        return SQLITE_ERROR;
    }
    it.input.no_discard = true;
    (*p_grp).one_change_iter_to_hash(it, false)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_output(
    p_grp: *mut Changegroup,
    pn_data: *mut ffi::c_int,
    pp_data: *mut *mut ffi::c_void,
) -> ffi::c_int {
    if p_grp.is_null() {
        return SQLITE_MISUSE;
    }
    give((*p_grp).output(None), pn_data, pp_data)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_output_strm(
    p_grp: *mut Changegroup,
    x_output: Option<XOutput>,
    p_out: *mut ffi::c_void,
) -> ffi::c_int {
    let (Some(x), false) = (x_output, p_grp.is_null()) else {
        return SQLITE_MISUSE;
    };
    (*p_grp).output(Some((x, p_out))).err().unwrap_or(SQLITE_OK)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_delete(p_grp: *mut Changegroup) {
    if !p_grp.is_null() {
        drop(Box::from_raw(p_grp));
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_concat(
    n_a: ffi::c_int,
    p_a: *mut ffi::c_void,
    n_b: ffi::c_int,
    p_b: *mut ffi::c_void,
    pn_out: *mut ffi::c_int,
    pp_out: *mut *mut ffi::c_void,
) -> ffi::c_int {
    let mut g = Changegroup::default();
    let mut rc = sqlite3changegroup_add(&mut g, n_a, p_a);
    if rc == SQLITE_OK {
        rc = sqlite3changegroup_add(&mut g, n_b, p_b);
    }
    if rc == SQLITE_OK {
        rc = give(g.output(None), pn_out, pp_out);
    }
    rc
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_concat_strm(
    x_input_a: Option<XInput>,
    p_in_a: *mut ffi::c_void,
    x_input_b: Option<XInput>,
    p_in_b: *mut ffi::c_void,
    x_output: Option<XOutput>,
    p_out: *mut ffi::c_void,
) -> ffi::c_int {
    let mut g = Changegroup::default();
    let mut rc = sqlite3changegroup_add_strm(&mut g, x_input_a, p_in_a);
    if rc == SQLITE_OK {
        rc = sqlite3changegroup_add_strm(&mut g, x_input_b, p_in_b);
    }
    if rc == SQLITE_OK {
        rc = sqlite3changegroup_output_strm(&mut g, x_output, p_out);
    }
    rc
}

// sqlite3changegroup_change_* ---------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_change_begin(
    p_grp: *mut Changegroup,
    e_op: ffi::c_int,
    z_tab: *const ffi::c_char,
    b_indirect: ffi::c_int,
    pz_err: *mut *mut ffi::c_char,
) -> ffi::c_int {
    if p_grp.is_null() {
        return SQLITE_MISUSE;
    }
    let g = &mut *p_grp;
    let op = e_op as u8;
    if g.cd.tab.is_some() {
        return SQLITE_MISUSE;
    }
    if e_op != SQLITE_INSERT as i32 && e_op != SQLITE_UPDATE as i32 && e_op != SQLITE_DELETE as i32
    {
        return SQLITE_ERROR;
    }
    let name = cstr(z_tab);
    let ti = match g.find_table(&name, None) {
        Ok(t) => t,
        Err(rc) => {
            if !pz_err.is_null() {
                *pz_err = mprintf(&format!("no such table: {name}"));
            }
            return rc;
        }
    };
    let n_col = g.tables[ti].n_col;
    let n_req = n_col * if op == SQLITE_UPDATE { 2 } else { 1 };
    g.cd.tab = Some(ti);
    g.cd.op = op;
    g.cd.indirect = b_indirect != 0;
    if g.cd.bufs.len() < n_req {
        g.cd.bufs.resize(n_req, Vec::new());
    }
    SQLITE_OK
}

/// checkChangeParams: the buffer for column `i_col`, emptied.
fn change_buf(g: &mut Changegroup, b_new: bool, i_col: ffi::c_int) -> Result<&mut Vec<u8>, i32> {
    let Some(ti) = g.cd.tab else {
        return Err(SQLITE_MISUSE);
    };
    let n_col = g.tables[ti].n_col;
    if i_col < 0 || i_col as usize >= n_col {
        return Err(SQLITE_RANGE);
    }
    if (b_new && g.cd.op == SQLITE_DELETE) || (!b_new && g.cd.op == SQLITE_INSERT) {
        return Err(SQLITE_ERROR);
    }
    let mut i = i_col as usize;
    if g.cd.op == SQLITE_UPDATE && b_new {
        i += n_col;
    }
    let b = &mut g.cd.bufs[i];
    b.clear();
    Ok(b)
}

unsafe fn change_val(
    p_grp: *mut Changegroup,
    b_new: ffi::c_int,
    i_col: ffi::c_int,
    v: Val,
) -> ffi::c_int {
    if p_grp.is_null() {
        return SQLITE_MISUSE;
    }
    match change_buf(&mut *p_grp, b_new != 0, i_col) {
        Ok(b) => {
            Val::serialize(Some(&v), b);
            SQLITE_OK
        }
        Err(rc) => rc,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_change_int64(
    p_grp: *mut Changegroup,
    b_new: ffi::c_int,
    i_col: ffi::c_int,
    i_val: i64,
) -> ffi::c_int {
    change_val(p_grp, b_new, i_col, Val::Int(i_val))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_change_null(
    p_grp: *mut Changegroup,
    b_new: ffi::c_int,
    i_col: ffi::c_int,
) -> ffi::c_int {
    change_val(p_grp, b_new, i_col, Val::Null)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_change_double(
    p_grp: *mut Changegroup,
    b_new: ffi::c_int,
    i_col: ffi::c_int,
    f_val: f64,
) -> ffi::c_int {
    change_val(p_grp, b_new, i_col, Val::Float(f_val))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_change_text(
    p_grp: *mut Changegroup,
    b_new: ffi::c_int,
    i_col: ffi::c_int,
    p_val: *const ffi::c_char,
    n_val: ffi::c_int,
) -> ffi::c_int {
    let bytes = if p_val.is_null() {
        Vec::new()
    } else if n_val >= 0 {
        std::slice::from_raw_parts(p_val as *const u8, n_val as usize).to_vec()
    } else {
        CStr::from_ptr(p_val).to_bytes().to_vec()
    };
    change_val(p_grp, b_new, i_col, Val::Text(bytes))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_change_blob(
    p_grp: *mut Changegroup,
    b_new: ffi::c_int,
    i_col: ffi::c_int,
    p_val: *const ffi::c_void,
    n_val: ffi::c_int,
) -> ffi::c_int {
    let bytes = if p_val.is_null() || n_val <= 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(p_val as *const u8, n_val as usize).to_vec()
    };
    change_val(p_grp, b_new, i_col, Val::Blob(bytes))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changegroup_change_finish(
    p_grp: *mut Changegroup,
    b_discard: ffi::c_int,
    pz_err: *mut *mut ffi::c_char,
) -> ffi::c_int {
    if p_grp.is_null() {
        return SQLITE_MISUSE;
    }
    let g = &mut *p_grp;
    let mut rc = SQLITE_OK;
    let mut err: Option<String> = None;
    if let Some(ti) = g.cd.tab {
        let n_col = g.tables[ti].n_col;
        let op = g.cd.op;
        if b_discard == 0 {
            let pk = g.tables[ti].pk.clone();
            let bufs = &g.cd.bufs;
            let mut n_buf = n_col;
            let mut undef = SQLITE_NULL;
            if op == SQLITE_UPDATE {
                for ii in 0..n_col {
                    if pk[ii] != 0 {
                        if bufs[ii].len() <= 1 {
                            err = Some(format!(
                                "invalid change: {} value in PK of old.* record",
                                if bufs[ii].len() == 1 {
                                    "null"
                                } else {
                                    "undefined"
                                }
                            ));
                            rc = SQLITE_ERROR;
                            break;
                        } else if !bufs[ii + n_col].is_empty() {
                            err = Some(
                                "invalid change: defined value in PK of new.* record".to_string(),
                            );
                            rc = SQLITE_ERROR;
                            break;
                        }
                    } else if !g.patch && bufs[ii].is_empty() != bufs[ii + n_col].is_empty() {
                        err = Some(format!(
                            "invalid change: column {ii} - old.* value is {}defined but new.* is {}defined",
                            if bufs[ii].is_empty() { "un" } else { "" },
                            if bufs[ii + n_col].is_empty() { "un" } else { "" }
                        ));
                        rc = SQLITE_ERROR;
                        break;
                    }
                }
                undef = 0;
                if !g.patch {
                    n_buf *= 2;
                }
            } else {
                for ii in 0..n_col {
                    let is_pk = pk[ii] != 0;
                    if (op == SQLITE_INSERT || !g.patch || is_pk) && bufs[ii].is_empty() {
                        err = Some(format!("invalid change: column {ii} is undefined"));
                        rc = SQLITE_ERROR;
                        break;
                    }
                    if bufs[ii].len() == 1 && is_pk {
                        err = Some("invalid change: null value in PK".to_string());
                        rc = SQLITE_ERROR;
                        break;
                    }
                }
            }
            let mut record = Vec::new();
            for ii in 0..n_buf {
                let mut idx = ii;
                if g.patch && pk.get(ii).copied().unwrap_or(0) == 0 {
                    if op == SQLITE_UPDATE {
                        idx += n_col;
                    } else if op == SQLITE_DELETE {
                        continue;
                    }
                }
                let b = &bufs[idx];
                if b.is_empty() {
                    record.push(undef);
                } else {
                    record.extend_from_slice(b);
                }
            }
            if rc == SQLITE_OK {
                let indirect = g.cd.indirect;
                rc = g.one_change_to_hash(ti, op, indirect, n_col, &record, false);
            }
        }
        for b in g.cd.bufs.iter_mut() {
            b.clear();
        }
        g.cd.tab = None;
    }
    if !pz_err.is_null() {
        *pz_err = err.map_or(ptr::null_mut(), |e| mprintf(&e));
    }
    rc
}
