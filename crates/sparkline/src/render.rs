//! Terminal rendering: result tables, errors with carets, progress, timings.

use std::fmt::Write as _;
use std::time::Duration;

use arrow_array::RecordBatch;
use arrow_cast::display::{ArrayFormatter, FormatOptions};
use arrow_schema::{DataType, SchemaRef};
use comfy_table::modifiers::UTF8_ROUND_CORNERS;
use comfy_table::presets::UTF8_FULL_CONDENSED;
use comfy_table::{CellAlignment, ContentArrangement, Table};
use nu_ansi_term::{Color, Style};
use sparkline_core::{ClientError, Progress};

const MAX_CELL_CHARS: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expanded {
    /// Vertical layout only when the table is wider than the terminal.
    Auto,
    On,
    Off,
}

impl Expanded {
    pub fn next(self) -> Self {
        match self {
            Expanded::Auto => Expanded::On,
            Expanded::On => Expanded::Off,
            Expanded::Off => Expanded::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RenderOpts {
    pub color: bool,
    pub expanded: Expanded,
    /// Terminal width, if known.
    pub width: Option<u16>,
}

pub fn terminal_width() -> Option<u16> {
    Table::new().width()
}

fn paint(color: bool, style: Style, s: &str) -> String {
    if color {
        style.paint(s).to_string()
    } else {
        s.to_string()
    }
}

/// Spark-style timestamps: `2024-01-01 12:00:00.123`. Zoned timestamps
/// arrive tagged with the session time zone and are shown as local time in
/// that zone, as Spark itself prints them.
const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.f";

fn format_options() -> FormatOptions<'static> {
    FormatOptions::default()
        .with_display_error(true)
        .with_timestamp_format(Some(TIMESTAMP_FORMAT))
        .with_timestamp_tz_format(Some(TIMESTAMP_FORMAT))
}

/// Formats every cell as display strings (`None` = SQL NULL).
fn cells(schema: &SchemaRef, batches: &[RecordBatch]) -> Vec<Vec<Option<String>>> {
    let opts = format_options();
    let mut rows = Vec::new();
    for b in batches {
        // A column arrow can't format shows the reason instead of failing silently.
        let fmts: Vec<Result<ArrayFormatter, String>> = b
            .columns()
            .iter()
            .map(|c| ArrayFormatter::try_new(c.as_ref(), &opts).map_err(|e| format!("<{e}>")))
            .collect();
        for r in 0..b.num_rows() {
            let row = (0..schema.fields().len())
                .map(|c| {
                    let col = b.column(c);
                    if col.is_null(r) {
                        return None;
                    }
                    Some(match &fmts[c] {
                        Ok(f) => f.value(r).to_string(),
                        Err(e) => e.clone(),
                    })
                })
                .collect();
            rows.push(row);
        }
    }
    rows
}

fn sanitize(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars + 4));
    for (n, ch) in s.chars().enumerate() {
        if n >= max_chars {
            out.push('…');
            break;
        }
        match ch {
            '\n' => out.push('↵'),
            '\r' => {}
            '\t' => out.push(' '),
            c if c.is_control() => out.push('�'),
            c => out.push(c),
        }
    }
    out
}

pub fn render_results(schema: &SchemaRef, batches: &[RecordBatch], opts: RenderOpts) -> String {
    let rows = cells(schema, batches);
    let table = render_table(schema, &rows, opts);
    let too_wide = match opts.width {
        Some(w) => table.lines().any(|l| visible_width(l) > w as usize),
        None => false,
    };
    let expand = schema.fields().len() > 1
        && match opts.expanded {
            Expanded::On => true,
            Expanded::Off => false,
            Expanded::Auto => too_wide,
        };
    if expand {
        render_expanded(schema, &rows, opts)
    } else {
        table
    }
}

fn render_table(schema: &SchemaRef, rows: &[Vec<Option<String>>], opts: RenderOpts) -> String {
    let mut t = Table::new();
    t.load_preset(UTF8_FULL_CONDENSED)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Disabled);
    if opts.color {
        t.enforce_styling();
    }
    t.set_header(schema.fields().iter().map(|f| {
        format!(
            "{}\n{}",
            paint(opts.color, Style::new().bold(), f.name()),
            paint(
                opts.color,
                Style::new().fg(Color::DarkGray),
                &sanitize(&spark_type(f.data_type()), 40)
            )
        )
    }));
    let null = paint(
        opts.color,
        Style::new().fg(Color::DarkGray).italic(),
        "NULL",
    );
    for row in rows {
        t.add_row(row.iter().map(|c| match c {
            Some(v) => sanitize(v, MAX_CELL_CHARS),
            None => null.clone(),
        }));
    }
    for (i, f) in schema.fields().iter().enumerate() {
        if f.data_type().is_numeric() {
            if let Some(col) = t.column_mut(i) {
                col.set_cell_alignment(CellAlignment::Right);
            }
        }
    }
    t.to_string()
}

