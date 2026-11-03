//! sqlite3changeset_apply and its variants.

use super::iter::{ChangesetIter, NextOut, SessionInput};
use super::*;
use crate::{sqlite3_bind_parameter_count, sqlite3_changes, sqlite3_clear_bindings};

const SQLITE_CHANGESET_DATA: i32 = 1;
const SQLITE_CHANGESET_CONFLICT: i32 = 3;
const SQLITE_CHANGESET_FOREIGN_KEY: i32 = 5;
const SQLITE_CHANGESET_OMIT: i32 = 0;
const SQLITE_CHANGESET_REPLACE: i32 = 1;
const SQLITE_CHANGESET_ABORT: i32 = 2;

const SQLITE_CHANGESETAPPLY_NOSAVEPOINT: i32 = 0x0001;
const SQLITE_CHANGESETAPPLY_INVERT: i32 = 0x0002;
const SQLITE_CHANGESETAPPLY_IGNORENOOP: i32 = 0x0004;
const SQLITE_CHANGESETAPPLY_NOUPDATELOOP: i32 = 0x0010;

/// Number of prepared UPDATE statements to cache.
const SESSION_UPDATE_CACHE_SZ: usize = 12;

pub(crate) type XConflict =
    unsafe extern "C" fn(*mut ffi::c_void, ffi::c_int, *mut ChangesetIter) -> ffi::c_int;
pub(crate) type XFilter = unsafe extern "C" fn(*mut ffi::c_void, *const ffi::c_char) -> ffi::c_int;
pub(crate) type XFilterIter =
    unsafe extern "C" fn(*mut ffi::c_void, *mut ChangesetIter) -> ffi::c_int;

/// SessionApplyCtx.
struct ApplyCtx {
    db: *mut sqlite3,
    delete: Option<Stmt>,
    insert: Option<Stmt>,
    select: Option<Stmt>,
    n_col: usize,
    cols: Vec<String>,
    pk: Vec<u8>,
    /// Cached UPDATE statements by column mask, most recent first.
    up: Vec<(Vec<u32>, Stmt)>,
    stat1: bool,
    defer_constraints: bool,
    invert_constraints: bool,
    constraints: Vec<u8>,
    rebase: Vec<u8>,
    rebase_started: bool,
    want_rebase: bool,
    ignore_noop: bool,
    no_update_loop: bool,
    err: Option<String>,
}

impl ApplyCtx {
    /// sessionPrepare: records the error message on failure.
    unsafe fn prepare(&mut self, sql: &str) -> Result<Stmt, i32> {
        Stmt::prepare(self.db, sql).inspect_err(|_| {
            self.err = Some(errmsg(self.db));
        })
    }

    fn sel(&self) -> *mut sqlite3_stmt {
        self.select.as_ref().map_or(ptr::null_mut(), |s| s.0)
    }

    fn del(&self) -> *mut sqlite3_stmt {
        self.delete.as_ref().map_or(ptr::null_mut(), |s| s.0)
    }

    fn ins(&self) -> *mut sqlite3_stmt {
        self.insert.as_ref().map_or(ptr::null_mut(), |s| s.0)
    }

    /// sessionDeleteRow.
    unsafe fn delete_row(&mut self, tab: &str) -> i32 {
        let mut sql = format!("DELETE FROM main.{} WHERE ", ident(tab));
        let mut sep = "";
        let mut n_pk = 0;
        for i in 0..self.n_col {
            if self.pk[i] != 0 {
                n_pk += 1;
                sql.push_str(&format!("{sep}{} = ?{}", ident(&self.cols[i]), i + 1));
                sep = " AND ";
            }
        }
        if n_pk < self.n_col {
            sql.push_str(&format!(" AND (?{} OR ", self.n_col + 1));
            sep = "";
            for i in 0..self.n_col {
                if self.pk[i] == 0 {
                    sql.push_str(&format!("{sep}{} IS ?{}", ident(&self.cols[i]), i + 1));
                    sep = "AND ";
                }
            }
            sql.push(')');
        }
        match self.prepare(&sql) {
            Ok(s) => {
                self.delete = Some(s);
                SQLITE_OK
            }
            Err(e) => e,
        }
    }

    /// sessionSelectRow.
    unsafe fn select_row(&mut self, tab: &str) -> i32 {
        match select_stmt(
            self.db,
            self.ignore_noop,
            "main",
            tab,
            self.n_col,
            &self.cols,
            &self.pk,
        ) {
            Ok(s) => {
                self.select = Some(s);
                SQLITE_OK
            }
            Err(e) => {
                self.err = Some(errmsg(self.db));
                e
            }
        }
    }

