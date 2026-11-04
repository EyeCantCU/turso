//! PRAGMA foreign_key_check: report child rows whose parent key is missing.

use crate::namespace::NamespaceId;
use crate::schema::Schema;
use crate::util::normalize_ident;
use crate::Result;

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// A SELECT yielding SQLite's `table|rowid|parent|fkid` rows for every
/// violation in `table` (or in every table), or None if there is nothing to check.
pub(crate) fn foreign_key_check_sql(
    schema: &Schema,
    namespace: NamespaceId,
    table: Option<&str>,
) -> Result<Option<String>> {
    let mut children: Vec<_> = schema
        .tables(namespace)
        .filter_map(|t| t.btree())
        .filter(|t| !t.foreign_keys.is_empty())
        .filter(|t| table.is_none_or(|name| t.name.eq_ignore_ascii_case(&normalize_ident(name))))
        .collect();
    // SQLite walks its table hash, which for a small schema lists the most
    // recently created table first.
    children.sort_by_key(|t| std::cmp::Reverse(t.root_page));
    let mut selects = Vec::new();
    for (ordinal, child) in children.into_iter().enumerate() {
        let rowid = if child.has_rowid { "rowid" } else { "NULL" };
        // foreign_key_list numbers the constraints from the last declared one.
        let mut fks: Vec<_> = child.foreign_keys.iter().collect();
        fks.sort_by_key(|fk| std::cmp::Reverse(fk.decl_order));
        for (fkid, fk) in fks.into_iter().enumerate() {
            let child_ref = |c: &str| format!("{}.{}", quote(&child.name), quote(c));
            let not_null = fk
                .child_columns
                .iter()
                .map(|c| format!("{} IS NOT NULL", child_ref(c)))
                .collect::<Vec<_>>()
                .join(" AND ");
            let missing_parent = match schema.get_btree_table(fk.parent_namespace, &fk.parent_table)
            {
                None => "1".to_string(),
                Some(parent) => {
                    // A parent key without a PRIMARY KEY or UNIQUE index is an error.
                    schema.resolve_fk(namespace, fk, &child, &parent, true)?;
                    let parent_cols: Vec<String> = if !fk.parent_columns.is_empty() {
                        fk.parent_columns.to_vec()
                    } else if parent.primary_key_columns.is_empty() {
                        vec!["rowid".to_string()]
                    } else {
                        parent
                            .primary_key_columns
                            .iter()
                            .map(|(c, _)| c.clone())
                            .collect()
                    };
                    let matches = parent_cols
                        .iter()
                        .zip(fk.child_columns.iter())
                        .map(|(p, c)| format!("p.{} = {}", quote(p), child_ref(c)))
                        .collect::<Vec<_>>()
                        .join(" AND ");
                    format!(
                        "NOT EXISTS (SELECT 1 FROM {} AS p WHERE {matches})",
                        quote(&parent.name)
                    )
                }
            };
            selects.push(format!(
                "SELECT {ordinal} AS o, {} AS \"table\", {rowid} AS rowid, {} AS parent, \
                 {fkid} AS fkid FROM {} WHERE {not_null} AND {missing_parent}",
                literal(&child.name),
                literal(&fk.parent_table),
                quote(&child.name),
            ));
        }
    }
    if selects.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "SELECT \"table\", rowid, parent, fkid FROM ({}) ORDER BY o, rowid, fkid",
        selects.join(" UNION ALL ")
    )))
}
