use crate::pragma::{PragmaVirtualTable, PragmaVirtualTableCursor};
use crate::schema::Column;
use crate::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use crate::sync::{Arc, RwLock, Weak};
use crate::types::IOResultOr;
use crate::util::columns_from_create_table_body;
use crate::{Connection, IOResult, LimboError, SymbolTable, Value};
use std::ffi::c_void;
use std::ptr::NonNull;
use turso_ext::{ConstraintInfo, IndexInfo, OrderByInfo, ResultCode, VTabKind, VTabModuleImpl};
use turso_parser::{ast, parser::Parser};

std::thread_local! {
    /// Statements executing on this thread (SQLite's `nVdbeExec`).
    static EXECUTING: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Counts a statement as executing while it steps.
pub(crate) struct ExecutingGuard;

impl ExecutingGuard {
    pub(crate) fn new() -> Self {
        EXECUTING.with(|n| n.set(n.get() + 1));
        Self
    }
}

impl Drop for ExecutingGuard {
    fn drop(&mut self) {
        EXECUTING.with(|n| n.set(n.get().saturating_sub(1)));
    }
}

/// `sqlite3ReadOnlyShadowTables`: defensive mode, outside any running
/// statement (virtual table methods run inside one and may write them).
pub(crate) fn readonly_shadow_tables(conn: &Connection) -> bool {
    conn.is_defensive() && EXECUTING.with(|n| n.get()) == 0
}

/// `sqlite3ShadowTableName`: `name` is `<vtab>_<suffix>` and the virtual
/// table's module claims `suffix` through xShadowName.
pub(crate) fn is_shadow_table_name(schema: &crate::schema::Schema, name: &str) -> bool {
    let Some(i) = name.rfind('_') else {
        return false;
    };
    match schema.get_table(crate::namespace::NamespaceId::ROOT, &name[..i]) {
        Some(table) => match table.as_ref() {
            crate::schema::Table::Virtual(vtab) => vtab.is_shadow_name(&name[i + 1..]),
            _ => false,
        },
        None => false,
    }
}

#[derive(Debug, Clone)]
pub(crate) enum VirtualTableType {
    Pragma(PragmaVirtualTable),
    External(crate::ext::ExtensionTable),
    Internal(Arc<RwLock<dyn InternalVirtualTable>>),
}

#[derive(Clone, Debug)]
pub struct VirtualTable {
    pub(crate) name: String,
    pub(crate) columns: Vec<Column>,
    pub(crate) kind: VTabKind,
    pub(crate) vtab_type: VirtualTableType,
    // identifier to tie a cursor to a specific instantiated virtual table instance
    pub(crate) vtab_id: u64,
    /// Whether `DROP TABLE` may remove this table from its schema.
    pub(crate) is_droppable: bool,
    // Whether this virtual table is safe to use from within triggers and views.
    // Corresponds to SQLite's SQLITE_VTAB_INNOCUOUS flag.
    pub(crate) innocuous: bool,
}

impl VirtualTable {
    pub(crate) fn id(&self) -> u64 {
        self.vtab_id
    }
    pub(crate) fn commits_after_create(&self) -> bool {
        matches!(
            &self.vtab_type,
            VirtualTableType::External(crate::ext::ExtensionTable::Native(table))
                if table.commits_after_create()
        )
    }

    pub(crate) fn readonly(&self) -> bool {
        match &self.vtab_type {
            VirtualTableType::Pragma(_) => true,
            VirtualTableType::External(table) => table.readonly(),
            VirtualTableType::Internal(_) => true,
        }
    }

    /// Wrap an `InternalVirtualTable` implementation so it can appear in a
    /// `Schema`'s catalog. The table's `name()` becomes the catalog name and
    /// its `sql()` is parsed to derive the column metadata. Returns an error
    /// if the SQL string is not a valid `CREATE TABLE` statement.
    pub(crate) fn wrap_internal_table<T>(table: T) -> crate::Result<Arc<VirtualTable>>
    where
        T: InternalVirtualTable + 'static,
    {
        let name = table.name();
        let sql = table.sql();
        Ok(Arc::new(Self::new_internal(
            name,
            sql,
            VTabKind::TableValuedFunction,
            Arc::new(RwLock::new(table)),
        )?))
    }

    /// Build a virtual table around an [`InternalVirtualTable`]
    /// implementation. `sql` must be a `CREATE TABLE` statement; it is
    /// parsed to derive the column metadata. Schema dialects use this to
    /// construct catalog tables (e.g. a `pg_catalog` surface) with a
    /// non-table-valued-function kind and install them through
    /// [`crate::dialect::Dialect::register_catalog`].
    pub fn new_internal(
        name: String,
        sql: String,
        kind: VTabKind,
        table: Arc<RwLock<dyn InternalVirtualTable>>,
    ) -> crate::Result<Self> {
        Ok(VirtualTable {
            name,
            columns: Self::resolve_columns(sql)?,
            kind,
            vtab_type: VirtualTableType::Internal(table),
            vtab_id: 0,
            is_droppable: false,
            innocuous: true,
        })
    }

    pub(crate) fn function(name: &str, syms: &SymbolTable) -> crate::Result<Arc<VirtualTable>> {
        let module = syms.vtab_module(name);
        let info = crate::native_ext::ConnectInfo {
            module_name: name,
            schema_name: "main",
            table_name: name,
            create: false,
            connection: None,
        };
        let (vtab_type, schema) = if module.is_some() {
            crate::ext::create_virtual_table(
                &info,
                module,
                Vec::new(),
                VTabKind::TableValuedFunction,
            )?
        } else {
            return Err(LimboError::ParseError(format!(
                "No such table-valued function: {name}"
            )));
        };

        let vtab = VirtualTable {
            name: name.to_owned(),
            columns: Self::resolve_columns(schema)?,
            kind: VTabKind::TableValuedFunction,
            vtab_type,
            vtab_id: 0,
            is_droppable: false,
            innocuous: module.is_some_and(|module| module.implementation.innocuous()),
        };
        Ok(Arc::new(vtab))
    }

    pub fn table(
        tbl_name: Option<&str>,
        module_name: &str,
        args: Vec<Value>,
        syms: &SymbolTable,
    ) -> crate::Result<Arc<VirtualTable>> {
        Self::table_for(
            tbl_name,
            module_name,
            args,
            syms.vtab_module(module_name),
            None,
        )
    }

    /// Like [`Self::table`]; `create` is the connection running
    /// `CREATE VIRTUAL TABLE`, if that is the caller.
    pub(crate) fn table_for(
        tbl_name: Option<&str>,
        module_name: &str,
        args: Vec<Value>,
        module: Option<&Arc<crate::ext::VTabImpl>>,
        create: Option<&Arc<Connection>>,
    ) -> crate::Result<Arc<VirtualTable>> {
        Self::table_in(tbl_name, module_name, "main", args, module, create)
    }

    /// Like [`Self::table_for`], for a table in schema `schema_name`.
    pub(crate) fn table_in(
        tbl_name: Option<&str>,
        module_name: &str,
        schema_name: &str,
        args: Vec<Value>,
        module: Option<&Arc<crate::ext::VTabImpl>>,
        create: Option<&Arc<Connection>>,
    ) -> crate::Result<Arc<VirtualTable>> {
        let info = crate::native_ext::ConnectInfo {
            module_name,
            schema_name,
            table_name: tbl_name.unwrap_or(module_name),
            create: create.is_some(),
            connection: create,
        };
        let (vtab_type, schema) =
            crate::ext::create_virtual_table(&info, module, args, VTabKind::VirtualTable)?;
        let vtab = VirtualTable {
            name: tbl_name.unwrap_or(module_name).to_owned(),
            columns: Self::resolve_columns(schema)?,
            kind: VTabKind::VirtualTable,
            vtab_type,
            vtab_id: VTAB_ID_COUNTER.fetch_add(1, Ordering::Acquire),
            is_droppable: true,
            innocuous: module.is_some_and(|module| module.implementation.innocuous()),
        };
        Ok(Arc::new(vtab))
    }

    /// Connects a stored table of `module` without consulting the symbol table.
    pub(crate) fn from_module(
        tbl_name: &str,
        module_name: &str,
        args: Vec<Value>,
        module: &Arc<crate::ext::VTabImpl>,
    ) -> crate::Result<Arc<VirtualTable>> {
        let info = crate::native_ext::ConnectInfo {
            module_name,
            schema_name: "main",
            table_name: tbl_name,
            create: false,
            connection: None,
        };
        let (vtab_type, schema) =
            crate::ext::create_virtual_table(&info, Some(module), args, VTabKind::VirtualTable)?;
        Ok(Arc::new(VirtualTable {
            name: tbl_name.to_owned(),
            columns: Self::resolve_columns(schema)?,
            kind: VTabKind::VirtualTable,
            vtab_type,
            vtab_id: VTAB_ID_COUNTER.fetch_add(1, Ordering::Acquire),
            is_droppable: true,
            innocuous: module.implementation.innocuous(),
        }))
    }

    /// Connects the eponymous table of `module` (named after the module).
    pub(crate) fn eponymous(
        name: &str,
        module: &crate::ext::VTabImpl,
    ) -> crate::Result<Arc<VirtualTable>> {
        let info = crate::native_ext::ConnectInfo {
            module_name: name,
            schema_name: "main",
            table_name: name,
            create: false,
            connection: None,
        };
        let (vtab_type, schema) = module.implementation.create(&info, Vec::new())?;
        Ok(Arc::new(VirtualTable {
            name: name.to_owned(),
            columns: Self::resolve_columns(schema)?,
            kind: VTabKind::TableValuedFunction,
            vtab_type,
            vtab_id: VTAB_ID_COUNTER.fetch_add(1, Ordering::Acquire),
            is_droppable: false,
            innocuous: module.implementation.innocuous(),
        }))
    }

    pub(crate) fn resolve_columns(schema: String) -> crate::Result<Vec<Column>> {
        let mut parser = Parser::new(schema.as_bytes());
        if let ast::Cmd::Stmt(ast::Stmt::CreateTable { body, .. }) =
            parser.next_cmd()?.ok_or_else(|| {
                LimboError::ParseError(
                    "Failed to parse schema from virtual table module".to_string(),
                )
            })?
        {
            let mut columns = columns_from_create_table_body(&body)?;
            // Like SQLite, HIDDEN marks the column and is not part of its declared type.
            for column in columns.iter_mut().filter(|column| column.hidden()) {
                column.ty_str = column
                    .ty_str
                    .split(' ')
                    .filter(|word| !word.eq_ignore_ascii_case("HIDDEN"))
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            Ok(columns)
        } else {
            Err(LimboError::ParseError(
                "Failed to parse schema from virtual table module".to_string(),
            ))
        }
    }

    pub(crate) fn open(&self, conn: Arc<Connection>) -> crate::Result<VirtualTableCursor> {
        match &self.vtab_type {
            VirtualTableType::Pragma(table) => {
                Ok(VirtualTableCursor::new_pragma(table.open(conn)?))
            }
            VirtualTableType::External(table) => Ok(VirtualTableCursor::new_external(
                table.open(conn, self.vtab_id)?,
            )),
            VirtualTableType::Internal(table) => {
                Ok(VirtualTableCursor::new_internal(table.read().open(conn)?))
            }
        }
    }

    pub(crate) fn update(
        &self,
        connection: &Arc<Connection>,
        args: &[Value],
        state: &mut crate::native_ext::ExtensionState,
        on_conflict: u8,
    ) -> IOResultOr<Option<i64>> {
        match &self.vtab_type {
            VirtualTableType::Pragma(_) => Err(LimboError::ReadOnly.into()),
            VirtualTableType::External(table) => table.update(connection, args, state, on_conflict),
            VirtualTableType::Internal(_) => Err(LimboError::ReadOnly.into()),
        }
    }

    pub(crate) fn destroy(&self, connection: &Arc<Connection>) -> crate::Result<()> {
        match &self.vtab_type {
            VirtualTableType::Pragma(_) => Ok(()),
            VirtualTableType::External(table) => table.destroy(connection),
            VirtualTableType::Internal(_) => Ok(()),
        }
    }

    /// SQLite's full xBestIndex; tables without one go through `best_index`.
    pub(crate) fn best_index_plan(
        &self,
        req: &crate::native_ext::IndexRequest,
    ) -> crate::Result<Option<crate::native_ext::IndexPlan>> {
        match &self.vtab_type {
            VirtualTableType::Pragma(table) => {
                crate::native_ext::best_index_via_legacy(req, |c, _| table.best_index(c))
            }
            VirtualTableType::External(table) => table.best_index_plan(req),
            VirtualTableType::Internal(table) => {
                crate::native_ext::best_index_via_legacy(req, |c, o| table.read().best_index(c, o))
            }
        }
    }

    /// xFindFunction for a function whose first argument is a column of this table.
    pub(crate) fn find_function(
        &self,
        n_arg: i32,
        name: &str,
    ) -> Option<(i32, Arc<crate::function::ExternalFunc>)> {
        match &self.vtab_type {
            VirtualTableType::External(table) => table.find_function(n_arg, name),
            _ => None,
        }
    }

    /// xIntegrity: an error description, or None when the table is fine.
    pub(crate) fn integrity(
        &self,
        connection: &Arc<Connection>,
        schema: &str,
        table: &str,
        quick: bool,
    ) -> crate::Result<Option<String>> {
        match &self.vtab_type {
            VirtualTableType::External(crate::ext::ExtensionTable::Native(t)) => {
                t.integrity(connection, schema, table, quick)
            }
            _ => Ok(None),
        }
    }

    /// Whether PRAGMA integrity_check calls this table's xIntegrity.
    pub(crate) fn has_integrity(&self) -> bool {
        matches!(&self.vtab_type,
            VirtualTableType::External(crate::ext::ExtensionTable::Native(t)) if t.has_integrity())
    }

    /// xShadowName of the table's module.
    pub(crate) fn is_shadow_name(&self, suffix: &str) -> bool {
        match &self.vtab_type {
            VirtualTableType::External(crate::ext::ExtensionTable::Native(t)) => {
                t.is_shadow_name(suffix)
            }
            _ => false,
        }
    }

    pub(crate) fn find_function_op(&self, n_arg: i32, name: &str) -> Option<i32> {
        self.find_function(n_arg, name).map(|(op, _)| op)
    }

    pub(crate) fn begin(&self) -> crate::Result<()> {
        match &self.vtab_type {
            VirtualTableType::Pragma(_) => Err(LimboError::ExtensionError(
                "Pragma virtual tables do not support transactions".to_string(),
            )),
            VirtualTableType::External(table) => table.begin(),
            VirtualTableType::Internal(_) => Err(LimboError::ExtensionError(
                "Internal virtual tables currently do not support transactions".to_string(),
            )),
        }
    }

    pub(crate) fn commit(&self) -> crate::Result<()> {
        match &self.vtab_type {
            VirtualTableType::Pragma(_) => Err(LimboError::ExtensionError(
                "Pragma virtual tables do not support transactions".to_string(),
            )),
            VirtualTableType::External(table) => table.commit(),
            VirtualTableType::Internal(_) => Err(LimboError::ExtensionError(
                "Internal virtual tables currently do not support transactions".to_string(),
            )),
        }
    }

    pub(crate) fn rollback(&self) -> crate::Result<()> {
        match &self.vtab_type {
            VirtualTableType::Pragma(_) => Err(LimboError::ExtensionError(
                "Pragma virtual tables do not support transactions".to_string(),
            )),
            VirtualTableType::External(table) => table.rollback(),
            VirtualTableType::Internal(_) => Err(LimboError::ExtensionError(
                "Internal virtual tables currently do not support transactions".to_string(),
            )),
        }
    }

    /// Runs a savepoint hook (0 = savepoint, 1 = release, 2 = rollback to).
    pub(crate) fn savepoint_op(&self, op: u8, level: usize) -> crate::Result<()> {
        match &self.vtab_type {
            VirtualTableType::External(table) => table.savepoint_op(op, level),
            _ => Ok(()),
        }
    }

    pub(crate) fn rename(&self, connection: &Arc<Connection>, new_name: &str) -> crate::Result<()> {
        match &self.vtab_type {
            VirtualTableType::Pragma(_) => Err(LimboError::ExtensionError(
                "Pragma virtual tables do not support renaming".to_string(),
            )),
            VirtualTableType::External(table) => table.rename(connection, new_name),
            VirtualTableType::Internal(_) => Err(LimboError::ExtensionError(
                "Internal virtual tables currently do not support renaming".to_string(),
            )),
        }
    }
}

enum VirtualTableCursorInner {
    Pragma(Box<PragmaVirtualTableCursor>),
    External(crate::ext::ExtensionCursor),
    Internal(Arc<RwLock<dyn InternalVirtualTableCursor>>),
}

pub struct VirtualTableCursor {
    inner: VirtualTableCursorInner,
    null_flag: bool,
}

crate::assert::assert_send_sync!(VirtualTableCursor);

impl VirtualTableCursor {
    pub(crate) fn new_pragma(cursor: PragmaVirtualTableCursor) -> Self {
        Self {
            inner: VirtualTableCursorInner::Pragma(Box::new(cursor)),
            null_flag: false,
        }
    }

    pub(crate) fn new_external(cursor: crate::ext::ExtensionCursor) -> Self {
        Self {
            inner: VirtualTableCursorInner::External(cursor),
            null_flag: false,
        }
    }

    pub(crate) fn new_internal(cursor: Arc<RwLock<dyn InternalVirtualTableCursor>>) -> Self {
        Self {
            inner: VirtualTableCursorInner::Internal(cursor),
            null_flag: false,
        }
    }

    pub(crate) fn set_null_flag(&mut self, flag: bool) {
        self.null_flag = flag;
    }

    pub(crate) fn get_null_flag(&self) -> bool {
        self.null_flag
    }

    pub(crate) fn needs_close_at_done(&self) -> bool {
        matches!(&self.inner, VirtualTableCursorInner::External(cursor) if cursor.needs_close_at_done())
    }

    pub(crate) fn next(&mut self) -> IOResultOr<bool> {
        self.null_flag = false;
        let result = match &mut self.inner {
            VirtualTableCursorInner::Pragma(cursor) => cursor.next(),
            VirtualTableCursorInner::External(cursor) => return cursor.next(),
            VirtualTableCursorInner::Internal(cursor) => cursor.write().next(),
        };
        result.map(IOResult::Done).map_err(Into::into)
    }

    pub(crate) fn rowid(&self) -> Option<i64> {
        if self.null_flag {
            return None;
        }
        Some(match &self.inner {
            VirtualTableCursorInner::Pragma(cursor) => cursor.rowid(),
            VirtualTableCursorInner::External(cursor) => cursor.rowid(),
            VirtualTableCursorInner::Internal(cursor) => cursor.read().rowid(),
        })
    }

    pub(crate) fn column(&mut self, column: usize) -> IOResultOr<Value> {
        if self.null_flag {
            return Ok(IOResult::Done(Value::Null));
        }
        let result = match &mut self.inner {
            VirtualTableCursorInner::Pragma(cursor) => cursor.column(column),
            VirtualTableCursorInner::External(cursor) => return cursor.column(column),
            VirtualTableCursorInner::Internal(cursor) => cursor.read().column(column),
        };
        result.map(IOResult::Done).map_err(Into::into)
    }

    /// `column` for an UPDATE that passes the column through unchanged.
    pub(crate) fn column_nochange(&mut self, column: usize) -> IOResultOr<Value> {
        match &mut self.inner {
            VirtualTableCursorInner::External(cursor) if !self.null_flag => {
                cursor.column_nochange(column)
            }
            _ => self.column(column),
        }
    }

    pub(crate) fn filter(
        &mut self,
        idx_num: i32,
        idx_str: Option<String>,
        arg_count: usize,
        args: crate::alloc::Vec<Value>,
    ) -> IOResultOr<bool> {
        self.null_flag = false;
        let result = match &mut self.inner {
            VirtualTableCursorInner::Pragma(cursor) => cursor.filter(args),
            VirtualTableCursorInner::External(cursor) => {
                return cursor.filter(idx_num, idx_str, arg_count, args);
            }
            VirtualTableCursorInner::Internal(cursor) => {
                cursor.write().filter(&args, idx_str, idx_num)
            }
        };
        result.map(IOResult::Done).map_err(Into::into)
    }

    /// `filter` where some arguments are IN lists handled all at once.
    pub(crate) fn filter_in(
        &mut self,
        idx_num: i32,
        idx_str: Option<String>,
        args: crate::alloc::Vec<Value>,
        in_lists: &[Option<Vec<Value>>],
    ) -> IOResultOr<bool> {
        match &mut self.inner {
            VirtualTableCursorInner::External(cursor) => {
                self.null_flag = false;
                cursor.filter_in(idx_num, idx_str, args, in_lists)
            }
            _ => {
                let n = args.len();
                self.filter(idx_num, idx_str, n, args)
            }
        }
    }

    pub(crate) fn vtab_id(&self) -> Option<u64> {
        match &self.inner {
            VirtualTableCursorInner::Pragma(_) => None,
            VirtualTableCursorInner::External(cursor) => Some(cursor.vtab_id()),
            VirtualTableCursorInner::Internal(_) => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ExtVirtualTable {
    implementation: Arc<VTabModuleImpl>,
    table_ptr: AtomicPtr<c_void>,
}
static VTAB_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

impl Clone for ExtVirtualTable {
    fn clone(&self) -> Self {
        Self {
            implementation: self.implementation.clone(),
            table_ptr: AtomicPtr::new(self.table_ptr.load(Ordering::SeqCst)),
        }
    }
}

impl ExtVirtualTable {
    pub(crate) fn readonly(&self) -> bool {
        self.implementation.readonly
    }
    pub(crate) fn best_index(
        &self,
        constraints: &[ConstraintInfo],
        order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        unsafe {
            IndexInfo::from_ffi((self.implementation.best_idx)(
                constraints.as_ptr(),
                constraints.len() as i32,
                order_by.as_ptr(),
                order_by.len() as i32,
            ))
        }
    }

    /// takes ownership of the provided Args
    pub(crate) fn create(
        implementation: Arc<VTabModuleImpl>,
        args: Vec<Value>,
    ) -> crate::Result<(Self, String)> {
        let args = args.iter().map(Value::to_ffi).collect();
        let (schema, table_ptr) = implementation.create(args)?;
        let vtab = ExtVirtualTable {
            implementation,
            table_ptr: AtomicPtr::new(table_ptr as *mut c_void),
        };
        Ok((vtab, schema))
    }

    /// Accepts a pointer connection that owns the VTable, that the module
    /// can optionally use to query the other tables.
    pub(crate) fn open(
        &self,
        conn: Arc<Connection>,
        id: u64,
    ) -> crate::Result<ExtVirtualTableCursor> {
        // we need a Weak<Connection> to upgrade and call from the extension.
        let weak = Arc::downgrade(&conn);
        let weak_box = Box::into_raw(Box::new(weak));
        let conn = turso_ext::Conn::new(
            weak_box as *mut c_void,
            crate::ext::prepare_stmt,
            crate::ext::execute,
        );
        let ext_conn_ptr = NonNull::new(Box::into_raw(Box::new(conn))).expect("null pointer");
        // store the leaked connection pointer on the table so it can be freed on drop
        let Some(cursor) = NonNull::new(unsafe {
            (self.implementation.open)(
                self.table_ptr.load(Ordering::SeqCst) as *const c_void,
                ext_conn_ptr.as_ptr(),
            ) as *mut c_void
        }) else {
            return Err(LimboError::ExtensionError("Open returned null".to_string()));
        };
        ExtVirtualTableCursor::new(cursor, ext_conn_ptr, self.implementation.clone(), id)
    }

    pub(crate) fn update(&self, args: &[Value]) -> crate::Result<Option<i64>> {
        let arg_count = args.len();
        let ext_args = args.iter().map(|arg| arg.to_ffi()).collect::<Vec<_>>();
        let newrowid = 0i64;
        let rc = unsafe {
            (self.implementation.update)(
                self.table_ptr.load(Ordering::SeqCst) as *const c_void,
                arg_count as i32,
                ext_args.as_ptr(),
                &newrowid as *const _ as *mut i64,
            )
        };
        for arg in ext_args {
            unsafe {
                arg.__free_internal_type();
            }
        }
        match rc {
            ResultCode::OK => Ok(None),
            ResultCode::RowID => Ok(Some(newrowid)),
            _ => Err(LimboError::ExtensionError(rc.to_string())),
        }
    }

    pub(crate) fn destroy(&self) -> crate::Result<()> {
        let rc = unsafe {
            (self.implementation.destroy)(self.table_ptr.load(Ordering::SeqCst) as *const c_void)
        };
        match rc {
            ResultCode::OK => Ok(()),
            _ => Err(LimboError::ExtensionError(rc.to_string())),
        }
    }

    pub(crate) fn commit(&self) -> crate::Result<()> {
        let rc = unsafe { (self.implementation.commit)(self.table_ptr.load(Ordering::SeqCst)) };
        match rc {
            ResultCode::OK => Ok(()),
            _ => Err(LimboError::ExtensionError("Commit failed".to_string())),
        }
    }

    pub(crate) fn begin(&self) -> crate::Result<()> {
        let rc = unsafe { (self.implementation.begin)(self.table_ptr.load(Ordering::SeqCst)) };
        match rc {
            ResultCode::OK => Ok(()),
            _ => Err(LimboError::ExtensionError("Begin failed".to_string())),
        }
    }

    pub(crate) fn rollback(&self) -> crate::Result<()> {
        let rc = unsafe { (self.implementation.rollback)(self.table_ptr.load(Ordering::SeqCst)) };
        match rc {
            ResultCode::OK => Ok(()),
            _ => Err(LimboError::ExtensionError("Rollback failed".to_string())),
        }
    }

    pub(crate) fn rename(&self, new_name: &str) -> crate::Result<()> {
        let c_new_name = std::ffi::CString::new(new_name).unwrap();
        let rc = unsafe {
            (self.implementation.rename)(self.table_ptr.load(Ordering::SeqCst), c_new_name.as_ptr())
        };
        match rc {
            ResultCode::OK => Ok(()),
            _ => Err(LimboError::ExtensionError("Rename failed".to_string())),
        }
    }
}

pub struct ExtVirtualTableCursor {
    cursor: NonNull<c_void>,
    // the core `[Connection]` pointer the vtab module needs to
    // query other internal tables.
    conn_ptr: Option<NonNull<turso_ext::Conn>>,
    implementation: Arc<VTabModuleImpl>,
    pub(crate) vtab_id: u64,
}

// SAFETY: Extension provider must guarantee Send + Sync on their side
// we cannot properly infer Send + Sync for dynamic libraries
unsafe impl Send for ExtVirtualTableCursor {}
unsafe impl Sync for ExtVirtualTableCursor {}
crate::assert::assert_send_sync!(ExtVirtualTableCursor);

impl ExtVirtualTableCursor {
    fn new(
        cursor: NonNull<c_void>,
        conn_ptr: NonNull<turso_ext::Conn>,
        implementation: Arc<VTabModuleImpl>,
        id: u64,
    ) -> crate::Result<Self> {
        Ok(Self {
            cursor,
            conn_ptr: Some(conn_ptr),
            implementation,
            vtab_id: id,
        })
    }

    pub(crate) fn rowid(&self) -> i64 {
        unsafe { (self.implementation.rowid)(self.cursor.as_ptr()) }
    }

    #[tracing::instrument(skip(self))]
    pub(crate) fn filter(
        &self,
        idx_num: i32,
        idx_str: Option<String>,
        arg_count: usize,
        args: crate::alloc::Vec<Value>,
    ) -> crate::Result<bool> {
        tracing::trace!("xFilter");
        let ext_args = args.iter().map(|arg| arg.to_ffi()).collect::<Vec<_>>();
        let idx_str = match idx_str {
            Some(idx_str) => Some(std::ffi::CString::new(idx_str).map_err(|e| {
                crate::LimboError::InternalError(format!("failed to convert idx_str string: {e}"))
            })?),
            None => None,
        };
        let c_idx_str_ptr = idx_str
            .as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        let rc = unsafe {
            (self.implementation.filter)(
                self.cursor.as_ptr(),
                arg_count as i32,
                ext_args.as_ptr(),
                c_idx_str_ptr,
                idx_num,
            )
        };
        for arg in ext_args {
            unsafe {
                arg.__free_internal_type();
            }
        }
        match rc {
            ResultCode::OK => Ok(true),
            ResultCode::EOF => Ok(false),
            _ => Err(LimboError::ExtensionError(rc.to_string())),
        }
    }

    pub(crate) fn column(&self, column: usize) -> crate::Result<Value> {
        let val = unsafe { (self.implementation.column)(self.cursor.as_ptr(), column as u32) };
        Value::from_ffi(val)
    }

    pub(crate) fn next(&self) -> crate::Result<bool> {
        let rc = unsafe { (self.implementation.next)(self.cursor.as_ptr()) };
        match rc {
            ResultCode::OK => Ok(true),
            ResultCode::EOF => Ok(false),
            _ => Err(LimboError::ExtensionError("Next failed".to_string())),
        }
    }
}

impl Drop for ExtVirtualTableCursor {
    fn drop(&mut self) {
        if let Some(ptr) = self.conn_ptr.take() {
            // first free the boxed turso_ext::Conn pointer itself
            let conn = unsafe { Box::from_raw(ptr.as_ptr()) };
            if !conn._ctx.is_null() {
                // we also leaked the Weak 'ctx' pointer, so free this as well
                let _ = unsafe { Box::from_raw(conn._ctx as *mut Weak<Connection>) };
            }
        }
        let result = unsafe { (self.implementation.close)(self.cursor.as_ptr()) };
        if !result.is_ok() {
            tracing::error!("Failed to close virtual table cursor");
        }
    }
}

pub trait InternalVirtualTable: std::fmt::Debug + Send + Sync {
    fn name(&self) -> String;
    fn open(
        &self,
        conn: Arc<Connection>,
    ) -> crate::Result<Arc<RwLock<dyn InternalVirtualTableCursor>>>;
    /// best_index is used by the optimizer. See the comment on `Table::best_index`.
    fn best_index(
        &self,
        constraints: &[turso_ext::ConstraintInfo],
        order_by: &[turso_ext::OrderByInfo],
    ) -> Result<turso_ext::IndexInfo, ResultCode>;
    fn sql(&self) -> String;
}

pub trait InternalVirtualTableCursor: Send + Sync {
    /// next returns `Ok(true)` if there are more rows, and `Ok(false)` otherwise.
    fn next(&mut self) -> Result<bool, LimboError>;
    fn rowid(&self) -> i64;
    fn column(&self, column: usize) -> Result<Value, LimboError>;
    fn filter(
        &mut self,
        args: &[Value],
        idx_str: Option<String>,
        idx_num: i32,
    ) -> Result<bool, LimboError>;
}

#[cfg(all(test, feature = "fs"))]
mod tests {
    use super::*;
    use crate::SqliteDialect;
    use crate::{ConnectOpts, Database, DatabaseOpts, MemoryIO, OpenFlags};

    /// Minimal `InternalVirtualTable` that exposes a fixed two-row table. Used
    /// to verify that callers can register an arbitrary catalog table at
    /// database open time and query it like any other table.
    #[derive(Debug)]
    struct StaticTable {
        name: &'static str,
    }

    impl InternalVirtualTable for StaticTable {
        fn name(&self) -> String {
            self.name.to_string()
        }
        fn sql(&self) -> String {
            format!("CREATE TABLE {}(key TEXT, value INTEGER)", self.name)
        }
        fn open(
            &self,
            _conn: Arc<Connection>,
        ) -> crate::Result<Arc<RwLock<dyn InternalVirtualTableCursor>>> {
            Ok(Arc::new(RwLock::new(StaticCursor {
                rows: vec![("alpha".to_string(), 1), ("beta".to_string(), 2)],
                position: -1,
            })))
        }
        fn best_index(
            &self,
            constraints: &[turso_ext::ConstraintInfo],
            _order_by: &[turso_ext::OrderByInfo],
        ) -> std::result::Result<turso_ext::IndexInfo, ResultCode> {
            Ok(turso_ext::IndexInfo {
                idx_num: 0,
                idx_str: None,
                order_by_consumed: false,
                estimated_cost: 1.0,
                estimated_rows: 2,
                constraint_usages: constraints
                    .iter()
                    .map(|_| turso_ext::ConstraintUsage {
                        argv_index: None,
                        omit: false,
                    })
                    .collect(),
            })
        }
    }

    struct StaticCursor {
        rows: Vec<(String, i64)>,
        position: i64,
    }

    impl InternalVirtualTableCursor for StaticCursor {
        fn next(&mut self) -> Result<bool, LimboError> {
            self.position += 1;
            Ok((self.position as usize) < self.rows.len())
        }
        fn rowid(&self) -> i64 {
            self.position
        }
        fn column(&self, column: usize) -> Result<Value, LimboError> {
            let (key, value) = &self.rows[self.position as usize];
            match column {
                0 => Ok(Value::build_text(key.clone())),
                1 => Ok(Value::from_i64(*value)),
                _ => Err(LimboError::InternalError(format!(
                    "column index {column} out of range"
                ))),
            }
        }
        fn filter(
            &mut self,
            _args: &[Value],
            _idx_str: Option<String>,
            _idx_num: i32,
        ) -> Result<bool, LimboError> {
            self.position = -1;
            self.next()
        }
    }

    #[test]
    fn registered_internal_vtab_is_visible_to_connections() {
        let io: Arc<dyn crate::IO> = Arc::new(MemoryIO::new());
        let db = Database::open_file_with_flags(
            io,
            crate::util::MEMORY_PATH,
            OpenFlags::Create,
            DatabaseOpts::new(),
            None,
            Arc::new(SqliteDialect),
        )
        .unwrap();
        let name = db
            .register_internal_vtab(StaticTable {
                name: "external_metadata",
            })
            .unwrap();
        assert_eq!(name, "external_metadata");

        let conn = db.connect(ConnectOpts::new()).unwrap();
        let mut stmt = conn
            .prepare("SELECT key, value FROM external_metadata")
            .unwrap();
        let rows = stmt.run_collect_rows().unwrap();
        let mapped: Vec<(String, i64)> = rows
            .into_iter()
            .map(|cols| {
                let key = match &cols[0] {
                    Value::Text(t) => t.as_str().to_string(),
                    other => panic!("unexpected key type {other:?}"),
                };
                let value = match &cols[1] {
                    Value::Numeric(crate::Numeric::Integer(i)) => *i,
                    other => panic!("unexpected value type {other:?}"),
                };
                (key, value)
            })
            .collect();
        assert_eq!(
            mapped,
            vec![("alpha".to_string(), 1), ("beta".to_string(), 2)]
        );
    }

    #[test]
    fn registered_internal_vtab_lookup_folds_ascii_only() {
        let io: Arc<dyn crate::IO> = Arc::new(MemoryIO::new());
        let db = Database::open_file_with_flags(
            io,
            crate::util::MEMORY_PATH,
            OpenFlags::Create,
            DatabaseOpts::new(),
            None,
            Arc::new(SqliteDialect),
        )
        .unwrap();
        let name = db
            .register_internal_vtab(StaticTable {
                name: "External_ΔΥΣ",
            })
            .unwrap();
        assert_eq!(name, "External_ΔΥΣ");

        let conn = db.connect(ConnectOpts::new()).unwrap();
        let mut stmt = conn.prepare("SELECT key, value FROM external_ΔΥΣ").unwrap();
        let rows = stmt.run_collect_rows().unwrap();
        assert_eq!(rows.len(), 2);

        assert!(conn.prepare("SELECT key, value FROM external_δυσ").is_err());
    }
}
