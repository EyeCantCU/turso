//! Changeset input buffering and the sqlite3changeset_iter API.

use super::*;

/// An sqlite3_value handed out by an iterator; freed with it.
pub(crate) struct OwnedValue(pub(crate) Box<ExtValue>);

impl OwnedValue {
    pub(crate) fn new(v: &Val) -> Self {
        OwnedValue(Box::new(v.to_ext()))
    }

    pub(crate) fn ptr(&self) -> *mut ffi::c_void {
        &*self.0 as *const ExtValue as *mut ffi::c_void
    }
}

impl Drop for OwnedValue {
    fn drop(&mut self) {
        let v = std::mem::replace(&mut *self.0, ExtValue::null());
        unsafe { v.__free_internal_type() };
    }
}

/// SessionInput: a changeset in memory or read from an xInput stream.
pub(crate) struct SessionInput {
    pub(crate) no_discard: bool,
    pub(crate) i_current: usize,
    pub(crate) i_next: usize,
    ext: *const u8,
    pub(crate) n_data: usize,
    buf: Vec<u8>,
    x_input: Option<(XInput, *mut ffi::c_void)>,
    pub(crate) eof: bool,
}

impl SessionInput {
    pub(crate) fn from_buffer(data: *const u8, n: usize) -> Self {
        SessionInput {
            no_discard: false,
            i_current: 0,
            i_next: 0,
            ext: data,
            n_data: if data.is_null() { 0 } else { n },
            buf: Vec::new(),
            x_input: None,
            eof: true,
        }
    }

    pub(crate) fn from_stream(x: XInput, ctx: *mut ffi::c_void) -> Self {
        SessionInput {
            no_discard: false,
            i_current: 0,
            i_next: 0,
            ext: ptr::null(),
            n_data: 0,
            buf: Vec::new(),
            x_input: Some((x, ctx)),
            eof: false,
        }
    }

    pub(crate) fn is_stream(&self) -> bool {
        self.x_input.is_some()
    }

