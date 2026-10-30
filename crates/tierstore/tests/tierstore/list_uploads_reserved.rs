//! The proxy's listing of uploads when the cold tier holds uploads of reserved keys: where they
//! sort in a page, a reserved marker, a cold tier with nothing else, and a cold tier whose
//! listing never advances or never ends.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use tierstore::{
    Backend, ByteRange, Error, Listed, MemoryBackend, Meta, Op, OpenUpload, Proxy, ProxyConfig,
    TestClock, UploadsAfter,
};

use crate::support::Rig;

/// The key of each upload.
fn keys_of(uploads: &[OpenUpload]) -> Vec<&str> {
    uploads.iter().map(|upload| upload.key.as_str()).collect()
}

/// A rig whose cold tier holds uploads of the keys `reserved`, made directly (the proxy refuses
/// to start one), and of the keys `plain`, made through the proxy. `A/..` sorts before the
/// reserved prefixes and `a/..` after them.
async fn rig_with(reserved: &[&str], plain: &[&str]) -> (Rig, Proxy) {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1 << 20).await;
    for key in reserved {
        rig.cold_mem.create_upload(key).await.unwrap();
    }
    for key in plain {
        proxy.create_upload(key).await.unwrap();
    }
    (rig, proxy)
}

/// Asserts that a reserved upload in the middle of a page and one at the end of a page are left
/// out, and that the page is still as full as the cold tier allows: `A/1`, `_journal/x`, `a/1`
/// is a page with a reserved entry in the middle; a page of two that ends on `_journal/x` is
/// made up by a second call.
#[tokio::test]
async fn a_reserved_upload_in_the_middle_or_at_the_end_of_a_page_is_left_out() {
    let (rig, proxy) = rig_with(&["_journal/x"], &["A/1", "a/1", "a/2"]).await;
    let middle = proxy.list_uploads("", None, 3).await.unwrap();
    assert_eq!(keys_of(&middle), ["A/1", "a/1", "a/2"]);
    assert_eq!(rig.cold.calls(Op::ListUploads), 2);

    let before = rig.cold.calls(Op::ListUploads);
    let at_end = proxy.list_uploads("", None, 2).await.unwrap();
    assert_eq!(keys_of(&at_end), ["A/1", "a/1"]);
    assert_eq!(rig.cold.calls(Op::ListUploads), before + 2);

    let before = rig.cold.calls(Op::ListUploads);
    let all = proxy.list_uploads("", None, 100).await.unwrap();
    assert_eq!(keys_of(&all), ["A/1", "a/1", "a/2"]);
    assert_eq!(rig.cold.calls(Op::ListUploads), before + 1);
}

/// Asserts that a marker that is itself a reserved key, with an id or without one, is a place
/// to start after like any other: the listing goes on with the keys after it.
#[tokio::test]
async fn a_listing_may_start_after_a_reserved_key() {
    let (rig, proxy) = rig_with(&["_journal/x", "_tiering/y"], &["A/1", "a/1"]).await;
    let reserved = rig
        .cold_mem
        .list_uploads("_journal/", None, 10)
        .await
        .unwrap();
    let id = reserved[0].id.as_str();
    let by_key = proxy.list_uploads("", Some(("_journal/x", None)), 10).await;
    assert_eq!(keys_of(&by_key.unwrap()), ["a/1"]);
    let by_id = proxy
        .list_uploads("", Some(("_journal/x", Some(id))), 10)
        .await;
    assert_eq!(keys_of(&by_id.unwrap()), ["a/1"]);
    let past = proxy.list_uploads("", Some(("_tiering/y", None)), 10).await;
    assert_eq!(keys_of(&past.unwrap()), ["a/1"]);
}

/// Asserts that a cold tier with nothing but reserved uploads gives an empty listing, after as
/// many calls as it takes to see them all: five reserved uploads and a page of two are three
/// calls, the last of them short.
#[tokio::test]
async fn a_cold_tier_with_only_reserved_uploads_lists_nothing_after_several_refills() {
    let keys = [
        "_journal/0",
        "_journal/1",
        "_journal/2",
        "_tiering/a",
        "_tiering/b",
    ];
    let (rig, proxy) = rig_with(&keys, &[]).await;
    let listed = proxy.list_uploads("", None, 2).await.unwrap();
    assert!(listed.is_empty());
    assert_eq!(rig.cold.calls(Op::ListUploads), 3);
}

/// A cold tier that holds nothing but answers every listing of uploads with a full page of
/// reserved keys. With `advance` each page is after the one before; without it, every page is
/// the same one.
struct Endless {
    advance: bool,
    calls: AtomicU64,
}

impl Endless {
    fn new(advance: bool) -> Arc<Self> {
        Arc::new(Self {
            advance,
            calls: AtomicU64::new(0),
        })
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Backend for Endless {
    async fn put(&self, key: &str, _body: Bytes) -> Result<(), Error> {
        Err(Error::Full(key.to_owned()))
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

    async fn list_uploads(
        &self,
        _prefix: &str,
        _after: Option<UploadsAfter<'_>>,
        max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let round = if self.advance { call } else { 0 };
        let page = (0..max_uploads).map(|index| OpenUpload {
            key: format!("_tiering/{round:08}-{index:08}"),
            id: "u".to_owned(),
            initiated: 0,
        });
        Ok(page.collect())
    }
}

/// A proxy over an empty hot tier and this cold tier.
async fn over(cold: Arc<Endless>) -> Proxy {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(clock.clone()));
    let config = ProxyConfig::new("p1", 1 << 20, clock);
    Proxy::open(hot, cold, config).await.unwrap()
}

/// Asserts that a cold tier that answers with the same full page of reserved keys for ever gets
/// an `Unavailable` error on the second call, and not a loop; and that one whose pages do
/// advance but hold nothing but reserved keys is cut at a fixed number of calls, also with
/// `Unavailable`. It catches a refill loop that never ends.
#[tokio::test]
async fn a_cold_tier_that_never_advances_or_never_ends_is_an_error_and_not_a_hang() {
    let stuck = Endless::new(false);
    let proxy = over(stuck.clone()).await;
    let error = proxy.list_uploads("", None, 3).await.unwrap_err();
    assert!(error.is_unavailable(), "{error}");
    assert!(error.to_string().contains("did not advance"), "{error}");
    assert_eq!(stuck.calls(), 2);

    let endless = Endless::new(true);
    let proxy = over(endless.clone()).await;
    let error = proxy.list_uploads("", None, 3).await.unwrap_err();
    assert!(error.is_unavailable(), "{error}");
    assert!(error.to_string().contains("took over"), "{error}");
    assert_eq!(endless.calls(), 100);
}
