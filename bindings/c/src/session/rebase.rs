//! sqlite3_rebaser: rebases changesets against the conflict resolutions
//! recorded by sqlite3changeset_apply_v2.

use super::group::{change_equal, skip_record, Changegroup};
use super::iter::{ChangesetIter, NextOut, SessionInput};
use super::*;

/// sqlite3_rebaser.
#[derive(Default)]
pub struct Rebaser {
    grp: Changegroup,
}

fn field_len(b: &[u8], o: usize) -> usize {
    if o >= b.len() {
        0
    } else {
        serial_len(&b[o..])
    }
}

fn sl(b: &[u8], start: usize, len: usize) -> &[u8] {
    let s = start.min(b.len());
    &b[s..(start + len).min(b.len())]
}

/// sessionAppendRecordMerge: a1, with its undefined fields taken from a2.
fn append_record_merge(out: &mut Vec<u8>, n_col: usize, a1: &[u8], a2: &[u8]) {
    let (mut o1, mut o2) = (0, 0);
    for _ in 0..n_col {
        let nn1 = field_len(a1, o1);
        let nn2 = field_len(a2, o2);
        let t1 = a1.get(o1).copied().unwrap_or(0);
        if nn1 == 0 || (nn2 > 0 && (t1 == 0 || t1 == 0xFF)) {
            out.extend_from_slice(sl(a2, o2, nn2));
        } else {
            out.extend_from_slice(sl(a1, o1, nn1));
        }
        o1 += nn1;
        o2 += nn2;
    }
}

/// sessionAppendPartialUpdate.
fn append_partial_update(out: &mut Vec<u8>, it: &ChangesetIter, rec: &[u8], change: &[u8]) {
    let start = out.len();
    let mut data = false;
    out.push(SQLITE_UPDATE);
    out.push(it.indirect as u8);
    let (mut a1, mut a2) = (0, 0);
    for i in 0..it.n_col {
        let n1 = field_len(rec, a1);
        let n2 = field_len(change, a2);
        let t1 = rec.get(a1).copied().unwrap_or(0);
        let t2 = change.get(a2).copied().unwrap_or(0);
        if n2 == 0 || it.pk[i] != 0 || t2 == 0 {
            if it.pk[i] == 0 && t1 != 0 {
                data = true;
            }
            out.extend_from_slice(sl(rec, a1, n1));
        } else if t2 != 0xFF && t1 != 0 {
            data = true;
            out.extend_from_slice(sl(change, a2, n2));
        } else {
            out.push(0);
        }
        a1 += n1;
        a2 += n2;
    }
    if data {
        let mut a2 = 0;
        for i in 0..it.n_col {
            let n1 = field_len(rec, a1);
            let n2 = field_len(change, a2);
            let t2 = change.get(a2).copied().unwrap_or(0);
            if it.pk[i] != 0 || t2 != 0xFF {
                out.extend_from_slice(sl(rec, a1, n1));
            } else {
                out.push(0);
            }
            a1 += n1;
            a2 += n2;
        }
    } else {
        out.truncate(start);
    }
}

