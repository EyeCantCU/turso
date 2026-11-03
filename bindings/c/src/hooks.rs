//! Callback APIs: sqlite3_commit_hook, sqlite3_rollback_hook,
//! sqlite3_update_hook, sqlite3_preupdate_hook (and the sqlite3_preupdate_*
//! accessors), sqlite3_wal_hook, sqlite3_wal_autocheckpoint and
//! sqlite3_set_authorizer. The engine side lives in turso_core
//! (`Connection::set_*_hook`, `Connection::set_authorizer`).

use super::*;
use std::cell::RefCell;
use std::sync::atomic::AtomicI32;

pub const SQLITE_CONSTRAINT_COMMITHOOK: ffi::c_int = SQLITE_CONSTRAINT | (2 << 8);

/// Per-handle hook bookkeeping, outside the handle mutex so callbacks never
/// need it.
pub(crate) struct HookState {
    /// The user-data pointers last registered; every register call returns
    /// the previous one.
    args: Mutex<HookArgs>,
    /// Non-zero result of the last WAL hook call, returned by sqlite3_step.
    wal_rc: AtomicI32,
}

#[derive(Clone, Copy)]
struct HookArgs {
    commit: usize,
    rollback: usize,
    update: usize,
    preupdate: usize,
    wal: usize,
}

impl HookState {
    pub(crate) fn new() -> Self {
        Self {
            args: Mutex::new(HookArgs {
                commit: 0,
                rollback: 0,
                update: 0,
                preupdate: 0,
                // SQLite's open installs its auto-checkpoint WAL hook with
                // user data (void*)1000; the first sqlite3_wal_hook returns it.
                wal: 1000,
            }),
            wal_rc: AtomicI32::new(SQLITE_OK),
        }
    }

    fn swap(
        &self,
        select: fn(&mut HookArgs) -> &mut usize,
        new: *mut ffi::c_void,
    ) -> *mut ffi::c_void {
        let mut args = self.args.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::replace(select(&mut args), new as usize) as *mut ffi::c_void
    }

    /// The WAL hook's error, if its last call returned one.
    pub(crate) fn take_wal_rc(&self) -> ffi::c_int {
        self.wal_rc.swap(SQLITE_OK, Ordering::SeqCst)
    }
}

/// For reset/finalize: a WAL hook error left by the last step becomes the
/// result (and the handle's error), unless something else failed first.
pub(crate) unsafe fn take_wal_hook_error(
    stmt: *mut sqlite3_stmt,
    result: ffi::c_int,
) -> ffi::c_int {
    let stmt = &mut *stmt;
    let rc = std::mem::replace(&mut stmt.wal_hook_rc, SQLITE_OK);
    if result != SQLITE_OK || rc == SQLITE_OK {
        return result;
    }
    set_db_err_code(&mut stmt.db.inner.lock().unwrap(), rc)
}

unsafe fn db_conn(db: *mut sqlite3) -> Option<Arc<turso_core::Connection>> {
    if db.is_null() {
        return None;
    }
    let inner = (*db).inner.lock().ok()?;
    Some(inner.conn.clone())
}

