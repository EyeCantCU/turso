//! INSERT, UPDATE and DELETE on a view, carried out by its INSTEAD OF triggers.
//!
//! Like SQLite, the rows the statement would affect are first collected in an
//! ephemeral table (the view rows for UPDATE and DELETE, the new rows for
//! INSERT), then the triggers fire once per row with OLD and NEW bound to the
//! view's columns. The statement itself changes no rows.

use crate::alloc::*;
use crate::namespace::NamespaceRef;
use crate::schema::{BTreeCharacteristics, BTreeTable, ColDef, Column, Type, View};
use crate::sync::Arc;
use crate::translate::emitter::Resolver;
use crate::translate::plan::{EphemeralRowidMode, QueryDestination};
use crate::translate::select::translate_select;
use crate::translate::trigger_exec::{fire_trigger, get_triggers_including_temp, TriggerContext};
use crate::vdbe::builder::{CursorType, DmlRowForm, ProgramBuilder};
use crate::vdbe::insn::Insn;
use crate::{bail_parse_error, Connection, LimboError, Result};
use turso_parser::ast::{self, TriggerEvent, TriggerTime};
use turso_parser::parser::Parser;

/// The view a DML statement targets, if `tbl_name` names a view.
pub(crate) fn view_target(
    resolver: &Resolver,
    tbl_name: &ast::QualifiedName,
) -> Result<Option<(NamespaceRef, Arc<View>)>> {
    let Some(namespace) = resolver.find_existing_table(tbl_name)? else {
        return Ok(None);
    };
    let name = crate::util::normalize_ident(tbl_name.name.as_str());
    Ok(resolver
        .with_schema(namespace.database_id, |s| s.get_view(namespace.id, &name))
        .map(|view| (namespace, view)))
}

/// The view as a table, so triggers can bind OLD and NEW to its columns.
fn view_as_table(view: &View) -> Result<Arc<BTreeTable>> {
    let mut columns = Vec::new();
    for col in &view.columns {
        columns.try_push(col.clone())?;
    }
    Ok(Arc::new(BTreeTable::new(
        0,
        view.name.clone(),
        crate::alloc::vec![],
        columns,
        BTreeCharacteristics::HAS_ROWID,
        crate::alloc::vec![],
        crate::alloc::vec![],
        crate::alloc::vec![],
        None,
    )))
}

fn parse_select(sql: &str) -> Result<ast::Select> {
    let mut parser = Parser::new(sql.as_bytes());
    match parser.next_cmd() {
        Ok(Some(ast::Cmd::Stmt(ast::Stmt::Select(select)))) => Ok(select),
        Ok(_) => Err(LimboError::InternalError(format!(
            "generated statement is not a SELECT: {sql}"
        ))),
        Err(e) => Err(LimboError::ParseError(e.to_string())),
    }
}

/// The triggers that will run in place of `event`, or SQLite's error when none do.
fn instead_of_triggers(
    resolver: &Resolver,
    namespace: NamespaceRef,
    view_table: &BTreeTable,
    event: TriggerEvent,
    updated: Option<&[ast::Name]>,
    connection: &Connection,
) -> Result<Vec<Arc<crate::schema::Trigger>>> {
    let triggers = get_triggers_including_temp(
        resolver,
        namespace,
        event,
        TriggerTime::InsteadOf,
        updated,
        view_table,
        connection,
    );
    if triggers.is_empty() {
        bail_parse_error!("cannot modify {} because it is a view", view_table.name);
    }
    Ok(triggers)
}

