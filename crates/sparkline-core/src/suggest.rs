//! Turns a completion [`Context`] plus a catalog snapshot into ranked
//! candidates. Pure: it also reports which catalog entries are missing so the
//! caller can load them and try again.

use std::collections::HashSet;

use crate::catalog::{CatalogState, Load};
use crate::complete::{quote_ident, Context, Expect, TableRef};
use crate::keywords::KEYWORDS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    // Declaration order is display order.
    Column,
    Alias,
    Cte,
    Table,
    Database,
    Function,
    Keyword,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Column => "column",
            Kind::Alias => "alias",
            Kind::Cte => "cte",
            Kind::Table => "table",
            Kind::Database => "database",
            Kind::Function => "function",
            Kind::Keyword => "keyword",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Text to insert.
    pub value: String,
    pub kind: Kind,
    /// Extra info shown in the menu (e.g. which table a column belongs to).
    pub detail: Option<String>,
    pub append_space: bool,
}

pub struct Suggestions {
    pub candidates: Vec<Candidate>,
    /// Catalog entries that would improve these suggestions once loaded.
    pub missing: Vec<Load>,
}

pub fn suggest(ctx: &Context, state: &CatalogState) -> Suggestions {
    let mut b = Builder { partial: ctx.partial.to_lowercase(), out: Vec::new(), missing: Vec::new() };
    let current_db = state.current_db.clone().unwrap_or_else(|| "default".into());

    match &ctx.expect {
        Expect::Table => {
            for r in ctx.tables.iter().filter(|r| r.is_cte) {
                b.push(quote_ident(&r.qualified()), Kind::Cte, None, true);
            }
            b.tables_in(state, &current_db, None);
            b.databases(state, ".");
        }
        Expect::Database => b.databases(state, ""),
        Expect::Member { qualifier } => {
            if let Some(r) = find_ref(&ctx.tables, qualifier) {
                b.columns_of(state, r, None);
            } else if is_database(state, qualifier) {
                let db = qualifier.join(".");
                b.tables_in(state, &db, Some(&db));
            } else if qualifier.len() == 1 && has_table(state, &current_db, &qualifier[0]) {
                let r = TableRef { name: qualifier.clone(), alias: None, is_cte: false };
                b.columns_of(state, &r, None);
            } else if state.databases.is_none() {
                b.missing.push(Load::Databases);
            } else if qualifier.len() == 1 {
                // Not a known alias or database: maybe a table not yet loaded.
                b.missing.push(Load::Tables(current_db.to_lowercase()));
            }
        }
        Expect::Any => {
            let refs: Vec<&TableRef> = ctx.tables.iter().filter(|r| !r.is_cte).collect();
            let label_refs = refs.len() > 1;
            for r in &refs {
                let label = label_refs.then(|| r.alias.clone().unwrap_or_else(|| r.qualified()));
                b.columns_of(state, r, label);
                if let Some(a) = &r.alias {
                    b.push(quote_ident(a), Kind::Alias, Some(r.qualified()), false);
                }
            }
            b.functions(state);
            b.keywords();
        }
    }

    let mut seen = HashSet::new();
    let mut candidates: Vec<Candidate> = b.out.into_iter().filter(|c| seen.insert(c.value.clone())).collect();
    candidates.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.value.to_lowercase().cmp(&b.value.to_lowercase())));
    let mut missing = b.missing;
    missing.dedup();
    Suggestions { candidates, missing }
}

struct Builder {
    partial: String,
    out: Vec<Candidate>,
    missing: Vec<Load>,
}

impl Builder {
    fn matches(&self, s: &str) -> bool {
        s.to_lowercase().starts_with(&self.partial)
    }

    fn push(&mut self, value: String, kind: Kind, detail: Option<String>, append_space: bool) {
        if self.matches(value.trim_start_matches('`')) {
            self.out.push(Candidate { value, kind, detail, append_space });
        }
    }

    fn tables_in(&mut self, state: &CatalogState, db: &str, detail: Option<&str>) {
        match state.tables.get(&db.to_lowercase()) {
            Some(tables) => {
                for t in tables {
                    let d = if t.is_temp { Some("temp view".to_string()) } else { detail.map(str::to_string) };
                    self.push(quote_ident(&t.name), Kind::Table, d, true);
                }
            }
            None => self.missing.push(Load::Tables(db.to_lowercase())),
        }
    }

    fn databases(&mut self, state: &CatalogState, suffix: &str) {
        match &state.databases {
            Some(dbs) => {
                for d in dbs {
                    self.push(format!("{}{suffix}", quote_ident(d)), Kind::Database, None, suffix.is_empty());
                }
            }
            None => self.missing.push(Load::Databases),
        }
    }

    fn columns_of(&mut self, state: &CatalogState, r: &TableRef, label: Option<String>) {
        if r.is_cte {
            return;
        }
        let key = r.qualified().to_lowercase();
        match state.columns.get(&key) {
            Some(cols) => {
                for c in cols {
                    self.push(quote_ident(c), Kind::Column, label.clone(), false);
                }
            }
            None => self.missing.push(Load::Columns(key)),
        }
    }

