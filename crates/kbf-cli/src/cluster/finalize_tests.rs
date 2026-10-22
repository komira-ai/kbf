//! The table, the `--status` print and the wait of `kbf cluster finalize`, over reports spelled
//! as the frontend sends them. The verb against a server that keeps what it was sent is in
//! `tests/cluster_finalize.rs`.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::net::SocketAddr;

use kbf_server::api::FINALIZE;

use super::*;
use crate::cluster::{Body, Call, Cluster, Verb};

/// `kbf cluster finalize` with `flags`, parsed.
fn parsed(flags: &[&str]) -> Result<Cluster, clap::Error> {
    let mut words: Vec<OsString> = vec!["kbf".into(), "cluster".into(), "finalize".into()];
    words.extend(flags.iter().map(OsString::from));
    crate::parse_cluster(&words)
}

/// The call the verb of `flags` makes.
fn call_of(flags: &[&str]) -> Call {
    let cluster = parsed(flags).expect("flags");
    cluster.verb.call(&mut std::io::empty()).expect("a call")
}

/// A report of the farm's target 5 with one member, 1, whose log is at version `at`, in the
/// state `state` names.
fn one(at: u32, state: &str) -> String {
    let value = serde_json::json!({
        "target": 5,
        "nodes": [{
            "member": 1,
            "build_commit": null,
            "runs_log": 5,
            "log_version": at,
            "state": state,
            "reason": "",
        }],
    });
    value.to_string()
}

fn reply(status: u16, body: &str) -> Result<Reply, ClientError> {
    Ok(Reply {
        status,
        body: body.to_owned(),
    })
}

/// What `wait` returned and wrote for the reads `answers`, and how many reads it made. A read
/// beyond the answers fails the test.
fn waited(
    answers: Vec<Result<Reply, ClientError>>,
    timeout: Duration,
) -> (u8, String, String, usize) {
    let mut answers = VecDeque::from(answers);
    let mut reads = 0;
    let mut fetch = || {
        reads += 1;
        answers.pop_front().expect("a read that was answered")
    };
    let every = Duration::from_millis(1);
    let polling = Polling { timeout, every };
    let (mut out, mut err) = (Vec::<u8>::new(), Vec::<u8>::new());
    let code = wait(&mut fetch, &polling, &mut out, &mut err);
    let out = String::from_utf8(out).expect("UTF-8");
    let err = String::from_utf8(err).expect("UTF-8");
    (code, out, err, reads)
}

/// The words of each line of `text`.
fn words(text: &str) -> Vec<Vec<&str>> {
    text.lines()
        .map(|line| line.split_whitespace().collect())
        .collect()
}

/// Asserts that the table names the target, prints a header and one line per member in the
/// order of the report, shortens a commit to its first 12 characters, writes `-` for what a
/// node did not say, gives the reason of a member that blocks, starts the state at one column
/// in every line, and ends no line with a space.
#[test]
fn the_table_has_a_line_per_member_with_its_facts() {
    let value = serde_json::json!({
        "target": 5,
        "nodes": [
            {
                "member": 1,
                "build_commit": "9f3c1d2e0a4b5566778899aabbccddeeff001122",
                "runs_log": 5,
                "log_version": 5,
                "state": "done",
                "reason": "",
            },
            {
                "member": 3,
                "build_commit": null,
                "runs_log": null,
                "log_version": null,
                "state": "blocked",
                "reason": "it has no live row",
            },
            {
                "member": 4,
                "build_commit": "abc",
                "runs_log": null,
                "log_version": null,
                "state": "unknown",
                "reason": "reports nothing",
            },
            {
                "member": 12,
                "build_commit": null,
                "runs_log": 5,
                "log_version": 4,
                "state": "pending",
                "reason": "",
            },
        ],
    });
    let view: FinalizeView = serde_json::from_value(value).expect("a report");
    let text = table(&view);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 6, "{text}");
    assert_eq!(lines[0], "finalize target 5");
    let columns = words(&text);
    let header = ["MEMBER", "BUILD", "RUNS", "LOG", "STATE", "REASON"];
    assert_eq!(columns[1], header);
    assert_eq!(columns[2], ["1", "9f3c1d2e0a4b", "5", "5", "done"]);
    assert_eq!(columns[3][..5], ["3", "-", "-", "-", "blocked"]);
    assert!(lines[3].ends_with("it has no live row"), "{}", lines[3]);
    assert_eq!(columns[4][..5], ["4", "abc", "-", "-", "unknown"]);
    assert!(lines[4].ends_with("reports nothing"), "{}", lines[4]);
    assert_eq!(columns[5], ["12", "-", "5", "4", "pending"]);
    let state_at = lines[1].find("STATE").expect("a STATE column");
    let states = ["done", "blocked", "unknown", "pending"];
    for (line, state) in lines[2..].iter().zip(states) {
        assert_eq!(line.find(state), Some(state_at), "{line}");
        assert!(!line.ends_with(' '), "{line:?}");
    }
}