/// Run `select` into an ephemeral table, then for each row fire `triggers`
/// with OLD from the first `n_old` columns and NEW from the columns that
/// `new_map` names (a `None` entry is NULL).
#[allow(clippy::too_many_arguments)]
fn emit_fire_for_each_row(
    program: &mut ProgramBuilder,
    resolver: &mut Resolver,
    connection: &Arc<Connection>,
    namespace: NamespaceRef,
    view_table: Arc<BTreeTable>,
    triggers: Vec<Arc<crate::schema::Trigger>>,
    select: ast::Select,
    width: usize,
    n_old: usize,
    new_map: Option<std::vec::Vec<Option<usize>>>,
) -> Result<()> {
    let schema_cookie = resolver.with_schema(namespace.database_id, |s| s.schema_version);
    program.begin_write_on_database(namespace.database_id, schema_cookie)?;

    // One scratch column per SELECT result column.
    let mut scratch_columns = Vec::new();
    for i in 0..width {
        scratch_columns.try_push(Column::new(
            Some(format!("c{i}")),
            "BLOB".to_string(),
            None,
            None,
            Type::Blob,
            None,
            ColDef::default(),
        ))?;
    }
    let scratch = Arc::new(BTreeTable::new(
        0,
        "instead_of_rows".to_string(),
        crate::alloc::vec![],
        scratch_columns,
        BTreeCharacteristics::HAS_ROWID,
        crate::alloc::vec![],
        crate::alloc::vec![],
        crate::alloc::vec![],
        None,
    ));
    let cursor_id = program.alloc_cursor_id(CursorType::BTreeTable(scratch.clone()));
    program.emit_insn(Insn::OpenEphemeral {
        cursor_id,
        is_table: true,
    });
    let produced = program.nested(|program| {
        translate_select(
            select,
            resolver,
            program,
            QueryDestination::EphemeralTable {
                cursor_id,
                table: scratch,
                rowid_mode: EphemeralRowidMode::Auto,
            },
            connection,
        )
    })?;
    if produced != width {
        bail_parse_error!(
            "table {} has {} columns but {} values were supplied",
            view_table.name,
            view_table.columns().len(),
            produced
        );
    }

    let n_cols = view_table.columns().len();
    let loop_start = program.allocate_label();
    let loop_end = program.allocate_label();
    let next_row = program.allocate_label();
    program.emit_insn(Insn::Rewind {
        cursor_id,
        pc_if_empty: loop_end,
    });
    program.preassign_label_to_next_insn(loop_start);
    let row = program.alloc_registers(width);
    for i in 0..width {
        program.emit_insn(Insn::Column {
            cursor_id,
            column: i,
            dest: row + i,
            default: None,
        });
    }
    let rowid_reg = program.alloc_register();
    program.emit_insn(Insn::Null {
        dest: rowid_reg,
        dest_end: None,
    });
    let null_reg = program.alloc_register();
    program.emit_insn(Insn::Null {
        dest: null_reg,
        dest_end: None,
    });
    let old_registers = (n_old > 0).then(|| {
        (0..n_cols)
            .map(|i| row + i)
            .chain(std::iter::once(rowid_reg))
            .collect::<std::vec::Vec<_>>()
    });
    let new_registers = new_map.as_ref().map(|map| {
        map.iter()
            .map(|src| src.map_or(null_reg, |i| row + i))
            .chain(std::iter::once(rowid_reg))
            .collect::<std::vec::Vec<_>>()
    });
    let ctx = TriggerContext::new(
        view_table,
        new_registers,
        DmlRowForm::UserFacing,
        old_registers,
    );
    for trigger in triggers {
        fire_trigger(
            program, resolver, trigger, &ctx, connection, namespace, next_row,
        )?;
    }
    program.preassign_label_to_next_insn(next_row);
    program.emit_insn(Insn::Next {
        cursor_id,
        pc_if_next: loop_start,
        fullscan: false,
        is_index: false,
    });
    program.preassign_label_to_next_insn(loop_end);
    Ok(())
}

fn with_prefix(with: Option<&ast::With>) -> String {
    with.map_or_else(String::new, |w| format!("{w} "))
}

fn where_suffix(where_clause: Option<&ast::Expr>) -> String {
    where_clause.map_or_else(String::new, |w| format!(" WHERE {w}"))
}

/// DELETE FROM a view: fire its INSTEAD OF DELETE triggers for each matching row.
pub(crate) fn translate_view_delete(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    connection: &Arc<Connection>,
    namespace: NamespaceRef,
    view: &View,
    tbl_name: &ast::QualifiedName,
    where_clause: Option<&ast::Expr>,
    with: Option<&ast::With>,
) -> Result<()> {
    let view_table = view_as_table(view)?;
    let triggers = instead_of_triggers(
        resolver,
        namespace,
        &view_table,
        TriggerEvent::Delete,
        None,
        connection,
    )?;
    let select = parse_select(&format!(
        "{}SELECT * FROM {tbl_name}{}",
        with_prefix(with),
        where_suffix(where_clause)
    ))?;
    let n = view_table.columns().len();
    emit_fire_for_each_row(
        program,
        &mut resolver.fork(),
        connection,
        namespace,
        view_table,
        triggers,
        select,
        n,
        n,
        None,
    )
}