/// psql-style `\x` layout: one block per record.
fn render_expanded(schema: &SchemaRef, rows: &[Vec<Option<String>>], opts: RenderOpts) -> String {
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    let label_w = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);
    let value_max = opts
        .width
        .map_or(500, |w| (w as usize).saturating_sub(label_w + 3).max(20));
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        let header = format!("─[ RECORD {} ]", i + 1);
        let pad = opts
            .width
            .map_or(40, |w| (w as usize).min(80))
            .saturating_sub(header.chars().count());
        let _ = writeln!(
            out,
            "{}",
            paint(
                opts.color,
                Style::new().fg(Color::DarkGray),
                &format!("{header}{}", "─".repeat(pad))
            )
        );
        for (name, v) in names.iter().zip(row) {
            let value = match v {
                Some(v) => sanitize(v, value_max),
                None => paint(
                    opts.color,
                    Style::new().fg(Color::DarkGray).italic(),
                    "NULL",
                ),
            };
            let _ = writeln!(
                out,
                "{} │ {value}",
                paint(
                    opts.color,
                    Style::new().bold(),
                    &format!("{name:<label_w$}")
                )
            );
        }
    }
    out.trim_end().to_string()
}

/// Display width ignoring ANSI escapes (good enough for box-drawing tables).
fn visible_width(s: &str) -> usize {
    let mut w = 0;
    let mut in_esc = false;
    for c in s.chars() {
        match (in_esc, c) {
            (false, '\x1b') => in_esc = true,
            (true, 'm') => in_esc = false,
            (true, _) => {}
            (false, _) => w += 1,
        }
    }
    w
}

/// Spark SQL type names for Arrow types as Spark Connect sends them.
pub fn spark_type(dt: &DataType) -> String {
    use DataType::*;
    match dt {
        Null => "void".into(),
        Boolean => "boolean".into(),
        Int8 => "tinyint".into(),
        Int16 => "smallint".into(),
        Int32 => "int".into(),
        Int64 => "bigint".into(),
        Float32 => "float".into(),
        Float64 => "double".into(),
        Utf8 | LargeUtf8 | Utf8View => "string".into(),
        Binary | LargeBinary | BinaryView | FixedSizeBinary(_) => "binary".into(),
        Date32 | Date64 => "date".into(),
        Timestamp(_, Some(_)) => "timestamp".into(),
        Timestamp(_, None) => "timestamp_ntz".into(),
        Decimal128(p, s) | Decimal256(p, s) => format!("decimal({p},{s})"),
        List(f) | LargeList(f) | FixedSizeList(f, _) => {
            format!("array<{}>", spark_type(f.data_type()))
        }
        Struct(fields) => format!(
            "struct<{}>",
            fields
                .iter()
                .map(|f| format!("{}:{}", f.name(), spark_type(f.data_type())))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Map(entries, _) => match entries.data_type() {
            Struct(kv) if kv.len() == 2 => format!(
                "map<{},{}>",
                spark_type(kv[0].data_type()),
                spark_type(kv[1].data_type())
            ),
            _ => "map".into(),
        },
        Interval(_) | Duration(_) => "interval".into(),
        other => other.to_string().to_lowercase(),
    }
}

/// Renders an error; for Spark errors with a known position, shows the
/// offending statement line with a caret underneath.
pub fn render_error(err: &ClientError, stmt: Option<&str>, color: bool) -> String {
    let red = Style::new().fg(Color::Red).bold();
    let dim = Style::new().fg(Color::DarkGray);
    let mut out = String::new();
    match err {
        ClientError::Spark(e) => {
            let _ = write!(out, "{} {}", paint(color, red, "error:"), e.message);
            if let Some((line, col, len)) =
                stmt.and_then(|s| e.caret_span(s).and_then(|span| caret_line(s, span)))
            {
                let gutter = paint(color, dim, "  │ ");
                let _ = write!(
                    out,
                    "\n{gutter}{line}\n{gutter}{}{}",
                    " ".repeat(col),
                    paint(color, red, &"^".repeat(len.max(1)))
                );
            }
        }
        ClientError::Cancelled => {
            let _ = write!(
                out,
                "{}",
                paint(color, Style::new().fg(Color::Yellow), "query cancelled")
            );
        }
        other => {
            let _ = write!(out, "{} {other}", paint(color, red, "error:"));
        }
    }
    out
}

/// For a char span in `stmt`, returns (the line containing the start,
/// caret column, caret length clipped to that line).
fn caret_line(stmt: &str, (start, stop): (usize, usize)) -> Option<(String, usize, usize)> {
    let mut offset = 0;
    for line in stmt.split('\n') {
        let n = line.chars().count();
        if start <= offset + n {
            let col = start - offset;
            let end = stop.min(offset + n.saturating_sub(1));
            let len = end.saturating_sub(start) + 1;
            // Tabs would misalign the caret; render them as single spaces.
            return Some((
                line.replace('\t', " "),
                col,
                len.min(n.saturating_sub(col).max(1)),
            ));
        }
        offset += n + 1;
    }
    None
}

pub fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.2} s", d.as_secs_f64())
    } else {
        let s = d.as_secs();
        format!("{}m {:02}s", s / 60, s % 60)
    }
}

