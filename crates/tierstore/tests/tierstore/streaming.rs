//! The streamed put of the proxy: the body arrives as chunks and is never held whole, the object
//! is created only after the last chunk, and a body that fails leaves no object and no journal
//! entry.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{FutureExt, StreamExt, stream};
use tierstore::{
    Backend, ByteRange, Error, JournalEntry, JournalOp, Listed, MemoryBackend, Meta, Op, PartBody,
};

use crate::support::{
    Rig, assert_absent, body, broken, chunks, journal, journal_key, journal_keys, read, set_usage,
};

/// A body that says whether it was asked for a chunk.
fn watched() -> (PartBody<'static>, Arc<AtomicBool>) {
    let asked = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&asked);
    let body = stream::poll_fn(move |_| {
        seen.store(true, Ordering::SeqCst);
        Poll::Ready(None)
    });
    (body.boxed(), asked)
}

fn cut() -> Error {
    Error::Other("cut".to_owned())
}

/// Asserts that a streamed put is acknowledged after the journal object that holds its entry is
/// in the hot tier: when it returns, the whole body is in the hot tier as one object and not in
/// the cold tier, and journal object 1 holds one put entry whose size is the number of bytes
/// the chunks came to, which the caller did not tell.
#[tokio::test(start_paused = true)]
async fn a_streamed_put_is_acknowledged_after_its_journal_entry_is_in_the_hot_tier() {
    let rig = Rig::new();
    rig.clock.set(42);
    let proxy = rig.proxy("p1", 1000).await;

    proxy
        .put_stream("k", None, chunks(&["va", "", "lue"]))
        .await
        .unwrap();

    assert_eq!(read(&*rig.hot_mem, "k").await, body("value"));
    assert_absent(&*rig.cold_mem, "k").await;
    let entries = journal(&*rig.hot_mem, "p1", 1).await;
    let want = JournalEntry {
        op: JournalOp::Put,
        key: "k".to_owned(),
        size: 5,
        time: 42,
    };
    assert_eq!(entries, [want]);
}

/// Asserts that a body that ends with an error after some chunks leaves no object in either
/// tier and no journal object, gives the error of the body, and does not keep the key from a
/// later put of a whole body, which is journaled.
#[tokio::test(start_paused = true)]
async fn a_streamed_put_whose_body_fails_leaves_no_object_and_no_journal_entry() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;

    let failed = proxy
        .put_stream("k", None, broken(&["va", "lu"], &cut()))
        .await;
    assert_eq!(failed, Err(cut()));
    assert_absent(&*rig.hot_mem, "k").await;
    assert_absent(&*rig.cold_mem, "k").await;
    assert_eq!(
        journal_keys(&*rig.hot_mem, "p1").await,
        Vec::<String>::new()
    );

    proxy
        .put_stream("k", None, chunks(&["value"]))
        .await
        .unwrap();
    assert_eq!(read(&*rig.hot_mem, "k").await, body("value"));
    assert_eq!(
        journal_keys(&*rig.hot_mem, "p1").await,
        [journal_key("p1", 1)]
    );
}

/// Asserts that a streamed put is refused with `AlreadyExists` where the cold tier holds the key
/// (the hot tier may have evicted it) before a chunk of the body is asked for, and where the
/// hot tier holds it, and that the first object is kept and the refused puts journal nothing.
#[tokio::test(start_paused = true)]
async fn a_streamed_put_is_refused_where_either_tier_holds_the_key() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("cold", body("old")).await.unwrap();
    proxy.put("hot", body("one")).await.unwrap();

    let (unread, asked) = watched();
    let in_cold = proxy.put_stream("cold", None, unread).await;
    assert_eq!(in_cold, Err(Error::AlreadyExists("cold".to_owned())));
    assert!(!asked.load(Ordering::SeqCst));
    let in_hot = proxy.put_stream("hot", None, chunks(&["two"])).await;
    assert_eq!(in_hot, Err(Error::AlreadyExists("hot".to_owned())));

    assert_eq!(read(&*rig.cold_mem, "cold").await, body("old"));
    assert_absent(&*rig.hot_mem, "cold").await;
    assert_eq!(read(&*rig.hot_mem, "hot").await, body("one"));
    assert_eq!(
        journal_keys(&*rig.hot_mem, "p1").await,
        [journal_key("p1", 1)]
    );
}

/// Asserts that a streamed put to a key the proxy keeps for itself is refused with `Reserved`
/// before a chunk of the body is asked for.
#[tokio::test(start_paused = true)]
async fn a_streamed_put_to_a_reserved_key_is_refused_without_reading_the_body() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let (unread, asked) = watched();

    let refused = proxy.put_stream("_journal/p1/x", None, unread).await;

    assert_eq!(refused, Err(Error::Reserved("_journal/p1/x".to_owned())));
    assert!(!asked.load(Ordering::SeqCst));
}

