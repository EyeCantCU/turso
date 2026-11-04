#[cfg(feature = "fs")]
mod dynamic;
mod function;
mod vtab;
mod vtab_xconnect;
use crate::index_method::backing_btree::BackingBtreeIndexMethod;
#[cfg(all(feature = "fts", not(target_family = "wasm")))]
use crate::index_method::fts::{FtsIndexMethod, FTS_INDEX_METHOD_NAME};
use crate::index_method::toy_vector_sparse_ivf::VectorSparseInvertedIndexMethod;
use crate::index_method::{
    BACKING_BTREE_INDEX_METHOD_NAME, TOY_VECTOR_SPARSE_IVF_INDEX_METHOD_NAME,
};
use crate::namespace::NamespaceId;
use crate::schema::{Schema, Table};
use crate::sync::atomic::{AtomicU64, Ordering};
use crate::sync::Mutex;
#[cfg(all(target_os = "linux", feature = "io_uring", not(miri)))]
use crate::UringIO;
#[cfg(all(target_os = "windows", feature = "experimental_win_iocp", not(miri)))]
use crate::WindowsIOCP;
pub(crate) use vtab::{
    create_virtual_table, ExtensionCursor, ExtensionTable, ModuleImplementation,
};

use crate::{function::ExternalFunc, Connection, Database};
use crate::{vtab::VirtualTable, SymbolTable};
#[cfg(feature = "fs")]
use crate::{LimboError, IO};
#[cfg(feature = "fs")]
pub use dynamic::{add_builtin_vfs_extensions, add_vfs_module, list_vfs_modules, VfsMod};
use std::{
    ffi::{c_char, c_void, CStr, CString},
    sync::Arc,
};
use turso_ext::{
    ContextDestructor, ExtensionApi, InitAggFunction, ResultCode, ScalarFunction, VTabKind,
    VTabModuleImpl, ValueDestructor,
};
pub use turso_ext::{FinalizeFunction, StepFunction, Value as ExtValue, ValueType as ExtValueType};
pub use vtab_xconnect::{execute, prepare_stmt};

/// The context passed to extensions to register with Core
/// along with the function pointers
#[repr(C)]
pub struct ExtensionCtx {
    syms: *mut SymbolTable,
    schema: *mut c_void,
    /// We must bump the prepare context generation so prepared statements
    /// know they need to be reprepared after extension registration.
    prepare_context_generation: *const AtomicU64,
}

pub(crate) unsafe extern "C" fn register_vtab_module(
    ctx: *mut c_void,
    name: *const c_char,
    module: VTabModuleImpl,
    kind: VTabKind,
) -> ResultCode {
    if name.is_null() || ctx.is_null() {
        return ResultCode::Error;
    }

    let c_str = unsafe { CString::from_raw(name as *mut c_char) };
    let name_str = match c_str.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return ResultCode::Error,
    };

    let ext_ctx = unsafe { &mut *(ctx as *mut ExtensionCtx) };
    let module = Arc::new(module);
    let vmodule = VTabImpl {
        module_kind: kind,
        implementation: ModuleImplementation::C(module),
        eponymous: None,
    };

    unsafe {
        let syms = &mut *ext_ctx.syms;
        syms.vtab_modules.insert(name_str.clone(), vmodule.into());
        if !ext_ctx.prepare_context_generation.is_null() {
            (*ext_ctx.prepare_context_generation).fetch_add(1, Ordering::Release);
        }

        if kind == VTabKind::TableValuedFunction {
            if let Ok(vtab) = VirtualTable::function(&name_str, syms) {
                let table = Arc::new(Table::Virtual(vtab));
                let mutex = &*(ext_ctx.schema as *mut Mutex<Arc<Schema>>);
                let mut guard = mutex.lock();
                let Ok(schema) = Schema::try_make_mut(&mut guard) else {
                    return ResultCode::Error;
                };
                schema.insert_table(NamespaceId::ROOT, name_str, table);
            } else {
                return ResultCode::Error;
            }
        }
    }
    ResultCode::OK
}

#[derive(Clone)]
pub struct VTabImpl {
    pub module_kind: VTabKind,
    pub(crate) implementation: ModuleImplementation,
    /// Set for modules with an eponymous table, which is connected on first use.
    pub(crate) eponymous: Option<Arc<EponymousSlot>>,
}

