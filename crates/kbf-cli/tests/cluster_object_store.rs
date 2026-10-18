//! `kbf cluster object-store ...` and `kbf cluster denied` end to end: the real binary
//! against a one-shot HTTP server that keeps the request it was sent, so each verb's
//! method, path, headers and body are pinned. The routes themselves are tested in
//! `kbf-server` (`tests/object_store_api`, `tests/operator_verbs.rs`); these tests pin
//! what the CLI sends, and that the secret key reaches the server and nothing else: not
//! the command line, not the output, not an error.

mod cluster_support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use cluster_support::{cluster, cluster_stdin, kbf, token_file};

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";

/// A string no other test or message holds, as the secret key.
const SENTINEL: &str = "SENTINEL-secret-0f3a9c71d2e84b65a1c0";

/// A file holding `content` with `mode`, under the test directory.
fn file(name: &str, content: &str, mode: u32) -> String {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kbf-cluster-os");
    std::fs::create_dir_all(&dir).expect("a directory");
    let path = dir.join(format!("{name}-{}", std::process::id()));
    std::fs::write(&path, content).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path.display().to_string()
}

/// The JSON of a request's body, without its `null` members: the shared DTOs
/// (`BindingPut`) spell an option not given as `null`, which the server reads as absent.
fn json_of(request: &str) -> serde_json::Value {
    let (_, body) = request.split_once("\r\n\r\n").expect("a request head");
    let mut json: serde_json::Value =
        serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body:?}"));
    if let Some(members) = json.as_object_mut() {
        members.retain(|_, v| !v.is_null());
    }
    json
}

