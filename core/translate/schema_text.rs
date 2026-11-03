//! The `sqlite_schema.sql` text of a `CREATE` statement, as SQLite stores it.
//!
//! SQLite keeps the statement as written, except that it starts the text with
//! `CREATE <kind> ` and the object name, so `TEMP`, `IF NOT EXISTS`, leading
//! whitespace and comments and a schema qualifier on the name are dropped
//! (`sqlite3EndTable`, `sqlite3CreateIndex`, `sqlite3FinishTrigger`). ALTER
//! TABLE edits that text in place (`renameEditSql`,
//! `sqlite3AlterFinishAddColumn`, `sqlite_drop_column`).

use turso_parser::ast;
use turso_parser::lexer::Lexer;
use turso_parser::parser::Parser;
use turso_parser::token::TokenType;

/// The kind of object a `CREATE` statement makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreateKind {
    Table,
    Index { unique: bool },
    View,
    Trigger,
}

struct Tok {
    ty: TokenType,
    start: usize,
    end: usize,
}

/// The text SQLite stores for `input`, a single `CREATE` statement of `kind`,
/// or `None` when `input` does not have the expected shape.
pub(crate) fn stored_create_sql(input: &str, kind: CreateKind) -> Option<String> {
    let toks = significant_tokens(input)?;

    let mut i = 0;
    let expect =
        |ty: TokenType, i: &mut usize| -> Option<()> { (toks.get(*i)?.ty == ty).then(|| *i += 1) };
    expect(TokenType::TK_CREATE, &mut i)?;
    if toks.get(i)?.ty == TokenType::TK_TEMP {
        i += 1;
    }
    let prefix = match kind {
        CreateKind::Table => {
            expect(TokenType::TK_TABLE, &mut i)?;
            "CREATE TABLE "
        }
        CreateKind::Index { unique } => {
            if unique {
                expect(TokenType::TK_UNIQUE, &mut i)?;
            }
            expect(TokenType::TK_INDEX, &mut i)?;
            if unique {
                "CREATE UNIQUE INDEX "
            } else {
                "CREATE INDEX "
            }
        }
        CreateKind::View => {
            expect(TokenType::TK_VIEW, &mut i)?;
            "CREATE VIEW "
        }
        CreateKind::Trigger => {
            expect(TokenType::TK_TRIGGER, &mut i)?;
            "CREATE TRIGGER "
        }
    };
    if toks.get(i..i + 3).is_some_and(|t| {
        t[0].ty == TokenType::TK_IF
            && t[1].ty == TokenType::TK_NOT
            && t[2].ty == TokenType::TK_EXISTS
    }) {
        i += 3;
    }

    // The name, maybe qualified: the text starts at its last part.
    let mut name_start = toks.get(i)?.start;
    i = skip_name(&toks, i)?;
    if toks.get(i).is_some_and(|t| t.ty == TokenType::TK_DOT) {
        name_start = toks.get(i + 1)?.start;
        skip_name(&toks, i + 1)?;
    }

    // The terminating semicolon, if the statement has one.
    let semi = toks
        .last()
        .filter(|t| t.ty == TokenType::TK_SEMI)
        .map(|t| t.start);
    let last = toks.iter().rev().find(|t| t.ty != TokenType::TK_SEMI)?;
    let end = match kind {
        // SQLite ends at the `)` of the column list, or at the token after the
        // table options, which includes what comes before a `;`.
        CreateKind::Table if last.ty != TokenType::TK_RP => semi.unwrap_or(input.len()),
        CreateKind::Index { .. } => semi.unwrap_or(input.len()),
        CreateKind::View => semi.unwrap_or(input.len()),
        _ => last.end,
    };
    if end <= name_start {
        return None;
    }
    let mut body = &input[name_start..end];
    if kind == CreateKind::View {
        body = body.trim_end();
    }
    Some(format!("{prefix}{body}"))
}

/// [stored_create_sql], but only when the text parses back to `expected`, the
/// statement being run without `TEMP`, `IF NOT EXISTS` and a name qualifier.
pub(crate) fn verified_create_sql(
    input: &str,
    kind: CreateKind,
    expected: &ast::Stmt,
) -> Option<String> {
    let sql = stored_create_sql(input, kind)?;
    let mut parser = Parser::new(sql.as_bytes());
    match parser.next_cmd() {
        Ok(Some(ast::Cmd::Stmt(stmt))) if stmt == *expected => Some(sql),
        _ => None,
    }
}

