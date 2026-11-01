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
    let cases: [(&[&str], &str, &str); 12] = [
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
        // No `--deadline-secs`: an empty body, which the server reads as a drain with no
        // deadline (it waits for the leases however long they run).
        (
            &["drain", "n1", "--token-file", &token],
            "POST /v1/nodes/n1:drain HTTP/1.1",
            "",
        ),
        (
            &[
                "drain",
                "n1",
                "--deadline-secs",
                "0",
                "--token-file",
                &token,
            ],
            "POST /v1/nodes/n1:drain HTTP/1.1",
            r#"{"deadline_secs":0}"#,
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
        (
            &[
                "member",
                "7",
                "--state",
                "planned-down",
                "--token-file",
                &token,
            ],
            "POST /v1/members/7 HTTP/1.1",
            r#"{"state":"planned_down"}"#,
        ),
        (
            &[
                "member",
                "7",
                "--state",
                "planned-down",
                "--for-secs",
                "600",
                "--token-file",
                &token,
            ],
            "POST /v1/members/7 HTTP/1.1",
            r#"{"for_secs":600,"state":"planned_down"}"#,
        ),
        (
            &["epoch-bump", "--token-file", &token],
            "POST /v1/epoch/bump HTTP/1.1",
            "",
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

/// Catches: `safe-to-stop` sending the wrong call (a `GET` of the member's path with the id
/// percent-encoded as one segment, behind the token and the JSON content type), and an exit
/// status that does not follow the verdict: 0 and "safe to stop" when it is safe, 3 and the
/// reasons on stdout when it is not (never 0, which would stop the host; never 1, which a
/// script reads as a failed call), nothing on stderr for either. It catches: the
/// exit status inverted; the reasons not printed.
#[test]
fn safe_to_stop_exits_0_when_safe_and_3_when_not() {
    let token = token_file("stop", "kbf-test-token-0123456789abcdef0123456789");
    let args: &[&str] = &["safe-to-stop", "7", "--token-file", &token];
    let safe = r#"{"safe":true,"reasons":[],"planned_down":false}"#;
    let (code, out, err, request) = cluster("200 OK", safe, args);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "safe to stop 7\n");
    assert!(err.is_empty(), "{err}");
    let head = "GET /v1/members/7/safe-to-stop HTTP/1.1\r\n";
    assert!(request.starts_with(head), "{request}");
    let lower = request.to_ascii_lowercase();
    assert!(lower.contains("authorization: bearer kbf-test-token-"));
    assert!(lower.contains("content-type: application/json"));

    let owing = r#"{"safe":false,"reasons":["member m2 owes 5 blobs"],"planned_down":false}"#;
    let (code, out, err, _) = cluster("200 OK", owing, args);
    assert_eq!(code, Some(3), "{err}");
    assert_eq!(out, "not safe to stop 7\n- member m2 owes 5 blobs\n");
    assert!(err.is_empty(), "{err}");

    let odd: &[&str] = &["safe-to-stop", "rack 1/a", "--token-file", &token];
    let (_, _, _, request) = cluster("200 OK", safe, odd);
    let head = "GET /v1/members/rack%201%2Fa/safe-to-stop HTTP/1.1\r\n";
    assert!(request.starts_with(head), "{request}");
}

/// Catches: `safe-to-stop` reading anything but a verdict as an answer: an id the server
/// does not know (404) and a 2xx body that is no verdict (a proxy's page) both exit 1 with
/// nothing on stdout and the reason on stderr, so a script that stops hosts on exit 0 and
/// waits on exit 3 never mistakes either for a yes or a "not yet".
#[test]
fn safe_to_stop_without_a_verdict_exits_1_and_says_why() {
    let token = token_file("nosay", "kbf-test-token-0123456789abcdef0123456789");
    let args: &[&str] = &["safe-to-stop", "9", "--token-file", &token];
    let missing = r#"{"error":"no member or node"}"#;
    let (code, out, err, _) = cluster("404 Not Found", missing, args);
    assert_eq!(code, Some(1), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("404"), "{err}");
    assert!(err.contains("no member or node"), "{err}");

    let (code, out, err, _) = cluster("200 OK", r#"{"answer":1}"#, args);
    assert_eq!(code, Some(1), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("not a safe-to-stop verdict"), "{err}");
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
/// (a frontend drain is not undone), a write with no token file, `safe-to-stop` with no id
/// or no token file, `epoch-bump` with no token file, and an unknown verb, all exit 2 with
/// clap's message.
#[test]
fn a_command_line_that_names_no_single_call_exits_2() {
    let rows: [&[&str]; 10] = [
        &["cluster", "safe-to-stop", "--token-file", "t"],
        &["cluster", "safe-to-stop", "7"],
        &["cluster", "epoch-bump"],
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
}

/// Catches `cluster` missing from `kbf --help` (nothing else would tell an operator it
/// exists) while the mode list stays the five modes.
#[test]
fn help_names_cluster_after_the_modes() {
    let (code, out, err) = kbf(&["--help"]);
    assert_eq!(code, Some(0), "{err}");
    let verbs =
        "kbf cluster status|drain|undrain|member|safe-to-stop|denied|epoch-bump|object-store";
    assert!(out.contains(verbs), "{out}");
}

/// Catches the `kbf cluster --help` about line and the "Not a mode" hint naming fewer
/// verbs than the command has: each verb group must be named in both.
#[test]
fn the_cluster_help_and_the_hint_name_every_verb_group() {
    let (code, out, err) = kbf(&["cluster", "--help"]);
    assert_eq!(code, Some(0), "{err}");
    let about = out.lines().next().unwrap_or_default();
    for word in ["member", "be stopped", "denied", "epoch", "object store"] {
        assert!(
            about.contains(word),
            "{word} missing from the about line: {out}"
        );
    }
    let (_, top, _) = kbf(&["--help"]);
    for word in [
        "member",
        "safe-to-stop",
        "denied",
        "epoch-bump",
        "object-store",
    ] {
        assert!(top.contains(word), "{word} missing from the hint: {top}");
    }
}

/// Catches `kbf cluster drain --help` still naming a default deadline: without
/// `--deadline-secs` the drain has none.
#[test]
fn the_drain_help_says_no_deadline_is_the_default() {
    let (code, out, err) = kbf(&["cluster", "drain", "--help"]);
    assert_eq!(code, Some(0), "{err}");
    let text = out.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(text.contains("Not given: no deadline"), "{out}");
    assert!(!text.contains("30 minutes"), "{out}");
}
