//! Virtual table C API: `sqlite3_create_module[_v2]`, `sqlite3_declare_vtab`
//! and the `sqlite3_vtab_*` helpers.
//!
//! A C `sqlite3_module` is bridged onto core's native module traits
//! (`turso_core::native_ext`). `sqlite3_value*` handed to the module are
//! `turso_ext::Value*` and the xColumn `sqlite3_context*` is a
//! [`SqliteContext`], as for C scalar functions.

use super::*;
use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_int, c_void};
use turso_core::native_ext::{
    ConnectInfo, ConstraintInfo, ConstraintUsage, IndexInfo, IndexPlan, IndexRequest, OrderByInfo,
    TableUpdate, VirtualTable, VirtualTableCursor, VirtualTableModule,
};
use turso_core::IOResult;
use turso_ext::ResultCode;

type XCreateFn = unsafe extern "C" fn(
    *mut sqlite3,
    *mut c_void,
    c_int,
    *const *const c_char,
    *mut *mut sqlite3_vtab,
    *mut *mut c_char,
) -> c_int;
type XVtabFn = unsafe extern "C" fn(*mut sqlite3_vtab) -> c_int;
type XVtabIntFn = unsafe extern "C" fn(*mut sqlite3_vtab, c_int) -> c_int;
type XCursorFn = unsafe extern "C" fn(*mut sqlite3_vtab_cursor) -> c_int;
type XFuncFn = unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_void);
type XSFunc = XFuncFn;

/// The function an xFindFunction overload calls, with its user data.
struct OverloadCtx {
    x_func: XSFunc,
    p_arg: *mut c_void,
    db: usize,
}

unsafe extern "C" fn overload_call(
    context: usize,
    argc: i32,
    argv: *const ExtValue,
    _context_destructor: Option<turso_ext::ContextDestructor>,
    _value_destructor: Option<turso_ext::ValueDestructor>,
) -> ExtValue {
    let o = &*(context as *const OverloadCtx);
    let mut ptrs: Vec<*mut c_void> = (0..argc.max(0) as usize)
        .map(|i| argv.add(i) as *mut c_void)
        .collect();
    let mut ctx = SqliteContext {
        result: ExtValue::null(),
        p_app: o.p_arg,
        db: o.db as *mut sqlite3,
    };
    (o.x_func)(
        &mut ctx as *mut SqliteContext as *mut c_void,
        argc,
        ptrs.as_mut_ptr(),
    );
    ctx.result
}

unsafe extern "C" fn overload_free(context: usize) {
    drop(Box::from_raw(context as *mut OverloadCtx));
}

/// `struct sqlite3_module`. Fields after xRename exist only when
/// `iVersion` is high enough and are read only then.
#[repr(C)]
#[allow(non_snake_case)]
pub struct sqlite3_module {
    pub iVersion: c_int,
    pub xCreate: Option<XCreateFn>,
    pub xConnect: Option<XCreateFn>,
    pub xBestIndex:
        Option<unsafe extern "C" fn(*mut sqlite3_vtab, *mut sqlite3_index_info) -> c_int>,
    pub xDisconnect: Option<XVtabFn>,
    pub xDestroy: Option<XVtabFn>,
    pub xOpen:
        Option<unsafe extern "C" fn(*mut sqlite3_vtab, *mut *mut sqlite3_vtab_cursor) -> c_int>,
    pub xClose: Option<XCursorFn>,
    pub xFilter: Option<
        unsafe extern "C" fn(
            *mut sqlite3_vtab_cursor,
            c_int,
            *const c_char,
            c_int,
            *mut *mut c_void,
        ) -> c_int,
    >,
    pub xNext: Option<XCursorFn>,
    pub xEof: Option<XCursorFn>,
    pub xColumn:
        Option<unsafe extern "C" fn(*mut sqlite3_vtab_cursor, *mut c_void, c_int) -> c_int>,
    pub xRowid: Option<unsafe extern "C" fn(*mut sqlite3_vtab_cursor, *mut i64) -> c_int>,
    pub xUpdate:
        Option<unsafe extern "C" fn(*mut sqlite3_vtab, c_int, *mut *mut c_void, *mut i64) -> c_int>,
    pub xBegin: Option<XVtabFn>,
    pub xSync: Option<XVtabFn>,
    pub xCommit: Option<XVtabFn>,
    pub xRollback: Option<XVtabFn>,
    pub xFindFunction: Option<
        unsafe extern "C" fn(
            *mut sqlite3_vtab,
            c_int,
            *const c_char,
            *mut Option<XFuncFn>,
            *mut *mut c_void,
        ) -> c_int,
    >,
    pub xRename: Option<unsafe extern "C" fn(*mut sqlite3_vtab, *const c_char) -> c_int>,
    // iVersion >= 2
    pub xSavepoint: Option<XVtabIntFn>,
    pub xRelease: Option<XVtabIntFn>,
    pub xRollbackTo: Option<XVtabIntFn>,
    // iVersion >= 3
    pub xShadowName: Option<unsafe extern "C" fn(*const c_char) -> c_int>,
    // iVersion >= 4
    pub xIntegrity: Option<
        unsafe extern "C" fn(
            *mut sqlite3_vtab,
            *const c_char,
            *const c_char,
            c_int,
            *mut *mut c_char,
        ) -> c_int,
    >,
}

#[repr(C)]
#[allow(non_snake_case)]
pub struct sqlite3_vtab {
    pub pModule: *const sqlite3_module,
    pub nRef: c_int,
    pub zErrMsg: *mut c_char,
}

#[repr(C)]
#[allow(non_snake_case)]
pub struct sqlite3_vtab_cursor {
    pub pVtab: *mut sqlite3_vtab,
}

