//! `kbf cluster finalize` end to end: the real binary against a server that keeps the requests
//! it was sent, so the method, path, headers and body of each call are pinned, and the exit
//! status and the output follow the answers. The same routes served by the real operator API,
//! with real members, are in `kbf-server`'s `tests/api_finalize.rs`.

mod cluster_support;

use cluster_support::{cluster, cluster_each, kbf, token_file};

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";

/// The answer to a `POST` that raised the farm's target to 5.
const POSTED: &str = r#"{"target":5,"written":true}"#;

/// The report of a farm of one node whose log is at the target.
const DONE: &str = r#"{"target": 5, "nodes": [
  {"member": 1, "build_commit": "abc", "runs_log": 5, "log_version": 5,
   "state": "done", "reason": ""}]}"#;

/// The report of a farm of one node whose log is below the target.
const PENDING: &str = r#"{"target": 5, "nodes": [
  {"member": 1, "build_commit": "abc", "runs_log": 5, "log_version": 4,
   "state": "pending", "reason": ""}]}"#;

/// The refusal of a `POST` while a member's build runs less than the target.
const BLOCKED: &str = r#"{"error": "a finalize to 5 is blocked by 1 member(s)", "target": 5,
  "blocked": [{"member": 2, "build_commit": null, "runs_log": 4, "log_version": 4,
   "state": "blocked", "reason": "its build runs log versions up to 4, below 5"}]}"#;

/// The words of each line of `text`.
fn words(text: &str) -> Vec<Vec<&str>> {
    text.lines()
        .map(|line| line.split_whitespace().collect())
        .collect()
}

/// Asserts that `request` is a call of `line` through the whole write gate: the bearer of the
/// token file and the JSON content type.
fn assert_gated(request: &str, line: &str) {
    assert!(request.starts_with(line), "{request}");
    let lower = request.to_ascii_lowercase();
    let bearer = format!("authorization: bearer {TOKEN}");
    assert!(lower.contains(&bearer), "{request}");
    let json = "content-type: application/json";
    assert!(lower.contains(json), "{request}");
}

/// Asserts that `finalize` sends `POST /v1/finalize` through the whole write gate, with
/// `{"target":4}` for `--target 4` and no body without it, and prints the answer as it came,
/// exit 0.
#[test]
fn finalize_posts_its_target_through_the_write_gate() {
    let token = token_file("finalize", TOKEN);
    let with: &[&str] = &["finalize", "--target", "4", "--token-file", &token];
    let (code, out, err, request) = cluster("200 OK", POSTED, with);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, format!("{POSTED}\n"));
    assert_gated(&request, "POST /v1/finalize HTTP/1.1\r\n");
    assert!(request.ends_with("\r\n\r\n{\"target\":4}"), "{request}");

    let without: &[&str] = &["finalize", "--token-file", &token];
    let (code, _, err, request) = cluster("200 OK", POSTED, without);
    assert_eq!(code, Some(0), "{err}");
    assert_gated(&request, "POST /v1/finalize HTTP/1.1\r\n");
    assert!(request.ends_with("\r\n\r\n"), "{request}");
}

/// Asserts that a finalize the frontend refuses exits 1 with nothing on stdout, and says on
/// stderr the status and the members that block it with their reasons, so an operator knows
/// which node to fix.
#[test]
fn a_refused_finalize_exits_1_and_names_the_blocking_member() {
    let token = token_file("finalize-409", TOKEN);
    let args: &[&str] = &["finalize", "--token-file", &token];
    let (code, out, err, _) = cluster("409 Conflict", BLOCKED, args);
    assert_eq!(code, Some(1), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("409"), "{err}");
    assert!(err.contains("versions up to 4"), "{err}");
}

/// Asserts that `finalize --status` sends `GET /v1/finalize` through the whole write gate and
/// prints the farm's target and one line per node, exit 0, without sending a `POST`.
#[test]
fn status_reads_the_report_and_prints_the_table() {
    let token = token_file("finalize-status", TOKEN);
    let args: &[&str] = &["finalize", "--status", "--token-file", &token];
    let (code, out, err, request) = cluster("200 OK", PENDING, args);
    assert_eq!(code, Some(0), "{err}");
    assert_gated(&request, "GET /v1/finalize HTTP/1.1\r\n");
    let lines = words(&out);
    assert_eq!(lines.len(), 3, "{out}");
    assert_eq!(lines[0], ["finalize", "target", "5"]);
    assert_eq!(lines[2], ["1", "abc", "5", "4", "pending"]);
}

/// Asserts that `finalize --wait` asks with a `POST`, prints its answer, then reads
/// `GET /v1/finalize` and, with every node done, prints the table and exits 0.
#[test]
fn wait_asks_then_reads_until_every_node_is_done() {
    let token = token_file("finalize-wait", TOKEN);
    let args: &[&str] = &["finalize", "--wait", "--token-file", &token];
    let answers = vec![("200 OK", POSTED), ("200 OK", DONE)];
    let (code, out, err, requests) = cluster_each(answers, args);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_gated(&requests[0], "POST /v1/finalize HTTP/1.1\r\n");
    assert_gated(&requests[1], "GET /v1/finalize HTTP/1.1\r\n");
    assert!(out.starts_with(POSTED), "{out}");
    let lines = words(&out);
    let last = lines.last().expect("a line");
    assert_eq!(last, &["1", "abc", "5", "5", "done"]);
}

/// Asserts that `finalize --wait --timeout-secs 0` gives up at the first read with a node not
/// done: exit 1, the table of where the node is on stdout, and the timeout on stderr.
#[test]
fn wait_gives_up_after_its_timeout() {
    let token = token_file("finalize-timeout", TOKEN);
    let args: &[&str] = &[
        "finalize",
        "--wait",
        "--timeout-secs",
        "0",
        "--token-file",
        &token,
    ];
    let answers = vec![("200 OK", POSTED), ("200 OK", PENDING)];
    let (code, out, err, requests) = cluster_each(answers, args);
    assert_eq!(code, Some(1), "{err}");
    assert_eq!(requests.len(), 2, "{requests:?}");
    let lines = words(&out);
    let last = lines.last().expect("a line");
    assert_eq!(last, &["1", "abc", "5", "4", "pending"]);
    assert!(err.contains("timed out after 0 s"), "{err}");
}

/// Asserts that a command line that means no single call exits 2: `--status` with `--wait` or
/// `--target` (a read that also asks), `--timeout-secs` without `--wait`, a target that is not
/// a number, a timeout of more than a day, and no token file.
#[test]
fn a_finalize_command_line_that_names_no_single_call_exits_2() {
    let rows: [&[&str]; 6] = [
        &["--status", "--wait", "--token-file", "t"],
        &["--status", "--target", "5", "--token-file", "t"],
        &["--timeout-secs", "5", "--token-file", "t"],
        &["--wait", "--timeout-secs", "86401", "--token-file", "t"],
        &["--target", "five", "--token-file", "t"],
        &["--target", "5"],
    ];
    for flags in rows {
        let mut argv = vec!["cluster", "finalize"];
        argv.extend_from_slice(flags);
        let (code, out, err) = kbf(&argv);
        assert_eq!(code, Some(2), "{argv:?}: {err}");
        assert!(out.is_empty(), "{argv:?}: {out}");
    }
}
