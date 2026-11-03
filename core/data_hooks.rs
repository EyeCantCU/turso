//! Connection-scoped data-change and transaction hooks, modelled on SQLite's
//! `sqlite3_commit_hook()`, `sqlite3_rollback_hook()`, `sqlite3_update_hook()`,
//! `sqlite3_preupdate_hook()` and `sqlite3_wal_hook()`.
//!
//! Callbacks run without any lock of this struct held, so they may replace or
//! clear hooks. As in SQLite, they must not modify the database through the
//! connection that invoked them.

use crate::sync::{
    atomic::{AtomicBool, AtomicI32, Ordering},
    Arc, RwLock,
};
use crate::Value;

/// Kind of row change. The discriminants are SQLite's authorizer codes,
/// which the update and preupdate hooks use as op codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum UpdateHookOp {
    Delete = 9,
    Insert = 18,
    Update = 23,
}

/// A row change reported to the preupdate hook, before it is applied.
pub struct PreUpdateEvent<'a> {
    pub op: UpdateHookOp,
    pub db_name: &'a str,
    pub table: &'a str,
    /// Rowid before the change (equal to `new_rowid` for INSERT).
    pub old_rowid: i64,
    /// Rowid after the change (equal to `old_rowid` for DELETE).
    pub new_rowid: i64,
    /// Old column values, for DELETE and UPDATE.
    pub old: Option<&'a [Value]>,
    /// New column values, for INSERT and UPDATE.
    pub new: Option<&'a [Value]>,
    /// Number of columns in the table.
    pub column_count: usize,
    /// 0 for a change made by the statement itself, N inside the Nth trigger level.
    pub depth: i32,
    /// Column being written by an incremental blob write, else -1.
    pub blob_write: i32,
}

/// Returns `true` to veto the commit (turn it into a rollback).
pub type CommitHookCallback = Arc<dyn Fn() -> bool + Send + Sync>;
pub type RollbackHookCallback = Arc<dyn Fn() + Send + Sync>;
/// `(op, database name, table name, rowid)`.
pub type UpdateHookCallback = Arc<dyn Fn(UpdateHookOp, &str, &str, i64) + Send + Sync>;
pub type PreUpdateHookCallback = Arc<dyn Fn(&PreUpdateEvent<'_>) + Send + Sync>;
/// `(database name, frames in the WAL)`.
pub type WalHookCallback = Arc<dyn Fn(&str, u64) + Send + Sync>;

#[derive(Default)]
pub(crate) struct DataChangeHooks {
    commit: RwLock<Option<CommitHookCallback>>,
    rollback: RwLock<Option<RollbackHookCallback>>,
    update: RwLock<Option<UpdateHookCallback>>,
    preupdate: RwLock<Option<PreUpdateHookCallback>>,
    wal: RwLock<Option<WalHookCallback>>,
    /// Fast-path flags so the row opcodes pay one atomic load without hooks.
    has_update: AtomicBool,
    has_preupdate: AtomicBool,
    /// Set once the commit hook let a commit through, until it finishes.
    commit_in_progress: AtomicBool,
    /// The commit hook is running; statements it runs must not commit.
    in_commit_hook: AtomicBool,
    /// Trigger / FK-action programs currently on the stack.
    depth: AtomicI32,
    /// WAL databases written by the commit in progress, for the WAL hook.
    pending_wal: crate::sync::Mutex<Vec<(String, Arc<crate::storage::pager::Pager>)>>,
}

impl std::fmt::Debug for DataChangeHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataChangeHooks")
            .field("commit", &self.commit.read().is_some())
            .field("rollback", &self.rollback.read().is_some())
            .field("update", &self.update.read().is_some())
            .field("preupdate", &self.preupdate.read().is_some())
            .field("wal", &self.wal.read().is_some())
            .finish()
    }
}

impl DataChangeHooks {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set_commit(&self, cb: Option<CommitHookCallback>) {
        *self.commit.write() = cb;
    }

    pub(crate) fn set_rollback(&self, cb: Option<RollbackHookCallback>) {
        *self.rollback.write() = cb;
    }

    pub(crate) fn set_update(&self, cb: Option<UpdateHookCallback>) {
        let mut slot = self.update.write();
        self.has_update.store(cb.is_some(), Ordering::SeqCst);
        *slot = cb;
    }

    pub(crate) fn set_preupdate(&self, cb: Option<PreUpdateHookCallback>) {
        let mut slot = self.preupdate.write();
        self.has_preupdate.store(cb.is_some(), Ordering::SeqCst);
        *slot = cb;
    }

    pub(crate) fn set_wal(&self, cb: Option<WalHookCallback>) {
        *self.wal.write() = cb;
    }

    /// True if an update or preupdate hook is installed.
    #[inline]
    pub(crate) fn has_row_hook(&self) -> bool {
        self.has_update_hook() || self.has_preupdate_hook()
    }