    /// sessionInsertRow.
    unsafe fn insert_row(&mut self, tab: &str) -> i32 {
        let cols: Vec<String> = self.cols[..self.n_col].iter().map(|c| ident(c)).collect();
        let vars = vec!["?"; self.n_col].join(", ");
        let sql = format!(
            "INSERT INTO main.{}({}) VALUES({})",
            ident(tab),
            cols.join(", "),
            vars
        );
        match self.prepare(&sql) {
            Ok(s) => {
                self.insert = Some(s);
                SQLITE_OK
            }
            Err(e) => e,
        }
    }

    /// sessionStat1Sql.
    unsafe fn stat1_sql(&mut self) -> i32 {
        let rc = self.select_row("sqlite_stat1");
        if rc != SQLITE_OK {
            return rc;
        }
        match Stmt::prepare(
            self.db,
            "INSERT INTO main.sqlite_stat1 VALUES(?1, \
             CASE WHEN length(?2)=0 AND typeof(?2)='blob' THEN NULL ELSE ?2 END, \
             ?3)",
        ) {
            Ok(s) => self.insert = Some(s),
            Err(e) => return e,
        }
        match Stmt::prepare(
            self.db,
            "DELETE FROM main.sqlite_stat1 WHERE tbl=?1 AND idx IS \
             CASE WHEN length(?2)=0 AND typeof(?2)='blob' THEN NULL ELSE ?2 END \
             AND (?4 OR stat IS ?3)",
        ) {
            Ok(s) => self.delete = Some(s),
            Err(e) => return e,
        }
        SQLITE_OK
    }

    /// sessionUpdateFind.
    unsafe fn update_find(
        &mut self,
        it: &ChangesetIter,
        patchset: bool,
    ) -> Result<*mut sqlite3_stmt, i32> {
        let n_col = it.n_col;
        let n_u32 = (n_col + 33) / 32;
        let mut mask = vec![0u32; n_u32];
        let mut any = false;
        for ii in 0..n_col {
            if it.new_val(ii).is_some() {
                mask[ii / 32] |= 1 << (ii % 32);
                any = true;
            }
        }
        if !any {
            return Err(SQLITE_CORRUPT);
        }
        if patchset {
            mask[n_col / 32] |= 1 << (n_col % 32);
        }
        if let Some(pos) = self.up.iter().position(|(m, _)| *m == mask) {
            let e = self.up.remove(pos);
            self.up.insert(0, e);
            return Ok(self.up[0].1 .0);
        }
        if self.up.len() >= SESSION_UPDATE_CACHE_SZ {
            self.up.pop();
        }
        let stat1 = it.tab_name().eq_ignore_ascii_case("sqlite_stat1");
        let mut sql = format!("UPDATE main.{} SET ", ident(&it.tab_name()));
        let mut sep = "";
        for ii in 0..n_col {
            if self.pk[ii] == 0 && it.new_val(ii).is_some() {
                sql.push_str(&format!("{sep}{} = ?{}", ident(&self.cols[ii]), ii * 2 + 1));
                sep = ", ";
            }
        }
        sep = "";
        sql.push_str(" WHERE ");
        for ii in 0..n_col {
            if self.pk[ii] != 0 || (!patchset && it.old(ii).is_some()) {
                sql.push_str(sep);
                if stat1 && ii == 1 {
                    sql.push_str(
                        "idx IS CASE WHEN length(?4)=0 AND typeof(?4)='blob' THEN NULL ELSE ?4 END ",
                    );
                } else {
                    sql.push_str(&format!("{} IS ?{}", ident(&self.cols[ii]), ii * 2 + 2));
                }
                sep = " AND ";
            }
        }
        let stmt = Stmt::prepare(self.db, &sql)?;
        let p = stmt.0;
        self.up.insert(0, (mask, stmt));
        Ok(p)
    }

    /// sessionRebaseAdd.
    fn rebase_add(&mut self, e_type: i32, it: &ChangesetIter) -> i32 {
        if !self.want_rebase {
            return SQLITE_OK;
        }
        let op = it.op;
        if !self.rebase_started {
            self.rebase.push(b'T');
            put_varint(&mut self.rebase, self.n_col as u32);
            self.rebase.extend_from_slice(&self.pk[..self.n_col]);
            self.rebase.extend_from_slice(it.tab_name().as_bytes());
            self.rebase.push(0);
            self.rebase_started = true;
        }
        self.rebase.push(if op == SQLITE_DELETE {
            SQLITE_DELETE
        } else {
            SQLITE_INSERT
        });
        self.rebase.push((e_type == SQLITE_CHANGESET_REPLACE) as u8);
        for i in 0..self.n_col {
            let v = if op == SQLITE_DELETE || (op == SQLITE_UPDATE && self.pk[i] != 0) {
                it.old(i)
            } else {
                it.new_val(i)
            };
            Val::serialize(v, &mut self.rebase);
        }
        SQLITE_OK
    }
}

