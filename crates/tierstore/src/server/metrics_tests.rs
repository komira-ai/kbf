//! `/metricz`: the counters of the proxy's reads, through the S3 front, the proxy and the two
//! tiers of the rig. Nothing waits and no test depends on time.

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, StatusCode};

use crate::{Backend, ByteRange, Error, Op};

use super::testing::{Reply, Rig};

pub(super) const BUDGET: u64 = 1 << 20;

/// Every series of `/metricz` with its labels, as the text names it.
const SERIES: [&str; 26] = [
    "tierstore_hits_total{tier=\"hot\",op=\"get\"}",
    "tierstore_hits_total{tier=\"hot\",op=\"head\"}",
    "tierstore_hits_total{tier=\"cold\",op=\"get\"}",
    "tierstore_hits_total{tier=\"cold\",op=\"head\"}",
    "tierstore_misses_total{tier=\"hot\",op=\"get\"}",
    "tierstore_misses_total{tier=\"hot\",op=\"head\"}",
    "tierstore_misses_total{tier=\"cold\",op=\"get\"}",
    "tierstore_misses_total{tier=\"cold\",op=\"head\"}",
    "tierstore_read_errors_total{tier=\"hot\",op=\"get\"}",
    "tierstore_read_errors_total{tier=\"hot\",op=\"head\"}",
    "tierstore_read_errors_total{tier=\"cold\",op=\"get\"}",
    "tierstore_read_errors_total{tier=\"cold\",op=\"head\"}",
    "tierstore_reads_total{tier=\"hot\",op=\"get\"}",
    "tierstore_reads_total{tier=\"hot\",op=\"head\"}",
    "tierstore_reads_total{tier=\"cold\",op=\"get\"}",
    "tierstore_reads_total{tier=\"cold\",op=\"head\"}",
    "tierstore_bytes_served_total{tier=\"hot\"}",
    "tierstore_bytes_served_total{tier=\"cold\"}",
    "tierstore_bytes_read_total{tier=\"hot\"}",
    "tierstore_bytes_read_total{tier=\"cold\"}",
    "tierstore_promotions_total",
    "tierstore_promoted_bytes_total",
    "tierstore_promotions_skipped_total{reason=\"critical\"}",
    "tierstore_promotions_skipped_total{reason=\"full\"}",
    "tierstore_promotions_skipped_total{reason=\"raced\"}",
    "tierstore_promotions_skipped_total{reason=\"failed\"}",
];

async fn metricz(rig: &Rig) -> Reply {
    let request = Request::builder()
        .method("GET")
        .uri("/metricz")
        .body(Full::new(Bytes::new()))
        .unwrap();
    rig.send_raw(request).await
}

/// The value of the series `name` in the text of a `/metricz`.
pub(super) fn sample(text: &str, name: &str) -> u64 {
    let prefix = format!("{name} ");
    let line = text
        .lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("no series {name} in:\n{text}"));
    line[prefix.len()..].parse().expect("a number")
}

/// The value of the series `name` on the `/metricz` of the rig.
async fn value(rig: &Rig, name: &str) -> u64 {
    sample(&metricz(rig).await.body, name)
}