    pub(crate) fn data(&self) -> &[u8] {
        if self.x_input.is_some() {
            &self.buf[..self.n_data]
        } else if self.ext.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.ext, self.n_data) }
        }
    }

    pub(crate) fn byte(&self, i: usize) -> u8 {
        self.data().get(i).copied().unwrap_or(0)
    }

    /// sessionDiscardData.
    pub(crate) fn discard(&mut self) {
        if self.x_input.is_some() && self.i_current >= strm_chunk_size() as usize {
            let n_move = self.buf.len() - self.i_current;
            self.buf.copy_within(self.i_current.., 0);
            self.buf.truncate(n_move);
            self.i_next -= self.i_current;
            self.i_current = 0;
            self.n_data = self.buf.len();
        }
    }

    /// sessionInputBuffer: make `n_byte` bytes past i_next available.
    pub(crate) unsafe fn buffer(&mut self, n_byte: usize) -> i32 {
        let mut rc = SQLITE_OK;
        if let Some((x, ctx)) = self.x_input {
            while !self.eof && self.i_next + n_byte >= self.n_data && rc == SQLITE_OK {
                let mut n_new = strm_chunk_size();
                if !self.no_discard {
                    self.discard();
                }
                let old = self.buf.len();
                self.buf.resize(old + n_new as usize, 0);
                rc = x(
                    ctx,
                    self.buf[old..].as_mut_ptr() as *mut ffi::c_void,
                    &mut n_new,
                );
                let n_new = n_new.max(0) as usize;
                self.buf.truncate(old + n_new);
                if n_new == 0 {
                    self.eof = true;
                }
                self.n_data = self.buf.len();
            }
        }
        rc
    }

    /// sessionChangesetBufferTblhdr: size of the table header after 'T'/'P'.
    pub(crate) unsafe fn buffer_tblhdr(&mut self) -> Result<usize, i32> {
        let mut rc = self.buffer(9);
        let mut n_read = 0usize;
        if rc == SQLITE_OK {
            let n_buf = self.n_data - self.i_next.min(self.n_data);
            let (n_col, nv) = get_varint_safe(&self.data()[self.i_next.min(self.n_data)..], n_buf);
            n_read += nv;
            if !(0..=65536).contains(&n_col) {
                rc = CORRUPT;
            } else {
                rc = self.buffer(n_read + n_col as usize + 100);
                n_read += n_col as usize;
            }
        }
        while rc == SQLITE_OK {
            while self.i_next + n_read < self.n_data && self.byte(self.i_next + n_read) != 0 {
                n_read += 1;
            }
            if self.i_next + n_read < self.n_data {
                break;
            }
            rc = self.buffer(n_read + 100);
            if rc == SQLITE_OK && self.i_next + n_read >= self.n_data {
                rc = CORRUPT;
            }
        }
        if rc != SQLITE_OK {
            return Err(rc);
        }
        Ok(n_read + 1)
    }

    /// sessionChangesetBufferRecord: size of a record of `n_col` fields.
    pub(crate) unsafe fn buffer_record(&mut self, n_col: usize) -> Result<usize, i32> {
        let mut rc = SQLITE_OK;
        let mut n_byte = 0usize;
        for _ in 0..n_col {
            if rc != SQLITE_OK {
                break;
            }
            rc = self.buffer(n_byte + 10);
            if rc == SQLITE_OK {
                if self.i_next + n_byte >= self.n_data {
                    rc = CORRUPT;
                } else {
                    let e = self.byte(self.i_next + n_byte);
                    n_byte += 1;
                    if e == SQLITE_TEXT || e == SQLITE_BLOB {
                        let at = self.i_next + n_byte;
                        let n_rem = self.n_data - at.min(self.n_data);
                        let (n, nv) = get_varint_safe(&self.data()[at.min(self.n_data)..], n_rem);
                        n_byte += nv + n as usize;
                        rc = self.buffer(n_byte);
                    } else if e == SQLITE_INTEGER || e == SQLITE_FLOAT {
                        n_byte += 8;
                    } else if e != 0 && e != SQLITE_NULL {
                        rc = CORRUPT;
                    }
                }
            }
            if rc == SQLITE_OK && self.i_next + n_byte > self.n_data {
                rc = CORRUPT;
            }
        }
        if rc != SQLITE_OK {
            return Err(rc);
        }
        Ok(n_byte)
    }

    /// sessionReadRecord: values of one record (only PK fields if `pk`).
    /// Sets `empty` when no field is defined.
    pub(crate) unsafe fn read_record(
        &mut self,
        n_col: usize,
        pk: Option<&[u8]>,
        out: &mut [Option<Val>],
        empty: Option<&mut bool>,
    ) -> i32 {
        let mut rc = SQLITE_OK;
        let mut is_empty = true;
        for i in 0..n_col {
            if rc != SQLITE_OK {
                break;
            }
            if let Some(pk) = pk {
                if pk[i] == 0 {
                    continue;
                }
            }
            let mut e = 0u8;
            rc = self.buffer(9);
            if rc == SQLITE_OK {
                if self.i_next >= self.n_data {
                    rc = CORRUPT;
                } else {
                    e = self.byte(self.i_next);
                    self.i_next += 1;
                    if e != 0 {
                        is_empty = false;
                    }
                }
            }
            if rc == SQLITE_OK {
                if e == SQLITE_TEXT || e == SQLITE_BLOB {
                    let n_rem = self.n_data - self.i_next;
                    let (n, nv) = get_varint_safe(&self.data()[self.i_next..], n_rem);
                    self.i_next += nv;
                    rc = self.buffer(n as usize);
                    if rc == SQLITE_OK {
                        if n < 0 || self.i_next + n as usize > self.n_data {
                            rc = CORRUPT;
                        } else {
                            let bytes = self.data()[self.i_next..self.i_next + n as usize].to_vec();
                            out[i] = Some(if e == SQLITE_TEXT {
                                Val::Text(bytes)
                            } else {
                                Val::Blob(bytes)
                            });
                            self.i_next += n as usize;
                        }
                    }
                } else if e == SQLITE_INTEGER || e == SQLITE_FLOAT {
                    if self.i_next + 8 > self.n_data {
                        rc = CORRUPT;
                    } else {
                        let v = get_i64(&self.data()[self.i_next..]);
                        out[i] = Some(if e == SQLITE_INTEGER {
                            Val::Int(v)
                        } else {
                            Val::Float(f64::from_bits(v as u64))
                        });
                        self.i_next += 8;
                    }
                } else if e != 0 {
                    // NULL; other type bytes are kept as NULL values, as
                    // sqlite3ValueNew leaves them.
                    out[i] = Some(Val::Null);
                }
            }
        }
        if let Some(empty) = empty {
            *empty = is_empty;
        }
        rc
    }
}