/// sqlite3_step then sqlite3_reset, which returns the step's error.
unsafe fn step_reset(stmt: *mut sqlite3_stmt) -> i32 {
    let s = sqlite3_step(stmt);
    let rc = sqlite3_reset(stmt);
    if rc == SQLITE_OK && s != SQLITE_ROW && s != SQLITE_DONE {
        s
    } else {
        rc
    }
}

/// sessionBindRow: binds old.* or new.* values (only PK ones if `pk`).
unsafe fn bind_row(
    it: &ChangesetIter,
    new: bool,
    n_col: usize,
    pk: Option<&[u8]>,
    stmt: *mut sqlite3_stmt,
) -> i32 {
    for i in 0..n_col {
        if pk.is_none_or(|pk| pk[i] != 0) {
            let v = if new { it.new_val(i) } else { it.old(i) };
            let rc = match v {
                None => SQLITE_CORRUPT,
                Some(v) => v.bind(stmt, i as i32 + 1),
            };
            if rc != SQLITE_OK {
                return rc;
            }
        }
    }
    SQLITE_OK
}

/// sessionSeekToRow.
unsafe fn seek_to_row(it: &ChangesetIter, p: &ApplyCtx) -> i32 {
    let sel = p.sel();
    sqlite3_clear_bindings(sel);
    let n_col = it.n_col;
    let mut rc = bind_row(it, it.op == SQLITE_INSERT, n_col, Some(&p.pk), sel);
    if it.op != SQLITE_DELETE && p.ignore_noop {
        for ii in 0..n_col {
            if rc != SQLITE_OK {
                break;
            }
            if p.pk[ii] == 0 {
                let v = it.new_val(ii);
                sqlite3_bind_int64(sel, (ii + 1 + n_col) as i32, v.is_none() as i64);
                if let Some(v) = v {
                    rc = v.bind(sel, ii as i32 + 1);
                }
            }
        }
    }
    if rc == SQLITE_OK {
        rc = sqlite3_step(sel);
        if rc != SQLITE_ROW {
            let r = sqlite3_reset(sel);
            rc = if r == SQLITE_OK && rc != SQLITE_DONE {
                rc
            } else {
                r
            };
        }
    }
    rc
}

/// sessionConflictHandler.
unsafe fn conflict_handler(
    e_type: i32,
    p: &mut ApplyCtx,
    it: *mut ChangesetIter,
    x_conflict: XConflict,
    ctx: *mut ffi::c_void,
    pb_replace: Option<&mut bool>,
) -> i32 {
    let mut res = SQLITE_CHANGESET_OMIT;
    let op = (*it).op;
    let mut rc = if pb_replace.is_some() {
        seek_to_row(&*it, p)
    } else {
        SQLITE_OK
    };
    if rc == SQLITE_ROW {
        let sel = p.sel();
        if !p.ignore_noop || sqlite3_column_int64(sel, sqlite3_column_count(sel) - 1) == 0 {
            (*it).conflict = sel;
            res = x_conflict(ctx, e_type, it);
            (*it).conflict = ptr::null_mut();
        }
        rc = sqlite3_reset(sel);
    } else if rc == SQLITE_OK {
        if p.defer_constraints && e_type == SQLITE_CHANGESET_CONFLICT {
            // Retry this change once the rest of the table is applied.
            let input = &(*it).input;
            let blob = &input.data()[input.i_current..input.i_next];
            p.constraints.extend_from_slice(blob);
            return SQLITE_OK;
        } else if !p.ignore_noop || op != SQLITE_DELETE || e_type == SQLITE_CHANGESET_CONFLICT {
            res = x_conflict(ctx, e_type + 1, it);
            if res == SQLITE_CHANGESET_REPLACE {
                rc = SQLITE_MISUSE;
            }
        }
    }
    if rc == SQLITE_OK {
        match res {
            SQLITE_CHANGESET_REPLACE => {
                if let Some(b) = pb_replace {
                    *b = true;
                }
            }
            SQLITE_CHANGESET_OMIT => {}
            SQLITE_CHANGESET_ABORT => rc = SQLITE_ABORT,
            _ => rc = SQLITE_MISUSE,
        }
        if rc == SQLITE_OK {
            rc = p.rebase_add(res, &*it);
        }
    }
    rc
}

