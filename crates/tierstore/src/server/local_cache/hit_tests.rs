//! What the cache answers on its own, and what it passes through.

use std::os::unix::fs::PermissionsExt as _;

use bytes::Bytes;

use super::metrics::read;
use super::testing::{Rig, pattern};
use crate::{Backend, ByteRange, Error, Op};

/// Asserts that after the first read fills an object, a HEAD, a whole GET and a ranged GET are
/// answered from the disk with the upstream's bytes and make no call to the upstream, and that
/// the counters say so: a defect that sent hits upstream, or answered with other bytes, would
/// fail the call counts or the equality.
#[tokio::test]
async fn a_hit_serves_the_upstream_bytes_and_makes_no_upstream_call() {
    let rig = Rig::new("tscache-hit");
    let body = pattern(1000);
    rig.put("cas/b/obj", &body).await;
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    let (gets, heads) = (rig.calls(Op::Get), rig.calls(Op::Head));
    assert_eq!(gets, 1);
    // A key that is not named by its content is looked at before the fill and again after it, to
    // see that nobody wrote over the object while it was read.
    assert_eq!(heads, 2);

    let meta = rig.cache.head("cas/b/obj").await.unwrap();
    let upstream_meta = rig.tamper.memory.head("cas/b/obj").await.unwrap();
    assert_eq!(meta, upstream_meta);
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    let range = ByteRange::new(10, 20);
    let part = rig.cache.get("cas/b/obj", Some(range)).await.unwrap();
    assert_eq!(part, Bytes::from(body[10..30].to_vec()));
    let past = ByteRange::new(990, 100);
    let tail = rig.cache.get("cas/b/obj", Some(past)).await.unwrap();
    assert_eq!(tail, Bytes::from(body[990..].to_vec()));

    assert_eq!((rig.calls(Op::Get), rig.calls(Op::Head)), (gets, heads));
    let counters = rig.cache.counters();
    assert_eq!(read(&counters.hits), 3);
    assert_eq!(read(&counters.misses), 1);
    assert_eq!(read(&counters.bytes_served_local), 1000 + 20 + 10);
    assert_eq!(read(&counters.bytes_served_upstream), 1000);
    assert_eq!(rig.cache.held(), (1000, 1));
}

/// Asserts that a range that starts past the end of a cached object is an error and one that
/// starts at the end is empty, as for any backend, and that an object the upstream does not have
/// is `NotFound` for a GET and a HEAD, which is an answer and not a failed fill.
#[tokio::test]
async fn ranges_past_the_end_and_missing_objects_are_answered_as_a_backend_does() {
    let rig = Rig::new("tscache-edges");
    rig.put("cas/b/obj", &pattern(100)).await;
    let _ = rig.get("cas/b/obj").await.unwrap();
    let at_end = rig
        .cache
        .get("cas/b/obj", Some(ByteRange::new(100, 5)))
        .await;
    assert_eq!(at_end.unwrap(), Bytes::new());
    let past = rig
        .cache
        .get("cas/b/obj", Some(ByteRange::new(101, 5)))
        .await;
    let text = "range starting at 101 is past the end of the 100-byte object";
    assert_eq!(past, Err(Error::Other(text.to_owned())));

    let absent = Error::NotFound("cas/b/none".to_owned());
    assert_eq!(rig.get("cas/b/none").await, Err(absent.clone()));
    assert_eq!(rig.cache.head("cas/b/none").await, Err(absent));
    let counters = rig.cache.counters();
    assert_eq!(
        counters.failures(super::metrics::FailedFill::UpstreamError),
        0
    );
    assert_eq!(rig.cache.held(), (100, 1));
}

/// Asserts that a key outside every caching policy goes to the upstream each time and is never
/// kept, and that a listing (whole, a page, the open uploads) is the upstream's answer.
#[tokio::test]
async fn keys_outside_a_policy_and_listings_pass_through_and_are_never_kept() {
    let rig = Rig::new("tscache-passthrough");
    let body = pattern(64);
    rig.put("other/x", &body).await;
    rig.put("cas/b/y", &body).await;
    assert_eq!(rig.get("other/x").await.unwrap(), body);
    assert_eq!(rig.get("other/x").await.unwrap(), body);
    assert_eq!(rig.calls(Op::Get), 2);
    assert_eq!(rig.cache.head("other/x").await.unwrap().size, 64);
    assert!(!rig.has_file("other/x"));
    assert_eq!(rig.cache.held(), (0, 0));
    let counters = rig.cache.counters();
    assert_eq!((read(&counters.hits), read(&counters.misses)), (0, 0));
    assert_eq!(read(&counters.bytes_served_upstream), 128);

    let all = rig.cache.list("").await.unwrap();
    assert_eq!(all.len(), 2);
    let page = rig.cache.list_page("", Some("cas/b/y"), 10).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].key, "other/x");
    assert_eq!(rig.calls(Op::List), 2);
    let id = rig.tamper.create_upload("cas/b/up").await.unwrap();
    let uploads = rig.cache.list_uploads("", None, 10).await.unwrap();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].id, id);
    assert_eq!(rig.cache.held(), (0, 0));
}

