//! The streamed put through `Server::handle`: the body of a put is not held whole, and the
//! object is stored, and journaled, only after the last piece was read and every hash came out
//! right. The tests over directories run on the real clock, since the file calls of a directory
//! backend are threads of the runtime that a paused clock does not wait for; nothing in them
//! measures time.

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use futures::StreamExt;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame};
use hyper::{Request, StatusCode};
use kbf_testkit::dir::TestDir;
use sha2::{Digest, Sha256};

use crate::journal::is_reserved;
use crate::{
    Backend, ByteRange, DirBackend, Error, JournalEntry, JournalOp, Listed, Meta, Op, PartBody,
    Proxy, ProxyConfig, TestClock, decode_journal,
};

use super::auth::{sign, sign_claiming};
use super::handover::PIECES_QUEUED;
use super::request::BoxError;
use super::testing::{BUCKET, KEY_ID, NOW, Reply, Rig, SECRET, Scripted, Step};
use super::{Server, ServerConfig};

/// The size of a piece of a generated body.
const PIECE: usize = 32 << 10;

/// The piece `n` of a generated body: `PIECE` bytes that depend on `n`, with the last one
/// changed if `flip` says so.
fn piece(n: usize, flip: bool) -> Bytes {
    let mut bytes = vec![(n % 251) as u8; PIECE];
    if flip {
        bytes[PIECE - 1] ^= 0xff;
    }
    Bytes::from(bytes)
}

/// The SHA-256 (hex) and the CRC-32C (base64) of a generated body of `pieces` pieces.
fn truth(pieces: usize) -> (String, String) {
    let mut sha = Sha256::new();
    let mut crc = 0_u32;
    for n in 0..pieces {
        let bytes = piece(n, false);
        sha.update(&bytes);
        crc = crc32c::crc32c_append(crc, &bytes);
    }
    (
        hex::encode(sha.finalize()),
        STANDARD.encode(crc.to_be_bytes()),
    )
}

/// A request body of `pieces` pieces, each made when it is asked for and not before, so that the
/// body is never in memory whole; `made` counts the pieces made.
struct Generated {
    next: usize,
    pieces: usize,
    flip_last: bool,
    made: Arc<AtomicUsize>,
}

impl Generated {
    fn new(pieces: usize, made: &Arc<AtomicUsize>) -> Self {
        Self {
            next: 0,
            pieces,
            flip_last: false,
            made: Arc::clone(made),
        }
    }

    /// The same, with the last byte of the last piece not the one the hash was made of.
    fn flipped(pieces: usize) -> Self {
        let mut body = Self::new(pieces, &Arc::default());
        body.flip_last = true;
        body
    }
}

impl Body for Generated {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        let this = self.get_mut();
        if this.next == this.pieces {
            return Poll::Ready(None);
        }
        let last = this.next + 1 == this.pieces;
        let bytes = piece(this.next, this.flip_last && last);
        this.next += 1;
        this.made.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(Some(Ok(Frame::data(bytes))))
    }
}

/// A put of `key` that claims `claim` as the hash of its body, signed for the server of
/// `config`, whose body is the one given.
fn put_request<B>(
    config: &ServerConfig,
    key: &str,
    claim: &str,
    extra: &[(&str, &str)],
    body: B,
) -> Request<B> {
    let uri = format!("/{BUCKET}/{key}");
    let who = (
        config.access_key_id.as_str(),
        config.secret_access_key.as_str(),
    );
    let signed = sign_claiming(config, who, "PUT", &uri, extra, claim, b"");
    let (parts, _) = signed.into_parts();
    Request::from_parts(parts, body)
}

/// Sends a request to the server and reads the answer.
async fn answer<B>(server: &Server, request: Request<B>) -> Reply
where
    B: Body + Send,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    let response = server.handle(request).await;
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await.expect("a whole body").to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// What a [`Probe`] saw of the puts it was given.
#[derive(Default)]
struct Seen {
    /// The calls of the put that holds its body.
    held_puts: usize,
    /// The lengths the streamed puts were told.
    lengths: Vec<Option<u64>>,
    pieces: usize,
    bytes: u64,
    /// The most pieces that the body had made and the backend had not yet taken.
    ahead: usize,
}

/// A backend that takes the body of a streamed put and keeps nothing of it, and that takes it
/// slowly: it lets the other tasks run between two pieces, so that a front that did not hold
/// its reading back would make many pieces ahead of it.
struct Probe {
    made: Arc<AtomicUsize>,
    seen: Arc<Mutex<Seen>>,
}

