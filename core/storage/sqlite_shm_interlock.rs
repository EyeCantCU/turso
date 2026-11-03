//! Keeps real SQLite out of a database that turso shares across processes.
//!
//! Turso coordinates its WAL through `-tshm`, which SQLite does not read, so
//! a SQLite process appending to the same WAL would corrupt it. While a turso
//! process has the database open it holds shared locks on SQLite's lock
//! bytes, which makes every SQLite access fail with SQLITE_BUSY (or
//! SQLITE_PROTOCOL) instead:
//!
//! - `-shm` DMS byte (128): SQLite sees a live peer and does not reset the
//!   wal-index, so it finds the header uninitialised and needs recovery;
//! - `-shm` WAL_WRITE_LOCK (120) and WAL_CKPT_LOCK (121): recovery, writes
//!   and checkpoints need these exclusively;
//! - `-shm` WAL_RECOVER_LOCK (122), held exclusively by one turso process
//!   when free, so SQLite reports SQLITE_BUSY_RECOVERY at once instead of
//!   retrying for seconds;
//! - database SHARED range (first byte at PENDING_BYTE + 2): SQLite's
//!   EXCLUSIVE lock (exclusive locking mode, rollback journal) needs it.
//!
//! Conversely, a turso process that finds no other turso process but a held
//! DMS lock knows SQLite has the database open and reports it busy.
//!
//! Locks are Linux open-file-description locks: they conflict with SQLite's
//! POSIX locks, are released when the descriptor closes, and are not wiped
//! when turso opens and closes other descriptors of the same file.

use crate::io::{File, SharedWalLockKind, IO};
use crate::sync::Arc;
use crate::{LimboError, Result};

const SHM_WRITE_LOCK: u64 = 120;
const SHM_CKPT_LOCK: u64 = 121;
const SHM_RECOVER_LOCK: u64 = 122;
const SHM_DMS: u64 = 128;
/// SQLite's wal-index header lives in the first 32 KiB page of `-shm`.
const SHM_PAGE_SIZE: u64 = 32768;
const DB_SHARED_FIRST: u64 = 0x4000_0000 + 2;
const KIND: SharedWalLockKind = SharedWalLockKind::LinuxOfd;

pub(crate) struct SqliteShmInterlock {
    _shm: Arc<dyn File>,
    _db: Arc<dyn File>,
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
        let db = io.open_shared_wal_file(db_path)?;
        for attempt in 1..=ATTEMPTS {
            if Self::try_lock(&shm, &db, &turso_peers_live)? {
                if shm.shared_wal_try_lock_byte(SHM_RECOVER_LOCK, true, KIND)?
                    && shm.size()? < SHM_PAGE_SIZE
                {
                    // A mapped, zeroed header page sends SQLite to the recover lock.
                    shm.shared_wal_set_len(SHM_PAGE_SIZE)?;
                }
                return Ok(Self { _shm: shm, _db: db });
            }
            if attempt < ATTEMPTS {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        Err(LimboError::LockingError(format!(
            "Failed opening database '{db_path}'. Database is open in a SQLite process"
        )))
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

    /// Take the shared locks; false on a conflict, with nothing left held.
    fn try_lock(
        shm: &Arc<dyn File>,
        db: &Arc<dyn File>,
        turso_peers_live: &impl Fn() -> bool,
    ) -> Result<bool> {
        // Any lock on DMS without a turso peer belongs to SQLite.
        if !shm.shared_wal_probe_exclusive_byte(SHM_DMS, KIND)? && !turso_peers_live() {
            return Ok(false);
        }
        let mut held = Vec::with_capacity(3);
        for offset in [SHM_DMS, SHM_WRITE_LOCK, SHM_CKPT_LOCK] {
            if !shm.shared_wal_try_lock_byte(offset, false, KIND)? {
                for offset in held {
                    shm.shared_wal_unlock_byte(offset, KIND)?;
                }
                return Ok(false);
            }
            held.push(offset);
        }
        if !db.shared_wal_try_lock_byte(DB_SHARED_FIRST, false, KIND)? {
            for offset in held {
                shm.shared_wal_unlock_byte(offset, KIND)?;
            }
            return Ok(false);
        }
        Ok(true)
    }
}
