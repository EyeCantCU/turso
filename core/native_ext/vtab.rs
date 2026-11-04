use super::{ConstraintInfo, ExtensionState, IndexInfo, IndexPlan, IndexRequest, OrderByInfo};
use crate::alloc::TryClone;
use crate::function::ExternalFunc;
use crate::sync::{Arc, RwLock};
use crate::types::IOResultOr;
use crate::{Connection, IOResult, LimboError, Result, Value};
use std::fmt::Debug;
use turso_ext::ResultCode;

/// Identifies the table a module instance is being created for.
#[derive(Clone, Copy)]
pub struct ConnectInfo<'a> {
    pub module_name: &'a str,
    pub schema_name: &'a str,
    pub table_name: &'a str,
    /// `true` for `CREATE VIRTUAL TABLE`, `false` when attaching to an existing table.
    pub create: bool,
    /// The connection running `CREATE VIRTUAL TABLE`, when there is one.
    pub connection: Option<&'a Arc<Connection>>,
    /// Column names of another table in the schema being loaded, for
    /// modules that derive their columns from one.
    pub lookup_columns: Option<&'a ColumnLookup<'a>>,
}

/// Finds the column names of a table by name.
pub type ColumnLookup<'a> = dyn Fn(&str) -> Option<Vec<String>> + 'a;

impl Debug for ConnectInfo<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectInfo")
            .field("module_name", &self.module_name)
            .field("schema_name", &self.schema_name)
            .field("table_name", &self.table_name)
            .field("create", &self.create)
            .finish_non_exhaustive()
    }
}

/// A transaction, rename or destroy hook, as passed to [`VirtualTable::hook`].
#[derive(Debug, Clone, Copy)]
pub enum TableHook<'a> {
    Begin,
    /// xSync: before commit, while the transaction is still open.
    Sync,
    Commit,
    Rollback,
    Savepoint(usize),
    Release(usize),
    RollbackTo(usize),
    Rename(&'a str),
    Destroy,
}

pub trait VirtualTableModule: Debug + Send + Sync {
    type Table: VirtualTable + 'static;

    fn schema(&self, args: &[Value]) -> Result<String>;
    fn create(&self, args: &[Value]) -> Result<Self::Table>;
    /// Builds a table and its schema in one call. Modules that need the table
    /// name or create-vs-connect override this; the default uses `schema` and `create`.
    fn connect(&self, _info: &ConnectInfo, args: &[Value]) -> Result<(Self::Table, String)> {
        let schema = self.schema(args)?;
        Ok((self.create(args)?, schema))
    }
    /// Whether triggers can read tables created by this module.
    /// Defaults to `false` because a trigger can call extension code as part
    /// of an unrelated write.
    /// Return `true` only if the module is safe to call this way.
    /// This permits reads only, not writes from triggers.
    fn innocuous(&self) -> bool {
        false
    }
}

pub trait VirtualTable: Debug + Send + Sync {
    type Cursor: VirtualTableCursor + 'static;

