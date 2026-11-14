//! Interactive REPL on top of reedline.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nu_ansi_term::{Color, Style};
use reedline::{
    default_emacs_keybindings, default_vi_insert_keybindings, default_vi_normal_keybindings,
    Completer, DefaultHinter, DescriptionMode, EditCommand, Emacs, FileBackedHistory, Highlighter,
    IdeMenu, KeyCode, KeyModifiers, Keybindings, MenuBuilder, Prompt, PromptEditMode,
    PromptHistorySearch, PromptHistorySearchStatus, PromptViMode, Reedline, ReedlineEvent,
    ReedlineMenu, Signal, Span, StyledText, Suggestion, ValidationResult, Validator, Vi,
};
use sparkline_core::catalog::{Catalog, Load};
use sparkline_core::complete::analyze;
use sparkline_core::lexer::{is_complete_statement, tokenize, TokenKind};
use sparkline_core::suggest::{apply_keyword_case, suggest};

use crate::config::{data_dir, EditMode};
use crate::render::Expanded;
use crate::App;

const MENU: &str = "completion_menu";
/// How long Tab may block waiting for catalog metadata that isn't cached yet.
const COMPLETION_WAIT: Duration = Duration::from_millis(400);
const HISTORY_SIZE: usize = 10_000;

pub fn run(app: &mut App, catalog: Catalog, edit_mode: EditMode) -> anyhow::Result<()> {
    let mut editor = build_editor(&catalog, edit_mode);
    let prompt = SqlPrompt {
        catalog: catalog.clone(),
    };
    // Usually instant; avoids a placeholder database name in the first prompt.
    catalog.wait_for(&[Load::CurrentDb], Duration::from_millis(500));
    app.catalog = Some(catalog);

    print_banner(app);
    loop {
        match editor.read_line(&prompt) {
            Ok(Signal::Success(input)) => {
                let trimmed = input.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if is_quit(trimmed) {
                    break;
                }
                if let Some(cmd) = trimmed.strip_prefix('\\') {
                    if meta_command(app, cmd) == Flow::Quit {
                        break;
                    }
                    continue;
                }
                if trimmed.eq_ignore_ascii_case("help") {
                    print_help();
                    continue;
                }
                app.run_script(&input);
            }
            Ok(Signal::CtrlC) => continue,
            Ok(Signal::CtrlD) => break,
            Err(e) => {
                eprintln!("terminal error: {e}");
                break;
            }
        }
    }
    Ok(())
}

/// The full edit buffer as of the last repaint.
///
/// reedline hands completers only the text *before* the cursor, but SQL
/// completion needs what follows too (`SELECT e.| FROM events e`). The
/// highlighter sees the whole line on every repaint, so it records it here.
#[derive(Clone, Default)]
struct LineSnapshot(Arc<Mutex<String>>);

impl LineSnapshot {
    fn set(&self, line: &str) {
        let mut cur = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if *cur != line {
            cur.clear();
            cur.push_str(line);
        }
    }

    /// The full line, if it is consistent with the prefix reedline passed.
    fn full_line(&self, prefix: &str) -> Option<String> {
        let cur = self.0.lock().unwrap_or_else(|e| e.into_inner());
        cur.starts_with(prefix).then(|| cur.clone())
    }
}

fn build_editor(catalog: &Catalog, edit_mode: EditMode) -> Reedline {
    let snapshot = LineSnapshot::default();
    let history: Option<Box<FileBackedHistory>> = data_dir().and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        FileBackedHistory::with_file(HISTORY_SIZE, dir.join("history.txt"))
            .ok()
            .map(Box::new)
    });

    let menu = IdeMenu::default()
        .with_name(MENU)
        .with_default_border()
        .with_description_mode(DescriptionMode::PreferRight)
        .with_max_completion_height(12)
        .with_max_description_width(40);

    let edit: Box<dyn reedline::EditMode> = match edit_mode {
        EditMode::Emacs => {
            let mut kb = default_emacs_keybindings();
            add_bindings(&mut kb);
            Box::new(Emacs::new(kb))
        }
        EditMode::Vi => {
            let mut insert = default_vi_insert_keybindings();
            add_bindings(&mut insert);
            Box::new(Vi::new(insert, default_vi_normal_keybindings()))
        }
    };

    let mut editor = Reedline::create()
        .with_completer(Box::new(SqlCompleter {
            catalog: catalog.clone(),
            snapshot: snapshot.clone(),
        }))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_quick_completions(true)
        .with_partial_completions(true)
        .with_highlighter(Box::new(SqlHighlighter { snapshot }))
        .with_validator(Box::new(SqlValidator))
        .with_hinter(Box::new(
            DefaultHinter::default().with_style(Style::new().fg(Color::DarkGray)),
        ))
        .with_edit_mode(edit);
    if let Some(h) = history {
        editor = editor.with_history(h);
    }
    editor
}

