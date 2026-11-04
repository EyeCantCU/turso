//! Loadable extensions: `sqlite3_auto_extension`, `sqlite3_load_extension`
//! and the `sqlite3_api_routines` table passed to extension entry points.

use super::*;
use std::collections::HashSet;

/// Table of API function pointers, in `struct sqlite3_api_routines` order
/// (sqlite3ext.h). Slots for functions this library lacks are NULL.
#[repr(transparent)]
pub struct ApiRoutines(pub [*const (); API_ROUTINES_LEN]);
// SAFETY: immutable table of function addresses.
unsafe impl Sync for ApiRoutines {}

include!(concat!(env!("OUT_DIR"), "/api_routines.rs"));

const SQLITE_OK_LOAD_PERMANENTLY: ffi::c_int = SQLITE_OK | (1 << 8);

type ExtEntry =
    unsafe extern "C" fn(*mut sqlite3, *mut *mut ffi::c_char, *const ApiRoutines) -> ffi::c_int;

/// Registered auto-extension entry points (function addresses).
static AUTO_EXTENSIONS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Connections with C-API extension loading enabled.
static LOAD_ENABLED: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

pub(crate) fn load_extension_enabled(db: *mut sqlite3) -> bool {
    LOAD_ENABLED
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.contains(&(db as usize))))
        .unwrap_or(false)
}

pub(crate) fn set_load_extension_enabled(db: *mut sqlite3, on: bool) {
    if let Ok(mut g) = LOAD_ENABLED.lock() {
        let set = g.get_or_insert_with(HashSet::new);
        if on {
            set.insert(db as usize);
        } else {
            set.remove(&(db as usize));
        }
    }
}

