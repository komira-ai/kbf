//! `report`: tables for the paper from the result JSON and the CSV of samples of a result
//! directory.
//!
//! For each cell, one row per arm and window (quiet passes, busy passes), the passes of that
//! window pooled, the warm-up pass 0 left out: how many requests, how many failed, p50 and p95
//! with a bootstrap 95% interval, p99 where it may be claimed, the slowest, and the rate.
//!
//! The rules the numbers keep:
//! - Percentiles are nearest rank over the requests that succeeded, pooled over the passes.
//! - A p99 is printed only for the quiet window and only from 1,000 successes or more (with its
//!   interval and the spread of the per-pass p99 where the passes had enough of their own); a
//!   busy row and a thin row print p50, p95 and the maximum, and say why there is no p99.
//! - A request that failed is counted and its time is shown apart (time to failure), never
//!   among the latencies.
//! - A result file is one attempt (`<cell>-<arm>-a<N>.json`, `run --attempt`): every attempt in
//!   the directory is read, and an arm that has more than one, or an attempt other than the
//!   first, is shown as `arm (attempt N)`; the CSV rows are joined to their attempt.
//! - An arm is labelled `v1` only if the cold store was read before every one of its passes (the
//!   warm-up included) and was healing in none of them. A pass that saw it healing, or a result
//!   with no reading, is listed under the table with the reason, and the label is refused.
//! - Every arm label names its path (`direct`, `direct-mp`, `instance`, `hot`, ...); the notes of
//!   the cell say which mode the instance ran in. The tables are labelled as measurements of a
//!   test instance on test buckets; a cold store that is healing, or busy with other work,
//!   moves cold numbers, and the environment block of the run is where that is recorded.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use crate::cells::CellId;
use crate::cells::stats::{self, bootstrap_ci};
use crate::error::BenchError;
use crate::result::{CellResult, Window};

/// A request of the CSV: whether it succeeded and how long it took.
struct Row {
    ok: bool,
    latency_us: u64,
}

/// The rows of the CSV by (cell, arm, attempt, pass).
type Rows = BTreeMap<(String, String, u32, u32), Vec<Row>>;

/// Splits one CSV line (RFC 4180: a field in quotes may hold commas and `""`).
fn split_csv(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                chars.next();
                fields.last_mut().expect("a field").push('"');
            }
            ('"', _) => quoted = !quoted,
            (',', false) => fields.push(String::new()),
            (c, _) => fields.last_mut().expect("a field").push(c),
        }
    }
    fields
}

/// Parses `samples.csv`.
fn parse_samples(text: &str) -> Result<Rows, BenchError> {
    let mut lines = text.lines();
    let header = split_csv(lines.next().unwrap_or_default());
    let column = |name: &str| {
        header
            .iter()
            .position(|h| h == name)
            .ok_or_else(|| BenchError::Result(format!("samples.csv has no column {name}")))
    };
    let (cell, arm, pass) = (column("cell")?, column("arm")?, column("pass")?);
    let (status, latency) = (column("status")?, column("latency_us")?);
    // A file from before attempts existed has no such column: every row is of the first.
    let attempt = header.iter().position(|h| h == "attempt");
    let mut rows = Rows::new();
    for (number, line) in lines.enumerate().filter(|(_, l)| !l.is_empty()) {
        let bad =
            |what: &str| BenchError::Result(format!("samples.csv line {}: {what}", number + 2));
        let fields = split_csv(line);
        let at = |i: usize| {
            fields
                .get(i)
                .map(String::as_str)
                .ok_or_else(|| bad("too few fields"))
        };
        let pass_no: u32 = at(pass)?
            .parse()
            .map_err(|_| bad("the pass is not a number"))?;
        let latency_us: u64 = at(latency)?
            .parse()
            .map_err(|_| bad("the latency is not a number"))?;
        let ok = at(status)?
            .parse::<u16>()
            .is_ok_and(|s| (200..300).contains(&s));
        let attempt_no: u32 = match attempt {
            Some(i) => at(i)?
                .parse()
                .map_err(|_| bad("the attempt is not a number"))?,
            None => 1,
        };
        rows.entry((
            at(cell)?.to_owned(),
            at(arm)?.to_owned(),
            attempt_no,
            pass_no,
        ))
        .or_default()
        .push(Row { ok, latency_us });
    }
    Ok(rows)
}

