//! The keys an ingest is verifying (`VerifyingKeys`) and the janitor of `cas/b/` that must not
//! delete them: what is held, what is claimed for a delete, and the ingests that race a delete
//! in each order. Every test says the behaviour it asserts.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_front::{
    Backend, Claimed, Ingested, IssuedKeySet, PackKeyMint, Packs, RejectReason, UntrustedLimits,
    VerifyingKeys, ingest_set,
};
use kbf_pack::{MIN_GRACE, Slot, SweepConfig, SweepGates, SweepReport, Sweeper, task_key_age};
use kbf_pack_index::PackIndex as _;
use kbf_types::Digest;

use crate::support::{Rig, Spy, blob};
use crate::untrusted_task::{LEASE, SECRET, now};

/// The keys of one run (a pack and a large object) minted at [`now`], the packs and backend they
/// are ingested through, and a janitor over the same store, index and registry.
struct Run {
    rig: Rig,
    backend: Backend,
    mint: PackKeyMint,
    action: Digest,
    set: IssuedKeySet,
}

impl Run {
    async fn new() -> Self {
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
        let set = IssuedKeySet::new(vec![pack], vec![large]);
        Self {
            rig,
            backend,
            mint,
            action,
            set,
        }
    }

    fn names(&self) -> Vec<String> {
        let all = self.set.packs().iter().chain(self.set.large());
        all.map(|key| key.key().to_owned()).collect()
    }

    /// The ingest of the set through `packs`.
    async fn ingest(&self, packs: &Packs) -> Ingested {
        let limits = UntrustedLimits::default();
        let (mint, action, set) = (&self.mint, &self.action, &self.set);
        ingest_set(packs, &self.backend, mint, LEASE, set, action, &limits).await
    }

    /// A sweep of the store `grace` after the keys were minted, claiming through the registry of
    /// the packs, over the index of the rig.
    async fn sweep(&self) -> SweepReport {
        self.sweep_at(MIN_GRACE * 2).await
    }

    /// A sweep made `age` after the keys were minted.
    async fn sweep_at(&self, age: Duration) -> SweepReport {
        sweeper(&self.rig).sweep(now() + age).await.expect("swept")
    }
}

/// The janitor of the rig: the age a task key says, the registry's claim, the rig's index.
fn sweeper(rig: &Rig) -> Sweeper<Spy> {
    let registry = rig.packs.verifying_keys();
    let held = Arc::clone(&registry);
    let gates = SweepGates {
        age_of: Box::new(task_key_age),
        is_leader: Box::new(|| true),
        verifying: Box::new(move |key| held.is_held(key)),
        claim: Box::new(move |key| {
            registry
                .claim(key)
                .map(|claim| Box::new(claim) as kbf_pack::Claim)
        }),
    };
    let index: Arc<dyn kbf_pack_index::PackIndex> = rig.index.clone();
    let config = SweepConfig {
        grace: MIN_GRACE,
        ..SweepConfig::default()
    };
    Sweeper::new(Arc::clone(&rig.store), index, config, gates)
}

/// Asserts that a key is held for as long as any hold on it lives: two holds on one key, one
/// dropped, still hold it; both dropped, it is free; and a key nobody holds never was.
#[test]
fn a_key_is_held_until_the_last_hold_on_it_is_dropped() {
    let registry = Arc::new(VerifyingKeys::default());
    let keys = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
    assert!(!registry.is_held("cas/b/a/x-0"));

    let first = registry
        .hold(keys(&["cas/b/a/x-0", "cas/b/a/x-L0"]))
        .expect("free");
    let second = registry.hold(keys(&["cas/b/a/x-0"])).expect("free");
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
    let run = Run::new().await;
    let names = run.names();
    let registry = run.rig.packs.verifying_keys();
    run.rig
        .store
        .watch_verifying(Arc::clone(&registry), names.clone());

    let got = run.ingest(&run.rig.packs).await;

    assert_eq!(got, Ingested::Reject(RejectReason::Missing));
    assert_eq!(
        run.rig.store.held_when_asked(),
        [true],
        "held when first asked"
    );
    for name in &names {
        assert!(!registry.is_held(name), "{name} is still held");
    }
}

