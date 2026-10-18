//! `kbf token` against a one-shot HTTP server that keeps the request it was sent. Each
//! test names the defect it catches.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kbf_types::enroll_api::{TokenCreated, TokenInfo};

use super::*;
use crate::parse_token;

const OPERATOR: &str = "kbf-test-token-0123456789abcdef0123456789";
const HANG: Duration = Duration::from_secs(20);

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "kbf-cli-token-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("a directory");
    dir
}

fn token_file(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("api-token");
    std::fs::write(&path, format!("{OPERATOR}\n")).expect("write");
    path
}

/// Answers one request with `status` and `body`; the thread returns the request as sent.
fn serve_once(status: &'static str, body: String) -> (SocketAddr, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("non-blocking");
    let addr = listener.local_addr().expect("addr");
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + HANG;
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("kbf never called the server: {e}"),
            }
        };
        stream.set_nonblocking(false).expect("blocking");
        stream.set_read_timeout(Some(HANG)).expect("timeout");
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).expect("read the request");
            request.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&request);
            if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().expect("a length"))
                    })
                    .unwrap_or(0);
                if rest.len() >= length {
                    break;
                }
            }
            assert!(n > 0, "the request ended early");
        }
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(reply.as_bytes()).expect("reply");
        String::from_utf8(request).expect("UTF-8")
    });
    (addr, thread)
}

fn parse(args: &[&str]) -> Token {
    // `args[0]` is the verb; the command line is `kbf --token=<verb> <flags>`.
    let mut argv: Vec<OsString> = vec!["kbf".into(), format!("{FLAG}={}", args[0]).into()];
    argv.extend(args[1..].iter().map(OsString::from));
    parse_token(&argv).expect("a command line")
}

fn info() -> TokenInfo {
    TokenInfo {
        id: "aaaaaaaaaaaa".to_owned(),
        roles: vec!["daemon".to_owned()],
        max_uses: 1,
        uses: 0,
        expires_at: 100,
        created_at: 10,
        revoked_at: None,
        node_id: None,
        node_prefix: None,
        names: Vec::new(),
        replace: false,
        label: String::new(),
        created_by: "operator-api".to_owned(),
    }
}

fn created_json() -> String {
    serde_json::to_string(&TokenCreated {
        token: "kbft1.aaaaaaaaaaaa.SECRETPART.PINPART".to_owned(),
        secret: "SECRETPART".to_owned(),
        info: info(),
    })
    .expect("json")
}

/// Runs `kbf token --api <server> <args> --token-file <file>` against a server answering
/// `status` and `body`; the result and the request the server saw.
fn against(
    status: &'static str,
    body: String,
    args: &[&str],
) -> (Result<String, TokenError>, String) {
    let dir = scratch("against");
    let file = token_file(&dir);
    let (addr, request) = serve_once(status, body);
    let (api, file) = (addr.to_string(), file.display().to_string());
    let mut argv = args.to_vec();
    argv.extend(["--api", api.as_str(), "--token-file", file.as_str()]);
    let result = execute(&parse(&argv));
    (
        result,
        request.join().expect("the server thread").to_lowercase(),
    )
}

// Catches a duration read wrongly (a unit mistaken, a bare number taken as seconds or
// minutes, an overflow wrapping to a short life): only a whole number and one of s, m, h, d.
#[test]
fn a_duration_is_a_number_and_a_unit() {
    for (text, want) in [
        ("30s", 30),
        ("15m", 900),
        ("24h", 86_400),
        ("7d", 604_800),
        ("0s", 0),
    ] {
        assert_eq!(parse_duration(text), Ok(want), "{text}");
    }
    for text in [
        "",
        "h",
        "5",
        "5x",
        "-5m",
        "1.5h",
        " 5m",
        "18446744073709551615d",
    ] {
        let why = parse_duration(text).expect_err(text);
        assert!(why.contains("not a duration"), "{why}");
    }
}

// Catches a create that sends another path, method, body or credential than the API
// documents: POST /v1/enroll/tokens with the bearer of the token file, a JSON content type
// and the options as the body; the answer is printed as it came.
#[test]
fn create_posts_the_options_with_the_bearer() {
    let body = created_json();
    let (result, request) = against(
        "200 OK",
        body.clone(),
        &[
            "create",
            "--roles",
            "daemon,storage",
            "--max-uses",
            "3",
            "--expires-in",
            "24h",
            "--node-prefix",
            "rack-",
            "--names",
            "a.example,.b.example",
            "--replace",
            "--label",
            "rack 4",
        ],
    );
    assert_eq!(result.expect("created"), body);
    assert!(
        request.starts_with("post /v1/enroll/tokens http/1.1\r\n"),
        "{request}"
    );
    assert!(
        request.contains(&format!("authorization: bearer {OPERATOR}")),
        "{request}"
    );
    assert!(
        request.contains("content-type: application/json"),
        "{request}"
    );
    let sent = request.split_once("\r\n\r\n").expect("a body").1;
    let sent: serde_json::Value = serde_json::from_str(sent).expect("JSON");
    assert_eq!(
        sent,
        serde_json::json!({
            "roles": ["daemon", "storage"], "max_uses": 3, "expires_in": 86_400,
            "node_id": null, "node_prefix": "rack-", "names": ["a.example", ".b.example"],
            "replace": true, "label": "rack 4"
        })
    );
}

