//! The lease that makes one reconciler the runner.

use tierstore::{Backend, Error, Op, PassReport};

use crate::support::{Rig, assert_absent, body, keys, text};

fn down() -> Error {
    Error::Unavailable("on purpose".to_owned())
}

/// Asserts that a lease body is the holder, a nonce of 32 hex digits and the count, one to a line.
fn assert_lease(body: &str, holder: &str, count: &str) {
    let lines: Vec<&str> = body.split('\n').collect();
    assert_eq!(lines.len(), 3, "{body:?}");
    assert_eq!(lines[0], holder);
    assert!(
        lines[1].len() == 32 && lines[1].bytes().all(|b| b.is_ascii_hexdigit()),
        "{body:?}"
    );
    assert_eq!(lines[2], count);
}

/// Asserts that the first reconciler to pass takes the lease and works, that a second finds it
/// held and does nothing at all, and that the holder renews it on its next pass.
#[tokio::test(start_paused = true)]
async fn one_reconciler_holds_the_lease_and_the_other_does_nothing() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("a", body("1")).await.unwrap();
    let mut one = rig.reconciler("r1", 1000);
    let mut two = rig.reconciler("r2", 1000);

    let report = one.pass().await;
    assert!(report.held);
    assert_eq!(report.copied, 1);
    assert_lease(&text(&*rig.hot_mem, "_tiering/lease").await, "r1", "1");

    proxy.put("b", body("2")).await.unwrap();
    assert_eq!(two.pass().await, PassReport::default());
    assert_eq!(keys(&*rig.cold_mem).await, ["a"]);
    assert_eq!(two.hot_bytes(), 0);

    assert!(one.pass().await.held);
    assert_lease(&text(&*rig.hot_mem, "_tiering/lease").await, "r1", "2");
    assert_eq!(keys(&*rig.cold_mem).await, ["a", "b"]);
}

/// Asserts that a lease is taken over once its object is as old as the ttl and not before, and
/// that the holder that lost it fails its next renewal, stops, and forgets what it knew.
#[tokio::test(start_paused = true)]
async fn an_expired_lease_is_taken_over_and_the_old_holder_stops() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("a", body("1")).await.unwrap();
    let mut one = rig.reconciler("r1", 1000);
    let mut two = rig.reconciler("r2", 1000);
    assert!(one.pass().await.held);
    assert_eq!(one.hot_bytes(), 1);

    rig.clock.set(29_999);
    assert!(!two.pass().await.held);
    rig.clock.set(30_000);
    assert!(two.pass().await.held);
    let lease = text(&*rig.hot_mem, "_tiering/lease").await;
    assert!(lease.starts_with("r2\n"));

    let report = one.pass().await;
    assert_eq!(report, PassReport::default());
    assert_eq!(one.hot_bytes(), 0);
    assert!(two.pass().await.held);
}

/// Asserts that a holder that stops gives the lease up, so another takes it at once, and that
/// stopping a reconciler that does not hold it deletes nothing.
#[tokio::test(start_paused = true)]
async fn a_stopped_reconciler_gives_the_lease_up() {
    let rig = Rig::new();
    let mut one = rig.reconciler("r1", 1000);
    let mut two = rig.reconciler("r2", 1000);
    assert!(one.pass().await.held);

    two.stop().await;
    assert_eq!(rig.hot.calls(Op::Delete), 0);
    assert!(!two.pass().await.held);

    one.stop().await;
    assert_absent(&*rig.hot_mem, "_tiering/lease").await;
    assert!(two.pass().await.held);
}

/// Asserts that a holder that lost the lease without noticing leaves the lease of the holder
/// that took it over in place when it stops.
#[tokio::test(start_paused = true)]
async fn a_holder_that_lost_the_lease_leaves_the_new_holders_lease_when_it_stops() {
    let rig = Rig::new();
    let mut one = rig.reconciler("r1", 1000);
    let mut two = rig.reconciler("r2", 1000);
    assert!(one.pass().await.held);
    rig.clock.set(30_000);
    assert!(two.pass().await.held);

    one.stop().await;
    assert_eq!(rig.hot.calls(Op::Delete), 0);
    let lease = text(&*rig.hot_mem, "_tiering/lease").await;
    assert!(lease.starts_with("r2\n"));
}

/// Asserts that a holder that cannot read the lease object when it stops leaves it in place,
/// since it cannot tell whether the lease is its own.
#[tokio::test(start_paused = true)]
async fn a_holder_that_cannot_read_the_lease_when_it_stops_leaves_it_in_place() {
    let rig = Rig::new();
    let mut one = rig.reconciler("r1", 1000);
    assert!(one.pass().await.held);
    rig.hot.fail_next_on(Op::Head, "_tiering/lease", down());

    one.stop().await;
    assert_eq!(rig.hot.calls(Op::Delete), 0);
    assert_lease(&text(&*rig.hot_mem, "_tiering/lease").await, "r1", "1");
}

/// Asserts that a reconciler that cannot create the lease object does not hold it, and holds
/// it on the next pass.
#[tokio::test(start_paused = true)]
async fn a_lease_the_hot_tier_fails_to_create_is_not_held() {
    let rig = Rig::new();
    let mut one = rig.reconciler("r1", 1000);
    rig.hot.fail_next_on(Op::Put, "_tiering/lease", down());

    assert!(!one.pass().await.held);
    assert!(one.pass().await.held);
}

/// Asserts that a reconciler that created the lease but could not read the content tag of its
/// object does not hold it, cannot take it while it is fresh, and does once it has expired.
#[tokio::test(start_paused = true)]
async fn a_lease_whose_tag_cannot_be_read_is_not_held_until_it_expires() {
    let rig = Rig::new();
    let mut one = rig.reconciler("r1", 1000);
    rig.hot.fail_next_on(Op::Head, "_tiering/lease", down());

    assert!(!one.pass().await.held);
    assert!(!one.pass().await.held);
    rig.clock.set(30_000);
    assert!(one.pass().await.held);
}

/// Asserts that a take over fails, and is tried again later, when the hot tier fails the read
/// of the lease object, or fails the replacement of the expired object.
#[tokio::test(start_paused = true)]
async fn a_take_over_the_hot_tier_fails_is_tried_again() {
    let rig = Rig::new();
    let mut one = rig.reconciler("r1", 1000);
    let mut two = rig.reconciler("r2", 1000);
    assert!(one.pass().await.held);
    rig.clock.set(30_000);

    let hot = &rig.hot;
    hot.fail_next_on(Op::Head, "_tiering/lease", down());
    assert!(!two.pass().await.held);
    hot.fail_next_on(Op::PutIfMatch, "_tiering/lease", down());
    assert!(!two.pass().await.held);
    assert!(two.pass().await.held);
}

/// Asserts that a holder whose renewal fails (the hot tier does not answer) stops holding the
/// lease at once, and holds it again only when it can take it, after it expired.
#[tokio::test(start_paused = true)]
async fn a_failed_renewal_stops_the_holder() {
    let rig = Rig::new();
    let mut one = rig.reconciler("r1", 1000);
    assert!(one.pass().await.held);

    let hot = &rig.hot;
    hot.fail_next_on(Op::PutIfMatch, "_tiering/lease", down());
    assert!(!one.pass().await.held);
    assert!(!one.pass().await.held);
    rig.clock.set(30_000);
    assert!(one.pass().await.held);
}
