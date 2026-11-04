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
//! Like SQLite's wal-index, the interlock is set up at the first transaction,
//! not at open. A connection in EXCLUSIVE locking mode at that point locks the
//! database exclusively through [`DbLock`] instead and creates no `-shm`,
//! like SQLite's heap-memory wal-index.
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

/// A database's interlock, shared with its WAL coordinations.
pub(crate) type SqliteInterlockCell = Arc<SqliteInterlockSlot>;

/// The live shared WAL state of the database (it is replaced at open).
pub(crate) type SharedWalHandle = Arc<crate::sync::RwLock<crate::storage::wal::WalFileShared>>;

type InterlockInit =
    Box<dyn Fn(Option<SharedWalHandle>) -> Result<SqliteShmInterlock> + Send + Sync>;

/// Holds the interlock once it is set up: by the first transaction not in
/// EXCLUSIVE locking mode, or by any WAL transaction while no EXCLUSIVE hold
/// stands in for it, so every commit reaches SQLite's wal-index.
#[derive(Default)]
pub(crate) struct SqliteInterlockSlot {
    cell: std::sync::OnceLock<Arc<SqliteShmInterlock>>,
    init: std::sync::OnceLock<InterlockInit>,
    setup: parking_lot::Mutex<()>,
    /// An exclusive [`DbLock`] replaces the interlock while set.
    exclusive_without_shm: AtomicBool,
    /// Set by the WAL coordinations; passed to the setup.
    shared_wal: parking_lot::Mutex<Option<SharedWalHandle>>,
}

impl SqliteInterlockSlot {
    pub(crate) fn get(&self) -> Option<&Arc<SqliteShmInterlock>> {
        self.cell.get()
    }

    /// Whether the database can set the interlock up (shares the WAL with
    /// SQLite), whether or not it has yet.
    pub(crate) fn is_installed(&self) -> bool {
        self.init.get().is_some()
    }

    /// Install how to set the interlock up; until then `ensure` does nothing.
    pub(crate) fn set_shared_wal(&self, shared: SharedWalHandle) {
        *self.shared_wal.lock() = Some(shared);
    }

    pub(crate) fn install(&self, init: InterlockInit) {
        let _ = self.init.set(init);
    }

    pub(crate) fn set_exclusive_without_shm(&self, on: bool) {
        self.exclusive_without_shm.store(on, Ordering::Release);
    }

    /// Set the interlock up if it is installed, missing and not replaced.
    pub(crate) fn ensure(&self) -> Result<()> {
        if self.cell.get().is_some() || self.exclusive_without_shm.load(Ordering::Acquire) {
            return Ok(());
        }
        let Some(init) = self.init.get() else {
            return Ok(());
        };
        let _setup = self.setup.lock();
        if self.cell.get().is_none() {
            let shared = self.shared_wal.lock().clone();
            let _ = self.cell.set(Arc::new(init(shared)?));
        }
        Ok(())
    }
}

