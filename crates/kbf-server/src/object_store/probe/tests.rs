//! The probe, against a scripted store on loopback. Each test names the defect it catches.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use super::*;
use crate::enroll::Random;

const SENTINEL: &str = "SENTINEL-probe-secret-77aa88bb";

/// What the scripted store answers; the rest is S3's usual shape.
#[derive(Clone, Copy)]
struct Script {
    /// The status of the first PUT.
    first_put: u16,
    /// The status of a PUT of a key that already exists and carries `If-None-Match`.
    second_put: u16,
    /// The status of a ranged GET; 206 answers the stored bytes.
    get: u16,
    /// Flip the bytes a 206 answers.
    wrong_bytes: bool,
    /// The status of DELETE.
    delete: u16,
    /// Never answer.
    silent: bool,
}

impl Script {
    const OK: Self = Self {
        first_put: 200,
        second_put: 412,
        get: 206,
        wrong_bytes: false,
        delete: 204,
        silent: false,
    };
}

/// The store on loopback and the requests it took, as `METHOD path [if-none-match]`.
struct Store {
    endpoint: String,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Store {
    async fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("addr"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            let mut kept: Option<Vec<u8>> = None;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(at + 4);
                    }
                };
                let Some(head_end) = head_end else { continue };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                while buf.len() < head_end + length {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let first = head.lines().next().unwrap_or_default().to_owned();
                let mut words = first.split(' ');
                let (method, path) = (
                    words.next().unwrap_or_default().to_owned(),
                    words.next().unwrap_or_default().to_owned(),
                );
                let conditional = head.contains("if-none-match: *");
                log.lock()
                    .expect("log")
                    .push(format!("{method} {path} {conditional}"));
                if script.silent {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    continue;
                }
                let body = buf[head_end..head_end + length.min(buf.len() - head_end)].to_vec();
                let (status, answer, extra) = match method.as_str() {
                    "put" => {
                        let status = if kept.is_none() {
                            kept = Some(body);
                            script.first_put
                        } else {
                            script.second_put
                        };
                        (status, Vec::new(), String::new())
                    }
                    "get" => {
                        let mut bytes = kept.clone().unwrap_or_default();
                        if script.wrong_bytes {
                            bytes.iter_mut().for_each(|b| *b ^= 0xff);
                        }
                        let range = format!(
                            "content-range: bytes 0-{}/{}\r\n",
                            bytes.len().saturating_sub(1),
                            bytes.len()
                        );
                        (script.get, bytes, range)
                    }
                    "delete" => {
                        kept = None;
                        (script.delete, Vec::new(), String::new())
                    }
                    _ => (501, Vec::new(), String::new()),
                };
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\n{extra}connection: close\r\n\r\n",
                    answer.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
                let _ = sock.write_all(&answer).await;
                let _ = sock.shutdown().await;
            }
        });
        Self { endpoint, seen }
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("log").clone()
    }
}

/// Every draw is the same eight bytes, so the probe key is known.
struct Fixed;

impl Random for Fixed {
    fn fill(&self, buf: &mut [u8]) -> io::Result<()> {
        buf.fill(0xab);
        Ok(())
    }
}

struct NoRandom;

impl Random for NoRandom {
    fn fill(&self, _: &mut [u8]) -> io::Result<()> {
        Err(io::Error::other("none"))
    }
}

fn target<'a>(endpoint: &'a str, secret: &'a Secret) -> ProbeTarget<'a> {
    ProbeTarget {
        endpoint,
        region: "",
        access_key_id: "AKID",
        secret,
        bucket: "farm-bucket",
        prefix: "cold/",
        conditional_put: false,
        min_put_rate: None,
    }
}

/// A secret that lives for the whole test run, so a target can borrow it for 'static.
fn secret() -> &'static Secret {
    Box::leak(Box::new(Secret::new(SENTINEL.to_owned())))
}

fn leaked(text: &str) -> &'static str {
    Box::leak(text.to_owned().into_boxed_str())
}

/// Runs the probe on the blocking pool, as the API does.
async fn run(random: Box<dyn Random>, budget: Duration, t: ProbeTarget<'static>) -> Verified {
    let handle = Handle::current();
    tokio::task::spawn_blocking(move || S3Probe::with(handle, random, budget).probe(&t))
        .await
        .expect("the probe")
}