/// UPDATE a view: fire its INSTEAD OF UPDATE triggers for each matching row,
/// with NEW being the row after the SET clauses.
pub(crate) fn translate_view_update(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    connection: &Arc<Connection>,
    namespace: NamespaceRef,
    view: &View,
    update: &ast::Update,
) -> Result<()> {
    let view_table = view_as_table(view)?;
    let n = view_table.columns().len();
    // Expand `(a, b) = (x, y)` into one assignment per column.
    let mut assignments: std::vec::Vec<(usize, String)> = std::vec::Vec::new();
    let mut updated_names = std::vec::Vec::new();
    for set in &update.sets {
        let exprs: std::vec::Vec<&ast::Expr> = match (set.col_names.len(), set.expr.as_ref()) {
            (1, e) => std::vec![e],
            (k, ast::Expr::Parenthesized(list)) if list.len() == k => {
                list.iter().map(|e| e.as_ref()).collect()
            }
            (k, _) => bail_parse_error!("{k} columns assigned 1 values"),
        };
        for (name, expr) in set.col_names.iter().zip(exprs) {
            let Some((pos, _)) =
                view_table.get_column(&crate::util::normalize_ident(name.as_str()))
            else {
                bail_parse_error!("no such column: {}", name.as_str());
            };
            updated_names.push(name.clone());
            assignments.retain(|(p, _)| *p != pos);
            assignments.push((pos, expr.to_string()));
        }
    }
    let triggers = instead_of_triggers(
        resolver,
        namespace,
        &view_table,
        TriggerEvent::Update,
        Some(&updated_names),
        connection,
    )?;
    let extra: String = assignments
        .iter()
        .map(|(_, e)| format!(", ({e})"))
        .collect();
    let select = parse_select(&format!(
        "{}SELECT *{extra} FROM {}{}",
        with_prefix(update.with.as_ref()),
        update.tbl_name,
        where_suffix(update.where_clause.as_deref())
    ))?;
    let new_map = (0..n)
        .map(|pos| {
            Some(
                assignments
                    .iter()
                    .position(|(p, _)| *p == pos)
                    .map_or(pos, |k| n + k),
            )
        })
        .collect();
    emit_fire_for_each_row(
        program,
        &mut resolver.fork(),
        connection,
        namespace,
        view_table,
        triggers,
        select,
        n + assignments.len(),
        n,
        Some(new_map),
    )
}

/// INSERT INTO a view: fire its INSTEAD OF INSERT triggers for each new row.
#[allow(clippy::too_many_arguments)]
pub(crate) fn translate_view_insert(
    program: &mut ProgramBuilder,
    resolver: &Resolver,
    connection: &Arc<Connection>,
    namespace: NamespaceRef,
    view: &View,
    columns: &[ast::Name],
    body: ast::InsertBody,
    with: Option<ast::With>,
) -> Result<()> {
    let view_table = view_as_table(view)?;
    let n = view_table.columns().len();
    let triggers = instead_of_triggers(
        resolver,
        namespace,
        &view_table,
        TriggerEvent::Insert,
        None,
        connection,
    )?;
    // Position of each inserted value among the view's columns.
    let targets: std::vec::Vec<usize> = if columns.is_empty() {
        (0..n).collect()
    } else {
        columns
            .iter()
            .map(|c| {
                view_table
                    .get_column(&crate::util::normalize_ident(c.as_str()))
                    .map(|(pos, _)| pos)
                    .ok_or_else(|| {
                        LimboError::ParseError(format!(
                            "table {} has no column named {}",
                            view_table.name,
                            c.as_str()
                        ))
                    })
            })
            .collect::<Result<_>>()?
    };
    let mut select = match body {
        ast::InsertBody::Select(select, None) => select,
        ast::InsertBody::Select(_, Some(_)) => {
            bail_parse_error!("UPSERT not implemented for virtual table \"{}\"", view.name)
        }
        ast::InsertBody::DefaultValues => {
            let nulls = std::vec!["NULL"; targets.len()].join(", ");
            parse_select(&format!("VALUES({nulls})"))?
        }
    };
    if let Some(with) = with {
        match &mut select.with {
            Some(w) => {
                let mut ctes = with.ctes;
                ctes.append(&mut w.ctes);
                w.ctes = ctes;
            }
            None => select.with = Some(with),
        }
    }
    let new_map = (0..n)
        .map(|pos| targets.iter().position(|t| *t == pos))
        .collect();
    emit_fire_for_each_row(
        program,
        &mut resolver.fork(),
        connection,
        namespace,
        view_table,
        triggers,
        select,
        targets.len(),
        0,
        Some(new_map),
    )
}
