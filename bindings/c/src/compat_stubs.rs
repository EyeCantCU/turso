//! Chainguard compat: real implementations for entry points that were
//! `todo!()` stubs (aggregates, window functions, backup, serialize,
//! deserialize) plus the UTF-16 API family used by Qt.
//!
//! Design notes
//! * Aggregates are bridged onto turso_ext's external aggregate support
//!   (init/step/finalize). `sqlite3_aggregate_context` finds the per-group
//!   state through a thread-local stack that is pushed around each xStep /
//!   xFinal invocation.
//! * Window functions are aggregates that also carry xValue/xInverse; the
//!   core windows them through `Connection::_register_window_function`.
//! * Turso has no page-level copy API, so backup / serialize / deserialize
//!   use a logical copy (schema + rows) built on the public C API above.
//! * UTF-16 functions transcode around the UTF-8 ones and keep the returned
//!   buffers in a global cache keyed by (handle, column, kind).
use super::*;
use std::cell::RefCell;
use std::collections::HashMap;
use turso_ext::{AggCtx, ValueType as ExtValueType};

// ---------------------------------------------------------------------------
// Aggregates
// ---------------------------------------------------------------------------

type XStep = unsafe extern "C" fn(*mut ffi::c_void, ffi::c_int, *mut *mut ffi::c_void);
type XFinal = unsafe extern "C" fn(*mut ffi::c_void);
type XDestroy = unsafe extern "C" fn(*mut ffi::c_void);

struct AggReg {
    x_step: Option<XStep>,
    x_final: Option<XFinal>,
    x_value: Option<XFinal>,
    x_inverse: Option<XStep>,
    p_app: usize,
    destroy: Option<XDestroy>,
    db: usize,
}

struct AggState {
    reg: *const AggReg,
    buf: *mut ffi::c_void,
    finalized: bool,
}