/// Asserts that while the published usage is at the budget a streamed put goes straight to the
/// cold tier, create-only and unjournaled; that a body that fails there leaves nothing in the
/// cold tier; and that the cold tier is not written where the hot tier cannot say it lacks
/// the key.
#[tokio::test(start_paused = true)]
async fn a_streamed_put_goes_straight_to_the_cold_tier_when_the_hot_tier_is_full() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    set_usage(&rig, 1000, 0).await;

    proxy
        .put_stream("k", Some(6), chunks(&["dir", "ect"]))
        .await
        .unwrap();
    assert_eq!(read(&*rig.cold_mem, "k").await, body("direct"));
    assert_absent(&*rig.hot_mem, "k").await;
    assert_eq!(
        journal_keys(&*rig.hot_mem, "p1").await,
        Vec::<String>::new()
    );

    let failed = proxy.put_stream("f", None, broken(&["di"], &cut())).await;
    assert_eq!(failed, Err(cut()));
    assert_absent(&*rig.cold_mem, "f").await;

    let down = Error::Unavailable("hot".to_owned());
    rig.hot.fail_next_on(Op::Head, "h", down.clone());
    let unsure = proxy.put_stream("h", None, chunks(&["x"])).await;
    assert_eq!(unsure, Err(down));
    assert_absent(&*rig.cold_mem, "h").await;
}

/// Asserts that a streamed put made while the usage is above the critical share is still
/// waiting for the back-pressure when it is polled (nothing is written to the hot tier), and
/// goes to the hot tier once the paused clock is moved past the back-pressure.
#[tokio::test(start_paused = true)]
async fn a_streamed_put_above_the_critical_mark_is_delayed_and_goes_to_the_hot_tier() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.critical = 0.5;
    config.back_pressure = Duration::from_millis(200);
    let proxy = rig.open(config).await.unwrap();
    set_usage(&rig, 600, 0).await;

    let mut putting = Box::pin(proxy.put_stream("b", None, chunks(&["2"])));
    assert!((&mut putting).now_or_never().is_none());
    assert_absent(&*rig.hot_mem, "b").await;
    tokio::time::advance(Duration::from_millis(200)).await;
    putting.await.unwrap();

    assert_eq!(read(&*rig.hot_mem, "b").await, body("2"));
}

/// Asserts that when the hot tier answers a streamed put with a capacity error before it took a
/// byte, the whole body goes to the cold tier and the put succeeds, and the next puts go there
/// without asking the hot tier.
#[tokio::test(start_paused = true)]
async fn a_hot_tier_with_no_room_for_the_first_byte_sends_a_streamed_put_to_the_cold_tier() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.usage_refresh = Duration::from_secs(10);
    let proxy = rig.open(config).await.unwrap();
    rig.hot
        .fail_next_on(Op::Put, "k1", Error::Full("hot".to_owned()));

    proxy
        .put_stream("k1", None, chunks(&["1", "1"]))
        .await
        .unwrap();
    assert_eq!(read(&*rig.cold_mem, "k1").await, body("11"));
    assert_absent(&*rig.hot_mem, "k1").await;
    let hot_puts = rig.hot.calls(Op::Put);

    proxy.put_stream("k2", None, chunks(&["2"])).await.unwrap();
    assert_eq!(read(&*rig.cold_mem, "k2").await, body("2"));
    assert_eq!(rig.hot.calls(Op::Put), hot_puts);
    assert_eq!(
        journal_keys(&*rig.hot_mem, "p1").await,
        Vec::<String>::new()
    );
}

/// A hot tier that takes the first chunk of a streamed put and then has no room.
struct RunsOut(MemoryBackend);

#[async_trait]
impl Backend for RunsOut {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.0.put(key, body).await
    }

    async fn put_stream(
        &self,
        _key: &str,
        _len: Option<u64>,
        mut body: PartBody<'_>,
    ) -> Result<(), Error> {
        let _first = body.next().await;
        Err(Error::Full("hot".to_owned()))
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.0.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.0.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.0.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.0.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.0.list(prefix).await
    }
}

/// Asserts that when the hot tier runs out of room after it took part of the body, the put fails
/// with the capacity error (the bytes taken cannot be offered to the cold tier), nothing is
/// written to the cold tier, nothing is journaled, and the next put goes to the cold tier.
#[tokio::test(start_paused = true)]
async fn a_hot_tier_that_runs_out_after_taking_bytes_fails_the_put_and_latches_full() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.usage_refresh = Duration::from_secs(10);
    let hot = Arc::new(RunsOut(MemoryBackend::new()));
    let proxy = tierstore::Proxy::open(hot.clone(), rig.cold.clone(), config)
        .await
        .unwrap();

    let failed = proxy.put_stream("k", None, chunks(&["ab", "cd"])).await;
    assert_eq!(failed, Err(Error::Full("hot".to_owned())));
    assert_absent(&*rig.cold_mem, "k").await;
    assert_eq!(journal_keys(&*hot, "p1").await, Vec::<String>::new());

    proxy.put_stream("j", None, chunks(&["ef"])).await.unwrap();
    assert_eq!(read(&*rig.cold_mem, "j").await, body("ef"));
}