/// The results of a directory: every JSON file that has a `schema_version`, in the order of the
/// cells and then of the arms.
fn read_results(dir: &Path) -> Result<Vec<CellResult>, BenchError> {
    let io = |path: &Path| {
        let path = path.to_owned();
        move |source| BenchError::Io { path, source }
    };
    let mut results = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(io(dir))? {
        let path = entry.map_err(io(dir))?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(io(&path))?;
        let is_result = serde_json::from_str::<serde_json::Value>(&text)
            .is_ok_and(|v| v.get("schema_version").is_some());
        if is_result {
            results.push(CellResult::from_json(&text)?);
        }
    }
    let order = |cell: CellId| CellId::ALL.iter().position(|c| *c == cell);
    results.sort_by(|a, b| {
        (order(a.cell), &a.arm, a.provenance.attempt).cmp(&(
            order(b.cell),
            &b.arm,
            b.provenance.attempt,
        ))
    });
    Ok(results)
}

/// `1.50 ms` or `850 us`.
#[must_use]
pub fn fmt_us(us: u64) -> String {
    if us >= 1000 {
        format!("{:.2} ms", us as f64 / 1000.0)
    } else {
        format!("{us} us")
    }
}

/// A value with its interval: `1.50 ms [1.40 ms, 1.60 ms]`.
fn with_ci(value: u64, ci: Option<(u64, u64)>) -> String {
    ci.map_or_else(
        || fmt_us(value),
        |(low, high)| format!("{} [{}, {}]", fmt_us(value), fmt_us(low), fmt_us(high)),
    )
}

/// The requests of the passes of one arm in one window.
#[derive(Default)]
struct Group {
    passes: u64,
    requests: u64,
    failures: u64,
    elapsed_us: u64,
    bytes: u64,
    ok: Vec<u64>,
    failed: Vec<u64>,
    pass_p99: Vec<u64>,
}

/// Pools the passes of `result` that ran in `window`, the warm-up left out.
fn gather(result: &CellResult, rows: &Rows, window: Window) -> Option<Group> {
    let mut group = Group::default();
    for pass in result
        .passes
        .iter()
        .filter(|p| !p.discarded && p.window == window)
    {
        group.passes += 1;
        group.requests += pass.requests;
        group.failures += pass.failures;
        group.elapsed_us += pass.elapsed_us;
        group.bytes += pass.bytes;
        group
            .pass_p99
            .extend(pass.percentiles.and_then(|p| p.p99_us));
        let key = (
            result.cell.as_str().to_owned(),
            result.arm.clone(),
            result.provenance.attempt,
            pass.pass,
        );
        for row in rows.get(&key).into_iter().flatten() {
            if row.ok {
                group.ok.push(row.latency_us);
            } else {
                group.failed.push(row.latency_us);
            }
        }
    }
    (group.passes > 0).then_some(group)
}

/// The cells of one arm in one window, as a table row.
fn table_row(arm: &str, window: Window, mut group: Group) -> String {
    let n = group.ok.len();
    let rate = if group.elapsed_us > 0 {
        let secs = group.elapsed_us as f64 / 1e6;
        format!(
            "{:.1} req/s, {:.2} MiB/s",
            group.requests as f64 / secs,
            group.bytes as f64 / secs / f64::from(1 << 20)
        )
    } else {
        "n/a".to_owned()
    };
    let failures = if group.failed.is_empty() {
        group.failures.to_string()
    } else {
        let mut times = group.failed.clone();
        let s = stats::summarise(&mut times).expect("failures were timed");
        format!(
            "{} (time to failure p50 {}, max {})",
            group.failures,
            fmt_us(s.p50),
            fmt_us(s.max)
        )
    };
    let ok = group.ok.clone();
    let Some(s) = stats::summarise(&mut group.ok) else {
        return format!(
            "| {arm} | {} | {} | {} | {failures} | no request succeeded | | | | {rate} |",
            window.as_str(),
            group.passes,
            group.requests
        );
    };
    let p99 = if window == Window::Busy {
        "not claimed: busy window".to_owned()
    } else if stats::p99_claimable(n) {
        let spread = match (group.pass_p99.iter().min(), group.pass_p99.iter().max()) {
            (Some(low), Some(high)) => format!(", per-pass {}..{}", fmt_us(*low), fmt_us(*high)),
            _ => String::new(),
        };
        format!("{}{spread}", with_ci(s.p99, bootstrap_ci(&ok, 99, 1)))
    } else {
        format!("not claimed: {n} quiet samples, 1000 needed")
    };
    format!(
        "| {arm} | {} | {} | {} | {failures} | {} | {} | {p99} | {} | {rate} |",
        window.as_str(),
        group.passes,
        group.requests,
        with_ci(s.p50, bootstrap_ci(&ok, 50, 1)),
        with_ci(s.p95, bootstrap_ci(&ok, 95, 1)),
        fmt_us(s.max),
    )
}

