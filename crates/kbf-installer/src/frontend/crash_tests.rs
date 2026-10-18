//! The installer killed before and after every step of a frontend update, then started
//! again as a new process: every kill point must converge on the same end, with no
//! unit stopped that should serve and no slot left on a release that did not win.
//!
//! The kill is a panic out of the executor at the Nth step it is given (before it for an odd
//! number, after it for an even one), caught by the test; the files, the units and the spool
//! stay as they were left, as they would after a `SIGKILL`.

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use super::pending::Instance;
use super::sim::{T0, World, record, to};
use crate::testkit::{A, B, c};

const A_: Instance = Instance::A;
const B_: Instance = Instance::B;

/// Run passes until the update has ended (or `max` passes), killing at `kill_at`, and
/// restarting the installer after a kill. `before` runs before pass `n`. Returns the number
/// of steps the whole run took.
fn run(w: &World, kill_at: usize, before: &dyn Fn(&World, u64)) -> usize {
    w.kill_at.set(kill_at);
    let mut h = w.helper();
    w.put(&record(5, 1, 1, B));
    for n in 0..24 {
        before(w, n);
        let now = T0 + 3 * n;
        if catch_unwind(AssertUnwindSafe(|| h.cycle(now).expect("a pass"))).is_err() {
            w.kill_at.set(0);
            h = w.helper();
        }
        // Over: the record is gone, the order is taken, and nothing is in flight.
        if n >= 3 && w.left() == 0 && w.file().frontend_pending.is_none() {
            break;
        }
    }
    // One more pass, as the unit's next one: the report is rewritten from the files.
    w.kill_at.set(0);
    h.cycle(T0 + 3000).expect("a settled pass");
    w.steps.get()
}

/// The happy path, killed at every step: after the restart the update is kept, once. The
/// old instance is never stopped or started, the node's service is never restarted, and
/// the end is the one an unkilled run reaches.
///
/// (The pending record written after the slot moved is caught by the cold-revert test
/// below, where the slot it restores is wrong.)
#[test]
fn a_kill_at_any_step_of_an_update_still_ends_with_it_kept_once() {
    let total = run(&World::new("crash-happy-count", A_), 0, &|_, _| {});
    assert!(total >= 8, "{total} steps");
    for k in 1..=2 * total {
        let w = World::new(&format!("crash-happy-{k}"), A_);
        run(&w, k, &|_, _| {});
        let state = w.state();
        assert_eq!(
            (state.current.clone(), state.previous.clone()),
            (Some(c(B)), Some(c(A))),
            "kill {k}"
        );
        assert_eq!(w.file().serving_instance, Some(B_), "kill {k}");
        assert_eq!(w.file().frontend_pending, None, "kill {k}");
        assert_eq!(w.slot(B_), Some(to(B)), "kill {k}");
        assert_eq!(w.slot(A_), Some(to(A)), "kill {k}");
        assert_eq!(w.sim.borrow().count("stop a"), 0, "kill {k}");
        assert_eq!(w.sim.borrow().count("start a"), 0, "kill {k}");
        assert!(w.sim.borrow().other_runs.is_empty(), "kill {k}");
        assert_eq!(w.left(), 0, "kill {k}");
        // A kill between the record ending and the report being saved loses the word
        // `confirmed`; the host is then simply idle on the new release.
        let status = w.status();
        assert_eq!(status["installed_commit"], B, "kill {k}");
        assert!(
            ["confirmed", "idle"].contains(&status["state"].as_str().expect("w")),
            "kill {k}"
        );
    }
}

/// The cold revert, killed at every step: `b` dies after `a` stood down. Whatever step the
/// installer dies at, `a` ends serving again, `current` is where it was, the slot of `b` is
/// back on its prior release and the record is gone. When `b` was never started (the kill
/// came first) `a` was never asked to stop.
///
/// Mutant planted: the pending record written after the slot moved (a kill between the two
/// leaves a slot nobody remembers, so the revert puts back the wrong release).
#[test]
fn a_kill_at_any_step_of_a_cold_revert_still_brings_the_old_instance_back() {
    // `b` dies the first time it is seen running, whichever pass that is.
    let dies = || {
        let done = Cell::new(false);
        move |w: &World, _n: u64| {
            if !done.get() && w.sim.borrow_mut().unit(B_).state == "active" {
                w.sim.borrow_mut().die(B_);
                done.set(true);
            }
        }
    };
    let total = run(&World::new("crash-cold-count", A_), 0, &dies());
    assert!(total >= 8, "{total} steps");
    for k in 1..=2 * total {
        let w = World::new(&format!("crash-cold-{k}"), A_);
        run(&w, k, &dies());
        let name = format!("kill {k}");
        let state = w.state();
        assert_eq!(state.current, Some(c(A)), "{name}");
        assert_eq!(w.file().frontend_pending, None, "{name}");
        assert_eq!(w.file().serving_instance, Some(A_), "{name}");
        assert_eq!(w.slot(B_), Some(to(A)), "{name}");
        {
            let mut sim = w.sim.borrow_mut();
            let (state, ready) = {
                let a = sim.unit(A_);
                (a.state, a.ready)
            };
            assert_eq!((state, ready), ("active", true), "{name}: a serves");
            if sim.count("start b") == 0 {
                assert_eq!(sim.count("stop a"), 0, "{name}: a was never stood down");
            }
            assert!(sim.other_runs.is_empty(), "{name}");
        }
        let status = w.status();
        assert_eq!(status["installed_commit"], A, "{name}");
        assert!(
            ["reverted", "idle"].contains(&status["state"].as_str().expect("w")),
            "{name}"
        );
    }
}
