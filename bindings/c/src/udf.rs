//! Application-defined SQL functions (`sqlite3_create_function*`).
//!
//! Every registration owns a `UdfReg` that carries its callbacks and pApp, and
//! is handed to the engine as the function's context. There is no fixed limit
//! on registrations, and they are per connection.
use super::*;
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use turso_core::user_function::SQLITE_FUNC_UTF8;

pub(crate) type XFunc = unsafe extern "C" fn(*mut ffi::c_void, ffi::c_int, *mut *mut ffi::c_void);
pub(crate) type XFinal = unsafe extern "C" fn(*mut ffi::c_void);
type XDestroy = unsafe extern "C" fn(*mut ffi::c_void);

const SQLITE_UTF16LE: ffi::c_int = 2;
const SQLITE_UTF16BE: ffi::c_int = 3;
const SQLITE_UTF16: ffi::c_int = 4;
const SQLITE_ANY: ffi::c_int = 5;
const FLAG_MASK: ffi::c_int = 0x0008_0000 // DIRECTONLY
    | 0x0000_0800 // DETERMINISTIC
    | 0x0010_0000 // SUBTYPE
    | 0x0020_0000 // INNOCUOUS
    | 0x0100_0000 // RESULT_SUBTYPE
    | 0x0200_0000; // SELFORDER1
const SQLITE_DETERMINISTIC: u32 = 0x800;
const SQLITE_MAX_FUNCTION_ARG: ffi::c_int = 1000;
const SQLITE_MAX_FUNCTION_NAME: usize = 255;

pub(crate) struct UdfReg {
    pub(crate) x_func: Option<XFunc>,
    pub(crate) x_step: Option<XFunc>,
    pub(crate) x_final: Option<XFinal>,
    pub(crate) x_value: Option<XFinal>,
    pub(crate) x_inverse: Option<XFunc>,
    pub(crate) p_app: usize,
    pub(crate) db: usize,
    destroy: Mutex<Option<XDestroy>>,
    /// Registry entries (one per name/nArg/encoding) that use this registration.
    uses: AtomicUsize,
    dead: AtomicBool,
}

impl UdfReg {
    /// Runs xDestroy once; later calls into the registration fail.
    fn destroy_now(&self) {
        self.dead.store(true, Ordering::Release);
        let destroy = self.destroy.lock().unwrap().take();
        if let Some(destroy) = destroy {
            unsafe { destroy(self.p_app as *mut ffi::c_void) };
        }
    }
}

type Key = (String, ffi::c_int, u8);
type DbRegistry = Mutex<HashMap<usize, HashMap<Key, Arc<UdfReg>>>>;

/// Live registrations by connection (address), for replacement and close.
static REGISTRY: OnceLock<DbRegistry> = OnceLock::new();

fn registry() -> &'static DbRegistry {
    REGISTRY.get_or_init(Default::default)
}

/// Drops one registry entry; xDestroy runs when its registration has none left.
fn unref(reg: &UdfReg) {
    if reg.uses.fetch_sub(1, Ordering::AcqRel) == 1 {
        reg.destroy_now();
    }
}

/// Connection close: xDestroy for every function.
pub(crate) fn release_db(db: *mut sqlite3) {
    let entries = registry().lock().unwrap().remove(&(db as usize));
    for reg in entries.into_iter().flat_map(|e| e.into_values()) {
        unref(&reg);
    }
}

/// The engine drops its handle on the registration when no statement uses it.
unsafe extern "C" fn release_context(context: usize) {
    drop(Arc::from_raw(context as *const UdfReg));
}

unsafe fn arg_ptrs(argc: i32, argv: *const ExtValue) -> Vec<*mut ffi::c_void> {
    (0..argc.max(0) as usize)
        .map(|i| argv.add(i) as *mut ffi::c_void)
        .collect()
}

unsafe extern "C" fn scalar_bridge(
    context: usize,
    argc: i32,
    argv: *const ExtValue,
    _context_destructor: Option<turso_ext::ContextDestructor>,
    _value_destructor: Option<turso_ext::ValueDestructor>,
) -> ExtValue {
    let reg = &*(context as *const UdfReg);
    let Some(x_func) = reg.x_func.filter(|_| !reg.dead.load(Ordering::Acquire)) else {
        return ExtValue::error_with_message("user function was deleted".into());
    };
    let mut ptrs = arg_ptrs(argc, argv);
    let mut ctx = SqliteContext {
        result: ExtValue::null(),
        p_app: reg.p_app as *mut ffi::c_void,
        db: reg.db as *mut sqlite3,
    };
    let _scope = crate::value_conv::UdfScope::enter();
    x_func(
        &mut ctx as *mut SqliteContext as *mut ffi::c_void,
        argc,
        ptrs.as_mut_ptr(),
    );
    ctx.result
}