pub(crate) fn forget_connection(db: *mut sqlite3) {
    set_load_extension_enabled(db, false);
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_auto_extension(
    x_entry_point: Option<unsafe extern "C" fn()>,
) -> ffi::c_int {
    let Some(f) = x_entry_point else {
        return SQLITE_MISUSE;
    };
    let rc = sqlite3_initialize();
    if rc != SQLITE_OK {
        return rc;
    }
    let Ok(mut list) = AUTO_EXTENSIONS.lock() else {
        return SQLITE_MISUSE;
    };
    if !list.contains(&(f as usize)) {
        list.push(f as usize);
    }
    SQLITE_OK
}

/// Returns 1 if the entry point was registered, else 0.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_cancel_auto_extension(
    x_entry_point: Option<unsafe extern "C" fn()>,
) -> ffi::c_int {
    let Some(f) = x_entry_point else {
        return 0;
    };
    let Ok(mut list) = AUTO_EXTENSIONS.lock() else {
        return 0;
    };
    match list.iter().position(|&p| p == f as usize) {
        Some(i) => {
            list.remove(i);
            1
        }
        None => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_reset_auto_extension() {
    if let Ok(mut list) = AUTO_EXTENSIONS.lock() {
        list.clear();
    }
}

/// Runs every auto-extension on a freshly opened connection. On failure the
/// handle is left in the "sick" state with the error recorded, as SQLite does;
/// the caller still owns it and must close it.
pub(crate) unsafe fn run_auto_extensions(db: *mut sqlite3) -> ffi::c_int {
    let mut i = 0;
    loop {
        // Re-read the list each step: an entry point may (un)register others.
        let entry = match AUTO_EXTENSIONS.lock() {
            Ok(list) => match list.get(i) {
                Some(&p) => p,
                None => return SQLITE_OK,
            },
            Err(_) => return SQLITE_OK,
        };
        i += 1;
        let mut err: *mut ffi::c_char = std::ptr::null_mut();
        // SAFETY: only addresses of C functions passed to sqlite3_auto_extension.
        let f: ExtEntry = std::mem::transmute::<usize, ExtEntry>(entry);
        let rc = f(db, &mut err, &API_ROUTINES);
        if rc != SQLITE_OK {
            let detail = if err.is_null() {
                String::new()
            } else {
                CStr::from_ptr(err).to_string_lossy().into_owned()
            };
            sqlite3_free(err.cast());
            let mut inner = (*db).inner.lock().unwrap();
            set_db_err_msg(
                &mut inner,
                rc,
                &format!("automatic extension loading failed: {detail}"),
            );
            inner.e_open_state = SQLITE_STATE_SICK;
            return rc;
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_enable_load_extension(
    db: *mut sqlite3,
    onoff: ffi::c_int,
) -> ffi::c_int {
    if db.is_null() {
        return SQLITE_MISUSE;
    }
    set_load_extension_enabled(db, onoff != 0);
    SQLITE_OK
}

unsafe fn set_err(out: *mut *mut ffi::c_char, msg: &str) {
    if out.is_null() {
        return;
    }
    let c = CString::new(msg.replace('\0', "")).unwrap_or_default();
    let n = c.as_bytes_with_nul().len();
    let p = libc::malloc(n) as *mut ffi::c_char;
    if !p.is_null() {
        std::ptr::copy_nonoverlapping(c.as_ptr(), p, n);
    }
    *out = p;
}

/// Entry point name SQLite derives from the file name:
/// `sqlite3_<letters of basename without "lib" prefix, up to '.'>_init`.
#[cfg(unix)]
fn derived_entry_name(file: &str) -> String {
    let base = file.rsplit('/').next().unwrap_or(file);
    let base = if base.len() >= 3 && base[..3].eq_ignore_ascii_case("lib") {
        &base[3..]
    } else {
        base
    };
    let stem: String = base
        .chars()
        .take_while(|&c| c != '.')
        .filter(char::is_ascii_alphabetic)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    format!("sqlite3_{stem}_init")
}

#[cfg(unix)]
unsafe fn dl_error() -> String {
    let e = libc::dlerror();
    if e.is_null() {
        String::new()
    } else {
        CStr::from_ptr(e).to_string_lossy().into_owned()
    }
}

#[cfg(not(unix))]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_load_extension(
    db: *mut sqlite3,
    z_file: *const ffi::c_char,
    _z_proc: *const ffi::c_char,
    pz_err_msg: *mut *mut ffi::c_char,
) -> ffi::c_int {
    if db.is_null() || z_file.is_null() {
        return SQLITE_MISUSE;
    }
    set_err(
        pz_err_msg,
        "extension loading is not supported on this platform",
    );
    SQLITE_ERROR
}

#[cfg(unix)]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_load_extension(
    db: *mut sqlite3,
    z_file: *const ffi::c_char,
    z_proc: *const ffi::c_char,
    pz_err_msg: *mut *mut ffi::c_char,
) -> ffi::c_int {
    if db.is_null() || z_file.is_null() {
        return SQLITE_MISUSE;
    }
    if !pz_err_msg.is_null() {
        *pz_err_msg = std::ptr::null_mut();
    }
    if !load_extension_enabled(db) {
        set_err(pz_err_msg, "not authorized");
        return SQLITE_ERROR;
    }
    let file = CStr::from_ptr(z_file).to_string_lossy().into_owned();
    if file.len() > 512 {
        set_err(pz_err_msg, "full pathname is too long");
        return SQLITE_ERROR;
    }
    let open = |name: &str| -> *mut ffi::c_void {
        match CString::new(name) {
            Ok(c) => libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL),
            Err(_) => std::ptr::null_mut(),
        }
    };
    let mut handle = open(&file);
    if handle.is_null() {
        let first_err = dl_error();
        // SQLite retries with the platform shared-library suffix appended.
        handle = open(&format!("{file}.so"));
        if handle.is_null() {
            dl_error();
            set_err(pz_err_msg, &first_err);
            return SQLITE_ERROR;
        }
    }
    let lookup = |name: &str| -> *mut ffi::c_void {
        match CString::new(name) {
            Ok(c) => libc::dlsym(handle, c.as_ptr()),
            Err(_) => std::ptr::null_mut(),
        }
    };
    let mut entry_name = if z_proc.is_null() {
        "sqlite3_extension_init".to_string()
    } else {
        CStr::from_ptr(z_proc).to_string_lossy().into_owned()
    };
    let mut entry = lookup(&entry_name);
    if entry.is_null() && z_proc.is_null() {
        entry_name = derived_entry_name(&file);
        entry = lookup(&entry_name);
    }
    if entry.is_null() {
        // As SQLite, dlerror() text replaces the generic message when present.
        let detail = dl_error();
        let msg = if detail.is_empty() {
            format!("no entry point [{entry_name}] in shared library [{file}]")
        } else {
            detail
        };
        set_err(pz_err_msg, &msg);
        libc::dlclose(handle);
        return SQLITE_ERROR;
    }
    let mut err: *mut ffi::c_char = std::ptr::null_mut();
    // SAFETY: the symbol is documented to have the extension entry signature.
    let f: ExtEntry = std::mem::transmute::<*mut ffi::c_void, ExtEntry>(entry);
    let rc = f(db, &mut err, &API_ROUTINES);
    if rc == SQLITE_OK || rc == SQLITE_OK_LOAD_PERMANENTLY {
        sqlite3_free(err.cast());
        // Never dlclose: the extension may have registered callbacks.
        return SQLITE_OK;
    }
    let detail = if err.is_null() {
        String::new()
    } else {
        CStr::from_ptr(err).to_string_lossy().into_owned()
    };
    sqlite3_free(err.cast());
    set_err(
        pz_err_msg,
        &format!("error during initialization: {detail}"),
    );
    libc::dlclose(handle);
    SQLITE_ERROR
}