/// sessionApplyOneOp.
unsafe fn apply_one_op(
    it: *mut ChangesetIter,
    p: &mut ApplyCtx,
    x_conflict: XConflict,
    ctx: *mut ffi::c_void,
    pb_replace: Option<&mut bool>,
    pb_retry: Option<&mut bool>,
) -> i32 {
    let op = (*it).op;
    let n_col = (*it).n_col;
    let mut rc;
    if op == SQLITE_DELETE {
        let pk = if (*it).patchset {
            Some(p.pk.clone())
        } else {
            None
        };
        let del = p.del();
        rc = bind_row(&*it, false, n_col, pk.as_deref(), del);
        if rc == SQLITE_OK && sqlite3_bind_parameter_count(del) as usize > n_col {
            rc = sqlite3_bind_int64(
                del,
                n_col as i32 + 1,
                (pb_retry.is_none() || pk.is_some()) as i64,
            );
        }
        if rc != SQLITE_OK {
            return rc;
        }
        rc = step_reset(del);
        if rc == SQLITE_OK && sqlite3_changes(p.db) == 0 {
            rc = conflict_handler(SQLITE_CHANGESET_DATA, p, it, x_conflict, ctx, pb_retry);
        } else if rc & 0xff == SQLITE_CONSTRAINT {
            rc = conflict_handler(SQLITE_CHANGESET_CONFLICT, p, it, x_conflict, ctx, None);
        }
    } else if op == SQLITE_UPDATE {
        let patchset = pb_retry.is_none() || (*it).patchset;
        let up = match p.update_find(&*it, patchset) {
            Ok(s) => s,
            Err(e) => return e,
        };
        rc = SQLITE_OK;
        for i in 0..n_col {
            if rc != SQLITE_OK {
                break;
            }
            if let Some(old) = (*it).old(i) {
                if p.pk[i] != 0 || !patchset {
                    rc = old.bind(up, (i * 2 + 2) as i32);
                }
            }
            if rc == SQLITE_OK {
                if let Some(new) = (*it).new_val(i) {
                    rc = new.bind(up, (i * 2 + 1) as i32);
                }
            }
        }
        if rc != SQLITE_OK {
            return rc;
        }
        rc = step_reset(up);
        if rc == SQLITE_OK && sqlite3_changes(p.db) == 0 {
            rc = conflict_handler(SQLITE_CHANGESET_DATA, p, it, x_conflict, ctx, pb_retry);
        } else if rc & 0xff == SQLITE_CONSTRAINT {
            rc = conflict_handler(SQLITE_CHANGESET_CONFLICT, p, it, x_conflict, ctx, None);
        }
    } else {
        rc = SQLITE_OK;
        if p.stat1 {
            // sqlite_stat1 has no PRIMARY KEY to detect duplicates.
            rc = seek_to_row(&*it, p);
            if rc == SQLITE_ROW {
                rc = SQLITE_CONSTRAINT;
                sqlite3_reset(p.sel());
            }
        }
        if rc == SQLITE_OK {
            let ins = p.ins();
            rc = bind_row(&*it, true, n_col, None, ins);
            if rc != SQLITE_OK {
                return rc;
            }
            rc = step_reset(ins);
        }
        if rc & 0xff == SQLITE_CONSTRAINT {
            rc = conflict_handler(
                SQLITE_CHANGESET_CONFLICT,
                p,
                it,
                x_conflict,
                ctx,
                pb_replace,
            );
        }
    }
    rc
}

/// sessionApplyOneWithRetry.
unsafe fn apply_one_with_retry(
    db: *mut sqlite3,
    it: *mut ChangesetIter,
    p: &mut ApplyCtx,
    x_conflict: XConflict,
    ctx: *mut ffi::c_void,
) -> i32 {
    let mut replace = false;
    let mut retry = false;
    let mut rc = apply_one_op(it, p, x_conflict, ctx, Some(&mut replace), Some(&mut retry));
    if rc == SQLITE_OK {
        if retry {
            rc = apply_one_op(it, p, x_conflict, ctx, None, None);
        } else if replace {
            rc = exec(db, "SAVEPOINT replace_op");
            if rc == SQLITE_OK {
                let del = p.del();
                rc = bind_row(&*it, true, p.n_col, Some(&p.pk), del);
                sqlite3_bind_int64(del, p.n_col as i32 + 1, 1);
            }
            if rc == SQLITE_OK {
                rc = step_reset(p.del());
            }
            if rc == SQLITE_OK {
                rc = apply_one_op(it, p, x_conflict, ctx, None, None);
            }
            if rc == SQLITE_OK {
                rc = exec(db, "RELEASE replace_op");
            }
        }
    }
    rc
}

/// sessionRetryIterInit: an iterator over buffered changes of one table.
unsafe fn retry_iter(buf: &[u8], patchset: bool, tab: &str, p: &ApplyCtx) -> Box<ChangesetIter> {
    let mut it = Box::new(ChangesetIter::new(
        SessionInput::from_buffer(buf.as_ptr(), buf.len()),
        p.invert_constraints,
        true,
    ));
    it.patchset = patchset;
    it.tab = Some(CString::new(tab).unwrap_or_default());
    it.n_col = p.n_col;
    it.pk = p.pk.clone();
    it.vals = vec![None; p.n_col * 2];
    it.set_ext();
    it
}

