//! The listings of a cache leave out the keys a storage node keeps for itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use kbf_testkit::dir::TestDir;

use super::listing::LISTING_CALLS;
use super::testing::{FakeDisk, Rig};
use super::{CacheConfig, LocalCache};
use crate::{Backend, ByteRange, Error, Listed, MemoryBackend, Meta, Op, TestClock};

/// The keys of a listing.
fn keys(listed: &[Listed]) -> Vec<&str> {
    listed.iter().map(|object| object.key.as_str()).collect()
}

/// A cache over an upstream that holds data keys and, between them in key order, keys under
/// `_journal/` and `_tiering/`: the shape of a bucket that a storage node also uses.
async fn rig(name: &str) -> Rig {
    let rig = Rig::new(name);
    let all = [
        "A/1",
        "_journal/p1/0000000001",
        "_journal/p1/0000000002",
        "_tiering/lease",
        "_tiering/usage",
        "b/1",
        "b/2",
        "b/3",
    ];
    for key in all {
        rig.put(key, b"x").await;
    }
    rig
}

/// Asserts that `list` leaves out the keys under `_journal/` and `_tiering/` for the whole
/// bucket, for the prefix of a reserved name, and for a prefix that holds data only; and that
/// the listing is the upstream's otherwise. It catches a listing that passes the upstream's
/// reserved names to a client.
#[tokio::test]
async fn list_leaves_out_the_reserved_keys() {
    let rig = rig("tscache-list-all").await;
    let all = rig.cache.list("").await.unwrap();
    assert_eq!(keys(&all), ["A/1", "b/1", "b/2", "b/3"]);
    assert!(rig.cache.list("_journal/").await.unwrap().is_empty());
    assert!(rig.cache.list("_tiering/").await.unwrap().is_empty());
    assert!(rig.cache.list("_").await.unwrap().is_empty());
    let under_b = rig.cache.list("b/").await.unwrap();
    assert_eq!(keys(&under_b), ["b/1", "b/2", "b/3"]);
}

/// Asserts that `list_page` leaves out the reserved keys and still gives full pages: a page of
/// two that begins before the reserved keys is made up from the keys after them, a page that
/// begins inside them starts at the first data key, the last key of a page continues the
/// listing, and a page of fewer than the asked keys is the last. It catches a filter that cuts
/// a page short (a caller takes a short page for the last, and would stop at `_journal/`) and
/// one that repeats or skips a key at the border of two pages.
#[tokio::test]
async fn list_page_leaves_out_the_reserved_keys_and_keeps_pages_full() {
    let rig = rig("tscache-list-page").await;
    let first = rig.cache.list_page("", None, 2).await.unwrap();
    assert_eq!(keys(&first), ["A/1", "b/1"]);
    let after = first.last().unwrap().key.as_str();
    let second = rig.cache.list_page("", Some(after), 2).await.unwrap();
    assert_eq!(keys(&second), ["b/2", "b/3"]);
    let after = second.last().unwrap().key.as_str();
    assert!(
        rig.cache
            .list_page("", Some(after), 2)
            .await
            .unwrap()
            .is_empty()
    );

    let inside = rig
        .cache
        .list_page("", Some("_journal/p1/0000000001"), 10)
        .await
        .unwrap();
    assert_eq!(keys(&inside), ["b/1", "b/2", "b/3"]);
    let none = rig.cache.list_page("_tiering/", None, 10).await.unwrap();
    assert!(none.is_empty());
    let nothing = rig.cache.list_page("", None, 0).await.unwrap();
    assert!(nothing.is_empty());
}

