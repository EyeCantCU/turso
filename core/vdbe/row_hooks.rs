//! Row-level hooks (`sqlite3_update_hook`, `sqlite3_preupdate_hook`) for the
//! Insert and Delete opcodes.

use super::execute::normalize_table_change_record;
use super::{CursorType, Program, ProgramState};
use crate::data_hooks::{PreUpdateEvent, UpdateHookOp};
use crate::numeric::Numeric;
use crate::schema::BTreeTable;
use crate::sync::Arc;
use crate::vdbe::affinity::Affinity;
use crate::{Result, Value};

/// The database and user table a write cursor changes.
#[derive(Clone)]
pub(crate) struct HookTarget {
    pub db_name: String,
    pub table: Arc<BTreeTable>,
}

/// A row removed by a Delete that is part of an UPDATE or a REPLACE; the
/// next Insert on the same cursor reports it.
pub(crate) struct PendingOldRow {
    pub cursor_id: usize,
    pub rowid: i64,
    pub values: crate::alloc::Vec<Value>,
}

/// The table to report for writes through `cursor_id`, if any: a user rowid
/// table, not an ephemeral, internal or `sqlite_*` table. Only called while a
/// row hook is installed.
///
/// SQLite also reports `sqlite_stat1` writes, but only to the preupdate
/// hook; `preupdate` selects that.
pub(crate) fn hook_target(
    program: &Program,
    state: &ProgramState,
    cursor_id: usize,
    preupdate: bool,
) -> Option<HookTarget> {
    if program
        .prepared()
        .explain
        .ephemeral_cursors
        .contains(&cursor_id)
    {
        return None;
    }
    let table = match program.cursor_ref.get(cursor_id) {
        Some((_, CursorType::BTreeTable(table))) if table.has_rowid => table.clone(),
        _ => return None,
    };
    let is_stat1 = preupdate && table.name.eq_ignore_ascii_case("sqlite_stat1");
    if !is_stat1 && !crate::data_hooks::hook_reports_table(&table.name) {
        return None;
    }
    let db = state.cursor_databases.get(cursor_id).copied().flatten()?;
    let db_name = program.connection.database_name_of(db)?;
    Some(HookTarget { db_name, table })
}

/// A stored record as the hooks see it: one value per column, defaults for
/// columns added later, the rowid for the rowid alias, and REAL affinity
/// applied.
pub(crate) fn row_values(
    table: &BTreeTable,
    rowid: i64,
    values: crate::alloc::Vec<Value>,
) -> Result<crate::alloc::Vec<Value>> {
    let mut values = normalize_table_change_record(table, rowid, values)?;
    for (value, column) in values.iter_mut().zip(table.columns()) {
        if column.affinity() == Affinity::Real {
            if let Value::Numeric(Numeric::Integer(i)) = value {
                *value = Value::from_f64(*i as f64);
            }
        }
    }
    Ok(values)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn fire_preupdate(
    program: &Program,
    target: &HookTarget,
    op: UpdateHookOp,
    old_rowid: i64,
    new_rowid: i64,
    old: Option<&[Value]>,
    new: Option<&[Value]>,
) {
    let hooks = &program.connection.data_hooks;
    hooks.on_preupdate(&PreUpdateEvent {
        op,
        db_name: &target.db_name,
        table: &target.table.name,
        old_rowid,
        new_rowid,
        old,
        new,
        column_count: target.table.columns().len(),
        depth: hooks.depth(),
        blob_write: -1,
    });
}
