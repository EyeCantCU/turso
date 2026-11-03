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
            res => return res,
        }
    }
    Database::open_file_with_flags(io, path, flags, opts, None, Arc::new(SqliteDialect))
}