fn is_busy(db: *mut sqlite3) -> bool {
    let inner = unsafe { (*db).inner.lock().unwrap() };
    let mut stmt = inner.stmt_list;
    while !stmt.is_null() {
        let s = unsafe { &*stmt };
        if s.stmt.is_busy() {
            return true;
        }
        stmt = s.next;
    }
    false
}

#[allow(clippy::too_many_arguments)]
unsafe fn create_function(
    db: *mut sqlite3,
    name: &[u8],
    n_args: ffi::c_int,
    enc: ffi::c_int,
    p_app: *mut ffi::c_void,
    x_func: Option<XFunc>,
    x_step: Option<XFunc>,
    x_final: Option<XFinal>,
    x_value: Option<XFinal>,
    x_inverse: Option<XFunc>,
    destroy: Option<XDestroy>,
) -> ffi::c_int {
    // SQLite runs xDestroy for a registration that never takes hold.
    let attached = AtomicBool::new(false);
    let rc = register(
        db, name, n_args, enc, p_app, x_func, x_step, x_final, x_value, x_inverse, destroy,
        &attached,
    );
    if !attached.load(Ordering::Acquire) {
        if let Some(destroy) = destroy {
            destroy(p_app);
        }
    }
    rc
}

#[allow(clippy::too_many_arguments)]
unsafe fn register(
    db: *mut sqlite3,
    name: &[u8],
    n_args: ffi::c_int,
    enc: ffi::c_int,
    p_app: *mut ffi::c_void,
    x_func: Option<XFunc>,
    x_step: Option<XFunc>,
    x_final: Option<XFinal>,
    x_value: Option<XFinal>,
    x_inverse: Option<XFunc>,
    destroy: Option<XDestroy>,
    attached: &AtomicBool,
) -> ffi::c_int {
    if (x_func.is_some() && x_final.is_some())
        || x_final.is_some() != x_step.is_some()
        || x_value.is_some() != x_inverse.is_some()
        || !(-1..=SQLITE_MAX_FUNCTION_ARG).contains(&n_args)
        || name.len() > SQLITE_MAX_FUNCTION_NAME
    {
        return SQLITE_MISUSE;
    }
    let flags = (enc & FLAG_MASK) as u32;
    let encs: &[u8] = match enc & !FLAG_MASK {
        SQLITE_UTF16 | SQLITE_UTF16LE => &[SQLITE_UTF16LE as u8],
        SQLITE_UTF16BE => &[SQLITE_UTF16BE as u8],
        SQLITE_ANY => &[SQLITE_FUNC_UTF8, SQLITE_UTF16LE as u8, SQLITE_UTF16BE as u8],
        _ => &[SQLITE_FUNC_UTF8],
    };
    let lname = String::from_utf8_lossy(name).to_ascii_lowercase();
    let conn = (*db).inner.lock().unwrap().conn.clone();
    let deleting = x_func.is_none() && x_step.is_none();

    // Replacing a function that a running statement may call is refused.
    if is_busy(db) {
        let reg = registry().lock().unwrap();
        if encs.iter().any(|e| {
            reg.get(&(db as usize))
                .is_some_and(|m| m.contains_key(&(lname.clone(), n_args, *e)))
        }) {
            let mut inner = (*db).inner.lock().unwrap();
            return set_db_err_msg(
                &mut inner,
                SQLITE_BUSY,
                "unable to delete/modify user-function due to active statements",
            );
        }
    }

    // A deleted entry keeps its xDestroy until it is replaced or the connection closes.
    let reg = (!deleting || destroy.is_some()).then(|| {
        Arc::new(UdfReg {
            x_func,
            x_step,
            x_final,
            x_value,
            x_inverse,
            p_app: p_app as usize,
            db: db as usize,
            destroy: Mutex::new(destroy),
            uses: AtomicUsize::new(0),
            dead: AtomicBool::new(false),
        })
    });
    let display = String::from_utf8_lossy(name).into_owned();
    for &e in encs {
        let func = match &reg {
            None => None,
            Some(_) if deleting => None,
            Some(reg) => {
                let context = Arc::into_raw(reg.clone()) as usize;
                let built = if x_step.is_some() {
                    let init: turso_ext::InitAggFunction = compat_stubs::agg_init;
                    let step: turso_ext::StepFunction = compat_stubs::agg_step;
                    let fin: turso_ext::FinalizeFunction = compat_stubs::agg_final;
                    turso_core::ExternalFunc::new_aggregate(
                        display.clone(),
                        n_args,
                        context,
                        (init, step, fin),
                        Some(release_context),
                        Some(compat_stubs::agg_destroy_state),
                        None,
                    )
                    .map(|f| match (x_value.is_some(), x_inverse.is_some()) {
                        (true, true) => {
                            f.with_window_callbacks(turso_core::ExternalWindowCallbacks {
                                value: compat_stubs::agg_value,
                                inverse: compat_stubs::agg_inverse,
                            })
                        }
                        _ => f,
                    })
                } else {
                    turso_core::ExternalFunc::new_scalar(
                        display.clone(),
                        n_args,
                        flags & SQLITE_DETERMINISTIC != 0,
                        context,
                        scalar_bridge,
                        Some(release_context),
                        None,
                    )
                };
                match built {
                    Ok(f) => Some(f.with_flags(flags)),
                    Err(_) => {
                        drop(Arc::from_raw(context as *const UdfReg));
                        return SQLITE_MISUSE;
                    }
                }
            }
        };
        if let Some(reg) = &reg {
            reg.uses.fetch_add(1, Ordering::AcqRel);
        }
        conn.set_user_function(&lname, n_args, e, func);
        let key = (lname.clone(), n_args, e);
        let old = {
            let mut all = registry().lock().unwrap();
            let per_db = all.entry(db as usize).or_default();
            match &reg {
                Some(reg) => per_db.insert(key, reg.clone()),
                None => per_db.remove(&key),
            }
        };
        if let Some(old) = old {
            unref(&old);
        }
    }
    attached.store(reg.is_some(), Ordering::Release);
    SQLITE_OK
}

