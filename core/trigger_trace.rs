//! Text reported to `sqlite3_trace_v2(SQLITE_TRACE_STMT)` when a trigger runs:
//! `-- TRIGGER <name>` and one `-- <step>` comment per trigger step.

use crate::schema::Trigger;
use turso_parser::lexer::Lexer;
use turso_parser::token::TokenType;

/// The comments SQLite reports when `trigger` fires, in order.
pub(crate) fn trace_lines(trigger: &Trigger) -> Vec<String> {
    let mut lines = vec![format!("-- TRIGGER {}", trigger.name)];
    lines.extend(
        step_texts(&trigger.sql)
            .into_iter()
            .map(|step| format!("-- {step}")),
    );
    lines
}

/// The source text of each statement in the BEGIN ... END body of a
/// CREATE TRIGGER, without the terminating semicolon.
fn step_texts(sql: &str) -> Vec<String> {
    let base = sql.as_ptr() as usize;
    let mut steps = Vec::new();
    let mut in_body = false;
    let mut start: Option<usize> = None;
    let mut end = 0;
    for token in Lexer::new(sql.as_bytes()) {
        let Ok(token) = token else { break };
        let offset = token.value.as_ptr() as usize - base;
        match token.token_type {
            TokenType::TK_NONE => continue,
            TokenType::TK_BEGIN if !in_body => in_body = true,
            _ if !in_body => {}
            TokenType::TK_SEMI => {
                if let Some(s) = start.take() {
                    steps.push(sql[s..end].to_string());
                }
            }
            _ => {
                start.get_or_insert(offset);
                end = offset + token.value.len();
            }
        }
    }
    // The closing END has no semicolon after it, so it never forms a step.
    steps
}

#[cfg(test)]
mod tests {
    use super::step_texts;

    #[test]
    fn splits_the_body_into_steps() {
        let sql = "CREATE TRIGGER tr AFTER INSERT ON t WHEN new.x > 0 BEGIN\n  \
                   INSERT INTO log VALUES (new.x); -- note\n  \
                   UPDATE t SET y = CASE WHEN x THEN 1 ELSE 2 END WHERE rowid = new.rowid;\n\
                   END";
        assert_eq!(
            step_texts(sql),
            [
                "INSERT INTO log VALUES (new.x)",
                "UPDATE t SET y = CASE WHEN x THEN 1 ELSE 2 END WHERE rowid = new.rowid",
            ]
        );
    }
}