/// sessionApplyRetryBuffer.
unsafe fn apply_retry_buffer(
    retry: &[u8],
    i_skip: isize,
    db: *mut sqlite3,
    patchset: bool,
    tab: &str,
    p: &mut ApplyCtx,
    x_conflict: XConflict,
    ctx: *mut ffi::c_void,
) -> i32 {
    let mut it = retry_iter(retry, patchset, tab, p);
    let mut rc = SQLITE_OK;
    let mut ii: isize = 0;
    while rc == SQLITE_OK && it.next(false, &mut NextOut::default()) == SQLITE_ROW {
        if ii != i_skip {
            rc = apply_one_with_retry(db, &mut *it, p, x_conflict, ctx);
        }
        ii += 1;
    }
    if rc == SQLITE_OK {
        rc = it.rc;
    }
    rc
}

/// sessionUniqueColumns.
unsafe fn unique_columns(tab: &str, p: &mut ApplyCtx) -> Result<Vec<u8>, i32> {
    let mut unique = vec![0u8; p.n_col];
    let list = p.prepare(&format!("PRAGMA main.index_list({})", qq(tab)))?;
    while list.step() == SQLITE_ROW {
        let idx = list.col_text(1).unwrap_or_default();
        let is_unique = list.col_int(2) != 0;
        let partial = list.col_int(4) != 0;
        if !is_unique {
            continue;
        }
        let info = p.prepare(&format!("PRAGMA main.index_xinfo({})", qq(&idx)))?;
        while info.step() == SQLITE_ROW {
            let cid = info.col_int(1);
            let col = info.col_text(2);
            if info.col_int(5) == 0 {
                continue;
            }
            for (u, name) in unique.iter_mut().zip(&p.cols) {
                if partial
                    || cid == -2
                    || col.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(name))
                {
                    *u = 1;
                }
            }
        }
        let rc = info.finalize();
        if rc != SQLITE_OK {
            return Err(rc);
        }
    }
    let rc = list.finalize();
    if rc != SQLITE_OK {
        return Err(rc);
    }
    Ok(unique)
}

/// sessionUpdateToUpdate.
unsafe fn update_to_update(tab: &str, p: &mut ApplyCtx, up: &ChangesetIter) -> Result<Stmt, i32> {
    let unique = unique_columns(tab, p)?;
    let mut randset = String::new();
    let mut upset = String::new();
    let mut whr = String::new();
    for (ii, is_unique) in unique.iter().enumerate() {
        if p.pk[ii] != 0 {
            if !whr.is_empty() {
                whr.push_str(" AND ");
            }
            whr.push_str(&format!("{} IS ?{}", ident(&p.cols[ii]), ii + 1));
        } else if let Some(new) = up.new_val(ii) {
            if *is_unique != 0 {
                let rand = match new {
                    Val::Null => "NULL",
                    Val::Text(_) => "CAST(random() AS TEXT)",
                    Val::Blob(_) => "unhex(hex(random()))",
                    _ => "random()",
                };
                if !randset.is_empty() {
                    randset.push_str(", ");
                }
                randset.push_str(&format!("{} = {}", ident(&p.cols[ii]), rand));
            }
            if !upset.is_empty() {
                upset.push_str(", ");
            }
            upset.push_str(&format!("{} = ?{}", ident(&p.cols[ii]), ii + 1));
        }
    }
    if randset.is_empty() {
        // Random values cannot resolve a conflict on non-UNIQUE columns.
        return Err(SQLITE_CONSTRAINT);
    }
    let rand = p.prepare(&format!(
        "UPDATE main.\"{}\" SET {} WHERE {}",
        w(tab),
        randset,
        whr
    ))?;
    let ret = p.prepare(&format!(
        "UPDATE main.\"{}\" SET {} WHERE {}",
        w(tab),
        upset,
        whr
    ))?;
    let mut rc = bind_row(up, false, p.n_col, Some(&p.pk), rand.0);
    if rc == SQLITE_OK {
        rc = bind_row(up, false, p.n_col, Some(&p.pk), ret.0);
    }
    for ii in 0..p.n_col {
        if rc != SQLITE_OK {
            break;
        }
        if p.pk[ii] == 0 {
            if let Some(new) = up.new_val(ii) {
                rc = new.bind(ret.0, ii as i32 + 1);
            }
        }
    }
    if rc == SQLITE_OK {
        rc = step_reset(rand.0);
    }
    drop(rand);
    if rc != SQLITE_OK {
        return Err(rc);
    }
    Ok(ret)
}

