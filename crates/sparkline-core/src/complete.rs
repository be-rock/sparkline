//! Context analysis for SQL completion.
//!
//! This is deliberately heuristic and token-based rather than a real parser:
//! the input is almost always an incomplete statement, which strict SQL
//! parsers reject. Everything here is pure so it can be unit-tested; the
//! front-end combines the result with the catalog cache.

use crate::lexer::{tokenize, Token, TokenKind};

/// What kind of name is expected at the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// After `FROM`, `JOIN`, `INTO`, `TABLE`, `UPDATE`, `DESCRIBE`, ...
    Table,
    /// After `USE`, `SHOW TABLES IN`, ...
    Database,
    /// `qualifier.<partial>`: the qualifier is a table/alias (→ columns) or a
    /// database (→ tables). The catalog decides which.
    Member { qualifier: Vec<String> },
    /// Anything: columns, functions, keywords, tables.
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    pub expect: Expect,
    /// The partial word being completed (after the last `.`), unquoted.
    pub partial: String,
    /// Byte offset where the replacement span starts.
    pub replace_start: usize,
    /// Tables (and their aliases) referenced in the current statement.
    pub tables: Vec<TableRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    /// Name parts as written, unquoted (e.g. `["db", "events"]`).
    pub name: Vec<String>,
    pub alias: Option<String>,
    /// Defined by a `WITH name AS (...)` clause in this statement.
    pub is_cte: bool,
}

impl TableRef {
    pub fn qualified(&self) -> String {
        self.name.join(".")
    }

    /// Does `ident` refer to this table (by alias, or by its last name part)?
    pub fn matches(&self, ident: &str) -> bool {
        match &self.alias {
            Some(a) => a.eq_ignore_ascii_case(ident),
            None => self
                .name
                .last()
                .is_some_and(|n| n.eq_ignore_ascii_case(ident)),
        }
    }
}

/// Returns `None` when completion makes no sense (cursor inside a string or
/// comment).
pub fn analyze(line: &str, pos: usize) -> Option<Context> {
    let pos = pos.min(line.len());
    let before = &line[..pos];
    let tokens = tokenize(before);
    if let Some(last) = tokens.last() {
        let inside = matches!(
            last.kind,
            TokenKind::String | TokenKind::Comment | TokenKind::Variable
        ) && (last.unterminated
            || (last.kind == TokenKind::Comment && last.text(before).starts_with("--")));
        if inside {
            return None;
        }
    }

    // Statement boundaries: completion only looks at the statement under the cursor.
    let stmt_start = tokens
        .iter()
        .rev()
        .find(|t| t.is(before, ";"))
        .map_or(0, |t| t.end);
    let stmt_end = tokenize(&line[pos..])
        .iter()
        .find(|t| t.is(&line[pos..], ";"))
        .map_or(line.len(), |t| pos + t.start);

    // The word under the cursor: a dotted chain of identifiers ending at pos.
    let (chain_start, replace_start, mut parts) = trailing_name_chain(before, &tokens);
    let partial = parts.pop().unwrap_or_default();

    // Table references in the statement, excluding the name being typed
    // (otherwise `FROM sales.` would make `sales` look like a table).
    let chain_start_in_stmt = chain_start.max(stmt_start);
    let statement = format!(
        "{}{}",
        &line[stmt_start..chain_start_in_stmt],
        &line[pos..stmt_end.max(pos)]
    );
    let tables = table_refs(&statement);

    let expect = if !parts.is_empty() {
        Expect::Member { qualifier: parts }
    } else {
        let sig: Vec<&Token> = tokens[..]
            .iter()
            .filter(|t| t.start >= stmt_start && t.end <= chain_start && !t.is_trivia())
            .collect();
        expect_from_previous(before, &sig)
    };

    Some(Context {
        expect,
        partial,
        replace_start,
        tables,
    })
}