/// Asserts that `list_uploads` leaves out the uploads of reserved keys, makes a page up to the
/// asked number from the next page of the upstream, and continues from the marker of the last
/// upload it gave. It catches a filter that lets an upload of a reserved key through, or one
/// that cuts the page short.
#[tokio::test]
async fn list_uploads_leaves_out_the_reserved_keys() {
    let rig = Rig::new("tscache-list-uploads");
    let mut data = Vec::new();
    for key in ["A/1", "_journal/p1/1", "_tiering/x", "b/1", "b/2"] {
        let id = rig.tamper.create_upload(key).await.unwrap();
        if !key.starts_with('_') {
            data.push((key.to_owned(), id));
        }
    }
    let all = rig.cache.list_uploads("", None, 10).await.unwrap();
    let got: Vec<(String, String)> = all
        .iter()
        .map(|up| (up.key.clone(), up.id.clone()))
        .collect();
    assert_eq!(got, data);

    let first = rig.cache.list_uploads("", None, 2).await.unwrap();
    assert_eq!(first.len(), 2);
    let last = first.last().unwrap();
    let marker = (last.key.as_str(), Some(last.id.as_str()));
    let second = rig.cache.list_uploads("", Some(marker), 2).await.unwrap();
    let both: Vec<&str> = first
        .iter()
        .chain(&second)
        .map(|up| up.key.as_str())
        .collect();
    assert_eq!(both, ["A/1", "b/1", "b/2"]);
    assert!(
        rig.cache
            .list_uploads("_journal/", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Asserts that a listing whose prefix is inside a reserved prefix is empty and makes no call to
/// the upstream, for the three listings. It catches a filter that asks the upstream for a range
/// it then throws away.
#[tokio::test]
async fn a_listing_inside_a_reserved_prefix_asks_the_upstream_nothing() {
    let rig = rig("tscache-list-inside").await;
    rig.tamper.create_upload("_journal/p1/1").await.unwrap();
    assert!(rig.cache.list("_journal/p1/").await.unwrap().is_empty());
    assert!(
        rig.cache
            .list_page("_tiering/", None, 5)
            .await
            .unwrap()
            .is_empty()
    );
    let uploads = rig.cache.list_uploads("_journal/", None, 5).await;
    assert!(uploads.unwrap().is_empty());
    assert_eq!(rig.calls(Op::List), 0);
    assert_eq!(rig.calls(Op::ListUploads), 0);
}

/// Asserts that a large reserved range costs about one call of the upstream and not one for each
/// page of it: two thousand keys under `_journal/` and some under `_tiering/`, between data keys
/// and the data keys `_journal0` and `_tiering0` that sort right after each range (which the jump
/// must not pass), are listed three keys at a time in three calls; a listing that walked the range
/// page by page would make hundreds.
#[tokio::test]
async fn a_large_reserved_range_costs_about_one_call() {
    let rig = Rig::new("tscache-list-jump");
    for key in [
        "A/1",
        "_journal0",
        "_tiering/t1",
        "_tiering/t2",
        "_tiering0",
        "b/1",
        "b/2",
    ] {
        rig.put(key, b"x").await;
    }
    for n in 0..2000 {
        rig.put(&format!("_journal/{n:04}"), b"x").await;
    }
    let page = rig.cache.list_page("", None, 3).await.unwrap();
    assert_eq!(keys(&page), ["A/1", "_journal0", "_tiering0"]);
    assert_eq!(rig.calls(Op::List), 3);
    let after = page.last().unwrap().key.as_str();
    let rest = rig.cache.list_page("", Some(after), 3).await.unwrap();
    assert_eq!(keys(&rest), ["b/1", "b/2"]);
}

/// Asserts that a page ending on a reserved key already past the end of its range (`_journal/~~a`
/// sorts after `_journal/~`) continues after that key itself, not back at the end of the range: the
/// listing reaches the data key behind the range in one call for each key. Continuing at the end
/// of the range would be given `_journal/~~a` again and again until the call limit refused it.
#[tokio::test]
async fn a_reserved_key_past_the_end_of_its_range_continues_after_itself() {
    let rig = Rig::new("tscache-list-past-end");
    for key in ["A/1", "_journal/~~a", "_journal/~~b", "z/1"] {
        rig.put(key, b"x").await;
    }
    let rest = rig.cache.list_page("", Some("A/1"), 1).await.unwrap();
    assert_eq!(keys(&rest), ["z/1"]);
    assert_eq!(rig.calls(Op::List), 3);
}

/// A store that ignores `start_after`: every page it gives is the first, and it counts the pages.
struct Stuck {
    inner: MemoryBackend,
    pages: AtomicU64,
}

#[async_trait]
impl Backend for Stuck {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.inner.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        _start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        let _ = self.pages.fetch_add(1, Ordering::SeqCst);
        let mut page = self.inner.list(prefix).await?;
        page.truncate(max_keys);
        Ok(page)
    }
}

/// Asserts the cap on the pages one listing reads: an upstream that answers every page with the
/// same two reserved keys (it ignores where to continue) ends the listing after exactly the cap,
/// with `Unavailable`, and not never. It catches a cap that is gone or off by one.
#[tokio::test]
async fn a_listing_that_never_gets_past_the_reserved_keys_stops_at_the_cap() {
    let inner = MemoryBackend::new();
    for key in ["_journal/a", "_journal/b", "_journal/c"] {
        inner.put(key, Bytes::from_static(b"x")).await.unwrap();
    }
    let stuck = Arc::new(Stuck {
        inner,
        pages: AtomicU64::new(0),
    });
    let dir = TestDir::under(std::env::temp_dir(), "tscache-list-cap");
    let config = CacheConfig::new(dir.path().join("cache"), 1 << 20, Vec::new());
    let (upstream, disk, clock) = (
        stuck.clone(),
        Arc::new(FakeDisk::new()),
        Arc::new(TestClock::new()),
    );
    let cache = LocalCache::open(config, upstream, disk, clock).unwrap();
    let page = cache.list_page("", None, 2).await;
    assert!(matches!(page, Err(Error::Unavailable(_))), "{page:?}");
    assert_eq!(stuck.pages.load(Ordering::SeqCst), u64::from(LISTING_CALLS));
}
