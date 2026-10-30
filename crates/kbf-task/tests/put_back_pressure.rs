//! The real PUT client of `kbf-task` against a real tierstore front over loopback, with the
//! front's put cap in use: a large body that the front refuses with 503 before it reads it must
//! end in the documented outcome, a retry, and not in a final transport error.
//!
//! A front that answers 503 and closes without reading a body that does not fit the socket
//! buffers can reset the connection under a client that is still sending, so the client may see
//! a broken connection instead of the 503. Whichever it sees, it must retry. Nothing here measures
//! time: the sleeper records the waits and does not wait, and the front's own wait for a permit is
//! switched off (`put_permit_wait` of zero).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_task::put::{Jitter, MAX_RETRIES, PutError, Sleeper, agent, put, put_reader, put_retrying};
use tierstore::server::{
    AccessKey, Grant, PresignKey, PresignRequest, Server, ServerConfig, presign_put,
};
use tierstore::{Backend, MemoryBackend, SystemClock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const BUCKET: &str = "tier-bucket";
const PRESIGN_ID: &str = "AKIDPRESIGN";
const PRESIGN_SECRET: &str = "presign-secret";

/// More than the socket buffers of a loopback connection hold, so that a client sending it is
/// still writing when the front has answered.
const BIG: usize = 8 << 20;

/// A sleeper that records the waits it was asked for and does not wait.
#[derive(Default)]
struct Recorded(std::sync::Mutex<Vec<Duration>>);

impl Sleeper for Recorded {
    fn sleep(&self, wait: Duration) {
        self.0.lock().expect("lock").push(wait);
    }
}

/// A jitter that leaves a wait as it is.
struct Middle;

impl Jitter for Middle {
    fn unit(&self) -> f64 {
        0.5
    }
}

/// A front on a loopback port over memory, with `permits` put slots and no wait for one.
struct Front {
    addr: SocketAddr,
    backend: Arc<MemoryBackend>,
}

impl Front {
    async fn start(permits: usize) -> Self {
        let grant = Grant {
            prefix: "cas/b/".to_owned(),
            max_expires_secs: 600,
            max_object_bytes: 64 << 20,
        };
        let key = PresignKey::new(PRESIGN_ID, PRESIGN_SECRET, grant).expect("a presigning key");
        let mut config =
            ServerConfig::new(BUCKET, "AKIDFRONT", "front-secret", Arc::new(SystemClock));
        config.presign_keys = vec![key];
        config.max_concurrent_puts = permits;
        config.put_permit_wait = Duration::ZERO;
        let backend = Arc::new(MemoryBackend::new());
        let held: Arc<dyn Backend> = backend.clone();
        let server = Server::new(held, config);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bound");
        let addr = listener.local_addr().expect("an address");
        tokio::spawn(async move { server.serve(listener).await });
        Self { addr, backend }
    }

    /// A presigned PUT URL for `name` under the prefix of the presigning key.
    fn url(&self, name: &str) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch");
        let request = PresignRequest {
            endpoint: &format!("http://{}", self.addr),
            region: "us-east-1",
            bucket: BUCKET,
            key: &format!("cas/b/{name}"),
            expires_secs: 300,
            now: u64::try_from(now.as_millis()).expect("a time"),
        };
        presign_put(&AccessKey::new(PRESIGN_ID, PRESIGN_SECRET), &request).expect("a URL")
    }
}

/// `len` bytes that differ from place to place.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// Asserts that a body of several MiB, sent by the real client to a front whose put slots are all
/// taken, is retried `MAX_RETRIES` times with a wait before each, by the one-shot path (`put`)
/// and by the streamed path (`put_reader`), and that what it ends in is the 503 or the broken
/// connection a refused large body can show, never `Transport`, which is final at the first
/// attempt. Nothing is stored. (Which of the two the last attempt shows depends on the timing of
/// the reset, so the test accepts either.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_put_refused_at_the_cap_is_retried_and_does_not_end_as_a_transport_error() {
    let front = Front::start(0).await;
    for streamed in [false, true] {
        let url = front.url(if streamed { "streamed" } else { "whole" });
        let (outcome, attempts, waits) = tokio::task::spawn_blocking(move || {
            let body = pattern(BIG);
            let client = agent(None);
            let sleeper = Recorded::default();
            let mut attempts = 0_usize;
            let outcome = put_retrying(
                || {
                    attempts += 1;
                    if streamed {
                        put_reader(&client, &url, BIG as u64, &mut body.as_slice())
                    } else {
                        put(&client, &url, &body)
                    }
                },
                &sleeper,
                &Middle,
            );
            let waits = sleeper.0.lock().expect("lock").len();
            (outcome, attempts, waits)
        })
        .await
        .expect("the client");
        let error = outcome.expect_err("the slots stay taken");
        assert!(
            matches!(
                error,
                PutError::Unavailable {
                    retry_after_secs: Some(1),
                    ..
                } | PutError::Interrupted { .. }
            ),
            "streamed={streamed}: {error:?}"
        );
        assert_eq!(attempts, MAX_RETRIES as usize + 1, "streamed={streamed}");
        assert_eq!(waits, MAX_RETRIES as usize, "streamed={streamed}");
    }
    assert!(front.backend.get("cas/b/whole", None).await.is_err());
    assert!(front.backend.get("cas/b/streamed", None).await.is_err());
}