    fn open(&self, connection: Arc<Connection>) -> Result<Self::Cursor>;
    fn best_index(
        &self,
        constraints: &[ConstraintInfo],
        order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode>;
    /// SQLite's full xBestIndex; `Ok(None)` is SQLITE_CONSTRAINT. The default
    /// runs `best_index`.
    fn best_index_plan(&self, req: &IndexRequest) -> Result<Option<IndexPlan>> {
        super::index::best_index_via_legacy(req, |c, o| self.best_index(c, o))
    }
    /// xFindFunction: an overload of `name`, called with `n_arg` arguments
    /// whose first is a column of this table, and its code (1, or a
    /// constraint op of at least 150).
    fn find_function(&self, _n_arg: i32, _name: &str) -> Option<(i32, Arc<ExternalFunc>)> {
        None
    }
    /// xIntegrity for PRAGMA integrity_check (`quick` for quick_check): an
    /// error description, or None when the table is fine.
    fn integrity(
        &self,
        _connection: &Arc<Connection>,
        _schema: &str,
        _table: &str,
        _quick: bool,
    ) -> Result<Option<String>> {
        Ok(None)
    }
    /// Whether the table has an xIntegrity for PRAGMA integrity_check to call.
    fn has_integrity(&self) -> bool {
        false
    }
    /// xShadowName: `<table>_<suffix>` is a shadow table of this module.
    fn is_shadow_name(&self, _suffix: &str) -> bool {
        false
    }
    fn readonly(&self) -> bool {
        true
    }
    fn create_update(&mut self, _args: Vec<Value>) -> Result<Box<dyn TableUpdate>> {
        Err(LimboError::ReadOnly)
    }
    /// Like `create_update`, with the statement's ON CONFLICT action
    /// (`ResolveType::bit_value`, 0 when none was given).
    fn create_update_on_conflict(
        &mut self,
        args: Vec<Value>,
        _on_conflict: u8,
    ) -> Result<Box<dyn TableUpdate>> {
        self.create_update(args)
    }
    /// Like `create_update_on_conflict`, with the connection running the
    /// statement, for modules that keep their data in ordinary tables.
    fn create_update_with_connection(
        &mut self,
        _connection: &Arc<Connection>,
        args: Vec<Value>,
        on_conflict: u8,
    ) -> Result<Box<dyn TableUpdate>> {
        self.create_update_on_conflict(args, on_conflict)
    }
    /// Runs before `commit`, while the transaction is still open (xSync).
    fn sync(&mut self, _connection: &Arc<Connection>) -> Result<()> {
        Ok(())
    }
    /// Like `savepoint`, with the connection that opened it.
    fn savepoint_with_connection(
        &mut self,
        _connection: &Arc<Connection>,
        level: usize,
    ) -> Result<()> {
        self.savepoint(level)
    }
    /// Whether `CREATE VIRTUAL TABLE` enlists the new table in the
    /// transaction, so `commit` (or `rollback`) runs without a `begin`.
    fn commits_after_create(&self) -> bool {
        false
    }
    fn savepoint(&mut self, _level: usize) -> Result<()> {
        Ok(())
    }
    fn release(&mut self, _level: usize) -> Result<()> {
        Ok(())
    }
    fn rollback_to(&mut self, _level: usize) -> Result<()> {
        Ok(())
    }
    fn begin(&mut self) -> Result<()> {
        Ok(())
    }
    fn commit(&mut self) -> Result<()> {
        Ok(())
    }
    fn rollback(&mut self) -> Result<()> {
        Ok(())
    }
    fn rename(&mut self, _name: &str) -> Result<()> {
        Ok(())
    }
    /// Like `rename`, with the connection running `ALTER TABLE`.
    fn rename_with_connection(&mut self, _connection: &Arc<Connection>, name: &str) -> Result<()> {
        self.rename(name)
    }
    fn destroy(&mut self) -> Result<()> {
        Ok(())
    }
    /// Runs a hook for `conn`. Modules that need the calling connection for
    /// every hook override this; the default dispatches to the methods above.
    fn hook(&mut self, conn: &Arc<Connection>, hook: TableHook<'_>) -> Result<()> {
        match hook {
            TableHook::Begin => self.begin(),
            TableHook::Sync => self.sync(conn),
            TableHook::Commit => self.commit(),
            TableHook::Rollback => self.rollback(),
            TableHook::Savepoint(level) => self.savepoint_with_connection(conn, level),
            TableHook::Release(level) => self.release(level),
            TableHook::RollbackTo(level) => self.rollback_to(level),
            TableHook::Rename(name) => self.rename_with_connection(conn, name),
            TableHook::Destroy => self.destroy_with_connection(conn),
        }
    }
    /// Whether the plan `idx_num` visits at most one row
    /// (`SQLITE_INDEX_SCAN_UNIQUE`), so a write through it is a single write.
    fn unique_scan(&self, _idx_num: i32) -> bool {
        false
    }
    /// Like `destroy`, with the connection running `DROP TABLE`.
    fn destroy_with_connection(&mut self, _connection: &Arc<Connection>) -> Result<()> {
        self.destroy()
    }
}

/// A pointer value for a column (`sqlite3_result_pointer` in xColumn).
pub type PointerPayload = Arc<crate::pointer::PointerValue>;

pub trait VirtualTableCursor: Send + Sync {
    /// A pointer-typed NULL for `column`, read before `column`.
    fn column_pointer(&mut self, _column: usize) -> Option<PointerPayload> {
        None
    }
    fn filter(&mut self, args: &[Value], idx_str: Option<&str>, idx_num: i32) -> IOResultOr<bool>;
    /// Like `filter`; `in_lists[i]` holds the values of argument `i` when it
    /// is an IN handled all at once.
    fn filter_in(
        &mut self,
        args: &[Value],
        _in_lists: &[Option<Vec<Value>>],
        idx_str: Option<&str>,
        idx_num: i32,
    ) -> IOResultOr<bool> {
        self.filter(args, idx_str, idx_num)
    }
    fn next(&mut self) -> IOResultOr<bool>;
    fn column(&mut self, column: usize) -> IOResultOr<Value>;
    /// `column` for an UPDATE that passes the column through unchanged
    /// (`sqlite3_vtab_nochange` is true).
    fn column_nochange(&mut self, column: usize) -> IOResultOr<Value> {
        self.column(column)
    }
    fn rowid(&self) -> i64;
}

pub trait TableUpdate: Send + Sync {
    fn step(&mut self) -> IOResultOr<Option<i64>>;
}

pub(crate) trait ModuleFactory: Debug + Send + Sync {
    fn schema(&self, args: &[Value]) -> Result<String>;
    fn connect(&self, info: &ConnectInfo, args: &[Value]) -> Result<(NativeTable, String)>;
    fn innocuous(&self) -> bool;
}

impl<M: VirtualTableModule> ModuleFactory for M {
    fn schema(&self, args: &[Value]) -> Result<String> {
        VirtualTableModule::schema(self, args)
    }