thread_local! {
    /// (SqliteContext address, AggState address) for in-flight callbacks.
    static AGG_STACK: RefCell<Vec<(usize, usize)>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "C" fn agg_init(context: usize) -> *mut AggCtx {
    let state = Box::into_raw(Box::new(AggState {
        reg: context as *const AggReg,
        buf: std::ptr::null_mut(),
        finalized: false,
    }));
    Box::into_raw(Box::new(AggCtx {
        state: state as *mut ffi::c_void,
    }))
}

unsafe fn agg_invoke(
    reg: &AggReg,
    state: *mut AggState,
    f: impl FnOnce(&mut SqliteContext),
) -> ExtValue {
    let mut sctx = SqliteContext {
        result: ExtValue::null(),
        p_app: reg.p_app as *mut ffi::c_void,
        db: reg.db as *mut sqlite3,
    };
    let key = &mut sctx as *mut SqliteContext as usize;
    AGG_STACK.with(|s| s.borrow_mut().push((key, state as usize)));
    f(&mut sctx);
    AGG_STACK.with(|s| {
        s.borrow_mut().pop();
    });
    sctx.result
}

unsafe extern "C" fn agg_step(
    context: usize,
    ctx: *mut AggCtx,
    argc: i32,
    argv: *const ExtValue,
) -> ExtValue {
    let reg = &*(context as *const AggReg);
    let state = (*ctx).state as *mut AggState;
    let Some(x_step) = reg.x_step else {
        return ExtValue::null();
    };
    let mut arg_ptrs: Vec<*mut ffi::c_void> = (0..argc.max(0) as usize)
        .map(|i| argv.add(i) as *mut ffi::c_void)
        .collect();
    let res = agg_invoke(reg, state, |c| {
        x_step(
            c as *mut SqliteContext as *mut ffi::c_void,
            argc,
            arg_ptrs.as_mut_ptr(),
        )
    });
    if res.value_type() == ExtValueType::Error {
        res
    } else {
        res.__free_internal_type();
        ExtValue::null()
    }
}

unsafe extern "C" fn agg_inverse(
    context: usize,
    ctx: *mut AggCtx,
    argc: i32,
    argv: *const ExtValue,
) -> ExtValue {
    let reg = &*(context as *const AggReg);
    let state = (*ctx).state as *mut AggState;
    let Some(x_inverse) = reg.x_inverse else {
        return ExtValue::null();
    };
    let mut arg_ptrs: Vec<*mut ffi::c_void> = (0..argc.max(0) as usize)
        .map(|i| argv.add(i) as *mut ffi::c_void)
        .collect();
    let res = agg_invoke(reg, state, |c| {
        x_inverse(
            c as *mut SqliteContext as *mut ffi::c_void,
            argc,
            arg_ptrs.as_mut_ptr(),
        )
    });
    if res.value_type() == ExtValueType::Error {
        res
    } else {
        res.__free_internal_type();
        ExtValue::null()
    }
}

unsafe extern "C" fn agg_value(context: usize, ctx: *mut AggCtx) -> ExtValue {
    let reg = &*(context as *const AggReg);
    let state = (*ctx).state as *mut AggState;
    let Some(x_value) = reg.x_value else {
        return ExtValue::null();
    };
    agg_invoke(reg, state, |c| {
        x_value(c as *mut SqliteContext as *mut ffi::c_void)
    })
}

unsafe extern "C" fn agg_final(context: usize, ctx: *mut AggCtx) -> ExtValue {
    let reg = &*(context as *const AggReg);
    let state = (*ctx).state as *mut AggState;
    (*state).finalized = true;
    let Some(x_final) = reg.x_final else {
        return ExtValue::null();
    };
    agg_invoke(reg, state, |c| {
        x_final(c as *mut SqliteContext as *mut ffi::c_void)
    })
}

/// Frees the per-group state. If xFinal never ran (xStep failed, or the
/// statement was abandoned) run it once so the application can release
/// whatever it keeps in the aggregate context, as SQLite guarantees.
unsafe extern "C" fn agg_destroy_state(ctx_addr: usize) {
    let ctx = Box::from_raw(ctx_addr as *mut AggCtx);
    let state = Box::from_raw(ctx.state as *mut AggState);
    let reg = &*state.reg;
    if !state.finalized {
        if let Some(x_final) = reg.x_final {
            let sp = &*state as *const AggState as *mut AggState;
            let res = agg_invoke(reg, sp, |c| {
                x_final(c as *mut SqliteContext as *mut ffi::c_void)
            });
            res.__free_internal_type();
        }
    }
    if !state.buf.is_null() {
        libc::free(state.buf);
    }
}

unsafe extern "C" fn agg_destroy_reg(context: usize) {
    let reg = Box::from_raw(context as *mut AggReg);
    if let Some(d) = reg.destroy {
        d(reg.p_app as *mut ffi::c_void);
    }
}

/// Register an xStep/xFinal aggregate. Called from sqlite3_create_function_v2.
pub(crate) unsafe fn create_aggregate(
    db: *mut sqlite3,
    name: *const ffi::c_char,
    n_args: ffi::c_int,
    p_app: *mut ffi::c_void,
    x_step: Option<unsafe extern "C" fn()>,
    x_final: Option<unsafe extern "C" fn()>,
    destroy: Option<unsafe extern "C" fn()>,
) -> ffi::c_int {
    register_agg(
        db, name, n_args, p_app, x_step, x_final, None, None, destroy,
    )
}

#[allow(clippy::too_many_arguments)]
unsafe fn register_agg(
    db: *mut sqlite3,
    name: *const ffi::c_char,
    n_args: ffi::c_int,
    p_app: *mut ffi::c_void,
    x_step: Option<unsafe extern "C" fn()>,
    x_final: Option<unsafe extern "C" fn()>,
    x_value: Option<unsafe extern "C" fn()>,
    x_inverse: Option<unsafe extern "C" fn()>,
    destroy: Option<unsafe extern "C" fn()>,
) -> ffi::c_int {
    let destroy: Option<XDestroy> = destroy.map(|f| std::mem::transmute(f));
    let fail = |rc: ffi::c_int, msg: &str| -> ffi::c_int {
        if let Some(d) = destroy {
            d(p_app);
        }
        let mut inner = (*db).inner.lock().unwrap();
        set_db_err_msg(&mut inner, rc, msg)
    };
    // SQLite requires xStep and xFinal together.
    let (Some(x_step), Some(x_final)) = (x_step, x_final) else {
        return fail(SQLITE_MISUSE, "invalid aggregate function definition");
    };
    if !(-1..=127).contains(&n_args) {
        return fail(SQLITE_MISUSE, "invalid function argument count");
    }
    let reg = Box::into_raw(Box::new(AggReg {
        x_step: Some(std::mem::transmute::<unsafe extern "C" fn(), XStep>(x_step)),
        x_final: Some(std::mem::transmute::<unsafe extern "C" fn(), XFinal>(
            x_final,
        )),
        x_value: x_value.map(|f| std::mem::transmute::<unsafe extern "C" fn(), XFinal>(f)),
        x_inverse: x_inverse.map(|f| std::mem::transmute::<unsafe extern "C" fn(), XStep>(f)),
        p_app: p_app as usize,
        destroy,
        db: db as usize,
    }));
    let name_str = CStr::from_ptr(name).to_string_lossy().into_owned();
    let inner = (*db).inner.lock().unwrap();
    let rc = match (x_value, x_inverse) {
        (Some(_), Some(_)) => {
            let value: turso_ext::FinalizeFunction = agg_value;
            let inverse: turso_ext::StepFunction = agg_inverse;
            let window = turso_core::ExternalWindowCallbacks { value, inverse };
            let r = inner.conn._register_window_function(
                &name_str,
                n_args,
                reg as usize,
                (agg_init, agg_step, agg_final),
                window,
                Some(agg_destroy_reg),
                Some(agg_destroy_state),
                None,
            );
            match r {
                Ok(()) => turso_ext::ResultCode::OK,
                Err(_) => turso_ext::ResultCode::Error,
            }
        }
        _ => {
            let api = inner.conn._build_turso_ext();
            let rc = (api.register_aggregate_function)(
                api.ctx,
                name,
                n_args,
                reg as usize,
                agg_init,
                agg_step,
                agg_final,
                Some(agg_destroy_reg),
                Some(agg_destroy_state),
                None,
            );
            inner.conn._free_extension_ctx(api);
            rc
        }
    };
    drop(inner);
    if rc != turso_ext::ResultCode::OK {
        // Registration failed before ownership of `reg` was taken.
        let reg = Box::from_raw(reg);
        if let Some(d) = reg.destroy {
            d(reg.p_app as *mut ffi::c_void);
        }
        let mut inner = (*db).inner.lock().unwrap();
        return set_db_err_msg(
            &mut inner,
            SQLITE_ERROR,
            "unable to register aggregate function",
        );
    }
    SQLITE_OK
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_aggregate_context(
    context: *mut ffi::c_void,
    n: ffi::c_int,
) -> *mut ffi::c_void {
    if context.is_null() {
        return std::ptr::null_mut();
    }
    let key = context as usize;
    let st = AGG_STACK.with(|s| {
        s.borrow()
            .iter()
            .rev()
            .find(|(c, _)| *c == key)
            .map(|(_, st)| *st)
    });
    let Some(st) = st else {
        return std::ptr::null_mut();
    };
    let st = &mut *(st as *mut AggState);
    if st.buf.is_null() && n > 0 {
        st.buf = libc::calloc(1, n as usize);
    }
    st.buf
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_window_function(
    db: *mut sqlite3,
    name: *const ffi::c_char,
    n_args: ffi::c_int,
    _enc: ffi::c_int,
    p_app: *mut ffi::c_void,
    x_step: Option<unsafe extern "C" fn()>,
    x_final: Option<unsafe extern "C" fn()>,
    x_value: Option<unsafe extern "C" fn()>,
    x_inverse: Option<unsafe extern "C" fn()>,
    destroy: Option<unsafe extern "C" fn()>,
) -> ffi::c_int {
    if db.is_null() || name.is_null() {
        return SQLITE_MISUSE;
    }
    if x_step.is_none() && x_final.is_none() && x_value.is_none() && x_inverse.is_none() {
        // Delete; a missing function is not an error.
        let inner = (*db).inner.lock().unwrap();
        let api = inner.conn._build_turso_ext();
        (api.unregister_function)(api.ctx, name);
        inner.conn._free_extension_ctx(api);
        return SQLITE_OK;
    }
    if x_value.is_some() != x_inverse.is_some() {
        let mut inner = (*db).inner.lock().unwrap();
        if let Some(d) = destroy {
            let d: XDestroy = std::mem::transmute(d);
            d(p_app);
        }
        return set_db_err_msg(
            &mut inner,
            SQLITE_MISUSE,
            "invalid window function definition",
        );
    }
    register_agg(
        db, name, n_args, p_app, x_step, x_final, x_value, x_inverse, destroy,
    )
}

// ---------------------------------------------------------------------------
// Small helpers over the C API
// ---------------------------------------------------------------------------

type Res<T> = Result<T, (ffi::c_int, String)>;

unsafe fn errmsg_string(db: *mut sqlite3) -> String {
    CStr::from_ptr(sqlite3_errmsg(db))
        .to_string_lossy()
        .into_owned()
}

struct Stmt {
    db: *mut sqlite3,
    raw: *mut sqlite3_stmt,
}

impl Drop for Stmt {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { sqlite3_finalize(self.raw) };
        }
    }
}

impl Stmt {
    unsafe fn new(db: *mut sqlite3, sql: &str) -> Res<Stmt> {
        let c = CString::new(sql).map_err(|_| (SQLITE_MISUSE, "NUL in SQL".to_string()))?;
        let mut raw = std::ptr::null_mut();
        let rc = sqlite3_prepare_v2(db, c.as_ptr(), -1, &mut raw, std::ptr::null_mut());
        if rc != SQLITE_OK {
            return Err((rc, errmsg_string(db)));
        }
        if raw.is_null() {
            return Err((SQLITE_ERROR, "empty statement".to_string()));
        }
        Ok(Stmt { db, raw })
    }
    /// Step: Ok(true) = row, Ok(false) = done.
    unsafe fn step(&self) -> Res<bool> {
        match sqlite3_step(self.raw) {
            SQLITE_ROW => Ok(true),
            SQLITE_DONE => Ok(false),
            rc => Err((rc, errmsg_string(self.db))),
        }
    }
    unsafe fn text(&self, i: ffi::c_int) -> Option<String> {
        let p = sqlite3_column_text(self.raw, i);
        if p.is_null() {
            return None;
        }
        let n = sqlite3_column_bytes(self.raw, i) as usize;
        Some(String::from_utf8_lossy(std::slice::from_raw_parts(p, n)).into_owned())
    }
}

unsafe fn exec(db: *mut sqlite3, sql: &str) -> Res<()> {
    let st = Stmt::new(db, sql)?;
    while st.step()? {}
    Ok(())
}

fn qident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

// ---------------------------------------------------------------------------
// Logical database copy (shared by backup, serialize and deserialize)
// ---------------------------------------------------------------------------

unsafe fn schema_objects(db: *mut sqlite3, schema: &str) -> Res<Vec<(String, String, String)>> {
    let master = if schema.eq_ignore_ascii_case("temp") {
        "sqlite_temp_master".to_string()
    } else {
        format!("{}.sqlite_master", qident(schema))
    };
    let st = Stmt::new(
        db,
        &format!(
            "SELECT type, name, sql FROM {master} \
             WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
             AND name NOT LIKE '\\_\\_turso\\_%' ESCAPE '\\' ORDER BY rowid"
        ),
    )?;
    let mut out = Vec::new();
    while st.step()? {
        out.push((
            st.text(0).unwrap_or_default(),
            st.text(1).unwrap_or_default(),
            strip_temp(st.text(2).unwrap_or_default()),
        ));
    }
    Ok(out)
}

/// `CREATE TEMP TABLE x` -> `CREATE TABLE x`: copied objects land in main.
fn strip_temp(sql: String) -> String {
    let mut words = sql.splitn(3, char::is_whitespace);
    if let (Some(c), Some(t), Some(rest)) = (words.next(), words.next(), words.next()) {
        if c.eq_ignore_ascii_case("create")
            && (t.eq_ignore_ascii_case("temp") || t.eq_ignore_ascii_case("temporary"))
        {
            return format!("CREATE {rest}");
        }
    }
    sql
}

unsafe fn table_columns(db: *mut sqlite3, schema: &str, table: &str) -> Res<Vec<String>> {
    let st = Stmt::new(
        db,
        &format!("PRAGMA {}.table_info({})", qident(schema), qident(table)),
    )?;
    let mut cols = Vec::new();
    while st.step()? {
        cols.push(st.text(1).unwrap_or_default());
    }
    Ok(cols)
}

/// Copies up to `limit` rows of `table` with rowid > `after` (all rows for
/// tables without a rowid). Returns the rows copied and the last rowid.
unsafe fn copy_table_rows(
    src: *mut sqlite3,
    dst: *mut sqlite3,
    schema: &str,
    table: &str,
    after: i64,
    limit: i64,
) -> Res<(i64, i64, bool)> {
    let cols = table_columns(src, schema, table)?;
    if cols.is_empty() {
        return Ok((0, after, false));
    }
    let col_sql = cols.iter().map(|c| qident(c)).collect::<Vec<_>>().join(",");
    let from = format!("{}.{}", qident(schema), qident(table));
    // Preserve rowids when the table has them (WITHOUT ROWID tables do not).
    let (sel, ins_cols, off, keyed) = match Stmt::new(
        src,
        &format!(
            "SELECT rowid,{col_sql} FROM {from} WHERE rowid > {after} ORDER BY rowid LIMIT {limit}"
        ),
    ) {
        Ok(s) => (s, format!("rowid,{col_sql}"), 1, true),
        Err(_) => (
            Stmt::new(src, &format!("SELECT {col_sql} FROM {from}"))?,
            col_sql.clone(),
            0,
            false,
        ),
    };
    let n = cols.len() + off as usize;
    let marks = vec!["?"; n].join(",");
    let ins = Stmt::new(
        dst,
        &format!(
            "INSERT INTO {}({}) VALUES({})",
            qident(table),
            ins_cols,
            marks
        ),
    )?;
    let (mut rows, mut last) = (0, after);
    while sel.step()? {
        for i in 0..n as ffi::c_int {
            let rc = match sqlite3_column_type(sel.raw, i) {
                SQLITE_INTEGER => {
                    sqlite3_bind_int64(ins.raw, i + 1, sqlite3_column_int64(sel.raw, i))
                }
                SQLITE_FLOAT => {
                    sqlite3_bind_double(ins.raw, i + 1, sqlite3_column_double(sel.raw, i))
                }
                SQLITE_TEXT => {
                    let p = sqlite3_column_text(sel.raw, i);
                    let len = sqlite3_column_bytes(sel.raw, i);
                    sqlite3_bind_text(
                        ins.raw,
                        i + 1,
                        p as *const ffi::c_char,
                        len,
                        sqlite_transient(),
                    )
                }
                SQLITE_BLOB => {
                    let p = sqlite3_column_blob(sel.raw, i);
                    let len = sqlite3_column_bytes(sel.raw, i);
                    if p.is_null() {
                        sqlite3_bind_blob(
                            ins.raw,
                            i + 1,
                            b"".as_ptr() as *const ffi::c_void,
                            0,
                            sqlite_transient(),
                        )
                    } else {
                        sqlite3_bind_blob(ins.raw, i + 1, p, len, sqlite_transient())
                    }
                }
                _ => sqlite3_bind_null(ins.raw, i + 1),
            };
            if rc != SQLITE_OK {
                return Err((rc, errmsg_string(dst)));
            }
        }
        if keyed {
            last = sqlite3_column_int64(sel.raw, 0);
        }
        ins.step()?;
        sqlite3_reset(ins.raw);
        rows += 1;
    }
    Ok((rows, last, keyed))
}

fn sqlite_transient() -> Option<unsafe extern "C" fn(*mut ffi::c_void)> {
    // SQLITE_TRANSIENT is (void(*)(void*))-1.
    unsafe { std::mem::transmute::<isize, Option<unsafe extern "C" fn(*mut ffi::c_void)>>(-1) }
}

/// Incremental logical copy of schema `ss` of `src` into main of `dst`.
struct Copier {
    ss: String,
    objs: Vec<(String, String, String)>,
    /// (table, last copied rowid, fully copied)
    tables: Vec<(String, i64, bool)>,
    rows_total: i64,
    rows_done: i64,
}

impl Copier {
    /// Wipes `dst`, creates the tables and counts the source rows.
    unsafe fn start(src: *mut sqlite3, dst: *mut sqlite3, ss: &str) -> Res<Copier> {
        if sqlite3_get_autocommit(dst) == 0 {
            return Err((SQLITE_BUSY, "destination database is in use".to_string()));
        }
        let objs = schema_objects(src, ss)?;
        let tables: Vec<(String, i64, bool)> = objs
            .iter()
            .filter(|o| {
                o.0 == "table"
                    && !o
                        .2
                        .trim_start()
                        .to_ascii_uppercase()
                        .starts_with("CREATE VIRTUAL")
            })
            .map(|o| (o.1.clone(), i64::MIN, false))
            .collect();
        let mut rows_total = 0;
        for (t, _, _) in &tables {
            let st = Stmt::new(
                src,
                &format!("SELECT count(*) FROM {}.{}", qident(ss), qident(t)),
            )?;
            if st.step()? {
                rows_total += sqlite3_column_int64(st.raw, 0);
            }
        }
        exec(dst, "BEGIN")?;
        let r = (|| -> Res<()> {
            let old = schema_objects(dst, "main")?;
            for kind in ["trigger", "view", "table"] {
                for (_, name, _) in old.iter().filter(|o| o.0 == kind) {
                    exec(
                        dst,
                        &format!("DROP {} IF EXISTS {}", kind.to_uppercase(), qident(name)),
                    )?;
                }
            }
            for (_, _, sql) in objs.iter().filter(|o| o.0 == "table") {
                exec(dst, sql)?;
            }
            Ok(())
        })();
        finish_txn(dst, r)?;
        Ok(Copier {
            ss: ss.to_string(),
            objs,
            tables,
            rows_total,
            rows_done: 0,
        })
    }

    /// Copies up to `budget` rows (all when None), in its own dest transaction.
    unsafe fn copy_rows(
        &mut self,
        src: *mut sqlite3,
        dst: *mut sqlite3,
        budget: Option<i64>,
    ) -> Res<()> {
        exec(dst, "BEGIN")?;
        let r = (|| -> Res<()> {
            let mut left = budget.unwrap_or(i64::MAX);
            for (t, last, done) in self.tables.iter_mut() {
                while !*done && left > 0 {
                    let after = if *last == i64::MIN { i64::MIN } else { *last };
                    let (n, l, keyed) = copy_table_rows(src, dst, &self.ss, t, after, left)?;
                    self.rows_done += n;
                    left -= n;
                    *last = l;
                    if !keyed || n == 0 {
                        *done = true;
                    }
                    if n == 0 {
                        break;
                    }
                }
            }
            Ok(())
        })();
        finish_txn(dst, r)
    }

    /// Rows appended to the source after they were passed over.
    unsafe fn has_more(&self, src: *mut sqlite3) -> bool {
        self.tables.iter().any(|(t, last, _)| {
            Stmt::new(
                src,
                &format!(
                    "SELECT 1 FROM {}.{} WHERE rowid > {} LIMIT 1",
                    qident(&self.ss),
                    qident(t),
                    last
                ),
            )
            .and_then(|s| s.step())
            .unwrap_or(false)
        })
    }

    /// Indexes, views, triggers, sqlite_sequence and user_version.
    unsafe fn finish(&self, src: *mut sqlite3, dst: *mut sqlite3) -> Res<()> {
        let user_version = Stmt::new(src, &format!("PRAGMA {}.user_version", qident(&self.ss)))
            .and_then(|s| {
                s.step()?;
                Ok(sqlite3_column_int64(s.raw, 0))
            })
            .unwrap_or(0);
        let seqs: Vec<(String, i64)> = (|| -> Res<Vec<(String, i64)>> {
            let s = Stmt::new(
                src,
                &format!("SELECT name, seq FROM {}.sqlite_sequence", qident(&self.ss)),
            )?;
            let mut v = Vec::new();
            while s.step()? {
                v.push((
                    s.text(0).unwrap_or_default(),
                    sqlite3_column_int64(s.raw, 1),
                ));
            }
            Ok(v)
        })()
        .unwrap_or_default();
        exec(dst, "BEGIN")?;
        let r = (|| -> Res<()> {
            for (name, seq) in &seqs {
                let _ = exec(
                    dst,
                    &format!("DELETE FROM sqlite_sequence WHERE name={}", sql_quote(name)),
                );
                let _ = exec(
                    dst,
                    &format!(
                        "INSERT INTO sqlite_sequence(name,seq) VALUES({},{})",
                        sql_quote(name),
                        seq
                    ),
                );
            }
            for (_, _, sql) in self.objs.iter().filter(|o| o.0 != "table") {
                exec(dst, sql)?;
            }
            let _ = exec(dst, &format!("PRAGMA user_version={user_version}"));
            Ok(())
        })();
        finish_txn(dst, r)
    }
}

unsafe fn finish_txn(dst: *mut sqlite3, r: Res<()>) -> Res<()> {
    match r {
        Ok(()) => exec(dst, "COMMIT"),
        Err(e) => {
            let _ = exec(dst, "ROLLBACK");
            Err(e)
        }
    }
}

/// Replace main of `dst` with a copy of main of `src` in one go.
unsafe fn copy_db(src: *mut sqlite3, dst: *mut sqlite3) -> Res<()> {
    // Read the source in one snapshot when it is not already in a transaction.
    let src_txn = sqlite3_get_autocommit(src) != 0 && exec(src, "BEGIN").is_ok();
    let r = (|| {
        let mut c = Copier::start(src, dst, "main")?;
        c.copy_rows(src, dst, None)?;
        c.finish(src, dst)
    })();
    if src_txn {
        let _ = exec(src, "COMMIT");
    }
    r
}

fn sql_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

unsafe fn page_count(db: *mut sqlite3, schema: &str) -> ffi::c_int {
    Stmt::new(db, &format!("PRAGMA {}.page_count", qident(schema)))
        .and_then(|s| {
            s.step()?;
            Ok(sqlite3_column_int64(s.raw, 0) as ffi::c_int)
        })
        .unwrap_or(1)
        .max(1)
}

// ---------------------------------------------------------------------------
// Backup API
// ---------------------------------------------------------------------------

/// Progress is measured in "pages" (the source's page count); each step
/// copies the matching share of the rows, so nPage works incrementally.
struct Backup {
    dest: *mut sqlite3,
    src: *mut sqlite3,
    schema: String,
    copier: Option<Copier>,
    pagecount: ffi::c_int,
    remaining: ffi::c_int,
    pages_done: ffi::c_int,
    rc: ffi::c_int,
}

fn is_main(name: *const ffi::c_char) -> bool {
    !name.is_null()
        && unsafe { CStr::from_ptr(name) }
            .to_bytes()
            .eq_ignore_ascii_case(b"main")
}

unsafe fn set_err(db: *mut sqlite3, rc: ffi::c_int, msg: &str) {
    let mut inner = (*db).inner.lock().unwrap();
    set_db_err_msg(&mut inner, rc, msg);
}

unsafe fn db_exists(db: *mut sqlite3, name: &str) -> bool {
    if name.eq_ignore_ascii_case("temp") {
        return true;
    }
    let Ok(st) = Stmt::new(db, "PRAGMA database_list") else {
        return name.eq_ignore_ascii_case("main");
    };
    while let Ok(true) = st.step() {
        if st.text(1).is_some_and(|n| n.eq_ignore_ascii_case(name)) {
            return true;
        }
    }
    false
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_init(
    dest_db: *mut sqlite3,
    dest_name: *const ffi::c_char,
    source_db: *mut sqlite3,
    source_name: *const ffi::c_char,
) -> *mut ffi::c_void {
    if dest_db.is_null() {
        return std::ptr::null_mut();
    }
    if source_db.is_null() || dest_name.is_null() || source_name.is_null() {
        set_err(dest_db, SQLITE_MISUSE, "bad parameter or other API misuse");
        return std::ptr::null_mut();
    }
    if source_db == dest_db {
        set_err(
            dest_db,
            SQLITE_ERROR,
            "source and destination must be distinct",
        );
        return std::ptr::null_mut();
    }
    let dname = CStr::from_ptr(dest_name).to_string_lossy().into_owned();
    let sname = CStr::from_ptr(source_name).to_string_lossy().into_owned();
    // Only main can be written; any existing schema of the source can be read.
    if !dname.eq_ignore_ascii_case("main") {
        set_err(dest_db, SQLITE_ERROR, &format!("unknown database {dname}"));
        return std::ptr::null_mut();
    }
    if !db_exists(source_db, &sname) {
        set_err(dest_db, SQLITE_ERROR, &format!("unknown database {sname}"));
        return std::ptr::null_mut();
    }
    if sqlite3_get_autocommit(dest_db) == 0 {
        set_err(dest_db, SQLITE_BUSY, "destination database is in use");
        return std::ptr::null_mut();
    }
    let pc = page_count(source_db, &sname);
    Box::into_raw(Box::new(Backup {
        dest: dest_db,
        src: source_db,
        schema: sname,
        copier: None,
        pagecount: pc,
        remaining: pc,
        pages_done: 0,
        rc: SQLITE_OK,
    })) as *mut ffi::c_void
}

unsafe fn backup_step(b: &mut Backup, n_pages: ffi::c_int) -> Res<ffi::c_int> {
    if b.copier.is_none() {
        b.copier = Some(Copier::start(b.src, b.dest, &b.schema)?);
    }
    let c = b.copier.as_mut().unwrap();
    let all = n_pages < 0 || b.pages_done.saturating_add(n_pages) >= b.pagecount;
    b.pages_done = if n_pages < 0 {
        b.pagecount
    } else {
        b.pages_done.saturating_add(n_pages).min(b.pagecount)
    };
    let target = (c.rows_total as i128 * b.pages_done as i128 + b.pagecount as i128 - 1)
        / b.pagecount as i128;
    let budget = if all {
        c.rows_total - c.rows_done
    } else {
        target as i64 - c.rows_done
    };
    // Rows beyond the initial count (added meanwhile) wait for another step.
    c.copy_rows(b.src, b.dest, Some(budget.max(0)))?;
    if all && c.has_more(b.src) {
        b.pagecount = page_count(b.src, &b.schema).max(b.pages_done + 1);
        c.rows_total = c.rows_done + 1;
        b.remaining = b.pagecount - b.pages_done;
        return Ok(SQLITE_OK);
    }
    if all {
        c.finish(b.src, b.dest)?;
        b.remaining = 0;
        b.pagecount = page_count(b.src, &b.schema);
        return Ok(SQLITE_DONE);
    }
    b.remaining = b.pagecount - b.pages_done;
    Ok(SQLITE_OK)
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_step(
    backup: *mut ffi::c_void,
    n_pages: ffi::c_int,
) -> ffi::c_int {
    if backup.is_null() {
        return SQLITE_MISUSE;
    }
    let b = &mut *(backup as *mut Backup);
    if b.rc == SQLITE_DONE {
        return SQLITE_DONE;
    }
    if b.rc != SQLITE_OK {
        return b.rc;
    }
    match backup_step(b, n_pages) {
        Ok(rc) => {
            b.rc = if rc == SQLITE_DONE {
                SQLITE_DONE
            } else {
                SQLITE_OK
            };
            rc
        }
        Err((rc, msg)) => {
            b.rc = rc;
            set_err(b.dest, rc, &msg);
            rc
        }
    }
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_remaining(backup: *mut ffi::c_void) -> ffi::c_int {
    if backup.is_null() {
        return 0;
    }
    (*(backup as *mut Backup)).remaining
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_pagecount(backup: *mut ffi::c_void) -> ffi::c_int {
    if backup.is_null() {
        return 0;
    }
    (*(backup as *mut Backup)).pagecount
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_finish(backup: *mut ffi::c_void) -> ffi::c_int {
    if backup.is_null() {
        return SQLITE_OK;
    }
    let b = Box::from_raw(backup as *mut Backup);
    if b.rc == SQLITE_DONE {
        SQLITE_OK
    } else {
        b.rc
    }
}

// ---------------------------------------------------------------------------
// serialize / deserialize (via a temporary database file)
// ---------------------------------------------------------------------------

const SQLITE_SERIALIZE_NOCOPY: ffi::c_uint = 1;
const SQLITE_DESERIALIZE_FREEONCLOSE: ffi::c_uint = 1;

fn tmp_db_path() -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("turso-ser-{}-{}.db", std::process::id(), n))
}

fn remove_db_files(p: &std::path::Path) {
    let base = p.to_string_lossy().into_owned();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{base}{suffix}"));
    }
}

unsafe fn open_path(p: &std::path::Path) -> Res<*mut sqlite3> {
    let c = CString::new(p.to_string_lossy().as_bytes())
        .map_err(|_| (SQLITE_ERROR, "bad path".into()))?;
    let mut db = std::ptr::null_mut();
    let rc = sqlite3_open(c.as_ptr(), &mut db);
    if rc != SQLITE_OK {
        let msg = if db.is_null() {
            "cannot open temporary database".to_string()
        } else {
            errmsg_string(db)
        };
        if !db.is_null() {
            sqlite3_close(db);
        }
        return Err((rc, msg));
    }
    Ok(db)
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_serialize(
    db: *mut sqlite3,
    schema: *const ffi::c_char,
    out_size: *mut i64,
    flags: ffi::c_uint,
) -> *mut ffi::c_uchar {
    if !out_size.is_null() {
        *out_size = 0;
    }
    if db.is_null() || (!schema.is_null() && !is_main(schema)) {
        return std::ptr::null_mut();
    }
    // There is no in-place buffer to hand out.
    if flags & SQLITE_SERIALIZE_NOCOPY != 0 {
        return std::ptr::null_mut();
    }
    let path = tmp_db_path();
    let result = (|| -> Res<Vec<u8>> {
        let tmp = open_path(&path)?;
        let r = copy_db(db, tmp);
        sqlite3_close(tmp);
        r?;
        std::fs::read(&path).map_err(|e| (SQLITE_IOERR, e.to_string()))
    })();
    remove_db_files(&path);
    match result {
        Ok(bytes) => {
            let p = libc::malloc(bytes.len().max(1)) as *mut u8;
            if p.is_null() {
                return std::ptr::null_mut();
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
            if !out_size.is_null() {
                *out_size = bytes.len() as i64;
            }
            p
        }
        Err((rc, msg)) => {
            set_err(db, rc, &msg);
            std::ptr::null_mut()
        }
    }
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_deserialize(
    db: *mut sqlite3,
    schema: *const ffi::c_char,
    data: *mut ffi::c_uchar,
    sz_db: i64,
    sz_buf: i64,
    flags: ffi::c_uint,
) -> ffi::c_int {
    let free_it = |data: *mut ffi::c_uchar| {
        if flags & SQLITE_DESERIALIZE_FREEONCLOSE != 0 && !data.is_null() {
            libc::free(data as *mut ffi::c_void);
        }
    };
    if db.is_null() {
        free_it(data);
        return SQLITE_MISUSE;
    }
    if (!schema.is_null() && !is_main(schema)) || (data.is_null() && sz_db > 0) {
        free_it(data);
        set_err(db, SQLITE_ERROR, "unable to deserialize database");
        return SQLITE_ERROR;
    }
    let sz = if sz_db < 0 { sz_buf } else { sz_db };
    if sz < 0 || sz > sz_buf.max(sz) {
        free_it(data);
        return SQLITE_MISUSE;
    }
    if sqlite3_get_autocommit(db) == 0 {
        free_it(data);
        set_err(db, SQLITE_BUSY, "database is in use");
        return SQLITE_BUSY;
    }
    let bytes: &[u8] = if sz == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(data, sz as usize)
    };
    if !bytes.is_empty() && (bytes.len() < 100 || &bytes[..16] != b"SQLite format 3\0") {
        free_it(data);
        set_err(db, SQLITE_NOTADB, "file is not a database");
        return SQLITE_NOTADB;
    }
    let path = tmp_db_path();
    let result = (|| -> Res<()> {
        std::fs::write(&path, bytes).map_err(|e| (SQLITE_IOERR, e.to_string()))?;
        let tmp = open_path(&path)?;
        let r = copy_db(tmp, db);
        sqlite3_close(tmp);
        r
    })();
    remove_db_files(&path);
    free_it(data);
    match result {
        Ok(()) => SQLITE_OK,
        Err((rc, msg)) => {
            set_err(db, rc, &msg);
            rc
        }
    }
}

// ---------------------------------------------------------------------------
// UTF-16 API
// ---------------------------------------------------------------------------

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
enum Kind {
    Text,
    Name,
    Decl,
    Table,
    ErrMsg,
}

type U16Map = HashMap<(usize, ffi::c_int, Kind), Vec<u16>>;
static U16_CACHE: Mutex<Option<U16Map>> = Mutex::new(None);

/// Store `s` (NUL-terminated UTF-16) for (handle, col, kind) and return a
/// pointer to it. An identical previous value keeps its buffer so that
/// pointers handed out earlier for the same unchanged value stay valid.
fn u16_store(handle: usize, col: ffi::c_int, kind: Kind, s: &[u8]) -> *const ffi::c_void {
    let text = String::from_utf8_lossy(s);
    let mut v: Vec<u16> = text.encode_utf16().collect();
    v.push(0);
    let mut g = U16_CACHE.lock().unwrap();
    let map = g.get_or_insert_with(HashMap::new);
    let e = map.entry((handle, col, kind)).or_default();
    if *e != v {
        *e = v;
    }
    e.as_ptr() as *const ffi::c_void
}

pub(crate) fn utf16_release_stmt(stmt: *mut sqlite3_stmt) {
    if let Some(m) = U16_CACHE.lock().unwrap().as_mut() {
        m.retain(|k, _| k.0 != stmt as usize || k.2 == Kind::ErrMsg);
    }
}

pub(crate) fn utf16_release_db(db: *mut sqlite3) {
    if let Some(m) = U16_CACHE.lock().unwrap().as_mut() {
        m.retain(|k, _| !(k.0 == db as usize && k.2 == Kind::ErrMsg));
    }
}

unsafe fn utf16_units(p: *const u16, nbyte: ffi::c_int) -> Vec<u16> {
    let max = if nbyte < 0 {
        usize::MAX
    } else {
        nbyte as usize / 2
    };
    let mut v = Vec::new();
    while v.len() < max {
        let u = *p.add(v.len());
        if u == 0 {
            break;
        }
        v.push(u);
    }
    v
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_prepare16_v2(
    db: *mut sqlite3,
    sql: *const ffi::c_void,
    n_byte: ffi::c_int,
    out_stmt: *mut *mut sqlite3_stmt,
    tail: *mut *const ffi::c_void,
) -> ffi::c_int {
    if db.is_null() || sql.is_null() || out_stmt.is_null() {
        return SQLITE_MISUSE;
    }
    let units = utf16_units(sql as *const u16, n_byte);
    let mut bytes = String::from_utf16_lossy(&units).into_bytes();
    let len = bytes.len();
    bytes.push(0);
    let mut tail8: *const ffi::c_char = std::ptr::null();
    let rc = sqlite3_prepare_v2(
        db,
        bytes.as_ptr() as *const ffi::c_char,
        len as ffi::c_int,
        out_stmt,
        &mut tail8,
    );
    if !tail.is_null() {
        let consumed = if tail8.is_null() {
            len
        } else {
            (tail8 as usize)
                .saturating_sub(bytes.as_ptr() as usize)
                .min(len)
        };
        let n16 = String::from_utf8_lossy(&bytes[..consumed])
            .encode_utf16()
            .count();
        *tail = (sql as *const u16).add(n16) as *const ffi::c_void;
    }
    rc
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_errmsg16(db: *mut sqlite3) -> *const ffi::c_void {
    let p = sqlite3_errmsg(db);
    if p.is_null() {
        return std::ptr::null();
    }
    u16_store(db as usize, -1, Kind::ErrMsg, CStr::from_ptr(p).to_bytes())
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_text16(
    stmt: *mut sqlite3_stmt,
    idx: ffi::c_int,
    data: *const ffi::c_void,
    n_byte: ffi::c_int,
    destructor: Option<unsafe extern "C" fn(*mut ffi::c_void)>,
) -> ffi::c_int {
    if stmt.is_null() {
        return SQLITE_MISUSE;
    }
    let rc = if data.is_null() {
        sqlite3_bind_null(stmt, idx)
    } else {
        let units = utf16_units(data as *const u16, n_byte);
        let bytes = String::from_utf16_lossy(&units).into_bytes();
        sqlite3_bind_text(
            stmt,
            idx,
            bytes.as_ptr() as *const ffi::c_char,
            bytes.len() as ffi::c_int,
            sqlite_transient(),
        )
    };
    // The text was copied, so a real destructor can run right away
    // (SQLITE_STATIC is 0 and SQLITE_TRANSIENT is -1: neither is called).
    if let Some(d) = destructor {
        if d as usize != usize::MAX && !data.is_null() {
            d(data as *mut ffi::c_void);
        }
    }
    rc
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_text16(
    stmt: *mut sqlite3_stmt,
    idx: ffi::c_int,
) -> *const ffi::c_void {
    let p = sqlite3_column_text(stmt, idx);
    if p.is_null() {
        return std::ptr::null();
    }
    let n = sqlite3_column_bytes(stmt, idx) as usize;
    u16_store(
        stmt as usize,
        idx,
        Kind::Text,
        std::slice::from_raw_parts(p, n),
    )
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_bytes16(
    stmt: *mut sqlite3_stmt,
    idx: ffi::c_int,
) -> ffi::c_int {
    let p = sqlite3_column_text(stmt, idx);
    if p.is_null() {
        return 0;
    }
    let n = sqlite3_column_bytes(stmt, idx) as usize;
    // Computed without touching the cache so that a pointer previously
    // returned by sqlite3_column_text16 is not invalidated.
    let s = String::from_utf8_lossy(std::slice::from_raw_parts(p, n));
    (s.encode_utf16().count() * 2) as ffi::c_int
}

unsafe fn col_str16(
    stmt: *mut sqlite3_stmt,
    idx: ffi::c_int,
    kind: Kind,
    f: unsafe extern "C" fn(*mut sqlite3_stmt, ffi::c_int) -> *const ffi::c_char,
) -> *const ffi::c_void {
    if stmt.is_null() || idx < 0 || idx >= sqlite3_column_count(stmt) {
        return std::ptr::null();
    }
    let p = f(stmt, idx);
    if p.is_null() {
        return std::ptr::null();
    }
    u16_store(stmt as usize, idx, kind, CStr::from_ptr(p).to_bytes())
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_name16(
    stmt: *mut sqlite3_stmt,
    idx: ffi::c_int,
) -> *const ffi::c_void {
    col_str16(stmt, idx, Kind::Name, sqlite3_column_name)
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_decltype16(
    stmt: *mut sqlite3_stmt,
    idx: ffi::c_int,
) -> *const ffi::c_void {
    col_str16(stmt, idx, Kind::Decl, sqlite3_column_decltype)
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_table_name16(
    stmt: *mut sqlite3_stmt,
    idx: ffi::c_int,
) -> *const ffi::c_void {
    col_str16(stmt, idx, Kind::Table, sqlite3_column_table_name)
}