/// Asserts the way out: with the only put slot held by a request that has sent part of its body,
/// the real client's first attempt with a large body is refused, the wait before its retry is
/// where the holder is let finish (its answer is read before the wait ends), and the retry is
/// stored whole: the object is the body that was sent. The client was refused at least once, so
/// the store was not simply idle when it came.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_put_refused_at_the_cap_is_stored_by_its_retry_once_a_slot_frees() {
    let front = Front::start(1).await;
    let holder_url = front.url("holder");
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let addr = front.addr;
    let path = holder_url
        .split_once(&format!("{addr}"))
        .expect("the URL names the front")
        .1
        .to_owned();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let holder = tokio::spawn(async move {
        let mut stream = TcpStream::connect(addr).await.expect("connected");
        let head = format!(
            "PUT {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 4\r\nIf-None-Match: *\r\n\r\nab"
        );
        stream.write_all(head.as_bytes()).await.expect("sent");
        started_tx.send(()).ok();
        released.await.expect("released");
        stream.write_all(b"cd").await.expect("sent");
        let mut answer = Vec::new();
        let mut piece = [0_u8; 256];
        while !answer.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut piece).await.expect("an answer");
            assert!(n > 0, "the front closed without answering");
            answer.extend_from_slice(&piece[..n]);
        }
        assert!(answer.starts_with(b"HTTP/1.1 200"), "{answer:?}");
        done_tx.send(()).expect("told");
    });
    started_rx.await.expect("the holder started");

    // The holder has sent its head; the front may not yet have taken its slot, so a small put
    // asks until the front refuses one. Each small put that is let in stores an object of its
    // own and frees the slot again.
    let mut probes = 0_usize;
    loop {
        let url = front.url(&format!("probe-{probes}"));
        probes += 1;
        let answer = tokio::task::spawn_blocking(move || put(&agent(None), &url, b"probe"))
            .await
            .expect("the probe");
        match answer {
            Ok(()) => tokio::task::yield_now().await,
            Err(PutError::Unavailable { .. } | PutError::Interrupted { .. }) => break,
            Err(other) => panic!("the probe was neither stored nor refused: {other:?}"),
        }
    }

    let url = front.url("big");
    let releasing = std::sync::Mutex::new(Some(release));
    struct Release<'a> {
        release: &'a std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        done: mpsc::Receiver<()>,
        waits: std::sync::Mutex<usize>,
    }
    impl Sleeper for Release<'_> {
        fn sleep(&self, _wait: Duration) {
            *self.waits.lock().expect("lock") += 1;
            if let Some(release) = self.release.lock().expect("lock").take() {
                release.send(()).expect("the holder waits");
                // The holder's answer is its slot given back.
                self.done.recv().expect("the holder finished");
            }
        }
    }
    let (outcome, attempts, waits) = tokio::task::spawn_blocking(move || {
        let sleeper = Release {
            release: &releasing,
            done: done_rx,
            waits: std::sync::Mutex::new(0),
        };
        let body = pattern(BIG);
        let client = agent(None);
        let mut attempts = 0_usize;
        let outcome = put_retrying(
            || {
                attempts += 1;
                put(&client, &url, &body)
            },
            &sleeper,
            &Middle,
        );
        let waits = *sleeper.waits.lock().expect("lock");
        (outcome, attempts, waits)
    })
    .await
    .expect("the client");

    assert_eq!(outcome, Ok(()));
    assert!(attempts >= 2, "the first attempt was not refused");
    assert_eq!(waits, attempts - 1);
    holder.await.expect("the holder");
    let stored = front.backend.get("cas/b/big", None).await.expect("stored");
    assert!(
        stored.as_ref() == pattern(BIG).as_slice(),
        "not the body that was sent"
    );
    let held = front
        .backend
        .get("cas/b/holder", None)
        .await
        .expect("stored");
    assert_eq!(held.as_ref(), b"abcd");
}
