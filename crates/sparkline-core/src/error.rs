//! Error types, including a compact representation of Spark's structured
//! errors (error class, SQLSTATE and the position in the query).

use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{0}")]
    Config(String),
    #[error("cannot connect to Spark Connect at {endpoint}: {detail}")]
    Connect { endpoint: String, detail: String },
    #[error("{0}")]
    Spark(SparkError),
    #[error("Spark Connect server unavailable: {0}")]
    Unavailable(String),
    #[error("query cancelled")]
    Cancelled,
    #[error("failed to decode Arrow data: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    #[error("gRPC error ({code:?}): {message}")]
    Rpc { code: tonic::Code, message: String },
}

/// A Spark-side error (analysis error, parse error, runtime failure...).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SparkError {
    pub error_class: Option<String>,
    pub sql_state: Option<String>,
    /// Human-readable message with plan dumps and Spark's own `== SQL ==`
    /// excerpt removed (sparkline renders its own).
    pub message: String,
    /// Character range `[start, stop]` (inclusive) in the statement that the
    /// error points at, from Spark's query context.
    pub span: Option<(usize, usize)>,
}

impl SparkError {
    pub fn from_message(raw: &str) -> Self {
        SparkError {
            message: clean_message(raw),
            ..Default::default()
        }
    }

    /// Where to draw the caret in `stmt`: the query-context span if Spark sent
    /// one, else `line L, pos P` parsed out of the message (parse errors).
    /// Returns a character range `[start, stop]`.
    pub fn caret_span(&self, stmt: &str) -> Option<(usize, usize)> {
        let len = stmt.chars().count();
        if let Some((s, e)) = self.span {
            if s < len {
                return Some((s, e.min(len.saturating_sub(1)).max(s)));
            }
        }
        let (line, pos) = parse_line_pos(&self.message)?;
        let mut offset = 0;
        for (i, l) in stmt.split('\n').enumerate() {
            if i + 1 == line {
                let p = offset + pos.min(l.chars().count());
                return (p <= len).then_some((p, p));
            }
            offset += l.chars().count() + 1;
        }
        None
    }
}

impl fmt::Display for SparkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Strips the parts of Spark's error text that aren't useful in a terminal:
/// the analyzed-plan dump after `;\n`, Spark's own `== SQL ==` excerpt, and
/// JVM stack traces.
pub fn clean_message(raw: &str) -> String {
    let mut msg = raw.trim();
    for marker in [
        "\n== SQL",
        "\n== SQL(",
        ";\n'",
        ";\n+-",
        "\n\tat ",
        "\nJVM stacktrace:",
    ] {
        if let Some(i) = msg.find(marker) {
            msg = &msg[..i];
        }
    }
    let msg = msg.trim().trim_end_matches(';').trim();
    msg.to_string()
}

/// Finds `line L, pos P` or `line L pos P` in a Spark message.
fn parse_line_pos(msg: &str) -> Option<(usize, usize)> {
    let i = msg.rfind("line ")?;
    let rest = &msg[i + 5..];
    let (line, rest) = take_number(rest)?;
    let rest = rest.trim_start_matches(',').trim_start();
    let rest = rest.strip_prefix("pos ")?;
    let (pos, _) = take_number(rest)?;
    Some((line, pos))
}

fn take_number(s: &str) -> Option<(usize, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    Some((s[..end].parse().ok()?, &s[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_plan_dump() {
        let raw = "[TABLE_OR_VIEW_NOT_FOUND] The table or view `nope` cannot be found. SQLSTATE: 42P01; line 1 pos 14;\n'Project [*]\n+- 'UnresolvedRelation [nope], [], false\n";
        assert_eq!(
            clean_message(raw),
            "[TABLE_OR_VIEW_NOT_FOUND] The table or view `nope` cannot be found. SQLSTATE: 42P01; line 1 pos 14"
        );
    }

    #[test]
    fn cleans_parse_excerpt() {
        let raw = "\n[PARSE_SYNTAX_ERROR] Syntax error at or near 'FORM'. SQLSTATE: 42601 (line 1, pos 9)\n\n== SQL ==\nSELECT 1 FORM t\n---------^^^\n";
        let e = SparkError::from_message(raw);
        assert_eq!(
            e.message,
            "[PARSE_SYNTAX_ERROR] Syntax error at or near 'FORM'. SQLSTATE: 42601 (line 1, pos 9)"
        );
        assert_eq!(e.caret_span("SELECT 1 FORM t"), Some((9, 9)));
    }

    #[test]
    fn caret_on_second_line() {
        let e = SparkError::from_message("bad thing; line 2 pos 3");
        assert_eq!(e.caret_span("SELECT\nFROM x"), Some((10, 10)));
    }

    #[test]
    fn span_preferred_and_clamped() {
        let e = SparkError {
            span: Some((14, 99)),
            message: "x line 1 pos 0".into(),
            ..Default::default()
        };
        assert_eq!(e.caret_span("SELECT * FROM nope"), Some((14, 17)));
    }
}
