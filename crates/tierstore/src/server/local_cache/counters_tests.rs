//! The counters of what the cache asks of the upstream: its GETs and HEADs, the bytes they bring,
//! the HEADs of clients that the index answers, and the puts and completions the upstream takes
//! or refuses. Each is checked against the calls the upstream fixture itself counted, so that a
//! call counted twice or not at all shows.

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;

use super::metrics::{
    BYTES, Counters, FAMILIES, FS_FREE_BYTES, LABELLED, OBJECTS, PutFailed, read,
};
use super::testing::{Rig, digest, pattern};
use crate::{Backend, ByteRange, Error, Op, PartBody};

/// A body of one chunk holding `bytes`.
fn whole(bytes: &[u8]) -> PartBody<'static> {
    let chunk = Bytes::copy_from_slice(bytes);
    stream::once(async move { Ok(chunk) }).boxed()
}

/// The GETs and HEADs the cache counted to the upstream, and the bytes it read.
fn reads(rig: &Rig) -> (u64, u64, u64) {
    let counters = rig.cache.counters();
    (
        read(&counters.upstream_gets),
        read(&counters.upstream_heads),
        read(&counters.upstream_bytes_read),
    )
}

/// The GETs and HEADs the upstream fixture counted.
fn seen(rig: &Rig) -> (u64, u64) {
    (rig.calls(Op::Get), rig.calls(Op::Head))
}

/// The puts and completions the upstream took, and the bytes of them.
fn puts(rig: &Rig) -> (u64, u64) {
    let counters = rig.cache.counters();
    (
        read(&counters.upstream_puts),
        read(&counters.upstream_put_bytes),
    )
}

/// The failures of puts, by reason: exists, unavailable, full, bad digest, other.
fn failures(rig: &Rig) -> [u64; 5] {
    let counters = rig.cache.counters();
    [
        counters.put_failures(PutFailed::Exists),
        counters.put_failures(PutFailed::Unavailable),
        counters.put_failures(PutFailed::Full),
        counters.put_failures(PutFailed::BadDigest),
        counters.put_failures(PutFailed::Other),
    ]
}

/// Asserts what a ranged read that misses costs the upstream: it fills the whole object, in
/// chunks. With an object of 350 bytes and chunks of 100: one HEAD to see the size for the range,
/// one HEAD and four GETs (100, 100, 100 and 50 bytes) for the fill and one HEAD after it to see
/// that the object did not change, so 3 HEADs, 4 GETs and 350 bytes, though the client asked for
/// 20. The counts equal the calls the upstream fixture saw, and the next read of the range makes
/// none. It fails on a count that is of the bytes the client got, or that misses a path.
#[tokio::test]
async fn a_ranged_miss_counts_the_whole_fill_and_every_chunk() {
    let rig = Rig::with("tscache-counters-ranged", |config| config.fill_chunk = 100);
    rig.put("cas/b/ranged", &pattern(350)).await;
    let got = rig
        .cache
        .get("cas/b/ranged", Some(ByteRange::new(10, 20)))
        .await
        .unwrap();
    assert_eq!(got.len(), 20);

    assert_eq!(reads(&rig), (4, 3, 350));
    assert_eq!(seen(&rig), (4, 3));
    let text = rig.cache.metrics();
    assert!(text.contains("tierstore_cache_upstream_reads_total{op=\"get\"} 4\n"));
    assert!(text.contains("tierstore_cache_upstream_reads_total{op=\"head\"} 3\n"));
    assert!(text.contains("tierstore_cache_upstream_bytes_read_total 350\n"));

    rig.cache
        .get("cas/b/ranged", Some(ByteRange::new(100, 50)))
        .await
        .unwrap();
    assert_eq!(reads(&rig), (4, 3, 350));
}

/// Asserts the reads that keep nothing: a read of a key the cache does not keep is one GET and
/// the bytes of the answer; a ranged read of an object over the whole-fill limit is one HEAD (the
/// size) and one GET of the range. Neither fills, and the counts equal the upstream's calls.
#[tokio::test]
async fn a_pass_through_counts_one_call_and_the_bytes_it_brought() {
    let rig = Rig::with("tscache-counters-pass", |config| config.whole_limit = 10);
    rig.put("other/obj", &pattern(40)).await;
    rig.put("cas/b/large", &pattern(50)).await;
    assert_eq!(rig.get("other/obj").await.unwrap().len(), 40);
    assert_eq!(reads(&rig), (1, 0, 40));

    let range = ByteRange::new(0, 5);
    let got = rig.cache.get("cas/b/large", Some(range)).await.unwrap();
    assert_eq!(got.len(), 5);
    assert_eq!(reads(&rig), (2, 1, 45));
    assert_eq!(seen(&rig), (2, 1));
    assert_eq!(rig.cache.held(), (0, 0));
}

