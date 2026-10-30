//! The read grant over plain HTTP: refused unless `--allow-plain-http-grant` is given.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;

use clap::Parser as _;
use kbf_segments::sha256;
use kbf_testkit::dir::TestDir;

use super::*;

const TOKEN: &str = "tok-secret-value";

/// Flags for a run against `grant_url`, with `extra` flags appended.
fn flags(dir: &TestDir, grant_url: &str, extra: &[&str]) -> Cli {
    let action = digest::to_reapi(&sha256(b"action"));
    let id = digest::from_reapi(&action).expect("digest");
    let work = dir.join("work");
    let mut args = vec![
        "kbf-task",
        "--pack-url-file",
        "/nonexistent-url",
        "--grant-url",
        grant_url,
        "--grant-token-file",
        "/nonexistent",
        "--lease",
        "lease-1",
    ];
    let id = id.to_string();
    args.extend(["--action", &id, "--work-dir", work.to_str().expect("utf-8")]);
    args.extend(extra);
    Cli::try_parse_from(args).expect("flags")
}

/// Points `cli` at a token, a pack URL and a trusted root the run can read.
fn with_files(dir: &TestDir, mut cli: Cli) -> Cli {
    std::fs::write(dir.join("token"), format!("{TOKEN}\n")).expect("write");
    std::fs::write(dir.join("url"), "https://store.example/pack?sig=s\n").expect("write");
    std::fs::write(dir.join("root.der"), [1, 2, 3]).expect("write");
    cli.grant_token_file = Some(dir.join("token"));
    cli.pack_url_file = Some(dir.join("url"));
    cli.root_der = vec![dir.join("root.der")];
    cli
}

/// Catches the opt-in being on by default, or not parsed: the flag is off unless given.
#[test]
fn the_plain_grant_flag_defaults_to_off_and_parses() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-task-plain-grant-flag");
    assert!(!flags(&dir, "https://grant.example", &[]).allow_plain_http_grant);
    let on = flags(&dir, "http://grant.example", &["--allow-plain-http-grant"]);
    assert!(on.allow_plain_http_grant);
}

/// Catches an `http` grant URL being accepted without the flag: it is a setup failure (exit 14)
/// at connect, before any connection (nothing listens at the port), and the message names the
/// flag but holds neither the token, the URL's query nor its host.
#[test]
fn an_http_grant_is_a_setup_failure_without_the_flag() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-task-plain-grant-refused");
    for url in [
        "http://127.0.0.1:9/grant?sig=QUERYSECRET",
        "HTTP://127.0.0.1:9/grant?sig=QUERYSECRET",
    ] {
        let cli = with_files(&dir, flags(&dir, url, &[]));
        let failure = connect(&cli).err().expect("refused");
        assert!(matches!(failure, Failure::Setup(_)));
        assert_eq!(failure.exit_code(), 14);
        let text = failure.to_string();
        assert!(text.contains("--allow-plain-http-grant"), "{text}");
        assert!(!text.contains(TOKEN), "{text}");
        assert!(!text.contains("QUERYSECRET"), "{text}");
        assert!(!text.contains("127.0.0.1"), "{text}");
    }
}

/// Catches an `https` grant URL being refused by the scheme check: it connects without the flag.
#[test]
fn an_https_grant_connects_without_the_flag() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-task-plain-grant-https");
    let cli = with_files(&dir, flags(&dir, "https://grant.example", &[]));
    assert!(connect(&cli).is_ok());
}

/// Serves one `200` with `body` over plain HTTP; the thread returns the request line and the
/// Authorization header value it saw.
fn serve_once(body: Vec<u8>) -> (String, std::thread::JoinHandle<(String, String)>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let reader = BufReader::new(stream.try_clone().expect("clone"));
        let lines: Vec<String> = reader
            .lines()
            .map(|line| line.expect("line"))
            .take_while(|line| !line.is_empty())
            .collect();
        let auth = lines
            .iter()
            .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
            .expect("authorization header");
        let value = auth.split_once(": ").expect("header").1.to_owned();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let mut stream = stream;
        stream.write_all(head.as_bytes()).expect("head");
        stream.write_all(&body).expect("body");
        (lines[0].clone(), value)
    });
    (base, handle)
}

/// Catches the flag not reaching the source: with it, the source built by `connect` fetches a
/// blob from a plain-HTTP server, and that server saw `Authorization: Bearer <token>`.
#[test]
fn the_flag_lets_the_source_fetch_over_plain_http_with_the_bearer() {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-task-plain-grant-fetch");
    let blob = b"payload".to_vec();
    let digest = digest::to_reapi(&sha256(&blob));
    let (base, server) = serve_once(blob.clone());
    let cli = with_files(&dir, flags(&dir, &base, &["--allow-plain-http-grant"]));
    let (source, _) = connect(&cli).expect("connect");
    let got = source.get(&digest).expect("fetch");
    assert_eq!(got, blob);
    let (request, auth) = server.join().expect("server");
    assert_eq!(
        request,
        format!("GET /{}/{} HTTP/1.1", digest.hash, digest.size_bytes)
    );
    assert_eq!(auth, format!("Bearer {TOKEN}"));
}
