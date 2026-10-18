//! Tests of the slot, gate, hold guard and permit rules.

use super::*;
use std::sync::atomic::AtomicBool;

const HOLD_MAX: Duration = Duration::from_secs(20);

fn live() -> ActiveSlot {
    let s = ActiveSlot::new();
    s.open(Slot::A);
    s.set_active(Slot::A).expect("A is open");
    s
}

fn bound(s: &ActiveSlot) -> Permit {
    match s.bind() {
        Bind::Bound(p) => p,
        other => panic!("expected a permit, got {other:?}"),
    }
}

#[test]
fn streams_stay_on_the_slot_they_bound_to() {
    let s = live();
    let old: Vec<Permit> = (0..200).map(|_| bound(&s)).collect();
    s.open(Slot::B);
    s.set_active(Slot::B).expect("B is open");
    assert_eq!(bound(&s).slot(), Slot::B);
    assert!(old.iter().all(|p| p.slot() == Slot::A));
    assert_eq!(s.open_permits(Slot::A), 200);
    assert_eq!(s.retire(Slot::A), Retire::Busy(200));
    drop(old);
    assert_eq!(s.retire(Slot::A), Retire::Closed);
    assert_eq!(s.retire(Slot::A), Retire::AlreadyClosed, "only the first call signals");
}

#[test]
fn the_active_slot_is_not_retired_and_a_never_opened_one_says_so() {
    let s = live();
    assert_eq!(s.retire(Slot::A), Retire::Current);
    assert_eq!(s.retire(Slot::B), Retire::NeverOpened);
    s.open(Slot::B);
    assert_eq!(s.retire(Slot::B), Retire::Closed);
    assert_eq!(s.retire(Slot::B), Retire::AlreadyClosed);
    assert_eq!(s.retire(Slot::B), Retire::AlreadyClosed, "a re-run changes nothing");
    // Reopened and retired again is a new child: Closed once more.
    s.open(Slot::B);
    assert_eq!(s.retire(Slot::B), Retire::Closed);
}

#[test]
fn can_retire_is_a_probe_that_latches_nothing() {
    let s = live();
    s.open(Slot::B);
    assert!(!s.can_retire(Slot::A), "active");
    assert!(s.can_retire(Slot::B));
    assert!(s.can_retire(Slot::B), "asking twice changes nothing");
    // Still open: a rollback can make it active.
    s.set_active(Slot::B).expect("still open");
    let held = bound(&s);
    assert!(!s.can_retire(Slot::B), "active and busy");
    s.set_active(Slot::A).expect("A is open");
    assert!(!s.can_retire(Slot::B), "a permit is open");
    drop(held);
    assert!(s.can_retire(Slot::B));
    assert!(!ActiveSlot::new().can_retire(Slot::A), "never opened");
    assert_eq!(s.retire(Slot::B), Retire::Closed);
    assert!(!s.can_retire(Slot::B), "closed already");
}

#[test]
fn a_closed_slot_cannot_become_active_or_bind() {
    let s = ActiveSlot::new();
    assert_eq!(s.set_active(Slot::B), Err(SwapError::Closed));
    assert!(matches!(s.bind(), Bind::NoActive));
    let t = live();
    t.open(Slot::B);
    t.set_active(Slot::B).expect("open");
    assert_eq!(t.retire(Slot::A), Retire::Closed);
    assert_eq!(t.set_active(Slot::A), Err(SwapError::Closed));
    // A stale active pointer at a closed slot is NoActive, not a spin.
    t.inner.gate.store(Slot::A.gate_bits(), Ordering::Release);
    assert!(matches!(t.bind(), Bind::NoActive));
}

