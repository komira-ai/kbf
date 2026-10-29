//! `/readyz` and the age of the usage object that the reconciler publishes. The time is the
//! clock of the rig, set by hand; nothing waits.

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, StatusCode};

use crate::Backend;

use super::testing::{Reply, Rig};

const MEBIBYTE: u64 = 1 << 20;

/// The limit of the rig's server: its default of two minutes.
const LIMIT_MS: u64 = 120_000;

async fn readyz(rig: &Rig) -> Reply {
    let request = Request::builder()
        .method("GET")
        .uri("/readyz")
        .body(Full::new(Bytes::new()))
        .unwrap();
    rig.send_raw(request).await
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

/// Asserts that a usage published within the limit leaves `/readyz` at 200, and one published
/// longer ago than the limit makes it 503 with a reason that says it is stale, and that a fresh
/// publication makes it 200 again. It would catch a `/readyz` that does not look at the usage,
/// and one that stays 503 after the reconciler is back.
#[tokio::test(start_paused = true)]
async fn readyz_is_503_while_the_usage_is_older_than_the_limit() {
    let rig = Rig::new(MEBIBYTE).await;
    let clock = rig.hot_mem.clock();
    publish(&rig, clock.now()).await;
    assert_eq!(readyz(&rig).await.status, StatusCode::OK);
    clock.advance(LIMIT_MS);
    let edge = readyz(&rig).await;
    assert_eq!(edge.status, StatusCode::OK, "{}", edge.body);
    clock.advance(1_000);
    let stale = readyz(&rig).await;
    assert_eq!(stale.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(stale.body.contains("stale"), "{}", stale.body);
    assert!(stale.body.contains("121s"), "{}", stale.body);
    assert!(stale.body.contains("120s"), "{}", stale.body);
    publish(&rig, clock.now()).await;
    assert_eq!(readyz(&rig).await.status, StatusCode::OK);
}

/// Asserts that a server that has not seen any usage is ready for the limit since it started,
/// and 503 with a reason that says it is missing after that. It would catch a server that is
/// never ready before the first pass of the reconciler, and one that is ready without usage for
/// ever.
#[tokio::test(start_paused = true)]
async fn missing_usage_is_ready_within_the_grace_and_not_after() {
    let rig = Rig::new(MEBIBYTE).await;
    let clock = rig.hot_mem.clock();
    assert_eq!(readyz(&rig).await.status, StatusCode::OK);
    clock.advance(LIMIT_MS);
    assert_eq!(readyz(&rig).await.status, StatusCode::OK);
    clock.advance(1_000);
    let missing = readyz(&rig).await;
    assert_eq!(missing.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(missing.body.contains("missing"), "{}", missing.body);
    publish(&rig, clock.now()).await;
    assert_eq!(readyz(&rig).await.status, StatusCode::OK);
}

/// Asserts that the drain still says 503 `draining` when the usage is fresh, and that an
/// unreadable usage object counts as missing. It would catch a usage check that hides the drain.
#[tokio::test(start_paused = true)]
async fn the_drain_is_reported_first_and_an_unreadable_usage_counts_as_missing() {
    let rig = Rig::new(MEBIBYTE).await;
    let clock = rig.hot_mem.clock();
    publish(&rig, clock.now()).await;
    rig.server.begin_drain();
    let draining = readyz(&rig).await;
    assert_eq!(draining.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(draining.body, "draining");

    let rig = Rig::new(MEBIBYTE).await;
    let clock = rig.hot_mem.clock();
    rig.hot_mem
        .put("_tiering/usage", Bytes::from_static(b"not json"))
        .await
        .unwrap();
    clock.advance(LIMIT_MS + 1_000);
    let unreadable = readyz(&rig).await;
    assert_eq!(unreadable.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(unreadable.body.contains("missing"), "{}", unreadable.body);
}

/// Asserts that a stale usage does not make the puts refuse: a put is still acknowledged while
/// `/readyz` says 503. It pins the decision to report the state and not to refuse.
#[tokio::test(start_paused = true)]
async fn a_stale_usage_does_not_refuse_puts() {
    let rig = Rig::new(MEBIBYTE).await;
    let clock = rig.hot_mem.clock();
    publish(&rig, clock.now()).await;
    clock.advance(LIMIT_MS + 1_000);
    assert_eq!(readyz(&rig).await.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(rig.put("k", b"v").await.status, StatusCode::OK);
}

/// Asserts that without a proxy in the settings `/readyz` says nothing of the usage.
#[tokio::test(start_paused = true)]
async fn readyz_without_a_proxy_ignores_the_usage() {
    let rig = Rig::with(MEBIBYTE, |config| config.proxy = None).await;
    let clock = rig.hot_mem.clock();
    clock.advance(10 * LIMIT_MS);
    assert_eq!(readyz(&rig).await.status, StatusCode::OK);
}
