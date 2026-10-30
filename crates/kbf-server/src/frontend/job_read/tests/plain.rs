//! The job listener in plain HTTP (`--job-plain-http`): the same routes and the same grants as
//! over TLS, on a listener that has no certificate, and the flags that choose between the two.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use tokio::net::TcpStream;

use std::sync::Arc;

use kbf_objstore::s3::fake::FakeS3;
use kbf_types::Digest;

use super::{
    Blobs, Got, JobReadFlags, SECRET, Serving, Timeouts, ca, exchange, files, ip, listening,
    path_of, prepare, world,
};
use crate::frontend::pack_tests::{flags_of, index, pack_flags, scrape, started, upload};
use crate::frontend::{command, parse};
use crate::listen::Latch;
use crate::test_dir::scratch_dir;

/// The flags of a plain listener at `listen` with a grant secret in `dir` and no certificate.
fn plain_flags(dir: &Path, listen: &str) -> JobReadFlags {
    let secret = dir.join("grant.secret");
    std::fs::write(&secret, SECRET).expect("secret");
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).expect("mode");
    JobReadFlags {
        listen: listen.parse().expect("an address"),
        tls: None,
        grant_secret_file: secret,
        max_blob_bytes: 536_870_912,
        reads_in_flight: 32,
    }
}

/// A plain listener on a loopback port over `source`, serving until the latch is dropped.
fn plain_listening(dir: &Path, source: Arc<Blobs>) -> (SocketAddr, Latch, bool) {
    let prepared = prepare(&plain_flags(dir, "127.0.0.1:0")).expect("prepared");
    let addr = prepared.local();
    let Serving {
        latch, task, tls, ..
    } = prepared
        .start(source, Timeouts::default())
        .expect("started");
    tokio::spawn(task);
    (addr, latch, tls.is_some())
}

async fn plain_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
) -> std::io::Result<Got> {
    let mut tcp = TcpStream::connect(addr).await?;
    exchange(&mut tcp, addr, method, path, token, "", &[]).await
}

/// Catches: a plain listener that wants a certificate or a key, one that serves no blob over
/// plain HTTP, one that answers with the wrong bytes or length, and one that is given a TLS
/// configuration anyway (it would have reload counters of files it does not have).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_listener_with_no_certificate_serves_an_input_to_a_valid_grant() {
    let dir = scratch_dir("job-read-plain-ok");
    let world = world();
    let (addr, _latch, has_tls) = plain_listening(dir.path(), world.source());
    assert!(!has_tls, "a plain listener has no TLS configuration");
    let got = plain_request(addr, "GET", &path_of(&world.input), Some(&world.token()))
        .await
        .expect("connects");
    assert_eq!(got.status, 200, "{}", got.text());
    assert_eq!(got.body, world.input_bytes);
    let length = world.input_bytes.len().to_string();
    assert_eq!(got.header("content-length"), Some(length.as_str()));
    assert_eq!(got.header("content-type"), Some("application/octet-stream"));
}

/// Catches: a plain listener whose authorization or routes differ from the TLS listener's: the
/// same requests (no grant, a bad grant, a grant for another Action's blob, a wrong method, an
/// unknown path, a good grant) are answered with the same status, headers and body by both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_listener_answers_every_request_as_the_tls_listener_does() {
    let dir = scratch_dir("job-read-plain-same");
    let farm = ca("farm CA");
    let tls_files = files(&dir.path().join("tls"), &farm, vec![ip("127.0.0.1")]);
    let world = world();
    let (tls_addr, _tls_latch) = listening(&tls_files, world.source(), Timeouts::default());
    let (plain_addr, _plain_latch, _) = plain_listening(&dir.path().join("plain"), world.source());
    let token = world.token();
    let asks: Vec<(&str, String, Option<&str>)> = vec![
        ("GET", path_of(&world.input), None),
        ("GET", path_of(&world.input), Some("not-a-grant")),
        ("GET", path_of(&world.stranger), Some(token.as_str())),
        ("POST", path_of(&world.input), Some(token.as_str())),
        ("GET", "/metrics".to_owned(), Some(token.as_str())),
        ("GET", path_of(&world.input), Some(token.as_str())),
    ];
    let mut statuses = Vec::new();
    for (method, path, bearer) in asks {
        let over_tls = super::request(tls_addr, &farm.pem, method, &path, bearer)
            .await
            .expect("tls connects");
        let plain = plain_request(plain_addr, method, &path, bearer)
            .await
            .expect("plain connects");
        assert_eq!(plain.status, over_tls.status, "{method} {path}");
        assert_eq!(plain.headers, over_tls.headers, "{method} {path}");
        assert_eq!(plain.body, over_tls.body, "{method} {path}");
        statuses.push(plain.status);
    }
    assert_eq!(statuses, [401, 401, 403, 405, 404, 200]);
}