#[repr(C)]
#[allow(non_snake_case)]
pub struct sqlite3_index_constraint {
    pub iColumn: c_int,
    pub op: u8,
    pub usable: u8,
    pub iTermOffset: c_int,
}

#[repr(C)]
#[allow(non_snake_case)]
pub struct sqlite3_index_orderby {
    pub iColumn: c_int,
    pub desc: u8,
}

#[repr(C)]
#[allow(non_snake_case)]
pub struct sqlite3_index_constraint_usage {
    pub argvIndex: c_int,
    pub omit: u8,
}

#[repr(C)]
#[allow(non_snake_case)]
pub struct sqlite3_index_info {
    pub nConstraint: c_int,
    pub aConstraint: *mut sqlite3_index_constraint,
    pub nOrderBy: c_int,
    pub aOrderBy: *mut sqlite3_index_orderby,
    pub aConstraintUsage: *mut sqlite3_index_constraint_usage,
    pub idxNum: c_int,
    pub idxStr: *mut c_char,
    pub needToFreeIdxStr: c_int,
    pub orderByConsumed: c_int,
    pub estimatedCost: f64,
    pub estimatedRows: i64,
    pub idxFlags: c_int,
    pub colUsed: u64,
}

const SQLITE_VTAB_CONSTRAINT_SUPPORT: c_int = 1;
const SQLITE_VTAB_USES_ALL_SCHEMAS: c_int = 4;

// sqlite3_vtab_on_conflict() results.
const SQLITE_ROLLBACK: c_int = 1;
const SQLITE_IGNORE: c_int = 2;
const SQLITE_FAIL: c_int = 3;
const SQLITE_REPLACE: c_int = 5;

/// Marks an idxStr encoded by this module (never valid UTF-8 text from C).
const IDXSTR_PTR: char = '\u{1}';
const IDXSTR_BYTES: char = '\u{2}';

std::thread_local! {
    /// Constructors (xCreate/xConnect) running on this thread: the
    /// connection and the schema handed to sqlite3_declare_vtab.
    static DECLARE: RefCell<Vec<(usize, Option<String>)>> = const { RefCell::new(Vec::new()) };
    /// ON CONFLICT mode of the xUpdate call in progress.
    static ON_CONFLICT: Cell<c_int> = const { Cell::new(SQLITE_ABORT) };
    /// The context of the nochange xColumn call in progress.
    static NOCHANGE_CTX: Cell<usize> = const { Cell::new(0) };
    /// Per table: columns of the current row left unset in a nochange read.
    static NOCHANGE_COLS: RefCell<Vec<(usize, Vec<usize>)>> = const { RefCell::new(Vec::new()) };
    /// xUpdate arguments that are unchanged columns.
    static NOCHANGE_VALUES: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// Marks a nochange xColumn result the table did not set.
const NOCHANGE_UNSET: &str = "\u{1}nochange";

/// One `sqlite3_create_module_v2` registration. The client data destructor
/// runs once the module and every table connected through it are gone.
struct ModuleInner {
    db: usize,
    module: *const sqlite3_module,
    p_aux: *mut c_void,
    x_destroy: Option<unsafe extern "C" fn(*mut c_void)>,
}

// SAFETY: the raw pointers belong to the C caller, as in SQLite.
unsafe impl Send for ModuleInner {}
unsafe impl Sync for ModuleInner {}

impl Drop for ModuleInner {
    fn drop(&mut self) {
        if let Some(destroy) = self.x_destroy {
            unsafe { destroy(self.p_aux) };
        }
    }
}

impl ModuleInner {
    fn methods(&self) -> &sqlite3_module {
        unsafe { &*self.module }
    }

    fn version(&self) -> c_int {
        self.methods().iVersion
    }
}

#[derive(Clone)]
struct CModule(Arc<ModuleInner>);

impl std::fmt::Debug for CModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CModule")
    }
}

/// Builds the error for a failed method, taking `pVtab->zErrMsg`.
unsafe fn vtab_error(vtab: *mut sqlite3_vtab, rc: c_int) -> LimboError {
    let mut msg = None;
    if !vtab.is_null() && !(*vtab).zErrMsg.is_null() {
        msg = Some(
            CStr::from_ptr((*vtab).zErrMsg)
                .to_string_lossy()
                .into_owned(),
        );
        sqlite3_free((*vtab).zErrMsg as *mut c_void);
        (*vtab).zErrMsg = std::ptr::null_mut();
    }
    rc_error(rc, msg)
}

fn rc_error(rc: c_int, msg: Option<String>) -> LimboError {
    let msg = msg.unwrap_or_else(|| unsafe {
        CStr::from_ptr(sqlite3_errstr_impl(rc))
            .to_string_lossy()
            .into_owned()
    });
    match rc & 0xff {
        SQLITE_CONSTRAINT => LimboError::Constraint(msg),
        SQLITE_READONLY => LimboError::ReadOnly,
        SQLITE_BUSY => LimboError::Busy,
        SQLITE_INTERRUPT => LimboError::Interrupt,
        SQLITE_CORRUPT => LimboError::Corrupt(msg),
        _ => LimboError::SqlError(msg),
    }
}

fn check(vtab: *mut sqlite3_vtab, rc: c_int) -> turso_core::Result<()> {
    if rc == SQLITE_OK {
        Ok(())
    } else {
        Err(unsafe { vtab_error(vtab, rc) })
    }
}

impl VirtualTableModule for CModule {
    type Table = CTable;

    fn schema(&self, _args: &[Value]) -> turso_core::Result<String> {
        // Only used to probe the schema at prepare time; SQLite runs no
        // constructor there.
        Err(LimboError::InternalError("no schema probe".to_string()))
    }

    fn create(&self, _args: &[Value]) -> turso_core::Result<CTable> {
        Err(LimboError::InternalError("use connect".to_string()))
    }