pub(crate) struct EponymousSlot {
    name: String,
    table: Mutex<Option<Arc<VirtualTable>>>,
}

impl SymbolTable {
    /// Looks up a module by name, case-insensitively.
    pub(crate) fn vtab_module(&self, name: &str) -> Option<&Arc<VTabImpl>> {
        self.vtab_modules
            .get(name)
            .or_else(|| self.vtab_modules.get(&crate::util::normalize_ident(name)))
    }

    /// The eponymous virtual table `name`, connecting it on first use.
    pub(crate) fn eponymous_table(&self, name: &str) -> crate::Result<Option<Arc<VirtualTable>>> {
        let Some(module) = self.vtab_module(name) else {
            return Ok(None);
        };
        let Some(slot) = &module.eponymous else {
            return Ok(None);
        };
        let mut table = slot.table.lock();
        if let Some(table) = table.as_ref() {
            return Ok(Some(table.clone()));
        }
        let vtab = VirtualTable::eponymous(&slot.name, module)?;
        *table = Some(vtab.clone());
        Ok(Some(vtab))
    }

    /// Finds a connected virtual table (created or eponymous) by its id.
    pub(crate) fn vtab_by_id(&self, id: u64) -> Option<Arc<VirtualTable>> {
        self.vtabs
            .values()
            .find(|vtab| vtab.id() == id)
            .cloned()
            .or_else(|| {
                self.vtab_modules.values().find_map(|module| {
                    let slot = module.eponymous.as_ref()?;
                    let table = slot.table.lock();
                    table.as_ref().filter(|t| t.id() == id).cloned()
                })
            })
    }
}

pub(crate) unsafe fn register_scalar_function(
    ctx: *mut c_void,
    name: *const c_char,
    func: ScalarFunction,
) -> ResultCode {
    unsafe { register_scalar_function_with_options(ctx, name, -1, false, 0, func, None, None) }
}

pub(crate) unsafe extern "C" fn register_scalar_function_with_options(
    ctx: *mut c_void,
    name: *const c_char,
    argc: i32,
    deterministic: bool,
    context: usize,
    callback: ScalarFunction,
    context_destructor: Option<ContextDestructor>,
    value_destructor: Option<ValueDestructor>,
) -> ResultCode {
    if ctx.is_null() || name.is_null() {
        return ResultCode::InvalidArgs;
    }
    let c_str = unsafe { CStr::from_ptr(name) };
    let name_str = match c_str.to_str() {
        Ok(s) => crate::util::normalize_ident(s),
        Err(_) => return ResultCode::InvalidArgs,
    };
    let function = match ExternalFunc::new_scalar(
        name_str.clone(),
        argc,
        deterministic,
        context,
        callback,
        context_destructor,
        value_destructor,
    ) {
        Ok(function) => function,
        Err(_) => return ResultCode::InvalidArgs,
    };
    let ext_ctx = unsafe { &mut *(ctx as *mut ExtensionCtx) };
    unsafe {
        (*ext_ctx.syms)
            .functions
            .insert(name_str, Arc::new(function));
        if !ext_ctx.prepare_context_generation.is_null() {
            (*ext_ctx.prepare_context_generation).fetch_add(1, Ordering::Release);
        }
    }
    ResultCode::OK
}

pub(crate) unsafe extern "C" fn unregister_function(
    ctx: *mut c_void,
    name: *const c_char,
) -> ResultCode {
    if ctx.is_null() || name.is_null() {
        return ResultCode::InvalidArgs;
    }
    let c_str = unsafe { CStr::from_ptr(name) };
    let name_str = match c_str.to_str() {
        Ok(s) => crate::util::normalize_ident(s),
        Err(_) => return ResultCode::InvalidArgs,
    };
    let ext_ctx = unsafe { &mut *(ctx as *mut ExtensionCtx) };
    unsafe {
        if (*ext_ctx.syms).functions.remove(&name_str).is_none() {
            return ResultCode::NotFound;
        }
        if !ext_ctx.prepare_context_generation.is_null() {
            (*ext_ctx.prepare_context_generation).fetch_add(1, Ordering::Release);
        }
    }
    ResultCode::OK
}

