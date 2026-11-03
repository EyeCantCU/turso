use super::{ConstraintInfo, ExtensionState, IndexInfo, OrderByInfo};
use crate::alloc::TryClone;
use crate::sync::{Arc, RwLock};
use crate::types::IOResultOr;
use crate::{Connection, IOResult, LimboError, Result, Value};
use std::fmt::Debug;
use turso_ext::ResultCode;

/// Identifies the table a module instance is being created for.
#[derive(Debug, Clone, Copy)]
pub struct ConnectInfo<'a> {
    pub module_name: &'a str,
    pub schema_name: &'a str,
    pub table_name: &'a str,
    /// `true` for `CREATE VIRTUAL TABLE`, `false` when attaching to an existing table.
    pub create: bool,
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
    fn destroy(&mut self) -> Result<()> {
        Ok(())
    }
}

pub trait VirtualTableCursor: Send + Sync {
    fn filter(&mut self, args: &[Value], idx_str: Option<&str>, idx_num: i32) -> IOResultOr<bool>;
    fn next(&mut self) -> IOResultOr<bool>;
    fn column(&mut self, column: usize) -> IOResultOr<Value>;
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

    pub(crate) fn best_index(
        &self,
        constraints: &[ConstraintInfo],
        order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        self.0.read().best_index(constraints, order_by)
    }

    pub(crate) fn open(&self, connection: Arc<Connection>, vtab_id: u64) -> Result<NativeCursor> {
        Ok(NativeCursor {
            cursor: self.0.read().open(connection)?,
            vtab_id,
        })
    }

    pub(crate) fn update(
        &self,
        state: &mut ExtensionState,
        args: &[Value],
        on_conflict: u8,
    ) -> IOResultOr<Option<i64>> {
        if matches!(state, ExtensionState::None) {
            let args = args
                .iter()
                .map(Value::try_clone)
                .collect::<Result<Vec<_>, _>>()?;
            *state = ExtensionState::TableUpdate(
                self.0
                    .write()
                    .create_update_on_conflict(args, on_conflict)?,
            );
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

    pub(crate) fn begin(&self) -> Result<()> {
        self.0.write().begin()
    }

    pub(crate) fn commit(&self) -> Result<()> {
        self.0.write().commit()
    }

    pub(crate) fn rollback(&self) -> Result<()> {
        self.0.write().rollback()
    }

    pub(crate) fn rename(&self, name: &str) -> Result<()> {
        self.0.write().rename(name)
    }

    pub(crate) fn commits_after_create(&self) -> bool {
        self.0.read().commits_after_create()
    }

    pub(crate) fn savepoint(&self, level: usize) -> Result<()> {
        self.0.write().savepoint(level)
    }

    pub(crate) fn release(&self, level: usize) -> Result<()> {
        self.0.write().release(level)
    }

    pub(crate) fn rollback_to(&self, level: usize) -> Result<()> {
        self.0.write().rollback_to(level)
    }

    pub(crate) fn destroy(&self) -> Result<()> {
        self.0.write().destroy()
    }
}

trait TableInstance: Debug + Send + Sync {
    fn open(&self, connection: Arc<Connection>) -> Result<Box<dyn VirtualTableCursor>>;
    fn best_index(
        &self,
        constraints: &[ConstraintInfo],
        order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode>;
    fn readonly(&self) -> bool;
    fn create_update_on_conflict(
        &mut self,
        args: Vec<Value>,
        on_conflict: u8,
    ) -> Result<Box<dyn TableUpdate>>;
    fn commits_after_create(&self) -> bool;
    fn savepoint(&mut self, level: usize) -> Result<()>;
    fn release(&mut self, level: usize) -> Result<()>;
    fn rollback_to(&mut self, level: usize) -> Result<()>;
    fn begin(&mut self) -> Result<()>;
    fn commit(&mut self) -> Result<()>;
    fn rollback(&mut self) -> Result<()>;
    fn rename(&mut self, name: &str) -> Result<()>;
    fn destroy(&mut self) -> Result<()>;
}

impl<T: VirtualTable> TableInstance for T {
    fn open(&self, connection: Arc<Connection>) -> Result<Box<dyn VirtualTableCursor>> {
        Ok(Box::new(VirtualTable::open(self, connection)?))
    }

    fn best_index(
        &self,
        constraints: &[ConstraintInfo],
        order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        VirtualTable::best_index(self, constraints, order_by)
    }

    fn readonly(&self) -> bool {
        VirtualTable::readonly(self)
    }

    fn create_update_on_conflict(
        &mut self,
        args: Vec<Value>,
        on_conflict: u8,
    ) -> Result<Box<dyn TableUpdate>> {
        VirtualTable::create_update_on_conflict(self, args, on_conflict)
    }

    fn commits_after_create(&self) -> bool {
        VirtualTable::commits_after_create(self)
    }

    fn savepoint(&mut self, level: usize) -> Result<()> {
        VirtualTable::savepoint(self, level)
    }

    fn release(&mut self, level: usize) -> Result<()> {
        VirtualTable::release(self, level)
    }

    fn rollback_to(&mut self, level: usize) -> Result<()> {
        VirtualTable::rollback_to(self, level)
    }

    fn begin(&mut self) -> Result<()> {
        VirtualTable::begin(self)
    }

    fn commit(&mut self) -> Result<()> {
        VirtualTable::commit(self)
    }

    fn rollback(&mut self) -> Result<()> {
        VirtualTable::rollback(self)
    }

    fn rename(&mut self, name: &str) -> Result<()> {
        VirtualTable::rename(self, name)
    }

    fn destroy(&mut self) -> Result<()> {
        VirtualTable::destroy(self)
    }
}

pub(crate) struct NativeCursor {
    pub(crate) cursor: Box<dyn VirtualTableCursor>,
    pub(crate) vtab_id: u64,
}