fn add_bindings(kb: &mut Keybindings) {
    kb.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu(MENU.into()),
            ReedlineEvent::MenuNext,
        ]),
    );
    kb.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::MenuPrevious,
    );
    // Alt-Enter: newline without submitting, even when the statement is complete.
    kb.add_binding(
        KeyModifiers::ALT,
        KeyCode::Enter,
        ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
    );
}

fn is_quit(s: &str) -> bool {
    let s = s.trim_end_matches(';').trim();
    s.eq_ignore_ascii_case("exit") || s.eq_ignore_ascii_case("quit")
}

#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

fn meta_command(app: &mut App, cmd: &str) -> Flow {
    let mut parts = cmd
        .trim()
        .trim_end_matches(';')
        .splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    let arg = parts.next().map(str::trim).filter(|a| !a.is_empty());
    match name {
        "q" | "quit" => return Flow::Quit,
        "?" | "h" | "help" => print_help(),
        "l" => {
            app.run_script("SHOW DATABASES");
        }
        "d" => {
            match arg {
                Some(t) => app.run_script(&format!("DESCRIBE TABLE {t}")),
                None => app.run_script("SHOW TABLES"),
            };
        }
        "limit" => match arg.map(str::parse::<usize>) {
            None => app.info(&format!(
                "max rows: {}",
                describe_limit(app.settings.max_rows)
            )),
            Some(Ok(n)) => {
                app.settings.max_rows = n;
                app.info(&format!("max rows set to {}", describe_limit(n)));
            }
            Some(Err(_)) => app.warn("usage: \\limit <rows>  (0 = unlimited)"),
        },
        "x" => {
            app.settings.expanded = app.settings.expanded.next();
            let mode = match app.settings.expanded {
                Expanded::Auto => "auto (vertical when too wide)",
                Expanded::On => "on",
                Expanded::Off => "off",
            };
            app.info(&format!("expanded display: {mode}"));
        }
        "refresh" => {
            if let Some(c) = &app.catalog {
                c.invalidate();
                c.refresh_current_db();
            }
            app.info("reloading catalog metadata");
        }
        "conninfo" => app.info(&format!(
            "connected to {} · Spark {} · session {}",
            app.remote_display,
            app.spark_version,
            app.client.session_id()
        )),
        other => app.warn(&format!("unknown command \\{other} (try \\?)")),
    }
    Flow::Continue
}

fn describe_limit(n: usize) -> String {
    if n == 0 {
        "unlimited".into()
    } else {
        n.to_string()
    }
}

fn print_banner(app: &App) {
    let dim = Style::new().fg(Color::DarkGray);
    println!(
        "{} {}",
        Style::new()
            .bold()
            .paint(concat!("sparkline ", env!("CARGO_PKG_VERSION"))),
        dim.paint(format!(
            "· {} · Spark {}",
            app.remote_display, app.spark_version
        ))
    );
    println!(
        "{}",
        dim.paint("End statements with ;  Tab completes, \\? for help, Ctrl-D to quit.")
    );
}

fn print_help() {
    println!(
        "\
SQL statements end with `;` (Alt-Enter inserts a newline). Several statements
can be entered at once. Ctrl-C cancels a running query.

  \\?            this help
  \\q            quit (also: exit, quit, Ctrl-D)
  \\l            list databases
  \\d [table]    list tables, or describe a table
  \\limit [n]    show or set the max rows fetched per query (0 = unlimited)
  \\x            cycle expanded display: auto → on → off
  \\refresh      reload completion metadata
  \\conninfo     connection details

Keys: Tab complete · Shift-Tab previous · Ctrl-R search history · Alt-Enter newline"
    );
}

// ---------------------------------------------------------------------------

struct SqlPrompt {
    catalog: Catalog,
}