pub(super) const HOT_HIT_GET: &str = "tierstore_hits_total{tier=\"hot\",op=\"get\"}";
pub(super) const HOT_MISS_GET: &str = "tierstore_misses_total{tier=\"hot\",op=\"get\"}";
pub(super) const HOT_ERR_GET: &str = "tierstore_read_errors_total{tier=\"hot\",op=\"get\"}";
pub(super) const HOT_READS_GET: &str = "tierstore_reads_total{tier=\"hot\",op=\"get\"}";
pub(super) const HOT_HIT_HEAD: &str = "tierstore_hits_total{tier=\"hot\",op=\"head\"}";
pub(super) const HOT_MISS_HEAD: &str = "tierstore_misses_total{tier=\"hot\",op=\"head\"}";
pub(super) const HOT_ERR_HEAD: &str = "tierstore_read_errors_total{tier=\"hot\",op=\"head\"}";
pub(super) const HOT_READS_HEAD: &str = "tierstore_reads_total{tier=\"hot\",op=\"head\"}";
pub(super) const COLD_HIT_GET: &str = "tierstore_hits_total{tier=\"cold\",op=\"get\"}";
pub(super) const COLD_MISS_GET: &str = "tierstore_misses_total{tier=\"cold\",op=\"get\"}";
pub(super) const COLD_ERR_GET: &str = "tierstore_read_errors_total{tier=\"cold\",op=\"get\"}";
pub(super) const COLD_READS_GET: &str = "tierstore_reads_total{tier=\"cold\",op=\"get\"}";
pub(super) const COLD_HIT_HEAD: &str = "tierstore_hits_total{tier=\"cold\",op=\"head\"}";
pub(super) const COLD_MISS_HEAD: &str = "tierstore_misses_total{tier=\"cold\",op=\"head\"}";
pub(super) const COLD_ERR_HEAD: &str = "tierstore_read_errors_total{tier=\"cold\",op=\"head\"}";
pub(super) const COLD_READS_HEAD: &str = "tierstore_reads_total{tier=\"cold\",op=\"head\"}";
pub(super) const HOT_SERVED: &str = "tierstore_bytes_served_total{tier=\"hot\"}";
pub(super) const COLD_SERVED: &str = "tierstore_bytes_served_total{tier=\"cold\"}";
pub(super) const HOT_BYTES: &str = "tierstore_bytes_read_total{tier=\"hot\"}";
pub(super) const COLD_BYTES: &str = "tierstore_bytes_read_total{tier=\"cold\"}";
pub(super) const PROMOTIONS: &str = "tierstore_promotions_total";
pub(super) const PROMOTED_BYTES: &str = "tierstore_promoted_bytes_total";

pub(super) fn skipped(reason: &str) -> String {
    format!("tierstore_promotions_skipped_total{{reason=\"{reason}\"}}")
}

/// A rig with the object `key` in the cold tier only.
async fn cold_only(key: &str, body: &'static [u8]) -> Rig {
    let rig = Rig::new(BUDGET).await;
    rig.cold_mem
        .put(key, Bytes::from_static(body))
        .await
        .unwrap();
    rig
}

/// Asserts that `/metricz` answers without a signature with every series, labels included, at
/// zero before any read. It would catch a renamed or
/// dropped series, a label that changed, and a probe that asks for a key.
#[tokio::test]
async fn metricz_lists_every_series_with_its_labels() {
    let rig = Rig::new(BUDGET).await;
    let reply = metricz(&rig).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), "text/plain");
    for series in SERIES {
        assert_eq!(value(&rig, series).await, 0, "{series}");
    }
}

/// Asserts that `/metricz` has the build gauge with the crate version and the commit the build
/// embeds, with value 1, once, whatever else the server has. It would catch a gauge that is not
/// exported, one that names another version or no commit, and one on only some shapes.
#[tokio::test]
async fn metricz_has_the_build_gauge_with_version_and_commit() {
    use super::build_info::COMMIT;
    let expected = format!(
        "tierstore_build_info{{version=\"{}\",commit=\"{COMMIT}\"}} 1",
        env!("CARGO_PKG_VERSION")
    );
    for rig in [
        Rig::new(BUDGET).await,
        Rig::with(BUDGET, |config| config.proxy = None).await,
    ] {
        let body = metricz(&rig).await.body;
        let lines: Vec<&str> = body.lines().collect();
        let found = lines.iter().filter(|l| **l == expected).count();
        assert_eq!(found, 1, "{body}");
        assert!(
            body.contains("# TYPE tierstore_build_info gauge\n"),
            "{body}"
        );
        assert!(!COMMIT.is_empty());
    }
}

/// Asserts that a server with no proxy and no OCI front answers `/metricz` 200 with one comment
/// line that names the sources and no series but the build gauge. It would catch a probe that
/// fails or says nothing when there is nothing to count.
#[tokio::test]
async fn metricz_without_a_source_is_one_line_naming_them() {
    let rig = Rig::with(BUDGET, |config| config.proxy = None).await;
    let reply = metricz(&rig).await;
    assert_eq!(reply.status, StatusCode::OK);
    let lines: Vec<&str> = reply
        .body
        .lines()
        .filter(|line| !line.contains("tierstore_build_info"))
        .collect();
    assert_eq!(lines.len(), 1, "{}", reply.body);
    for name in ["proxy", "OCI read front", "local cache"] {
        assert!(
            lines[0].starts_with('#') && lines[0].contains(name),
            "{}",
            reply.body
        );
    }
}

