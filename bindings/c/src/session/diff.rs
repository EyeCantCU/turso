//! sqlite3session_diff: changes that turn a table of another database into
//! the session's table.

use super::*;

fn expr_compare_pk(
    n_col: usize,
    db1: &str,
    db2: &str,
    tab: &str,
    cols: &[String],
    pk: &[u8],
) -> String {
    let mut out = String::new();
    let mut sep = "";
    for i in 0..n_col {
        if pk[i] != 0 {
            out.push_str(&format!(
                "{}\"{}\".\"{}\".\"{}\"=\"{}\".\"{}\".\"{}\"",
                sep,
                w(db1),
                w(tab),
                w(&cols[i]),
                w(db2),
                w(tab),
                w(&cols[i])
            ));
            sep = " AND ";
        }
    }
    out
}

fn expr_compare_other(
    n_col: usize,
    db1: &str,
    db2: &str,
    tab: &str,
    cols: &[String],
    pk: &[u8],
) -> String {
    let mut out = String::new();
    let mut sep = "";
    let mut have = false;
    for i in 0..n_col {
        if pk[i] == 0 {
            have = true;
            out.push_str(&format!(
                "{}\"{}\".\"{}\".\"{}\" IS NOT \"{}\".\"{}\".\"{}\"",
                sep,
                w(db1),
                w(tab),
                w(&cols[i]),
                w(db2),
                w(tab),
                w(&cols[i])
            ));
            sep = " OR ";
        }
    }
    if !have {
        out = "0".to_string();
    }
    out
}

fn all_cols(zdb: &str, t: &SessionTable) -> String {
    t.cols[..t.n_col]
        .iter()
        .map(|c| format!("\"{}\".\"{}\".\"{}\"", w(zdb), w(&t.name), w(c)))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Sqlite3Session {
    /// sessionDiffFindNew.
    unsafe fn diff_find_new(&mut self, op: u8, ti: usize, db1: &str, db2: &str, expr: &str) -> i32 {
        let t = &self.tables[ti];
        let sel = if t.rowid {
            format!("{SESSIONS_ROWID}, *")
        } else {
            "*".to_string()
        };
        let sql = format!(
            "SELECT {} FROM \"{}\".\"{}\" WHERE NOT EXISTS (  SELECT 1 FROM \"{}\".\"{}\" WHERE {})",
            sel,
            w(db1),
            w(&t.name),
            w(db2),
            w(&t.name),
            expr
        );
        let rowid = t.rowid;
        let stmt = match Stmt::prepare(self.db, &sql) {
            Ok(s) => s,
            Err(e) => return e,
        };
        self.hook = Hook::Diff {
            stmt: stmt.0,
            rowid,
            old_off: 0,
        };
        while stmt.step() == SQLITE_ROW {
            let r = if rowid { stmt.col_int(0) } else { 0 };
            self.preupdate_one_change(op, r, ti);
        }
        self.hook = Hook::Preupdate;
        stmt.finalize()
    }

    /// sessionDiffFindModified.
    unsafe fn diff_find_modified(&mut self, ti: usize, from: &str, expr: &str) -> i32 {
        let t = &self.tables[ti];
        let expr2 = expr_compare_other(t.n_col, &self.zdb, from, &t.name, &t.cols, &t.pk);
        let z1 = all_cols(&self.zdb, t);
        let z2 = all_cols(from, t);
        let sql = format!(
            "SELECT {},{} FROM \"{}\".\"{}\", \"{}\".\"{}\" WHERE {} AND ({})",
            z1,
            z2,
            w(&self.zdb),
            w(&t.name),
            w(from),
            w(&t.name),
            expr,
            expr2
        );
        let (rowid, n_col) = (t.rowid, t.n_col);
        let stmt = match Stmt::prepare(self.db, &sql) {
            Ok(s) => s,
            Err(e) => return e,
        };
        // SQLite keeps bRowid from the previous query here.
        self.hook = Hook::Diff {
            stmt: stmt.0,
            rowid,
            old_off: n_col,
        };
        while stmt.step() == SQLITE_ROW {
            let r = if self.tables[ti].rowid {
                stmt.col_int(0)
            } else {
                0
            };
            self.preupdate_one_change(SQLITE_UPDATE, r, ti);
        }
        self.hook = Hook::Preupdate;
        stmt.finalize()
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3session_diff(
    p_session: *mut Sqlite3Session,
    z_from_db: *const ffi::c_char,
    z_tbl: *const ffi::c_char,
    pz_err_msg: *mut *mut ffi::c_char,
) -> ffi::c_int {
    if p_session.is_null() || z_from_db.is_null() || z_tbl.is_null() {
        return SQLITE_MISUSE;
    }
    let s = &mut *p_session;
    let from = cstr(z_from_db);
    let tbl = cstr(z_tbl);
    if !pz_err_msg.is_null() {
        *pz_err_msg = ptr::null_mut();
    }
    let set_err = |m: &str| {
        if !pz_err_msg.is_null() {
            *pz_err_msg = mprintf(m);
        }
    };
    let mut rc = s.rc;
    if rc != SQLITE_OK {
        return rc;
    }
    s.auto_attach += 1;
    let found = s.find_table(&tbl);
    s.auto_attach -= 1;
    let ti = match found {
        Ok(Some(ti)) => ti,
        Ok(None) => return SQLITE_OK,
        Err(e) => return e,
    };
    if s.init_table(ti) {
        return s.rc;
    }
    let zdb = s.zdb.clone();
    // The schemas must match.
    let mut n_col: isize = 0;
    let exists = format!("SELECT * FROM {}.sqlite_schema", qq(&from));
    match Stmt::prepare(s.db, &exists) {
        Ok(_) => {}
        Err(SQLITE_ERROR) => n_col = -1,
        Err(e) => rc = e,
    }
    let mut info = None;
    if rc == SQLITE_OK && n_col == 0 {
        match table_info(s.db, &from, &tbl, s.implicit_pk) {
            Ok(i) => {
                n_col = i.n_col as isize;
                info = Some(i);
            }
            Err(e) => rc = e,
        }
    }
    let mut has_pk = false;
    if rc == SQLITE_OK {
        let t = &s.tables[ti];
        if t.n_col as isize != n_col {
            if n_col <= 0 {
                rc = SQLITE_SCHEMA;
                set_err(&format!("no such table: {from}.{tbl}"));
            } else {
                set_err("table schemas do not match");
                rc = SQLITE_SCHEMA;
            }
        } else if let Some(info) = &info {
            let mut mismatch = false;
            for i in 0..info.n_col {
                if t.pk[i] != info.pk[i] {
                    mismatch = true;
                }
                if !info.cols[i].eq_ignore_ascii_case(&t.cols[i]) {
                    mismatch = true;
                }
                if info.pk[i] != 0 {
                    has_pk = true;
                }
            }
            if mismatch {
                set_err("table schemas do not match");
                rc = SQLITE_SCHEMA;
            }
        }
    }
    if !has_pk {
        return rc;
    }
    if rc == SQLITE_OK {
        let t = &s.tables[ti];
        let expr = expr_compare_pk(t.n_col, &zdb, &from, &t.name, &t.cols, &t.pk);
        rc = s.diff_find_new(SQLITE_INSERT, ti, &zdb, &from, &expr);
        if rc == SQLITE_OK {
            rc = s.diff_find_new(SQLITE_DELETE, ti, &from, &zdb, &expr);
        }
        if rc == SQLITE_OK {
            rc = s.diff_find_modified(ti, &from, &expr);
        }
    }
    rc
}
