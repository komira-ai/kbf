//! `fill-paper` as a process: the flags, the exit status and the files.

use std::path::Path;
use std::process::{Command, Output};

use kbf_testkit::dir::TestDir;

const PAPER: &str = "<html><body><p>Put: <b data-cell=\"put\">TBD</b> and <i data-cell=\"gone\">TBD</i></p></body></html>\n";

const MAPPING: &str = r#"[
 {"id":"put","cell":"cold-put","metric":"p50","unit":"ms","condition":"direct"},
 {"id":"gone","cell":"cold-put","metric":"p50","unit":"ms","condition":"direct","blocked":true}
]"#;

const RESULT: &str = r#"{"schema_version":1,"cell":"cold-put","arm":"direct","started_unix_ms":0,"finished_unix_ms":1,
"provenance":{"run_id":"","git_sha":"","binary_digest":"","tool_versions":{},"hardware":{}},
"passes":[{"pass":0,"window":"quiet","discarded":true,"requests":10,"failures":0,"bytes":1,"elapsed_us":1,"percentiles":null,"gate_tripped":null},
{"pass":1,"window":"quiet","discarded":false,"requests":10,"failures":0,"bytes":1,"elapsed_us":1,"percentiles":{"p50_us":2500,"p95_us":3,"p99_us":null,"max_us":4},"gate_tripped":null}],
"notes":[]}"#;

fn setup(name: &str) -> TestDir {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    std::fs::create_dir(dir.path().join("results")).unwrap();
    std::fs::write(dir.path().join("results/cold-put-direct-a1.json"), RESULT).unwrap();
    std::fs::write(dir.path().join("paper.html"), PAPER).unwrap();
    std::fs::write(dir.path().join("cells.json"), MAPPING).unwrap();
    dir
}

/// `fill-paper` with the result table beside the paper.
fn fill_paper(dir: &Path, extra: &[&str]) -> Output {
    let tsv = dir.join("paper-results.tsv");
    let mut args = vec!["--result-tsv", tsv.to_str().unwrap()];
    args.extend_from_slice(extra);
    fill_paper_raw(dir, &args)
}

fn fill_paper_raw(dir: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tierstore-bench"))
        .arg("fill-paper")
        .arg("--paper")
        .arg(dir.join("paper.html"))
        .arg("--cells")
        .arg(dir.join("cells.json"))
        .arg("--result-dir")
        .arg(dir.join("results"))
        .args(extra)
        .output()
        .unwrap()
}

/// Catches a fill that writes the paper in place without being asked, writes nothing to
/// `--out`, or leaves the blocked cell changed.
#[test]
fn fill_writes_out_and_leaves_the_paper_alone() {
    let dir = setup("fill-paper-out");
    let out = dir.path().join("filled.html");
    let run = fill_paper(dir.path(), &["--out", out.to_str().unwrap()]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("paper.html")).unwrap(),
        PAPER
    );
    let filled = std::fs::read_to_string(&out).unwrap();
    assert!(filled.contains(
        "<b data-cell=\"put\" data-source=\"cold-put-direct-a1.json#p50\" data-label=\"farm\" data-result=\"paper-results.tsv\">2.50 ms</b>"
    ), "{filled}");
    assert!(filled.contains("<i data-cell=\"gone\">TBD</i>"), "{filled}");
    let table = std::fs::read_to_string(dir.path().join("paper-results.tsv")).unwrap();
    let rows: Vec<Vec<&str>> = table
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.split('\t').collect())
        .collect();
    assert_eq!(rows.len(), 1, "{table}");
    assert_eq!(rows[0][..4], ["derived", "fill:put:p50", "2.50", "ms"]);
    assert_eq!(rows[0][5], "put");
    assert!(
        rows[0][4].contains("cold-put-direct-a1.json#p50"),
        "{table}"
    );
}

/// Catches a fill that needs no table (the numbers would have nothing behind them), and a name
/// the paper's check would refuse; each exits 2 and writes nothing.
#[test]
fn a_fill_needs_a_result_table_with_a_plain_name() {
    let dir = setup("fill-paper-tsv-flag");
    let run = fill_paper_raw(dir.path(), &["--in-place"]);
    assert_eq!(run.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&run.stderr).contains("--result-tsv"));
    let bad = dir.path().join("Bad Name.tsv");
    let run = fill_paper_raw(
        dir.path(),
        &["--in-place", "--result-tsv", bad.to_str().unwrap()],
    );
    assert_eq!(run.status.code(), Some(2));
    assert!(!bad.exists());
    // A table path that is a directory cannot be read: exit 2, nothing written.
    let as_dir = dir.path().join("dir.tsv");
    std::fs::create_dir(&as_dir).unwrap();
    let run = fill_paper_raw(
        dir.path(),
        &["--in-place", "--result-tsv", as_dir.to_str().unwrap()],
    );
    assert_eq!(run.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&run.stderr).contains("dir.tsv"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("paper.html")).unwrap(),
        PAPER
    );
}

/// Catches a hand-filled cell being rewritten by the process: exit 2 naming the cell and its
/// table, the paper unchanged, no table created.
#[test]
fn a_hand_filled_cell_stops_the_verb() {
    let dir = setup("fill-paper-hand");
    let hand = PAPER.replace(
        "data-cell=\"put\"",
        "data-cell=\"put\" data-result=\"hand.tsv\"",
    );
    std::fs::write(dir.path().join("paper.html"), &hand).unwrap();
    let run = fill_paper(dir.path(), &["--in-place"]);
    assert_eq!(run.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("\"put\"") && stderr.contains("hand.tsv"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("paper.html")).unwrap(),
        hand
    );
    assert!(!dir.path().join("paper-results.tsv").exists());
}