/// sessionRetryConstraints.
unsafe fn retry_constraints(
    db: *mut sqlite3,
    patchset: bool,
    tab: &str,
    p: &mut ApplyCtx,
    x_conflict: XConflict,
    ctx: *mut ffi::c_void,
) -> i32 {
    let mut rc = SQLITE_OK;
    let mut i_update: isize = 0;

    // Step (1): retry until no progress is made.
    while !p.constraints.is_empty() {
        let cons = std::mem::take(&mut p.constraints);
        rc = apply_retry_buffer(&cons, -1, db, patchset, tab, p, x_conflict, ctx);
        if rc != SQLITE_OK || p.constraints.len() >= cons.len() {
            break;
        }
    }

    // Step (2): break UPDATE loops by moving UNIQUE columns aside.
    while rc == SQLITE_OK && !p.constraints.is_empty() && !p.no_update_loop {
        let mut update: Option<Stmt> = None;
        let mut i_skip: isize = 0;
        let mut constraint = false;
        let snapshot = p.constraints.clone();
        {
            let mut up = retry_iter(&snapshot, patchset, tab, p);
            let mut i_this: isize = -1;
            while up.next(false, &mut NextOut::default()) == SQLITE_ROW {
                if up.op == SQLITE_UPDATE {
                    i_this += 1;
                }
                if i_this == i_update {
                    break;
                }
                i_skip += 1;
            }
            if i_this != i_update {
                break;
            }
            rc = exec(db, "SAVEPOINT update_op");
            if rc == SQLITE_OK {
                match update_to_update(tab, p, &up) {
                    Ok(s) => update = Some(s),
                    Err(e) if e & 0xff == SQLITE_CONSTRAINT => constraint = true,
                    Err(e) => rc = e,
                }
            }
        }

        let mut cons = Vec::new();
        if rc == SQLITE_OK {
            cons = p.constraints.clone();
            if constraint {
                p.constraints.clear();
            }
            while rc == SQLITE_OK && !p.constraints.is_empty() {
                let app = std::mem::take(&mut p.constraints);
                rc = apply_retry_buffer(&app, i_skip, db, patchset, tab, p, x_conflict, ctx);
                if p.constraints.len() >= app.len() {
                    break;
                }
                i_skip = -1;
            }
        }

        i_update += 1;
        if rc == SQLITE_OK {
            if !constraint {
                if let Some(u) = update.take() {
                    let r = step_reset(u.0);
                    drop(u);
                    if r & 0xff == SQLITE_CONSTRAINT {
                        constraint = true;
                    } else {
                        rc = r;
                    }
                }
            }
            if constraint {
                rc = exec(db, "ROLLBACK TO update_op");
                p.constraints = std::mem::take(&mut cons);
            } else if rc == SQLITE_OK {
                i_update = 0;
            }
            if rc == SQLITE_OK {
                rc = exec(db, "RELEASE update_op");
            }
        }
    }

    // Step (3): a last attempt that reports conflicts to the handler.
    if rc == SQLITE_OK && !p.constraints.is_empty() {
        let cons = std::mem::take(&mut p.constraints);
        p.defer_constraints = false;
        rc = apply_retry_buffer(&cons, -1, db, patchset, tab, p, x_conflict, ctx);
    }
    rc
}

/// Outstanding foreign key violations (SQLITE_DBSTATUS_DEFERRED_FKS).
unsafe fn deferred_fk_count(db: *mut sqlite3) -> i32 {
    let Ok(stmt) = Stmt::prepare(db, "PRAGMA foreign_keys") else {
        return 0;
    };
    if stmt.step() != SQLITE_ROW || stmt.col_int(0) == 0 {
        return 0;
    }
    drop(stmt);
    let Ok(stmt) = Stmt::prepare(db, "PRAGMA main.foreign_key_check") else {
        return 0;
    };
    let mut n = 0;
    while stmt.step() == SQLITE_ROW {
        n += 1;
    }
    n
}