pub(crate) unsafe extern "C" fn register_aggregate_function(
    ctx: *mut c_void,
    name: *const c_char,
    args: i32,
    context: usize,
    init_func: InitAggFunction,
    step_func: StepFunction,
    finalize_func: FinalizeFunction,
    context_destructor: Option<ContextDestructor>,
    aggregate_destructor: Option<ContextDestructor>,
    value_destructor: Option<ValueDestructor>,
) -> ResultCode {
    if ctx.is_null() || name.is_null() {
        return ResultCode::InvalidArgs;
    }
    let c_str = unsafe { CStr::from_ptr(name) };
    let name_str = match c_str.to_str() {
        Ok(s) => crate::util::normalize_ident(s),
        Err(_) => return ResultCode::InvalidArgs,
    };
    let function = match ExternalFunc::new_aggregate(
        name_str.clone(),
        args,
        context,
        (init_func, step_func, finalize_func),
        context_destructor,
        aggregate_destructor,
        value_destructor,
    ) {
        Ok(function) => function,
        Err(_) => return ResultCode::InvalidArgs,
    };
    let ext_ctx = unsafe { &mut *(ctx as *mut ExtensionCtx) };
    unsafe {
        (*ext_ctx.syms)
            .functions
            .insert(name_str, Arc::new(function));
        if !ext_ctx.prepare_context_generation.is_null() {
            (*ext_ctx.prepare_context_generation).fetch_add(1, Ordering::Release);
        }
    }
    ResultCode::OK
}

impl Database {
    #[cfg(feature = "fs")]
    #[allow(clippy::arc_with_non_send_sync, dead_code)]
    pub fn open_with_vfs(
        &self,
        path: &str,
        vfs: &str,
        dialect: Arc<dyn crate::Dialect>,
    ) -> crate::Result<(Arc<dyn IO>, Arc<Database>)> {
        use crate::{MemoryIO, SyscallIO};
        use dynamic::get_vfs_modules;

        let io: Arc<dyn IO> = match vfs {
            "memory" => Arc::new(MemoryIO::new()),
            #[cfg(feature = "io_memory_yield")]
            "memory_yield" => Arc::new(crate::MemoryYieldIO::new()),
            "syscall" => Arc::new(SyscallIO::new()?),
            #[cfg(all(target_os = "linux", feature = "io_uring", not(miri)))]
            "io_uring" => Arc::new(UringIO::new()?),
            #[cfg(all(target_os = "windows", feature = "experimental_win_iocp", not(miri)))]
            "experimental_win_iocp" => Arc::new(WindowsIOCP::new()?),
            other => match get_vfs_modules().iter().find(|v| v.0 == vfs) {
                Some((_, vfs)) => vfs.clone(),
                None => {
                    return Err(LimboError::InvalidArgument(format!("no such VFS: {other}")));
                }
            },
        };
        let db = Self::open_file(io.clone(), path, dialect)?;
        Ok((io, db))
    }

