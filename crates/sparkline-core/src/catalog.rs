//! Background-loaded catalog cache for completion.
//!
//! Everything is loaded lazily with plain SQL (`SHOW DATABASES`, `SHOW TABLES
//! IN`, `SHOW COLUMNS IN`, `SHOW FUNCTIONS`) on the shared Spark Connect
//! session, so it works against any catalog Spark can talk to, and large
//! metastores are only loaded one database or table at a time.
//!
//! The editor runs on a plain thread, so the API here is synchronous:
//! `request` queues a load, `wait_for` blocks briefly for it to finish.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};

use arrow_array::RecordBatch;
use arrow_cast::display::{ArrayFormatter, FormatOptions};
use tokio::sync::mpsc;

use crate::client::SparkClient;
use crate::complete::quote_ident;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    pub name: String,
    pub is_temp: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CatalogState {
    pub current_db: Option<String>,
    pub databases: Option<Vec<String>>,
    /// Lowercased database name → tables (includes temp views).
    pub tables: HashMap<String, Vec<TableInfo>>,
    /// Lowercased table reference as written in a query → column names.
    pub columns: HashMap<String, Vec<String>>,
    pub functions: Option<Vec<String>>,
}

impl CatalogState {
    fn is_loaded(&self, load: &Load) -> bool {
        match load {
            Load::CurrentDb => self.current_db.is_some(),
            Load::Databases => self.databases.is_some(),
            Load::Tables(db) => self.tables.contains_key(db),
            Load::Columns(t) => self.columns.contains_key(t),
            Load::Functions => self.functions.is_some(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Load {
    CurrentDb,
    Databases,
    /// Lowercased database name.
    Tables(String),
    /// Lowercased dotted table reference.
    Columns(String),
    Functions,
}

struct Shared {
    state: RwLock<CatalogState>,
    inflight: Mutex<HashSet<Load>>,
    done: Condvar,
}

#[derive(Clone)]
pub struct Catalog {
    shared: Arc<Shared>,
    tx: mpsc::UnboundedSender<Load>,
}

impl Catalog {
    /// Starts the loader on `rt` and kicks off the initial loads.
    pub fn spawn(client: SparkClient, rt: &tokio::runtime::Handle) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Load>();
        let shared = Arc::new(Shared {
            state: RwLock::new(CatalogState::default()),
            inflight: Mutex::new(HashSet::new()),
            done: Condvar::new(),
        });
        let catalog = Catalog {
            shared: shared.clone(),
            tx,
        };

        let loader = catalog.clone();
        rt.spawn(async move {
            while let Some(load) = rx.recv().await {
                let (client, loader) = (client.clone(), loader.clone());
                tokio::spawn(async move { loader.run_load(&client, load).await });
            }
        });

        catalog.request(Load::CurrentDb);
        catalog.request(Load::Databases);
        catalog.request(Load::Functions);
        catalog
    }

    pub fn state(&self) -> RwLockReadGuard<'_, CatalogState> {
        self.shared.state.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn current_db(&self) -> Option<String> {
        self.state().current_db.clone()
    }

    /// Queues `load` unless it's already cached or in flight.
    pub fn request(&self, load: Load) {
        if self.state().is_loaded(&load) {
            return;
        }
        let mut inflight = self
            .shared
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if inflight.insert(load.clone()) {
            let _ = self.tx.send(load);
        }
    }

    /// Blocks until none of `loads` is in flight, or `timeout` passes.
    pub fn wait_for(&self, loads: &[Load], timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut inflight = self
            .shared
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while loads.iter().any(|l| inflight.contains(l)) {
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            inflight = match self.shared.done.wait_timeout(inflight, deadline - now) {
                Ok((g, _)) => g,
                Err(e) => e.into_inner().0,
            };
        }
    }

    /// Drops cached databases/tables/columns (after DDL) and reloads the basics.
    pub fn invalidate(&self) {
        {
            let mut s = self.shared.state.write().unwrap_or_else(|e| e.into_inner());
            s.databases = None;
            s.tables.clear();
            s.columns.clear();
        }
        self.request(Load::Databases);
        if let Some(db) = self.current_db() {
            self.request(Load::Tables(db.to_lowercase()));
        }
    }

    /// After `USE`: re-read the current database and load its tables.
    pub fn refresh_current_db(&self) {
        self.shared
            .state
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .current_db = None;
        self.request(Load::CurrentDb);
    }

    async fn run_load(&self, client: &SparkClient, load: Load) {
        let result = self.fetch(client, &load).await;
        {
            let mut s = self.shared.state.write().unwrap_or_else(|e| e.into_inner());
            // On failure, cache an empty entry so completion doesn't retry
            // on every keystroke (e.g. columns of a CTE or a TVF).
            match (&load, result) {
                (Load::CurrentDb, r) => {
                    s.current_db = Some(
                        r.ok()
                            .and_then(|v| v.into_iter().next())
                            .unwrap_or_else(|| "default".into()),
                    )
                }
                (Load::Databases, r) => s.databases = Some(r.unwrap_or_default()),
                (Load::Tables(db), r) => {
                    let tables = r.unwrap_or_default().into_iter().map(|row| {
                        let mut parts = row.splitn(2, '\u{1f}');
                        let name = parts.next().unwrap_or_default().to_string();
                        let is_temp = parts.next() == Some("true");
                        TableInfo { name, is_temp }
                    });
                    s.tables.insert(db.clone(), tables.collect());
                }
                (Load::Columns(t), r) => {
                    s.columns.insert(t.clone(), r.unwrap_or_default());
                }
                (Load::Functions, r) => s.functions = Some(r.unwrap_or_default()),
            }
        }
        if load == Load::CurrentDb {
            if let Some(db) = self.current_db() {
                self.request(Load::Tables(db.to_lowercase()));
            }
        }
        let mut inflight = self
            .shared
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        inflight.remove(&load);
        self.shared.done.notify_all();
    }

    async fn fetch(
        &self,
        client: &SparkClient,
        load: &Load,
    ) -> Result<Vec<String>, crate::error::ClientError> {
        Ok(match load {
            Load::CurrentDb => column(&client.sql_collect("SELECT current_database()").await?, 0),
            Load::Databases => column(&client.sql_collect("SHOW DATABASES").await?, 0),
            Load::Tables(db) => {
                let batches = client
                    .sql_collect(&format!("SHOW TABLES IN {}", quote_parts(db)))
                    .await?;
                // columns: namespace, tableName, isTemporary
                let names = column(&batches, 1);
                let temps = column(&batches, 2);
                names
                    .into_iter()
                    .zip(temps.into_iter().chain(std::iter::repeat(String::new())))
                    .map(|(n, t)| format!("{n}\u{1f}{t}"))
                    .collect()
            }
            Load::Columns(t) => column(
                &client
                    .sql_collect(&format!("SHOW COLUMNS IN {}", quote_parts(t)))
                    .await?,
                0,
            ),
            Load::Functions => {
                let mut fns: Vec<String> = column(&client.sql_collect("SHOW FUNCTIONS").await?, 0)
                    .into_iter()
                    .map(|f| f.rsplit('.').next().unwrap_or_default().to_string())
                    .filter(|f| {
                        f.chars()
                            .next()
                            .is_some_and(|c| c.is_alphabetic() || c == '_')
                            && f.chars().all(|c| c.is_alphanumeric() || c == '_')
                    })
                    .collect();
                fns.sort();
                fns.dedup();
                fns
            }
        })
    }
}

fn quote_parts(dotted: &str) -> String {
    dotted
        .split('.')
        .map(quote_ident)
        .collect::<Vec<_>>()
        .join(".")
}

/// Column `idx` of all batches, rendered as strings.
pub fn column(batches: &[RecordBatch], idx: usize) -> Vec<String> {
    let opts = FormatOptions::default();
    let mut out = Vec::new();
    for b in batches {
        if idx >= b.num_columns() {
            continue;
        }
        if let Ok(f) = ArrayFormatter::try_new(b.column(idx).as_ref(), &opts) {
            out.extend((0..b.num_rows()).map(|i| f.value(i).to_string()));
        }
    }
    out
}
