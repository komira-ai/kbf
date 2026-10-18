//! The event-log reader over lines of a real log of the pinned Buck2 (`tests/golden`), and
//! variants of them planted to be each way an action can fail.

use std::io::Write as _;

use serde_json::{Value, json};

use super::*;

/// Four action ends of a build that failed (the farm was not reachable), as Buck2 wrote
/// them, with the header and footer lines shortened and the host name replaced.
const FAILED: &str = include_str!("../tests/golden/buck2-failed-actions.jsonl");

/// Four action ends of a build that succeeded (`//:joined`: a salted source, `sort`, `upper`
/// and `join`, on the farm), as Buck2 wrote them, shortened the same way.
const SUCCEEDED: &str = include_str!("../tests/golden/buck2-succeeded-actions.jsonl");

/// The lines of the golden log of a failed build.
fn golden() -> Vec<Value> {
    FAILED
        .lines()
        .map(|l| serde_json::from_str(l).expect("JSON"))
        .collect()
}

/// The text of `lines`.
fn text(lines: &[Value]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

/// An action end that succeeded: the first of the golden log of a passing build.
fn succeeded() -> Value {
    let line = SUCCEEDED.lines().nth(1).expect("an action");
    serde_json::from_str(line).expect("JSON")
}

fn action_mut(line: &mut Value) -> &mut Value {
    line.pointer_mut("/Event/data/SpanEnd/data/ActionExecution")
        .expect("an action")
}

#[test]
fn the_real_log_of_a_failed_build_counts_every_failed_action() {
    // Catches: a reader that finds no failure in a log where Buck2 recorded four (the shape
    // of `failed`, `error` and the command statuses is the pinned Buck2's).
    assert_eq!(
        count(FAILED),
        Ok(Counts {
            actions: 4,
            failed: 4
        })
    );
}

#[test]
fn the_real_log_of_a_passing_build_counts_no_failure() {
    // Catches: a reader that finds a failure in what Buck2 writes for an action that ran on the
    // farm and succeeded (`failed` false, `error` null, a `Success` status).
    assert_eq!(
        count(SUCCEEDED),
        Ok(Counts {
            actions: 4,
            failed: 0
        })
    );
}

#[test]
fn a_succeeded_action_is_not_a_failure() {
    // Catches: a reader that counts every action as failed, so no build could pass.
    let log = text(&[
        golden().remove(0),
        succeeded(),
        succeeded(),
        golden().pop().expect("footer"),
    ]);
    assert_eq!(
        count(&log),
        Ok(Counts {
            actions: 2,
            failed: 0
        })
    );
}

#[test]
fn each_sign_of_failure_counts_alone() {
    // Catches: a reader that needs all three signs, or reads one: `failed` set, an `error`
    // present, a command whose status is not `Success` (a retry that hid a failure).
    let mut failed = succeeded();
    action_mut(&mut failed)["failed"] = json!(true);
    let mut errored = succeeded();
    action_mut(&mut errored)["error"] = json!({"CommandExecutionError": {}});
    let mut status = succeeded();
    action_mut(&mut status)["commands"] = json!([
        {"status": {"Success": {}}},
        {"status": {"Timeout": {"duration": 1}}}
    ]);
    for (what, line) in [("failed", failed), ("error", errored), ("status", status)] {
        assert_eq!(
            count(&text(&[line])),
            Ok(Counts {
                actions: 1,
                failed: 1
            }),
            "{what}"
        );
    }
}

#[test]
fn an_action_without_commands_is_judged_by_failed_and_error() {
    // Catches: a missing `commands` list read as a failure or a panic (an action answered
    // from the cache may list none).
    let mut line = succeeded();
    action_mut(&mut line)
        .as_object_mut()
        .expect("object")
        .remove("commands");
    assert_eq!(
        count(&text(&[line])),
        Ok(Counts {
            actions: 1,
            failed: 0
        })
    );
}

#[test]
fn only_the_end_of_an_action_counts() {
    // Catches: the start of an action counted as a second action, or the `ActionError`
    // instant (which repeats a failure the end already says) counted as another.
    let mut start = succeeded();
    let data = start
        .pointer("/Event/data/SpanEnd/data")
        .cloned()
        .expect("data");
    start["Event"]["data"] = json!({"SpanStart": {"data": data}});
    let instant =
        json!({"Event": {"data": {"Instant": {"data": {"ActionError": {"failed": true}}}}}});
    let log = text(&[start, instant, succeeded()]);
    assert_eq!(
        count(&log),
        Ok(Counts {
            actions: 1,
            failed: 0
        })
    );
}

#[test]
fn a_log_that_is_not_json_lines_is_an_error_naming_the_line() {
    // Catches: a corrupt or truncated log read as one with no failure.
    let log = format!("{}\nthis is not json\n", succeeded());
    let err = count(&log).expect_err("refused");
    assert!(err.starts_with("line 2: "), "{err}");
    assert_eq!(
        count("[1,2]\n").expect_err("an array"),
        "line 1: not an object"
    );
}

#[test]
fn a_log_with_no_action_in_it_is_an_error() {
    // Catches: fail open on a log whose events this reader no longer recognises, an empty
    // file, or a header and footer alone: a build of salted actions always runs some.
    let header_and_footer = text(&[golden().remove(0), golden().pop().expect("footer")]);
    for log in ["", "\n\n", header_and_footer.as_str()] {
        assert_eq!(
            count(log),
            Err("no action execution in the event log".to_owned())
        );
    }
}

fn file(name: &str) -> std::path::PathBuf {
    crate::scratch("eventlog").join(name)
}

#[test]
fn a_file_is_read_as_gzip_or_as_text() {
    // Catches: the file `--event-log x.json-lines.gz` writes not being unzipped, and the
    // plain `.json-lines` of the stand-in not being read.
    let log = text(&[succeeded(), succeeded(), succeeded()]);
    let gz = file("log.json-lines.gz");
    let mut encoder = flate2::write::GzEncoder::new(
        std::fs::File::create(&gz).expect("file"),
        flate2::Compression::fast(),
    );
    encoder.write_all(log.as_bytes()).expect("write");
    encoder.finish().expect("finish");
    assert_eq!(failed_actions(&gz), Ok(0));
    let plain = file("log.json-lines");
    std::fs::write(&plain, FAILED).expect("file");
    assert_eq!(failed_actions(&plain), Ok(4));
}

#[test]
fn a_file_that_cannot_be_read_is_an_error() {
    // Catches: a missing log (a build that never wrote one) or one that claims to be gzip and
    // is not, or is not text, read as no failure.
    let missing = failed_actions(&file("missing")).expect_err("a missing file");
    assert!(missing.contains("missing"), "{missing}");
    let bad_gzip = file("bad.gz");
    std::fs::write(&bad_gzip, [0x1f, 0x8b, 0x08, 0x00, 1, 2, 3]).expect("file");
    assert!(
        failed_actions(&bad_gzip)
            .expect_err("bad gzip")
            .contains("not a log")
    );
    let binary = file("binary");
    std::fs::write(&binary, [0xff, 0xfe, 0x00]).expect("file");
    assert!(
        failed_actions(&binary)
            .expect_err("not text")
            .contains("not a log")
    );
}