/// sessionChangesetApply.
unsafe fn changeset_apply(
    db: *mut sqlite3,
    mut it: Box<ChangesetIter>,
    x_filter: Option<XFilter>,
    x_filter_iter: Option<XFilterIter>,
    x_conflict: XConflict,
    ctx: *mut ffi::c_void,
    pp_rebase: *mut *mut ffi::c_void,
    pn_rebase: *mut ffi::c_int,
    flags: i32,
) -> i32 {
    let mut schema_mismatch = false;
    let mut rc = SQLITE_OK;
    let mut tab = String::new();
    let mut p = ApplyCtx {
        db,
        delete: None,
        insert: None,
        select: None,
        n_col: 0,
        cols: Vec::new(),
        pk: Vec::new(),
        up: Vec::new(),
        stat1: false,
        defer_constraints: false,
        invert_constraints: flags & SQLITE_CHANGESETAPPLY_INVERT != 0,
        constraints: Vec::new(),
        rebase: Vec::new(),
        rebase_started: false,
        want_rebase: !pp_rebase.is_null() && !pn_rebase.is_null(),
        ignore_noop: flags & SQLITE_CHANGESETAPPLY_IGNORENOOP != 0,
        no_update_loop: flags & SQLITE_CHANGESETAPPLY_NOUPDATELOOP != 0,
        err: None,
    };
    it.input.no_discard = true;
    if flags & SQLITE_CHANGESETAPPLY_NOSAVEPOINT == 0 {
        rc = exec(db, "SAVEPOINT changeset_apply");
    }
    if rc == SQLITE_OK {
        rc = exec(db, "PRAGMA defer_foreign_keys = 1");
    }
    let it_ptr: *mut ChangesetIter = &mut *it;
    while rc == SQLITE_OK {
        let mut out = NextOut::default();
        if (*it_ptr).next(false, &mut out) != SQLITE_ROW {
            break;
        }
        let n_col = (*it_ptr).n_col;
        let z_new = (*it_ptr).tab_name();
        if out.new_table {
            rc = retry_constraints(db, (*it_ptr).patchset, &tab, &mut p, x_conflict, ctx);
            if rc != SQLITE_OK {
                break;
            }
            p.up.clear();
            p.delete = None;
            p.insert = None;
            p.select = None;
            p.n_col = 0;
            p.cols.clear();
            p.pk.clear();
            p.stat1 = false;
            p.defer_constraints = true;
            p.rebase_started = false;
            p.constraints.clear();

            schema_mismatch = match x_filter {
                Some(f) => {
                    let c = CString::new(z_new.clone()).unwrap_or_default();
                    f(ctx, c.as_ptr()) == 0
                }
                None => false,
            };
            tab = z_new.clone();
            if !schema_mismatch {
                let info = match table_info(db, "main", &z_new, true) {
                    Ok(i) => i,
                    Err(e) => {
                        rc = e;
                        break;
                    }
                };
                p.n_col = info.n_col;
                p.cols = info.cols;
                p.pk = info.pk;
                let n_min_col = p.pk.iter().rposition(|x| *x != 0).map_or(0, |i| i + 1);
                let it_pk = &(*it_ptr).pk;
                // SQLite logs each mismatch with sqlite3_log and skips the table.
                if p.n_col == 0
                    || p.n_col < n_col
                    || n_col < n_min_col
                    || p.pk[..n_col] != it_pk[..n_col]
                {
                    schema_mismatch = true;
                } else {
                    p.n_col = n_col;
                    if tab.eq_ignore_ascii_case("sqlite_stat1") {
                        rc = p.stat1_sql();
                        if rc != SQLITE_OK {
                            break;
                        }
                        p.stat1 = true;
                    } else {
                        rc = p.select_row(&tab);
                        if rc == SQLITE_OK {
                            rc = p.delete_row(&tab);
                        }
                        if rc == SQLITE_OK {
                            rc = p.insert_row(&tab);
                        }
                        if rc != SQLITE_OK {
                            break;
                        }
                    }
                }
            }
        }
        if schema_mismatch {
            continue;
        }
        if let Some(f) = x_filter_iter {
            if f(ctx, it_ptr) == 0 {
                continue;
            }
        }
        rc = apply_one_with_retry(db, it_ptr, &mut p, x_conflict, ctx);
    }

    let patchset = it.patchset;
    if rc == SQLITE_OK {
        rc = it.rc;
    }
    drop(it);
    if rc == SQLITE_OK {
        rc = retry_constraints(db, patchset, &tab, &mut p, x_conflict, ctx);
    }
    if rc == SQLITE_OK {
        let n_fk = deferred_fk_count(db);
        if n_fk != 0 {
            let mut fk =
                ChangesetIter::new(SessionInput::from_buffer(ptr::null(), 0), false, false);
            fk.fk_mode = true;
            fk.n_col = n_fk as usize;
            let res = x_conflict(ctx, SQLITE_CHANGESET_FOREIGN_KEY, &mut fk);
            if res != SQLITE_CHANGESET_OMIT {
                rc = SQLITE_CONSTRAINT;
            }
        }
    }
    let rc2 = exec(db, "PRAGMA defer_foreign_keys = 0");
    if rc == SQLITE_OK {
        rc = rc2;
    }
    if flags & SQLITE_CHANGESETAPPLY_NOSAVEPOINT == 0 {
        if rc == SQLITE_OK {
            rc = exec(db, "RELEASE changeset_apply");
        }
        if rc != SQLITE_OK {
            exec(db, "ROLLBACK TO changeset_apply");
            exec(db, "RELEASE changeset_apply");
        }
    }
    if rc == SQLITE_OK && !patchset && p.want_rebase {
        *pn_rebase = p.rebase.len() as i32;
        *pp_rebase = to_sqlite_buf(&p.rebase);
    }
    let err = p.err.take();
    drop(p);
    set_errmsg(db, rc, err.as_deref());
    rc
}

/// sqlite3_set_errmsg.
unsafe fn set_errmsg(db: *mut sqlite3, rc: i32, msg: Option<&str>) {
    let mut inner = (*db).inner.lock().unwrap_or_else(|e| e.into_inner());
    match msg {
        Some(m) if rc != SQLITE_OK => {
            crate::set_db_err_msg(&mut inner, rc, m);
        }
        _ => {
            crate::set_db_err_code(&mut inner, rc);
        }
    }
}