/// Asserts that a HEAD answered from the index is a hit and makes no call to the upstream, and
/// that every other HEAD (a key not yet kept, a key of a prefix that is not kept, a key that is
/// not there) is a miss and one call to the upstream.
#[tokio::test]
async fn a_head_from_the_index_is_a_hit_and_any_other_is_a_miss() {
    let rig = Rig::new("tscache-counters-head");
    rig.put("cas/b/h", &pattern(30)).await;
    rig.put("other/x", &pattern(3)).await;
    let counters = rig.cache.counters();
    let heads = || (read(&counters.head_hits), read(&counters.head_misses));

    rig.cache.head("cas/b/h").await.unwrap();
    assert_eq!((heads(), reads(&rig).1), ((0, 1), 1));
    assert_eq!(rig.get("cas/b/h").await.unwrap().len(), 30);
    // The fill made two HEADs of its own, which are not the client's.
    assert_eq!((heads(), reads(&rig).1), ((0, 1), 3));

    rig.cache.head("cas/b/h").await.unwrap();
    assert_eq!((heads(), reads(&rig).1), ((1, 1), 3));
    rig.cache.head("other/x").await.unwrap();
    assert_eq!((heads(), reads(&rig).1), ((1, 2), 4));
    let missing = rig.cache.head("cas/b/missing").await;
    assert!(matches!(missing, Err(Error::NotFound(_))));
    assert_eq!((heads(), reads(&rig).1), ((1, 3), 5));
    assert_eq!(seen(&rig).1, 5);
    let text = rig.cache.metrics();
    assert!(text.contains("tierstore_cache_head_hits_total 1\n"));
    assert!(text.contains("tierstore_cache_head_misses_total 3\n"));
}

/// Asserts the puts to the upstream: a create-only put under a kept prefix and one under a prefix
/// that is not kept are each one put with their bytes; a key that the HEAD finds is not a put to
/// the upstream (it is counted as a repeat only); and each way the upstream refuses a put counts
/// once under its reason: a 412 that the HEAD did not see, a 507, an upstream that does not
/// answer, a body that does not hash to its name, and any other error. Puts and bytes stay
/// those of the two that were taken.
#[tokio::test]
async fn a_put_a_412_and_a_507_are_each_counted_once() {
    let rig = Rig::writing("tscache-counters-put");
    let made = rig.cache.put_stream("cas/b/ok", Some(5), whole(b"fives"));
    assert_eq!(made.await, Ok(()));
    assert_eq!(puts(&rig), (1, 5));
    let made = rig.cache.put_stream("other/u", Some(7), whole(b"sevenby"));
    assert_eq!(made.await, Ok(()));
    assert_eq!(puts(&rig), (2, 12));
    assert_eq!(failures(&rig), [0; 5]);

    let again = rig.cache.put_stream("cas/b/ok", Some(1), whole(b"x"));
    assert!(matches!(again.await, Err(Error::AlreadyExists(_))));
    assert_eq!(puts(&rig), (2, 12));
    assert_eq!(failures(&rig), [0; 5]);
    assert_eq!(read(&rig.cache.counters().write_exists), 1);

    let hidden = Error::NotFound("cas/b/ok".to_owned());
    rig.upstream.fail_next_with(Op::Head, hidden);
    let raced = rig.cache.put_stream("cas/b/ok", Some(1), whole(b"x"));
    assert!(matches!(raced.await, Err(Error::AlreadyExists(_))));
    assert_eq!(failures(&rig), [1, 0, 0, 0, 0]);
    assert_eq!(read(&rig.cache.counters().write_exists), 2);

    let refusals = [
        ("cas/b/full", Error::Full("no room".to_owned())),
        ("cas/b/down", Error::Unavailable("down".to_owned())),
        ("cas/b/odd", Error::Other("odd".to_owned())),
    ];
    for (key, error) in refusals {
        rig.upstream.fail_next_with(Op::Put, error.clone());
        let refused = rig.cache.put_stream(key, Some(1), whole(b"x"));
        assert_eq!(refused.await, Err(error), "{key}");
    }
    assert_eq!(failures(&rig), [1, 1, 1, 0, 1]);

    // A failure in the middle of the body, under a kept prefix and under one that is not kept:
    // the upstream read part of the body and then the body failed. Not a put, and no bytes.
    for key in ["cas/b/mid", "other/mid"] {
        let items: Vec<Result<Bytes, Error>> = vec![
            Ok(Bytes::from_static(b"part")),
            Err(Error::Unavailable("the client went away".to_owned())),
        ];
        let cut = rig
            .cache
            .put_stream(key, Some(9), stream::iter(items).boxed());
        assert!(matches!(cut.await, Err(Error::Unavailable(_))), "{key}");
    }
    assert_eq!(failures(&rig), [1, 3, 1, 0, 1]);
    assert_eq!(puts(&rig), (2, 12));

    let named = format!("cas/l/{}", digest(b"the name's bytes"));
    let wrong = rig.cache.put_stream(&named, Some(5), whole(b"other"));
    assert_eq!(wrong.await, Err(Error::BadDigest(named.clone())));
    assert_eq!(failures(&rig), [1, 3, 1, 1, 1]);
    assert_eq!(puts(&rig), (2, 12));
    let text = rig.cache.metrics();
    assert!(text.contains("tierstore_cache_upstream_puts_total 2\n"));
    assert!(text.contains("tierstore_cache_upstream_put_bytes_total 12\n"));
    let expected = [
        ("exists", 1),
        ("unavailable", 3),
        ("full", 1),
        ("bad_digest", 1),
        ("other", 1),
    ];
    for (reason, count) in expected {
        let line =
            format!("tierstore_cache_upstream_put_failures_total{{reason=\"{reason}\"}} {count}\n");
        assert!(text.contains(&line), "{reason}");
    }

    // The counts are of the calls the upstream saw: every put it was sent either was taken or
    // failed, and every GET and HEAD counted is one the fixture saw.
    let (taken, _) = puts(&rig);
    let attempts = taken + failures(&rig).iter().sum::<u64>();
    assert_eq!(rig.calls(Op::Put), attempts);
    let (gets, heads, _) = reads(&rig);
    assert_eq!((gets, heads), seen(&rig));
}