/// Catches: a start that accepts a TLS file beside the plain flag, or a plain flag without what
/// it needs: each is refused by the flags' parse, naming the flags.
#[test]
fn the_plain_flag_replaces_the_certificate_and_keeps_the_secret() {
    let base = [
        "frontend",
        "--storage=http://127.0.0.1:1",
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
        "--api-listen=127.0.0.1:0",
    ];
    let parsed = |flags: &[&str]| {
        let argv = base.iter().chain(flags.iter()).copied();
        command().try_get_matches_from(argv).map(|m| parse(&m))
    };
    let listen = "--job-listen=127.0.0.1:8990";
    let secret = "--job-grant-secret-file=/etc/kbf/grant.secret";
    let cert = "--job-tls-cert=/etc/kbf/job.pem";
    let key = "--job-tls-key=/etc/kbf/job.key";
    let plain = "--job-plain-http";

    let flags = parsed(&[listen, secret, plain])
        .expect("matches")
        .expect("flags");
    let job = flags.job_read.expect("the flags are read");
    assert!(job.tls.is_none(), "plain HTTP");
    assert_eq!(job.listen, "127.0.0.1:8990".parse().expect("address"));
    let flags = parsed(&[listen, secret, cert, key])
        .expect("matches")
        .expect("flags");
    assert!(flags.job_read.expect("flags").tls.is_some(), "TLS");

    // No transport at all is as refused as it was; the plain flag and TLS files together are
    // ambiguous, and the plain flag alone has no listener or no secret to go with.
    let why = |flags: &[&str]| parsed(flags).expect_err("refused").to_string();
    let none = why(&[listen, secret]);
    assert!(
        none.contains("--job-tls-cert") && none.contains("--job-plain-http"),
        "{none}"
    );
    for both in [&[plain, cert, key][..], &[plain, cert], &[plain, key]] {
        let mut flags = vec![listen, secret];
        flags.extend(both);
        let refused = why(&flags);
        assert!(refused.contains("--job-plain-http"), "{refused}");
    }
    assert!(parsed(&[plain, secret]).is_err(), "no --job-listen");
    assert!(
        parsed(&[listen, plain]).is_err(),
        "no --job-grant-secret-file"
    );
    assert!(parsed(&[listen, secret, cert]).is_err(), "no key");
}

/// Catches: a plain listener that does not read its secret (it would serve any grant), and a
/// plain start that skips the bind: a bad secret and a busy address are refused as in TLS.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_plain_start_still_refuses_a_bad_secret_and_a_busy_address() {
    let dir = scratch_dir("job-read-plain-refused");
    let mut flags = plain_flags(dir.path(), "127.0.0.1:0");
    std::fs::write(&flags.grant_secret_file, b"too short").expect("secret");
    let why = prepare(&flags).err().expect("refused").to_string();
    assert!(why.contains("at least 32"), "{why}");
    flags.grant_secret_file = dir.path().join("absent");
    let why = prepare(&flags).err().expect("refused").to_string();
    assert!(why.contains("--job-grant-secret-file"), "{why}");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
    let busy = plain_flags(
        dir.path(),
        &taken.local_addr().expect("address").to_string(),
    );
    assert!(prepare(&busy).is_err());
}

/// Catches: the frontend not wiring a plain listener: it is bound, serves the blobs the REAPI
/// listener holds by a grant, counts its requests, and has no TLS files in the reload counters.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_serves_the_job_listener_in_plain_http() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("job-read-plain-frontend");
    let secret = plain_flags(dir.path(), "127.0.0.1:0").grant_secret_file;
    let mut more = pack_flags(s3.endpoint(), dir.path());
    more.extend([
        "--job-listen=127.0.0.1:0".to_owned(),
        "--job-plain-http".to_owned(),
        format!("--job-grant-secret-file={}", secret.display()),
    ]);
    let handle = started(&flags_of(&more), Some(index())).expect("starts");
    let job = handle.job_read.expect("the job listener is bound");
    let (front, api) = (handle.reapi, handle.api.expect("the operator API"));
    let served = tokio::spawn(handle.serve());
    let world = world();
    let blobs: Vec<(Digest, &[u8])> = world.blobs.iter().map(|(d, b)| (*d, &b[..])).collect();
    assert!(upload(front, &blobs).await.iter().all(|code| *code == 0));

    let got = plain_request(job, "GET", &path_of(&world.input), Some(&world.token()))
        .await
        .expect("connects");
    assert_eq!(got.status, 200, "{}", got.text());
    assert_eq!(got.body, world.input_bytes);
    let text = scrape(api).await;
    let ok = "kbf_job_read_requests_total{outcome=\"ok\"} 1\n";
    assert!(text.contains(ok), "{text}");
    assert!(!text.contains("listener=\"job\""), "{text}");
    served.abort();
}