    fn connect(&self, info: &ConnectInfo, args: &[Value]) -> turso_core::Result<(CTable, String)> {
        let inner = &self.0;
        let m = inner.methods();
        let ctor = if info.create { m.xCreate } else { m.xConnect };
        let Some(ctor) = ctor else {
            return Err(LimboError::SqlError(format!(
                "vtable constructor failed: {}",
                info.table_name
            )));
        };
        let cstr = |s: &str| CString::new(s.replace('\0', "")).unwrap_or_default();
        let mut owned = vec![
            cstr(info.module_name),
            cstr(info.schema_name),
            cstr(info.table_name),
        ];
        owned.extend(args.iter().map(|a| cstr(&a.to_string())));
        let argv: Vec<*const c_char> = owned.iter().map(|s| s.as_ptr()).collect();

        DECLARE.with(|d| d.borrow_mut().push((inner.db, None)));
        let mut vtab: *mut sqlite3_vtab = std::ptr::null_mut();
        let mut err: *mut c_char = std::ptr::null_mut();
        let rc = unsafe {
            ctor(
                inner.db as *mut sqlite3,
                inner.p_aux,
                argv.len() as c_int,
                argv.as_ptr(),
                &mut vtab,
                &mut err,
            )
        };
        let declared = DECLARE.with(|d| d.borrow_mut().pop()).and_then(|d| d.1);
        let err_msg = (!err.is_null()).then(|| unsafe {
            let s = CStr::from_ptr(err).to_string_lossy().into_owned();
            sqlite3_free(err as *mut c_void);
            s
        });
        if rc != SQLITE_OK {
            let msg = err_msg
                .unwrap_or_else(|| format!("vtable constructor failed: {}", info.table_name));
            return Err(rc_error(rc, Some(msg)));
        }
        if vtab.is_null() {
            return Err(LimboError::SqlError(format!(
                "vtable constructor failed: {}",
                info.table_name
            )));
        }
        unsafe {
            (*vtab).pModule = inner.module;
            (*vtab).nRef = 1;
            (*vtab).zErrMsg = std::ptr::null_mut();
        }
        let table = CTable {
            inner: inner.clone(),
            vtab,
            destroyed: false,
        };
        let Some(schema) = declared else {
            return Err(LimboError::SqlError(format!(
                "vtable constructor did not declare schema: {}",
                info.table_name
            )));
        };
        Ok((table, schema))
    }
}

struct CTable {
    inner: Arc<ModuleInner>,
    vtab: *mut sqlite3_vtab,
    destroyed: bool,
}

unsafe impl Send for CTable {}
unsafe impl Sync for CTable {}

impl std::fmt::Debug for CTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CTable({:p})", self.vtab)
    }
}

impl Drop for CTable {
    fn drop(&mut self) {
        if !self.destroyed {
            if let Some(f) = self.inner.methods().xDisconnect {
                unsafe { f(self.vtab) };
            }
        }
    }
}

/// The index info passed to xBestIndex, followed by private state that the
/// `sqlite3_vtab_*` planning helpers read (as SQLite's HiddenIndexInfo).
#[repr(C)]
struct IndexInfoBox {
    info: sqlite3_index_info,
    n_constraint: usize,
    collations: Vec<CString>,
    rhs: Vec<Option<Value>>,
    /// `rhs` as sqlite3_values, made on first use; freed with the box.
    rhs_values: Vec<Option<Box<ExtValue>>>,
    is_in: Vec<bool>,
    handle_in: Vec<bool>,
    distinct: c_int,
}

impl Drop for IndexInfoBox {
    fn drop(&mut self) {
        for v in self.rhs_values.drain(..).flatten() {
            unsafe { v.__free_internal_type() };
        }
    }
}