/// Asserts that a put the upstream took without reading the body to its end, for a key named by
/// its content, is counted as a put (the upstream has the object) and as a `bad_digest` failure
/// (the client is told so).
#[tokio::test]
async fn a_bad_digest_after_the_upstream_took_the_put_counts_as_both() {
    let rig = Rig::writing("tscache-counters-bad-after");
    rig.tamper
        .stop_after
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let key = format!("cas/l/{}", digest(b"shortened!"));
    let parts: [&'static [u8]; 2] = [b"short", b"ened!"];
    let items: Vec<Result<Bytes, Error>> = parts
        .iter()
        .copied()
        .map(|part| Ok(Bytes::from_static(part)))
        .collect();
    let made = rig
        .cache
        .put_stream(&key, Some(10), stream::iter(items).boxed());
    assert_eq!(made.await, Err(Error::BadDigest(key.clone())));
    assert_eq!(puts(&rig), (1, 5));
    assert_eq!(failures(&rig), [0, 0, 0, 1, 0]);
}

/// Asserts that the listings the cache makes at the upstream are counted: `list` and each page of
/// `list_page` as `list`, and `list_uploads`, equal to the calls the upstream saw; and that a
/// listing of a reserved prefix, which the cache answers itself, makes and counts none.
#[tokio::test]
async fn the_listings_are_counted_as_upstream_reads() {
    let rig = Rig::new("tscache-counters-list");
    rig.put("cas/b/one", b"1").await;
    rig.put("cas/b/two", b"2").await;
    let counters = rig.cache.counters();
    let lists = || {
        (
            read(&counters.upstream_lists),
            read(&counters.upstream_list_uploads),
        )
    };

    rig.cache.list("cas/").await.unwrap();
    assert_eq!(lists(), (1, 0));
    rig.cache.list_page("cas/", None, 10).await.unwrap();
    assert_eq!(lists(), (2, 0));
    rig.cache.list_uploads("cas/", None, 10).await.unwrap();
    assert_eq!(lists(), (2, 1));
    assert!(rig.cache.list("_journal/").await.unwrap().is_empty());
    assert_eq!(lists(), (2, 1));

    assert_eq!(rig.upstream.listed_prefixes().len() as u64, 2);
    assert_eq!(rig.calls(Op::ListUploads), 1);
    let text = rig.cache.metrics();
    assert!(text.contains("tierstore_cache_upstream_reads_total{op=\"list\"} 2\n"));
    assert!(text.contains("tierstore_cache_upstream_reads_total{op=\"list_uploads\"} 1\n"));
}

/// Asserts that a multipart upload is one put with the bytes of its parts: two parts of 6 and 5
/// bytes and a completion are one put and 11 bytes; a completion that the upstream refuses
/// (the key exists) is a failure of reason `exists` and no put, and the bytes of its part, which
/// the upstream took, are counted as the bytes of parts are.
#[tokio::test]
async fn a_multipart_counts_one_put_with_the_bytes_of_its_parts() {
    let rig = Rig::writing("tscache-counters-multipart");
    let cache = &rig.cache;
    let id = cache.create_upload("cas/b/mp").await.unwrap();
    let one = cache
        .upload_part("cas/b/mp", &id, 1, whole(b"hello "))
        .await;
    let two = cache
        .upload_part_with_len("cas/b/mp", &id, 2, Some(5), whole(b"world"))
        .await;
    assert_eq!(puts(&rig), (0, 11));
    let parts = [(1, one.unwrap()), (2, two.unwrap())];
    cache
        .complete_upload("cas/b/mp", &id, &parts)
        .await
        .unwrap();
    assert_eq!(puts(&rig), (1, 11));
    assert_eq!(failures(&rig), [0; 5]);

    let again = cache.create_upload("cas/b/mp").await.unwrap();
    let part = cache.upload_part("cas/b/mp", &again, 1, whole(b"x")).await;
    let refused = cache
        .complete_upload("cas/b/mp", &again, &[(1, part.unwrap())])
        .await;
    assert!(matches!(refused, Err(Error::AlreadyExists(_))));
    assert_eq!(puts(&rig), (1, 12));
    assert_eq!(failures(&rig), [1, 0, 0, 0, 0]);
    // The completions the upstream saw are the ones taken and the ones that failed.
    let failed = failures(&rig).iter().sum::<u64>();
    assert_eq!(rig.calls(Op::CompleteUpload), puts(&rig).0 + failed);
}

/// Asserts that the page is the families of the constants, each once and in their order, byte for
/// byte: a `# TYPE` line and a sample for each family with one sample, then for each labelled
/// family a `# TYPE` line and a sample for each label. A family that is renamed in the page and
/// not in its constant, or left out or repeated, fails here.
#[test]
fn the_page_is_the_families_of_the_constants_in_order() {
    let text = Counters::default().render(4, 5, 6);
    let mut expected = String::new();
    for (name, kind) in FAMILIES {
        let value = match name {
            BYTES => 4,
            OBJECTS => 5,
            FS_FREE_BYTES => 6,
            _ => 0,
        };
        expected.push_str(&format!("# TYPE {name} {kind}\n{name} {value}\n"));
    }
    for (name, key, labels) in LABELLED {
        expected.push_str(&format!("# TYPE {name} counter\n"));
        for label in labels {
            expected.push_str(&format!("{name}{{{key}=\"{label}\"}} 0\n"));
        }
    }
    assert_eq!(text, expected);
    let names: BTreeSet<&str> = FAMILIES
        .iter()
        .map(|(name, _)| *name)
        .chain(LABELLED.iter().map(|(name, _, _)| *name))
        .collect();
    assert_eq!(names.len(), FAMILIES.len() + LABELLED.len());
}

/// Asserts the form of the page: every family has exactly one `# TYPE` line, before its first
/// sample, and no sample is of a family without one; the four measured now (`bytes`, `objects`,
/// `fs_free_bytes`, `staging_files`) are gauges and every other family is a counter; and every
/// family with a `# TYPE` line has a sample. It fails on a family that lost its line or has two,
/// and on a gauge typed as a counter.
#[tokio::test]
async fn every_family_has_one_type_line_of_the_right_kind() {
    let rig = Rig::writing("tscache-counters-types");
    let made = rig.cache.put_stream("cas/b/t", Some(3), whole(b"abc"));
    assert_eq!(made.await, Ok(()));
    rig.put("cas/b/u", b"def").await;
    assert_eq!(rig.get("cas/b/u").await.unwrap(), "def");
    let text = rig.cache.metrics();

    let mut kinds: BTreeMap<String, String> = BTreeMap::new();
    let mut sampled: BTreeSet<String> = BTreeSet::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let (name, kind) = rest.split_once(' ').expect("a name and a kind");
            assert!(!sampled.contains(name), "{name}: TYPE after a sample");
            let before = kinds.insert(name.to_owned(), kind.to_owned());
            assert!(before.is_none(), "{name}: two TYPE lines");
        } else {
            let name = line.split([' ', '{']).next().expect("a name");
            assert!(kinds.contains_key(name), "{name}: a sample with no TYPE");
            let _ = sampled.insert(name.to_owned());
        }
    }
    let typed: BTreeSet<String> = kinds.keys().cloned().collect();
    assert_eq!(typed, sampled);
    let gauges: BTreeSet<&str> = kinds
        .iter()
        .filter(|(_, kind)| kind.as_str() == "gauge")
        .map(|(name, _)| name.as_str())
        .collect();
    let expected: BTreeSet<&str> = [
        "tierstore_cache_bytes",
        "tierstore_cache_objects",
        "tierstore_cache_fs_free_bytes",
        "tierstore_cache_staging_files",
    ]
    .into();
    assert_eq!(gauges, expected);
    for (name, kind) in &kinds {
        assert!(
            matches!(kind.as_str(), "counter" | "gauge"),
            "{name}: {kind}"
        );
    }
    assert!(kinds.len() > 20, "{}", kinds.len());
}