unsafe fn apply_v23(
    db: *mut sqlite3,
    input: SessionInput,
    x_filter: Option<XFilter>,
    x_filter_iter: Option<XFilterIter>,
    x_conflict: Option<XConflict>,
    ctx: *mut ffi::c_void,
    pp_rebase: *mut *mut ffi::c_void,
    pn_rebase: *mut ffi::c_int,
    flags: i32,
) -> ffi::c_int {
    let (Some(x_conflict), false) = (x_conflict, db.is_null()) else {
        return SQLITE_MISUSE;
    };
    let invert = flags & SQLITE_CHANGESETAPPLY_INVERT != 0;
    let it = Box::new(ChangesetIter::new(input, invert, true));
    changeset_apply(
        db,
        it,
        x_filter,
        x_filter_iter,
        x_conflict,
        ctx,
        pp_rebase,
        pn_rebase,
        flags,
    )
}

fn buf_input(n: ffi::c_int, p: *mut ffi::c_void) -> SessionInput {
    SessionInput::from_buffer(p as *const u8, n.max(0) as usize)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_apply(
    db: *mut sqlite3,
    n_changeset: ffi::c_int,
    p_changeset: *mut ffi::c_void,
    x_filter: Option<XFilter>,
    x_conflict: Option<XConflict>,
    p_ctx: *mut ffi::c_void,
) -> ffi::c_int {
    apply_v23(
        db,
        buf_input(n_changeset, p_changeset),
        x_filter,
        None,
        x_conflict,
        p_ctx,
        ptr::null_mut(),
        ptr::null_mut(),
        0,
    )
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_apply_v2(
    db: *mut sqlite3,
    n_changeset: ffi::c_int,
    p_changeset: *mut ffi::c_void,
    x_filter: Option<XFilter>,
    x_conflict: Option<XConflict>,
    p_ctx: *mut ffi::c_void,
    pp_rebase: *mut *mut ffi::c_void,
    pn_rebase: *mut ffi::c_int,
    flags: ffi::c_int,
) -> ffi::c_int {
    apply_v23(
        db,
        buf_input(n_changeset, p_changeset),
        x_filter,
        None,
        x_conflict,
        p_ctx,
        pp_rebase,
        pn_rebase,
        flags,
    )
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_apply_v3(
    db: *mut sqlite3,
    n_changeset: ffi::c_int,
    p_changeset: *mut ffi::c_void,
    x_filter: Option<XFilterIter>,
    x_conflict: Option<XConflict>,
    p_ctx: *mut ffi::c_void,
    pp_rebase: *mut *mut ffi::c_void,
    pn_rebase: *mut ffi::c_int,
    flags: ffi::c_int,
) -> ffi::c_int {
    apply_v23(
        db,
        buf_input(n_changeset, p_changeset),
        None,
        x_filter,
        x_conflict,
        p_ctx,
        pp_rebase,
        pn_rebase,
        flags,
    )
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_apply_strm(
    db: *mut sqlite3,
    x_input: Option<XInput>,
    p_in: *mut ffi::c_void,
    x_filter: Option<XFilter>,
    x_conflict: Option<XConflict>,
    p_ctx: *mut ffi::c_void,
) -> ffi::c_int {
    let Some(x) = x_input else {
        return SQLITE_MISUSE;
    };
    apply_v23(
        db,
        SessionInput::from_stream(x, p_in),
        x_filter,
        None,
        x_conflict,
        p_ctx,
        ptr::null_mut(),
        ptr::null_mut(),
        0,
    )
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_apply_v2_strm(
    db: *mut sqlite3,
    x_input: Option<XInput>,
    p_in: *mut ffi::c_void,
    x_filter: Option<XFilter>,
    x_conflict: Option<XConflict>,
    p_ctx: *mut ffi::c_void,
    pp_rebase: *mut *mut ffi::c_void,
    pn_rebase: *mut ffi::c_int,
    flags: ffi::c_int,
) -> ffi::c_int {
    let Some(x) = x_input else {
        return SQLITE_MISUSE;
    };
    apply_v23(
        db,
        SessionInput::from_stream(x, p_in),
        x_filter,
        None,
        x_conflict,
        p_ctx,
        pp_rebase,
        pn_rebase,
        flags,
    )
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3changeset_apply_v3_strm(
    db: *mut sqlite3,
    x_input: Option<XInput>,
    p_in: *mut ffi::c_void,
    x_filter: Option<XFilterIter>,
    x_conflict: Option<XConflict>,
    p_ctx: *mut ffi::c_void,
    pp_rebase: *mut *mut ffi::c_void,
    pn_rebase: *mut ffi::c_int,
    flags: ffi::c_int,
) -> ffi::c_int {
    let Some(x) = x_input else {
        return SQLITE_MISUSE;
    };
    apply_v23(
        db,
        SessionInput::from_stream(x, p_in),
        None,
        x_filter,
        x_conflict,
        p_ctx,
        pp_rebase,
        pn_rebase,
        flags,
    )
}
