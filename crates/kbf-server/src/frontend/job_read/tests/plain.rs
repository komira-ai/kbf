//! The job listener in plain HTTP (`--job-plain-http`): the same routes and the same grants as
//! over TLS, on a listener that has no certificate, and the flags that choose between the two.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use tokio::net::TcpStream;

use std::sync::Arc;

use kbf_front::job_read::{Limits, MAX_BATCH_REQUEST_BYTES};
use kbf_objstore::s3::fake::FakeS3;
use kbf_types::Digest;

use super::batch::{digests, json_headers};
use super::{
    Blobs, Got, JobReadFlags, SECRET, Serving, Timeouts, ca, exchange, files, ip, listening,
    path_of, prepare, world,
};
use crate::frontend::pack_tests::{flags_of, index, pack_flags, scrape, started, upload};
use crate::frontend::{command, parse};
use crate::listen::Latch;
use crate::test_dir::scratch_dir;
use crate::test_log::capture_at;

/// The flags of a plain listener at `listen` with a grant secret in `dir` and no certificate.
fn plain_flags(dir: &Path, listen: &str) -> JobReadFlags {
    std::fs::create_dir_all(dir).expect("the directory");
    let secret = dir.join("grant.secret");
    std::fs::write(&secret, SECRET).expect("secret");
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).expect("mode");
    JobReadFlags {
        listen: listen.parse().expect("an address"),
        tls: None,
        grant_secret_file: secret,
        max_blob_bytes: 536_870_912,
        reads_in_flight: 32,
        read_budget_bytes: Limits::default().read_budget_bytes,
        read_idle_secs: Limits::default().held_idle.as_secs(),
        store: None,
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

/// One request of the parity tests: method, path, bearer, extra header lines and body.
struct Ask<'a> {
    method: &'a str,
    path: String,
    bearer: Option<&'a str>,
    extra: String,
    body: Vec<u8>,
}

fn get<'a>(path: String, bearer: Option<&'a str>) -> Ask<'a> {
    Ask {
        method: "GET",
        path,
        bearer,
        extra: String::new(),
        body: Vec::new(),
    }
}

fn batch<'a>(body: &str, bearer: Option<&'a str>) -> Ask<'a> {
    Ask {
        method: "POST",
        path: "/batch".to_owned(),
        bearer,
        extra: json_headers(body),
        body: body.as_bytes().to_vec(),
    }
}

/// Catches: a plain listener whose authorization or routes differ from the TLS listener's: the
/// same requests (no grant, a bad grant, a grant for another Action's blob, a wrong method, an
/// unknown path, a good grant, a batch with a good and with a bad grant, and a batch body over
/// the bound) are answered with the same status, headers and body by both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_listener_answers_every_request_as_the_tls_listener_does() {
    let dir = scratch_dir("job-read-plain-same");
    let farm = ca("farm CA");
    let tls_files = files(&dir.path().join("tls"), &farm, vec![ip("127.0.0.1")]);
    let world = world();
    let (tls_addr, _tls_latch) = listening(&tls_files, world.source(), Timeouts::default());
    let (plain_addr, _plain_latch, _) = plain_listening(&dir.path().join("plain"), world.source());
    let token = world.token();
    let good = Some(token.as_str());
    let wanted = digests(&[&world.input, &world.stranger]);
    let oversize = "x".repeat(MAX_BATCH_REQUEST_BYTES + 1);
    let mut post_input = get(path_of(&world.input), good);
    post_input.method = "POST";
    let asks: Vec<Ask<'_>> = vec![
        get(path_of(&world.input), None),
        get(path_of(&world.input), Some("not-a-grant")),
        get(path_of(&world.stranger), good),
        post_input,
        get("/metrics".to_owned(), good),
        get(path_of(&world.input), good),
        batch(&wanted, good),
        batch(&wanted, Some("not-a-grant")),
        batch(&oversize, good),
    ];
    let mut statuses = Vec::new();
    for ask in asks {
        let Ask {
            method,
            path,
            bearer,
            extra,
            body,
        } = &ask;
        let over_tls = super::send(tls_addr, &farm.pem, method, path, *bearer, extra, body)
            .await
            .expect("tls connects");
        let mut tcp = TcpStream::connect(plain_addr)
            .await
            .expect("plain connects");
        let plain = exchange(&mut tcp, plain_addr, method, path, *bearer, extra, body)
            .await
            .expect("plain answers");
        assert_eq!(plain.status, over_tls.status, "{method} {path}");
        assert_eq!(
            without_date(&plain.headers),
            without_date(&over_tls.headers),
            "{method} {path}"
        );
        assert_eq!(
            has_date(&plain.headers),
            has_date(&over_tls.headers),
            "{method} {path}"
        );
        assert_eq!(plain.body, over_tls.body, "{method} {path}");
        statuses.push(plain.status);
    }
    assert_eq!(&statuses[..8], [401, 401, 403, 405, 404, 200, 200, 401]);
    assert_ne!(statuses[8], 200, "a body over the bound is not served");
}

/// Catches: a plain listener that keeps its socket open once the drain begins (the latch
/// closes) while the TLS listener closes it: after the close neither takes a new connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drain_closes_the_plain_socket_as_it_closes_the_tls_one() {
    let dir = scratch_dir("job-read-plain-drain");
    let farm = ca("farm CA");
    let tls_files = files(&dir.path().join("tls"), &farm, vec![ip("127.0.0.1")]);
    let world = world();
    let (tls_addr, tls_latch) = listening(&tls_files, world.source(), Timeouts::default());
    let (plain_addr, plain_latch, _) = plain_listening(&dir.path().join("plain"), world.source());
    let token = world.token();
    let ok = plain_request(plain_addr, "GET", &path_of(&world.input), Some(&token))
        .await
        .expect("answers before the drain");
    assert_eq!(ok.status, 200);
    tls_latch.close();
    plain_latch.close();
    for addr in [tls_addr, plain_addr] {
        let mut refused = false;
        for _ in 0..200 {
            if TcpStream::connect(addr).await.is_err() {
                refused = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(refused, "{addr} is still accepting");
    }
}

/// Catches: a plain listener that starts without saying the grants it serves cross the network
/// unencrypted, and a TLS listener that says so: the warning names the address, and only the
/// plain start writes it.
#[tokio::test]
async fn only_a_plain_start_warns_that_grants_travel_unencrypted() {
    let dir = scratch_dir("job-read-plain-warn");
    let farm = ca("farm CA");
    let log = capture_at(tracing::Level::WARN);
    let tls_files = files(&dir.path().join("tls"), &farm, vec![ip("127.0.0.1")]);
    let (_, _tls_latch) = listening(&tls_files, world().source(), Timeouts::default());
    assert!(!log.text().contains("unencrypted"), "{}", log.text());
    let prepared = prepare(&plain_flags(&dir.path().join("plain"), "127.0.0.1:0")).expect("ok");
    let addr = prepared.local();
    let started = prepared.start(world().source(), Timeouts::default());
    let _serving = started.expect("started");
    let text = log.text();
    assert!(text.contains("--job-plain-http"), "{text}");
    assert!(text.contains("unencrypted"), "{text}");
    assert!(text.contains(&addr.to_string()), "{text}");
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

/// `headers` without `date`: two answers given a moment apart may fall in different seconds, so
/// the clock's value is no part of what the two transports must agree on.
fn without_date(headers: &[(String, String)]) -> Vec<&(String, String)> {
    headers.iter().filter(|(name, _)| name != "date").collect()
}

/// Whether `headers` carries a `date`.
fn has_date(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(name, _)| name == "date")
}