#[test]
fn hold_defers_the_bind_until_the_guard_releases() {
    let s = live();
    let _guard = s.hold(HOLD_MAX).expect("first hold");
    assert!(s.holding());
    assert!(matches!(s.hold(HOLD_MAX), Err(AlreadyHolding)));
    assert_eq!(AlreadyHolding.to_string(), "the gate is already holding");
    assert!(matches!(
        s.bind(),
        Bind::Held { hold_max, .. } if hold_max == HOLD_MAX
    ));
    s.open(Slot::B);
    s.set_active(Slot::B).expect("open");
    assert!(s.holding());
    s.release_into(Slot::B).expect("open");
    assert!(!s.holding());
    assert_eq!(bound(&s).slot(), Slot::B);
    assert_eq!(Slot::A.other(), Slot::B);
    assert!(format!("{s:?}").contains("ActiveSlot"));
    assert!(format!("{:?}", bound(&s)).contains("Permit"));
    assert_eq!(ActiveSlot::default().active(), None);
}

#[test]
fn a_rejected_hold_does_not_rewrite_the_bound_in_force() {
    let s = live();
    let guard = s.hold(Duration::from_secs(300)).expect("the all-in-one hold");
    assert!(matches!(s.hold(Duration::from_secs(20)), Err(AlreadyHolding)));
    assert!(
        matches!(s.bind(), Bind::Held { hold_max, .. } if hold_max == Duration::from_secs(300)),
        "a stray hold(20 s) must not shorten the 300 s in force"
    );
    // The gate word still reads as one hold of the first guard's.
    drop(guard);
    assert!(!s.holding());
    // The next hold sets its own bound, and an absurd one saturates instead of wrapping.
    let _next = s.hold(Duration::from_secs(20)).expect("free again");
    assert!(matches!(s.bind(), Bind::Held { hold_max, .. } if hold_max == HOLD_MAX));
    s.release_into(Slot::A).expect("open");
    let _huge = s.hold(Duration::MAX).expect("free");
    assert!(matches!(
        s.bind(),
        Bind::Held { hold_max, .. } if hold_max == Duration::from_millis(u64::from(u32::MAX))
    ));
}

#[test]
fn a_release_after_held_is_never_missed() {
    let s = live();
    let guard = s.hold(HOLD_MAX).expect("hold");
    let Bind::Held { epoch, .. } = s.bind() else {
        panic!("holding");
    };
    // The release lands after the stream saw Held and before it parks.
    assert_eq!(s.epoch(), epoch);
    drop(guard);
    assert_ne!(s.epoch(), epoch, "the waiter's condition is already true");
    assert!(!s.holding(), "and the gate is open by the time the epoch moved");
    // A second hold and a release into a slot moves it again.
    let before = s.epoch();
    let _again = s.hold(HOLD_MAX).expect("again");
    s.release_into(Slot::A).expect("open");
    assert_ne!(s.epoch(), before);
}

#[test]
fn a_dropped_guard_releases_into_nothing_and_is_counted() {
    let s = live();
    let epoch = s.epoch();
    {
        let _guard = s.hold(HOLD_MAX).expect("hold");
        assert!(s.holding());
        assert_eq!(s.active(), Some(Slot::A));
    }
    assert!(!s.holding());
    assert_eq!(s.active(), None, "not released to the active slot, nor to a default");
    assert!(matches!(s.bind(), Bind::NoActive), "held streams fail UNAVAILABLE");
    assert_ne!(s.epoch(), epoch, "and they were woken to find out");
    assert_eq!(s.abandoned_holds(), 1);
    // The supervisor can still set a slot afterwards.
    s.release_into(Slot::A).expect("open");
    assert_eq!(bound(&s).slot(), Slot::A);
    assert_eq!(s.abandoned_holds(), 1, "a release into a slot is not abandonment");
}

#[test]
fn a_guard_dropped_on_a_panic_still_rings_the_doorbell() {
    let s = live();
    let mut bell = s.subscribe();
    let guard = s.hold(HOLD_MAX).expect("hold");
    assert!(!bell.has_changed().expect("open"));
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _guard = guard;
        panic!("the supervisor died mid-swap");
    }));
    assert!(unwound.is_err());
    assert!(bell.has_changed().expect("open"), "a parked waiter would wake");
    assert_eq!(*bell.borrow_and_update(), s.epoch(), "and the value is the epoch");
    assert!(!s.holding());
    assert_eq!(s.abandoned_holds(), 1);
    // release_into rings it too, and only when a hold was in force.
    let _h = s.hold(HOLD_MAX).expect("hold");
    s.release_into(Slot::A).expect("open");
    assert!(bell.has_changed().expect("open"));
    bell.borrow_and_update();
    s.release_into(Slot::A).expect("open");
    assert!(!bell.has_changed().expect("open"), "nothing was held: nothing rung");
}