#[async_trait]
impl Backend for Probe {
    async fn put(&self, _key: &str, _body: Bytes) -> Result<(), Error> {
        self.seen.lock().unwrap().held_puts += 1;
        Err(Error::Other("a put that holds the body".to_owned()))
    }

    async fn put_stream(
        &self,
        _key: &str,
        len: Option<u64>,
        mut body: PartBody<'_>,
    ) -> Result<(), Error> {
        let (mut pieces, mut bytes, mut ahead) = (0_usize, 0_u64, 0_usize);
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            pieces += 1;
            bytes += chunk.len() as u64;
            let made = self.made.load(Ordering::SeqCst);
            ahead = ahead.max(made.saturating_sub(pieces));
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
        }
        let mut seen = self.seen.lock().unwrap();
        seen.lengths.push(len);
        seen.pieces += pieces;
        seen.bytes += bytes;
        seen.ahead = seen.ahead.max(ahead);
        Ok(())
    }

    async fn put_if_match(&self, key: &str, _body: Bytes, _etag: &str) -> Result<String, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn get(&self, key: &str, _range: Option<ByteRange>) -> Result<Bytes, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn delete(&self, _key: &str) -> Result<(), Error> {
        Ok(())
    }

    async fn list(&self, _prefix: &str) -> Result<Vec<Listed>, Error> {
        Ok(Vec::new())
    }
}

const _: () = assert!(256 > PIECES_QUEUED + 2);

/// Asserts that a body of 8 MiB, made piece by piece, is taken by the front with a queue of a few
/// pieces and not held: the backend is given it by its streamed put, with the length of the
/// request if the request has one, and its held put is never called; the pieces the body had made
/// and the backend had not yet taken never went over the queue and the two pieces in hand,
/// although the body has far more pieces than that. The limit of the request is the length of the
/// body, so the limit bounds a length and not memory.
#[tokio::test(start_paused = true)]
async fn a_large_body_is_streamed_through_a_bounded_queue() {
    const PIECES: usize = 256;
    let total = (PIECES * PIECE) as u64;
    let made = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Seen::default()));
    let probe = Probe {
        made: Arc::clone(&made),
        seen: Arc::clone(&seen),
    };
    let clock = Arc::new(TestClock::new());
    clock.set(NOW);
    let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, clock);
    config.max_request_bytes = total as usize;
    let server = Server::new(Arc::new(probe), config.clone());
    let (claim, _) = truth(PIECES);

    let length = total.to_string();
    let told = [("content-length", length.as_str())];
    let body = Generated::new(PIECES, &made);
    let with_length = answer(&server, put_request(&config, "told", &claim, &told, body)).await;
    assert_eq!(with_length.status, StatusCode::OK);
    made.store(0, Ordering::SeqCst);
    let body = Generated::new(PIECES, &made);
    let without = answer(&server, put_request(&config, "untold", &claim, &[], body)).await;
    assert_eq!(without.status, StatusCode::OK);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.held_puts, 0);
    assert_eq!(seen.lengths, [Some(total), None]);
    assert_eq!(seen.pieces, 2 * PIECES);
    assert_eq!(seen.bytes, 2 * total);
    assert!(
        seen.ahead <= PIECES_QUEUED + 2,
        "{} pieces ahead",
        seen.ahead
    );
}

/// Two directories as the tiers of a proxy, and the server over it.
struct Tiers {
    hot_dir: TestDir,
    cold_dir: TestDir,
    hot: Arc<DirBackend>,
    cold: Arc<DirBackend>,
    server: Arc<Server>,
    config: ServerConfig,
}