fn c_name(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

pub type sqlite3_commit_hook_cb = unsafe extern "C" fn(*mut ffi::c_void) -> ffi::c_int;
pub type sqlite3_rollback_hook_cb = unsafe extern "C" fn(*mut ffi::c_void);
pub type sqlite3_update_hook_cb =
    unsafe extern "C" fn(*mut ffi::c_void, ffi::c_int, *const ffi::c_char, *const ffi::c_char, i64);
pub type sqlite3_wal_hook_cb = unsafe extern "C" fn(
    *mut ffi::c_void,
    *mut sqlite3,
    *const ffi::c_char,
    ffi::c_int,
) -> ffi::c_int;
pub type sqlite3_preupdate_hook_cb = unsafe extern "C" fn(
    *mut ffi::c_void,
    *mut sqlite3,
    ffi::c_int,
    *const ffi::c_char,
    *const ffi::c_char,
    i64,
    i64,
);

/// Runs before each transaction that wrote to a database commits; a non-zero
/// return turns the commit into a rollback (SQLITE_CONSTRAINT_COMMITHOOK).
#[no_mangle]
pub unsafe extern "C" fn sqlite3_commit_hook(
    db: *mut sqlite3,
    callback: Option<sqlite3_commit_hook_cb>,
    arg: *mut ffi::c_void,
) -> *mut ffi::c_void {
    let Some(conn) = db_conn(db) else {
        return std::ptr::null_mut();
    };
    let prev = (*db).hooks.swap(|h| &mut h.commit, arg);
    let arg = arg as usize;
    conn.set_commit_hook(callback.map(|cb| {
        Arc::new(move || unsafe { cb(arg as *mut ffi::c_void) != 0 })
            as turso_core::CommitHookCallback
    }));
    prev
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_rollback_hook(
    db: *mut sqlite3,
    callback: Option<sqlite3_rollback_hook_cb>,
    arg: *mut ffi::c_void,
) -> *mut ffi::c_void {
    let Some(conn) = db_conn(db) else {
        return std::ptr::null_mut();
    };
    let prev = (*db).hooks.swap(|h| &mut h.rollback, arg);
    let arg = arg as usize;
    conn.set_rollback_hook(callback.map(|cb| {
        Arc::new(move || unsafe { cb(arg as *mut ffi::c_void) }) as turso_core::RollbackHookCallback
    }));
    prev
}

/// Runs after each row inserted, updated or deleted in a rowid table.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_update_hook(
    db: *mut sqlite3,
    callback: Option<sqlite3_update_hook_cb>,
    arg: *mut ffi::c_void,
) -> *mut ffi::c_void {
    let Some(conn) = db_conn(db) else {
        return std::ptr::null_mut();
    };
    let prev = (*db).hooks.swap(|h| &mut h.update, arg);
    let arg = arg as usize;
    conn.set_update_hook(callback.map(|cb| {
        Arc::new(
            move |op: turso_core::UpdateHookOp, db_name: &str, table: &str, rowid: i64| {
                let (db_name, table) = (c_name(db_name), c_name(table));
                unsafe {
                    cb(
                        arg as *mut ffi::c_void,
                        op as ffi::c_int,
                        db_name.as_ptr(),
                        table.as_ptr(),
                        rowid,
                    )
                }
            },
        ) as turso_core::UpdateHookCallback
    }));
    prev
}

/// Runs after each commit to a WAL database with the number of frames in
/// its WAL. Like SQLite, it replaces the auto-checkpoint, and a non-zero
/// return becomes the result of the sqlite3_step that committed.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_wal_hook(
    db: *mut sqlite3,
    callback: Option<sqlite3_wal_hook_cb>,
    arg: *mut ffi::c_void,
) -> *mut ffi::c_void {
    let Some(conn) = db_conn(db) else {
        return std::ptr::null_mut();
    };
    let prev = (*db).hooks.swap(|h| &mut h.wal, arg);
    let arg = arg as usize;
    let db_addr = db as usize;
    conn.set_wal_hook(callback.map(|cb| {
        Arc::new(move |db_name: &str, frames: u64| {
            let name = c_name(db_name);
            let rc = unsafe {
                cb(
                    arg as *mut ffi::c_void,
                    db_addr as *mut sqlite3,
                    name.as_ptr(),
                    frames.min(ffi::c_int::MAX as u64) as ffi::c_int,
                )
            };
            if rc != SQLITE_OK {
                let db = unsafe { &*(db_addr as *const sqlite3) };
                db.hooks.wal_rc.store(rc, Ordering::SeqCst);
            }
        }) as turso_core::WalHookCallback
    }));
    prev
}

