//! The loom models of the slot and gate (design note section 5). Run by
//! `cargo xtask proxy-loom` (`--cfg kbf_loom`); under a plain `cargo test` this module is not
//! built. Each model runs every interleaving of its threads up to a preemption bound.

use super::*;
use loom::sync::atomic::AtomicBool;
use loom::thread;
use std::sync::Arc;

const HOLD_MAX: Duration = Duration::from_secs(20);

fn model(f: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(4);
    builder.check(f);
}

fn live() -> ActiveSlot {
    let s = ActiveSlot::new();
    s.open(Slot::A);
    s.set_active(Slot::A).expect("A is open");
    s
}

/// What a bind returned, without the permit's lifetime.
fn kind(b: &Bind) -> &'static str {
    match b {
        Bind::Bound(p) if p.slot() == Slot::A => "A",
        Bind::Bound(_) => "B",
        Bind::Held { .. } => "held",
        Bind::NoActive => "none",
    }
}

/// Model 1: a bind races a flip to B, a retire of A and a hold that ends. A permit on A
/// exists only if the retire saw it (`Busy`); a retire that closed A means the bind never got
/// A; nothing underflows; and once the permit drops the retire succeeds.
fn model_one(end: fn(HoldGuard, &ActiveSlot)) {
    model(move || {
        let s = live();
        let binder = {
            let s = s.clone();
            thread::spawn(move || s.bind())
        };
        let flipper = {
            let s = s.clone();
            thread::spawn(move || {
                s.open(Slot::B);
                s.set_active(Slot::B).expect("B was just opened");
                s.retire(Slot::A)
            })
        };
        let holder = {
            let s = s.clone();
            thread::spawn(move || {
                if let Ok(guard) = s.hold(HOLD_MAX) {
                    end(guard, &s);
                }
            })
        };
        let bind = binder.join().expect("binder");
        let retire = flipper.join().expect("flipper");
        holder.join().expect("holder");

        match retire {
            Retire::Closed => assert_ne!(kind(&bind), "A", "a closed slot gave out a permit"),
            Retire::Busy(n) => assert_eq!(n, 1, "the one permit"),
            other => panic!("retire of a non-active slot answered {other:?}"),
        }
        if kind(&bind) == "A" {
            assert!(
                matches!(retire, Retire::Busy(1)),
                "a permit on A the retire did not see"
            );
        }
        drop(bind);
        assert_eq!(s.open_permits(Slot::A), 0, "no underflow, no leak");
        assert_eq!(s.open_streams(), 0);
        let again = s.retire(Slot::A);
        match retire {
            // Closed by that call: a repeat only says so.
            Retire::Closed => assert_eq!(again, Retire::AlreadyClosed),
            // Refused for the permit: with the permit gone the retire now closes.
            _ => assert_eq!(again, Retire::Closed),
        }
    });
}

#[test]
fn loom_model_1_bind_flip_retire_with_a_release() {
    model_one(|guard, s| {
        let _ = s.release_into(Slot::B);
        drop(guard);
    });
}

#[test]
fn loom_model_1_bind_flip_retire_with_a_dropped_guard() {
    model_one(|guard, _| drop(guard));
}

/// Model 2, the lost wakeup: a stream that bound to a hold and got `Held { epoch }` must find
/// `epoch() != epoch` once the release has landed, in every interleaving, so a wait on
/// "epoch differs" cannot sleep through the release. A bind that sampled the epoch after the
/// gate would fail this.
#[test]
fn loom_model_2_a_release_is_never_missed() {
    // Outside the model, so it survives its runs: some interleaving must have answered `Held`,
    // or the waiter's assertion never ran and a `bind` that ignores the hold would pass.
    static HELD_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    model(|| {
        let s = live();
        s.open(Slot::B);
        let guard = s.hold(HOLD_MAX).expect("no hold yet");
        let released = Arc::new(AtomicBool::new(false));
        let binder = {
            let s = s.clone();
            let released = Arc::clone(&released);
            thread::spawn(move || match s.bind() {
                Bind::Held { epoch, .. } => {
                    HELD_SEEN.store(true, std::sync::atomic::Ordering::Relaxed);
                    // The waiter parks only if the epoch still equals what it sampled; so
                    // once the release is over, it must differ.
                    while !released.load(Ordering::Acquire) {
                        thread::yield_now();
                    }
                    assert_ne!(s.epoch(), epoch, "a release the waiter would sleep through");
                }
                Bind::Bound(_) => {}
                Bind::NoActive => panic!("A is active and open"),
            })
        };
        let releaser = {
            let s = s.clone();
            let released = Arc::clone(&released);
            thread::spawn(move || {
                s.release_into(Slot::B).expect("B is open");
                released.store(true, Ordering::Release);
            })
        };
        binder.join().expect("binder");
        releaser.join().expect("releaser");
        assert_eq!(s.epoch(), 1, "one hold ended once");
        drop(guard);
        assert_eq!(s.abandoned_holds(), 0, "released, so the drop is inert");
    });
    assert!(
        HELD_SEEN.load(std::sync::atomic::Ordering::Relaxed),
        "no interleaving answered Held: the model checked nothing"
    );
}

/// Model 2b, the doorbell: two holds end on two threads (the supervisor is single-writer in
/// production; this is the shape of the race `ring` must survive). The doorbell's last write
/// is the last epoch, in every interleaving: it never goes backwards, so a sleeper that
/// sampled the older number is woken.
#[test]
fn loom_model_2b_the_doorbell_never_goes_backwards() {
    model(|| {
        let s = live();
        let ring_twice = |s: ActiveSlot| {
            thread::spawn(move || {
                s.inner.ring();
            })
        };
        let a = ring_twice(s.clone());
        let b = ring_twice(s.clone());
        a.join().expect("a");
        b.join().expect("b");
        assert_eq!(s.epoch(), 2);
        assert_eq!(
            s.inner.doorbell.value(),
            2,
            "the doorbell holds a stale epoch"
        );
    });
}

/// Model 3: a stream bound to A before the supervisor held and flipped; it dispatches after.
/// Its permit keeps the retire of A at `Busy` for as long as the stream lives: a retire that
/// closed A must find the permit already gone.
#[test]
fn loom_model_3_a_bound_stream_blocks_the_retire_until_it_ends() {
    model(|| {
        let s = live();
        let permit = match s.bind() {
            Bind::Bound(p) => p,
            other => panic!("expected a permit, got {other:?}"),
        };
        let alive = Arc::new(AtomicBool::new(true));
        let supervisor = {
            let s = s.clone();
            let alive = Arc::clone(&alive);
            thread::spawn(move || {
                let guard = s.hold(HOLD_MAX).expect("no hold yet");
                s.open(Slot::B);
                s.release_into(Slot::B).expect("B is open");
                drop(guard);
                let retire = s.retire(Slot::A);
                if retire == Retire::Closed {
                    assert!(
                        !alive.load(Ordering::Acquire),
                        "A closed under a live permit"
                    );
                }
                retire
            })
        };
        // The stream dispatches and runs, then ends.
        assert_eq!(permit.slot(), Slot::A);
        alive.store(false, Ordering::Release);
        drop(permit);
        let retire = supervisor.join().expect("supervisor");
        assert!(
            matches!(retire, Retire::Busy(1) | Retire::Closed),
            "{retire:?}"
        );
        assert_eq!(s.open_streams(), 0);
    });
}
