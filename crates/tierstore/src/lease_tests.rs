//! The lease body: two holders of one name, a restarted process and the one it replaced, are not
//! mistaken for each other by the content tag.

use std::sync::Arc;

use crate::journal::LEASE_KEY;
use crate::lease::Lease;
use crate::{Backend, MemoryBackend, TestClock};

const TTL: u64 = 1000;

/// A hot tier on a clock the test sets, and two leases of the holder name `p` over it, as a
/// process and its restart are.
fn two_holders() -> (Arc<MemoryBackend>, Arc<TestClock>, Lease, Lease) {
    let clock = Arc::new(TestClock::new());
    let hot = Arc::new(MemoryBackend::with_clock(Arc::clone(&clock)));
    let a = Lease::new(hot.clone(), clock.clone(), "p", TTL);
    let b = Lease::new(hot.clone(), clock.clone(), "p", TTL);
    (hot, clock, a, b)
}

async fn tag(hot: &MemoryBackend) -> Option<String> {
    hot.head(LEASE_KEY).await.ok().map(|meta| meta.tag)
}

/// Asserts that a holder whose lease expired and was taken over by a restart with the same name
/// cannot renew the new holder's lease. A takes the lease (its first `acquire` and a renewal
/// write generations 1 and 2) and holds the tag of `p\n2`. B's take over also writes generation
/// 2, because its `acquire` counts a body for the create-only put that fails and the renewal
/// that replaces the object counts the next. Defect it catches: A's body and B's are the same
/// bytes, so the tag is the same, and A's renewal replaces the new holder's object.
#[tokio::test]
async fn a_stale_holder_of_the_same_name_cannot_renew_the_new_holders_lease() {
    let (hot, clock, mut a, mut b) = two_holders();
    assert!(a.ensure().await);
    assert!(a.ensure().await);
    let stale = tag(&hot).await.unwrap();

    clock.set(30_000);
    assert!(b.ensure().await);
    let current = tag(&hot).await.unwrap();
    assert_ne!(current, stale);

    assert!(!a.ensure().await);
    assert_eq!(tag(&hot).await.unwrap(), current);
    assert!(b.ensure().await);
}

/// Asserts that the old holder's release leaves the new holder's lease in place. Defect it
/// catches: the old holder sees a tag equal to its own and deletes the new holder's object. A
/// is at generation 2 when it is taken over, as B is when its take over writes, so the bodies
/// and the tags are equal (see the test above).
#[tokio::test]
async fn a_stale_holder_of_the_same_name_cannot_release_the_new_holders_lease() {
    let (hot, clock, mut a, mut b) = two_holders();
    assert!(a.ensure().await);
    assert!(a.ensure().await);

    clock.set(30_000);
    assert!(b.ensure().await);
    let current = tag(&hot).await.unwrap();

    a.release().await;
    assert_eq!(tag(&hot).await, Some(current));
}

/// Asserts that two leases of one holder name write different bodies for the same count of
/// writes. Defect it catches: a body made of the name and the count alone, which every process
/// of that name reproduces.
#[tokio::test]
async fn two_leases_of_one_name_write_different_first_bodies() {
    let (hot, _clock, mut a, _b) = two_holders();
    assert!(a.ensure().await);
    let first = hot.get(LEASE_KEY, None).await.unwrap();

    let other_clock = Arc::new(TestClock::new());
    let other_hot = Arc::new(MemoryBackend::with_clock(Arc::clone(&other_clock)));
    let mut c = Lease::new(other_hot.clone(), other_clock, "p", TTL);
    assert!(c.ensure().await);
    let second = other_hot.get(LEASE_KEY, None).await.unwrap();

    assert_ne!(first, second);
}
