//! Tests against a real Spark Connect server.
//!
//! Skipped unless SPARKLINE_TEST_REMOTE is set, e.g.:
//!   dev/spark-connect-server.sh &
//!   SPARKLINE_TEST_REMOTE=sc://localhost:15002 cargo test -p sparkline-core --test live

use std::time::{Duration, Instant};

use sparkline_core::catalog::{column, Catalog, Load};
use sparkline_core::{ClientError, ConnectionString, ExecEvent, SparkClient};

async fn client() -> Option<SparkClient> {
    let remote = std::env::var("SPARKLINE_TEST_REMOTE").ok()?;
    let conn = ConnectionString::parse(&remote).expect("valid SPARKLINE_TEST_REMOTE");
    Some(SparkClient::connect(&conn).await.expect("connect"))
}

/// Defines a live test that fails after TEST_TIMEOUT instead of hanging (a
/// stuck query, e.g. behind orphaned tasks, would otherwise block forever).
macro_rules! live_test {
    ($name:ident, $body:block) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            tokio::time::timeout(TEST_TIMEOUT, async move $body)
                .await
                .unwrap_or_else(|_| panic!("{} timed out after {:?}", stringify!($name), TEST_TIMEOUT))
        }
    };
}

const TEST_TIMEOUT: Duration = Duration::from_secs(120);

macro_rules! live_client {
    () => {
        match client().await {
            Some(c) => c,
            None => {
                eprintln!("skipping: SPARKLINE_TEST_REMOTE not set");
                return;
            }
        }
    };
}

live_test!(version_and_simple_query, {
    let c = live_client!();
    let v = c.spark_version().await.unwrap();
    assert!(v.starts_with("4."), "unexpected version {v}");

    let batches = c
        .sql_collect("SELECT id, id * 2 AS twice FROM range(5)")
        .await
        .unwrap();
    let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 5);
    assert_eq!(column(&batches, 1), vec!["0", "2", "4", "6", "8"]);
});

live_test!(limit_wrapping, {
    let c = live_client!();
    let mut exec = c
        .execute_sql("SELECT * FROM range(1000)", Some(11))
        .await
        .unwrap();
    let mut rows = 0;
    while let Some(ev) = exec.next_event().await.unwrap() {
        if let ExecEvent::Batches { batches, .. } = ev {
            rows += batches.iter().map(|b| b.num_rows()).sum::<usize>();
        }
    }
    assert_eq!(rows, 11);
});

live_test!(ddl_and_temp_views, {
    let c = live_client!();
    c.sql_collect("CREATE OR REPLACE TEMP VIEW sl_test_v AS SELECT 1 AS a, 'x' AS b")
        .await
        .unwrap();
    let cols = column(
        &c.sql_collect("SHOW COLUMNS IN sl_test_v").await.unwrap(),
        0,
    );
    assert_eq!(cols, vec!["a", "b"]);
});

live_test!(analysis_error_has_class_and_span, {
    let c = live_client!();
    let err = c
        .sql_collect("SELECT * FROM definitely_missing_table")
        .await
        .unwrap_err();
    let ClientError::Spark(e) = err else {
        panic!("expected Spark error, got {err:?}")
    };
    assert_eq!(
        e.error_class.as_deref(),
        Some("TABLE_OR_VIEW_NOT_FOUND"),
        "{e:?}"
    );
    assert_eq!(e.sql_state.as_deref(), Some("42P01"));
    let stmt = "SELECT * FROM definitely_missing_table";
    assert_eq!(e.caret_span(stmt), Some((14, stmt.len() - 1)), "{e:?}");
    assert!(
        !e.message.contains("UnresolvedRelation"),
        "plan dump should be stripped: {}",
        e.message
    );
});

live_test!(parse_error_has_position, {
    let c = live_client!();
    let stmt = "SELECT 1 FORM t";
    let err = c.sql_collect(stmt).await.unwrap_err();
    let ClientError::Spark(e) = err else {
        panic!("expected Spark error, got {err:?}")
    };
    assert_eq!(
        e.error_class.as_deref(),
        Some("PARSE_SYNTAX_ERROR"),
        "{e:?}"
    );
    assert!(e.caret_span(stmt).is_some(), "{e:?}");
    assert!(!e.message.contains("== SQL =="), "{}", e.message);
});

// Cancelling right after submitting lands while Spark is still planning the
// query: the race where a plain interrupt orphans the job (see
// Execution::cancel).
live_test!(cancel_during_planning_leaves_nothing_running, {
    let c = live_client!();
    let exec = c.execute_sql(&endless(), None).await.unwrap();
    exec.cancel().await;
    assert_task_slots_free(&c).await;
});