/// Asserts that a farm with no target says so in the first line, and a farm with no member
/// prints the header alone.
#[test]
fn a_report_with_no_target_and_no_member_prints_a_line_and_a_header() {
    let view = parse(r#"{"target":null,"nodes":[]}"#).expect("a report");
    let text = table(&view);
    assert_eq!(text.lines().next(), Some("no finalize target is set"));
    assert_eq!(words(&text).len(), 2, "{text}");
}

/// Asserts that `--status` prints the table of the answer and exits 0, and prints nothing on
/// stdout and says on stderr, exit 1, when the answer is no report.
#[test]
fn status_prints_the_table_or_says_the_answer_is_no_report() {
    let (mut out, mut err) = (Vec::<u8>::new(), Vec::<u8>::new());
    let body = one(4, "pending");
    let shown = Reply { status: 200, body };
    assert_eq!(show(&shown, &mut out, &mut err), 0);
    let text = String::from_utf8(out).expect("UTF-8");
    assert_eq!(words(&text)[2], ["1", "-", "5", "4", "pending"], "{text}");
    assert!(err.is_empty());

    let (mut out, mut err) = (Vec::<u8>::new(), Vec::<u8>::new());
    let odd = Reply {
        status: 200,
        body: r#"{"answer":1}"#.to_owned(),
    };
    assert_eq!(show(&odd, &mut out, &mut err), 1);
    assert!(out.is_empty());
    let said = String::from_utf8(err).expect("UTF-8");
    assert!(said.contains("not a finalize report"), "{said}");
}

/// Asserts that `--wait` returns 0 at the first read that has every node done, printing the
/// table and nothing on stderr, and that it reads again, after the wait between reads, while a
/// node is still pending.
#[test]
fn the_wait_ends_when_every_node_is_done() {
    let hour = Duration::from_secs(3600);
    let done = vec![reply(200, &one(5, "done"))];
    let (code, out, err, reads) = waited(done, hour);
    assert_eq!((code, reads), (0, 1));
    assert_eq!(words(&out)[2], ["1", "-", "5", "5", "done"], "{out}");
    assert!(err.is_empty(), "{err}");

    let pending = reply(200, &one(4, "pending"));
    let later = vec![pending, reply(200, &one(5, "done"))];
    let (code, out, err, reads) = waited(later, hour);
    assert_eq!((code, reads), (0, 2));
    assert_eq!(words(&out)[2], ["1", "-", "5", "5", "done"], "{out}");
    assert!(err.is_empty(), "{err}");
}

/// Asserts that `--wait` gives up when its timeout has passed with a node not done: exit 1,
/// the table of where the nodes were on stdout, and the timeout said on stderr. A timeout of 0
/// reads once.
#[test]
fn the_wait_times_out_with_the_table_of_where_the_nodes_were() {
    let pending = vec![reply(200, &one(4, "pending"))];
    let (code, out, err, reads) = waited(pending, Duration::ZERO);
    assert_eq!((code, reads), (1, 1));
    assert_eq!(words(&out)[2], ["1", "-", "5", "4", "pending"], "{out}");
    assert!(err.contains("timed out after 0 s"), "{err}");
}

/// Asserts that `--wait` does not wait out a read that fails, an answer with a status that is
/// not 2xx, or an answer that is no report: each is exit 1 at once, with nothing on stdout and
/// the reason on stderr (the address that failed, the status and the body, or that the answer
/// is no report).
#[test]
fn the_wait_stops_at_a_read_that_fails() {
    let addr = SocketAddr::from(([127, 0, 0, 1], 9));
    let refused = Err(ClientError::Malformed {
        addr,
        why: "not http",
    });
    let (code, out, err, reads) = waited(vec![refused], Duration::from_secs(3600));
    assert_eq!((code, reads), (1, 1));
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("127.0.0.1:9"), "{err}");

    let down = reply(503, r#"{"error":"the store is down"}"#);
    let (code, out, err, reads) = waited(vec![down], Duration::from_secs(3600));
    assert_eq!((code, reads), (1, 1));
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("503"), "{err}");
    assert!(err.contains("the store is down"), "{err}");

    let odd = reply(200, "<html>a proxy's page</html>");
    let (code, out, err, reads) = waited(vec![odd], Duration::from_secs(3600));
    assert_eq!((code, reads), (1, 1));
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("not a finalize report"), "{err}");
}

/// Asserts that the flags of `finalize` make the call they mean: no flag but the token file
/// is a `POST` with no body, `--target 4` a `POST` of `{"target":4}`, `--wait` and its timeout
/// are kept by the verb and ask the same `POST`, and `--status` is the gated `GET`; the verb
/// carries the token file it was given.
#[test]
fn the_flags_make_the_call_they_mean() {
    let post = |body| Call::Post(FINALIZE.to_owned(), body);
    assert_eq!(call_of(&["--token-file", "t"]), post(Body::empty()));
    let four = Body::json(&serde_json::json!({ "target": 4 }));
    let targeted = ["--target", "4", "--token-file", "t"];
    assert_eq!(call_of(&targeted), post(four));
    let waiting = ["--wait", "--timeout-secs", "30", "--token-file", "t"];
    assert_eq!(call_of(&waiting), post(Body::empty()));
    let cluster = parsed(&waiting).expect("flags");
    let Verb::Finalize {
        wait,
        timeout_secs,
        token_file,
        ..
    } = &cluster.verb
    else {
        panic!("not a finalize verb");
    };
    assert_eq!((*wait, *timeout_secs), (true, Some(30)));
    assert_eq!(token_file.to_str(), Some("t"));
    assert_eq!(cluster.verb.token_file(), Some(token_file));
    let status = Call::GetGated(FINALIZE.to_owned());
    assert_eq!(call_of(&["--status", "--token-file", "t"]), status);
}

/// Asserts that flags that mean no single call are refused by the parser: `--status` with
/// `--wait` or with `--target`, a timeout without `--wait`, a target that is not a number and
/// a command line with no token file.
#[test]
fn flags_that_name_no_single_call_are_refused() {
    let rows: [&[&str]; 5] = [
        &["--status", "--wait", "--token-file", "t"],
        &["--status", "--target", "5", "--token-file", "t"],
        &["--timeout-secs", "5", "--token-file", "t"],
        &["--target", "five", "--token-file", "t"],
        &["--target", "5"],
    ];
    for flags in rows {
        let refused = parsed(flags).is_err();
        assert!(refused, "{flags:?} was accepted");
    }
}