impl Tiers {
    /// Tiers whose server takes a body of `limit` bytes at most.
    async fn new(limit: usize) -> Self {
        let clock = Arc::new(TestClock::new());
        clock.set(NOW);
        let hot_dir = TestDir::under(std::env::temp_dir(), "tierstore-stream-hot");
        let cold_dir = TestDir::under(std::env::temp_dir(), "tierstore-stream-cold");
        let hot = Arc::new(DirBackend::open(hot_dir.path()).unwrap());
        let cold = Arc::new(DirBackend::open(cold_dir.path()).unwrap());
        let mut proxy_config = ProxyConfig::new("p1", 1 << 40, clock.clone());
        proxy_config.linger = Duration::from_millis(1);
        let proxy = Proxy::open(hot.clone(), cold.clone(), proxy_config).await;
        let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, clock);
        config.max_request_bytes = limit;
        let server = Server::new(Arc::new(proxy.unwrap()), config.clone());
        Self {
            hot_dir,
            cold_dir,
            hot,
            cold,
            server,
            config,
        }
    }

    /// The keys of the client's objects in a tier: those the proxy does not keep for itself.
    async fn data_keys(tier: &DirBackend) -> Vec<String> {
        let listed = tier.list("").await.unwrap();
        let keys = listed.into_iter().map(|object| object.key);
        keys.filter(|key| !is_reserved(key)).collect()
    }

    /// The entries of the journal of the proxy, in order.
    async fn journal(&self) -> Vec<JournalEntry> {
        let mut entries = Vec::new();
        for object in self.hot.list("_journal/p1/").await.unwrap() {
            let bytes = self.hot.get(&object.key, None).await.unwrap();
            entries.extend(decode_journal(&bytes).unwrap());
        }
        entries
    }

    /// Whether a directory holds the temporary file of a put that is not over.
    fn has_temporary_file(dir: &TestDir) -> bool {
        let names = std::fs::read_dir(dir.path()).unwrap();
        let mut names = names.map(|entry| entry.unwrap().file_name());
        names.any(|name| name.to_string_lossy().starts_with(".tmp-"))
    }

    /// Asserts that no object of the client is in either tier, nothing is journaled, and no
    /// temporary file is left.
    async fn assert_nothing_stored(&self) {
        assert_eq!(Self::data_keys(&self.hot).await, Vec::<String>::new());
        assert_eq!(Self::data_keys(&self.cold).await, Vec::<String>::new());
        assert_eq!(self.journal().await, Vec::<JournalEntry>::new());
        assert!(!Self::has_temporary_file(&self.hot_dir));
        assert!(!Self::has_temporary_file(&self.cold_dir));
    }
}

/// Asserts that a body whose hash fails at its very last byte is refused 400 and leaves nothing
/// visible: no object in either tier, no temporary file and no journal entry, whether the hash
/// is the one the signature covers or a checksum header; and that the body the hashes are right
/// for is then stored under the same key, with one journal entry, put, of its length.
#[tokio::test]
async fn a_hash_that_fails_at_the_last_byte_leaves_nothing_visible() {
    const PIECES: usize = 8;
    let tiers = Tiers::new(256 << 20).await;
    let (claim, crc) = truth(PIECES);

    let signed = put_request(&tiers.config, "k", &claim, &[], Generated::flipped(PIECES));
    let refused = answer(&tiers.server, signed).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.code(), "XAmzContentSHA256Mismatch");
    tiers.assert_nothing_stored().await;

    let header = [("x-amz-checksum-crc32c", crc.as_str())];
    let unsigned = "UNSIGNED-PAYLOAD";
    let checked = put_request(
        &tiers.config,
        "k",
        unsigned,
        &header,
        Generated::flipped(PIECES),
    );
    let refused = answer(&tiers.server, checked).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.code(), "BadDigest");
    tiers.assert_nothing_stored().await;

    let made = Arc::default();
    let whole = put_request(
        &tiers.config,
        "k",
        &claim,
        &header,
        Generated::new(PIECES, &made),
    );
    let stored = answer(&tiers.server, whole).await;
    assert_eq!(stored.status, StatusCode::OK);
    let object = tiers.hot.get("k", None).await.unwrap();
    assert_eq!(object.len(), PIECES * PIECE);
    assert_eq!(
        Sha256::digest(&object).as_slice(),
        hex::decode(&claim).unwrap()
    );
    let entry = JournalEntry {
        op: JournalOp::Put,
        key: "k".to_owned(),
        size: (PIECES * PIECE) as u64,
        time: NOW,
    };
    assert_eq!(tiers.journal().await, [entry]);
    assert!(!Tiers::has_temporary_file(&tiers.hot_dir));
}