    fn functions(&mut self, state: &CatalogState) {
        match &state.functions {
            Some(fns) => {
                for f in fns {
                    if self.matches(f) {
                        self.out.push(Candidate { value: format!("{f}("), kind: Kind::Function, detail: None, append_space: false });
                    }
                }
            }
            None => self.missing.push(Load::Functions),
        }
    }

    fn keywords(&mut self) {
        for k in KEYWORDS {
            if self.matches(k) {
                self.out.push(Candidate { value: (*k).to_string(), kind: Kind::Keyword, detail: None, append_space: true });
            }
        }
    }
}

fn find_ref<'a>(refs: &'a [TableRef], qualifier: &[String]) -> Option<&'a TableRef> {
    if qualifier.len() == 1 {
        if let Some(r) = refs.iter().rev().find(|r| r.matches(&qualifier[0])) {
            return Some(r);
        }
    }
    let q = qualifier.join(".").to_lowercase();
    refs.iter().find(|r| r.qualified().to_lowercase() == q)
}

fn is_database(state: &CatalogState, qualifier: &[String]) -> bool {
    let q = qualifier.join(".").to_lowercase();
    state.databases.as_ref().is_some_and(|dbs| dbs.iter().any(|d| d.to_lowercase() == q))
}

fn has_table(state: &CatalogState, db: &str, name: &str) -> bool {
    state
        .tables
        .get(&db.to_lowercase())
        .is_some_and(|ts| ts.iter().any(|t| t.name.eq_ignore_ascii_case(name)))
}

/// Applies the user's keyword casing: if they typed a lowercase prefix,
/// keywords are inserted lowercase.
pub fn apply_keyword_case(candidates: &mut [Candidate], raw_partial: &str) {
    let lower = raw_partial.chars().any(char::is_alphabetic) && raw_partial == raw_partial.to_lowercase();
    if lower {
        for c in candidates.iter_mut().filter(|c| c.kind == Kind::Keyword) {
            c.value = c.value.to_lowercase();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::TableInfo;
    use crate::complete::analyze;

    fn state() -> CatalogState {
        let mut s = CatalogState { current_db: Some("default".into()), ..Default::default() };
        s.databases = Some(vec!["default".into(), "sales".into()]);
        s.tables.insert(
            "default".into(),
            vec![TableInfo { name: "events".into(), is_temp: false }, TableInfo { name: "users".into(), is_temp: false }],
        );
        s.tables.insert("sales".into(), vec![TableInfo { name: "orders".into(), is_temp: false }]);
        s.columns.insert("events".into(), vec!["event_id".into(), "user_id".into(), "ts".into()]);
        s.columns.insert("sales.orders".into(), vec!["order_id".into(), "amount".into()]);
        s.functions = Some(vec!["count".into(), "coalesce".into(), "upper".into()]);
        s
    }

    fn values(line: &str) -> Vec<String> {
        let ctx = analyze(line, line.len()).unwrap();
        suggest(&ctx, &state()).candidates.into_iter().map(|c| c.value).collect()
    }

    #[test]
    fn tables_after_from() {
        assert_eq!(values("SELECT * FROM e"), vec!["events"]);
        let all = values("SELECT * FROM ");
        assert!(all.contains(&"users".to_string()) && all.contains(&"sales.".to_string()), "{all:?}");
    }

    #[test]
    fn tables_in_database() {
        assert_eq!(values("SELECT * FROM sales."), vec!["orders"]);
    }

    #[test]
    fn columns_via_alias_and_table_name() {
        let line = "SELECT o.am FROM sales.orders o";
        let ctx = analyze(line, "SELECT o.am".len()).unwrap();
        let got: Vec<_> = suggest(&ctx, &state()).candidates.into_iter().map(|c| c.value).collect();
        assert_eq!(got, vec!["amount"]);
    }

    #[test]
    fn columns_functions_keywords_in_select() {
        let line = "SELECT u FROM events";
        let ctx = analyze(line, "SELECT u".len()).unwrap();
        let got = suggest(&ctx, &state());
        let kinds: Vec<_> = got.candidates.iter().map(|c| (c.value.as_str(), c.kind)).collect();
        assert_eq!(kinds[0], ("user_id", Kind::Column));
        assert!(kinds.contains(&("upper(", Kind::Function)));
        assert!(kinds.contains(&("UNION", Kind::Keyword)));
        assert!(got.missing.is_empty());
    }

    #[test]
    fn reports_missing_columns() {
        let line = "SELECT x FROM sales.unknown";
        let ctx = analyze(line, "SELECT x".len()).unwrap();
        let got = suggest(&ctx, &state());
        assert_eq!(got.missing, vec![Load::Columns("sales.unknown".into())]);
    }

    #[test]
    fn keyword_case_follows_user() {
        let kw = || vec![Candidate { value: "SELECT".into(), kind: Kind::Keyword, detail: None, append_space: true }];
        let mut c = kw();
        apply_keyword_case(&mut c, "sel");
        assert_eq!(c[0].value, "select");
        let mut c = kw();
        apply_keyword_case(&mut c, "Sel");
        assert_eq!(c[0].value, "SELECT");
        let mut c = kw();
        apply_keyword_case(&mut c, "");
        assert_eq!(c[0].value, "SELECT");
    }
}
