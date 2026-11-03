//! Process-lifetime SQLite locks that let turso and real SQLite share a
//! database, plus SQLite's wal-index ([`SqliteWalIndex`]) that turso keeps
//! current so both read and write through the same WAL.
//!
//! While a turso process has the database open it holds, as OFD locks:
//!
//! - `-shm` DMS byte (128), shared: SQLite sees a live peer and trusts the
//!   wal-index instead of resetting it;
//! - the first byte of the database SHARED range (PENDING_BYTE + 2),
//!   shared: SQLite's close-time checkpoint, exclusive locking mode and
//!   rollback-journal commits need it exclusively. `PRAGMA
//!   locking_mode=EXCLUSIVE` upgrades it.
//!
//! WAL_WRITE_LOCK, WAL_CKPT_LOCK and the read marks are taken per
//! transaction through the index. The first turso process invalidates any
//! stale index on open and rebuilds it; a turso process that finds DMS held
//! with no turso peer alive knows SQLite owns the database, and joins it.
//!
//! OFD locks conflict with SQLite's POSIX locks, are released when the
//! descriptor closes, and are not wiped when turso opens and closes other
//! descriptors of the same file.

use crate::io::{File, SharedWalLockKind, IO};
use crate::storage::sqlite_wal_index::SqliteWalIndex;
use crate::sync::Arc;
use crate::{LimboError, Result};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const SHM_DMS: u64 = 128;
const DB_SHARED_FIRST: u64 = 0x4000_0000 + 2;
const KIND: SharedWalLockKind = SharedWalLockKind::LinuxOfd;

pub(crate) struct SqliteShmInterlock {
    _shm: Arc<dyn File>,
    _db: Arc<dyn File>,
    index: SqliteWalIndex,
    /// Whether this process built (or found peers maintaining) the index.
    index_ready: AtomicBool,
    /// Index iChange this process last imported or published.
    seen_change: AtomicU32,
}

/// Attempts before concluding a lock conflict is not transient: turso
/// processes probing the DMS byte hold it exclusively for microseconds.
const ATTEMPTS: u32 = 100;

impl SqliteShmInterlock {
    /// Take the interlock for `db_path`. `turso_peers_live` says whether
    /// another turso process already has the database open.
    pub(crate) fn acquire(
        io: &Arc<dyn IO>,
        db_path: &str,
        turso_peers_live: impl Fn() -> bool,
    ) -> Result<Self> {
        let shm = io.open_shared_wal_file(&format!("{db_path}-shm"))?;
        // Writable for locking_mode=EXCLUSIVE's write lock; a shared lock
        // needs only read access, for read-only files.
        let db = io
            .open_file(db_path, crate::io::OpenFlags::NoLock, false)
            .or_else(|_| io.open_file(db_path, crate::io::OpenFlags::ReadOnly, false))?;
        for attempt in 1..=ATTEMPTS {
            if let Some(others) = Self::try_lock(&shm, &db)? {
                let index = SqliteWalIndex::new(shm.clone(), &format!("{db_path}-wal"))?;
                // Alone (no turso peer, no SQLite holding DMS): stale contents
                // from an earlier session must not be trusted by a SQLite
                // reader before this process rebuilds the index.
                let alone = !others && !turso_peers_live();
                if alone {
                    index.invalidate()?;
                }
                return Ok(Self {
                    _shm: shm,
                    _db: db,
                    index,
                    index_ready: AtomicBool::new(!alone),
                    // Never a real iChange right after open: import once.
                    seen_change: AtomicU32::new(u32::MAX),
                });
            }
            if attempt < ATTEMPTS {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        Err(LimboError::LockingError(format!(
            "Failed opening database '{db_path}'. Database is locked by another process"
        )))
    }

    pub(crate) fn index(&self) -> &SqliteWalIndex {
        &self.index
    }

    /// Whether `change` is the iChange this process has already caught up to.
    pub(crate) fn change_seen(&self, change: u32) -> bool {
        self.seen_change.load(Ordering::Acquire) == change
    }

    pub(crate) fn note_change(&self, change: u32) {
        self.seen_change.store(change, Ordering::Release);
    }

    /// Claim the one-time index rebuild for this process.
    pub(crate) fn claim_index_rebuild(&self) -> bool {
        !self.index_ready.swap(true, Ordering::AcqRel)
    }

    /// Upgrade the database SHARED-byte lock to exclusive, for
    /// `PRAGMA locking_mode=EXCLUSIVE`. Fails while any other process (turso
    /// or SQLite) has the database open.
    pub(crate) fn try_exclusive(&self) -> Result<bool> {
        self._db
            .shared_wal_try_lock_byte(DB_SHARED_FIRST, true, KIND)
    }

    /// Downgrade back to the shared lock.
    pub(crate) fn release_exclusive(&self) -> Result<()> {
        self._db.shared_wal_lock_byte(DB_SHARED_FIRST, false, KIND)
    }

    /// Take the shared locks, or None on a conflict with nothing left held.
    /// Some(true) when another process (turso or SQLite) already holds DMS.
    fn try_lock(shm: &Arc<dyn File>, db: &Arc<dyn File>) -> Result<Option<bool>> {
        let others = !shm.shared_wal_probe_exclusive_byte(SHM_DMS, KIND)?;
        if !shm.shared_wal_try_lock_byte(SHM_DMS, false, KIND)? {
            return Ok(None);
        }
        if !db.shared_wal_try_lock_byte(DB_SHARED_FIRST, false, KIND)? {
            shm.shared_wal_unlock_byte(SHM_DMS, KIND)?;
            return Ok(None);
        }
        Ok(Some(others))
    }
}

impl std::fmt::Debug for SqliteShmInterlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteShmInterlock").finish_non_exhaustive()
    }
}