pub fn format_bytes(b: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub fn format_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn progress_line(
    frame: usize,
    elapsed: Duration,
    p: Option<&Progress>,
    note: Option<&str>,
) -> String {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let mut s = format!(
        "{} {}",
        FRAMES[frame % FRAMES.len()],
        format_duration_coarse(elapsed)
    );
    if let Some(note) = note {
        let _ = write!(s, " · {note}");
    } else if let Some(p) = p.filter(|p| p.stages_total > 0) {
        let _ = write!(s, " · stages {}/{}", p.stages_done, p.stages_total);
        if p.tasks_total > 0 {
            let pct = p.tasks_done as f64 * 100.0 / p.tasks_total as f64;
            let _ = write!(s, " · tasks {}/{} ({pct:.0}%)", p.tasks_done, p.tasks_total);
        }
        if p.bytes_read > 0 {
            let _ = write!(s, " · {} read", format_bytes(p.bytes_read));
        }
    }
    s
}

fn format_duration_coarse(d: Duration) -> String {
    if d.as_secs() < 60 {
        format!("{:.1} s", d.as_secs_f64())
    } else {
        format_duration(d)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{ArrayRef, Int64Array, StringArray};
    use arrow_schema::{Field, Schema};
    use sparkline_core::SparkError;

    use super::*;

    fn batch() -> (SchemaRef, Vec<RecordBatch>) {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let cols: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(vec![1, 22])),
            Arc::new(StringArray::from(vec![Some("a\nb"), None])),
        ];
        (
            schema.clone(),
            vec![RecordBatch::try_new(schema, cols).unwrap()],
        )
    }

    #[test]
    fn table_plain() {
        let (s, b) = batch();
        let out = render_results(
            &s,
            &b,
            RenderOpts {
                color: false,
                expanded: Expanded::Off,
                width: None,
            },
        );
        assert!(out.contains("bigint") && out.contains("string"), "{out}");
        assert!(out.contains("a↵b"), "newlines are made visible: {out}");
        assert!(out.contains("NULL"));
    }

    #[test]
    fn auto_expands_when_too_wide() {
        let (s, b) = batch();
        let out = render_results(
            &s,
            &b,
            RenderOpts {
                color: false,
                expanded: Expanded::Auto,
                width: Some(10),
            },
        );
        assert!(
            out.contains("RECORD 1") && out.contains("RECORD 2"),
            "{out}"
        );
        let out = render_results(
            &s,
            &b,
            RenderOpts {
                color: false,
                expanded: Expanded::Auto,
                width: Some(200),
            },
        );
        assert!(!out.contains("RECORD"), "{out}");
    }

    #[test]
    fn named_time_zone_timestamps() {
        use arrow_array::{Array, TimestampMicrosecondArray};
        // 2024-01-01T18:00:00Z, which is noon in Chicago (UTC-6 in winter).
        let ts = TimestampMicrosecondArray::from(vec![1_704_132_000_000_000])
            .with_timezone("America/Chicago");
        let schema = Arc::new(Schema::new(vec![Field::new(
            "ts",
            ts.data_type().clone(),
            true,
        )]));
        let b = RecordBatch::try_new(schema.clone(), vec![Arc::new(ts) as ArrayRef]).unwrap();
        let out = render_results(
            &schema,
            &[b],
            RenderOpts {
                color: false,
                expanded: Expanded::Off,
                width: None,
            },
        );
        assert!(out.contains("2024-01-01 12:00:00"), "{out}");
        assert!(out.contains("timestamp"), "{out}");
    }

    #[test]
    fn spark_types() {
        assert_eq!(spark_type(&DataType::Decimal128(10, 2)), "decimal(10,2)");
        let list = DataType::List(Arc::new(Field::new("element", DataType::Utf8, true)));
        assert_eq!(spark_type(&list), "array<string>");
    }

    #[test]
    fn error_caret() {
        let e = ClientError::Spark(SparkError {
            message: "[X] bad".into(),
            span: Some((14, 17)),
            ..Default::default()
        });
        let out = render_error(&e, Some("SELECT * FROM nope"), false);
        assert_eq!(
            out,
            "error: [X] bad\n  │ SELECT * FROM nope\n  │               ^^^^"
        );
    }

    #[test]
    fn error_caret_multiline() {
        let e = ClientError::Spark(SparkError {
            message: "m".into(),
            span: Some((17, 20)),
            ..Default::default()
        });
        let out = render_error(&e, Some("SELECT *\nFROM t, nope\nWHERE 1"), false);
        assert!(out.ends_with("  │ FROM t, nope\n  │         ^^^^"), "{out}");
    }

    #[test]
    fn counts_and_sizes() {
        assert_eq!(format_count(1234567), "1,234,567");
        assert_eq!(format_count(12), "12");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_duration(Duration::from_millis(42)), "42 ms");
    }
}