live_test!(cancel_while_running, {
    let c = live_client!();
    let mut exec = c.execute_sql(&endless(), None).await.unwrap();
    loop {
        match exec.next_event().await.unwrap() {
            Some(ExecEvent::Progress(p)) if p.stages_total > 0 => break,
            Some(_) => continue,
            None => panic!("the endless query ended on its own"),
        }
    }
    let start = Instant::now();
    exec.cancel().await;
    assert!(
        start.elapsed() < Duration::from_secs(15),
        "cancel took {:?}",
        start.elapsed()
    );
    assert_task_slots_free(&c).await;
});

fn cores() -> usize {
    std::thread::available_parallelism().map_or(8, |n| n.get())
}

/// A query that never ends unless cancelled (1 ms per row), with one partition
/// per core, so if its tasks outlive a cancel they occupy every task slot of a
/// `local[*]` server on this machine.
fn endless() -> String {
    format!(
        "SELECT count(*) FROM range(0, 100000000000, 1, {}) \
         WHERE reflect('java.lang.Thread', 'sleep', 1L) IS NULL",
        cores()
    )
}

/// Fails (fast) if cancelled work is still occupying Spark's task slots, by
/// running a query that needs every slot. Orphaned tasks from the endless
/// query would make it hang.
async fn assert_task_slots_free(c: &SparkClient) {
    let sql = format!("SELECT count(*) FROM range(0, 1000, 1, {})", cores() * 2);
    let r = tokio::time::timeout(Duration::from_secs(60), c.sql_collect(&sql)).await;
    assert!(
        matches!(r, Ok(Ok(_))),
        "Spark couldn't schedule tasks after a cancel; cancelled work is still running: {r:?}"
    );
}

// A client giving up on an execution (error path, early return) without
// cancelling it explicitly. Dropped immediately, i.e. during planning.
live_test!(dropping_an_execution_cancels_it, {
    let c = live_client!();
    let exec = c.execute_sql(&endless(), None).await.unwrap();
    drop(exec);
    assert_task_slots_free(&c).await;
});

live_test!(catalog_loads_in_background, {
    let c = live_client!();
    c.sql_collect("CREATE OR REPLACE TEMP VIEW sl_cat_v AS SELECT 1 AS col_one")
        .await
        .unwrap();
    let catalog = Catalog::spawn(c, &tokio::runtime::Handle::current());
    let cat = catalog.clone();
    tokio::task::spawn_blocking(move || {
        cat.request(Load::Columns("sl_cat_v".into()));
        cat.wait_for(
            &[
                Load::CurrentDb,
                Load::Databases,
                Load::Functions,
                Load::Columns("sl_cat_v".into()),
            ],
            Duration::from_secs(30),
        );
        // CurrentDb triggers a Tables load for it.
        cat.wait_for(&[Load::Tables("default".into())], Duration::from_secs(30));
    })
    .await
    .unwrap();
    let s = catalog.state();
    assert_eq!(s.current_db.as_deref(), Some("default"));
    assert!(s
        .databases
        .as_ref()
        .unwrap()
        .contains(&"default".to_string()));
    assert!(s
        .functions
        .as_ref()
        .unwrap()
        .iter()
        .any(|f| f == "coalesce"));
    assert_eq!(
        s.columns.get("sl_cat_v"),
        Some(&vec!["col_one".to_string()])
    );
});

live_test!(alias_column_completion_end_to_end, {
    use sparkline_core::complete::analyze;
    use sparkline_core::suggest::suggest;
    let c = live_client!();
    c.sql_collect("CREATE OR REPLACE TEMP VIEW sl_alias_v AS SELECT 1 AS event_id, 2 AS user_id")
        .await
        .unwrap();
    let catalog = Catalog::spawn(c, &tokio::runtime::Handle::current());
    let line = "SELECT e. FROM sl_alias_v e";
    let pos = "SELECT e.".len();
    let cat = catalog.clone();
    let values = tokio::task::spawn_blocking(move || {
        let ctx = analyze(line, pos).unwrap();
        let first = suggest(&ctx, &cat.state());
        eprintln!("ctx={ctx:?}\nmissing={:?}", first.missing);
        for l in &first.missing {
            cat.request(l.clone());
        }
        cat.wait_for(&first.missing, Duration::from_secs(30));
        let s = suggest(&ctx, &cat.state());
        eprintln!("columns cache={:?}", cat.state().columns);
        s.candidates
            .into_iter()
            .map(|c| c.value)
            .collect::<Vec<_>>()
    })
    .await
    .unwrap();
    assert_eq!(values, vec!["event_id", "user_id"]);
});