/// How [preserve_renamed_text] writes a new name, like SQLite's
/// `renameEditSql`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenameQuote {
    /// Always `"new"` (RENAME TO, or a quoted new column name).
    Always,
    /// Bare where the old token was bare, quoted where it was quoted.
    AsWritten,
}

/// An identifier or string token: its span and dequoted text.
struct NameTok {
    start: usize,
    end: usize,
    quoted: bool,
    value: String,
}

fn name_tokens(sql: &str) -> Option<Vec<NameTok>> {
    let mut out = Vec::new();
    let mut bracket: Option<usize> = None;
    // The tokens partition the input, but some carry static text, so spans
    // come from their lengths.
    let mut end = 0;
    for token in Lexer::new(sql.as_bytes()) {
        let token = token.ok()?;
        let start = end;
        end += token.value.len();
        if let Some(open) = bracket {
            if token.token_type == TokenType::TK_RBRACKET {
                out.push(NameTok {
                    start: open,
                    end,
                    quoted: true,
                    value: sql[open + 1..start].to_string(),
                });
                bracket = None;
            }
            continue;
        }
        match token.token_type {
            TokenType::TK_LBRACKET => bracket = Some(start),
            TokenType::TK_ID | TokenType::TK_STRING => {
                let text = &sql[start..end];
                let quoted = matches!(text.as_bytes()[0], b'"' | b'\'' | b'`');
                let value = if quoted {
                    let q = &text[..1];
                    text[1..text.len() - 1].replace(&q.repeat(2), q)
                } else {
                    text.to_string()
                };
                out.push(NameTok {
                    start,
                    end,
                    quoted,
                    value,
                });
            }
            _ => {}
        }
    }
    (end == sql.len()).then_some(out)
}

/// Rewrites `old_sql`, the stored text of a schema object, so that it says
/// what `new_sql` says when a rename only changed identifiers: the renamed
/// tokens are replaced in place and the rest of the text is kept, as SQLite
/// does. `new_name` is the new name as written in the ALTER statement.
/// Returns `new_sql` when the two cannot be matched token for token.
pub(crate) fn preserve_renamed_text(
    old_sql: &str,
    new_sql: String,
    new_name: &str,
    quote: RenameQuote,
) -> String {
    patch_renamed(old_sql, &new_sql, new_name, quote).unwrap_or(new_sql)
}

fn patch_renamed(
    old_sql: &str,
    new_sql: &str,
    new_name: &str,
    quote: RenameQuote,
) -> Option<String> {
    let mut parser = Parser::new(old_sql.as_bytes());
    let Ok(Some(ast::Cmd::Stmt(old_stmt))) = parser.next_cmd() else {
        return None;
    };
    // The canonical texts of the old and new statements line up token for
    // token; the written text lines up with the old canonical one.
    let canonical = old_stmt.to_string();
    let written = name_tokens(old_sql)?;
    let old = name_tokens(&canonical)?;
    let new = name_tokens(new_sql)?;
    if written.len() != old.len() || old.len() != new.len() {
        return None;
    }
    let mut out = String::with_capacity(old_sql.len() + 16);
    let mut pos = 0;
    for ((w, o), n) in written.iter().zip(&old).zip(&new) {
        if !w.value.eq_ignore_ascii_case(&o.value) {
            return None;
        }
        if o.value == n.value {
            continue;
        }
        out.push_str(&old_sql[pos..w.start]);
        // The rewrite may have folded the case of the new name.
        let value = if n.value.eq_ignore_ascii_case(new_name) {
            new_name
        } else {
            n.value.as_str()
        };
        if quote == RenameQuote::AsWritten && !w.quoted {
            out.push_str(value);
        } else {
            out.push('"');
            out.push_str(&value.replace('"', "\"\""));
            out.push('"');
        }
        pos = w.end;
    }
    out.push_str(&old_sql[pos..]);
    Some(out)
}

fn significant_tokens(sql: &str) -> Option<Vec<Tok>> {
    let mut toks = Vec::new();
    let mut end = 0;
    for token in Lexer::new(sql.as_bytes()) {
        let token = token.ok()?;
        let start = end;
        end += token.value.len();
        if token.token_type != TokenType::TK_NONE {
            toks.push(Tok {
                ty: token.token_type,
                start,
                end,
            });
        }
    }
    (end == sql.len()).then_some(toks)
}

