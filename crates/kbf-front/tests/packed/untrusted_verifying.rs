//! The keys an ingest is verifying (`VerifyingKeys`): what the janitor of `cas/b/` is told not
//! to delete. Every test says the behaviour it asserts.

use std::sync::Arc;

use kbf_front::{
    Backend, Ingested, IssuedKeySet, PackKeyMint, RejectReason, UntrustedLimits, VerifyingKeys,
    ingest_set,
};
use kbf_pack::Slot;

use crate::support::{Rig, blob};
use crate::untrusted_task::{LEASE, SECRET, now};

/// Asserts that a key is held for as long as any hold on it lives: two holds on one key, one
/// dropped, still hold it; both dropped, it is free; and a key nobody holds never was.
#[test]
fn a_key_is_held_until_the_last_hold_on_it_is_dropped() {
    let registry = Arc::new(VerifyingKeys::default());
    let keys = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
    assert!(!registry.is_held("cas/b/a/x-0"));

    let first = registry.hold(keys(&["cas/b/a/x-0", "cas/b/a/x-L0"]));
    let second = registry.hold(keys(&["cas/b/a/x-0"]));
    assert!(registry.is_held("cas/b/a/x-0"));
    assert!(registry.is_held("cas/b/a/x-L0"));
    assert!(!registry.is_held("cas/b/a/y-0"));

    drop(first);
    assert!(registry.is_held("cas/b/a/x-0"), "the second hold is alive");
    assert!(!registry.is_held("cas/b/a/x-L0"));

    drop(second);
    assert!(!registry.is_held("cas/b/a/x-0"));
}

/// Asserts that an ingest holds every key of its set (pack and large) from before it asks the
/// store for anything, and lets all of them go when it ends, here refused because the pack was
/// never written. The registry is the one of the packs the ingest ran through.
#[tokio::test]
async fn an_ingest_holds_every_key_of_its_set_while_it_runs_and_none_after() {
    let rig = Rig::new().await;
    let backend = Backend::remote_over(rig.node().clone(), Some(rig.packs.clone()));
    let mint = PackKeyMint::new(SECRET).expect("a mint");
    let action = blob("an action").0;
    let pack = mint
        .mint(LEASE, &action, now(), Slot::Pack(0))
        .expect("a key");
    let large = mint
        .mint(LEASE, &action, now(), Slot::Large(0))
        .expect("a key");
    let names = vec![pack.key().to_owned(), large.key().to_owned()];
    let registry = rig.packs.verifying_keys();
    rig.store
        .watch_verifying(Arc::clone(&registry), names.clone());
    let set = IssuedKeySet::new(vec![pack], vec![large]);

    let limits = UntrustedLimits::default();
    let got = ingest_set(&rig.packs, &backend, &mint, LEASE, &set, &action, &limits).await;

    assert_eq!(got, Ingested::Reject(RejectReason::Missing));
    assert_eq!(rig.store.held_when_asked(), [true], "held when first asked");
    for name in &names {
        assert!(!registry.is_held(name), "{name} is still held");
    }
}