/// Checkpoint automatically once the WAL holds `n` frames (`n <= 0`
/// disables). Replaces any WAL hook, as in SQLite.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_wal_autocheckpoint(db: *mut sqlite3, n: ffi::c_int) -> ffi::c_int {
    let Some(conn) = db_conn(db) else {
        return SQLITE_MISUSE;
    };
    (*db)
        .hooks
        .swap(|h| &mut h.wal, n.max(0) as usize as *mut ffi::c_void);
    conn.set_wal_autocheckpoint_frames(n.max(0) as u32);
    SQLITE_OK
}

/// The row change a preupdate callback is running for.
struct PreupdateFrame {
    db: usize,
    op: turso_core::UpdateHookOp,
    old: Option<Vec<Value>>,
    new: Option<Vec<Value>>,
    column_count: usize,
    depth: i32,
    blob_write: i32,
    /// sqlite3_value objects handed out; boxed so their addresses are stable.
    #[allow(clippy::vec_box)]
    values: RefCell<Vec<Box<ExtValue>>>,
}

impl Drop for PreupdateFrame {
    fn drop(&mut self) {
        for v in self.values.get_mut().drain(..) {
            unsafe { (*v).__free_internal_type() };
        }
    }
}

thread_local! {
    static PREUPDATE: RefCell<Vec<PreupdateFrame>> = const { RefCell::new(Vec::new()) };
}

fn with_frame<R>(db: *mut sqlite3, f: impl FnOnce(Option<&PreupdateFrame>) -> R) -> R {
    PREUPDATE.with(|stack| {
        let stack = stack.borrow();
        f(stack.iter().rev().find(|fr| fr.db == db as usize))
    })
}

fn to_ext_value(v: &Value) -> ExtValue {
    match v {
        Value::Numeric(turso_core::Numeric::Integer(n)) => ExtValue::from_integer(*n),
        Value::Numeric(turso_core::Numeric::Float(f)) => ExtValue::from_float(f64::from(*f)),
        Value::Text(t) => ExtValue::from_text(t.value.to_string()),
        Value::Blob(b) => ExtValue::from_blob(b.clone()),
        _ => ExtValue::null(),
    }
}

/// Runs before each row of a rowid table is inserted, updated or deleted;
/// the sqlite3_preupdate_* functions read the row from inside it.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_preupdate_hook(
    db: *mut sqlite3,
    callback: Option<sqlite3_preupdate_hook_cb>,
    arg: *mut ffi::c_void,
) -> *mut ffi::c_void {
    let Some(conn) = db_conn(db) else {
        return std::ptr::null_mut();
    };
    let prev = (*db).hooks.swap(|h| &mut h.preupdate, arg);
    let arg = arg as usize;
    let db_addr = db as usize;
    conn.set_preupdate_hook(callback.map(|cb| {
        Arc::new(move |ev: &turso_core::PreUpdateEvent<'_>| {
            let (db_name, table) = (c_name(ev.db_name), c_name(ev.table));
            let frame = PreupdateFrame {
                db: db_addr,
                op: ev.op,
                old: ev.old.map(|v| v.to_vec()),
                new: ev.new.map(|v| v.to_vec()),
                column_count: ev.column_count,
                depth: ev.depth,
                blob_write: ev.blob_write,
                values: RefCell::new(Vec::new()),
            };
            PREUPDATE.with(|s| s.borrow_mut().push(frame));
            unsafe {
                cb(
                    arg as *mut ffi::c_void,
                    db_addr as *mut sqlite3,
                    ev.op as ffi::c_int,
                    db_name.as_ptr(),
                    table.as_ptr(),
                    ev.old_rowid,
                    ev.new_rowid,
                )
            };
            let frame = PREUPDATE.with(|s| s.borrow_mut().pop());
            drop(frame);
        }) as turso_core::PreUpdateHookCallback
    }));
    prev
}

