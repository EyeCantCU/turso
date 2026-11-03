use crate::native_ext::{
    ConnectInfo, ExtensionState, ModuleFactory, NativeCursor, NativeTable, TableHook,
};
use crate::sync::Arc;
use crate::types::IOResultOr;
use crate::vtab::{ExtVirtualTable, ExtVirtualTableCursor, VirtualTableType};
use crate::{Connection, IOResult, LimboError, Result, Value};
use turso_ext::{ConstraintInfo, IndexInfo, OrderByInfo, ResultCode, VTabKind, VTabModuleImpl};

#[derive(Clone, Debug)]
pub(crate) enum ModuleImplementation {
    C(Arc<VTabModuleImpl>),
    Native(Arc<dyn ModuleFactory>),
}

impl ModuleImplementation {
    pub(crate) fn create_schema(&self, args: Vec<Value>) -> Result<String> {
        match self {
            Self::C(module) => Ok(module.create_schema(args.iter().map(Value::to_ffi).collect())?),
            Self::Native(module) => module.schema(&args),
        }
    }

    pub(crate) fn create(
        &self,
        info: &ConnectInfo,
        args: Vec<Value>,
    ) -> Result<(VirtualTableType, String)> {
        let (table, schema) = match self {
            Self::C(module) => {
                let (table, schema) = ExtVirtualTable::create(module.clone(), args)?;
                (ExtensionTable::C(table), schema)
            }
            Self::Native(module) => {
                let (table, schema) = module.connect(info, &args)?;
                (ExtensionTable::Native(table), schema)
            }
        };
        Ok((VirtualTableType::External(table), schema))
    }

    pub(crate) fn innocuous(&self) -> bool {
        match self {
            Self::C(_) => false,
            Self::Native(module) => module.innocuous(),
        }
    }
}

pub(crate) fn create_virtual_table(
    info: &ConnectInfo,
    module: Option<&Arc<super::VTabImpl>>,
    args: Vec<Value>,
    kind: VTabKind,
) -> Result<(VirtualTableType, String)> {
    let module_name = info.module_name;
    let module = module.ok_or_else(|| {
        LimboError::ExtensionError(format!("Virtual table module not found: {module_name}"))
    })?;
    if kind != module.module_kind {
        let expected = match kind {
            VTabKind::VirtualTable => "virtual table",
            VTabKind::TableValuedFunction => "table-valued function",
        };
        return Err(LimboError::ExtensionError(format!(
            "{module_name} is not a {expected} module"
        )));
    }
    module.implementation.create(info, args)
}

#[derive(Clone, Debug)]
pub(crate) enum ExtensionTable {
    C(ExtVirtualTable),
    Native(NativeTable),
}

impl ExtensionTable {
    pub(crate) fn readonly(&self) -> bool {
        match self {
            Self::C(table) => table.readonly(),
            Self::Native(table) => table.readonly(),
        }
    }

    pub(crate) fn best_index(
        &self,
        constraints: &[ConstraintInfo],
        order_by: &[OrderByInfo],
    ) -> Result<IndexInfo, ResultCode> {
        match self {
            Self::C(table) => table.best_index(constraints, order_by),
            Self::Native(table) => table.best_index(constraints, order_by),
        }
    }

    pub(crate) fn open(&self, connection: Arc<Connection>, id: u64) -> Result<ExtensionCursor> {
        match self {
            Self::C(table) => Ok(ExtensionCursor::C(table.open(connection, id)?)),
            Self::Native(table) => Ok(ExtensionCursor::Native(table.open(connection, id)?)),
        }
    }

    pub(crate) fn update(
        &self,
        conn: &Arc<Connection>,
        args: &[Value],
        state: &mut ExtensionState,
        on_conflict: u8,
    ) -> IOResultOr<Option<i64>> {
        match self {
            Self::C(table) => table.update(args).map(IOResult::Done).map_err(Into::into),
            Self::Native(table) => table.update(conn, state, args, on_conflict),
        }
    }

    /// Runs a transaction, rename or destroy hook.
    pub(crate) fn hook(&self, conn: &Arc<Connection>, hook: TableHook<'_>) -> Result<()> {
        match self {
            Self::C(table) => match hook {
                TableHook::Begin => table.begin(),
                TableHook::Commit => table.commit(),
                TableHook::Rollback => table.rollback(),
                TableHook::Rename(name) => table.rename(name),
                TableHook::Destroy => table.destroy(),
                _ => Ok(()),
            },
            Self::Native(table) => table.hook(conn, hook),
        }
    }
}

pub(crate) enum ExtensionCursor {
    C(ExtVirtualTableCursor),
    Native(NativeCursor),
}

impl ExtensionCursor {
    pub(crate) fn needs_close_at_done(&self) -> bool {
        matches!(self, Self::Native(_))
    }

    pub(crate) fn next(&mut self) -> IOResultOr<bool> {
        match self {
            Self::C(cursor) => cursor.next().map(IOResult::Done).map_err(Into::into),
            Self::Native(cursor) => cursor.cursor.next(),
        }
    }

    pub(crate) fn rowid(&self) -> i64 {
        match self {
            Self::C(cursor) => cursor.rowid(),
            Self::Native(cursor) => cursor.cursor.rowid(),
        }
    }

    pub(crate) fn column(&mut self, column: usize) -> IOResultOr<Value> {
        match self {
            Self::C(cursor) => cursor
                .column(column)
                .map(IOResult::Done)
                .map_err(Into::into),
            Self::Native(cursor) => cursor.cursor.column(column),
        }
    }

    pub(crate) fn filter(
        &mut self,
        idx_num: i32,
        idx_str: Option<String>,
        arg_count: usize,
        args: crate::alloc::Vec<Value>,
    ) -> IOResultOr<bool> {
        match self {
            Self::C(cursor) => cursor
                .filter(idx_num, idx_str, arg_count, args)
                .map(IOResult::Done)
                .map_err(Into::into),
            Self::Native(cursor) => cursor.cursor.filter(&args, idx_str.as_deref(), idx_num),
        }
    }

    pub(crate) fn vtab_id(&self) -> u64 {
        match self {
            Self::C(cursor) => cursor.vtab_id,
            Self::Native(cursor) => cursor.vtab_id,
        }
    }
}