/// Asserts the two sides of a claim: a free key is claimed once and not twice, a claimed key
/// cannot be held (and the refusal names it), a held key cannot be claimed, and a claim let go
/// frees the key to be held.
#[test]
fn a_claimed_key_cannot_be_held_and_a_held_key_cannot_be_claimed() {
    let registry = Arc::new(VerifyingKeys::default());
    let (a, b) = ("cas/b/a/x-0".to_owned(), "cas/b/a/x-1".to_owned());

    let claim = registry.claim(&a).expect("free");
    assert!(registry.claim(&a).is_none(), "claimed twice");
    let refused = registry.hold(vec![b.clone(), a.clone()]).err();
    assert_eq!(refused, Some(Claimed(a.clone())));
    assert!(!registry.is_held(&b), "a refused hold takes nothing");
    drop(claim);

    let hold = registry.hold(vec![a.clone()]).expect("free again");
    assert!(registry.claim(&a).is_none(), "claimed while held");
    assert!(registry.claim(&b).is_some(), "another key is free");
    drop(hold);
    assert!(registry.claim(&a).is_some());
}

/// Asserts that an ingest refuses a key as old as the grace bound with `Expired`, counts it
/// under that reason, and holds nothing; and that a key a minute younger is not refused for its
/// age. The packs judge age by a clock the test sets, so no time passes.
#[tokio::test]
async fn an_ingest_refuses_a_key_as_old_as_the_grace_bound() {
    let run = Run::new().await;
    let at = |after: Duration| {
        run.rig
            .packs
            .clone()
            .with_clock(Arc::new(move || now() + after))
    };

    let old = at(MIN_GRACE);
    assert_eq!(
        run.ingest(&old).await,
        Ingested::Reject(RejectReason::Expired)
    );
    let metrics = run.rig.packs.metrics();
    assert_eq!(metrics.ingest_rejects(RejectReason::Expired), 1);
    for name in run.names() {
        assert!(!run.rig.packs.verifying_keys().is_held(&name));
    }

    let almost = at(MIN_GRACE - Duration::from_secs(60));
    assert_eq!(
        run.ingest(&almost).await,
        Ingested::Reject(RejectReason::Missing)
    );
    assert_eq!(metrics.ingest_rejects(RejectReason::Expired), 1);
}

/// A hook for the store of a rig that stores what its future produces in `slot`.
fn once<T: Send + 'static>(
    slot: &Arc<Mutex<Vec<T>>>,
    make: impl Fn() -> futures::future::BoxFuture<'static, T> + Send + Sync + 'static,
) -> crate::support::Hook {
    let slot = Arc::clone(slot);
    Arc::new(move |_key| {
        let (slot, made) = (Arc::clone(&slot), make());
        Box::pin(async move {
            let got = made.await;
            slot.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(got);
        })
    })
}

/// Asserts the order janitor first, ingest second: an ingest that begins after the janitor has
/// claimed the key (here inside the delete, after the index's last answer) is refused, never
/// reads the object and records nothing, so the index never names the deleted object.
#[tokio::test]
async fn an_ingest_that_starts_inside_the_delete_is_refused_and_records_nothing() {
    let run = Arc::new(Run::new().await);
    for name in run.names() {
        run.rig.store.seed(&name, b"an orphan").await;
    }
    let seen: Arc<Mutex<Vec<Ingested>>> = Arc::default();
    let racing = Arc::clone(&run);
    run.rig.store.on_delete(once(&seen, move || {
        let run = Arc::clone(&racing);
        Box::pin(async move { run.ingest(&run.rig.packs.clone()).await })
    }));

    let report = run.sweep().await;

    assert_eq!(report.deleted, 2);
    let seen = std::mem::take(&mut *seen.lock().unwrap_or_else(PoisonError::into_inner));
    assert_eq!(seen.len(), 2, "an ingest began inside each delete");
    for got in seen.iter() {
        assert_eq!(*got, Ingested::Reject(RejectReason::Claimed));
    }
    assert!(run.rig.store.keys().await.is_empty());
    for name in run.names() {
        assert!(run.rig.index.pack(&name).expect("asked").is_none());
    }
}

