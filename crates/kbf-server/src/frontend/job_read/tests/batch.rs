//! `POST /batch` over the job listener's real TLS: the request's body is read within a bound
//! and a time, the frames are sent with their content length, and a GET of `/batch` is told the
//! method that is allowed.

use std::time::Duration;

use super::{Timeouts, ca, files, ip, listening, path_of, send, world};
use crate::test_dir::scratch_dir;

pub(super) fn digests(of: &[&kbf_types::Digest]) -> String {
    let quoted: Vec<String> = of.iter().map(|d| format!("\"{d}\"")).collect();
    format!("{{\"digests\":[{}]}}", quoted.join(","))
}

fn frame(status: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![status];
    let length = if status == 0 { payload.len() as u64 } else { 0 };
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(if status == 0 { payload } else { &[] });
    out
}

/// The header lines of a JSON POST of `body`.
pub(super) fn json_headers(body: &str) -> String {
    format!(
        "Content-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    )
}

/// Catches: a batch answered without its content length or with wrong frames over the wire, a
/// body that is not read (a POST reaching the handler with no body), and a content type that is
/// not an octet stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_is_answered_with_frames_and_their_content_length_over_tls() {
    let dir = scratch_dir("job-read-batch-ok");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let world = world();
    let (addr, _latch) = listening(&files, world.source(), Timeouts::default());
    let body = digests(&[&world.input, &world.stranger, &world.input]);
    let got = send(
        addr,
        &farm.pem,
        "POST",
        "/batch",
        Some(&world.token()),
        &json_headers(&body),
        body.as_bytes(),
    )
    .await
    .expect("connects");
    assert_eq!(got.status, 200, "{}", got.text());
    let expected = [
        frame(0, &world.input_bytes),
        frame(2, b""),
        frame(0, &world.input_bytes),
    ]
    .concat();
    assert_eq!(got.body, expected);
    let length = expected.len().to_string();
    assert_eq!(got.header("content-length"), Some(length.as_str()));
    assert_eq!(got.header("content-type"), Some("application/octet-stream"));
}

/// Catches: a client that promises a body and never sends it holding a connection and a read
/// of the listener for the connection's lifetime (a job is untrusted code): the body is waited
/// for as long as the request's headers are, then the request is a malformed one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_that_never_arrives_is_a_malformed_request() {
    let dir = scratch_dir("job-read-batch-stall");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let world = world();
    let timeouts = Timeouts {
        header: Duration::from_millis(300),
        connection: Duration::from_secs(60),
    };
    let (addr, _latch) = listening(&files, world.source(), timeouts);
    let token = world.token();
    let asked = send(
        addr,
        &farm.pem,
        "POST",
        "/batch",
        Some(&token),
        "Content-Length: 100\r\n",
        &[],
    );
    let got = tokio::time::timeout(Duration::from_secs(20), asked)
        .await
        .expect("answered within the bound")
        .expect("connects");
    assert_eq!(got.status, 400);
    assert_eq!(got.text(), "malformed\n");
}

/// Catches: a body of any size being read into memory (the largest request is 256 digests, a
/// few KiB): one far over that is a malformed request, and one that is not authorized is
/// refused for its token, as the order of the refusals is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_over_the_bound_is_malformed_and_a_missing_token_comes_first() {
    let dir = scratch_dir("job-read-batch-big");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let world = world();
    let (addr, _latch) = listening(&files, world.source(), Timeouts::default());
    let big = vec![b' '; 70_000];
    let extra = format!("Content-Length: {}\r\n", big.len());
    let ask = |token: Option<String>| {
        let pem = farm.pem.clone();
        let (extra, big) = (extra.clone(), big.clone());
        async move {
            send(addr, &pem, "POST", "/batch", token.as_deref(), &extra, &big)
                .await
                .expect("connects")
        }
    };
    let got = ask(Some(world.token())).await;
    assert_eq!(got.status, 400);
    assert_eq!(got.text(), "malformed\n");
    let got = ask(None).await;
    assert_eq!(got.status, 401);
    assert_eq!(got.text(), "missing_token\n");
}

/// Catches: a 405 that tells a client of `/batch` it may GET (`Allow` names the method the path
/// takes), and a GET of `/batch` read as a blob path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_get_of_batch_is_405_and_allow_names_post() {
    let dir = scratch_dir("job-read-batch-allow");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let world = world();
    let (addr, _latch) = listening(&files, world.source(), Timeouts::default());
    let token = world.token();
    let got = send(addr, &farm.pem, "GET", "/batch", Some(&token), "", &[])
        .await
        .expect("connects");
    assert_eq!(got.status, 405);
    assert_eq!(got.header("allow"), Some("POST"));
    let got = send(
        addr,
        &farm.pem,
        "POST",
        &path_of(&world.input),
        Some(&token),
        "",
        &[],
    )
    .await
    .expect("connects");
    assert_eq!(got.status, 405);
    assert_eq!(got.header("allow"), Some("GET"));
}