fn expect_from_previous(src: &str, sig: &[&Token]) -> Expect {
    let Some(prev) = sig.last() else {
        return Expect::Any;
    };
    let kw = |t: &Token| (t.kind == TokenKind::Keyword).then(|| t.text(src).to_ascii_uppercase());
    let prev_kw = kw(prev);

    if let Some(k) = prev_kw.as_deref() {
        match k {
            "FROM" | "JOIN" | "INTO" | "UPDATE" | "DESCRIBE" | "DESC" => {
                // `DELETE FROM`, `SHOW TABLES FROM db` are handled below.
                if k == "FROM" && preceded_by(src, sig, &["TABLES"]) {
                    return Expect::Database;
                }
                return Expect::Table;
            }
            "TABLE" => {
                // `CREATE TABLE <new name>`: nothing useful to complete.
                if preceded_by(src, sig, &["CREATE", "EXISTS", "REPLACE"]) {
                    return Expect::Any;
                }
                return Expect::Table;
            }
            "USE" | "DATABASE" | "SCHEMA" | "NAMESPACE" => return Expect::Database,
            "IN" if preceded_by(src, sig, &["TABLES", "VIEWS", "FUNCTIONS"]) => {
                return Expect::Database
            }
            "IN" if preceded_by(src, sig, &["COLUMNS"]) => return Expect::Table,
            _ => {}
        }
    }

    // `FROM a, <cursor>` → another table.
    if prev.is(src, ",") {
        let clause = sig
            .iter()
            .rev()
            .filter_map(|t| kw(t))
            .find(|k| CLAUSE_KEYWORDS.contains(&k.as_str()));
        if clause.as_deref() == Some("FROM") && paren_depth_since_clause(src, sig) == 0 {
            return Expect::Table;
        }
    }
    Expect::Any
}

const CLAUSE_KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "GROUP",
    "HAVING",
    "ORDER",
    "LIMIT",
    "JOIN",
    "ON",
    "SET",
    "VALUES",
    "WINDOW",
    "SORT",
    "CLUSTER",
    "DISTRIBUTE",
    "USING",
];

/// Is the token before `sig.last()` one of `words`?
fn preceded_by(src: &str, sig: &[&Token], words: &[&str]) -> bool {
    sig.len() >= 2
        && words
            .iter()
            .any(|w| sig[sig.len() - 2].text(src).eq_ignore_ascii_case(w))
}

/// Net paren depth between the last FROM and the end (a `,` inside
/// `FROM f(a, ` is an argument separator, not a table list).
fn paren_depth_since_clause(src: &str, sig: &[&Token]) -> i32 {
    let from = sig.iter().rposition(|t| t.is_kw(src, "FROM")).unwrap_or(0);
    sig[from..].iter().fold(0, |d, t| {
        if t.is(src, "(") {
            d + 1
        } else if t.is(src, ")") {
            d - 1
        } else {
            d
        }
    })
}

/// Finds the `a.b.c` chain that ends exactly at the end of `src`.
///
/// Returns (offset where the chain starts, offset where the final part starts,
/// unquoted parts). A trailing `.` (or no name at all) yields an empty final
/// part.
fn trailing_name_chain(src: &str, tokens: &[Token]) -> (usize, usize, Vec<String>) {
    let mut idx = tokens.len();
    let mut parts = Vec::new();
    let (mut start, replace_start) = match tokens.last() {
        Some(t) if is_name(t) => {
            parts.push(unquote(t.text(src)));
            idx -= 1;
            (t.start, t.start)
        }
        Some(t) if t.is(src, ".") => {
            parts.push(String::new());
            (src.len(), src.len())
        }
        _ => return (src.len(), src.len(), vec![String::new()]),
    };
    while idx >= 2 && tokens[idx - 1].is(src, ".") && is_name(&tokens[idx - 2]) {
        parts.push(unquote(tokens[idx - 2].text(src)));
        start = tokens[idx - 2].start;
        idx -= 2;
    }
    parts.reverse();
    (start, replace_start, parts)
}

fn is_name(t: &Token) -> bool {
    matches!(
        t.kind,
        TokenKind::Ident | TokenKind::Keyword | TokenKind::QuotedIdent
    )
}

pub fn unquote(s: &str) -> String {
    match s.strip_prefix('`') {
        Some(inner) => inner.strip_suffix('`').unwrap_or(inner).replace("``", "`"),
        None => s.to_string(),
    }
}

/// Quote an identifier with backticks if it isn't a plain identifier.
pub fn quote_ident(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_alphanumeric() || c == '_');
    if plain {
        s.to_string()
    } else {
        format!("`{}`", s.replace('`', "``"))
    }
}

/// Words that can't be a table alias.
const NOT_ALIAS: &[&str] = &[
    "WHERE",
    "JOIN",
    "ON",
    "LEFT",
    "RIGHT",
    "INNER",
    "OUTER",
    "FULL",
    "CROSS",
    "SEMI",
    "ANTI",
    "NATURAL",
    "GROUP",
    "ORDER",
    "LIMIT",
    "HAVING",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "MINUS",
    "LATERAL",
    "USING",
    "WINDOW",
    "CLUSTER",
    "DISTRIBUTE",
    "SORT",
    "SELECT",
    "FROM",
    "AS",
    "TABLESAMPLE",
    "PIVOT",
    "UNPIVOT",
    "SET",
    "VALUES",
    "WHEN",
    "THEN",
    "OFFSET",
    "VERSION",
    "TIMESTAMP",
    "FOR",
    "WITH",
];