    fn connect(&self, info: &ConnectInfo, args: &[Value]) -> Result<(NativeTable, String)> {
        let (table, schema) = VirtualTableModule::connect(self, info, args)?;
        Ok((NativeTable(Arc::new(RwLock::new(Box::new(table)))), schema))
    }

    fn innocuous(&self) -> bool {
        VirtualTableModule::innocuous(self)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NativeTable(Arc<RwLock<Box<dyn TableInstance>>>);

impl NativeTable {
    pub(crate) fn readonly(&self) -> bool {
        self.0.read().readonly()
    }

    pub(crate) fn best_index_plan(&self, req: &IndexRequest) -> Result<Option<IndexPlan>> {
        self.0.read().best_index_plan(req)
    }

    pub(crate) fn find_function(&self, n_arg: i32, name: &str) -> Option<(i32, Arc<ExternalFunc>)> {
        self.0.read().find_function(n_arg, name)
    }

    pub(crate) fn integrity(
        &self,
        connection: &Arc<Connection>,
        schema: &str,
        table: &str,
        quick: bool,
    ) -> Result<Option<String>> {
        self.0.read().integrity(connection, schema, table, quick)
    }

    pub(crate) fn is_shadow_name(&self, suffix: &str) -> bool {
        self.0.read().is_shadow_name(suffix)
    }

    pub(crate) fn has_integrity(&self) -> bool {
        self.0.read().has_integrity()
    }

    pub(crate) fn open(&self, connection: Arc<Connection>, vtab_id: u64) -> Result<NativeCursor> {
        Ok(NativeCursor {
            cursor: self.0.read().open(connection)?,
            vtab_id,
        })
    }

    pub(crate) fn update(
        &self,
        connection: &Arc<Connection>,
        state: &mut ExtensionState,
        args: &[Value],
        on_conflict: u8,
    ) -> IOResultOr<Option<i64>> {
        if matches!(state, ExtensionState::None) {
            let args = args
                .iter()
                .map(Value::try_clone)
                .collect::<Result<Vec<_>, _>>()?;
            *state = ExtensionState::TableUpdate(self.0.write().create_update_with_connection(
                connection,
                args,
                on_conflict,
            )?);
        }
        let ExtensionState::TableUpdate(update) = state else {
            unreachable!("update instruction requires table update state");
        };
        let result = update.step();
        if !matches!(result, Ok(IOResult::IO(_))) {
            *state = ExtensionState::None;
        }
        result
    }

    pub(crate) fn hook(&self, conn: &Arc<Connection>, hook: TableHook<'_>) -> Result<()> {
        self.0.write().hook(conn, hook)
    }

    pub(crate) fn rollback(&self) -> Result<()> {
        self.0.write().rollback()
    }

    pub(crate) fn commits_after_create(&self) -> bool {
        self.0.read().commits_after_create()
    }

    pub(crate) fn unique_scan(&self, idx_num: i32) -> bool {
        self.0.read().unique_scan(idx_num)
    }
}

trait TableInstance: Debug + Send + Sync {
    fn open(&self, connection: Arc<Connection>) -> Result<Box<dyn VirtualTableCursor>>;
    fn best_index_plan(&self, req: &IndexRequest) -> Result<Option<IndexPlan>>;
    fn find_function(&self, n_arg: i32, name: &str) -> Option<(i32, Arc<ExternalFunc>)>;
    fn integrity(
        &self,
        connection: &Arc<Connection>,
        schema: &str,
        table: &str,
        quick: bool,
    ) -> Result<Option<String>>;
    fn has_integrity(&self) -> bool;
    fn is_shadow_name(&self, suffix: &str) -> bool;
    fn readonly(&self) -> bool;
    fn create_update_with_connection(
        &mut self,
        connection: &Arc<Connection>,
        args: Vec<Value>,
        on_conflict: u8,
    ) -> Result<Box<dyn TableUpdate>>;
    fn commits_after_create(&self) -> bool;
    fn unique_scan(&self, idx_num: i32) -> bool;
    fn hook(&mut self, conn: &Arc<Connection>, hook: TableHook<'_>) -> Result<()>;
    fn rollback(&mut self) -> Result<()>;
}

impl<T: VirtualTable> TableInstance for T {
    fn open(&self, connection: Arc<Connection>) -> Result<Box<dyn VirtualTableCursor>> {
        Ok(Box::new(VirtualTable::open(self, connection)?))
    }

    fn best_index_plan(&self, req: &IndexRequest) -> Result<Option<IndexPlan>> {
        VirtualTable::best_index_plan(self, req)
    }

    fn find_function(&self, n_arg: i32, name: &str) -> Option<(i32, Arc<ExternalFunc>)> {
        VirtualTable::find_function(self, n_arg, name)
    }

    fn integrity(
        &self,
        connection: &Arc<Connection>,
        schema: &str,
        table: &str,
        quick: bool,
    ) -> Result<Option<String>> {
        VirtualTable::integrity(self, connection, schema, table, quick)
    }

    fn has_integrity(&self) -> bool {
        VirtualTable::has_integrity(self)
    }

    fn is_shadow_name(&self, suffix: &str) -> bool {
        VirtualTable::is_shadow_name(self, suffix)
    }

    fn readonly(&self) -> bool {
        VirtualTable::readonly(self)
    }

    fn create_update_with_connection(
        &mut self,
        connection: &Arc<Connection>,
        args: Vec<Value>,
        on_conflict: u8,
    ) -> Result<Box<dyn TableUpdate>> {
        VirtualTable::create_update_with_connection(self, connection, args, on_conflict)
    }

    fn commits_after_create(&self) -> bool {
        VirtualTable::commits_after_create(self)
    }

    fn unique_scan(&self, idx_num: i32) -> bool {
        VirtualTable::unique_scan(self, idx_num)
    }

    fn hook(&mut self, conn: &Arc<Connection>, hook: TableHook<'_>) -> Result<()> {
        VirtualTable::hook(self, conn, hook)
    }

    fn rollback(&mut self) -> Result<()> {
        VirtualTable::rollback(self)
    }
}

pub(crate) struct NativeCursor {
    pub(crate) cursor: Box<dyn VirtualTableCursor>,
    pub(crate) vtab_id: u64,
}