/// Why `result` may not be labelled `v1`, or `None` if it may: the cold store was read before
/// every pass of it (the warm-up included) and was healing before none.
#[must_use]
pub fn v1_refusal(result: &CellResult) -> Option<String> {
    let mut healing: Vec<u32> = result
        .cold_state
        .iter()
        .filter(|r| r.state.healing)
        .map(|r| r.pass)
        .collect();
    healing.sort_unstable();
    healing.dedup();
    if !healing.is_empty() {
        let passes: Vec<String> = healing.iter().map(u32::to_string).collect();
        return Some(format!(
            "the cold store was healing before pass {}",
            passes.join(", ")
        ));
    }
    if result.passes.is_empty() {
        return Some("it has no pass".to_owned());
    }
    let unread = result
        .passes
        .iter()
        .filter(|p| !result.cold_state.iter().any(|r| r.pass == p.pass))
        .count();
    (unread > 0).then(|| {
        format!(
            "the cold store's state was not read before {unread} of {} passes",
            result.passes.len()
        )
    })
}

/// How the table names the arm of `result`: its label, and its attempt if the cell has more
/// than one attempt of the arm or this is not the first.
fn arm_name(result: &CellResult, of_cell: &[&CellResult]) -> String {
    let tries = of_cell.iter().filter(|r| r.arm == result.arm).count();
    if tries > 1 || result.provenance.attempt != 1 {
        format!("{} (attempt {})", result.arm, result.provenance.attempt)
    } else {
        result.arm.clone()
    }
}

/// The report for `results` with their `rows`.
fn format_report(results: &[CellResult], rows: &Rows) -> String {
    let mut out = String::new();
    writeln!(out, "# tierstore-bench report\n").expect("a string takes text");
    writeln!(
        out,
        "Measurements of a test instance on test buckets. A cold store that is healing or busy with other work moves cold numbers: the cold store is read again before every pass and the reading is in the result (`cold_state`), and an arm is labelled `v1` below only if it was read before every pass and was healing in none. Tierstore at the pinned tag has no cold-only mode, so every `instance` arm ran two-tier (a hot tier of its own beside the cold tier). Pass 0 of every arm is a warm-up and is left out; percentiles are nearest rank over the requests that succeeded; a p99 is printed only for quiet passes with at least 1000 successes.\n"
    )
    .expect("a string takes text");
    if let Some(first) = results.first() {
        let p = &first.provenance;
        writeln!(
            out,
            "run `{}`, tool commit `{}`, binary `{}`, open loop `{}`\n",
            p.run_id,
            p.git_sha,
            p.binary_digest,
            if p.open_loop { "on" } else { "off" }
        )
        .expect("a string takes text");
    }
    let cells: BTreeSet<usize> = results
        .iter()
        .filter_map(|r| CellId::ALL.iter().position(|c| *c == r.cell))
        .collect();
    for index in cells {
        let cell = CellId::ALL[index];
        writeln!(out, "## {}\n", cell.as_str()).expect("a string takes text");
        writeln!(
            out,
            "| arm | window | passes | requests | failed | p50 | p95 | p99 | max | rate |\n|---|---|---|---|---|---|---|---|---|---|"
        )
        .expect("a string takes text");
        let of_cell: Vec<&CellResult> = results.iter().filter(|r| r.cell == cell).collect();
        for result in &of_cell {
            for window in [Window::Quiet, Window::Busy] {
                if let Some(group) = gather(result, rows, window) {
                    let name = arm_name(result, &of_cell);
                    writeln!(out, "{}", table_row(&name, window, group))
                        .expect("a string takes text");
                }
            }
        }
        out.push('\n');
        let mut said = BTreeSet::new();
        for result in &of_cell {
            for note in &result.notes {
                if !note.starts_with("pass ")
                    && !note.starts_with("arms interleaved")
                    && said.insert(note)
                {
                    writeln!(out, "- {note}").expect("a string takes text");
                }
            }
        }
        for result in &of_cell {
            let name = arm_name(result, &of_cell);
            let label = match v1_refusal(result) {
                None => format!(
                    "label v1 (the cold store was read before each of its {} passes and was healing in none)",
                    result.passes.len()
                ),
                Some(why) => format!("not labelled v1: {why}"),
            };
            writeln!(out, "- arm {name}: {label}").expect("a string takes text");
            for pass in result.passes.iter().filter(|p| p.gate_tripped.is_some()) {
                writeln!(
                    out,
                    "- arm {name}: pass {} was cut by the load gate ({})",
                    pass.pass,
                    pass.gate_tripped.as_deref().unwrap_or_default()
                )
                .expect("a string takes text");
            }
            for pass in result.passes.iter() {
                if let Some(trip) = &pass.cold_guard {
                    writeln!(
                        out,
                        "- arm {name}: pass {} was cut by the cold store guard ({trip})",
                        pass.pass
                    )
                    .expect("a string takes text");
                }
            }
            for note in result.notes.iter().filter(|n| n.starts_with("pass ")) {
                writeln!(out, "- arm {name}: {note}").expect("a string takes text");
            }
        }
        out.push('\n');
    }
    out
}