/// Extracts table references (`FROM`/`JOIN` targets with optional aliases,
/// plus CTE names) from a statement.
pub fn table_refs(stmt: &str) -> Vec<TableRef> {
    let toks: Vec<Token> = tokenize(stmt)
        .into_iter()
        .filter(|t| !t.is_trivia())
        .collect();
    let mut refs = Vec::new();
    let mut in_from = false;
    let mut depth_at_from = 0;
    let mut depth = 0i32;
    let mut i = 0;

    while i < toks.len() {
        let t = &toks[i];
        let text = t.text(stmt);
        if t.is(stmt, "(") {
            depth += 1;
        } else if t.is(stmt, ")") {
            depth -= 1;
            if depth < depth_at_from {
                in_from = false;
            }
        }

        // CTE: `name AS (`
        if is_name(t)
            && t.kind != TokenKind::Keyword
            && toks.get(i + 1).is_some_and(|n| n.is_kw(stmt, "AS"))
            && toks.get(i + 2).is_some_and(|n| n.is(stmt, "("))
        {
            refs.push(TableRef {
                name: vec![unquote(text)],
                alias: None,
                is_cte: true,
            });
        }

        let starts_ref = t.is_kw(stmt, "FROM")
            || t.is_kw(stmt, "JOIN")
            || t.is_kw(stmt, "UPDATE")
            || t.is_kw(stmt, "INTO")
            || (in_from && depth == depth_at_from && t.is(stmt, ","));
        if t.is_kw(stmt, "FROM") || t.is_kw(stmt, "JOIN") {
            in_from = true;
            depth_at_from = depth;
        } else if t.kind == TokenKind::Keyword
            && matches!(
                text.to_ascii_uppercase().as_str(),
                "WHERE" | "GROUP" | "ORDER" | "HAVING" | "LIMIT" | "UNION" | "WINDOW"
            )
            && depth == depth_at_from
        {
            in_from = false;
        }

        if starts_ref {
            let (name, next) = read_name(stmt, &toks, i + 1);
            if !name.is_empty() {
                let mut j = next;
                if toks.get(j).is_some_and(|n| n.is_kw(stmt, "AS")) {
                    j += 1;
                }
                let alias = toks.get(j).filter(|n| {
                    is_name(n)
                        && !NOT_ALIAS
                            .iter()
                            .any(|w| n.text(stmt).eq_ignore_ascii_case(w))
                });
                let alias = alias.map(|n| unquote(n.text(stmt)));
                if alias.is_some() {
                    j += 1;
                }
                refs.push(TableRef {
                    name,
                    alias,
                    is_cte: false,
                });
                i = j;
                continue;
            }
        }
        i += 1;
    }
    refs
}

/// Reads `a.b.c` starting at token index `i`.
fn read_name(src: &str, toks: &[Token], mut i: usize) -> (Vec<String>, usize) {
    let mut parts = Vec::new();
    while let Some(t) = toks.get(i) {
        if !is_name(t)
            || (parts.is_empty() && t.kind == TokenKind::Keyword && !is_soft_keyword(t.text(src)))
        {
            break;
        }
        parts.push(unquote(t.text(src)));
        i += 1;
        if toks.get(i).is_some_and(|n| n.is(src, ".")) && toks.get(i + 1).is_some_and(is_name) {
            i += 1;
        } else {
            break;
        }
    }
    (parts, i)
}

