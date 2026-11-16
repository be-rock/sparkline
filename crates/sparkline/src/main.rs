//! sparkline: a fast terminal client for Apache Spark over Spark Connect.

mod config;
mod render;
mod repl;
mod run;

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use nu_ansi_term::{Color, Style};
use sparkline_core::catalog::Catalog;
use sparkline_core::lexer::{leading_keyword, split_statements};
use sparkline_core::{ClientError, ConnectionString, SparkClient};

use crate::config::{Config, EditMode, DEFAULT_MAX_ROWS};
use crate::render::{
    format_count, format_duration, render_error, render_results, terminal_width, Expanded,
    RenderOpts,
};
use crate::run::{run_statement, Sigint, StatementResult};

#[derive(Parser, Debug)]
#[command(
    name = "sparkline",
    version,
    about = "A fast terminal client for Apache Spark (Spark Connect)"
)]
struct Args {
    /// Spark Connect URL, e.g. sc://localhost:15002 (overrides --profile and SPARK_REMOTE)
    #[arg(short, long)]
    remote: Option<String>,
    /// Connection profile from ~/.config/sparkline/config.toml
    #[arg(short, long)]
    profile: Option<String>,
    /// Run SQL (one or more ;-separated statements) and exit
    #[arg(short, long, conflicts_with = "file")]
    execute: Option<String>,
    /// Run a SQL file and exit
    #[arg(short, long)]
    file: Option<PathBuf>,
    /// Max rows fetched and shown per query (0 = unlimited) [default: 1000]
    #[arg(long)]
    max_rows: Option<usize>,
    /// Config file [default: ~/.config/sparkline/config.toml]
    #[arg(long)]
    config: Option<PathBuf>,
    /// Use vi key bindings
    #[arg(long)]
    vi: bool,
}

pub struct Settings {
    pub max_rows: usize,
    pub expanded: Expanded,
    pub color_out: bool,
    pub color_err: bool,
    pub interactive: bool,
}

pub struct App {
    pub rt: tokio::runtime::Runtime,
    pub sigint: Sigint,
    pub client: SparkClient,
    pub catalog: Option<Catalog>,
    pub settings: Settings,
    pub remote_display: String,
    pub spark_version: String,
}

impl App {
    /// Runs every statement in `text`, stopping at the first error.
    /// Returns whether all statements succeeded.
    pub fn run_script(&mut self, text: &str) -> bool {
        // A Ctrl-C from before this input (e.g. while results were printing)
        // doesn't apply to it.
        self.rt.block_on(async { self.sigint.take_pending() });
        for stmt in split_statements(text) {
            // Ctrl-C between statements stops the rest of the script.
            if self.rt.block_on(async { self.sigint.take_pending() }) {
                eprintln!(
                    "{}",
                    render_error(&ClientError::Cancelled, None, self.settings.color_err)
                );
                return false;
            }
            let res = self.rt.block_on(run_statement(
                &self.client,
                &mut self.sigint,
                stmt,
                self.settings.max_rows,
                true,
            ));
            match res {
                Ok(r) => {
                    self.print_result(&r);
                    self.after_statement(stmt);
                }
                Err(e) => {
                    eprintln!("{}", render_error(&e, Some(stmt), self.settings.color_err));
                    if matches!(e, ClientError::Unavailable(_)) {
                        eprintln!(
                            "{}",
                            self.dim_err(
                                "the server may have restarted; restart sparkline to reconnect"
                            )
                        );
                    }
                    return false;
                }
            }
        }
        true
    }

    fn print_result(&self, r: &StatementResult) {
        let has_columns = r.schema.as_ref().is_some_and(|s| !s.fields().is_empty());
        if has_columns {
            let schema = r.schema.as_ref().expect("checked");
            let opts = RenderOpts {
                color: self.settings.color_out,
                expanded: self.settings.expanded,
                width: if self.settings.color_out {
                    terminal_width()
                } else {
                    None
                },
            };
            println!("{}", render_results(schema, &r.batches, opts));
        }
        if !(self.settings.interactive || std::io::stderr().is_terminal()) {
            return;
        }
        let time = format_duration(r.elapsed);
        let status = if !has_columns {
            format!("OK · {time}")
        } else if r.truncated() && r.server_limited {
            format!(
                "first {} rows · {time} · more available (\\limit to change)",
                format_count(r.rows_shown)
            )
        } else if r.truncated() {
            format!(
                "{} of {} rows shown · {time}",
                format_count(r.rows_shown),
                format_count(r.rows_total)
            )
        } else {
            let n = r.rows_shown;
            format!(
                "{} row{} · {time}",
                format_count(n),
                if n == 1 { "" } else { "s" }
            )
        };
        eprintln!("{}", self.dim_err(&status));
    }