// Catches a GET or DELETE the operator gate would refuse: a list or revoke sent without
// the content type (415 on a real server), or a DELETE without a content length; and a
// revoke of something that is not a token id reaching the network at all.
#[test]
fn list_and_revoke_carry_the_content_type_the_gate_wants() {
    let (result, request) = against("200 OK", r#"{"tokens":[]}"#.to_owned(), &["list"]);
    assert_eq!(result.expect("listed"), r#"{"tokens":[]}"#);
    assert!(
        request.starts_with("get /v1/enroll/tokens http/1.1\r\n"),
        "{request}"
    );
    assert!(
        request.contains("content-type: application/json"),
        "{request}"
    );
    assert!(
        request.contains(&format!("authorization: bearer {OPERATOR}")),
        "{request}"
    );

    let (result, request) = against(
        "200 OK",
        r#"{"id":"aaaaaaaaaaaa"}"#.to_owned(),
        &["revoke", "--token-id=aaaaaaaaaaaa"],
    );
    result.expect("revoked");
    assert!(
        request.starts_with("delete /v1/enroll/tokens/aaaaaaaaaaaa http/1.1\r\n"),
        "{request}"
    );
    assert!(
        request.contains("content-type: application/json"),
        "{request}"
    );
    assert!(request.contains("content-length: 0"), "{request}");

    let dir = scratch("badid");
    let file = token_file(&dir).display().to_string();
    for id in ["../x", "AAAAAAAAAAAA", "a"] {
        let bad = execute(&parse(&[
            "revoke",
            &format!("--token-id={id}"),
            "--token-file",
            &file,
        ]));
        assert!(matches!(bad, Err(TokenError::Id)), "{id}");
    }
}

// Catches an API refusal read as success, and a refusal that hides the reason: a non-2xx
// answer is an error that carries the status and the API's own words.
#[test]
fn a_refusal_is_an_error_with_the_apis_words() {
    let (result, _) = against(
        "403 Forbidden",
        r#"{"error":"writes are off"}"#.to_owned(),
        &["list"],
    );
    let shown = result.expect_err("refused").to_string();
    assert_eq!(shown, r#"403 {"error":"writes are off"}"#);
}

// Catches a token file written with the umask's mode (readable by others for a moment or
// for good), written with more than the string, or a secret echoed to the terminal: the
// file is 0600, holds the token string and a newline, and what is printed is the token as a
// list shows it.
#[test]
fn out_writes_the_token_string_to_a_0600_file_and_prints_none_of_it() {
    let dir = scratch("out");
    let out = dir.join("enroll-token");
    let (result, _) = against(
        "200 OK",
        created_json(),
        &[
            "create",
            "--roles",
            "daemon",
            "--out",
            &out.display().to_string(),
        ],
    );
    let printed = result.expect("created");
    assert_eq!(
        std::fs::read_to_string(&out).expect("the file"),
        "kbft1.aaaaaaaaaaaa.SECRETPART.PINPART\n"
    );
    let mode = std::fs::metadata(&out).expect("meta").permissions().mode();
    assert_eq!(mode & 0o7777, 0o600);
    assert!(
        !printed.contains("SECRETPART") && !printed.contains("PINPART"),
        "{printed}"
    );
    let shown: TokenInfo = serde_json::from_str(&printed).expect("a token as a list shows it");
    assert_eq!(shown, info());
}

// Catches a failed call that leaves an empty or partial token file behind (the operator
// would take it for a token): a refusal, an answer that is not a created token and a server
// that cannot be reached each remove the file.
#[test]
fn out_leaves_no_file_when_the_call_fails() {
    let dir = scratch("failed");
    let out = dir.join("enroll-token");
    let shown = out.display().to_string();
    let args = ["create", "--roles", "daemon", "--out", shown.as_str()];

    let (result, _) = against(
        "500 Internal Server Error",
        r#"{"error":"down"}"#.to_owned(),
        &args,
    );
    assert!(matches!(
        result,
        Err(TokenError::Refused { status: 500, .. })
    ));
    assert!(!out.exists(), "a refusal left the file");

    let (result, _) = against("200 OK", r#"{"token":"x"}"#.to_owned(), &args);
    assert!(matches!(result, Err(TokenError::Answer)));
    assert!(!out.exists(), "a garbled answer left the file");

    let gone = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr")
    };
    let (api, file) = (gone.to_string(), token_file(&dir).display().to_string());
    let mut argv = args.to_vec();
    argv.extend(["--api", api.as_str(), "--token-file", file.as_str()]);
    assert!(matches!(execute(&parse(&argv)), Err(TokenError::Client(_))));
    assert!(!out.exists(), "an unreachable server left the file");
}

// Catches a write that overwrites or follows what is already there (a token written
// through a symlink to another file, or over a file the operator wanted to keep), and one
// that spends a token before finding that out: an existing file and a dangling symlink are
// refused, untouched, with no call made.
#[test]
fn out_refuses_what_exists_before_calling() {
    let dir = scratch("exists");
    let kept = dir.join("kept");
    std::fs::write(&kept, "keep me").expect("write");
    let link = dir.join("link");
    let target = dir.join("target");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("non-blocking");
    let (api, file) = (
        listener.local_addr().expect("addr").to_string(),
        token_file(&dir).display().to_string(),
    );
    for path in [&kept, &link] {
        let shown = path.display().to_string();
        let argv = [
            "create",
            "--roles",
            "daemon",
            "--out",
            shown.as_str(),
            "--api",
            api.as_str(),
            "--token-file",
            file.as_str(),
        ];
        let err = execute(&parse(&argv)).expect_err("refused");
        assert!(matches!(&err, TokenError::Out { made: None, .. }), "{err}");
        assert!(err.to_string().contains(&shown), "{err}");
    }
    assert_eq!(std::fs::read_to_string(&kept).expect("kept"), "keep me");
    assert!(!target.exists(), "the write followed the symlink");
    assert!(listener.accept().is_err(), "a call was made");
}

struct Full;

impl Write for Full {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("no space left"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// Catches a token that was made but could not be saved being lost without a word: the
// error names the token's id and says how to revoke it, and never the string.
#[test]
fn a_failed_save_names_the_token_that_was_made() {
    let (addr, request) = serve_once("200 OK", created_json());
    let client = OperatorClient::new(addr, Some(OPERATOR.to_owned()));
    let err = deliver(&client, "{}", std::path::Path::new("/x/token"), &mut Full)
        .expect_err("a full disk");
    request.join().expect("server");
    let shown = err.to_string();
    assert!(
        shown.contains("/x/token") && shown.contains("no space left"),
        "{shown}"
    );
    assert!(
        shown.contains("aaaaaaaaaaaa")
            && shown.contains("kbf --token=revoke --token-id=aaaaaaaaaaaa"),
        "{shown}"
    );
    assert!(!shown.contains("SECRETPART"), "{shown}");
}

// Catches a missing token file reaching the network, and a command line that lets a call
// through without its credential or without roles.
#[test]
fn the_credential_is_required_and_read_before_any_call() {
    let missing = execute(&parse(&["list", "--token-file", "/nonexistent/api-token"]));
    let err = missing.expect_err("no file");
    assert!(matches!(err, TokenError::Client(_)));
    assert!(err.to_string().contains("/nonexistent/api-token"), "{err}");
    for argv in [
        vec!["kbf", "--token=list"],
        vec!["kbf", "--token=create", "--token-file", "f"],
        vec!["kbf", "--token=revoke", "--token-file", "f"],
        vec!["kbf", "--token=bogus", "--token-file", "f"],
        vec!["kbf", "--token="],
        vec!["kbf", "--token-file", "f"],
        vec!["kbf", "--token=list", "extra", "--token-file", "f"],
    ] {
        let argv: Vec<OsString> = argv.into_iter().map(OsString::from).collect();
        assert!(parse_token(&argv).is_err(), "{argv:?}");
    }
}

// Catches exit codes that do not follow the answer: 0 for a 2xx, 1 for anything else.
#[test]
fn the_exit_status_follows_the_answer() {
    let dir = scratch("exit");
    let file = token_file(&dir).display().to_string();
    for (status, want) in [
        ("200 OK", ExitCode::SUCCESS),
        ("404 Not Found", ExitCode::FAILURE),
    ] {
        let (addr, request) = serve_once(status, "{}".to_owned());
        let api = addr.to_string();
        let token = parse(&["list", "--api", &api, "--token-file", &file]);
        assert_eq!(
            format!("{:?}", run(&token)),
            format!("{want:?}"),
            "{status}"
        );
        request.join().expect("server");
    }
}

// Catches the id of an error being the only place a token string is built: the Display of
// every error is free of the string (a log line of the error leaks nothing).
#[test]
fn error_texts_name_what_failed() {
    let shown = [
        TokenError::Answer.to_string(),
        TokenError::Id.to_string(),
        TokenError::Refused {
            status: 409,
            body: "not the signer\n".into(),
        }
        .to_string(),
    ];
    assert_eq!(shown[2], "409 not the signer");
    assert!(shown[0].contains("not a created token") && shown[1].contains("token id"));
}