impl std::fmt::Debug for SqliteInterlockSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteInterlockSlot")
            .finish_non_exhaustive()
    }
}

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
    /// Take the interlock for `db_path`. A process that finds nobody holding
    /// DMS builds the index with `build` while it holds DMS exclusively, as
    /// SQLite resets its wal-index, so no process joins a stale index.
    /// `db` is the database's [`DbLock`] descriptor, already holding the
    /// shared SHARED-byte lock.
    pub(crate) fn acquire(
        io: &Arc<dyn IO>,
        db_path: &str,
        db: Arc<dyn File>,
        wal: Option<Arc<dyn File>>,
        build: impl Fn(&SqliteWalIndex) -> Result<()>,
    ) -> Result<Self> {
        let shm = io.open_shared_wal_file(&format!("{db_path}-shm"))?;
        let wal_source = crate::storage::sqlite_wal_index::WalSource {
            path: format!("{db_path}-wal"),
            shared: wal.map(|file| (file, io.clone())),
        };
        let wal_path = &wal_source;
        for attempt in 1..=ATTEMPTS {
            if let Some(index) = Self::try_build_alone(&shm, wal_path, &build)? {
                return Ok(Self::new(shm, db, index, true));
            }
            if let Some(others) = Self::try_lock(&shm)? {
                let index = SqliteWalIndex::new(shm.clone(), wal_path)?;
                if !others {
                    // Nobody maintains the index and it was not built above:
                    // stale contents must not be trusted before the rebuild.
                    index.invalidate()?;
                }
                return Ok(Self::new(shm, db, index, others));
            }
            if attempt < ATTEMPTS {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        Err(LimboError::LockingError(format!(
            "Failed opening database '{db_path}'. Database is locked by another process"
        )))
    }

    fn new(shm: Arc<dyn File>, db: Arc<dyn File>, index: SqliteWalIndex, ready: bool) -> Self {
        Self {
            _shm: shm,
            _db: db,
            index,
            index_ready: AtomicBool::new(ready),
            // Never a real iChange right after open: import once.
            seen_change: AtomicU32::new(u32::MAX),
        }
    }

    /// With DMS free, take it exclusively, build the index, then downgrade.
    /// An index a SQLite process left that still matches the WAL file is kept:
    /// it may be newer than this process's view, which imports it.
    fn try_build_alone(
        shm: &Arc<dyn File>,
        wal_path: &crate::storage::sqlite_wal_index::WalSource,
        build: &impl Fn(&SqliteWalIndex) -> Result<()>,
    ) -> Result<Option<SqliteWalIndex>> {
        if !shm.shared_wal_try_lock_byte(SHM_DMS, true, KIND)? {
            return Ok(None);
        }
        let built = SqliteWalIndex::new(shm.clone(), wal_path).and_then(|index| {
            if !index.matches_wal_file() {
                index.invalidate()?;
                build(&index)?;
            }
            Ok(index)
        });
        match built {
            Ok(index) => {
                shm.shared_wal_lock_byte(SHM_DMS, false, KIND)?;
                Ok(Some(index))
            }
            Err(err) => {
                let _ = shm.shared_wal_unlock_byte(SHM_DMS, KIND);
                Err(err)
            }
        }
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

    /// Take DMS shared, or None on a conflict. Some(true) when another
    /// process (turso or SQLite) already holds it.
    fn try_lock(shm: &Arc<dyn File>) -> Result<Option<bool>> {
        let others = !shm.shared_wal_probe_exclusive_byte(SHM_DMS, KIND)?;
        if !shm.shared_wal_try_lock_byte(SHM_DMS, false, KIND)? {
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

/// The database file's SHARED-byte lock, held shared from open on: SQLite
/// then cannot take the database exclusively (its close-time checkpoint that
/// deletes the WAL and `-shm`, exclusive locking mode, rollback journal).
/// EXCLUSIVE locking mode upgrades it.
pub(crate) struct DbLock {
    db: Arc<dyn File>,
}

impl DbLock {
    /// None while another process holds the database exclusively.
    /// `db`: the database's own descriptor, shared (as SQLite's
    /// unixInodeInfo shares one per file) instead of opening another.
    pub(crate) fn acquire(
        io: &Arc<dyn IO>,
        db_path: &str,
        db: Option<Arc<dyn File>>,
    ) -> Result<Option<Self>> {
        // Writable for the exclusive lock; read-only files get a shared one.
        let db = match db {
            Some(db) => db,
            None => io
                .open_file(db_path, crate::io::OpenFlags::NoLock, false)
                .or_else(|_| io.open_file(db_path, crate::io::OpenFlags::ReadOnly, false))?,
        };
        if db.shared_wal_try_lock_byte(DB_SHARED_FIRST, false, KIND)? {
            Ok(Some(Self { db }))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn file(&self) -> Arc<dyn File> {
        self.db.clone()
    }

    /// Lock the database from every other process, for EXCLUSIVE locking
    /// mode; false while another process has it open.
    pub(crate) fn try_exclusive(&self) -> Result<bool> {
        self.db
            .shared_wal_try_lock_byte(DB_SHARED_FIRST, true, KIND)
    }

    pub(crate) fn release_exclusive(&self) -> Result<()> {
        self.db.shared_wal_lock_byte(DB_SHARED_FIRST, false, KIND)
    }
}