/// Asserts that a server with a proxy and an OCI front shows both blocks, the proxy's first, each
/// ending in a newline so that no two lines run together, and that the series of the one do not
/// replace those of the other. It would catch a probe that picks one block.
#[tokio::test]
async fn metricz_renders_every_block_the_server_has() {
    let oci = std::sync::Arc::new(super::oci::OciMetrics::default());
    let rig = Rig::with(BUDGET, |config| config.oci_metrics = Some(oci)).await;
    let body = metricz(&rig).await.body;
    let proxy = body.find("tierstore_hits_total{").expect("the proxy block");
    let front = body
        .find("tierstore_oci_requests_total{")
        .expect("the OCI block");
    assert!(proxy < front, "{body}");
    assert!(body.ends_with('\n'), "{body}");
    assert_eq!(sample(&body, HOT_HIT_GET), 0);
    assert!(!body.contains("no proxy"), "{body}");
}

/// Asserts that a GET of an object only in the cold tier counts a hot miss, a cold read, a cold
/// hit, the bytes read from the cold tier and served, and one promotion with its bytes, and that
/// a second GET counts a hot hit and its bytes and no more cold reads. It would catch a cold
/// read that is not counted, bytes counted on the wrong tier, and a promotion that is not
/// seen.
#[tokio::test]
async fn a_get_of_a_cold_object_counts_the_cold_read_and_the_promotion() {
    let rig = cold_only("obj", b"twelve bytes").await;
    let first = rig.get("obj", None).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.body, "twelve bytes");
    assert_eq!(value(&rig, HOT_MISS_GET).await, 1);
    assert_eq!(value(&rig, COLD_READS_GET).await, 1);
    assert_eq!(value(&rig, COLD_HIT_GET).await, 1);
    assert_eq!(value(&rig, COLD_MISS_GET).await, 0);
    assert_eq!(value(&rig, HOT_HIT_GET).await, 0);
    assert_eq!(value(&rig, COLD_BYTES).await, 12);
    assert_eq!(value(&rig, COLD_SERVED).await, 12);
    assert_eq!(value(&rig, HOT_SERVED).await, 0);
    assert_eq!(value(&rig, PROMOTIONS).await, 1);
    assert_eq!(value(&rig, PROMOTED_BYTES).await, 12);
    // The front asks for the size first: that is a head, answered by the cold tier too.
    assert_eq!(value(&rig, HOT_MISS_HEAD).await, 1);
    assert_eq!(value(&rig, COLD_READS_HEAD).await, 1);
    assert_eq!(value(&rig, COLD_HIT_HEAD).await, 1);

    let second = rig.get("obj", None).await;
    assert_eq!(second.body, "twelve bytes");
    assert_eq!(value(&rig, HOT_HIT_GET).await, 1);
    assert_eq!(value(&rig, HOT_HIT_HEAD).await, 1);
    assert_eq!(value(&rig, HOT_SERVED).await, 12);
    assert_eq!(value(&rig, COLD_READS_GET).await, 1);
    assert_eq!(value(&rig, COLD_READS_HEAD).await, 1);
    assert_eq!(value(&rig, PROMOTIONS).await, 1);
    assert_eq!(value(&rig, COLD_SERVED).await, 12);
}

/// Asserts that a GET of an object in the hot tier counts a hot hit and its bytes and no cold
/// read, miss or promotion. It would catch a hit counted against the cold tier.
#[tokio::test]
async fn a_get_of_a_hot_object_counts_only_a_hot_hit() {
    let rig = Rig::new(BUDGET).await;
    assert_eq!(rig.put("hot", b"hello").await.status, StatusCode::OK);
    assert_eq!(rig.get("hot", None).await.body, "hello");
    assert_eq!(value(&rig, HOT_HIT_GET).await, 1);
    assert_eq!(value(&rig, HOT_READS_GET).await, 1);
    assert_eq!(value(&rig, HOT_SERVED).await, 5);
    assert_eq!(value(&rig, HOT_BYTES).await, 5);
    assert_eq!(value(&rig, COLD_BYTES).await, 0);
    assert_eq!(value(&rig, HOT_MISS_GET).await, 0);
    assert_eq!(value(&rig, COLD_READS_GET).await, 0);
    assert_eq!(value(&rig, COLD_READS_HEAD).await, 0);
    assert_eq!(value(&rig, COLD_SERVED).await, 0);
    assert_eq!(value(&rig, PROMOTIONS).await, 0);
}