async fn probe_of(script: Script, conditional: bool) -> (Verified, Vec<String>) {
    let store = Store::start(script).await;
    let t = ProbeTarget {
        conditional_put: conditional,
        ..target(leaked(&store.endpoint), secret())
    };
    let verified = run(Box::new(Fixed), Duration::from_secs(10), t).await;
    (verified, store.seen())
}

const KEY: &str = "/farm-bucket/cold/.kbf-probe/abababababababab";

// Catches a probe that reports ok without all three steps, one that leaves its object
// behind, and one that writes outside `<prefix>.kbf-probe/`: the requests are exactly a
// PUT, a ranged GET and a DELETE of one key under the probe directory.
#[tokio::test(flavor = "multi_thread")]
async fn a_good_store_passes_every_step_and_the_object_is_removed() {
    let (v, seen) = probe_of(Script::OK, false).await;
    assert_eq!(
        v,
        Verified {
            ok: true,
            put: true,
            get: true,
            delete: true,
            conditional_put: false
        }
    );
    assert_eq!(
        seen,
        [
            format!("put {KEY} false"),
            format!("get {KEY} false"),
            format!("delete {KEY} false")
        ]
    );
}

// Catches a conditional-put check that is skipped (a store that overwrites would pass), one
// that runs when the connection did not declare it, and one that counts a second write that
// succeeded as proof.
#[tokio::test(flavor = "multi_thread")]
async fn the_conditional_put_is_checked_only_when_declared_and_must_be_refused() {
    let (v, seen) = probe_of(Script::OK, true).await;
    assert!(v.ok && v.conditional_put, "{v:?}");
    assert_eq!(seen.len(), 4, "{seen:?}");
    assert_eq!(
        seen[2],
        format!("put {KEY} true"),
        "the second write: {seen:?}"
    );

    let overwriting = Script {
        second_put: 200,
        ..Script::OK
    };
    let (v, _) = probe_of(overwriting, true).await;
    assert!(
        !v.ok && !v.conditional_put && v.put && v.get && v.delete,
        "{v:?}"
    );
    // The same store passes when the connection does not claim conditional writes.
    let (v, _) = probe_of(overwriting, false).await;
    assert!(v.ok && !v.conditional_put, "{v:?}");
}

// Catches a PUT failure that goes on to read and delete, a GET that is believed without
// comparing the bytes, a failed GET that skips the delete (the object would stay), and a
// failed DELETE reported as ok.
#[tokio::test(flavor = "multi_thread")]
async fn each_failing_step_is_named_and_a_written_object_is_always_deleted() {
    let (v, seen) = probe_of(
        Script {
            first_put: 403,
            ..Script::OK
        },
        false,
    )
    .await;
    assert_eq!(v, Verified::default());
    assert_eq!(
        seen.len(),
        1,
        "nothing else is asked once the write failed: {seen:?}"
    );

    let (v, seen) = probe_of(
        Script {
            get: 500,
            ..Script::OK
        },
        false,
    )
    .await;
    assert!(v.put && !v.get && v.delete && !v.ok, "{v:?}");
    assert!(
        seen.last().is_some_and(|l| l.starts_with("delete")),
        "{seen:?}"
    );

    let (v, _) = probe_of(
        Script {
            wrong_bytes: true,
            ..Script::OK
        },
        false,
    )
    .await;
    assert!(v.put && !v.get && v.delete && !v.ok, "{v:?}");

    let (v, _) = probe_of(
        Script {
            delete: 403,
            ..Script::OK
        },
        false,
    )
    .await;
    assert!(v.put && v.get && !v.delete && !v.ok, "{v:?}");
}

// Catches a probe that waits for a store that never answers (it would hold the request and
// a blocking thread), and one that loses the steps it had passed when it was cut off.
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_store_is_cut_off_within_the_budget() {
    let store = Store::start(Script {
        silent: true,
        ..Script::OK
    })
    .await;
    let began = std::time::Instant::now();
    let t = target(leaked(&store.endpoint), secret());
    let v = run(Box::new(Fixed), Duration::from_millis(300), t).await;
    assert_eq!(v, Verified::default());
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
}

