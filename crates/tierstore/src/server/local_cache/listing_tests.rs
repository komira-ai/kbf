//! The listings of a cache leave out the keys a storage node keeps for itself.

use super::testing::Rig;
use crate::{Backend, Listed};

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