unsafe fn cast<T>(f: Option<unsafe extern "C" fn()>) -> Option<T> {
    f.map(|f| std::mem::transmute_copy::<unsafe extern "C" fn(), T>(&f))
}

unsafe fn name_bytes<'a>(name: *const ffi::c_char) -> &'a [u8] {
    CStr::from_ptr(name).to_bytes()
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_function_v2(
    db: *mut sqlite3,
    name: *const ffi::c_char,
    n_args: ffi::c_int,
    enc: ffi::c_int,
    context: *mut ffi::c_void,
    func: Option<unsafe extern "C" fn()>,
    step: Option<unsafe extern "C" fn()>,
    final_: Option<unsafe extern "C" fn()>,
    destroy: Option<unsafe extern "C" fn()>,
) -> ffi::c_int {
    if db.is_null() || name.is_null() {
        return SQLITE_MISUSE;
    }
    create_function(
        db,
        name_bytes(name),
        n_args,
        enc,
        context,
        cast(func),
        cast(step),
        cast(final_),
        None,
        None,
        cast(destroy),
    )
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_window_function(
    db: *mut sqlite3,
    name: *const ffi::c_char,
    n_args: ffi::c_int,
    enc: ffi::c_int,
    context: *mut ffi::c_void,
    step: Option<unsafe extern "C" fn()>,
    final_: Option<unsafe extern "C" fn()>,
    value: Option<unsafe extern "C" fn()>,
    inverse: Option<unsafe extern "C" fn()>,
    destroy: Option<unsafe extern "C" fn()>,
) -> ffi::c_int {
    if db.is_null() || name.is_null() {
        return SQLITE_MISUSE;
    }
    create_function(
        db,
        name_bytes(name),
        n_args,
        enc,
        context,
        None,
        cast(step),
        cast(final_),
        cast(value),
        cast(inverse),
        cast(destroy),
    )
}

#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_function16(
    db: *mut sqlite3,
    name: *const ffi::c_void,
    n_args: ffi::c_int,
    enc: ffi::c_int,
    context: *mut ffi::c_void,
    func: Option<unsafe extern "C" fn()>,
    step: Option<unsafe extern "C" fn()>,
    final_: Option<unsafe extern "C" fn()>,
) -> ffi::c_int {
    if db.is_null() || name.is_null() {
        return SQLITE_MISUSE;
    }
    let mut units = Vec::new();
    let mut p = name as *const u16;
    while *p != 0 {
        units.push(*p);
        p = p.add(1);
    }
    let utf8 = String::from_utf16_lossy(&units);
    create_function(
        db,
        utf8.as_bytes(),
        n_args,
        enc,
        context,
        cast(func),
        cast(step),
        cast(final_),
        None,
        None,
        None,
    )
}