// Catches a refused target (nothing listens, a bucket that is not a bucket, an endpoint with
// a path, no randomness for the probe key, a prefix that is not a key) becoming a panic or an
// error text instead of every step false.
#[tokio::test(flavor = "multi_thread")]
async fn a_target_that_cannot_be_probed_is_every_step_false() {
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        format!("http://{}", l.local_addr().expect("addr"))
    };
    let secret = secret();
    let budget = Duration::from_secs(5);
    let cases: Vec<(Box<dyn Random>, ProbeTarget<'static>)> = vec![
        (Box::new(Fixed), target(leaked(&closed), secret)),
        (
            Box::new(Fixed),
            ProbeTarget {
                bucket: "NOT A BUCKET",
                ..target("http://127.0.0.1:1", secret)
            },
        ),
        (Box::new(Fixed), target("http://127.0.0.1:1/path", secret)),
        (Box::new(NoRandom), target("http://127.0.0.1:1", secret)),
        (
            Box::new(Fixed),
            ProbeTarget {
                prefix: "a/../b",
                ..target("http://127.0.0.1:1", secret)
            },
        ),
    ];
    for (random, t) in cases {
        let v = run(random, budget, t).await;
        assert_eq!(v, Verified::default());
    }
}

// Catches a prefix without its trailing slash gluing the probe directory onto the last
// word (`coldx.kbf-probe`), and an empty prefix writing outside the bucket root.
#[tokio::test(flavor = "multi_thread")]
async fn the_probe_directory_sits_under_the_prefix_whatever_its_spelling() {
    for (prefix, want) in [
        ("cold", "/farm-bucket/cold/.kbf-probe/abababababababab"),
        ("cold/", "/farm-bucket/cold/.kbf-probe/abababababababab"),
        ("", "/farm-bucket/.kbf-probe/abababababababab"),
    ] {
        let store = Store::start(Script::OK).await;
        let t = ProbeTarget {
            prefix,
            ..target(leaked(&store.endpoint), secret())
        };
        let v = run(Box::new(Fixed), Duration::from_secs(10), t).await;
        assert!(v.ok, "{prefix:?}: {v:?}");
        assert_eq!(store.seen()[0], format!("put {want} false"), "{prefix:?}");
    }
}

// Catches an `https` endpoint reaching the S3 client (which refuses it on every node), a
// path, userinfo or query in the endpoint, a bad bucket or prefix, and an error text that
// repeats the endpoint (it may carry a password typed into the URL).
#[test]
fn an_origin_is_checked_the_way_every_node_will_check_it() {
    assert_eq!(
        check_origin("http://s3.example:9000", "farm-bucket", "cold/"),
        Ok(())
    );
    assert_eq!(
        check_origin("http://s3.example:9000", "farm-bucket", ""),
        Ok(())
    );
    for https in [
        "https://s3.example",
        "HTTPS://s3.example",
        "https://u:p@s3.example",
    ] {
        assert_eq!(
            check_origin(https, "farm-bucket", ""),
            Err(OriginFault::Tls),
            "{https}"
        );
    }
    for bad in [
        "ftp://s3.example",
        "s3.example:9000",
        "http://user:hunter2@s3.example",
        "http://s3.example/path",
        "http://s3.example/?q=1",
        "",
    ] {
        let fault = check_origin(bad, "farm-bucket", "").expect_err(bad);
        assert_eq!(fault, OriginFault::Endpoint, "{bad}");
        assert!(!fault.to_string().contains("hunter2"));
    }
    assert_eq!(
        check_origin("http://s3.example", "Not_A_Bucket", ""),
        Err(OriginFault::Bucket)
    );
    assert_eq!(
        check_origin("http://s3.example", "ab", ""),
        Err(OriginFault::Bucket)
    );
    for prefix in ["a/../b", "a//b", "a b", "/lead"] {
        assert_eq!(
            check_origin("http://s3.example", "farm-bucket", prefix),
            Err(OriginFault::Prefix),
            "{prefix:?}"
        );
    }
    for fault in [
        OriginFault::Tls,
        OriginFault::Endpoint,
        OriginFault::Bucket,
        OriginFault::Prefix,
    ] {
        assert!(!fault.to_string().is_empty());
    }
}
