//! `sqlite3_rtree_geometry_callback` and `sqlite3_rtree_query_callback`,
//! bridged onto turso's R*Tree MATCH callbacks. `sqlite3_value*` handed to
//! callbacks are `turso_ext::Value`, as for C scalar functions.

use crate::{sqlite3, sqlite3_errstr_impl, SQLITE_MISUSE, SQLITE_OK};
use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::sync::Arc;
use turso_core::{LimboError, RtreeMatchCall, RtreeMatchFunction, RtreeQueryInfo, Value};
use turso_ext::Value as ExtValue;
use turso_macros::capi_guard;

/// `sqlite3_rtree_query_info`; `sqlite3_rtree_geometry` is its prefix.
#[repr(C)]
#[allow(non_snake_case, non_camel_case_types)]
pub struct sqlite3_rtree_query_info {
    pub pContext: *mut c_void,
    pub nParam: c_int,
    pub aParam: *mut f64,
    pub pUser: *mut c_void,
    pub xDelUser: Option<unsafe extern "C" fn(*mut c_void)>,
    pub aCoord: *mut f64,
    pub anQueue: *mut c_uint,
    pub nCoord: c_int,
    pub iLevel: c_int,
    pub mxLevel: c_int,
    pub iRowid: i64,
    pub rParentScore: f64,
    pub eParentWithin: c_int,
    pub eWithin: c_int,
    pub rScore: f64,
    pub apSqlParam: *mut *mut c_void,
}

type GeomFn =
    unsafe extern "C" fn(*mut sqlite3_rtree_query_info, c_int, *mut f64, *mut c_int) -> c_int;
type QueryFn = unsafe extern "C" fn(*mut sqlite3_rtree_query_info) -> c_int;

struct CMatch {
    x_geom: Option<GeomFn>,
    x_query: Option<QueryFn>,
    context: usize,
    destructor: Option<unsafe extern "C" fn(*mut c_void)>,
}

impl std::fmt::Debug for CMatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CMatch")
    }
}

// SAFETY: the callbacks and context are owned by the application, which
// must make them safe to use from the threads that use the connection.
unsafe impl Send for CMatch {}
unsafe impl Sync for CMatch {}

impl Drop for CMatch {
    fn drop(&mut self) {
        if let Some(d) = self.destructor {
            unsafe { d(self.context as *mut c_void) };
        }
    }
}

impl RtreeMatchFunction for CMatch {
    fn is_query(&self) -> bool {
        self.x_query.is_some()
    }

    fn start(
        &self,
        params: &[Value],
        n_coord: usize,
    ) -> turso_core::Result<Box<dyn RtreeMatchCall>> {
        let mut call = Box::new(CCall {
            x_geom: self.x_geom,
            x_query: self.x_query,
            info: Box::new(unsafe { std::mem::zeroed() }),
            params: params.iter().map(as_double).collect(),
            values: params.iter().map(Value::to_ffi).collect(),
            value_ptrs: Vec::new(),
            coords: [0.0; 10],
            queue: Vec::new(),
        });
        call.value_ptrs = call
            .values
            .iter_mut()
            .map(|v| v as *mut ExtValue as *mut c_void)
            .collect();
        let info = call.info.as_mut();
        info.pContext = self.context as *mut c_void;
        info.nParam = params.len() as c_int;
        info.aParam = call.params.as_mut_ptr();
        info.apSqlParam = call.value_ptrs.as_mut_ptr();
        info.nCoord = n_coord as c_int;
        Ok(call)
    }
}