/// Asserts that a GET of a key neither tier holds counts a miss on each tier (on the head the
/// front makes first) and one cold read, and serves and promotes nothing. It would catch a
/// missing key that is not counted as a miss.
#[tokio::test]
async fn a_get_of_a_missing_key_counts_a_miss_on_each_tier() {
    let rig = Rig::new(BUDGET).await;
    assert_eq!(rig.get("nope", None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(value(&rig, HOT_MISS_HEAD).await, 1);
    assert_eq!(value(&rig, COLD_MISS_HEAD).await, 1);
    assert_eq!(value(&rig, COLD_READS_HEAD).await, 1);
    assert_eq!(value(&rig, HOT_HIT_HEAD).await, 0);
    assert_eq!(value(&rig, COLD_HIT_HEAD).await, 0);
    assert_eq!(value(&rig, HOT_SERVED).await, 0);
    assert_eq!(value(&rig, COLD_SERVED).await, 0);
    assert_eq!(value(&rig, PROMOTIONS).await, 0);
}

/// Asserts that a ranged GET of a cold object serves the bytes of the range and reads the whole
/// object from the cold tier, and that the promotion is of the whole object. It would catch
/// bytes served counted as the whole object, and cold bytes counted as the range.
#[tokio::test]
async fn a_ranged_get_of_a_cold_object_serves_the_range_and_reads_the_whole() {
    let rig = cold_only("obj", b"0123456789").await;
    let reply = rig.get("obj", Some("bytes=2-4")).await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(reply.body, "234");
    assert_eq!(value(&rig, COLD_SERVED).await, 3);
    assert_eq!(value(&rig, COLD_BYTES).await, 10);
    assert_eq!(value(&rig, PROMOTED_BYTES).await, 10);
    let again = rig.get("obj", Some("bytes=5-9")).await;
    assert_eq!(again.body, "56789");
    assert_eq!(value(&rig, HOT_SERVED).await, 5);
}

/// Asserts that a HEAD of a cold object counts a hot miss, a cold read and a cold hit on the
/// head, serves no bytes and copies nothing back. It would catch a head that promotes.
#[tokio::test]
async fn a_head_of_a_cold_object_counts_the_read_and_copies_nothing() {
    let rig = cold_only("obj", b"abc").await;
    assert_eq!(rig.call("HEAD", "obj", b"").await.status, StatusCode::OK);
    assert_eq!(value(&rig, HOT_MISS_HEAD).await, 1);
    assert_eq!(value(&rig, COLD_READS_HEAD).await, 1);
    assert_eq!(value(&rig, COLD_HIT_HEAD).await, 1);
    assert_eq!(value(&rig, COLD_READS_GET).await, 0);
    assert_eq!(value(&rig, COLD_SERVED).await, 0);
    assert_eq!(value(&rig, PROMOTIONS).await, 0);
    assert_eq!(rig.call("HEAD", "obj", b"").await.status, StatusCode::OK);
    assert_eq!(value(&rig, HOT_HIT_HEAD).await, 0);
    assert_eq!(value(&rig, COLD_HIT_HEAD).await, 2);
}

/// Asserts that a cold read whose copy into the hot tier is not made counts the reason: the hot
/// tier at its critical mark, a hot tier that answers "full", one that already holds the object,
/// one that fails. The read itself is answered in each case. It would catch a reason counted as
/// another, a skipped copy counted as a promotion, and a copy failure that fails the read.
#[tokio::test]
async fn a_cold_read_that_is_not_copied_back_counts_why() {
    let rig = cold_only("a", b"aaaa").await;
    // 96 percent of the budget: above the critical mark (95), below the budget, which is `Full`.
    let hot_bytes = BUDGET / 100 * 96;
    let json = format!(
        r#"{{"hot_bytes":{hot_bytes},"copy_lag":0,"time":{}}}"#,
        super::testing::NOW
    );
    rig.hot_mem
        .put("_tiering/usage", Bytes::from(json))
        .await
        .unwrap();
    assert_eq!(rig.get("a", None).await.body, "aaaa");
    assert_eq!(value(&rig, &skipped("critical")).await, 1);

    let rig = cold_only("a", b"aaaa").await;
    rig.hot.fail_next_with(Op::Put, Error::Full("a".to_owned()));
    assert_eq!(rig.get("a", None).await.body, "aaaa");
    assert_eq!(value(&rig, &skipped("full")).await, 1);

    let rig = cold_only("a", b"aaaa").await;
    let exists = Error::AlreadyExists("a".to_owned());
    rig.hot.fail_next_with(Op::Put, exists);
    assert_eq!(rig.get("a", None).await.body, "aaaa");
    assert_eq!(value(&rig, &skipped("raced")).await, 1);

    let rig = cold_only("a", b"aaaa").await;
    rig.hot.fail_next(Op::Put, 1);
    assert_eq!(rig.get("a", None).await.body, "aaaa");
    assert_eq!(value(&rig, &skipped("failed")).await, 1);
    assert_eq!(value(&rig, PROMOTIONS).await, 0);
    for reason in ["critical", "full", "raced"] {
        assert_eq!(value(&rig, &skipped(reason)).await, 0, "{reason}");
    }
}

/// Asserts that a read that fails with something other than "not found" counts neither a hit
/// nor a miss on the tier that failed, and that a range past the end of a cold object is an
/// error that serves no bytes. It would catch an outage counted as a miss, which would lower
/// the hit rate of a tier that is down.
#[tokio::test]
async fn a_failed_read_is_not_counted_as_a_hit_or_a_miss() {
    let rig = cold_only("obj", b"abc").await;
    let proxy = rig.config.proxy.clone().expect("a proxy");

    rig.hot.fail_next(Op::Get, 1);
    assert!(matches!(
        proxy.get("obj", None).await,
        Err(Error::Unavailable(_))
    ));
    rig.hot.fail_next(Op::Head, 1);
    assert!(matches!(
        proxy.head("obj").await,
        Err(Error::Unavailable(_))
    ));
    assert_eq!(value(&rig, HOT_ERR_GET).await, 1);
    assert_eq!(value(&rig, HOT_ERR_HEAD).await, 1);
    assert_eq!(
        value(&rig, HOT_HIT_GET).await + value(&rig, HOT_HIT_HEAD).await,
        0
    );
    assert_eq!(value(&rig, HOT_MISS_GET).await, 0);
    assert_eq!(value(&rig, HOT_MISS_HEAD).await, 0);
    assert_eq!(value(&rig, COLD_READS_GET).await, 0);

    rig.cold.fail_next(Op::Get, 1);
    assert!(matches!(
        proxy.get("obj", None).await,
        Err(Error::Unavailable(_))
    ));
    rig.cold.fail_next(Op::Head, 1);
    assert!(matches!(
        proxy.head("obj").await,
        Err(Error::Unavailable(_))
    ));
    assert_eq!(value(&rig, COLD_ERR_GET).await, 1);
    assert_eq!(value(&rig, COLD_ERR_HEAD).await, 1);
    assert_eq!(value(&rig, COLD_READS_GET).await, 1);
    assert_eq!(value(&rig, COLD_READS_HEAD).await, 1);
    assert_eq!(value(&rig, COLD_HIT_GET).await, 0);
    assert_eq!(value(&rig, COLD_MISS_GET).await, 0);
    assert_eq!(value(&rig, COLD_HIT_HEAD).await, 0);
    assert_eq!(value(&rig, COLD_MISS_HEAD).await, 0);

    // A range past the end of the object: the cold tier gave the whole object (3 bytes) and the
    // proxy copied it into the hot tier before it cut the range, which it cannot.
    let past = proxy.get("obj", Some(ByteRange::new(9, 1))).await;
    assert!(past.is_err());
    assert_eq!(value(&rig, COLD_SERVED).await, 0);
    assert_eq!(value(&rig, COLD_BYTES).await, 3);
    assert_eq!(value(&rig, PROMOTIONS).await, 1);
}

/// Asserts that an object deleted between the head and the get (the get finds neither tier)
/// counts a miss on each tier for the get. It would catch a get miss that is never counted.
#[tokio::test]
async fn a_get_that_finds_neither_tier_counts_a_get_miss() {
    let rig = Rig::new(BUDGET).await;
    let proxy = rig.config.proxy.clone().expect("a proxy");
    assert!(matches!(
        proxy.get("gone", None).await,
        Err(Error::NotFound(_))
    ));
    assert_eq!(value(&rig, HOT_MISS_GET).await, 1);
    assert_eq!(value(&rig, COLD_MISS_GET).await, 1);
    assert_eq!(value(&rig, COLD_READS_GET).await, 1);
}

/// Asserts that a hot tier that is down, met by a GET through the S3 front, moves
/// `read_errors_total` of the hot tier and leaves every hit and miss where it was, and that the
/// reads of the cold tier, which was never asked, stay at zero. It would catch an outage that
/// counts as a miss (the hit rate of a tier that is down would fall) or as nothing.
#[tokio::test]
async fn a_hot_outage_moves_the_error_counter_and_not_the_hit_rate() {
    let rig = cold_only("obj", b"abc").await;
    rig.hot.set_down(true);
    let reply = rig.get("obj", None).await;
    assert_ne!(reply.status, StatusCode::OK);
    assert_eq!(value(&rig, HOT_ERR_HEAD).await, 1);
    assert_eq!(value(&rig, HOT_READS_HEAD).await, 1);
    for series in [
        HOT_HIT_GET,
        HOT_HIT_HEAD,
        HOT_MISS_GET,
        HOT_MISS_HEAD,
        COLD_READS_HEAD,
    ] {
        assert_eq!(value(&rig, series).await, 0, "{series}");
    }
}

/// Asserts that a GET whose object vanishes from the cold tier between the head and the get (the
/// head found it, the get does not) counts a cold miss at `op="get"` and answers 404. It would
/// catch a miss at the get that the front's own head hides.
#[tokio::test]
async fn a_get_whose_object_vanishes_after_the_head_counts_a_cold_get_miss() {
    let rig = cold_only("obj", b"abc").await;
    rig.cold
        .fail_next_with(Op::Get, Error::NotFound("obj".to_owned()));
    // The cold tier's head is the first call the front makes; the injected failure is for the get.
    let reply = rig.get("obj", None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(value(&rig, COLD_HIT_HEAD).await, 1);
    assert_eq!(value(&rig, COLD_MISS_GET).await, 1);
    assert_eq!(value(&rig, COLD_HIT_GET).await, 0);
    assert_eq!(value(&rig, HOT_MISS_GET).await, 1);
    assert_eq!(value(&rig, COLD_SERVED).await, 0);
}

/// Asserts that no series of `/metricz` is named twice, whatever its value, and that every line
/// is a comment or `name value` or `name{labels} value`. It would catch two code paths that
/// emit the same series, which a scraper takes as a duplicate and drops.
#[tokio::test]
async fn no_series_is_emitted_twice() {
    let rig = cold_only("obj", b"abc").await;
    assert_eq!(rig.get("obj", None).await.status, StatusCode::OK);
    let text = metricz(&rig).await.body;
    let mut seen = std::collections::BTreeSet::new();
    for line in text.lines().filter(|line| !line.starts_with('#')) {
        let (name, number) = line.rsplit_once(' ').expect("name and value");
        assert!(number.parse::<u64>().is_ok(), "{line}");
        assert!(seen.insert(name.to_owned()), "{name} twice in:\n{text}");
    }
    // Every series of SERIES, and the build gauge, whose labels name the build and so are not in
    // the list.
    assert_eq!(seen.len(), SERIES.len() + 1);
}

/// Asserts that a ranged GET of a cold object while the hot tier is above its critical mark
/// reads only the range from the cold tier (bytes read equal bytes served), copies nothing and
/// counts a `critical` skip. It would catch bytes read counted as the whole object where the
/// proxy asked for the part, and a promotion that did not happen.
#[tokio::test]
async fn a_ranged_get_of_a_cold_object_with_the_hot_tier_critical_reads_the_part() {
    let rig = cold_only("obj", b"0123456789").await;
    let hot_bytes = BUDGET / 100 * 96;
    let json = format!(
        r#"{{"hot_bytes":{hot_bytes},"copy_lag":0,"time":{}}}"#,
        super::testing::NOW
    );
    rig.hot_mem
        .put("_tiering/usage", Bytes::from(json))
        .await
        .unwrap();
    let reply = rig.get("obj", Some("bytes=2-4")).await;
    assert_eq!(reply.body, "234");
    assert_eq!(value(&rig, COLD_BYTES).await, 3);
    assert_eq!(value(&rig, COLD_SERVED).await, 3);
    assert_eq!(value(&rig, COLD_HIT_GET).await, 1);
    assert_eq!(value(&rig, PROMOTIONS).await, 0);
    assert_eq!(value(&rig, &skipped("critical")).await, 1);
}
