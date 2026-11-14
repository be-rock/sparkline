# sparkline

A fast terminal SQL client for Apache Spark, written in Rust. It talks to a
[Spark Connect](https://spark.apache.org/docs/latest/spark-connect-overview.html)
server over gRPC, so there's no JVM or Python on the client: it starts in
milliseconds.

```
spark:default > select kind, count(*) AS n
  … from events group by kind;
╭────────┬────────╮
│ kind   ┆      n │
│ string ┆ bigint │
╞════════╪════════╡
│ click  ┆     10 │
╰────────┴────────╯
1 row · 187 ms
spark:default > SELECT * FROM missing_tbl;
error: [TABLE_OR_VIEW_NOT_FOUND] The table or view `missing_tbl` cannot be found. … SQLSTATE: 42P01
  │ SELECT * FROM missing_tbl
  │               ^^^^^^^^^^^
```

## Features

- **Fast**: a 6 MB native binary; connecting, running `SELECT 1` and exiting
  takes ~20 ms against a local server (the PySpark Connect client takes
  ~300 ms warm).
- **Completion**: Tab completes keywords, functions, databases, tables and
  columns. It's context-aware: tables after `FROM`/`JOIN`, columns of
  the tables in the statement (`e.<Tab>` resolves the alias `e`), tables
  in `db.<Tab>`, CTE names. Catalog metadata loads lazily in the background.
- **Syntax highlighting** as you type, plus history hints, Ctrl-R search and
  persistent history.
- **Multi-line editing**: Enter runs the input once it ends with `;`
  (Alt-Enter forces a newline). Several statements can run at once.
- **Ctrl-C cancels the running Spark job** on the server, not just the client.
- **Live progress**: elapsed time, stages, tasks and bytes read while a
  query runs.
- **Readable errors**: Spark's error class and SQLSTATE, with the plan dump
  stripped and a caret under the offending SQL.
- **Sensible result display**: a type row under the column names, NULLs
  shown distinctly, numbers right-aligned, and a vertical record layout when
  the table is wider than the terminal. Queries are capped at 1,000 rows by
  default (applied server-side as a LIMIT).
- **Scriptable**: `-e "SQL"`, `-f file.sql` or SQL piped on stdin; tables go
  to stdout, status to stderr, and the exit code is non-zero on error.

## Quick start

```sh
cargo build --release
# Start a local Spark 4.0 Connect server (uses uv + pyspark from PyPI):
dev/spark-connect-server.sh &
target/release/sparkline                       # sc://localhost:15002 by default
target/release/sparkline -r sc://host:15002 -e "SHOW DATABASES"
```

Requires a Spark Connect server. Tested against Spark 4.0.5; 3.5 should work
but is untested (live progress needs 4.0). On a cluster, start the server with
`$SPARK_HOME/sbin/start-connect-server.sh`.

## Connecting

The remote is chosen in this order: `--remote`, `--profile`, `SPARK_REMOTE`
(if it's an `sc://` URL), `default_profile` from the config file, then
`sc://localhost:15002`.

Connection strings use the standard Spark Connect format:
`sc://host:port/;token=…;user_id=…;use_ssl=true`. Any other parameter is sent
as a gRPC header (e.g. `x-databricks-cluster-id=…`).

### Config: `~/.config/sparkline/config.toml`

```toml
default_profile = "local"
edit_mode = "vi"          # or "emacs" (default)
max_rows = 1000           # 0 = unlimited

[profiles.local]
remote = "sc://localhost:15002"

[profiles.prod]
# ${VAR} is read from the environment so secrets stay out of the file.
remote = "sc://spark.example.com:443/;token=${SPARK_TOKEN}"
```

History is stored in `~/.local/share/sparkline/history.txt`.

## Commands and keys

| | |
|---|---|
| `\?` | help |
| `\q`, `exit`, Ctrl-D | quit |
| `\l` | list databases |
| `\d [table]` | list tables / describe a table |
| `\limit [n]` | show or set max rows per query (0 = unlimited) |
| `\x` | expanded display: auto → on → off |
| `\refresh` | reload completion metadata |
| `\conninfo` | connection details |
| Tab / Shift-Tab | completion menu |
| Alt-Enter | newline without running |
| Ctrl-C | cancel the running query (twice: stop waiting) |
| Ctrl-R | search history |

## Layout

```
crates/sparkline-core   Spark Connect client, lexer, completion engine, catalog cache
  proto/                vendored Spark Connect protos (see proto/SPARK_VERSION)
crates/sparkline        the REPL binary (reedline, comfy-table)
dev/                    local Spark Connect server script, TUI smoke-test driver
```

`sparkline-core` has no terminal dependencies, so a full-screen (ratatui)
front-end can reuse it as-is.

## Development

```sh
cargo test                                     # unit tests (no server needed)

dev/spark-connect-server.sh &                  # live protocol tests
SPARKLINE_TEST_REMOTE=sc://localhost:15002 cargo test -p sparkline-core --test live

# Interactive smoke test in a pseudo-terminal (prints screen snapshots)
uv pip install --python .spark-venv/bin/python pyte
.spark-venv/bin/python dev/drive_tui.py target/debug/sparkline
```

Protos are compiled with [protox](https://crates.io/crates/protox), so `protoc`
isn't needed. To target another Spark version, replace
`crates/sparkline-core/proto/spark/connect/*.proto` with that release's
`sql/connect/common/src/main/protobuf` files and update `SPARK_VERSION`.

## Not yet

- Reconnecting automatically after a server restart (for now, restart sparkline)
- Reattachable execution, for long queries over flaky networks
- Completion of nested struct fields and of columns in subquery aliases
- Databricks OAuth (token auth works)
- A full-screen mode with a scrollable results grid