/// Catches each read verb sending the wrong call, or sending it ungated. Every object
/// store route passes the whole write gate, a GET included, so even a read carries the
/// bearer of the token file and the JSON content type (a plain `GET` would be 401 or
/// 415 on a real frontend): the method, the path, the query of `audit`, and the answer's
/// body printed as it came.
#[test]
fn each_read_verb_sends_a_gated_get_and_prints_the_answer() {
    let token = token_file("reads", TOKEN);
    let cases: [(&[&str], &str); 6] = [
        (
            &["object-store", "status", "--token-file", &token],
            "GET /v1/object-stores HTTP/1.1",
        ),
        (
            &["object-store", "audit", "--token-file", &token],
            "GET /v1/object-stores/audit HTTP/1.1",
        ),
        (
            &["object-store", "audit", "--limit", "7", "--token-file", &token],
            "GET /v1/object-stores/audit?limit=7 HTTP/1.1",
        ),
        (
            &["object-store", "connection", "list", "--token-file", &token],
            "GET /v1/object-stores/connections HTTP/1.1",
        ),
        (
            &[
                "object-store",
                "connection",
                "show",
                "main-1",
                "--token-file",
                &token,
            ],
            "GET /v1/object-stores/connections/main-1 HTTP/1.1",
        ),
        (
            &["denied", "--token-file", &token],
            "GET /v1/denied HTTP/1.1",
        ),
    ];
    for (args, line) in cases {
        let (code, out, err, request) = cluster("200 OK", r#"{"answer":1}"#, args);
        assert_eq!(code, Some(0), "{args:?}: {err}");
        assert_eq!(out, "{\"answer\":1}\n", "{args:?}");
        assert!(
            request.starts_with(&format!("{line}\r\n")),
            "{args:?}: {request}"
        );
        let lower = request.to_ascii_lowercase();
        assert!(
            lower.contains(&format!("authorization: bearer {TOKEN}\r\n")),
            "{args:?}: {request}"
        );
        assert!(
            lower.contains("content-type: application/json\r\n"),
            "{args:?}: {request}"
        );
        assert!(request.ends_with("\r\n\r\n"), "{args:?}: {request}");
    }
}

/// Catches each write verb sending the wrong method, path or body: `PUT` of a
/// connection with exactly the fields given (an absent option is absent, not `null`
/// and not a default the server would then take for the operator's choice), `DELETE`
/// with no body, `PUT` and `DELETE` of the `cold` binding, and `POST` of
/// `:verify` with the body the options make. The secret key is the file's content,
/// without its final newline.
#[test]
fn each_write_verb_sends_its_method_path_and_body() {
    let token = token_file("writes", TOKEN);
    let secret = file("secret", &format!("{SENTINEL}\n"), 0o600);
    let put_minimal: &[&str] = &[
        "object-store",
        "connection",
        "put",
        "main",
        "--endpoint",
        "http://127.0.0.1:9000",
        "--access-key-id",
        "AKID",
        "--secret-file",
        &secret,
        "--token-file",
        &token,
    ];
    let put_full: &[&str] = &[
        "object-store",
        "connection",
        "put",
        "main",
        "--endpoint",
        "http://127.0.0.1:9000",
        "--access-key-id",
        "AKID",
        "--secret-file",
        &secret,
        "--region",
        "us-east-1",
        "--conditional-put",
        "--min-put-rate",
        "4096",
        "--expect-generation",
        "3",
        "--no-verify",
        "--token-file",
        &token,
    ];
    let cases: [(&[&str], &str, serde_json::Value); 8] = [
        (
            put_minimal,
            "PUT /v1/object-stores/connections/main HTTP/1.1",
            serde_json::json!({
                "endpoint": "http://127.0.0.1:9000",
                "access_key_id": "AKID",
                "secret_access_key": SENTINEL,
            }),
        ),
        (
            put_full,
            "PUT /v1/object-stores/connections/main HTTP/1.1",
            serde_json::json!({
                "endpoint": "http://127.0.0.1:9000",
                "access_key_id": "AKID",
                "secret_access_key": SENTINEL,
                "region": "us-east-1",
                "conditional_put": true,
                "min_put_rate": 4096,
                "expect_generation": 3,
                "verify": false,
            }),
        ),
        (
            &["object-store", "connection", "delete", "main", "--token-file", &token],
            "DELETE /v1/object-stores/connections/main HTTP/1.1",
            serde_json::Value::Null,
        ),
        (
            &[
                "object-store",
                "bind",
                "--connection",
                "main",
                "--bucket",
                "farm-cold",
                "--token-file",
                &token,
            ],
            "PUT /v1/object-stores/bindings/cold HTTP/1.1",
            serde_json::json!({"connection": "main", "bucket": "farm-cold", "prefix": ""}),
        ),
        (
            &[
                "object-store",
                "bind",
                "--role",
                "cold",
                "--connection",
                "main",
                "--bucket",
                "farm-cold",
                "--prefix",
                "cold/",
                "--expect-generation",
                "9",
                "--no-verify",
                "--token-file",
                &token,
            ],
            "PUT /v1/object-stores/bindings/cold HTTP/1.1",
            serde_json::json!({
                "connection": "main",
                "bucket": "farm-cold",
                "prefix": "cold/",
                "expect_generation": 9,
                "verify": false,
            }),
        ),
        (
            &["object-store", "unbind", "--token-file", &token],
            "DELETE /v1/object-stores/bindings/cold HTTP/1.1",
            serde_json::Value::Null,
        ),
        (
            &["object-store", "verify", "main", "--token-file", &token],
            "POST /v1/object-stores/connections/main:verify HTTP/1.1",
            serde_json::json!({}),
        ),
        (
            &[
                "object-store",
                "verify",
                "main",
                "--bucket",
                "b",
                "--prefix",
                "p/",
                "--token-file",
                &token,
            ],
            "POST /v1/object-stores/connections/main:verify HTTP/1.1",
            serde_json::json!({"bucket": "b", "prefix": "p/"}),
        ),
    ];
    for (args, line, body) in cases {
        let (code, out, err, request) = cluster("200 OK", r#"{"ok":true}"#, args);
        assert_eq!(code, Some(0), "{args:?}: {err}");
        assert_eq!(out, "{\"ok\":true}\n", "{args:?}");
        assert!(
            request.starts_with(&format!("{line}\r\n")),
            "{args:?}: {request}"
        );
        let lower = request.to_ascii_lowercase();
        assert!(
            lower.contains(&format!("authorization: bearer {TOKEN}\r\n")),
            "{args:?}: {request}"
        );
        assert!(
            lower.contains("content-type: application/json\r\n"),
            "{args:?}: {request}"
        );
        let (_, sent) = request.split_once("\r\n\r\n").expect("a head");
        if line.starts_with("PUT /v1/object-stores/connections") {
            assert!(!sent.contains("null"), "an absent option is absent: {sent}");
        }
        if body.is_null() {
            assert_eq!(sent, "", "{args:?}");
        } else if body == serde_json::json!({}) {
            // `:verify` with no options sends nothing or `{}`: the server takes both.
            assert!(sent.is_empty() || sent == "{}", "{args:?}: {sent:?}");
        } else {
            assert_eq!(json_of(&request), body, "{args:?}");
        }
    }
}

/// Catches the secret taken from the wrong place or in the wrong shape: standard input
/// is read to its end and only the final line ending is cut (`\n` or `\r\n`: the
/// key would never match with it), the file gives the same, and a key with an inner
/// space is not trimmed into another key.
#[test]
fn the_secret_comes_from_stdin_or_a_private_file_without_its_line_ending() {
    let token = token_file("secret-src", TOKEN);
    let put = |source: &[&str], stdin: &str| {
        let mut args = vec![
            "object-store",
            "connection",
            "put",
            "main",
            "--endpoint",
            "http://127.0.0.1:9000",
            "--access-key-id",
            "AKID",
            "--token-file",
            &token,
        ];
        args.extend_from_slice(source);
        let (code, _, err, request) = cluster_stdin("200 OK", "{}", &args, stdin);
        assert_eq!(code, Some(0), "{source:?}: {err}");
        json_of(&request)["secret_access_key"]
            .as_str()
            .expect("a string")
            .to_owned()
    };
    assert_eq!(
        put(&["--secret-stdin"], &format!("{SENTINEL}\n")),
        SENTINEL
    );
    assert_eq!(
        put(&["--secret-stdin"], &format!("{SENTINEL}\r\n")),
        SENTINEL
    );
    assert_eq!(put(&["--secret-stdin"], SENTINEL), SENTINEL);
    assert_eq!(put(&["--secret-stdin"], "a b\n\n"), "a b\n");
    let f = file("crlf", &format!("{SENTINEL}\r\n"), 0o400);
    assert_eq!(put(&["--secret-file", &f], ""), SENTINEL);
}

/// Catches a secret file anyone but its owner can read being used (the mutant: the
/// mode check skipped), and a refusal that calls the server anyway or names no file.
/// The address is one nothing listens at: a refusal that went on to call it would say
/// so, naming the address instead of the file.
#[test]
fn a_secret_file_others_can_read_is_refused_before_any_call() {
    let token = token_file("secret-mode", TOKEN);
    let gone = kbf_testkit::port::dead_addr().to_string();
    for mode in [0o640, 0o604, 0o644, 0o660, 0o666, 0o601] {
        let path = file(&format!("wide-{mode:o}"), &format!("{SENTINEL}\n"), mode);
        let (code, out, err) = kbf(&[
            "cluster",
            "--api",
            &gone,
            "object-store",
            "connection",
            "put",
            "main",
            "--endpoint",
            "http://127.0.0.1:9000",
            "--access-key-id",
            "AKID",
            "--secret-file",
            &path,
            "--token-file",
            &token,
        ]);
        assert_eq!(code, Some(1), "{mode:o}: {err}");
        assert!(out.is_empty(), "{mode:o}: {out}");
        assert!(err.contains(&path), "{mode:o}: {err}");
        assert!(
            !err.contains(&gone),
            "{mode:o}: the server was called: {err}"
        );
        assert!(!err.contains(SENTINEL), "{mode:o}: {err}");
    }
}

/// Catches a secret that is nothing (an empty file, a lone newline, an empty stdin)
/// being sent as an empty key, a missing file or a directory read as a key, and a
/// refusal that does not say which source it was. Nothing is sent: the address is dead
/// and the message names the source, not the address.
#[test]
fn a_secret_that_is_missing_or_empty_is_refused_naming_its_source() {
    let token = token_file("secret-none", TOKEN);
    let gone = kbf_testkit::port::dead_addr().to_string();
    let empty = file("empty", "", 0o600);
    let newline = file("newline", "\n", 0o600);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).display().to_string();
    let missing = format!("{dir}/kbf-cluster-os/no-such-file");
    for (source, stdin, names) in [
        (vec!["--secret-file", &empty], "", empty.as_str()),
        (vec!["--secret-file", &newline], "", newline.as_str()),
        (vec!["--secret-file", &missing], "", missing.as_str()),
        (vec!["--secret-file", &dir], "", dir.as_str()),
        (vec!["--secret-stdin"], "", "standard input"),
        (vec!["--secret-stdin"], "\n", "standard input"),
    ] {
        let mut argv = vec![
            "cluster",
            "--api",
            &gone,
            "object-store",
            "connection",
            "put",
            "main",
            "--endpoint",
            "http://127.0.0.1:9000",
            "--access-key-id",
            "AKID",
            "--token-file",
            &token,
        ];
        argv.extend_from_slice(&source);
        let (code, out, err) = cluster_support::kbf_stdin(&argv, stdin);
        assert_eq!(code, Some(1), "{source:?}: {err}");
        assert!(out.is_empty(), "{source:?}: {out}");
        assert!(err.contains(names), "{source:?}: {err}");
        assert!(!err.contains(&gone), "{source:?}: the server was called");
    }
}

/// Catches the key being on the command line, in the output, or in an error. Where the
/// server refuses the write (a 422, a 409, a 500), the CLI says the status and the
/// server's reason and nothing of the request it sent (the mutant: the request body
/// printed on an error). `--secret VALUE` is not a flag at all: a key in argv is in
/// `ps` and in shell history.
#[test]
fn the_secret_is_never_printed_and_never_a_flag() {
    let token = token_file("secret-out", TOKEN);
    let secret = file("secret-out", &format!("{SENTINEL}\n"), 0o600);
    for (status, body) in [
        (
            "422 Unprocessable Entity",
            r#"{"error":"the store refused the probe","code":"probe_failed"}"#,
        ),
        (
            "409 Conflict",
            r#"{"error":"a bound connection keeps its endpoint","code":"identity_change"}"#,
        ),
        (
            "500 Internal Server Error",
            r#"{"error":"internal","code":"internal"}"#,
        ),
    ] {
        let (code, out, err, _) = cluster(
            status,
            body,
            &[
                "object-store",
                "connection",
                "put",
                "main",
                "--endpoint",
                "http://127.0.0.1:9000",
                "--access-key-id",
                "AKID",
                "--secret-file",
                &secret,
                "--token-file",
                &token,
            ],
        );
        assert_eq!(code, Some(1), "{status}: {err}");
        assert!(out.is_empty(), "{status}: {out}");
        let code_word = status.split(' ').next().expect("a code");
        assert!(
            err.contains(code_word) && err.contains("\"code\""),
            "{status}: {err}"
        );
        assert!(!err.contains(SENTINEL), "{status}: {err}");
        assert!(!err.contains("secret_access_key"), "{status}: {err}");
        assert!(!err.contains("AKID"), "{status}: {err}");
    }
    for argv in [
        vec!["--secret", SENTINEL],
        vec!["--secret-access-key", SENTINEL],
    ] {
        let mut args = vec![
            "cluster",
            "object-store",
            "connection",
            "put",
            "main",
            "--endpoint",
            "http://127.0.0.1:9000",
            "--access-key-id",
            "AKID",
            "--token-file",
            &token,
        ];
        args.extend_from_slice(&argv);
        let (code, _, err) = kbf(&args);
        assert_eq!(code, Some(2), "{argv:?}: {err}");
        assert!(!err.contains(SENTINEL), "{argv:?}: clap echoed it: {err}");
    }
}

/// Catches a command line that is accepted and means something else: a key from no
/// source or from both, a name that would change the path (`/`, `:`, `..`, a capital),
/// a role with no consumer, a zero rate or limit, a write with no token file, and a
/// credential-only flag missing. All exit 2 with clap's message, and none reaches the
/// server (the address is dead).
#[test]
fn a_command_line_that_names_no_single_call_exits_2() {
    let secret = file("rows", &format!("{SENTINEL}\n"), 0o600);
    let put = |extra: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = [
            "cluster",
            "object-store",
            "connection",
            "put",
            "main",
            "--endpoint",
            "http://127.0.0.1:9000",
            "--access-key-id",
            "AKID",
            "--token-file",
            "t",
        ]
        .map(str::to_owned)
        .to_vec();
        v.extend(extra.iter().map(|s| (*s).to_owned()));
        v
    };
    let mut rows: Vec<Vec<String>> = vec![
        put(&[]),
        put(&["--secret-stdin", "--secret-file", &secret]),
        put(&["--secret-stdin", "--min-put-rate", "0"]),
        put(&["--secret-stdin", "--expect-generation", "x"]),
    ];
    let mut bad_name = put(&["--secret-stdin"]);
    for name in ["a/b", "a:verify", "..", "Main", "-x", ""] {
        bad_name[4] = name.to_owned();
        rows.push(bad_name.clone());
    }
    for tail in [
        &["connection", "show", "a/b"][..],
        &["connection", "delete", "A"],
        &["verify", "a:b"],
        &["bind", "--role", "history", "--connection", "m", "--bucket", "b"],
        &["bind", "--role", "backup", "--connection", "m", "--bucket", "b"],
        &["bind", "--role", "x", "--connection", "m", "--bucket", "b"],
        &["bind", "--connection", "a/b", "--bucket", "b"],
        &["bind", "--connection", "m"],
        &["bind", "--bucket", "b"],
        &["unbind", "--role", "history"],
        &["audit", "--limit", "0"],
        &["audit", "--limit", "-1"],
        &["connection"],
        &["connection", "put"],
    ] {
        let mut v: Vec<String> = ["cluster", "object-store"].map(str::to_owned).to_vec();
        v.extend(tail.iter().map(|s| (*s).to_owned()));
        v.extend(["--token-file", "t"].map(str::to_owned));
        rows.push(v);
    }
    // No token file: every verb of the group needs one (the whole gate, a read too).
    for tail in [
        &["status"][..],
        &["audit"],
        &["connection", "list"],
        &["connection", "show", "m"],
        &["connection", "delete", "m"],
        &["bind", "--connection", "m", "--bucket", "b"],
        &["unbind"],
        &["verify", "m"],
    ] {
        let mut v: Vec<String> = ["cluster", "object-store"].map(str::to_owned).to_vec();
        v.extend(tail.iter().map(|s| (*s).to_owned()));
        rows.push(v);
    }
    rows.push(["cluster", "denied"].map(str::to_owned).to_vec());
    rows.push(["cluster", "object-store"].map(str::to_owned).to_vec());
    for argv in rows {
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        let (code, out, err) = kbf(&args);
        assert_eq!(code, Some(2), "{argv:?}: {err}");
        assert!(out.is_empty(), "{argv:?}: {out}");
    }
}

