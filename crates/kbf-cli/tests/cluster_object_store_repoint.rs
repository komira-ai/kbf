//! `kbf cluster object-store repoint`: the real binary against a one-shot HTTP server that
//! keeps the request it was sent. The route is tested in `kbf-server`
//! (`tests/object_store_api/repoint.rs`); these tests pin what the verb sends and what it
//! refuses to send.

mod cluster_support;

use cluster_support::{cluster, kbf, token_file};

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";

/// The JSON of a request's body.
fn json_of(request: &str) -> serde_json::Value {
    let (_, body) = request.split_once("\r\n\r\n").expect("a request head");
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body:?}"))
}

/// Catches a verb that sends the wrong method or path, ungated, or a body the server would
/// read differently: a bucket or prefix not given must be absent (the server then keeps the
/// binding's own; a default sent here would overwrite it), and one given, an empty prefix
/// included, must be sent.
#[test]
fn repoint_posts_the_identity_and_the_generation() {
    let token = token_file("repoint-sends", TOKEN);
    let cases: [(&[&str], serde_json::Value); 2] = [
        (
            &[
                "--endpoint",
                "http://127.0.0.1:30900",
                "--expect-generation",
                "7",
            ],
            serde_json::json!({"endpoint": "http://127.0.0.1:30900", "expect_generation": 7}),
        ),
        (
            &[
                "--endpoint",
                "http://127.0.0.1:9300",
                "--bucket",
                "av2dev1",
                "--prefix",
                "",
                "--expect-generation",
                "0",
            ],
            serde_json::json!({
                "endpoint": "http://127.0.0.1:9300",
                "bucket": "av2dev1",
                "prefix": "",
                "expect_generation": 0,
            }),
        ),
    ];
    for (flags, body) in cases {
        let mut args = vec![
            "object-store",
            "repoint",
            "--connection",
            "main",
            "--token-file",
            &token,
        ];
        args.extend_from_slice(flags);
        let (code, out, err, request) = cluster("200 OK", r#"{"ok":true}"#, &args);
        assert_eq!(code, Some(0), "{flags:?}: {err}");
        assert_eq!(out, "{\"ok\":true}\n", "{flags:?}");
        assert!(
            request.starts_with("POST /v1/object-stores/connections/main:repoint HTTP/1.1\r\n"),
            "{flags:?}: {request}"
        );
        let lower = request.to_ascii_lowercase();
        assert!(
            lower.contains(&format!("authorization: bearer {TOKEN}\r\n")),
            "{request}"
        );
        assert!(
            lower.contains("content-type: application/json\r\n"),
            "{request}"
        );
        assert_eq!(json_of(&request), body, "{flags:?}");
    }
}

/// Catches a repoint that goes out without the generation the operator read (the change
/// would then be unguarded), one that has a `--no-verify` (a repoint is always probed), and
/// the other omissions and bad values: each exits 2 from the command line and calls no
/// server (the address is one nothing listens at).
#[test]
fn repoint_refuses_without_expect_generation_and_has_no_no_verify() {
    let gone = kbf_testkit::port::dead_addr().to_string();
    let base = [
        "cluster",
        "--api",
        &gone,
        "object-store",
        "repoint",
        "--connection",
        "main",
        "--endpoint",
        "http://127.0.0.1:30900",
        "--expect-generation",
        "1",
        "--token-file",
        "t",
    ];
    let without = |flag: &str| -> Vec<&str> {
        let mut args = base.to_vec();
        let at = args.iter().position(|a| *a == flag).expect("a flag");
        args.drain(at..at + 2);
        args
    };
    let rows: Vec<Vec<&str>> = vec![
        without("--expect-generation"),
        without("--connection"),
        without("--endpoint"),
        without("--token-file"),
        [&base[..], &["--no-verify"]].concat(),
        [&base[..], &["--verify"]].concat(),
        base.iter()
            .map(|a| if *a == "1" { "x" } else { *a })
            .collect(),
        base.iter()
            .map(|a| if *a == "main" { "a/b" } else { *a })
            .collect(),
    ];
    for args in rows {
        let (code, out, err) = kbf(&args);
        assert_eq!(code, Some(2), "{args:?}: {err}");
        assert!(out.is_empty(), "{args:?}: {out}");
        assert!(
            !err.contains(&gone),
            "{args:?}: the server was called: {err}"
        );
    }
    let missing = without("--expect-generation");
    let (_, _, err) = kbf(&missing);
    assert!(err.contains("--expect-generation"), "{err}");
}

/// Catches the verb missing from the group's help (nothing else would tell an operator it
/// exists) and a help that does not say `--expect-generation` is needed.
#[test]
fn help_lists_repoint_and_its_generation_flag() {
    let (code, out, err) = kbf(&["cluster", "object-store", "--help"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("repoint"), "{out}");
    let (code, out, err) = kbf(&["cluster", "object-store", "repoint", "--help"]);
    assert_eq!(code, Some(0), "{err}");
    for flag in [
        "--connection",
        "--endpoint",
        "--bucket",
        "--prefix",
        "--expect-generation",
    ] {
        assert!(out.contains(flag), "{flag}: {out}");
    }
    assert!(!out.contains("--no-verify"), "{out}");
}