/// Renders the table for `result_dir`.
///
/// # Errors
/// [`BenchError`] if the directory cannot be read, a result file has another schema version, or
/// `samples.csv` is damaged. A directory without `samples.csv` renders rows with no samples.
pub fn render(result_dir: &Path) -> Result<String, BenchError> {
    let results = read_results(result_dir)?;
    let path = result_dir.join("samples.csv");
    let rows = match std::fs::read_to_string(&path) {
        Ok(text) => parse_samples(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Rows::new(),
        Err(source) => return Err(BenchError::Io { path, source }),
    };
    Ok(format_report(&results, &rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::{PassResult, Percentiles, Provenance, SCHEMA_VERSION};

    fn pass(index: u32, window: Window, requests: u64, p99: Option<u64>) -> PassResult {
        PassResult {
            pass: index,
            window,
            discarded: index == 0,
            requests,
            failures: 0,
            bytes: requests * 1024,
            elapsed_us: 2_000_000,
            percentiles: Some(Percentiles {
                p50_us: 1,
                p95_us: 2,
                p99_us: p99,
                max_us: 3,
            }),
            gate_tripped: None,
            cold_guard: None,
        }
    }

    fn result(arm: &str, passes: Vec<PassResult>, notes: &[&str]) -> CellResult {
        CellResult {
            schema_version: SCHEMA_VERSION,
            cell: CellId::ColdPut,
            arm: arm.to_owned(),
            started_unix_ms: 1,
            finished_unix_ms: 2,
            provenance: Provenance {
                run_id: "run-7".to_owned(),
                git_sha: "abc".to_owned(),
                binary_digest: "def".to_owned(),
                ..Provenance::unrecorded()
            },
            passes,
            notes: notes.iter().map(|n| (*n).to_owned()).collect(),
            cold_state: vec![],
        }
    }

    fn rows(arm: &str, pass: u32, latencies: &[u64], status: &str) -> Rows {
        let lines: Vec<String> = latencies
            .iter()
            .map(|l| format!("r,s,d,cold-put,{arm},{pass},quiet,cold,put,10,1,5,{l},{status},1"))
            .collect();
        let text = format!(
            "{}\n{}\n",
            crate::result::Sample::CSV_HEADER,
            lines.join("\n")
        );
        parse_samples(&text).unwrap()
    }

    fn merged(parts: Vec<Rows>) -> Rows {
        let mut all = Rows::new();
        for part in parts {
            for (key, value) in part {
                all.entry(key).or_default().extend(value);
            }
        }
        all
    }

    /// Catches a CSV reader that breaks a quoted field at its comma, or loses an escaped quote.
    #[test]
    fn the_csv_reader_keeps_quoted_fields_whole() {
        assert_eq!(split_csv("a,b,,c"), ["a", "b", "", "c"]);
        assert_eq!(split_csv("a,\"b,c\",d"), ["a", "b,c", "d"]);
        assert_eq!(split_csv("\"say \"\"hi\"\"\",x"), ["say \"hi\"", "x"]);
    }

    /// Catches samples read into the wrong pass or arm, a status that is not a success counted
    /// as one, a blank status taken for success, and damaged files read without a word.
    #[test]
    fn samples_are_grouped_by_cell_arm_and_pass_and_damage_is_named() {
        let all = merged(vec![
            rows("a", 1, &[10, 20], "200"),
            rows("a", 1, &[30], "503"),
            rows("a", 2, &[40], ""),
            rows("b", 1, &[50], "204"),
        ]);
        let key = |arm: &str, pass| ("cold-put".to_owned(), arm.to_owned(), 1, pass);
        let ok = |k| all[&k].iter().filter(|r| r.ok).count();
        assert_eq!((all[&key("a", 1)].len(), ok(key("a", 1))), (3, 2));
        assert_eq!((all[&key("a", 2)].len(), ok(key("a", 2))), (1, 0));
        assert_eq!(ok(key("b", 1)), 1);

        let err = |text: &str| parse_samples(text).err().unwrap().to_string();
        assert!(err("x,y\n1,2\n").contains("no column cell"));
        let header = crate::result::Sample::CSV_HEADER;
        assert!(err(&format!("{header}\nshort\n")).contains("line 2: too few fields"));
        let line = |pass: &str, lat: &str| {
            format!("r,s,d,cold-put,a,{pass},quiet,cold,put,10,1,5,{lat},200,1")
        };
        assert!(
            err(&format!("{header}\n{}\n", line("x", "1"))).contains("the pass is not a number")
        );
        assert!(
            err(&format!("{header}\n{}\n", line("1", "y"))).contains("the latency is not a number")
        );
        assert!(parse_samples("").is_err());
        assert!(parse_samples(&format!("{header}\n\n")).unwrap().is_empty());
    }

    /// Catches a p99 claimed from thin or busy samples, one not claimed from a thousand quiet
    /// ones, the warm-up pass counted, the failures timed among the latencies, and an interval
    /// that is missing: three quiet passes and a busy one of one arm.
    #[test]
    fn a_p99_is_printed_only_for_a_thousand_quiet_successes() {
        let thousand: Vec<u64> = (1..=1000).collect();
        let mut with_failure = pass(1, Window::Quiet, 1000, Some(990));
        with_failure.failures = 1;
        let r = result(
            "big",
            vec![
                pass(0, Window::Quiet, 1000, None),
                with_failure,
                pass(2, Window::Busy, 1000, None),
            ],
            &[],
        );
        let data = merged(vec![
            rows("big", 0, &[9_999_999], "200"),
            rows("big", 1, &thousand, "200"),
            rows("big", 1, &[7000], "500"),
            rows("big", 2, &thousand, "200"),
        ]);
        let text = format_report(&[r], &data);
        let quiet = text
            .lines()
            .find(|l| l.contains("| big | quiet |"))
            .unwrap();
        assert!(
            quiet.contains("| 1 | 1000 | 1 (time to failure p50 7.00 ms, max 7.00 ms) |"),
            "{quiet}"
        );
        assert!(quiet.contains("500 us ["), "{quiet}");
        assert!(quiet.contains("990 us ["), "{quiet}");
        assert!(quiet.contains(", per-pass 990 us..990 us"), "{quiet}");
        assert!(!quiet.contains("9999"), "the warm-up is left out: {quiet}");
        assert!(quiet.contains("500.0 req/s, 0.49 MiB/s"), "{quiet}");
        let busy = text.lines().find(|l| l.contains("| big | busy |")).unwrap();
        assert!(busy.contains("not claimed: busy window"), "{busy}");

        let pooled = result(
            "pooled",
            vec![
                pass(1, Window::Quiet, 600, None),
                pass(2, Window::Quiet, 600, None),
            ],
            &[],
        );
        let both = merged(vec![
            rows("pooled", 1, &(1..=600).collect::<Vec<u64>>(), "200"),
            rows("pooled", 2, &(1..=600).collect::<Vec<u64>>(), "200"),
        ]);
        let line = format_report(&[pooled], &both)
            .lines()
            .find(|l| l.contains("| pooled |"))
            .unwrap()
            .to_owned();
        assert!(line.contains("| 2 | 1200 |"), "{line}");
        assert!(
            line.contains("594 us ["),
            "the p99 of 1200 pooled quiet samples: {line}"
        );
        assert!(!line.contains("per-pass"), "{line}");
        assert!(!line.contains("not claimed"), "{line}");

        let thin = result("thin", vec![pass(1, Window::Quiet, 5, None)], &[]);
        let text = format_report(&[thin], &rows("thin", 1, &[100, 200, 300], "200"));
        let line = text.lines().find(|l| l.contains("| thin |")).unwrap();
        assert!(
            line.contains("not claimed: 3 quiet samples, 1000 needed"),
            "{line}"
        );
        assert!(line.contains("200 us [100 us, 300 us]"), "{line}");
    }

    /// Catches an arm with nothing succeeded or nothing recorded being given numbers, and a rate
    /// divided by a pass that took no time.
    #[test]
    fn an_arm_with_no_success_or_no_samples_says_so() {
        let mut empty_pass = pass(1, Window::Quiet, 4, None);
        empty_pass.elapsed_us = 0;
        let r = result("dead", vec![empty_pass], &[]);
        let failed = rows("dead", 1, &[5000, 9000], "");
        let text = format_report(std::slice::from_ref(&r), &failed);
        let line = text.lines().find(|l| l.contains("| dead |")).unwrap();
        assert!(line.contains("no request succeeded"), "{line}");
        assert!(
            line.contains("(time to failure p50 5.00 ms, max 9.00 ms)"),
            "{line}"
        );
        assert!(line.trim_end().ends_with("| n/a |"), "{line}");
        let text = format_report(&[r], &Rows::new());
        assert!(text.contains("no request succeeded"));
        assert!(format_report(&[], &Rows::new()).contains("# tierstore-bench report"));
    }

    /// Catches notes lost or repeated: a note several arms share is printed once, the
    /// interleaving note and the per-pass notes are not among the cell's, a pass cut by the gate
    /// is listed with its measure, and the provenance is named.
    #[test]
    fn notes_are_printed_once_and_a_tripped_pass_is_listed() {
        let mut cut = pass(1, Window::Busy, 2, None);
        cut.gate_tripped = Some("psi_io_avg10".to_owned());
        let a = result(
            "a",
            vec![cut],
            &[
                "shared note",
                "arms interleaved, the order of each pass: x",
                "pass 1: 5 requests went through the fault proxy in the pass",
            ],
        );
        let b = result(
            "b",
            vec![pass(1, Window::Quiet, 1, None)],
            &["shared note", "own note"],
        );
        let text = format_report(&[a, b], &Rows::new());
        assert_eq!(text.matches("- shared note").count(), 1);
        assert_eq!(text.matches("- own note").count(), 1);
        assert!(!text.contains("- arms interleaved"));
        assert!(text.contains("- arm a: pass 1 was cut by the load gate (psi_io_avg10)"));
        assert!(
            text.contains("- arm a: pass 1: 5 requests went through the fault proxy in the pass")
        );
        assert!(text.contains("run `run-7`, tool commit `abc`, binary `def`"));
        assert!(text.contains("two-tier"));
    }

    /// Catches a report that does not say whether the run had its open-loop arms: a reader of a
    /// closed-loop-only table would take the missing arms for a failure.
    #[test]
    fn the_report_header_says_whether_the_open_loop_arms_were_run() {
        let mut a = result("a", vec![pass(1, Window::Quiet, 1, None)], &[]);
        let on = format_report(&[a.clone()], &Rows::new());
        assert!(on.contains(", open loop `on`"), "{on}");
        a.provenance.open_loop = false;
        let off = format_report(&[a], &Rows::new());
        assert!(off.contains(", open loop `off`"), "{off}");
    }

    /// A result of `arm` at `attempt`, with one quiet pass of 1,000 requests, and the cold store
    /// read before each of `passes` passes with `healing[pass]`.
    fn with_cold(arm: &str, attempt: u32, healing: &[bool]) -> CellResult {
        let mut r = result(
            arm,
            (0..healing.len() as u32)
                .map(|p| pass(p, Window::Quiet, 1000, None))
                .collect(),
            &[],
        );
        r.provenance.attempt = attempt;
        r.cold_state = healing
            .iter()
            .enumerate()
            .map(|(p, h)| crate::result::PassColdState {
                pass: p as u32,
                state: crate::result::ColdState {
                    healing: *h,
                    percent_done: None,
                    drives: vec![],
                    read_unix_ms: 7,
                    source: None,
                },
            })
            .collect();
        r
    }

    /// Catches a result labelled v1 that saw the cold store healing in any pass (the warm-up
    /// included), one labelled on no reading or on a reading missing for a pass, and a result
    /// refused that was read before every pass and found quiet each time.
    #[test]
    fn a_result_is_labelled_v1_only_if_no_pass_saw_the_cold_store_healing() {
        assert_eq!(v1_refusal(&with_cold("a", 1, &[false, false, false])), None);
        let why = v1_refusal(&with_cold("a", 1, &[false, true, false])).unwrap();
        assert_eq!(why, "the cold store was healing before pass 1");
        let why = v1_refusal(&with_cold("a", 1, &[true, true, false])).unwrap();
        assert_eq!(why, "the cold store was healing before pass 0, 1");
        let mut unread = with_cold("a", 1, &[false, false, false]);
        unread.cold_state.remove(1);
        let why = v1_refusal(&unread).unwrap();
        assert_eq!(
            why,
            "the cold store's state was not read before 1 of 3 passes"
        );
        let mut none = unread.clone();
        none.cold_state.clear();
        assert!(v1_refusal(&none).unwrap().contains("3 of 3 passes"));
        let mut empty = with_cold("a", 1, &[]);
        empty.passes.clear();
        assert_eq!(v1_refusal(&empty).unwrap(), "it has no pass");

        let text = format_report(
            &[
                with_cold("quiet", 1, &[false, false]),
                with_cold("healing", 1, &[false, true]),
            ],
            &Rows::new(),
        );
        assert!(text.contains("- arm quiet: label v1 (the cold store was read before each of its 2 passes and was healing in none)"), "{text}");
        assert!(
            text.contains(
                "- arm healing: not labelled v1: the cold store was healing before pass 1"
            ),
            "{text}"
        );
        assert!(!text.contains("- arm healing: label v1"), "{text}");
    }

    /// Catches attempts read as one: the two attempts of an arm are two rows, each labelled with
    /// its number, each joined to its own samples (the same pass index in both), while an arm
    /// with a single first attempt keeps its plain name.
    #[test]
    fn every_attempt_is_read_and_labelled_and_keeps_its_own_samples() {
        let rows_of = |attempt: u32, latencies: &[u64]| {
            let lines: Vec<String> = latencies
                .iter()
                .map(|l| format!("r,s,d,cold-put,a,1,quiet,cold,put,10,1,5,{l},200,{attempt}"))
                .collect();
            let text = format!(
                "{}\n{}\n",
                crate::result::Sample::CSV_HEADER,
                lines.join("\n")
            );
            parse_samples(&text).unwrap()
        };
        let first = with_cold("a", 1, &[false, false]);
        let second = with_cold("a", 2, &[false, false]);
        let lone = with_cold("b", 1, &[false, false]);
        // A lone attempt that is not the first is still named.
        let retried = with_cold("c", 3, &[false, false]);
        let data = merged(vec![rows_of(1, &[100, 100, 100]), rows_of(2, &[900, 900])]);
        let text = format_report(&[first, second, lone, retried], &data);
        assert!(text.contains("| c (attempt 3) |"), "{text}");
        let one = text
            .lines()
            .find(|l| l.contains("| a (attempt 1) | quiet |"))
            .unwrap();
        let two = text
            .lines()
            .find(|l| l.contains("| a (attempt 2) | quiet |"))
            .unwrap();
        assert!(one.contains("100 us ["), "{one}");
        assert!(!one.contains("900"), "{one}");
        assert!(two.contains("900 us ["), "{two}");
        assert!(!two.contains("100 us ["), "{two}");
        assert!(text.contains("| b | quiet |"), "{text}");
        assert!(text.contains("- arm a (attempt 2): label v1"), "{text}");
    }

    /// Catches a samples file from before attempts existed being refused, or its rows put under
    /// an attempt other than the first; and an attempt that is not a number being taken.
    #[test]
    fn a_samples_file_without_an_attempt_column_is_the_first_attempt() {
        let old_header = crate::result::Sample::CSV_HEADER.trim_end_matches(",attempt");
        let text = format!("{old_header}\nr,s,d,cold-put,a,1,quiet,cold,put,10,1,5,100,200\n");
        let rows = parse_samples(&text).unwrap();
        assert!(rows.contains_key(&("cold-put".to_owned(), "a".to_owned(), 1, 1)));
        let header = crate::result::Sample::CSV_HEADER;
        let bad = format!("{header}\nr,s,d,cold-put,a,1,quiet,cold,put,10,1,5,100,200,x\n");
        assert!(
            parse_samples(&bad)
                .err()
                .unwrap()
                .to_string()
                .contains("the attempt is not a number")
        );
    }

    /// Catches a pass cut by the cold store guard going unsaid: the report lists the pass with
    /// the measure and the numbers.
    #[test]
    fn a_pass_cut_by_the_cold_store_guard_is_listed() {
        let mut cut = pass(1, Window::Busy, 40, None);
        cut.cold_guard = Some(crate::result::ColdTrip {
            metric: "error_share".to_owned(),
            value: 0.0625,
            limit: 0.05,
            requests: 32,
            window_secs: 60,
        });
        let text = format_report(&[result("a", vec![cut], &[])], &Rows::new());
        assert!(
            text.contains("- arm a: pass 1 was cut by the cold store guard (cold store guard: error_share is 0.0625, over the limit 0.05 (over 32 requests of the last 60 s))"),
            "{text}"
        );
    }

    /// Catches times printed in the wrong unit.
    #[test]
    fn times_are_us_below_a_millisecond_and_ms_above() {
        assert_eq!(fmt_us(999), "999 us");
        assert_eq!(fmt_us(1000), "1.00 ms");
        assert_eq!(fmt_us(1_234_567), "1234.57 ms");
        assert_eq!(with_ci(5, None), "5 us");
        assert_eq!(with_ci(5, Some((4, 6))), "5 us [4 us, 6 us]");
    }

    /// Catches the directory read wrong: result files found whatever else is beside them
    /// (the ledger, the manifest, text that is not JSON), the cells in the order of the paper, a
    /// result of another schema version refused, a missing directory an error, and a missing
    /// samples file no error.
    #[test]
    fn a_directory_is_read_for_its_results_in_the_order_of_the_cells() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "tb-report");
        let write = |name: &str, text: &str| std::fs::write(dir.path().join(name), text).unwrap();
        let mut g = result("z-arm", vec![pass(1, Window::Quiet, 1, None)], &[]);
        g.cell = CellId::G;
        write("g-z-arm.json", &g.to_json());
        write(
            "cold-put-b.json",
            &result("b", vec![pass(1, Window::Quiet, 1, None)], &[]).to_json(),
        );
        write(
            "cold-put-a.json",
            &result("a", vec![pass(1, Window::Quiet, 1, None)], &[]).to_json(),
        );
        write("ledger.json", "{\"cold_written_bytes\":5}");
        write("g-manifest.json", "{\"seed\":1}");
        write("notes.json", "not json");
        write("requests.jsonl", "{}");
        let text = render(dir.path()).unwrap();
        let at = |needle: &str| text.find(needle).unwrap();
        assert!(at("## cold-put") < at("## g"));
        assert!(at("| a |") < at("| b |"));
        assert!(text.contains("no request succeeded"));

        write(
            "samples.csv",
            &format!("{}\n", crate::result::Sample::CSV_HEADER),
        );
        assert!(render(dir.path()).is_ok());
        write("old.json", "{\"schema_version\": 0}");
        assert!(matches!(render(dir.path()), Err(BenchError::Result(_))));
        assert!(matches!(
            render(&dir.path().join("absent")),
            Err(BenchError::Io { .. })
        ));
    }

    /// Catches a samples file that cannot be read being taken for none, and a result file that
    /// cannot be read being skipped.
    #[test]
    fn an_unreadable_samples_or_result_file_is_an_error() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "tb-report-io");
        std::fs::create_dir(dir.path().join("samples.csv")).unwrap();
        assert!(matches!(render(dir.path()), Err(BenchError::Io { .. })));
        let other = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "tb-report-io2");
        std::fs::create_dir(other.path().join("x.json")).unwrap();
        assert!(matches!(render(other.path()), Err(BenchError::Io { .. })));
    }
}