/// sqlite3_changeset_iter.
pub struct ChangesetIter {
    pub(crate) input: SessionInput,
    pub(crate) patchset: bool,
    pub(crate) invert: bool,
    pub(crate) skip_empty: bool,
    pub(crate) rc: i32,
    pub(crate) conflict: *mut sqlite3_stmt,
    pub(crate) tab: Option<CString>,
    pub(crate) n_col: usize,
    pub(crate) op: u8,
    pub(crate) indirect: bool,
    pub(crate) pk: Vec<u8>,
    /// old.* then new.* values.
    pub(crate) vals: Vec<Option<Val>>,
    ext: Vec<Option<OwnedValue>>,
    /// Set for the iterator passed to an SQLITE_CHANGESET_FOREIGN_KEY handler.
    pub(crate) fk_mode: bool,
}

/// What sessionChangesetNextOne returns besides its code.
#[derive(Default)]
pub(crate) struct NextOut {
    /// (offset, length) of the raw record in the input.
    pub(crate) rec: Option<(usize, usize)>,
    pub(crate) new_table: bool,
    pub(crate) empty: bool,
}

impl ChangesetIter {
    pub(crate) fn new(input: SessionInput, invert: bool, skip_empty: bool) -> Self {
        ChangesetIter {
            input,
            patchset: false,
            invert,
            skip_empty,
            rc: SQLITE_OK,
            conflict: ptr::null_mut(),
            tab: None,
            n_col: 0,
            op: 0,
            indirect: false,
            pk: Vec::new(),
            vals: Vec::new(),
            ext: Vec::new(),
            fk_mode: false,
        }
    }