/// Asserts the accounting of misses: a read of a kept key that the index did not serve is one
/// miss, counted before any call to the upstream: a ranged read whose HEAD fails (it makes
/// no GET), a ranged read of an object over the whole-fill limit, a whole read that fills, and a
/// read of an object whose file was removed behind the cache's back; a hit and a read of a
/// key under a prefix that is not kept are not misses.
#[tokio::test]
async fn a_read_the_index_did_not_serve_is_one_miss_counted_before_the_upstream() {
    let rig = Rig::with("tscache-counters-misses", |config| config.whole_limit = 10);
    rig.put("cas/b/k", &pattern(50)).await;
    rig.put("other/x", &pattern(5)).await;
    let counters = rig.cache.counters();
    let tally = || (read(&counters.hits), read(&counters.misses));

    rig.upstream.fail_next(Op::Head, 1);
    let failed = rig.cache.get("cas/b/k", Some(ByteRange::new(0, 5))).await;
    assert!(matches!(failed, Err(Error::Unavailable(_))), "{failed:?}");
    assert_eq!(tally(), (0, 1));
    assert_eq!(rig.calls(Op::Get), 0);

    let over = rig.cache.get("cas/b/k", Some(ByteRange::new(0, 5))).await;
    assert_eq!(over.unwrap().len(), 5);
    assert_eq!(tally(), (0, 2));

    assert_eq!(rig.get("cas/b/k").await.unwrap().len(), 50);
    assert_eq!(tally(), (0, 3));
    assert_eq!(rig.get("cas/b/k").await.unwrap().len(), 50);
    assert_eq!(tally(), (1, 3));

    std::fs::remove_file(rig.file("cas/b/k")).unwrap();
    assert_eq!(rig.get("cas/b/k").await.unwrap().len(), 50);
    assert_eq!(tally(), (1, 4));

    assert_eq!(rig.get("other/x").await.unwrap().len(), 5);
    assert_eq!(tally(), (1, 4));
}

