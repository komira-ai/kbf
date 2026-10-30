//! `/usagez`, `/readyz` and the age of the usage object that the reconciler publishes. The time
//! is the clock of the rig, set by hand; nothing waits.

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, StatusCode};

use crate::Backend;

use super::testing::{Reply, Rig};

const MEBIBYTE: u64 = 1 << 20;

/// The limit of the rig's server: its default of two minutes.
const LIMIT_MS: u64 = 120_000;

async fn probe(rig: &Rig, path: &str) -> Reply {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .body(Full::new(Bytes::new()))
        .unwrap();
    rig.send_raw(request).await
}

async fn usagez(rig: &Rig) -> Reply {
    probe(rig, "/usagez").await
}

async fn readyz(rig: &Rig) -> Reply {
    probe(rig, "/readyz").await
}

/// Publishes a usage object as the reconciler does, stamped at `time`.
async fn publish(rig: &Rig, time: u64) {
    let json = format!(r#"{{"hot_bytes":0,"copy_lag":0,"time":{time}}}"#);
    let _ = rig.hot_mem.delete("_tiering/usage").await;
    rig.hot_mem
        .put("_tiering/usage", Bytes::from(json))
        .await
        .unwrap();
}

/// Asserts that a usage published within the limit leaves `/usagez` at 200 with its age, and one
/// published longer ago than the limit makes it 503 with a reason that says it is stale, and that
/// a fresh publication makes it 200 again. It would catch a `/usagez` that does not look at the
/// usage, an off-by-one at the limit, and one that stays 503 after the reconciler is back.
#[tokio::test(start_paused = true)]
async fn usagez_is_503_while_the_usage_is_older_than_the_limit() {
    let rig = Rig::new(MEBIBYTE).await;
    let clock = rig.hot_mem.clock();
    publish(&rig, clock.now()).await;
    let fresh = usagez(&rig).await;
    assert_eq!(fresh.status, StatusCode::OK);
    assert!(fresh.body.contains("published 0s ago"), "{}", fresh.body);
    clock.advance(LIMIT_MS);
    let edge = usagez(&rig).await;
    assert_eq!(edge.status, StatusCode::OK, "{}", edge.body);
    clock.advance(1_000);
    let stale = usagez(&rig).await;
    assert_eq!(stale.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(stale.body.contains("stale"), "{}", stale.body);
    assert!(stale.body.contains("121s"), "{}", stale.body);
    assert!(stale.body.contains("120s"), "{}", stale.body);
    publish(&rig, clock.now()).await;
    assert_eq!(usagez(&rig).await.status, StatusCode::OK);
}

/// Asserts that `/readyz` stays 200 while the usage is stale or missing, so a stopped
/// reconciler does not take a node out of a load balancer. It would catch a `/readyz` that looks
/// at the usage.
#[tokio::test(start_paused = true)]
async fn readyz_stays_200_when_the_usage_is_stale_or_missing() {
    let rig = Rig::new(MEBIBYTE).await;
    let clock = rig.hot_mem.clock();
    publish(&rig, clock.now()).await;
    clock.advance(10 * LIMIT_MS);
    assert_eq!(usagez(&rig).await.status, StatusCode::SERVICE_UNAVAILABLE);
    let stale = readyz(&rig).await;
    assert_eq!(stale.status, StatusCode::OK);
    assert_eq!(stale.body, "ready");
    let rig = Rig::new(MEBIBYTE).await;
    rig.hot_mem.clock().advance(10 * LIMIT_MS);
    assert_eq!(usagez(&rig).await.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(readyz(&rig).await.status, StatusCode::OK);
}

/// Asserts that a server that has not seen any usage is 200 on `/usagez` for the limit since it
/// started, and 503 with a reason that says it is missing after that. It would catch a server
/// that alerts before the first pass of the reconciler, and one that is quiet without usage for
/// ever.
#[tokio::test(start_paused = true)]
async fn missing_usage_is_ok_within_the_grace_and_not_after() {
    let rig = Rig::new(MEBIBYTE).await;
    let clock = rig.hot_mem.clock();
    assert_eq!(usagez(&rig).await.status, StatusCode::OK);
    clock.advance(LIMIT_MS);
    assert_eq!(usagez(&rig).await.status, StatusCode::OK);
    clock.advance(1_000);
    let missing = usagez(&rig).await;
    assert_eq!(missing.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(missing.body.contains("missing"), "{}", missing.body);
    publish(&rig, clock.now()).await;
    assert_eq!(usagez(&rig).await.status, StatusCode::OK);
}

/// Asserts that the drain still makes `/readyz` 503 `draining` while the usage is fresh, and
/// that an unreadable usage object counts as missing on `/usagez`.
#[tokio::test(start_paused = true)]
async fn the_drain_is_reported_by_readyz_and_an_unreadable_usage_counts_as_missing() {
    let rig = Rig::new(MEBIBYTE).await;
    let clock = rig.hot_mem.clock();
    publish(&rig, clock.now()).await;
    rig.server.begin_drain();
    let draining = readyz(&rig).await;
    assert_eq!(draining.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(draining.body, "draining");
    assert_eq!(usagez(&rig).await.status, StatusCode::OK);

    let rig = Rig::new(MEBIBYTE).await;
    rig.hot_mem
        .put("_tiering/usage", Bytes::from_static(b"not json"))
        .await
        .unwrap();
    rig.hot_mem.clock().advance(LIMIT_MS + 1_000);
    let unreadable = usagez(&rig).await;
    assert_eq!(unreadable.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(unreadable.body.contains("missing"), "{}", unreadable.body);
}

/// Asserts that a stale usage does not make the puts refuse: a put is still acknowledged while
/// `/usagez` says 503.
#[tokio::test(start_paused = true)]
async fn a_stale_usage_does_not_refuse_puts() {
    let rig = Rig::new(MEBIBYTE).await;
    let clock = rig.hot_mem.clock();
    publish(&rig, clock.now()).await;
    clock.advance(LIMIT_MS + 1_000);
    assert_eq!(usagez(&rig).await.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(rig.put("k", b"v").await.status, StatusCode::OK);
}

/// Asserts that without a proxy in the settings `/usagez` says the usage is not watched.
#[tokio::test(start_paused = true)]
async fn usagez_without_a_proxy_says_the_usage_is_not_watched() {
    let rig = Rig::with(MEBIBYTE, |config| config.proxy = None).await;
    rig.hot_mem.clock().advance(10 * LIMIT_MS);
    let reply = usagez(&rig).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, "usage not watched");
}