    pub(crate) fn tab_name(&self) -> String {
        self.tab
            .as_ref()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub(crate) fn old(&self, i: usize) -> Option<&Val> {
        self.vals.get(i).and_then(|v| v.as_ref())
    }

    pub(crate) fn new_val(&self, i: usize) -> Option<&Val> {
        self.vals.get(self.n_col + i).and_then(|v| v.as_ref())
    }

    fn clear_values(&mut self) {
        for v in self.vals.iter_mut() {
            *v = None;
        }
        for v in self.ext.iter_mut() {
            *v = None;
        }
    }

    /// sessionChangesetReadTblhdr.
    unsafe fn read_tblhdr(&mut self) -> i32 {
        let mut rc = SQLITE_OK;
        match self.input.buffer_tblhdr() {
            Ok(mut n_copy) => {
                let (n_col, nv) = get_varint(&self.input.data()[self.input.i_next..]);
                if n_col > 0 {
                    n_copy -= nv;
                    self.input.i_next += nv;
                    let n_col = n_col as usize;
                    let at = self.input.i_next;
                    let hdr = self.input.data()[at..at + n_copy].to_vec();
                    self.input.i_next += n_copy;
                    self.n_col = n_col;
                    self.pk = hdr[..n_col.min(hdr.len())].to_vec();
                    let name = &hdr[n_col.min(hdr.len())..];
                    let name = name.split(|b| *b == 0).next().unwrap_or(&[]);
                    self.tab = Some(CString::new(name.to_vec()).unwrap_or_default());
                    self.vals = vec![None; n_col * 2];
                    self.ext = (0..n_col * 2).map(|_| None).collect();
                } else {
                    rc = CORRUPT;
                }
            }
            Err(e) => rc = e,
        }
        if rc != SQLITE_OK {
            self.tab = None;
            self.pk.clear();
            self.vals.clear();
            self.ext.clear();
        }
        self.rc = rc;
        rc
    }

    /// sessionChangesetNextOne.
    pub(crate) unsafe fn next_one(&mut self, want_rec: bool, out: &mut NextOut) -> i32 {
        if self.rc != SQLITE_OK {
            return self.rc;
        }
        self.clear_values();
        self.rc = self.input.buffer(2);
        if self.rc != SQLITE_OK {
            return self.rc;
        }
        self.input.i_current = self.input.i_next;
        self.input.discard();
        if self.input.i_next >= self.input.n_data {
            return SQLITE_DONE;
        }
        let mut op = self.input.byte(self.input.i_next);
        self.input.i_next += 1;
        while op == b'T' || op == b'P' {
            out.new_table = true;
            self.patchset = op == b'P';
            if self.read_tblhdr() != SQLITE_OK {
                return self.rc;
            }
            self.rc = self.input.buffer(2);
            if self.rc != SQLITE_OK {
                return self.rc;
            }
            self.input.i_current = self.input.i_next;
            if self.input.i_next >= self.input.n_data {
                return SQLITE_DONE;
            }
            op = self.input.byte(self.input.i_next);
            self.input.i_next += 1;
        }
        if self.tab.is_none() || (self.patchset && self.invert) {
            self.rc = CORRUPT;
            return self.rc;
        }
        if (op != SQLITE_UPDATE && op != SQLITE_DELETE && op != SQLITE_INSERT)
            || self.input.i_next >= self.input.n_data
        {
            self.rc = CORRUPT;
            return self.rc;
        }
        self.op = op;
        self.indirect = self.input.byte(self.input.i_next) != 0;
        self.input.i_next += 1;

        if want_rec {
            let n_val = if !self.patchset && op == SQLITE_UPDATE {
                self.n_col * 2
            } else if self.patchset && op == SQLITE_DELETE {
                self.pk.iter().filter(|p| **p != 0).count()
            } else {
                self.n_col
            };
            match self.input.buffer_record(n_val) {
                Ok(n) => {
                    out.rec = Some((self.input.i_next, n));
                    self.input.i_next += n;
                }
                Err(e) => {
                    self.rc = e;
                    return e;
                }
            }
        } else {
            let n_col = self.n_col;
            let (old_off, new_off) = if self.invert { (n_col, 0) } else { (0, n_col) };
            let mut vals = std::mem::take(&mut self.vals);
            if self.op != SQLITE_INSERT && (!self.patchset || self.op == SQLITE_DELETE) {
                let pk = if self.patchset {
                    Some(self.pk.clone())
                } else {
                    None
                };
                self.rc = self.input.read_record(
                    n_col,
                    pk.as_deref(),
                    &mut vals[old_off..old_off + n_col],
                    None,
                );
                if self.rc != SQLITE_OK {
                    self.vals = vals;
                    return self.rc;
                }
            }
            if self.op != SQLITE_DELETE {
                let mut empty = false;
                self.rc = self.input.read_record(
                    n_col,
                    None,
                    &mut vals[new_off..new_off + n_col],
                    Some(&mut empty),
                );
                out.empty = empty;
                if self.rc != SQLITE_OK {
                    self.vals = vals;
                    return self.rc;
                }
            }
            if (self.patchset || self.invert) && self.op == SQLITE_UPDATE {
                for i in 0..n_col {
                    if self.pk[i] != 0 {
                        vals[i] = vals[i + n_col].take();
                        if vals[i].is_none() {
                            self.vals = vals;
                            self.rc = CORRUPT;
                            return self.rc;
                        }
                    }
                }
            } else if self.invert {
                if self.op == SQLITE_INSERT {
                    self.op = SQLITE_DELETE;
                } else if self.op == SQLITE_DELETE {
                    self.op = SQLITE_INSERT;
                }
            }
            if !self.patchset && self.op == SQLITE_UPDATE {
                for i in 0..n_col {
                    if self.pk[i] == 0 && vals[i + n_col].is_none() {
                        vals[i] = None;
                    }
                }
            }
            self.ext = vals
                .iter()
                .map(|v| v.as_ref().map(OwnedValue::new))
                .collect();
            self.vals = vals;
        }
        SQLITE_ROW
    }

    /// sessionChangesetNext.
    pub(crate) unsafe fn next(&mut self, want_rec: bool, out: &mut NextOut) -> i32 {
        loop {
            out.empty = false;
            let rc = self.next_one(want_rec, out);
            if !(rc == SQLITE_ROW && self.skip_empty && out.empty) {
                return rc;
            }
        }
    }

    /// The raw bytes of a record returned by `next(true, ..)`.
    pub(crate) fn rec(&self, r: (usize, usize)) -> &[u8] {
        &self.input.data()[r.0..r.0 + r.1]
    }

    pub(crate) fn set_ext(&mut self) {
        self.ext = self
            .vals
            .iter()
            .map(|v| v.as_ref().map(OwnedValue::new))
            .collect();
    }
}

pub(crate) unsafe fn start(
    pp: *mut *mut ChangesetIter,
    input: SessionInput,
    invert: bool,
) -> ffi::c_int {
    if pp.is_null() {
        return SQLITE_MISUSE;
    }
    *pp = Box::into_raw(Box::new(ChangesetIter::new(input, invert, false)));
    SQLITE_OK
}

const SQLITE_CHANGESETSTART_INVERT: i32 = 0x0002;

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_start(
    pp: *mut *mut ChangesetIter,
    n_changeset: ffi::c_int,
    p_changeset: *mut ffi::c_void,
) -> ffi::c_int {
    start(
        pp,
        SessionInput::from_buffer(p_changeset as *const u8, n_changeset.max(0) as usize),
        false,
    )
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_start_v2(
    pp: *mut *mut ChangesetIter,
    n_changeset: ffi::c_int,
    p_changeset: *mut ffi::c_void,
    flags: ffi::c_int,
) -> ffi::c_int {
    start(
        pp,
        SessionInput::from_buffer(p_changeset as *const u8, n_changeset.max(0) as usize),
        flags & SQLITE_CHANGESETSTART_INVERT != 0,
    )
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_start_strm(
    pp: *mut *mut ChangesetIter,
    x_input: Option<XInput>,
    p_in: *mut ffi::c_void,
) -> ffi::c_int {
    let Some(x) = x_input else {
        return SQLITE_MISUSE;
    };
    start(pp, SessionInput::from_stream(x, p_in), false)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_start_v2_strm(
    pp: *mut *mut ChangesetIter,
    x_input: Option<XInput>,
    p_in: *mut ffi::c_void,
    flags: ffi::c_int,
) -> ffi::c_int {
    let Some(x) = x_input else {
        return SQLITE_MISUSE;
    };
    start(
        pp,
        SessionInput::from_stream(x, p_in),
        flags & SQLITE_CHANGESETSTART_INVERT != 0,
    )
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_next(p: *mut ChangesetIter) -> ffi::c_int {
    if p.is_null() {
        return SQLITE_MISUSE;
    }
    (*p).next(false, &mut NextOut::default())
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_op(
    p: *mut ChangesetIter,
    pz_tab: *mut *const ffi::c_char,
    pn_col: *mut ffi::c_int,
    p_op: *mut ffi::c_int,
    pb_indirect: *mut ffi::c_int,
) -> ffi::c_int {
    if p.is_null() {
        return SQLITE_MISUSE;
    }
    let it = &*p;
    if !p_op.is_null() {
        *p_op = it.op as ffi::c_int;
    }
    if !pn_col.is_null() {
        *pn_col = it.n_col as ffi::c_int;
    }
    if !pz_tab.is_null() {
        *pz_tab = it.tab.as_ref().map_or(ptr::null(), |c| c.as_ptr());
    }
    if !pb_indirect.is_null() {
        *pb_indirect = it.indirect as ffi::c_int;
    }
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_pk(
    p: *mut ChangesetIter,
    pab_pk: *mut *mut ffi::c_uchar,
    pn_col: *mut ffi::c_int,
) -> ffi::c_int {
    if p.is_null() {
        return SQLITE_MISUSE;
    }
    let it = &mut *p;
    if !pab_pk.is_null() {
        *pab_pk = if it.pk.is_empty() {
            ptr::null_mut()
        } else {
            it.pk.as_mut_ptr()
        };
    }
    if !pn_col.is_null() {
        *pn_col = it.n_col as ffi::c_int;
    }
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_old(
    p: *mut ChangesetIter,
    i_val: ffi::c_int,
    pp_value: *mut *mut ffi::c_void,
) -> ffi::c_int {
    if p.is_null() {
        return SQLITE_MISUSE;
    }
    let it = &*p;
    if it.op != SQLITE_UPDATE && it.op != SQLITE_DELETE {
        return SQLITE_MISUSE;
    }
    if i_val < 0 || i_val as usize >= it.n_col {
        return SQLITE_RANGE;
    }
    *pp_value = it.ext[i_val as usize]
        .as_ref()
        .map_or(ptr::null_mut(), |v| v.ptr());
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_new(
    p: *mut ChangesetIter,
    i_val: ffi::c_int,
    pp_value: *mut *mut ffi::c_void,
) -> ffi::c_int {
    if p.is_null() {
        return SQLITE_MISUSE;
    }
    let it = &*p;
    if it.op != SQLITE_UPDATE && it.op != SQLITE_INSERT {
        return SQLITE_MISUSE;
    }
    if i_val < 0 || i_val as usize >= it.n_col {
        return SQLITE_RANGE;
    }
    *pp_value = it.ext[it.n_col + i_val as usize]
        .as_ref()
        .map_or(ptr::null_mut(), |v| v.ptr());
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_conflict(
    p: *mut ChangesetIter,
    i_val: ffi::c_int,
    pp_value: *mut *mut ffi::c_void,
) -> ffi::c_int {
    if p.is_null() {
        return SQLITE_MISUSE;
    }
    let it = &*p;
    if it.conflict.is_null() {
        return SQLITE_MISUSE;
    }
    if i_val < 0 || i_val as usize >= it.n_col {
        return SQLITE_RANGE;
    }
    *pp_value = sqlite3_column_value(it.conflict, i_val);
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_fk_conflicts(
    p: *mut ChangesetIter,
    pn_out: *mut ffi::c_int,
) -> ffi::c_int {
    if p.is_null() {
        return SQLITE_MISUSE;
    }
    let it = &*p;
    if !it.conflict.is_null() || !it.fk_mode {
        return SQLITE_MISUSE;
    }
    if !pn_out.is_null() {
        *pn_out = it.n_col as ffi::c_int;
    }
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_finalize(p: *mut ChangesetIter) -> ffi::c_int {
    if p.is_null() {
        return SQLITE_OK;
    }
    let it = Box::from_raw(p);
    it.rc
}
