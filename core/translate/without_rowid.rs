//! Helpers for WITHOUT ROWID tables, whose b-tree is keyed on the PRIMARY KEY
//! and whose secondary index entries end with the primary key columns.

use crate::alloc::*;
use crate::schema::{BTreeTable, Index};
use crate::vdbe::builder::ProgramBuilder;
use crate::vdbe::insn::Insn;
use crate::vdbe::BranchOffset;
use crate::Result;

/// Number of primary key columns (the row key) of a WITHOUT ROWID table.
pub(crate) fn pk_len(table: &BTreeTable) -> Result<usize> {
    Ok(table.without_rowid_pk_columns()?.len())
}

/// Position of each primary key column in an index entry of `index`.
pub(crate) fn pk_positions_in_index(table: &BTreeTable, index: &Index) -> Result<Vec<usize>> {
    let mut positions = Vec::new();
    for pk_col in table.without_rowid_pk_columns()? {
        let Some(pos) = index.columns.iter().position(|c| {
            c.expr.is_none()
                && c.pos_in_table == pk_col.pos_in_table
                && c.collation == pk_col.collation
        }) else {
            crate::bail_corrupt_error!(
                "index {} lacks primary key column {}",
                index.name,
                pk_col.name
            );
        };
        positions.push(pos);
    }
    Ok(positions)
}

/// Read the primary key of the entry `index_cursor` points at into `dest..`.
pub(crate) fn emit_pk_from_index(
    program: &mut ProgramBuilder,
    table: &BTreeTable,
    index: &Index,
    index_cursor: usize,
    dest: usize,
) -> Result<()> {
    for (i, pos) in pk_positions_in_index(table, index)?.into_iter().enumerate() {
        program.emit_insn(Insn::Column {
            cursor_id: index_cursor,
            column: pos,
            dest: dest + i,
            default: None,
        });
    }
    Ok(())
}

/// Read the primary key of the row `table_cursor` points at into `dest..`.
pub(crate) fn emit_pk_from_table(
    program: &mut ProgramBuilder,
    table: &BTreeTable,
    table_cursor: usize,
    dest: usize,
) -> Result<()> {
    // The key columns come first in the record.
    for i in 0..pk_len(table)? {
        program.emit_insn(Insn::Column {
            cursor_id: table_cursor,
            column: i,
            dest: dest + i,
            default: None,
        });
    }
    Ok(())
}

/// Position `table_cursor` on the row whose primary key is in `start..`, or
/// jump to `not_found`.
pub(crate) fn emit_seek_row(
    program: &mut ProgramBuilder,
    table: &BTreeTable,
    table_cursor: usize,
    start: usize,
    not_found: BranchOffset,
) -> Result<()> {
    program.emit_insn(Insn::NotFound {
        cursor_id: table_cursor,
        target_pc: not_found,
        record_reg: start,
        num_regs: pk_len(table)?,
    });
    Ok(())
}

/// Result columns selecting the primary key of `table` (internal id `table_id`)
/// and an ephemeral index to collect them in, for two-pass DELETE and UPDATE.
pub(crate) fn pk_set_columns(
    table: &BTreeTable,
    table_id: turso_parser::ast::TableInternalId,
) -> Result<(
    Vec<crate::translate::plan::ResultSetColumn>,
    std::sync::Arc<Index>,
)> {
    let pk_cols = table.without_rowid_pk_columns()?;
    let result_columns = pk_cols
        .iter()
        .map(|c| crate::translate::plan::ResultSetColumn {
            expr: turso_parser::ast::Expr::Column {
                database: None,
                table: table_id,
                column: c.pos_in_table,
                is_rowid_alias: false,
            },
            alias: None,
            implicit_column_name: None,
            contains_aggregates: false,
        })
        .collect();
    let mut columns = crate::alloc::Vec::new();
    for (i, c) in pk_cols.iter().enumerate() {
        let mut col = c.clone();
        col.pos_in_table = i;
        col.expr = None;
        columns.try_push(col)?;
    }
    let index = Index {
        name: format!("{}_pk_set", table.name),
        table_name: String::new(),
        root_page: 0,
        n_key_col: columns.len(),
        columns,
        unique: false,
        ephemeral: true,
        has_rowid: false,
        where_clause: None,
        index_method: None,
        on_conflict: None,
        constraint_name: None,
    };
    Ok((result_columns, std::sync::Arc::new(index)))
}

/// Jump to `target` if the primary keys in `a..` and `b..` are equal.
pub(crate) fn emit_jump_if_pk_equal(
    program: &mut ProgramBuilder,
    table: &BTreeTable,
    a: usize,
    b: usize,
    target: BranchOffset,
) -> Result<()> {
    let differ = program.allocate_label();
    for (i, col) in table.without_rowid_pk_columns()?.iter().enumerate() {
        program.emit_insn(Insn::Ne {
            lhs: a + i,
            rhs: b + i,
            target_pc: differ,
            flags: crate::vdbe::insn::CmpInsFlags::default(),
            collation: col.collation,
        });
    }
    program.emit_insn(Insn::Goto { target_pc: target });
    program.preassign_label_to_next_insn(differ);
    Ok(())
}