unsafe fn preupdate_value(
    db: *mut sqlite3,
    i: ffi::c_int,
    pp_value: *mut *mut ffi::c_void,
    want_old: bool,
) -> ffi::c_int {
    if db.is_null() || pp_value.is_null() {
        return SQLITE_MISUSE;
    }
    let rc = with_frame(db, |frame| {
        let Some(frame) = frame else {
            return SQLITE_MISUSE;
        };
        let row = match (want_old, frame.op) {
            (true, turso_core::UpdateHookOp::Insert)
            | (false, turso_core::UpdateHookOp::Delete) => return SQLITE_MISUSE,
            (true, _) => frame.old.as_ref(),
            (false, _) => frame.new.as_ref(),
        };
        if i < 0 {
            return SQLITE_RANGE;
        }
        if i as usize >= frame.column_count {
            return SQLITE_MISUSE;
        }
        let value = row
            .and_then(|r| r.get(i as usize))
            .map(to_ext_value)
            .unwrap_or_else(ExtValue::null);
        let mut values = frame.values.borrow_mut();
        values.push(Box::new(value));
        let last = values.last_mut().expect("just pushed");
        *pp_value = &mut **last as *mut ExtValue as *mut ffi::c_void;
        SQLITE_OK
    });
    if let Ok(mut inner) = (*db).inner.lock() {
        set_db_err_code(&mut inner, rc);
    }
    rc
}

/// Old value of column `i`, in an UPDATE or DELETE preupdate callback.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_preupdate_old(
    db: *mut sqlite3,
    i: ffi::c_int,
    pp_value: *mut *mut ffi::c_void,
) -> ffi::c_int {
    preupdate_value(db, i, pp_value, true)
}

/// New value of column `i`, in an INSERT or UPDATE preupdate callback.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_preupdate_new(
    db: *mut sqlite3,
    i: ffi::c_int,
    pp_value: *mut *mut ffi::c_void,
) -> ffi::c_int {
    preupdate_value(db, i, pp_value, false)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_preupdate_count(db: *mut sqlite3) -> ffi::c_int {
    with_frame(db, |f| f.map_or(0, |f| f.column_count as ffi::c_int))
}

/// 0 for a change made by the statement itself, N inside the Nth trigger level.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_preupdate_depth(db: *mut sqlite3) -> ffi::c_int {
    with_frame(db, |f| f.map_or(0, |f| f.depth))
}

/// Column written by sqlite3_blob_write, or -1.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_preupdate_blobwrite(db: *mut sqlite3) -> ffi::c_int {
    with_frame(db, |f| f.map_or(-1, |f| f.blob_write))
}

/// Callback type for sqlite3_set_authorizer: (userData, actionCode, arg1,
/// arg2, database, trigger-or-view) -> SQLITE_OK / SQLITE_DENY / SQLITE_IGNORE.
pub type sqlite3_authorizer_callback = unsafe extern "C" fn(
    *mut ffi::c_void,
    ffi::c_int,
    *const ffi::c_char,
    *const ffi::c_char,
    *const ffi::c_char,
    *const ffi::c_char,
) -> ffi::c_int;

/// Consulted while statements are prepared. Prepared statements are
/// re-prepared on their next step, so the new authorizer sees them.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_set_authorizer(
    db: *mut sqlite3,
    callback: Option<sqlite3_authorizer_callback>,
    arg: *mut ffi::c_void,
) -> ffi::c_int {
    let Some(conn) = db_conn(db) else {
        return SQLITE_MISUSE;
    };
    let arg = arg as usize;
    conn.set_authorizer(callback.map(|cb| {
        Arc::new(
            move |action: i32,
                  a1: Option<&str>,
                  a2: Option<&str>,
                  a3: Option<&str>,
                  a4: Option<&str>| {
                let [a1, a2, a3, a4] = [a1, a2, a3, a4].map(|s| s.map(c_name));
                let ptr = |s: &Option<CString>| s.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
                unsafe {
                    cb(
                        arg as *mut ffi::c_void,
                        action,
                        ptr(&a1),
                        ptr(&a2),
                        ptr(&a3),
                        ptr(&a4),
                    )
                }
            },
        ) as turso_core::authorizer::AuthorizerCallback
    }));
    SQLITE_OK
}