#[test]
fn release_into_sets_the_slot_and_releases_in_one_step() {
    let s = live();
    s.open(Slot::B);
    let guard = s.hold(HOLD_MAX).expect("hold");
    let Bind::Held { epoch, .. } = s.bind() else {
        panic!("holding");
    };
    s.release_into(Slot::B).expect("open");
    assert_eq!(s.active(), Some(Slot::B));
    assert!(!s.holding());
    assert_ne!(s.epoch(), epoch);
    assert_eq!(bound(&s).slot(), Slot::B, "every held stream binds into B");
    // The guard of the ended hold is inert, and so is a guard of an earlier hold against a
    // later one.
    let later = s.hold(HOLD_MAX).expect("a new hold");
    drop(guard);
    assert!(s.holding(), "the old guard did not end the new hold");
    assert_eq!(s.abandoned_holds(), 0);
    drop(later);
    assert_eq!(s.abandoned_holds(), 1);
}

#[test]
fn release_into_with_nothing_held_only_sets_the_active_slot() {
    let s = live();
    s.open(Slot::B);
    let epoch = s.epoch();
    s.release_into(Slot::B).expect("open");
    assert_eq!(s.active(), Some(Slot::B));
    assert_eq!(s.epoch(), epoch, "no hold, no release");
    s.release_into(Slot::A).expect("open");
    assert_eq!(s.active(), Some(Slot::A));
}

#[test]
fn release_into_a_closed_slot_is_refused_and_the_hold_stays() {
    let s = live();
    let guard = s.hold(Duration::from_secs(300)).expect("hold");
    assert_eq!(s.release_into(Slot::B), Err(SwapError::Closed));
    assert!(s.holding());
    assert_eq!(s.active(), Some(Slot::A));
    assert!(matches!(
        s.bind(),
        Bind::Held { hold_max, .. } if hold_max == Duration::from_secs(300)
    ));
    s.release_into(Slot::A).expect("open");
    drop(guard);
    assert_eq!(s.abandoned_holds(), 0);
}

#[test]
fn a_rollback_releases_into_the_old_slot() {
    let s = live();
    s.open(Slot::B);
    let _guard = s.hold(HOLD_MAX).expect("hold");
    // The candidate B failed its takeover; A was resumed and is still open.
    assert_eq!(s.retire(Slot::B), Retire::Closed);
    s.release_into(Slot::A).expect("A is open");
    assert_eq!(bound(&s).slot(), Slot::A);
}

#[test]
fn all_in_one_failed_start_restarts_old_with_new_generation() {
    let s = live();
    assert_eq!(s.generation(Slot::A), 1);
    let straggler = bound(&s);
    let guard = s.hold(Duration::from_secs(300)).expect("all-in-one hold");
    // Stop the old daemon: nothing is active, and the slot is not yet retired.
    s.clear_active();
    assert!(s.holding(), "the hold survives");
    assert_eq!(s.retire(Slot::A), Retire::Busy(1), "a stream of the dead child is draining");
    // Without the retire a restart would reuse the dead process's generation.
    assert_eq!(s.open(Slot::A), 1, "the trap: open on an open slot is the same child");
    drop(straggler);
    assert_eq!(s.retire(Slot::A), Retire::Closed);
    assert_eq!(s.retire(Slot::A), Retire::AlreadyClosed);
    // The new daemon failed; the supervisor restarts the old one in A.
    assert_eq!(s.open(Slot::A), 2, "a new child, a new generation");
    s.release_into(Slot::A).expect("open");
    let p = bound(&s);
    assert_eq!((p.slot(), p.generation()), (Slot::A, 2));
    drop(guard);
    assert_eq!(s.abandoned_holds(), 0);
}