    /// Register any built-in extensions that can be stored on the Database so we do not have
    /// to register these once-per-connection, and the connection can just extend its symbol table
    pub fn register_global_builtin_extensions(&self) -> Result<(), String> {
        {
            let mut syms = self.builtin_syms.write();
            syms.index_methods.insert(
                TOY_VECTOR_SPARSE_IVF_INDEX_METHOD_NAME.to_string(),
                Arc::new(VectorSparseInvertedIndexMethod),
            );
            syms.index_methods.insert(
                BACKING_BTREE_INDEX_METHOD_NAME.to_string(),
                Arc::new(BackingBtreeIndexMethod),
            );
            #[cfg(all(feature = "fts", not(target_family = "wasm")))]
            syms.index_methods
                .insert(FTS_INDEX_METHOD_NAME.to_string(), Arc::new(FtsIndexMethod));
            #[cfg(feature = "fts3")]
            crate::fts3::register(&mut syms).map_err(|e| e.to_string())?;
        }
        let syms = self.builtin_syms.data_ptr();
        // Pass the mutex pointer and the appropriate handler
        let schema_mutex_ptr =
            &*self.schema as *const Mutex<Arc<Schema>> as *mut Mutex<Arc<Schema>>;
        let ctx = Box::into_raw(Box::new(ExtensionCtx {
            syms,
            schema: schema_mutex_ptr as *mut c_void,
            prepare_context_generation: std::ptr::null(),
        }));
        #[allow(unused)]
        let mut ext_api = ExtensionApi {
            ctx: ctx as *mut c_void,
            register_scalar_function: register_scalar_function_with_options,
            register_aggregate_function,
            unregister_function,
            register_vtab_module,
            #[cfg(feature = "fs")]
            vfs_interface: turso_ext::VfsInterface {
                register_vfs: dynamic::register_vfs,
                builtin_vfs: std::ptr::null_mut(),
                builtin_vfs_count: 0,
            },
        };

        #[cfg(feature = "uuid")]
        crate::uuid::register_extension(&mut ext_api);
        #[cfg(feature = "series")]
        crate::series::register_extension(&mut ext_api);
        #[cfg(feature = "time")]
        crate::time::register_extension(&mut ext_api);
        #[cfg(feature = "percentile")]
        crate::percentile::register_extension(&mut ext_api);
        crate::regexp::register_extension(&mut ext_api);
        crate::rtree::register(&mut self.builtin_syms.write()).map_err(|e| e.to_string())?;
        #[cfg(feature = "fs")]
        {
            let vfslist = add_builtin_vfs_extensions(Some(ext_api)).map_err(|e| e.to_string())?;
            for (name, vfs) in vfslist {
                add_vfs_module(name, vfs);
            }
        }
        let _ = unsafe { Box::from_raw(ctx) };
        Ok(())
    }
}

impl Connection {
    /// Registers a virtual table module on this connection only, as
    /// `sqlite3_create_module` does. `creatable` modules back
    /// `CREATE VIRTUAL TABLE`; `eponymous` ones also get a table named after
    /// the module. Stored tables that use the module are connected.
    pub fn register_vtab_module_local<M>(
        self: &Arc<Connection>,
        name: &str,
        module: M,
        creatable: bool,
        eponymous: bool,
    ) -> crate::Result<()>
    where
        M: crate::native_ext::VirtualTableModule + 'static,
    {
        let module = VTabImpl {
            module_kind: if creatable {
                VTabKind::VirtualTable
            } else {
                VTabKind::TableValuedFunction
            },
            implementation: ModuleImplementation::Native(Arc::new(module)),
            eponymous: eponymous.then(|| {
                Arc::new(EponymousSlot {
                    name: name.to_string(),
                    table: Mutex::new(None),
                })
            }),
        };
        let key = crate::util::normalize_ident(name);
        {
            let mut syms = self.syms.write();
            syms.vtab_modules.remove(name);
            syms.vtab_modules.insert(key, Arc::new(module));
        }
        self.bump_prepare_context_generation();
        if creatable && self.is_db_initialized() {
            if let Err(e) = self.connect_stored_vtabs(name) {
                tracing::debug!("connecting stored tables of vtab module {name}: {e}");
            }
        }
        Ok(())
    }

    /// Connects the tables in sqlite_schema that use `module_name` and are
    /// not loaded yet. No lock is held while the module's constructor runs,
    /// since it may query this connection.
    fn connect_stored_vtabs(self: &Arc<Connection>, module_name: &str) -> crate::Result<()> {
        let mut rows: Vec<(String, String)> = Vec::new();
        {
            let mut stmt = self
                .query("SELECT name, sql FROM sqlite_schema WHERE type = 'table' AND rootpage = 0")?
                .ok_or_else(|| crate::LimboError::InternalError("no statement".to_string()))?;
            stmt.run_with_row_callback(|row| {
                if let (Ok(name), Ok(sql)) = (row.get::<&str>(0), row.get::<&str>(1)) {
                    rows.push((name.to_string(), sql.to_string()));
                }
                Ok(())
            })?;
        }
        for (name, sql) in rows {
            let Ok(Some(turso_parser::ast::Cmd::Stmt(
                turso_parser::ast::Stmt::CreateVirtualTable(create),
            ))) = turso_parser::parser::Parser::new(sql.as_bytes()).next_cmd()
            else {
                continue;
            };
            if !create
                .module_name
                .as_str()
                .eq_ignore_ascii_case(module_name)
                || self
                    .schema
                    .read()
                    .get_table(NamespaceId::ROOT, &name)
                    .is_some()
            {
                continue;
            }
            let Some(module) = self.syms.read().vtab_module(module_name).cloned() else {
                return Ok(());
            };
            let args = create
                .args
                .iter()
                .map(|a| crate::Value::from_text(a.to_string()))
                .collect();
            match VirtualTable::from_module(&name, create.module_name.as_str(), args, &module) {
                Ok(vtab) => {
                    self.with_schema_mut(|schema| {
                        schema.add_virtual_table(NamespaceId::ROOT, vtab)
                    })??;
                }
                Err(e) => tracing::debug!("connecting virtual table {name}: {e}"),
            }
        }
        self.bump_prepare_context_generation();
        Ok(())
    }

