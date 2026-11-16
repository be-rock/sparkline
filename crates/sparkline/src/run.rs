//! Executes one statement: streams results, shows live progress on stderr,
//! and turns Ctrl-C into a server-side interrupt.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use sparkline_core::lexer::leading_keyword;
use sparkline_core::{ClientError, ExecEvent, Progress, SparkClient};

use crate::render::progress_line;

/// Don't flash a spinner for fast queries.
const SPINNER_DELAY: Duration = Duration::from_millis(300);

pub struct StatementResult {
    pub schema: Option<SchemaRef>,
    /// At most `max_rows` rows.
    pub batches: Vec<RecordBatch>,
    pub rows_shown: usize,
    /// Rows the server produced (with a server-side limit this is capped at
    /// `max_rows + 1`, enough to know there were more).
    pub rows_total: usize,
    pub server_limited: bool,
    pub elapsed: Duration,
}

impl StatementResult {
    pub fn truncated(&self) -> bool {
        self.rows_total > self.rows_shown
    }
}

/// Statements that are plain queries can be wrapped in a server-side LIMIT.
/// Commands (DDL, INSERT, SHOW, ...) run unwrapped and are capped client-side.
fn is_query(stmt: &str) -> bool {
    matches!(
        leading_keyword(stmt).as_deref(),
        Some("SELECT" | "WITH" | "FROM" | "VALUES" | "TABLE")
    )
}

/// Ctrl-C (SIGINT), registered once for the whole process.
///
/// While this exists Ctrl-C can't kill sparkline. That matters: Spark Connect
/// keeps running a query whose client disconnects without interrupting it, so
/// dying mid-query would leave the query running on the cluster. Signals are
/// buffered, so a Ctrl-C pressed while a query is still being submitted is
/// acted on as soon as the operation exists.
pub struct Sigint(tokio::signal::unix::Signal);

impl Sigint {
    /// Must be called inside the tokio runtime.
    pub fn install() -> std::io::Result<Self> {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).map(Sigint)
    }

    pub async fn recv(&mut self) {
        self.0.recv().await;
    }

    /// Consumes any Ctrl-C received since the last call, without waiting.
    pub fn take_pending(&mut self) -> bool {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let mut any = false;
        while let std::task::Poll::Ready(Some(())) = self.0.poll_recv(&mut cx) {
            any = true;
        }
        any
    }
}

pub async fn run_statement(
    client: &SparkClient,
    sigint: &mut Sigint,
    stmt: &str,
    max_rows: usize,
    show_progress: bool,
) -> Result<StatementResult, ClientError> {
    let start = Instant::now();
    let server_limited = max_rows > 0 && is_query(stmt);
    let limit = server_limited.then(|| (max_rows + 1).min(i32::MAX as usize) as i32);

    let mut spinner = Spinner::new(show_progress && std::io::stderr().is_terminal(), start);
    // Not raced against Ctrl-C: dropping the submission would disconnect
    // without an interrupt. A Ctrl-C during submission stays buffered in
    // `sigint` and is handled by the loop below.
    let mut exec = client.execute_sql(stmt, limit).await?;

    let mut result = StatementResult {
        schema: None,
        batches: Vec::new(),
        rows_shown: 0,
        rows_total: 0,
        server_limited,
        elapsed: Duration::ZERO,
    };
    let mut tick = tokio::time::interval(Duration::from_millis(80));

    // `None` = Ctrl-C: cancel below. The loop only borrows `exec`.
    let outcome: Option<Result<(), ClientError>> = loop {
        tokio::select! {
            ev = exec.next_event() => match ev {
                Ok(Some(ExecEvent::Batches { schema, batches })) => {
                    result.schema.get_or_insert(schema);
                    for b in batches {
                        result.rows_total += b.num_rows();
                        let room = if max_rows == 0 { usize::MAX } else { max_rows - result.rows_shown };
                        if room > 0 && b.num_rows() > 0 {
                            let take = room.min(b.num_rows());
                            result.batches.push(if take == b.num_rows() { b } else { b.slice(0, take) });
                            result.rows_shown += take;
                        }
                    }
                }
                Ok(Some(ExecEvent::Progress(p))) => spinner.progress = Some(p),
                Ok(None) => break Some(Ok(())),
                Err(e) => break Some(Err(e)),
            },
            _ = sigint.recv() => break None,
            _ = tick.tick() => spinner.draw(),
        }
    };

    let Some(outcome) = outcome else {
        // Cancel in a background task so that giving up on waiting (a second
        // Ctrl-C) still lets it finish and not orphan the query.
        spinner.note = Some("cancelling… (Ctrl-C again to stop waiting)");
        spinner.force = true;
        let mut cancel = tokio::spawn(exec.cancel());
        loop {
            tokio::select! {
                _ = &mut cancel => break,
                _ = sigint.recv() => break,
                _ = tick.tick() => spinner.draw(),
            }
        }
        spinner.clear();
        return Err(ClientError::Cancelled);
    };
    spinner.clear();
    outcome?;
    result.elapsed = start.elapsed();
    Ok(result)
}

struct Spinner {
    enabled: bool,
    start: Instant,
    frame: usize,
    drawn: bool,
    force: bool,
    progress: Option<Progress>,
    note: Option<&'static str>,
}

impl Spinner {
    fn new(enabled: bool, start: Instant) -> Self {
        Spinner {
            enabled,
            start,
            frame: 0,
            drawn: false,
            force: false,
            progress: None,
            note: None,
        }
    }

    fn draw(&mut self) {
        if !self.enabled || (!self.force && self.start.elapsed() < SPINNER_DELAY) {
            return;
        }
        self.frame += 1;
        let line = progress_line(
            self.frame,
            self.start.elapsed(),
            self.progress.as_ref(),
            self.note,
        );
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r\x1b[2K\x1b[2m{line}\x1b[0m");
        let _ = err.flush();
        self.drawn = true;
    }

    fn clear(&mut self) {
        if self.drawn {
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r\x1b[2K");
            let _ = err.flush();
            self.drawn = false;
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_detection() {
        assert!(is_query("SELECT 1"));
        assert!(is_query("with x as (select 1) select * from x"));
        assert!(is_query("(SELECT 1) UNION (SELECT 2)"));
        assert!(!is_query("SHOW TABLES"));
        assert!(!is_query("INSERT INTO t SELECT 1"));
        assert!(!is_query("EXPLAIN SELECT 1"));
    }
}
