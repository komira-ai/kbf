//! `kbf cluster` end to end: the real binary against a one-shot HTTP server that keeps
//! the request it was sent, so each verb's method, path, headers and body are pinned,
//! and the exit status follows the answer. The same routes are served and called in
//! `kbf-server`'s `tests/frontend_drain.rs`, which runs the client against the real
//! operator API.

mod cluster_support;

use cluster_support::{cluster, kbf, token_file};

/// Catches each verb sending the wrong call: the method, the path (the node id
/// percent-encoded as one segment), the `Authorization` header of the token file on a
/// write and none on a read, the JSON content type and length on a write, and the
/// deadline in the body. The answer's body is printed as it came, exit 0.
#[test]
fn each_verb_sends_its_call_and_prints_the_answer() {
    let token = token_file("each", "kbf-test-token-0123456789abcdef0123456789");
    let bearer = "authorization: bearer kbf-test-token-0123456789abcdef0123456789";
    let cases: [(&[&str], &str, &str); 7] = [
        (&["status"], "GET /v1/nodes HTTP/1.1", ""),
        (
            &["status", "--frontend"],
            "GET /v1/frontend/drain HTTP/1.1",
            "",
        ),
        (
            &["drain", "--frontend", "--token-file", &token],
            "POST /v1/frontend/drain HTTP/1.1",
            "",
        ),
        (
            &[
                "drain",
                "rack 1/a",
                "--deadline-secs",
                "5",
                "--token-file",
                &token,
            ],
            "POST /v1/nodes/rack%201%2Fa:drain HTTP/1.1",
            r#"{"deadline_secs":5}"#,
        ),
        (
            &["undrain", "n1", "--token-file", &token],
            "POST /v1/nodes/n1:uncordon HTTP/1.1",
            "",
        ),
        (
            &["member", "7", "--state", "active", "--token-file", &token],
            "POST /v1/members/7 HTTP/1.1",
            r#"{"state":"active"}"#,
        ),
        (
            &["member", "7", "--state", "draining", "--token-file", &token],
            "POST /v1/members/7 HTTP/1.1",
            r#"{"state":"draining"}"#,
        ),
    ];
    for (args, line, body) in cases {
        let (code, out, err, request) = cluster("200 OK", r#"{"answer":1}"#, args);
        assert_eq!(code, Some(0), "{args:?}: {err}");
        assert_eq!(out, "{\"answer\":1}\n", "{args:?}");
        assert!(
            request.starts_with(&format!("{line}\r\n")),
            "{args:?}: {request}"
        );
        let lower = request.to_ascii_lowercase();
        let writes = line.starts_with("POST");
        assert_eq!(lower.contains(bearer), writes, "{args:?}: {request}");
        assert_eq!(
            lower.contains("content-type: application/json"),
            writes,
            "{args:?}: {request}"
        );
        assert!(
            request.ends_with(&format!("\r\n\r\n{body}")),
            "{args:?}: {request}"
        );
        if writes {
            assert!(
                lower.contains(&format!("content-length: {}\r\n", body.len())),
                "{args:?}: {request}"
            );
        }
    }
}

/// Catches a refused or failed call that exits 0 (a rollout would go on), and one that
/// hides why: a 401 from the API exits 1 naming the status and the API's reason, and an
/// address nothing listens on exits 1 naming the address.
#[test]
fn a_refusal_or_no_answer_exits_1_and_says_why() {
    let token = token_file("refused", "wrong-token-wrong-token-wrong-token-1");
    let (code, out, err, _) = cluster(
        "401 Unauthorized",
        r#"{"error":"a write needs Authorization"}"#,
        &["drain", "--frontend", "--token-file", &token],
    );
    assert_eq!(code, Some(1), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(
        err.contains("401") && err.contains("a write needs Authorization"),
        "{err}"
    );

    let gone = kbf_testkit::port::dead_addr().to_string();
    let (code, out, err) = kbf(&["cluster", "--api", &gone, "status"]);
    assert_eq!(code, Some(1), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains(&gone), "{err}");

    let (code, _, err) = kbf(&[
        "cluster",
        "drain",
        "--frontend",
        "--token-file",
        "/nonexistent/token",
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("/nonexistent/token"), "{err}");
}

/// Catches a command line that is accepted and means something else: `drain` with no
/// target or with both, a deadline for the frontend (it has none), `undrain --frontend`
/// (a frontend drain is not undone), a write with no token file, and an unknown verb,
/// all exit 2 with clap's message, and `check-args` does not take `cluster` as a mode.
#[test]
fn a_command_line_that_names_no_single_call_exits_2() {
    let rows: [&[&str]; 7] = [
        &["cluster", "drain", "--token-file", "t"],
        &["cluster", "drain", "n1", "--frontend", "--token-file", "t"],
        &[
            "cluster",
            "drain",
            "--frontend",
            "--deadline-secs",
            "5",
            "--token-file",
            "t",
        ],
        &["cluster", "undrain", "--frontend", "--token-file", "t"],
        &["cluster", "drain", "n1"],
        &["cluster", "undrain", "n1"],
        &["cluster", "restart"],
    ];
    for argv in rows {
        let (code, out, err) = kbf(argv);
        assert_eq!(code, Some(2), "{argv:?}: {err}");
        assert!(out.is_empty(), "{argv:?}: {out}");
    }
    let (code, _, err) = kbf(&["check-args", "cluster", "status"]);
    assert_eq!(code, Some(2), "{err}");
}

/// Catches `cluster` missing from `kbf --help` (nothing else would tell an operator it
/// exists) while the mode list stays the five modes.
#[test]
fn help_names_cluster_after_the_modes() {
    let (code, out, err) = kbf(&["--help"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(
        out.contains("kbf cluster status|drain|undrain|member|denied|object-store"),
        "{out}"
    );
}

/// Catches the `kbf cluster --help` about line and the "Not a mode" hint naming fewer
/// verbs than the command has: each verb group must be named in both.
#[test]
fn the_cluster_help_and_the_hint_name_every_verb_group() {
    let (code, out, err) = kbf(&["cluster", "--help"]);
    assert_eq!(code, Some(0), "{err}");
    let about = out.lines().next().unwrap_or_default();
    for word in ["member", "denied", "object store"] {
        assert!(
            about.contains(word),
            "{word} missing from the about line: {out}"
        );
    }
    let (_, top, _) = kbf(&["--help"]);
    for word in ["member", "denied", "object-store"] {
        assert!(top.contains(word), "{word} missing from the hint: {top}");
    }
}
