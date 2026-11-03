//! SQLite's text and extended codes for `LimboError`, applied at the C boundary.
//!
//! Core keeps its own `Display` (the CLI and many tests match "Parse error: ...");
//! consumers of libsqlite3 match SQLite's exact wording.

use turso_core::{ConstraintKind, LimboError};
use turso_parser::error::Error as ParserError;

pub const SQLITE_ERROR_MISSING_COLLSEQ: i32 = 257;
pub const SQLITE_CONSTRAINT_CHECK: i32 = 275;
pub const SQLITE_CONSTRAINT_FOREIGNKEY: i32 = 787;
pub const SQLITE_CONSTRAINT_NOTNULL: i32 = 1299;
pub const SQLITE_CONSTRAINT_PRIMARYKEY: i32 = 1555;
pub const SQLITE_CONSTRAINT_UNIQUE: i32 = 2067;
pub const SQLITE_CONSTRAINT_DATATYPE: i32 = 3091;

fn is_id_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

/// SQLite reports a malformed token as `unrecognized token: "<text>"`, where the
/// text runs through the identifier characters that follow it.
fn unrecognized(token: &str, offset: usize, sql: Option<&str>, widen: bool) -> String {
    let mut text = token.to_string();
    if let (true, Some(sql)) = (widen, sql) {
        if let Some(head) = sql.get(..offset.min(sql.len())) {
            let start = head
                .char_indices()
                .rev()
                .take_while(|&(_, c)| is_id_char(c) || c == '.')
                .last()
                .map_or(head.len(), |(i, _)| i);
            let rest = &sql[start..];
            let end = rest
                .char_indices()
                .find(|&(_, c)| !(is_id_char(c) || c == '.'))
                .map_or(rest.len(), |(i, _)| i);
            if start + end > offset {
                text = rest[..end].to_string();
            }
        }
    }
    format!("unrecognized token: \"{text}\"")
}

fn parser_message(err: &ParserError, sql: Option<&str>) -> String {
    match err {
        ParserError::UnterminatedLiteral {
            token_text, offset, ..
        }
        | ParserError::UnterminatedBracket {
            token_text, offset, ..
        }
        | ParserError::ExpectedEqualsSign {
            token_text, offset, ..
        }
        | ParserError::UnrecognizedToken {
            token_text, offset, ..
        } => unrecognized(token_text, *offset, sql, false),
        ParserError::BadNumber {
            token_text, offset, ..
        }
        | ParserError::BadFractionalPart {
            token_text, offset, ..
        }
        | ParserError::BadExponentPart {
            token_text, offset, ..
        }
        | ParserError::MalformedHexInteger {
            token_text, offset, ..
        } => unrecognized(token_text, *offset, sql, true),
        ParserError::ParseUnexpectedToken { token_text, .. }
            if matches!(token_text.as_str(), ":" | "#") =>
        {
            unrecognized(token_text, 0, None, false)
        }
        ParserError::ParseError(msg) | ParserError::Custom(msg) => rewrite(msg),
        other => other.to_string(),
    }
}

/// Core's wording for a few messages that differ from SQLite's.
fn rewrite(msg: &str) -> String {
    let msg = msg.strip_prefix("Parse error: ").unwrap_or(msg);
    for (from, to) in REWRITES {
        if let Some(rest) = msg.strip_prefix(from) {
            return format!("{to}{rest}");
        }
    }
    if let Some(head) = msg.strip_suffix(" in ALTER TABLE statement") {
        if head.starts_with("Cannot add a ") {
            return head.to_string();
        }
    }
    // cannot drop column "x": PRIMARY KEY  ->  cannot drop PRIMARY KEY column: "x"
    if let Some(rest) = msg.strip_prefix("cannot drop column ") {
        for kind in ["PRIMARY KEY", "UNIQUE"] {
            if let Some(name) = rest.strip_suffix(&format!(": {kind}")) {
                return format!("cannot drop {kind} column: {name}");
            }
        }
    }
    if msg.starts_with("coalesce function with less than") {
        return "wrong number of arguments to function coalesce()".to_string();
    }
    msg.to_string()
}

const REWRITES: &[(&str, &str)] = &[
    (
        "Object name reserved for internal use",
        "object name reserved for internal use",
    ),
    ("No such index", "no such index"),
];

/// The message `sqlite3_errmsg` reports for `err`.
pub fn message(err: &LimboError, sql: Option<&str>) -> String {
    match err {
        LimboError::ParseError(msg)
        | LimboError::InternalError(msg)
        | LimboError::InvalidArgument(msg)
        | LimboError::SqlError(msg) => rewrite(msg),
        LimboError::ParseIntError(_)
        | LimboError::ParseFloatError(_)
        | LimboError::InvalidDate(_)
        | LimboError::InvalidTime(_) => rewrite(&err.to_string()),
        // Application-defined functions report their own text.
        LimboError::ExtensionError(msg) => msg.clone(),
        // Like SQLite, corruption has one standard text.
        LimboError::Corrupt(_) => "database disk image is malformed".to_string(),
        LimboError::LexerError(e) => parser_message(e, sql),
        // Core appends the result code the shell would print.
        LimboError::Constraint(msg) => strip_code_suffix(msg).to_string(),
        LimboError::ConstraintViolation(v) => strip_code_suffix(&v.message).to_string(),
        _ => err.to_string(),
    }
}

fn strip_code_suffix(msg: &str) -> &str {
    if let Some(head) = msg.strip_suffix(')') {
        if let Some((head, digits)) = head.rsplit_once(" (") {
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                return head;
            }
        }
    }
    msg
}

/// Extended result code for a constraint or collation error, or `None`.
pub fn extended_code(err: &LimboError) -> Option<i32> {
    match err {
        LimboError::ConstraintViolation(v) => Some(match v.kind {
            ConstraintKind::PrimaryKey => SQLITE_CONSTRAINT_PRIMARYKEY,
            ConstraintKind::Unique => SQLITE_CONSTRAINT_UNIQUE,
            ConstraintKind::ForeignKey => SQLITE_CONSTRAINT_FOREIGNKEY,
            ConstraintKind::NotNull => SQLITE_CONSTRAINT_NOTNULL,
            ConstraintKind::Check => SQLITE_CONSTRAINT_CHECK,
            ConstraintKind::Other if v.message.starts_with("cannot store ") => {
                SQLITE_CONSTRAINT_DATATYPE
            }
            ConstraintKind::Other => return None,
        }),
        LimboError::ForeignKeyConstraint(_) => Some(SQLITE_CONSTRAINT_FOREIGNKEY),
        LimboError::Constraint(msg) if msg.starts_with("cannot store ") => {
            Some(SQLITE_CONSTRAINT_DATATYPE)
        }
        LimboError::ParseError(msg) if msg.starts_with("no such collation sequence") => {
            Some(SQLITE_ERROR_MISSING_COLLSEQ)
        }
        _ => None,
    }
}