impl Prompt for SqlPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        // The database appears once the background catalog load has it:
        // immediately on a warm server, a few seconds on a cold one.
        match self.catalog.current_db() {
            Some(db) => Cow::Owned(format!("spark:{db}")),
            None => Cow::Borrowed("spark"),
        }
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, mode: PromptEditMode) -> Cow<'_, str> {
        match mode {
            PromptEditMode::Vi(PromptViMode::Normal) => Cow::Borrowed(" : "),
            _ => Cow::Borrowed(" > "),
        }
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("  … ")
    }

    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        let prefix = match search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!("({prefix}search: {}) ", search.term))
    }

    fn get_prompt_color(&self) -> reedline::Color {
        reedline::Color::Cyan
    }

    fn get_indicator_color(&self) -> reedline::Color {
        reedline::Color::DarkGrey
    }
}

// ---------------------------------------------------------------------------

#[derive(Default)]
struct SqlHighlighter {
    snapshot: LineSnapshot,
}

impl Highlighter for SqlHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        self.snapshot.set(line);
        let mut out = StyledText::new();
        if line.trim_start().starts_with('\\') {
            out.push((Style::new().fg(Color::Magenta), line.to_string()));
            return out;
        }
        let tokens = tokenize(line);
        for (i, t) in tokens.iter().enumerate() {
            let next_is_paren = tokens[i + 1..]
                .iter()
                .find(|n| !n.is_trivia())
                .is_some_and(|n| n.is(line, "("));
            let style = match t.kind {
                TokenKind::Keyword => Style::new().fg(Color::Blue).bold(),
                TokenKind::Ident if next_is_paren => Style::new().fg(Color::Cyan),
                TokenKind::String => Style::new().fg(Color::Green),
                TokenKind::Number => Style::new().fg(Color::Purple),
                TokenKind::Comment => Style::new().fg(Color::DarkGray).italic(),
                TokenKind::Variable => Style::new().fg(Color::Yellow),
                TokenKind::Unknown => Style::new().fg(Color::Red),
                _ => Style::new(),
            };
            out.push((style, t.text(line).to_string()));
        }
        out
    }
}

struct SqlValidator;

impl Validator for SqlValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        let t = line.trim();
        if t.is_empty()
            || t.starts_with('\\')
            || is_quit(t)
            || t.eq_ignore_ascii_case("help")
            || is_complete_statement(line)
        {
            ValidationResult::Complete
        } else {
            ValidationResult::Incomplete
        }
    }
}

struct SqlCompleter {
    catalog: Catalog,
    snapshot: LineSnapshot,
}

impl Completer for SqlCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        if line.trim_start().starts_with('\\') {
            return Vec::new();
        }
        let full = match self.snapshot.full_line(&line[..pos.min(line.len())]) {
            Some(full) => full,
            None => line.to_string(),
        };
        let line = full.as_str();
        let Some(ctx) = analyze(line, pos) else {
            return Vec::new();
        };
        let mut s = suggest(&ctx, &self.catalog.state());
        if !s.missing.is_empty() {
            for load in &s.missing {
                self.catalog.request(load.clone());
            }
            self.catalog.wait_for(&s.missing, COMPLETION_WAIT);
            s = suggest(&ctx, &self.catalog.state());
        }
        apply_keyword_case(&mut s.candidates, &line[ctx.replace_start..pos]);
        s.candidates
            .into_iter()
            .take(500)
            .map(|c| Suggestion {
                description: Some(match &c.detail {
                    Some(d) => format!("{} · {d}", c.kind.label()),
                    None => c.kind.label().to_string(),
                }),
                value: c.value,
                style: None,
                extra: None,
                span: Span::new(ctx.replace_start, pos),
                append_whitespace: c.append_space,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validator() {
        let v = SqlValidator;
        assert!(matches!(
            v.validate("select 1"),
            ValidationResult::Incomplete
        ));
        assert!(matches!(
            v.validate("select 1;"),
            ValidationResult::Complete
        ));
        assert!(matches!(v.validate("\\d"), ValidationResult::Complete));
        assert!(matches!(v.validate("exit"), ValidationResult::Complete));
        assert!(matches!(v.validate("  "), ValidationResult::Complete));
    }

    #[test]
    fn highlighter_preserves_text() {
        let line = "SELECT count(*), 'x' FROM t -- hi";
        let styled = SqlHighlighter::default().highlight(line, 0);
        let joined: String = styled.buffer.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(joined, line);
    }
}