    #[inline]
    pub(crate) fn has_update_hook(&self) -> bool {
        self.has_update.load(Ordering::Relaxed)
    }

    #[inline]
    pub(crate) fn has_preupdate_hook(&self) -> bool {
        self.has_preupdate.load(Ordering::Relaxed)
    }

    /// Run the commit hook. Returns `true` if the commit must become a rollback.
    pub(crate) fn before_commit(&self) -> bool {
        let cb = self.commit.read().clone();
        self.in_commit_hook.store(true, Ordering::SeqCst);
        let veto = cb.is_some_and(|cb| cb());
        self.in_commit_hook.store(false, Ordering::SeqCst);
        self.commit_in_progress.store(!veto, Ordering::SeqCst);
        veto
    }

    pub(crate) fn in_commit_hook(&self) -> bool {
        self.in_commit_hook.load(Ordering::SeqCst)
    }

    /// True once per commit that `before_commit` let through.
    pub(crate) fn take_commit_in_progress(&self) -> bool {
        self.commit_in_progress.swap(false, Ordering::SeqCst)
    }

    pub(crate) fn set_pending_wal(&self, dbs: Vec<(String, Arc<crate::storage::pager::Pager>)>) {
        *self.pending_wal.lock() = dbs;
    }

    pub(crate) fn take_pending_wal(&self) -> Vec<(String, Arc<crate::storage::pager::Pager>)> {
        std::mem::take(&mut *self.pending_wal.lock())
    }

    pub(crate) fn on_rollback(&self) {
        self.commit_in_progress.store(false, Ordering::SeqCst);
        self.pending_wal.lock().clear();
        let cb = self.rollback.read().clone();
        if let Some(cb) = cb {
            cb();
        }
    }

    pub(crate) fn on_row_change(&self, op: UpdateHookOp, db: &str, table: &str, rowid: i64) {
        let cb = self.update.read().clone();
        if let Some(cb) = cb {
            cb(op, db, table, rowid);
        }
    }

    pub(crate) fn on_preupdate(&self, event: &PreUpdateEvent<'_>) {
        let cb = self.preupdate.read().clone();
        if let Some(cb) = cb {
            cb(event);
        }
    }

    pub(crate) fn has_wal_hook(&self) -> bool {
        self.wal.read().is_some()
    }

    pub(crate) fn on_wal_commit(&self, db: &str, frames: u64) {
        let cb = self.wal.read().clone();
        if let Some(cb) = cb {
            cb(db, frames);
        }
    }

    pub(crate) fn depth(&self) -> i32 {
        self.depth.load(Ordering::Relaxed)
    }

    pub(crate) fn enter_subprogram(&self) {
        self.depth.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn leave_subprogram(&self) {
        self.depth.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Whether row changes to `table` are reported. SQLite skips its internal
/// `sqlite_*` tables; Turso's internal tables are skipped likewise.
pub(crate) fn hook_reports_table(table: &str) -> bool {
    !table.is_empty()
        && !table
            .get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("sqlite_"))
        && !table.starts_with(crate::schema::TURSO_INTERNAL_PREFIX)
}

impl crate::Connection {
    /// Name of database `index` ("main", "temp" or the attached name).
    pub(crate) fn database_name_of(&self, index: usize) -> Option<String> {
        self.qualifiers.of_database(index).map(|q| q.name)
    }

    /// Before a commit: whether any database (main, temp or attached) is in a
    /// write transaction, and the WAL databases whose commit the WAL hook
    /// reports afterwards.
    pub(crate) fn hook_write_targets(
        &self,
        main_writes: bool,
        main_pager: &Arc<crate::storage::pager::Pager>,
    ) -> (bool, Vec<(String, Arc<crate::storage::pager::Pager>)>) {
        let want_wal = self.data_hooks.has_wal_hook();
        let mut wal_dbs = Vec::new();
        if main_writes && want_wal && !self.db.is_in_memory_db() && self.mv_store().is_none() {
            wal_dbs.push(("main".to_string(), main_pager.clone()));
        }
        let mut any = main_writes;
        self.with_all_attached_pagers_with_index(|pagers| {
            for (index, pager) in pagers {
                if !pager.holds_write_lock() {
                    continue;
                }
                any = true;
                if !want_wal || *index == crate::TEMP_DB_ID {
                    continue;
                }
                let in_memory = self
                    .attached_databases
                    .read()
                    .index_to_data
                    .get(index)
                    .is_none_or(|entry| entry.db.is_in_memory_db());
                if !in_memory {
                    if let Some(name) = self.database_name_of(*index) {
                        wal_dbs.push((name, pager.clone()));
                    }
                }
            }
        });
        (any, wal_dbs)
    }
}