/// Catches `--in-place` that does nothing, and a re-run that changes the file again.
#[test]
fn in_place_rewrites_the_paper_and_a_second_run_changes_nothing() {
    let dir = setup("fill-paper-inplace");
    assert!(fill_paper(dir.path(), &["--in-place"]).status.success());
    let first = std::fs::read(dir.path().join("paper.html")).unwrap();
    assert_ne!(first, PAPER.as_bytes());
    assert!(fill_paper(dir.path(), &["--in-place"]).status.success());
    assert_eq!(std::fs::read(dir.path().join("paper.html")).unwrap(), first);
    let table = std::fs::read(dir.path().join("paper-results.tsv")).unwrap();
    assert!(fill_paper(dir.path(), &["--in-place"]).status.success());
    assert_eq!(
        std::fs::read(dir.path().join("paper-results.tsv")).unwrap(),
        table
    );
    let check = fill_paper(dir.path(), &["--check"]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
}

/// Catches flag combinations that would write where the user did not say: neither target, both
/// targets, and `--check` with a target. Each exits 2 and writes nothing.
#[test]
fn contradictory_flags_are_refused_and_write_nothing() {
    let dir = setup("fill-paper-flags");
    let out = dir.path().join("o.html");
    let o = out.to_str().unwrap();
    for extra in [
        &[][..],
        &["--out", o, "--in-place"],
        &["--check", "--out", o],
    ] {
        let run = fill_paper(dir.path(), extra);
        assert_eq!(run.status.code(), Some(2), "{extra:?}");
    }
    assert!(!out.exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("paper.html")).unwrap(),
        PAPER
    );
}

/// Catches a check that exits 0 when a result has been deleted since the fill: exit 2 and the
/// listing on stderr.
#[test]
fn check_exits_2_and_lists_when_a_source_is_gone() {
    let dir = setup("fill-paper-check");
    assert!(fill_paper(dir.path(), &["--in-place"]).status.success());
    std::fs::remove_file(dir.path().join("results/cold-put-direct-a1.json")).unwrap();
    let run = fill_paper(dir.path(), &["--check"]);
    assert_eq!(run.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("mapping \"put\" has no result"), "{stderr}");
    assert!(stderr.contains("no longer exists"), "{stderr}");
}

/// Catches a check that passes when the table lost the row behind a filled cell.
#[test]
fn check_exits_2_when_the_table_lost_a_row() {
    let dir = setup("fill-paper-check-row");
    assert!(fill_paper(dir.path(), &["--in-place"]).status.success());
    std::fs::write(dir.path().join("paper-results.tsv"), "# emptied\n").unwrap();
    let run = fill_paper(dir.path(), &["--check"]);
    assert_eq!(run.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(stderr.contains("\"put\" has no row naming it"), "{stderr}");
}

/// Catches a result directory read wrongly: other files skipped, a result-shaped file that is not
/// a result refused, a directory that does not exist refused, and the lookup by cell and arm
/// (a mapping for an arm that is not there has no result).
#[test]
fn results_load_from_a_directory() {
    use tierstore_bench::fill::{FillError, Mapping, Results, check};

    let dir = setup("fill-paper-load");
    let results_dir = dir.path().join("results");
    std::fs::write(results_dir.join("before.json"), "not a result").unwrap();
    std::fs::write(results_dir.join("cold-put-note.txt"), "no").unwrap();
    let loaded = Results::load(&results_dir).unwrap();
    let map = |arm: &str, cell: &str| {
        Mapping::parse_all(&format!(
            r#"[{{"id":"a","cell":"{cell}","metric":"p50","unit":"ms","condition":"{arm}"}}]"#
        ))
        .unwrap()
    };
    assert!(
        check("", &map("direct", "cold-put"), &loaded, None)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        check("", &map("direct", "cold-read"), &loaded, None)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        check("", &map("other", "cold-put"), &loaded, None)
            .unwrap()
            .len(),
        1
    );

    std::fs::write(results_dir.join("cold-read-x.json"), "{}").unwrap();
    let err = Results::load(&results_dir).unwrap_err();
    assert!(err.to_string().starts_with("cold-read-x.json: "), "{err}");
    std::fs::remove_file(results_dir.join("cold-read-x.json")).unwrap();
    // A result under another cell's or arm's name would be looked up wrongly: refused.
    std::fs::write(results_dir.join("cold-put-wrong.json"), RESULT).unwrap();
    let err = Results::load(&results_dir).unwrap_err();
    assert!(err.to_string().contains("named for another"), "{err}");
    std::fs::remove_file(results_dir.join("cold-put-wrong.json")).unwrap();
    // The name of a file written before attempts existed does not say its attempt: refused.
    std::fs::write(results_dir.join("cold-put-direct.json"), RESULT).unwrap();
    let err = Results::load(&results_dir).unwrap_err();
    assert!(err.to_string().contains("named for another"), "{err}");
    std::fs::remove_file(results_dir.join("cold-put-direct.json")).unwrap();
    let missing = Results::load(&dir.path().join("none")).unwrap_err();
    assert!(matches!(missing, FillError::Io { .. }));
}

/// Catches an unreadable input or an unwritable target that is swallowed: each exits 2 naming the
/// file, and nothing is created.
#[test]
fn files_that_cannot_be_read_or_written_stop_the_verb() {
    let dir = setup("fill-paper-io");
    let nowhere = dir.path().join("no-such-dir/out.html");
    let run = fill_paper(dir.path(), &["--out", nowhere.to_str().unwrap()]);
    assert_eq!(run.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&run.stderr).contains("no-such-dir"));

    std::fs::remove_file(dir.path().join("paper.html")).unwrap();
    let run = fill_paper(dir.path(), &["--check"]);
    assert_eq!(run.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&run.stderr).contains("paper.html"));
}