/// Catches an answer the API refuses exiting 0, a refusal that hides why, and a token
/// file that is missing passing: a 409 `in_use` exits 1 naming the status and the
/// API's code; an address nothing listens at exits 1 naming the address.
#[test]
fn a_refusal_or_no_answer_exits_1_and_says_why() {
    let token = token_file("os-refused", TOKEN);
    let (code, out, err, _) = cluster(
        "409 Conflict",
        r#"{"error":"a binding names it","code":"in_use"}"#,
        &["object-store", "connection", "delete", "main", "--token-file", &token],
    );
    assert_eq!(code, Some(1), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("409") && err.contains("in_use"), "{err}");

    let gone = kbf_testkit::port::dead_addr().to_string();
    let (code, out, err) = kbf(&[
        "cluster",
        "--api",
        &gone,
        "object-store",
        "status",
        "--token-file",
        &token,
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains(&gone), "{err}");

    let (code, _, err) = kbf(&[
        "cluster",
        "denied",
        "--token-file",
        "/nonexistent/token",
    ]);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("/nonexistent/token"), "{err}");
}

/// Catches the group missing from `kbf cluster --help` (nothing else would tell an
/// operator it exists).
#[test]
fn help_names_the_object_store_group_and_denied() {
    let (code, out, err) = kbf(&["cluster", "--help"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("object-store"), "{out}");
    assert!(out.contains("denied"), "{out}");
    let (code, out, err) = kbf(&["cluster", "object-store", "--help"]);
    assert_eq!(code, Some(0), "{err}");
    for verb in ["status", "audit", "connection", "bind", "unbind", "verify"] {
        assert!(out.contains(verb), "{verb}: {out}");
    }
}