#[test]
fn both_children_dead_then_a_fresh_start_is_released_into() {
    let s = live();
    s.open(Slot::B);
    let _guard = s.hold(Duration::from_secs(300)).expect("hold");
    s.clear_active();
    assert_eq!(s.retire(Slot::A), Retire::Closed);
    assert_eq!(s.retire(Slot::B), Retire::Closed);
    assert_eq!(s.release_into(Slot::A), Err(SwapError::Closed), "nothing is running yet");
    assert_eq!(s.open(Slot::A), 2);
    s.release_into(Slot::A).expect("started fresh");
    assert_eq!(bound(&s).generation(), 2);
}

#[test]
fn a_starting_proxy_holds_until_adoption_is_done() {
    let (s, guard) = ActiveSlot::starting(Duration::from_secs(300));
    assert!(matches!(
        s.bind(),
        Bind::Held { hold_max, .. } if hold_max == Duration::from_secs(300)
    ));
    s.adopt(Slot::B, Some(Slot::A));
    assert_eq!(s.active(), Some(Slot::B));
    assert!(s.holding(), "adoption does not end the hold");
    assert!(s.can_retire(Slot::A), "the other child is open");
    // An adopted-held gate releases into the OLD slot too (the rollback case after a
    // proxy restart): the guard is not needed.
    s.release_into(Slot::A).expect("adopted");
    assert_eq!(bound(&s).slot(), Slot::A);
    drop(guard);
    assert_eq!(s.abandoned_holds(), 0, "the hold had ended already");
    let (t, guard) = ActiveSlot::starting(Duration::from_secs(300));
    t.adopt(Slot::B, Some(Slot::A));
    drop(guard);
    assert!(matches!(t.bind(), Bind::NoActive), "an untaken startup hold fails closed");
    let one = ActiveSlot::new();
    one.adopt(Slot::A, None);
    assert_eq!(one.retire(Slot::B), Retire::NeverOpened);
    assert_eq!(bound(&one).slot(), Slot::A);
}

#[test]
fn clearing_the_active_slot_makes_a_release_answer_at_once() {
    let s = live();
    let guard = s.hold(HOLD_MAX).expect("hold");
    s.clear_active();
    assert_eq!(s.active(), None);
    assert!(s.holding(), "the hold survives");
    assert!(matches!(s.bind(), Bind::Held { hold_max, .. } if hold_max == HOLD_MAX));
    drop(guard);
    assert!(matches!(s.bind(), Bind::NoActive));
}

#[test]
fn a_reopened_slot_gets_a_new_generation_and_its_permits_carry_it() {
    let s = ActiveSlot::new();
    assert_eq!(s.generation(Slot::A), 0);
    assert_eq!(s.open(Slot::A), 1);
    assert_eq!(s.open(Slot::A), 1, "already open: same child");
    s.set_active(Slot::A).expect("open");
    let first = bound(&s);
    assert_eq!(first.generation(), 1);
    s.open(Slot::B);
    s.set_active(Slot::B).expect("open");
    drop(first);
    assert_eq!(s.retire(Slot::A), Retire::Closed);
    // A runs a new child: the pool of generation 1 must not be reused.
    assert_eq!(s.open(Slot::A), 2);
    s.set_active(Slot::A).expect("reopened after a retire");
    assert_eq!(bound(&s).generation(), 2);
    assert_eq!(s.generation(Slot::B), 1);
}

#[test]
fn open_streams_counts_both_slots_and_the_pending() {
    let s = live();
    s.open(Slot::B);
    assert_eq!(s.open_streams(), 0);
    let accepted = s.begin_stream();
    assert_eq!(s.open_streams(), 1, "counted from accept, before any bind");
    let a = bound(&s);
    drop(accepted);
    assert_eq!(s.open_streams(), 1, "the permit takes over");
    s.set_active(Slot::B).expect("open");
    let _b = bound(&s);
    let bypass = s.begin_stream();
    assert_eq!(s.open_streams(), 3, "both slots and a bypass stream");
    drop(a);
    assert_eq!(s.open_streams(), 2);
    assert!(format!("{bypass:?}").contains("Pending"));
}