/// Turns the module's idxStr into a string that survives the plan; see
/// [`decode_idx_str`].
unsafe fn encode_idx_str(p: *mut c_char, need_free: bool) -> Option<String> {
    if p.is_null() {
        return None;
    }
    if !need_free {
        // Module-owned: hand the same pointer back to xFilter.
        return Some(format!("{IDXSTR_PTR}{:x}", p as usize));
    }
    // sqlite3_malloc'd and owned by us: copy the allocation (it may be
    // binary) and free it now.
    let len = alloc_size(p as *mut c_void);
    let bytes = std::slice::from_raw_parts(p as *const u8, len);
    let mut s = String::with_capacity(1 + 2 * len);
    s.push(IDXSTR_BYTES);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    sqlite3_free(p as *mut c_void);
    Some(s)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe fn alloc_size(p: *mut c_void) -> usize {
    // sqlite3_malloc is libc malloc.
    libc::malloc_usable_size(p)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
unsafe fn alloc_size(p: *mut c_void) -> usize {
    libc::strlen(p as *const c_char) + 1
}

enum IdxStr {
    None,
    Ptr(*const c_char),
    Owned(Vec<u8>),
}

fn decode_idx_str(s: Option<&str>) -> IdxStr {
    let Some(s) = s else {
        return IdxStr::None;
    };
    if let Some(hex) = s.strip_prefix(IDXSTR_PTR) {
        if let Ok(p) = usize::from_str_radix(hex, 16) {
            return IdxStr::Ptr(p as *const c_char);
        }
    } else if let Some(hex) = s.strip_prefix(IDXSTR_BYTES) {
        let mut bytes: Vec<u8> = (0..hex.len() / 2)
            .filter_map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok())
            .collect();
        bytes.push(0);
        return IdxStr::Owned(bytes);
    }
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    IdxStr::Owned(bytes)
}

impl VirtualTable for CTable {
    type Cursor = CCursor;

    fn open(&self, _connection: Arc<turso_core::Connection>) -> turso_core::Result<CCursor> {
        let Some(x_open) = self.inner.methods().xOpen else {
            return Err(LimboError::SqlError(
                "virtual table has no xOpen".to_string(),
            ));
        };
        let mut cur: *mut sqlite3_vtab_cursor = std::ptr::null_mut();
        check(self.vtab, unsafe { x_open(self.vtab, &mut cur) })?;
        if cur.is_null() {
            return Err(LimboError::SqlError("xOpen returned no cursor".to_string()));
        }
        unsafe { (*cur).pVtab = self.vtab };
        Ok(CCursor {
            inner: self.inner.clone(),
            cur,
            eof: true,
            idx_str: None,
        })
    }

    fn best_index(
        &self,
        _constraints: &[ConstraintInfo],
        _order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        // The planner always calls best_index_plan.
        Err(ResultCode::Error)
    }

    fn best_index_plan(&self, req: &IndexRequest) -> turso_core::Result<Option<IndexPlan>> {
        let Some(x_best_index) = self.inner.methods().xBestIndex else {
            return Err(LimboError::SqlError(
                "virtual table has no xBestIndex".into(),
            ));
        };
        let n = req.constraints.len();
        let mut a_constraint: Vec<sqlite3_index_constraint> = req
            .constraints
            .iter()
            .enumerate()
            .map(|(i, c)| sqlite3_index_constraint {
                iColumn: c.column,
                op: c.op,
                usable: c.usable as u8,
                iTermOffset: i as c_int,
            })
            .collect();
        let mut a_order_by: Vec<sqlite3_index_orderby> = req
            .order_by
            .iter()
            .map(|o| sqlite3_index_orderby {
                iColumn: o.column,
                desc: o.desc as u8,
            })
            .collect();
        let mut a_usage: Vec<sqlite3_index_constraint_usage> = (0..n)
            .map(|_| sqlite3_index_constraint_usage {
                argvIndex: 0,
                omit: 0,
            })
            .collect();
        let defaults = IndexPlan::new(n);
        let mut boxed = IndexInfoBox {
            info: sqlite3_index_info {
                nConstraint: n as c_int,
                aConstraint: a_constraint.as_mut_ptr(),
                nOrderBy: a_order_by.len() as c_int,
                aOrderBy: a_order_by.as_mut_ptr(),
                aConstraintUsage: a_usage.as_mut_ptr(),
                idxNum: 0,
                idxStr: std::ptr::null_mut(),
                needToFreeIdxStr: 0,
                orderByConsumed: 0,
                estimatedCost: defaults.estimated_cost,
                estimatedRows: defaults.estimated_rows,
                idxFlags: 0,
                colUsed: req.col_used,
            },
            n_constraint: n,
            collations: req
                .constraints
                .iter()
                .map(|c| CString::new(c.collation.as_str()).unwrap_or_default())
                .collect(),
            rhs: req.constraints.iter().map(|c| c.rhs.clone()).collect(),
            rhs_values: (0..n).map(|_| None).collect(),
            is_in: req.constraints.iter().map(|c| c.is_in).collect(),
            handle_in: vec![false; n],
            distinct: req.distinct,
        };
        let rc = unsafe { x_best_index(self.vtab, &mut boxed.info) };
        let info = &boxed.info;
        let idx_str_display = (!info.idxStr.is_null())
            .then(|| unsafe { CStr::from_ptr(info.idxStr).to_string_lossy().into_owned() });
        let idx_str = unsafe { encode_idx_str(info.idxStr, info.needToFreeIdxStr != 0) };
        if rc != SQLITE_OK {
            let err = unsafe { vtab_error(self.vtab, rc) };
            if rc & 0xff == SQLITE_CONSTRAINT {
                return Ok(None);
            }
            return Err(err);
        }
        let mut plan = defaults;
        for (i, usage) in a_usage.iter().enumerate() {
            plan.usages[i] = ConstraintUsage {
                argv_index: (usage.argvIndex > 0).then_some(usage.argvIndex as u32),
                omit: usage.omit != 0,
            };
        }
        plan.in_all_at_once = boxed.handle_in.clone();
        plan.idx_num = info.idxNum;
        plan.idx_str = idx_str;
        plan.idx_str_display = idx_str_display;
        plan.order_by_consumed = info.orderByConsumed != 0;
        plan.estimated_cost = info.estimatedCost;
        plan.estimated_rows = info.estimatedRows;
        plan.idx_flags = info.idxFlags;
        Ok(Some(plan))
    }

    fn find_function(
        &self,
        n_arg: i32,
        name: &str,
    ) -> Option<(i32, Arc<turso_core::ExternalFunc>)> {
        let f = self.inner.methods().xFindFunction?;
        let cname = CString::new(name).ok()?;
        let mut x_func: Option<XSFunc> = None;
        let mut p_arg: *mut c_void = std::ptr::null_mut();
        let rc = unsafe { f(self.vtab, n_arg, cname.as_ptr(), &mut x_func, &mut p_arg) };
        if rc == 0 {
            return None;
        }
        let ctx = Box::new(OverloadCtx {
            x_func: x_func?,
            p_arg,
            db: self.inner.db,
        });
        let func = turso_core::ExternalFunc::new_scalar(
            name.to_string(),
            n_arg,
            false,
            Box::into_raw(ctx) as usize,
            overload_call,
            Some(overload_free),
            None,
        )
        .ok()?;
        Some((rc, Arc::new(func)))
    }

    fn integrity(
        &self,
        schema: &str,
        table: &str,
        quick: bool,
    ) -> turso_core::Result<Option<String>> {
        if self.inner.version() < 4 {
            return Ok(None);
        }
        let Some(f) = self.inner.methods().xIntegrity else {
            return Ok(None);
        };
        let schema = CString::new(schema).unwrap_or_default();
        let table = CString::new(table).unwrap_or_default();
        let mut err: *mut c_char = std::ptr::null_mut();
        let rc = unsafe {
            f(
                self.vtab,
                schema.as_ptr(),
                table.as_ptr(),
                quick as c_int,
                &mut err,
            )
        };
        let msg = (!err.is_null()).then(|| unsafe {
            let s = CStr::from_ptr(err).to_string_lossy().into_owned();
            sqlite3_free(err as *mut c_void);
            s
        });
        if rc != SQLITE_OK {
            return Err(rc_error(rc, None));
        }
        Ok(msg)
    }

    fn has_integrity(&self) -> bool {
        self.inner.version() >= 4 && self.inner.methods().xIntegrity.is_some()
    }

    fn is_shadow_name(&self, suffix: &str) -> bool {
        if self.inner.version() < 3 {
            return false;
        }
        let Some(f) = self.inner.methods().xShadowName else {
            return false;
        };
        let suffix = CString::new(suffix).unwrap_or_default();
        unsafe { f(suffix.as_ptr()) != 0 }
    }

    fn readonly(&self) -> bool {
        self.inner.methods().xUpdate.is_none()
    }

    fn create_update(&mut self, args: Vec<Value>) -> turso_core::Result<Box<dyn TableUpdate>> {
        self.create_update_on_conflict(args, 0)
    }

    fn create_update_on_conflict(
        &mut self,
        args: Vec<Value>,
        on_conflict: u8,
    ) -> turso_core::Result<Box<dyn TableUpdate>> {
        if self.inner.methods().xUpdate.is_none() {
            return Err(LimboError::ReadOnly);
        }
        Ok(Box::new(CUpdate {
            inner: self.inner.clone(),
            vtab: self.vtab,
            args,
            on_conflict: match on_conflict {
                1 => SQLITE_ROLLBACK,
                3 => SQLITE_FAIL,
                4 => SQLITE_IGNORE,
                5 => SQLITE_REPLACE,
                _ => SQLITE_ABORT,
            },
        }))
    }

    fn begin(&mut self) -> turso_core::Result<()> {
        match self.inner.methods().xBegin {
            Some(f) => check(self.vtab, unsafe { f(self.vtab) }),
            None => Ok(()),
        }
    }

    fn commit(&mut self) -> turso_core::Result<()> {
        let m = self.inner.methods();
        if let Some(f) = m.xSync {
            check(self.vtab, unsafe { f(self.vtab) })?;
        }
        match m.xCommit {
            Some(f) => check(self.vtab, unsafe { f(self.vtab) }),
            None => Ok(()),
        }
    }

    fn rollback(&mut self) -> turso_core::Result<()> {
        match self.inner.methods().xRollback {
            Some(f) => check(self.vtab, unsafe { f(self.vtab) }),
            None => Ok(()),
        }
    }

    fn commits_after_create(&self) -> bool {
        true
    }

    fn savepoint(&mut self, level: usize) -> turso_core::Result<()> {
        self.savepoint_method(|m| m.xSavepoint, level)
    }

    fn release(&mut self, level: usize) -> turso_core::Result<()> {
        self.savepoint_method(|m| m.xRelease, level)
    }

    fn rollback_to(&mut self, level: usize) -> turso_core::Result<()> {
        self.savepoint_method(|m| m.xRollbackTo, level)
    }

    fn rename(&mut self, name: &str) -> turso_core::Result<()> {
        let Some(f) = self.inner.methods().xRename else {
            return Ok(());
        };
        let name = CString::new(name).unwrap_or_default();
        check(self.vtab, unsafe { f(self.vtab, name.as_ptr()) })
    }

    fn destroy(&mut self) -> turso_core::Result<()> {
        let m = self.inner.methods();
        let Some(f) = m.xDestroy.or(m.xDisconnect) else {
            self.destroyed = true;
            return Ok(());
        };
        check(self.vtab, unsafe { f(self.vtab) })?;
        self.destroyed = true;
        Ok(())
    }
}

impl CTable {
    fn savepoint_method(
        &self,
        pick: impl Fn(&sqlite3_module) -> Option<XVtabIntFn>,
        level: usize,
    ) -> turso_core::Result<()> {
        if self.inner.version() < 2 {
            return Ok(());
        }
        match pick(self.inner.methods()) {
            Some(f) => check(self.vtab, unsafe { f(self.vtab, level as c_int) }),
            None => Ok(()),
        }
    }
}

struct CUpdate {
    inner: Arc<ModuleInner>,
    vtab: *mut sqlite3_vtab,
    args: Vec<Value>,
    on_conflict: c_int,
}

unsafe impl Send for CUpdate {}
unsafe impl Sync for CUpdate {}

/// `turso_ext::Value`s for C callbacks, freed on drop.
struct ExtArgs(Vec<ExtValue>);

impl ExtArgs {
    fn new(values: &[Value]) -> Self {
        Self(values.iter().map(Value::to_ffi).collect())
    }

    fn ptrs(&mut self) -> Vec<*mut c_void> {
        self.0
            .iter_mut()
            .map(|v| v as *mut ExtValue as *mut c_void)
            .collect()
    }
}

impl Drop for ExtArgs {
    fn drop(&mut self) {
        for v in self.0.drain(..) {
            unsafe { v.__free_internal_type() };
        }
    }
}

impl TableUpdate for CUpdate {
    fn step(&mut self) -> turso_core::types::IOResultOr<Option<i64>> {
        let x_update = self.inner.methods().xUpdate.ok_or(LimboError::ReadOnly)?;
        // Core passes a DELETE as [rowid, NULL]; xUpdate takes it as argc == 1.
        let args = if self.args.len() == 2 {
            &self.args[..1]
        } else {
            &self.args[..]
        };
        let mut args = ExtArgs::new(args);
        let mut ptrs = args.ptrs();
        let mut rowid: i64 = 0;
        // UPDATE: columns the table left unset in a nochange read.
        let unchanged: Vec<usize> = if ptrs.len() > 2 && !matches!(self.args[0], Value::Null) {
            take_unchanged(self.vtab as usize)
                .into_iter()
                .filter_map(|col| ptrs.get(col + 2).map(|p| *p as usize))
                .collect()
        } else {
            Vec::new()
        };
        NOCHANGE_VALUES.with(|v| v.borrow_mut().extend(&unchanged));
        let prev = ON_CONFLICT.with(|c| c.replace(self.on_conflict));
        let rc = unsafe {
            x_update(
                self.vtab,
                ptrs.len() as c_int,
                ptrs.as_mut_ptr(),
                &mut rowid,
            )
        };
        ON_CONFLICT.with(|c| c.set(prev));
        NOCHANGE_VALUES.with(|v| v.borrow_mut().retain(|p| !unchanged.contains(p)));
        check(self.vtab, rc)?;
        let is_insert = self.args.len() > 1 && matches!(self.args[0], Value::Null);
        Ok(IOResult::Done(is_insert.then_some(rowid)))
    }
}

struct CCursor {
    inner: Arc<ModuleInner>,
    cur: *mut sqlite3_vtab_cursor,
    eof: bool,
    /// Decoded idxStr; kept alive for the module until the next xFilter.
    idx_str: Option<Vec<u8>>,
}

unsafe impl Send for CCursor {}
unsafe impl Sync for CCursor {}

impl Drop for CCursor {
    fn drop(&mut self) {
        if let Some(f) = self.inner.methods().xClose {
            unsafe { f(self.cur) };
        }
    }
}

impl CCursor {
    fn vtab(&self) -> *mut sqlite3_vtab {
        unsafe { (*self.cur).pVtab }
    }

    fn check(&self, rc: c_int) -> turso_core::Result<()> {
        check(self.vtab(), rc)
    }

    fn at_eof(&mut self) -> bool {
        self.eof = match self.inner.methods().xEof {
            Some(f) => unsafe { f(self.cur) != 0 },
            None => true,
        };
        self.eof
    }
}

impl VirtualTableCursor for CCursor {
    fn filter(
        &mut self,
        args: &[Value],
        idx_str: Option<&str>,
        idx_num: i32,
    ) -> turso_core::types::IOResultOr<bool> {
        self.filter_in(args, &[], idx_str, idx_num)
    }

    fn filter_in(
        &mut self,
        args: &[Value],
        in_lists: &[Option<Vec<Value>>],
        idx_str: Option<&str>,
        idx_num: i32,
    ) -> turso_core::types::IOResultOr<bool> {
        let x_filter = self
            .inner
            .methods()
            .xFilter
            .ok_or_else(|| LimboError::SqlError("virtual table has no xFilter".to_string()))?;
        let p_idx = match decode_idx_str(idx_str) {
            IdxStr::None => {
                self.idx_str = None;
                std::ptr::null()
            }
            IdxStr::Ptr(p) => {
                self.idx_str = None;
                p
            }
            IdxStr::Owned(bytes) => {
                let bytes = self.idx_str.insert(bytes);
                bytes.as_ptr() as *const c_char
            }
        };
        let mut ext = ExtArgs::new(args);
        let mut ptrs = ext.ptrs();
        // An IN passed whole: a NULL argument that sqlite3_vtab_in_first knows.
        let registered: Vec<usize> = in_lists
            .iter()
            .enumerate()
            .filter_map(|(i, list)| {
                let list = list.as_ref()?;
                let addr = *ptrs.get(i)? as usize;
                let values = list.iter().map(Value::to_ffi).collect();
                IN_LISTS.with(|l| l.borrow_mut().push((addr, values, 0)));
                Some(addr)
            })
            .collect();
        let rc = unsafe {
            x_filter(
                self.cur,
                idx_num,
                p_idx,
                ptrs.len() as c_int,
                ptrs.as_mut_ptr(),
            )
        };
        for addr in registered {
            IN_LISTS.with(|l| {
                let mut l = l.borrow_mut();
                if let Some(pos) = l.iter().rposition(|(a, _, _)| *a == addr) {
                    for v in l.remove(pos).1 {
                        unsafe { v.__free_internal_type() };
                    }
                }
            });
        }
        drop(ext);
        self.check(rc)?;
        Ok(IOResult::Done(!self.at_eof()))
    }

    fn next(&mut self) -> turso_core::types::IOResultOr<bool> {
        if let Some(f) = self.inner.methods().xNext {
            self.check(unsafe { f(self.cur) })?;
        }
        Ok(IOResult::Done(!self.at_eof()))
    }

    fn column(&mut self, column: usize) -> turso_core::types::IOResultOr<Value> {
        self.read_column(column, false)
    }

    fn column_nochange(&mut self, column: usize) -> turso_core::types::IOResultOr<Value> {
        self.read_column(column, true)
    }

    fn rowid(&self) -> i64 {
        let Some(f) = self.inner.methods().xRowid else {
            return 0;
        };
        let mut rowid: i64 = 0;
        if unsafe { f(self.cur, &mut rowid) } != SQLITE_OK {
            unsafe { vtab_error(self.vtab(), SQLITE_ERROR) };
        }
        rowid
    }
}

impl CCursor {
    fn read_column(
        &mut self,
        column: usize,
        nochange: bool,
    ) -> turso_core::types::IOResultOr<Value> {
        let Some(f) = self.inner.methods().xColumn else {
            return Ok(IOResult::Done(Value::Null));
        };
        let mut ctx = SqliteContext {
            // In a nochange read, a result left unset means "unchanged".
            result: if nochange {
                ExtValue::error_with_message(NOCHANGE_UNSET.to_string())
            } else {
                ExtValue::null()
            },
            p_app: std::ptr::null_mut(),
            db: self.inner.db as *mut sqlite3,
        };
        let ctx_ptr = &mut ctx as *mut SqliteContext as *mut c_void;
        let prev = nochange.then(|| NOCHANGE_CTX.with(|c| c.replace(ctx_ptr as usize)));
        let rc = unsafe { f(self.cur, ctx_ptr, column as c_int) };
        if let Some(prev) = prev {
            NOCHANGE_CTX.with(|c| c.set(prev));
        }
        let result = std::mem::replace(&mut ctx.result, ExtValue::null());
        if nochange {
            let unset = result
                .to_error_details()
                .is_some_and(|(_, m)| m.as_deref() == Some(NOCHANGE_UNSET));
            set_unchanged(self.vtab() as usize, column, unset);
            if unset {
                unsafe { result.__free_internal_type() };
                if rc != SQLITE_OK {
                    return Err(Box::new(unsafe { vtab_error(self.vtab(), rc) }));
                }
                return Ok(IOResult::Done(Value::Null));
            }
        }
        if result.value_type() == turso_ext::ValueType::Error {
            let msg = result
                .to_error_details()
                .and_then(|(_, m)| m)
                .unwrap_or_default();
            unsafe { result.__free_internal_type() };
            return Err(LimboError::SqlError(msg).into());
        }
        if rc != SQLITE_OK {
            unsafe { result.__free_internal_type() };
            return Err(Box::new(unsafe { vtab_error(self.vtab(), rc) }));
        }
        Ok(IOResult::Done(Value::from_ffi(result)?))
    }
}

unsafe fn db_conn(db: *mut sqlite3) -> Option<Arc<turso_core::Connection>> {
    if db.is_null() {
        return None;
    }
    Some(
        (*db)
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .conn
            .clone(),
    )
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_module(
    db: *mut sqlite3,
    name: *const c_char,
    module: *const sqlite3_module,
    p_aux: *mut c_void,
) -> c_int {
    sqlite3_create_module_v2(db, name, module, p_aux, None)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_module_v2(
    db: *mut sqlite3,
    name: *const c_char,
    module: *const sqlite3_module,
    p_aux: *mut c_void,
    x_destroy: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    let (Some(conn), false) = (db_conn(db), name.is_null()) else {
        return SQLITE_MISUSE;
    };
    let inner = ModuleInner {
        db: db as usize,
        module,
        p_aux,
        x_destroy,
    };
    let name = CStr::from_ptr(name).to_string_lossy().into_owned();
    if module.is_null() {
        drop(inner);
        conn.unregister_vtab_module_local(&name);
        return SQLITE_OK;
    }
    let m = &*module;
    let creatable = m.xCreate.is_some();
    // xCreate == NULL: eponymous-only; xCreate == xConnect: also eponymous.
    let eponymous = m.xConnect.is_some()
        && (m.xCreate.is_none() || m.xCreate.map(|f| f as usize) == m.xConnect.map(|f| f as usize));
    match conn.register_vtab_module_local(&name, CModule(Arc::new(inner)), creatable, eponymous) {
        Ok(()) => SQLITE_OK,
        Err(e) => set_db_err(&mut (*db).inner.lock().unwrap(), e),
    }
}

/// True for a lightly checked `CREATE TABLE` statement.
fn is_create_table(sql: &str) -> bool {
    let mut words = sql.split_whitespace();
    matches!(
        (words.next(), words.next()),
        (Some(a), Some(b)) if a.eq_ignore_ascii_case("create")
            && (b.eq_ignore_ascii_case("table") || b.to_ascii_lowercase().starts_with("table"))
    )
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_declare_vtab(db: *mut sqlite3, sql: *const c_char) -> c_int {
    if db.is_null() || sql.is_null() {
        return SQLITE_MISUSE;
    }
    let sql = CStr::from_ptr(sql).to_string_lossy().into_owned();
    let ok = is_create_table(&sql);
    let found = DECLARE.with(|d| {
        let mut d = d.borrow_mut();
        match d.iter_mut().rev().find(|(id, _)| *id == db as usize) {
            Some(slot) => {
                if ok {
                    slot.1 = Some(sql.clone());
                }
                true
            }
            None => false,
        }
    });
    if !found {
        return set_err(db, SQLITE_MISUSE, None);
    }
    if !ok {
        return set_err(
            db,
            SQLITE_ERROR,
            Some(&format!("syntax error in \"{sql}\"")),
        );
    }
    set_err(db, SQLITE_OK, None)
}

/// Records an error on `db`. Constructors run while the caller (e.g.
/// sqlite3_prepare_v2) holds the handle lock, so this skips it when busy.
unsafe fn set_err(db: *mut sqlite3, code: c_int, msg: Option<&str>) -> c_int {
    if let Ok(mut inner) = (*db).inner.try_lock() {
        match msg {
            Some(msg) => set_db_err_msg(&mut inner, code, msg),
            None => set_db_err_code(&mut inner, code),
        };
    }
    code
}

/// `sqlite3_vtab_config(db, op, ...)`. The variadic argument is never read
/// (every option is accepted and ignored), so a fixed-argument definition
/// is ABI-compatible on every target.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_vtab_config(db: *mut sqlite3, op: c_int) -> c_int {
    if db.is_null() {
        return SQLITE_MISUSE;
    }
    let in_ctor = DECLARE.with(|d| d.borrow().iter().any(|(id, _)| *id == db as usize));
    if !in_ctor || !(SQLITE_VTAB_CONSTRAINT_SUPPORT..=SQLITE_VTAB_USES_ALL_SCHEMAS).contains(&op) {
        return set_err(db, SQLITE_MISUSE, None);
    }
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_vtab_on_conflict(_db: *mut sqlite3) -> c_int {
    ON_CONFLICT.with(|c| c.get())
}

/// True inside xColumn when UPDATE passes the column through unchanged.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_vtab_nochange(ctx: *mut c_void) -> c_int {
    (!ctx.is_null() && NOCHANGE_CTX.with(|c| c.get()) == ctx as usize) as c_int
}

/// True for an xUpdate argument the table left unset in a nochange read.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_value_nochange(value: *mut c_void) -> c_int {
    (!value.is_null() && NOCHANGE_VALUES.with(|v| v.borrow().contains(&(value as usize)))) as c_int
}

/// Records whether `column` of the table's current row was left unset.
fn set_unchanged(vtab: usize, column: usize, unset: bool) {
    NOCHANGE_COLS.with(|m| {
        let mut m = m.borrow_mut();
        let pos = m.iter().position(|(v, _)| *v == vtab);
        let cols = match pos {
            Some(i) => &mut m[i].1,
            None => {
                m.push((vtab, Vec::new()));
                &mut m.last_mut().unwrap().1
            }
        };
        cols.retain(|c| *c != column);
        if unset {
            cols.push(column);
        }
    });
}

fn take_unchanged(vtab: usize) -> Vec<usize> {
    NOCHANGE_COLS.with(|m| {
        let mut m = m.borrow_mut();
        match m.iter().position(|(v, _)| *v == vtab) {
            Some(i) => m.swap_remove(i).1,
            None => Vec::new(),
        }
    })
}

unsafe fn index_box<'a>(p: *mut sqlite3_index_info, i: c_int) -> Option<&'a mut IndexInfoBox> {
    if p.is_null() {
        return None;
    }
    let b = &mut *(p as *mut IndexInfoBox);
    (i >= 0 && (i as usize) < b.n_constraint).then_some(b)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_vtab_collation(
    p: *mut sqlite3_index_info,
    i: c_int,
) -> *const c_char {
    match index_box(p, i) {
        Some(b) => b.collations[i as usize].as_ptr(),
        None => std::ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_vtab_distinct(p: *mut sqlite3_index_info) -> c_int {
    if p.is_null() {
        return 0;
    }
    (*(p as *const IndexInfoBox)).distinct
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_vtab_in(
    p: *mut sqlite3_index_info,
    i: c_int,
    handle: c_int,
) -> c_int {
    // SQLite tracks IN constraints in a 32-bit mask.
    let Some(b) = index_box(p, i).filter(|_| i < 32) else {
        return 0;
    };
    if !b.is_in[i as usize] {
        return 0;
    }
    if handle == 0 {
        b.handle_in[i as usize] = false;
    } else if handle > 0 {
        b.handle_in[i as usize] = true;
    }
    1
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_vtab_rhs_value(
    p: *mut sqlite3_index_info,
    i: c_int,
    out: *mut *mut c_void,
) -> c_int {
    let mut value: *mut c_void = std::ptr::null_mut();
    let rc = match index_box(p, i) {
        None => SQLITE_MISUSE,
        Some(b) => {
            let i = i as usize;
            if b.rhs_values[i].is_none() {
                if let Some(v) = &b.rhs[i] {
                    b.rhs_values[i] = Some(Box::new(v.to_ffi()));
                }
            }
            match b.rhs_values[i].as_mut() {
                Some(v) => {
                    value = v.as_mut() as *mut ExtValue as *mut c_void;
                    SQLITE_OK
                }
                None => SQLITE_NOTFOUND,
            }
        }
    };
    if !out.is_null() {
        *out = value;
    }
    rc
}

std::thread_local! {
    /// IN lists handed to the xFilter calls in progress: the address of the
    /// argument, its values, and the position of sqlite3_vtab_in_next.
    static IN_LISTS: RefCell<Vec<(usize, Vec<ExtValue>, usize)>> = const { RefCell::new(Vec::new()) };
}

unsafe fn in_list_value(val: *mut c_void, out: *mut *mut c_void, next: bool) -> c_int {
    if !out.is_null() {
        *out = std::ptr::null_mut();
    }
    if val.is_null() {
        return SQLITE_MISUSE;
    }
    IN_LISTS.with(|lists| {
        let mut lists = lists.borrow_mut();
        let Some((_, values, pos)) = lists
            .iter_mut()
            .rev()
            .find(|(addr, _, _)| *addr == val as usize)
        else {
            return SQLITE_ERROR;
        };
        *pos = if next { *pos + 1 } else { 0 };
        match values.get_mut(*pos) {
            Some(v) => {
                if !out.is_null() {
                    *out = v as *mut ExtValue as *mut c_void;
                }
                SQLITE_OK
            }
            None => SQLITE_DONE,
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_vtab_in_first(val: *mut c_void, out: *mut *mut c_void) -> c_int {
    in_list_value(val, out, false)
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_vtab_in_next(val: *mut c_void, out: *mut *mut c_void) -> c_int {
    in_list_value(val, out, true)
}

/// SQLITE_DBCONFIG_DEFENSIVE: shadow tables become read-only to ordinary SQL.
pub(crate) unsafe fn db_config_defensive(db: *mut sqlite3, value: c_int, out: *mut c_int) -> c_int {
    let Some(conn) = db_conn(db) else {
        return SQLITE_MISUSE;
    };
    if value >= 0 {
        conn.set_defensive(value > 0);
    }
    if !out.is_null() {
        *out = conn.is_defensive() as c_int;
    }
    SQLITE_OK
}
