//! Open-failure helpers: SQLite returns a handle carrying the error.
use super::*;

/// Return a SICK handle (private in-memory db) carrying `code`/`msg`, as
/// SQLite does for a failed open, so errmsg() works and close() frees it.
pub(crate) unsafe fn open_failed_with(db_out: *mut *mut sqlite3, code: i32, msg: &str) -> i32 {
    let io: Arc<dyn turso_core::IO> = Arc::new(turso_core::MemoryIO::new());
    let built = turso_core::Database::open_file_with_flags(
        io.clone(),
        ":memory:",
        turso_core::OpenFlags::default(),
        DatabaseOpts::new(),
        None,
        Arc::new(SqliteDialect),
    )
    .and_then(|db| db.connect(ConnectOpts::new()).map(|conn| (db, conn)));
    if let Ok((db, conn)) = built {
        let handle = sqlite3::new(io, db, conn, CString::default());
        {
            let mut inner = handle.inner.lock().unwrap();
            inner.e_open_state = SQLITE_STATE_SICK;
            set_db_err_msg(&mut inner, code, msg);
        }
        *db_out = handle.into_raw();
    }
    code
}

/// Failed open with the standard message for `code`.
pub(crate) unsafe fn open_failed(db_out: *mut *mut sqlite3, code: i32) -> i32 {
    let msg = CStr::from_ptr(sqlite3_errstr_impl(code))
        .to_string_lossy()
        .into_owned();
    open_failed_with(db_out, code, &msg)
}

/// Failed open caused by a core error: lock contention is BUSY, anything
/// else CANTOPEN.
pub(crate) unsafe fn open_failed_err(db_out: *mut *mut sqlite3, err: &LimboError) -> i32 {
    trace!("open failed: {err}");
    let code = match err {
        LimboError::Busy | LimboError::LockingError(_) => SQLITE_BUSY,
        LimboError::NotADB => SQLITE_NOTADB,
        LimboError::Corrupt(..) => SQLITE_CORRUPT,
        _ => SQLITE_CANTOPEN,
    };
    open_failed(db_out, code)
}

/// Apply URI `mode=ro|rw|rwc` to the open flags as SQLite does. Err holds
/// the message for SQLITE_PERM when the mode exceeds what `flags` allow.
pub(crate) fn effective_flags(flags: ffi::c_int, filename: &str) -> Result<ffi::c_int, String> {
    let mask = SQLITE_OPEN_READONLY | SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE;
    let mut flags = flags;
    if (flags & SQLITE_OPEN_URI) != 0 && filename.starts_with("file:") {
        if let Some(q) = filename.split('#').next().and_then(|f| f.split_once('?')) {
            for p in q.1.split('&') {
                let (name, mode) = match p {
                    "mode=ro" => ("ro", SQLITE_OPEN_READONLY),
                    "mode=rw" => ("rw", SQLITE_OPEN_READWRITE),
                    "mode=rwc" => ("rwc", SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE),
                    _ => continue,
                };
                if mode > (flags & mask) {
                    return Err(format!("access mode not allowed: {name}"));
                }
                flags = (flags & !mask) | mode;
            }
        }
    }
    Ok(flags)
}