/// sessionRebase.
unsafe fn rebase(
    p: &Rebaser,
    it: &mut ChangesetIter,
    out_fn: Option<(XOutput, *mut ffi::c_void)>,
) -> Result<Vec<u8>, i32> {
    let mut rc = SQLITE_OK;
    let mut out: Vec<u8> = Vec::new();
    let mut tab: Option<&SessionTable> = None;
    let chunk = strm_chunk_size() as usize;
    loop {
        let mut nx = NextOut::default();
        if it.next(true, &mut nx) != SQLITE_ROW {
            break;
        }
        let (ro, rn) = nx.rec.unwrap_or((0, 0));
        let rec = it.rec((ro, rn)).to_vec();
        if nx.new_table {
            let name = it.tab_name();
            tab = p
                .grp
                .tables
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case(&name));
            if it.patchset {
                // A patchset cannot be rebased.
                rc = SQLITE_ERROR;
            }
            out.push(if it.patchset { b'P' } else { b'T' });
            put_varint(&mut out, it.n_col as u32);
            out.extend_from_slice(&it.pk);
            out.extend_from_slice(name.as_bytes());
            out.push(0);
        }
        let mut change: Option<&SessionChange> = None;
        if let (Some(t), SQLITE_OK) = (tab, rc) {
            if t.n_change() > 0 {
                let h = t.change_hash(false, &rec, t.n_change());
                change = t.buckets[h]
                    .iter()
                    .rev()
                    .find(|c| change_equal(t, false, &rec, false, &c.record));
            }
        }
        let mut done = false;
        if let Some(c) = change {
            match it.op {
                SQLITE_INSERT => {
                    if c.op == SQLITE_INSERT {
                        done = true;
                        if !c.indirect {
                            out.push(SQLITE_UPDATE);
                            out.push(it.indirect as u8);
                            out.extend_from_slice(&c.record);
                            out.extend_from_slice(&rec);
                        }
                    }
                }
                SQLITE_UPDATE => {
                    done = true;
                    if c.op == SQLITE_DELETE {
                        if !c.indirect {
                            let csr = skip_record(&rec, 0, it.n_col);
                            out.push(SQLITE_INSERT);
                            out.push(it.indirect as u8);
                            append_record_merge(
                                &mut out,
                                it.n_col,
                                sl(&rec, csr, rec.len()),
                                &c.record,
                            );
                        }
                    } else {
                        append_partial_update(&mut out, it, &rec, &c.record);
                    }
                }
                _ => {
                    done = true;
                    if c.op == SQLITE_INSERT {
                        out.push(SQLITE_DELETE);
                        out.push(it.indirect as u8);
                        append_record_merge(&mut out, it.n_col, &c.record, &rec);
                    }
                }
            }
        }
        if !done {
            out.push(it.op);
            out.push(it.indirect as u8);
            out.extend_from_slice(&rec);
        }
        if let Some((x, ctx)) = out_fn {
            if rc == SQLITE_OK && out.len() > chunk {
                rc = x(ctx, out.as_ptr() as *const ffi::c_void, out.len() as i32);
                out.clear();
            }
        }
        if rc != SQLITE_OK {
            break;
        }
    }
    if rc == SQLITE_OK {
        rc = it.rc;
    }
    if rc != SQLITE_OK {
        return Err(rc);
    }
    if let Some((x, ctx)) = out_fn {
        if !out.is_empty() {
            let rc = x(ctx, out.as_ptr() as *const ffi::c_void, out.len() as i32);
            if rc != SQLITE_OK {
                return Err(rc);
            }
        }
        out.clear();
    }
    Ok(out)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3rebaser_create(pp_new: *mut *mut Rebaser) -> ffi::c_int {
    if pp_new.is_null() {
        return SQLITE_MISUSE;
    }
    *pp_new = Box::into_raw(Box::default());
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3rebaser_configure(
    p: *mut Rebaser,
    n_rebase: ffi::c_int,
    p_rebase: *const ffi::c_void,
) -> ffi::c_int {
    if p.is_null() {
        return SQLITE_MISUSE;
    }
    let mut it = ChangesetIter::new(
        SessionInput::from_buffer(p_rebase as *const u8, n_rebase.max(0) as usize),
        false,
        false,
    );
    (*p).grp.add_iter(&mut it, true)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3rebaser_rebase(
    p: *mut Rebaser,
    n_in: ffi::c_int,
    p_in: *const ffi::c_void,
    pn_out: *mut ffi::c_int,
    pp_out: *mut *mut ffi::c_void,
) -> ffi::c_int {
    if p.is_null() {
        return SQLITE_MISUSE;
    }
    if !pn_out.is_null() {
        *pn_out = 0;
    }
    if !pp_out.is_null() {
        *pp_out = ptr::null_mut();
    }
    let mut it = ChangesetIter::new(
        SessionInput::from_buffer(p_in as *const u8, n_in.max(0) as usize),
        false,
        false,
    );
    match rebase(&*p, &mut it, None) {
        Ok(buf) => {
            if !pn_out.is_null() {
                *pn_out = buf.len() as i32;
            }
            if !pp_out.is_null() {
                *pp_out = to_sqlite_buf(&buf);
            }
            SQLITE_OK
        }
        Err(rc) => rc,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3rebaser_rebase_strm(
    p: *mut Rebaser,
    x_input: Option<XInput>,
    p_in: *mut ffi::c_void,
    x_output: Option<XOutput>,
    p_out: *mut ffi::c_void,
) -> ffi::c_int {
    let (Some(xi), Some(xo), false) = (x_input, x_output, p.is_null()) else {
        return SQLITE_MISUSE;
    };
    let mut it = ChangesetIter::new(SessionInput::from_stream(xi, p_in), false, false);
    rebase(&*p, &mut it, Some((xo, p_out)))
        .err()
        .unwrap_or(SQLITE_OK)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3rebaser_delete(p: *mut Rebaser) {
    if !p.is_null() {
        drop(Box::from_raw(p));
    }
}