/// Asserts that a body over the limit of the request is refused 413 and leaves nothing, and a
/// body of exactly the limit is stored.
#[tokio::test]
async fn a_streamed_body_over_the_limit_leaves_nothing_and_one_of_the_limit_is_stored() {
    let tiers = Tiers::new(4 * PIECE).await;
    let made = Arc::default();
    let (claim, _) = truth(5);
    let long = put_request(&tiers.config, "k", &claim, &[], Generated::new(5, &made));
    let refused = answer(&tiers.server, long).await;
    assert_eq!(refused.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(refused.code(), "EntityTooLarge");
    tiers.assert_nothing_stored().await;

    let (claim, _) = truth(4);
    let exact = put_request(&tiers.config, "k", &claim, &[], Generated::new(4, &made));
    assert_eq!(answer(&tiers.server, exact).await.status, StatusCode::OK);
    assert_eq!(Tiers::data_keys(&tiers.hot).await, ["k"]);
}

/// Asserts that a put of a key that exists is refused 412 whichever tier holds it, that the
/// first object and its journal entry are all there is afterwards, and that a key the cold tier
/// holds is refused without the body being read to its end: the pieces the body made are no more
/// than the queue and the pieces in hand.
#[tokio::test]
async fn create_only_refuses_an_existing_key_and_stops_reading_a_body_it_will_not_store() {
    let tiers = Tiers::new(256 << 20).await;
    let made = Arc::default();
    let (claim, _) = truth(2);
    let first = put_request(&tiers.config, "k", &claim, &[], Generated::new(2, &made));
    assert_eq!(answer(&tiers.server, first).await.status, StatusCode::OK);
    let (claim, _) = truth(6);
    let second = put_request(&tiers.config, "k", &claim, &[], Generated::new(6, &made));
    let again = answer(&tiers.server, second).await;
    assert_eq!(again.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(again.code(), "PreconditionFailed");
    let kept = tiers.hot.get("k", None).await.unwrap();
    assert_eq!(kept.len(), 2 * PIECE);
    assert_eq!(tiers.journal().await.len(), 1);

    tiers
        .cold
        .put("c", Bytes::from_static(b"old"))
        .await
        .unwrap();
    let read = Arc::new(AtomicUsize::new(0));
    let (claim, _) = truth(100);
    let third = put_request(&tiers.config, "c", &claim, &[], Generated::new(100, &read));
    let refused = answer(&tiers.server, third).await;
    assert_eq!(refused.status, StatusCode::PRECONDITION_FAILED);
    assert!(read.load(Ordering::SeqCst) <= PIECES_QUEUED + 2);
    assert_eq!(tiers.cold.get("c", None).await.unwrap(), "old");
    assert_eq!(Tiers::data_keys(&tiers.hot).await, ["k"]);
    assert_eq!(tiers.journal().await.len(), 1);
    assert!(!Tiers::has_temporary_file(&tiers.hot_dir));
}

/// Asserts that a body that fails half way is answered 400 `IncompleteBody`, and that a request
/// dropped half way (the client went away, the body still to come) ends the stream with an error
/// and not with its end: in both nothing is stored in either tier and nothing is journaled.
#[tokio::test(start_paused = true)]
async fn a_body_cut_by_an_error_or_by_the_client_stores_nothing() {
    let rig = Rig::new(1 << 20).await;
    let uri = format!("/{BUCKET}/k");
    let steps = vec![Step::Data(b"ab"), Step::Fail];
    let cut = rig
        .send_played("PUT", &uri, &[], b"abcd", Scripted::new(steps))
        .await;
    assert_eq!(cut.status, StatusCode::BAD_REQUEST);
    assert_eq!(cut.code(), "IncompleteBody");

    let steps = vec![Step::Data(b"ab"), Step::Wait(Duration::from_secs(1000))];
    let (parts, _) = sign(&rig.config, "PUT", &uri, &[], b"abcd").into_parts();
    let request = Request::from_parts(parts, Scripted::new(steps));
    let waited = tokio::time::timeout(Duration::from_secs(5), rig.server.handle(request)).await;
    assert!(waited.is_err());
    tokio::time::sleep(Duration::from_secs(60)).await;

    for tier in [&rig.hot_mem, &rig.cold_mem] {
        let held = tier.list("k").await.unwrap();
        assert_eq!(held, Vec::<Listed>::new());
    }
    let journals = rig.hot_mem.list("_journal/").await.unwrap();
    assert_eq!(journals, Vec::<Listed>::new());
}

/// Asserts that a hot tier with no room for the first byte of a streamed put sends the whole
/// body to the cold tier: the put succeeds, the object is in the cold tier only and nothing is
/// journaled.
#[tokio::test(start_paused = true)]
async fn a_hot_tier_with_no_room_sends_the_streamed_body_to_the_cold_tier() {
    let rig = Rig::new(1 << 20).await;
    rig.hot
        .fail_next_on(Op::Put, "k", Error::Full("hot".to_owned()));

    let stored = rig.put("k", b"value").await;

    assert_eq!(stored.status, StatusCode::OK);
    assert_eq!(rig.cold_mem.get("k", None).await.unwrap(), "value");
    assert!(rig.hot_mem.get("k", None).await.is_err());
    let journals = rig.hot_mem.list("_journal/").await.unwrap();
    assert_eq!(journals, Vec::<Listed>::new());
}
