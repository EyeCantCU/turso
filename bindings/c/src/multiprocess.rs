//! Multi-process access for file databases, on by default like SQLite.

use std::sync::Arc;
use turso_core::{Database, DatabaseOpts, LimboError, OpenFlags, SqliteDialect, IO};

/// Open a file database shared with other processes through the WAL
/// coordination file. Falls back to the exclusive per-process lock where
/// multiprocess WAL is unavailable (network filesystems, MVCC databases).
pub(crate) fn open_file_db(
    io: Arc<dyn IO>,
    path: &str,
    flags: OpenFlags,
    opts: DatabaseOpts,
) -> turso_core::Result<Arc<Database>> {
    if Database::multiprocess_wal_supported(&io, path) {
        match Database::open_file_with_flags(
            io.clone(),
            path,
            flags,
            opts.with_multiprocess_wal(true),
            None,
            Arc::new(SqliteDialect),
        ) {
            Err(LimboError::InvalidArgument(msg)) if msg.contains("multiprocess") => {}
            // A new file at the path of a renamed, still-open database: its
            // coordination file belongs to the old one, so open unshared.
            Err(LimboError::InternalError(msg)) if msg.contains("duplicate same-process") => {}
            res => return res,
        }
    }
    Database::open_file_with_flags(io, path, flags, opts, None, Arc::new(SqliteDialect))
}

/// Whether an open failed because another process holds the database.
pub(crate) fn is_locked(err: &LimboError) -> bool {
    matches!(err, LimboError::LockingError(_) | LimboError::Busy)
}

/// A handle reporting SQLITE_BUSY "database is locked" for an open that
/// another process blocks. It only serves sqlite3_errmsg and sqlite3_close.
pub(crate) fn locked_handle(filename: &str) -> Option<*mut crate::sqlite3> {
    let io: Arc<dyn IO> = Arc::new(turso_core::MemoryIO::new());
    let db = Database::open_file_with_flags(
        io.clone(),
        ":memory:",
        OpenFlags::default(),
        DatabaseOpts::new(),
        None,
        Arc::new(SqliteDialect),
    )
    .ok()?;
    let conn = db.connect(turso_core::ConnectOpts::new()).ok()?;
    let filename = std::ffi::CString::new(filename).ok()?;
    let handle = crate::sqlite3::new(io, db, conn, filename);
    {
        let mut inner = handle.inner.lock().unwrap();
        inner.err_code = crate::SQLITE_BUSY;
        inner.p_err = std::ffi::CString::new("database is locked").ok();
        inner.e_open_state = crate::SQLITE_STATE_SICK;
    }
    Some(handle.into_raw())
}