/// Asserts the order ingest first, janitor second: a sweep that runs while an ingest is asking
/// the store for its first length (holding the keys) keeps every key and counts it as verifying,
/// and the sweep after the ingest has ended deletes them.
#[tokio::test]
async fn a_sweep_inside_an_ingest_keeps_its_keys_and_one_after_it_deletes_them() {
    let run = Arc::new(Run::new().await);
    for name in run.names() {
        run.rig.store.seed(&name, b"an orphan").await;
    }
    let seen: Arc<Mutex<Vec<SweepReport>>> = Arc::default();
    let sweeping = Arc::clone(&run);
    run.rig.store.on_len(once(&seen, move || {
        let run = Arc::clone(&sweeping);
        Box::pin(async move { run.sweep().await })
    }));

    let got = run.ingest(&run.rig.packs).await;

    assert!(matches!(got, Ingested::Reject(_)), "{got:?}");
    let seen = std::mem::take(&mut *seen.lock().unwrap_or_else(PoisonError::into_inner));
    assert_eq!((seen[0].verifying, seen[0].deleted), (2, 0));
    assert_eq!(run.rig.store.keys().await.len(), 2);
    run.rig.store.on_len(Arc::new(|_| Box::pin(async {})));

    let after = run.sweep().await;

    assert_eq!(after.deleted, 2);
    assert!(run.rig.store.keys().await.is_empty());
}

/// Asserts that a hold taken after the page's answer from the index and before the janitor's
/// claim (an ingest that starts in that gap, with the real registry) keeps the key: the claim
/// fails, the key is counted verifying and is not deleted, and once the hold is let go a sweep
/// deletes it. The hold is taken by the index's hook the first time it answers.
#[tokio::test]
async fn a_hold_taken_between_the_pages_answer_and_the_claim_keeps_the_key() {
    let run = Run::new().await;
    let name = run.names().remove(0);
    run.rig.store.seed(&name, b"an orphan").await;
    let registry = run.rig.packs.verifying_keys();
    let holds: Arc<Mutex<Vec<kbf_front::Held>>> = Arc::default();
    let (taken, calls) = (Arc::clone(&holds), std::sync::atomic::AtomicU32::new(0));
    let held_name = name.clone();
    run.rig.index.after_unreferenced(Arc::new(move || {
        // The first answer is the page's; the ones after are the re-asks.
        if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            let hold = registry
                .hold(vec![held_name.clone()])
                .expect("not claimed yet");
            taken
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(hold);
        }
    }));

    let report = run.sweep().await;

    assert_eq!((report.verifying, report.deleted), (1, 0));
    assert_eq!(run.rig.store.keys().await, vec![name.clone()]);
    holds.lock().unwrap_or_else(PoisonError::into_inner).clear();

    let after = run.sweep().await;

    assert_eq!(after.deleted, 1);
    assert!(run.rig.store.keys().await.is_empty());
}

/// Asserts, for ages on both sides of [`MIN_GRACE`] and at it, that the ingest refuses a key
/// exactly when the sweeper (whose grace is `MIN_GRACE`) deletes it: no age is both ingestible and
/// deletable, and none is neither.
#[tokio::test]
async fn the_ingest_refuses_a_key_exactly_when_the_sweep_would_delete_it() {
    let minute = Duration::from_secs(60);
    for age in [MIN_GRACE - minute, MIN_GRACE, MIN_GRACE + minute] {
        let run = Run::new().await;
        let name = run.names().remove(0);
        run.rig.store.seed(&name, b"an orphan").await;

        let deleted = run.sweep_at(age).await.deleted > 0;
        let packs = run
            .rig
            .packs
            .clone()
            .with_clock(Arc::new(move || now() + age));
        let got = run.ingest(&packs).await;
        let refused = got == Ingested::Reject(RejectReason::Expired);

        assert_eq!(refused, deleted, "at {age:?}");
        assert_eq!(deleted, age >= MIN_GRACE, "at {age:?}");
    }
}