    /// Keeps completion metadata in sync with what the statement changed.
    fn after_statement(&self, stmt: &str) {
        let Some(catalog) = &self.catalog else { return };
        match leading_keyword(stmt).as_deref() {
            Some("USE") => catalog.refresh_current_db(),
            Some("CREATE" | "DROP" | "ALTER" | "MSCK" | "REPAIR" | "CACHE" | "UNCACHE") => {
                catalog.invalidate()
            }
            _ => {}
        }
    }

    fn dim_err(&self, s: &str) -> String {
        if self.settings.color_err {
            Style::new().fg(Color::DarkGray).paint(s).to_string()
        } else {
            s.to_string()
        }
    }

    pub fn info(&self, s: &str) {
        eprintln!("{}", self.dim_err(s));
    }

    pub fn warn(&self, s: &str) {
        if self.settings.color_err {
            eprintln!("{}", Style::new().fg(Color::Yellow).paint(s));
        } else {
            eprintln!("{s}");
        }
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    match real_main(args) {
        Ok(code) => code,
        Err(e) => {
            let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
            let label = if color {
                Style::new()
                    .fg(Color::Red)
                    .bold()
                    .paint("error:")
                    .to_string()
            } else {
                "error:".into()
            };
            eprintln!("{label} {e:#}");
            if let Some(ClientError::Connect { .. }) = e.downcast_ref::<ClientError>() {
                eprintln!("hint: is a Spark Connect server running there? Use --remote or a --profile to point elsewhere.");
            }
            ExitCode::from(2)
        }
    }
}

fn real_main(args: Args) -> Result<ExitCode> {
    let config = Config::load(args.config.as_deref())?;
    let env_remote = std::env::var("SPARK_REMOTE").ok();
    let remote = config.resolve_remote(
        args.remote.as_deref(),
        args.profile.as_deref(),
        env_remote.as_deref(),
    )?;
    let conn = ConnectionString::parse(&remote).context("invalid Spark Connect URL")?;

    let script = match (&args.execute, &args.file) {
        (Some(sql), _) => Some(sql.clone()),
        (None, Some(path)) => Some(
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
        ),
        (None, None) if !std::io::stdin().is_terminal() => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .context("reading SQL from stdin")?;
            Some(s)
        }
        _ => None,
    };
    let interactive = script.is_none();
    let no_color = std::env::var_os("NO_COLOR").is_some();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    // Installed before anything talks to Spark; see `Sigint`.
    let mut sigint = rt
        .block_on(async { Sigint::install() })
        .context("installing Ctrl-C handler")?;
    let connected = rt.block_on(async {
        tokio::select! {
            r = async {
                let client = SparkClient::connect(&conn).await?;
                let version = client.spark_version().await?;
                Ok::<_, ClientError>((client, version))
            } => Some(r),
            _ = sigint.recv() => None,
        }
    });
    let Some(connected) = connected else {
        return Ok(ExitCode::from(130));
    };
    let (client, spark_version) = connected?;

    let mut app = App {
        rt,
        sigint,
        client,
        catalog: None,
        settings: Settings {
            max_rows: args
                .max_rows
                .or(config.max_rows)
                .unwrap_or(DEFAULT_MAX_ROWS),
            expanded: Expanded::Auto,
            color_out: std::io::stdout().is_terminal() && !no_color,
            color_err: std::io::stderr().is_terminal() && !no_color,
            interactive,
        },
        remote_display: conn.to_string(),
        spark_version,
    };

    let ok = match script {
        Some(sql) => app.run_script(&sql),
        None => {
            let catalog = Catalog::spawn(app.client.clone(), app.rt.handle());
            let edit_mode = if args.vi {
                EditMode::Vi
            } else {
                config.edit_mode
            };
            repl::run(&mut app, catalog, edit_mode)?;
            true
        }
    };

    // Free server-side session state; don't hang on exit if the server is gone.
    let client = app.client.clone();
    let _ = app.rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), client.release_session()).await
    });
    app.rt.shutdown_timeout(Duration::from_millis(200));

    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