/// Asserts that a create-only put whose HEAD before the body fails with anything but "not found"
/// is refused with that error and counted as a failed put under the reason the error maps to
/// (`unavailable`, `other`), though no put was sent (the upstream saw none); and that one whose
/// HEAD finds the object is counted as a repeat only, not as a failed put.
#[tokio::test]
async fn a_put_whose_precheck_head_fails_is_a_counted_failure() {
    let rig = Rig::writing("tscache-counters-precheck");
    let down = Error::Unavailable("head".to_owned());
    rig.upstream.fail_next_with(Op::Head, down.clone());
    let refused = rig.cache.put_stream("cas/b/p", Some(1), whole(b"x"));
    assert_eq!(refused.await, Err(down));
    assert_eq!(failures(&rig), [0, 1, 0, 0, 0]);

    let odd = Error::Other("head".to_owned());
    rig.upstream.fail_next_with(Op::Head, odd.clone());
    let refused = rig.cache.put_stream("cas/b/p", Some(1), whole(b"x"));
    assert_eq!(refused.await, Err(odd));
    assert_eq!(failures(&rig), [0, 1, 0, 0, 1]);
    assert_eq!(puts(&rig), (0, 0));
    assert_eq!(rig.calls(Op::Put), 0);
    assert_eq!(read(&rig.cache.counters().write_exists), 0);

    rig.put("cas/b/there", b"here").await;
    let found = rig.cache.put_stream("cas/b/there", Some(1), whole(b"x"));
    assert!(matches!(found.await, Err(Error::AlreadyExists(_))));
    assert_eq!(failures(&rig), [0, 1, 0, 0, 1]);
    assert_eq!(read(&rig.cache.counters().write_exists), 1);
    assert_eq!(rig.calls(Op::Put), 0);
}