    /// Finds a connected virtual table by id: one this connection created,
    /// an eponymous one, or one loaded from the schema.
    pub(crate) fn vtab_by_id(&self, id: u64) -> Option<Arc<VirtualTable>> {
        if let Some(vtab) = self.syms.read().vtab_by_id(id) {
            return Some(vtab);
        }
        self.schema
            .read()
            .all_tables()
            .find_map(|table| match table.as_ref() {
                Table::Virtual(vtab) if vtab.id() == id => Some(vtab.clone()),
                _ => None,
            })
    }

    /// Removes a module registered with [`Self::register_vtab_module_local`].
    pub fn unregister_vtab_module_local(&self, name: &str) {
        let mut syms = self.syms.write();
        syms.vtab_modules.remove(name);
        syms.vtab_modules
            .remove(&crate::util::normalize_ident(name));
        drop(syms);
        self.bump_prepare_context_generation();
    }

    /// Register statically linked functions or virtual tables against this
    /// connection using the generic extension API.
    pub fn register_static_extension<F>(&self, register: F)
    where
        F: FnOnce(&mut ExtensionApi),
    {
        unsafe {
            let mut ext_api = self._build_turso_ext();
            register(&mut ext_api);
            self._free_extension_ctx(ext_api);
        }
    }

    /// Build the connection's extension api context for manually registering an extension.
    /// you probably want to use `Connection::load_extension(path)`.
    ///
    /// # Safety
    /// Only to be used when registering a staticly linked extension manually.
    /// You should only ever call this method on your applications startup,
    /// The caller is responsible for calling `_free_extension_ctx` after registering the
    /// extension.
    ///
    /// usage:
    /// ```ignore
    /// let ext_api = conn._build_turso_ext();
    /// unsafe {
    ///     my_extension::register_extension(&mut ext_api);
    ///     conn._free_extension_ctx(ext_api);
    /// }
    ///```
    pub unsafe fn _build_turso_ext(&self) -> ExtensionApi {
        let schema_mutex_ptr =
            &*self.db.schema as *const Mutex<Arc<Schema>> as *mut Mutex<Arc<Schema>>;
        let ctx = ExtensionCtx {
            syms: self.syms.data_ptr(),
            schema: schema_mutex_ptr as *mut c_void,
            prepare_context_generation: &self.prepare_context_generation as *const _,
        };
        let ctx = Box::into_raw(Box::new(ctx)) as *mut c_void;
        ExtensionApi {
            ctx,
            register_scalar_function: register_scalar_function_with_options,
            register_aggregate_function,
            unregister_function,
            register_vtab_module,
            #[cfg(feature = "fs")]
            vfs_interface: turso_ext::VfsInterface {
                register_vfs: dynamic::register_vfs,
                builtin_vfs: std::ptr::null_mut(),
                builtin_vfs_count: 0,
            },
        }
    }

    /// Free the connection's extension libary context after registering an extension manually.
    /// # Safety
    /// Only to be used if you have previously called Connection::build_turso_ext
    pub unsafe fn _free_extension_ctx(&self, api: ExtensionApi) {
        if api.ctx.is_null() {
            return;
        }
        let _ = unsafe { Box::from_raw(api.ctx as *mut ExtensionCtx) };
    }
}