#[test]
fn a_slot_has_one_text_form() {
    assert_eq!(Slot::A.name(), "a");
    assert_eq!(Slot::B.to_string(), "b");
    assert_eq!("a".parse::<Slot>(), Ok(Slot::A));
    assert_eq!("b".parse::<Slot>(), Ok(Slot::B));
    for bad in ["A", "B", "0", "", "ab", " a"] {
        assert_eq!(bad.parse::<Slot>(), Err(SlotParseError), "{bad:?}");
    }
    assert_eq!(SlotParseError.to_string(), "a slot is \"a\" or \"b\"");
    assert_eq!(serde_json::to_string(&Slot::A).expect("ser"), "\"a\"");
    assert_eq!(serde_json::from_str::<Slot>("\"b\"").expect("de"), Slot::B);
    assert!(serde_json::from_str::<Slot>("\"A\"").is_err());
    assert!(serde_json::from_str::<Slot>("0").is_err());
}

#[test]
fn a_racing_flip_and_retire_never_put_a_permit_on_a_closed_slot() {
    for _ in 0..200 {
        let s = live();
        s.open(Slot::B);
        // Set by the supervisor thread once `retire(A)` has returned `Closed`.
        let closed = Arc::new(AtomicBool::new(false));
        let binder = {
            let (s, closed) = (s.clone(), Arc::clone(&closed));
            std::thread::spawn(move || {
                let mut late_on_a = 0_u32;
                for _ in 0..2000 {
                    // Read the flag before the bind: a bind that starts after the
                    // retire completed must never land on A.
                    let was_closed = closed.load(Ordering::Acquire);
                    if let Bind::Bound(p) = s.bind() {
                        late_on_a += u32::from(was_closed && p.slot() == Slot::A);
                    }
                    std::thread::yield_now();
                }
                late_on_a
            })
        };
        s.set_active(Slot::B).expect("open");
        while s.retire(Slot::A) != Retire::Closed {
            std::thread::yield_now();
        }
        closed.store(true, Ordering::Release);
        assert_eq!(binder.join().expect("binder"), 0);
        assert_eq!(s.open_permits(Slot::A), 0);
        assert_eq!(bound(&s).slot(), Slot::B);
    }
}

#[test]
fn a_retry_rebinds_through_the_gate_and_waits_for_the_new_child() {
    // A stream bound to A before the hold; the supervisor then held and quiesced A.
    let s = live();
    s.open(Slot::B);
    let first = bound(&s);
    let guard = s.hold(HOLD_MAX).expect("hold");
    // The fenced answer arrives: the retry drops the old permit, then binds again.
    drop(first);
    assert!(matches!(s.bind(), Bind::Held { .. }), "not back onto A");
    s.release_into(Slot::B).expect("open");
    assert_eq!(bound(&s).slot(), Slot::B);
    drop(guard);
}

#[test]
fn open_streams_never_reads_zero_while_a_stream_lives() {
    let s = live();
    // One stream is always live from here on: a retry counts the new stream before it
    // drops the old permit, and a bind takes the permit before the Pending drops.
    let mut current = bound(&s);
    let reader = {
        let s = s.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut zeros = 0_u32;
            while !flag.load(Ordering::Acquire) {
                zeros += u32::from(s.open_streams() == 0);
            }
            zeros
        });
        (handle, stop)
    };
    for _ in 0..20_000 {
        let pending = s.begin_stream();
        drop(current);
        let next = bound(&s);
        drop(pending);
        current = next;
    }
    reader.1.store(true, Ordering::Release);
    assert_eq!(reader.0.join().expect("reader"), 0);
    drop(current);
    assert_eq!(s.open_streams(), 0);
}