/// The start of a column definition, and of the `,` before it.
type ColumnStart = (usize, Option<usize>);

/// Where each column definition of a `CREATE TABLE` text starts, and the
/// offset SQLite adds new columns at: the `,` before the table constraints,
/// or else the closing `)`.
fn column_layout(sql: &str) -> Option<(Vec<ColumnStart>, usize)> {
    let toks = significant_tokens(sql)?;
    let lp = toks.iter().position(|t| t.ty == TokenType::TK_LP)?;
    let mut columns = vec![(toks.get(lp + 1)?.start, None)];
    let mut depth = 1;
    let mut i = lp + 1;
    while let Some(t) = toks.get(i) {
        match t.ty {
            TokenType::TK_LP => depth += 1,
            TokenType::TK_RP => {
                depth -= 1;
                if depth == 0 {
                    return Some((columns, t.start));
                }
            }
            TokenType::TK_COMMA if depth == 1 => {
                let next = toks.get(i + 1)?;
                if matches!(
                    next.ty,
                    TokenType::TK_CONSTRAINT
                        | TokenType::TK_PRIMARY
                        | TokenType::TK_UNIQUE
                        | TokenType::TK_CHECK
                        | TokenType::TK_FOREIGN
                ) {
                    return Some((columns, t.start));
                }
                columns.push((next.start, Some(t.start)));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The table text after `ALTER TABLE ... ADD COLUMN`, as SQLite writes it
/// (`sqlite3AlterFinishAddColumn`): the column definition as written in
/// `alter_sql`, spliced in after the last column of `table_sql`.
pub(crate) fn table_sql_with_added_column(table_sql: &str, alter_sql: &str) -> Option<String> {
    let (_, offset) = column_layout(table_sql)?;
    let toks = significant_tokens(alter_sql)?;
    let add = toks.iter().position(|t| t.ty == TokenType::TK_ADD)?;
    let mut col = add + 1;
    if toks.get(col)?.ty == TokenType::TK_COLUMNKW {
        col += 1;
    }
    let def = alter_sql[toks.get(col)?.start..]
        .trim_end_matches(|c: char| c == ';' || c.is_ascii_whitespace());
    Some(format!(
        "{}, {def}{}",
        &table_sql[..offset],
        &table_sql[offset..]
    ))
}

/// The table text after `ALTER TABLE ... DROP COLUMN` of column `index`,
/// as SQLite writes it (`sqlite_drop_column`).
pub(crate) fn table_sql_without_column(table_sql: &str, index: usize) -> Option<String> {
    let (columns, offset) = column_layout(table_sql)?;
    let (start, end) = match columns.get(index + 1) {
        Some(&(next, _)) => (columns.get(index)?.0, next),
        None => (columns.get(index)?.1?, offset),
    };
    Some(format!("{}{}", &table_sql[..start], &table_sql[end..]))
}

/// The index after the name element at `i`: one token, or a `[...]` group.
fn skip_name(toks: &[Tok], i: usize) -> Option<usize> {
    if toks.get(i)?.ty != TokenType::TK_LBRACKET {
        return Some(i + 1);
    }
    let close = toks[i..]
        .iter()
        .position(|t| t.ty == TokenType::TK_RBRACKET)?;
    Some(i + close + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_text_matches_sqlite() {
        let cases = [
            (
                "CREATE TABLE t1(x) ;",
                CreateKind::Table,
                "CREATE TABLE t1(x)",
            ),
            (
                "CREATE TABLE t2(x) /*c*/",
                CreateKind::Table,
                "CREATE TABLE t2(x)",
            ),
            (
                "create temp table if not exists main2(x)",
                CreateKind::Table,
                "CREATE TABLE main2(x)",
            ),
            (
                "CREATE TABLE main.\"Q\"\"t\" ( x )",
                CreateKind::Table,
                "CREATE TABLE \"Q\"\"t\" ( x )",
            ),
            (
                "CREATE TABLE [b r](x)",
                CreateKind::Table,
                "CREATE TABLE [b r](x)",
            ),
            (
                "CREATE TABLE t5(x PRIMARY KEY) WITHOUT ROWID /*c*/ ;",
                CreateKind::Table,
                "CREATE TABLE t5(x PRIMARY KEY) WITHOUT ROWID /*c*/ ",
            ),
            (
                "-- lead\n  CREATE TABLE t6(x)",
                CreateKind::Table,
                "CREATE TABLE t6(x)",
            ),
            (
                "CREATE INDEX i1 ON t1(x) ;",
                CreateKind::Index { unique: false },
                "CREATE INDEX i1 ON t1(x) ",
            ),
            (
                "create unique index if not exists main.i3 on t1 ( x ) where x>0 /*c*/;",
                CreateKind::Index { unique: true },
                "CREATE UNIQUE INDEX i3 on t1 ( x ) where x>0 /*c*/",
            ),
            (
                "CREATE VIEW v1 AS SELECT 1 ;",
                CreateKind::View,
                "CREATE VIEW v1 AS SELECT 1",
            ),
            (
                "create temp view if not exists v3(a) as select 1   ;",
                CreateKind::View,
                "CREATE VIEW v3(a) as select 1",
            ),
            (
                "CREATE TRIGGER tr1 AFTER INSERT ON t1 BEGIN SELECT 1; END ;",
                CreateKind::Trigger,
                "CREATE TRIGGER tr1 AFTER INSERT ON t1 BEGIN SELECT 1; END",
            ),
            (
                "create temp trigger if not exists tr2 after insert on main.t1 begin select 1; end",
                CreateKind::Trigger,
                "CREATE TRIGGER tr2 after insert on main.t1 begin select 1; end",
            ),
        ];
        for (input, kind, expected) in cases {
            assert_eq!(
                stored_create_sql(input, kind).as_deref(),
                Some(expected),
                "{input}"
            );
        }
        assert_eq!(
            stored_create_sql("CREATE VIEW v AS SELECT 1", CreateKind::Table),
            None
        );
    }

    #[test]
    fn add_and_drop_column_edit_the_written_text() {
        let t = "CREATE TABLE t2 ( a , b, CHECK (a > 0) , UNIQUE(a) )";
        assert_eq!(
            table_sql_with_added_column(t, "ALTER TABLE t2 ADD COLUMN \"Z z\" TEXT /* c */ ;")
                .as_deref(),
            Some("CREATE TABLE t2 ( a , b, \"Z z\" TEXT /* c */, CHECK (a > 0) , UNIQUE(a) )")
        );
        assert_eq!(
            table_sql_with_added_column("CREATE TABLE t1(a INT)", "alter table main.t1 add e")
                .as_deref(),
            Some("CREATE TABLE t1(a INT, e)")
        );
        let t = "CREATE TABLE t3(a, b /* bee */, c -- cee\n)";
        let t = table_sql_without_column(t, 1).unwrap();
        assert_eq!(t, "CREATE TABLE t3(a, c -- cee\n)");
        assert_eq!(
            table_sql_without_column(&t, 1).as_deref(),
            Some("CREATE TABLE t3(a)")
        );
        assert_eq!(
            table_sql_without_column("CREATE TABLE t(a, b DEFAULT (1+2), c)", 0).as_deref(),
            Some("CREATE TABLE t(b DEFAULT (1+2), c)")
        );
    }

    #[test]
    fn rename_keeps_the_written_text() {
        let old = "CREATE INDEX i on t( a , \"b\" )";
        let new = "CREATE INDEX i ON u (a, b)".to_string();
        assert_eq!(
            preserve_renamed_text(old, new, "U", RenameQuote::Always),
            "CREATE INDEX i on \"U\"( a , \"b\" )"
        );
        let old = "CREATE TABLE t(a, [b c] , CHECK(a > 0))";
        let new = "CREATE TABLE t (x, \"y z\", CHECK (x > 0))".to_string();
        assert_eq!(
            preserve_renamed_text(old, new, "x", RenameQuote::AsWritten),
            "CREATE TABLE t(x, \"y z\" , CHECK(x > 0))"
        );
        // Texts that do not line up fall back to the new text.
        let new = "CREATE TABLE t (a, b, c)".to_string();
        assert_eq!(
            preserve_renamed_text(
                "CREATE TABLE t(a, b)",
                new.clone(),
                "t",
                RenameQuote::Always
            ),
            new
        );
    }
}