/// Asserts that the longest prefix decides: under a prefix that says `cache = false` inside one
/// that says `true` nothing is kept, and the keys of the reserved prefixes are never kept even
/// under a policy for everything.
#[tokio::test]
async fn the_longest_policy_decides_and_reserved_keys_are_never_kept() {
    let rig = Rig::with("tscache-policy", |config| {
        config.policies = vec![
            (String::new(), true),
            ("cas/x/".to_owned(), false),
            ("cas/x/ok/".to_owned(), true),
        ];
    });
    for key in [
        "a",
        "cas/x/no",
        "cas/x/ok/yes",
        "_journal/p/1",
        "_tiering/usage",
    ] {
        rig.put(key, &pattern(10)).await;
        assert_eq!(rig.get(key).await.unwrap().len(), 10, "{key}");
    }
    assert!(rig.has_file("a"));
    assert!(rig.has_file("cas/x/ok/yes"));
    assert!(!rig.has_file("cas/x/no"));
    assert!(!rig.has_file("_journal/p/1"));
    assert!(!rig.has_file("_tiering/usage"));
    assert_eq!(rig.cache.held(), (20, 2));
}

/// Asserts that a HEAD of an object that is not kept goes to the upstream and fills nothing, so
/// a client that only asks for the size does not pull the object.
#[tokio::test]
async fn a_head_of_an_uncached_object_goes_upstream_and_fills_nothing() {
    let rig = Rig::new("tscache-head");
    rig.put("cas/b/obj", &pattern(10)).await;
    let meta = rig.cache.head("cas/b/obj").await.unwrap();
    assert_eq!(meta.size, 10);
    assert_eq!(rig.calls(Op::Head), 1);
    assert_eq!(rig.calls(Op::Get), 0);
    assert_eq!(rig.cache.held(), (0, 0));
}

/// Asserts that every write the trait has is refused with the text that says why, whatever the
/// key, and nothing reaches the upstream (the front refuses writes before they come here; this
/// is the backend's own answer for a caller that does not go through the front).
#[tokio::test]
async fn the_backend_refuses_every_write() {
    let rig = Rig::new("tscache-writes");
    let body = Bytes::from_static(b"x");
    let refused = Error::Unsupported("this tierstore is a read cache".to_owned());
    let put = rig.cache.put("cas/b/new", body.clone()).await;
    assert_eq!(put, Err(refused.clone()));
    let replace = rig.cache.put_if_match("cas/b/new", body, "tag").await;
    assert_eq!(replace, Err(refused.clone()));
    assert_eq!(rig.cache.delete("cas/b/new").await, Err(refused));
    assert_eq!(
        rig.calls(Op::Put) + rig.calls(Op::PutIfMatch) + rig.calls(Op::Delete),
        0
    );
}

/// Asserts that an entry whose file was deleted behind the cache's back, or cut short, is a
/// miss: the entry is dropped, the object is filled again and the reader gets the right bytes.
#[tokio::test]
async fn a_file_that_vanished_or_was_cut_short_is_a_miss() {
    let rig = Rig::new("tscache-vanished");
    let body = pattern(300);
    rig.put("cas/b/obj", &body).await;
    let _ = rig.get("cas/b/obj").await.unwrap();
    std::fs::remove_file(rig.file("cas/b/obj")).unwrap();
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(read(&rig.cache.counters().fills), 2);

    // Cut short: the file is shorter than the entry says.
    let path = rig.file("cas/b/obj");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(100)
        .unwrap();
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(read(&rig.cache.counters().fills), 3);
    assert_eq!(rig.cache.held(), (300, 1));
    assert_eq!(read(&rig.cache.counters().hits), 0);
}