/// `sqlite3_value_double`.
fn as_double(v: &Value) -> f64 {
    match v {
        Value::Numeric(n) => n.to_f64(),
        Value::Text(t) => t.as_str().trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

struct CCall {
    x_geom: Option<GeomFn>,
    x_query: Option<QueryFn>,
    info: Box<sqlite3_rtree_query_info>,
    params: Vec<f64>,
    values: Vec<ExtValue>,
    value_ptrs: Vec<*mut c_void>,
    coords: [f64; 10],
    queue: Vec<c_uint>,
}

// SAFETY: the raw pointers point into this struct's own allocations.
unsafe impl Send for CCall {}
unsafe impl Sync for CCall {}

impl Drop for CCall {
    fn drop(&mut self) {
        if let Some(del) = self.info.xDelUser {
            unsafe { del(self.info.pUser) };
        }
        for v in self.values.drain(..) {
            unsafe { v.__free_internal_type() };
        }
    }
}

fn callback_error(rc: c_int) -> LimboError {
    let msg = unsafe { CStr::from_ptr(sqlite3_errstr_impl(rc)) };
    LimboError::SqlError(msg.to_string_lossy().into_owned())
}

impl RtreeMatchCall for CCall {
    fn geometry(&mut self, coords: &[f64]) -> turso_core::Result<bool> {
        let f = self.x_geom.expect("geometry callback");
        self.coords[..coords.len()].copy_from_slice(coords);
        let mut within: c_int = 0;
        let rc = unsafe {
            f(
                self.info.as_mut(),
                coords.len() as c_int,
                self.coords.as_mut_ptr(),
                &mut within,
            )
        };
        if rc != SQLITE_OK {
            return Err(callback_error(rc));
        }
        Ok(within != 0)
    }

    fn query(&mut self, q: &mut RtreeQueryInfo) -> turso_core::Result<()> {
        let f = self.x_query.expect("query callback");
        self.coords[..q.coords.len()].copy_from_slice(q.coords);
        self.queue.clear();
        self.queue.extend_from_slice(q.queue);
        let info = self.info.as_mut();
        info.aCoord = self.coords.as_mut_ptr();
        info.anQueue = self.queue.as_mut_ptr();
        info.iLevel = q.level;
        info.mxLevel = q.max_level;
        info.iRowid = q.rowid;
        info.rParentScore = q.parent_score;
        info.eParentWithin = q.parent_within;
        info.rScore = q.score;
        info.eWithin = q.within;
        let rc = unsafe { f(info) };
        if rc != SQLITE_OK {
            return Err(callback_error(rc));
        }
        q.score = info.rScore;
        q.within = info.eWithin;
        Ok(())
    }
}

unsafe fn register(db: *mut sqlite3, name: *const c_char, m: CMatch) -> c_int {
    if db.is_null() || name.is_null() {
        return SQLITE_MISUSE;
    }
    let conn = (*db)
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .conn
        .clone();
    let name = CStr::from_ptr(name).to_string_lossy().into_owned();
    match conn.register_rtree_match_function(&name, Arc::new(m)) {
        Ok(()) => SQLITE_OK,
        Err(_) => crate::SQLITE_ERROR,
    }
}

/// # Safety
/// `db` must be an open connection and `name` a NUL-terminated string.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_rtree_geometry_callback(
    db: *mut sqlite3,
    name: *const c_char,
    x_geom: Option<GeomFn>,
    context: *mut c_void,
) -> c_int {
    register(
        db,
        name,
        CMatch {
            x_geom,
            x_query: None,
            context: context as usize,
            destructor: None,
        },
    )
}

/// # Safety
/// `db` must be an open connection and `name` a NUL-terminated string.
#[capi_guard]
#[no_mangle]
pub unsafe extern "C" fn sqlite3_rtree_query_callback(
    db: *mut sqlite3,
    name: *const c_char,
    x_query: Option<QueryFn>,
    context: *mut c_void,
    destructor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    if db.is_null() || name.is_null() {
        if let Some(d) = destructor {
            d(context);
        }
        return SQLITE_MISUSE;
    }
    register(
        db,
        name,
        CMatch {
            x_geom: None,
            x_query,
            context: context as usize,
            destructor,
        },
    )
}