/// Keywords that are commonly used as table names and are non-reserved in Spark.
fn is_soft_keyword(word: &str) -> bool {
    !NOT_ALIAS.iter().any(|w| word.eq_ignore_ascii_case(w))
        && !["TABLE", "VALUES", "LATERAL", "SELECT"]
            .iter()
            .any(|w| word.eq_ignore_ascii_case(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(s: &str) -> Context {
        analyze(s, s.len()).expect("context")
    }

    #[test]
    fn table_after_from_and_join() {
        assert_eq!(ctx("SELECT * FROM ").expect, Expect::Table);
        assert_eq!(ctx("SELECT * FROM ev").expect, Expect::Table);
        assert_eq!(ctx("SELECT * FROM ev").partial, "ev");
        assert_eq!(ctx("select * from a join b").expect, Expect::Table);
        assert_eq!(ctx("select * from a, ").expect, Expect::Table);
        assert_eq!(ctx("insert into ").expect, Expect::Table);
        assert_eq!(ctx("describe ").expect, Expect::Table);
    }

    #[test]
    fn database_contexts() {
        assert_eq!(ctx("USE ").expect, Expect::Database);
        assert_eq!(ctx("show tables in ").expect, Expect::Database);
        assert_eq!(ctx("show tables from de").expect, Expect::Database);
    }

    #[test]
    fn any_context() {
        assert_eq!(ctx("SELECT ").expect, Expect::Any);
        assert_eq!(ctx("SELECT a, ").expect, Expect::Any);
        assert_eq!(ctx("SELECT * FROM t WHERE ").expect, Expect::Any);
        assert_eq!(ctx("").expect, Expect::Any);
        // A comma inside a function call in FROM isn't a table list.
        assert_eq!(ctx("SELECT * FROM range(1, ").expect, Expect::Any);
    }

    #[test]
    fn member_access() {
        let c = ctx("SELECT e.");
        assert_eq!(
            c.expect,
            Expect::Member {
                qualifier: vec!["e".into()]
            }
        );
        assert_eq!(c.partial, "");
        assert_eq!(c.replace_start, "SELECT e.".len());

        let c = ctx("SELECT * FROM `my db`.ev");
        assert_eq!(
            c.expect,
            Expect::Member {
                qualifier: vec!["my db".into()]
            }
        );
        assert_eq!(c.partial, "ev");
        assert_eq!(c.replace_start, "SELECT * FROM `my db`.".len());

        let c = ctx("select cat.db.t");
        assert_eq!(
            c.expect,
            Expect::Member {
                qualifier: vec!["cat".into(), "db".into()]
            }
        );
    }

    #[test]
    fn replace_span_covers_partial() {
        let c = ctx("SELECT coun");
        assert_eq!(c.partial, "coun");
        assert_eq!(c.replace_start, 7);
    }

    #[test]
    fn no_completion_in_strings_or_comments() {
        assert!(analyze("SELECT 'abc", 11).is_none());
        assert!(analyze("SELECT 1 -- note", 16).is_none());
    }

    #[test]
    fn only_current_statement() {
        let s = "SELECT * FROM a x; SELECT x.";
        let c = ctx(s);
        assert!(c.tables.is_empty(), "{:?}", c.tables);
    }

    #[test]
    fn cursor_in_middle_uses_whole_statement_for_tables() {
        let s = "SELECT e. FROM db.events e";
        let c = analyze(s, "SELECT e.".len()).unwrap();
        assert_eq!(
            c.expect,
            Expect::Member {
                qualifier: vec!["e".into()]
            }
        );
        assert_eq!(c.tables.len(), 1);
        assert_eq!(c.tables[0].qualified(), "db.events");
        assert_eq!(c.tables[0].alias.as_deref(), Some("e"));
    }

    #[test]
    fn table_refs_with_aliases_and_ctes() {
        let refs = table_refs(
            "WITH recent AS (SELECT * FROM db.events WHERE x) \
             SELECT * FROM recent r JOIN `db`.`users` AS u ON r.id = u.id, other \
             LEFT JOIN t2 WHERE 1",
        );
        let got: Vec<_> = refs
            .iter()
            .map(|r| (r.qualified(), r.alias.clone(), r.is_cte))
            .collect();
        assert_eq!(
            got,
            vec![
                ("recent".into(), None, true),
                ("db.events".into(), None, false),
                ("recent".into(), Some("r".into()), false),
                ("db.users".into(), Some("u".into()), false),
                ("other".into(), None, false),
                ("t2".into(), None, false),
            ]
        );
    }

    #[test]
    fn subquery_in_from_is_skipped() {
        let refs = table_refs("SELECT * FROM (SELECT * FROM inner_t) s WHERE 1");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].qualified(), "inner_t");
    }

    #[test]
    fn alias_matching() {
        let r = TableRef {
            name: vec!["db".into(), "events".into()],
            alias: None,
            is_cte: false,
        };
        assert!(r.matches("EVENTS"));
        assert!(!r.matches("db"));
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("events"), "events");
        assert_eq!(quote_ident("my table"), "`my table`");
        assert_eq!(quote_ident("we`ird"), "`we``ird`");
        assert_eq!(unquote("`we``ird`"), "we`ird");
    }
}
