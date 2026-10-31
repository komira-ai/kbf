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
    server: Arc<Server>,
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
        let serving = Arc::clone(&server);
        tokio::spawn(async move { serving.serve(listener).await });
        Self {
            addr,
            backend,
            server,
        }
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

    // The holder has sent its head, but the front takes its slot a moment later, on its own
    // task. The front says when it has: its free put slots go to zero. Nothing is sent to the
    // front meanwhile, so no connection is spent on asking; the loop yields to the front's tasks
    // and is bounded, so a slot that is never taken fails the test with a message.
    let mut tries = 0_u32;
    while front.server.put_permits_free() != 0 {
        tries += 1;
        assert!(
            tries <= 1_000_000,
            "the front never took the holder's put slot"
        );
        tokio::task::yield_now().await;
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

/// What the stub saw of its connections.
#[derive(Default)]
struct Stub {
    connections: std::sync::atomic::AtomicUsize,
}

/// Reads from `stream` until the request head is complete, and returns the head's bytes and
/// whatever of the body came with them.
async fn read_head(stream: &mut TcpStream) -> (Vec<u8>, Vec<u8>) {
    let mut seen = Vec::new();
    let mut piece = [0_u8; 16 << 10];
    loop {
        if let Some(end) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
            let body = seen.split_off(end + 4);
            return (seen, body);
        }
        let n = stream.read(&mut piece).await.expect("a request");
        assert!(n > 0, "the client closed before it sent a head");
        seen.extend_from_slice(&piece[..n]);
    }
}

/// Asserts, with a raw stub and no tierstore, that a connection reset under a large body is
/// retried and the retry stored: the stub reads the head and a part of the body of the first
/// connection and resets it (`SO_LINGER` of zero, then closes, which sends an RST), and answers
/// 201 on the second after reading its whole body. The client attempts twice and waits once
/// (the default wait, as a reset names no `Retry-After`), and the stub saw exactly two
/// connections. This is the case the first test above cannot pin, since it accepts either end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_reset_under_a_large_body_is_retried_and_the_retry_is_stored() {
    use std::sync::atomic::Ordering;

    const BODY: usize = 2 << 20;
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bound");
    let addr = listener.local_addr().expect("an address");
    let stub = Arc::new(Stub::default());
    let seen = Arc::clone(&stub);
    let serving = tokio::spawn(async move {
        // The first connection: a part of the body is read, then the connection is reset.
        let (mut first, _) = listener.accept().await.expect("accepted");
        seen.connections.fetch_add(1, Ordering::SeqCst);
        let (_, mut body) = read_head(&mut first).await;
        let mut piece = [0_u8; 16 << 10];
        while body.len() < 64 << 10 {
            let n = first.read(&mut piece).await.expect("a part of the body");
            assert!(n > 0, "the client closed before it sent a body");
            body.extend_from_slice(&piece[..n]);
        }
        #[allow(deprecated)]
        first.set_linger(Some(Duration::ZERO)).expect("linger");
        drop(first);
        // The second: the whole body is read, then 201.
        let (mut second, _) = listener.accept().await.expect("accepted");
        seen.connections.fetch_add(1, Ordering::SeqCst);
        let (head, mut body) = read_head(&mut second).await;
        let head = String::from_utf8(head)
            .expect("a text head")
            .to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .expect("a length")
            .trim()
            .parse()
            .expect("a number");
        while body.len() < length {
            let n = second.read(&mut piece).await.expect("the body");
            assert!(n > 0, "the client closed before it sent the whole body");
            body.extend_from_slice(&piece[..n]);
        }
        second
            .write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .expect("an answer");
        second.shutdown().await.ok();
        body.len()
    });

    let url = format!("http://{addr}/cas/b/run/nonce?X-Sig=secret");
    let (outcome, attempts, waits) = tokio::task::spawn_blocking(move || {
        let body = pattern(BODY);
        let client = agent(None);
        let sleeper = Recorded::default();
        let mut attempts = 0_usize;
        let outcome = put_retrying(
            || {
                attempts += 1;
                put(&client, &url, &body)
            },
            &sleeper,
            &Middle,
        );
        let waits = sleeper.0.lock().expect("lock").clone();
        (outcome, attempts, waits)
    })
    .await
    .expect("the client");

    assert_eq!(outcome, Ok(()));
    assert_eq!(attempts, 2);
    assert_eq!(waits, [kbf_task::put::DEFAULT_WAIT]);
    assert_eq!(serving.await.expect("the stub"), BODY);
    assert_eq!(stub.connections.load(Ordering::SeqCst), 2);
}
