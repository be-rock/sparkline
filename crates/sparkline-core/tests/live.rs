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

#[tokio::test]
async fn version_and_simple_query() {
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
}

#[tokio::test]
async fn limit_wrapping() {
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
}

#[tokio::test]
async fn ddl_and_temp_views() {
    let c = live_client!();
    c.sql_collect("CREATE OR REPLACE TEMP VIEW sl_test_v AS SELECT 1 AS a, 'x' AS b")
        .await
        .unwrap();
    let cols = column(
        &c.sql_collect("SHOW COLUMNS IN sl_test_v").await.unwrap(),
        0,
    );
    assert_eq!(cols, vec!["a", "b"]);
}

#[tokio::test]
async fn analysis_error_has_class_and_span() {
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
}

#[tokio::test]
async fn parse_error_has_position() {
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
}

#[tokio::test]
async fn interrupt_cancels_long_query() {
    let c = live_client!();
    let mut exec = c
        .execute_sql("SELECT count(*) FROM range(0, 100000000000, 1, 4) WHERE reflect('java.lang.Thread', 'sleep', 1L) IS NULL", None)
        .await
        .unwrap();
    let op = exec.operation_id.clone();
    let c2 = c.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        c2.interrupt(&op).await.unwrap();
    });
    let start = Instant::now();
    let result = loop {
        match exec.next_event().await {
            Ok(Some(_)) => continue,
            other => break other,
        }
    };
    assert!(
        matches!(result, Err(ClientError::Cancelled)),
        "got {result:?}"
    );
    assert!(start.elapsed() < Duration::from_secs(30));
}

#[tokio::test(flavor = "multi_thread")]
async fn catalog_loads_in_background() {
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
}

#[tokio::test(flavor = "multi_thread")]
async fn alias_column_completion_end_to_end() {
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
}
