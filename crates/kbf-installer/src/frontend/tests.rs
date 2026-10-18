//! The frontend update on a scratch host: real links, state and `frontend.json`, with systemd
//! and `/readyz` played by [`Sim`]. Time is the `now` each pass is given.

use std::fs;

use kbf_update::Facts;

use super::Progress;
use super::pending::{FrontendFile, Instance, Phase};
use super::sim::{NODE_UNIT, Start, T0, UPDATE, World, record, to};
use crate::helper::fetcher::FetchError;
use crate::helper::memory::Kind;
use crate::testkit::{A, B, C, c};

const A_: Instance = Instance::A;
const B_: Instance = Instance::B;

/// A pass every three seconds from `T0`.
fn pass(h: &mut crate::helper::cycle::Helper<super::sim::SimExec>, n: u64) {
    h.cycle(T0 + 3 * n).expect("a pass");
}

fn pending(w: &World) -> Option<super::pending::FrontendPending> {
    w.file().frontend_pending
}

/// The whole update, to its confirmation: the record is read, the new release is probed
/// against the serving instance, the pending record is written, `slots/b` is pointed at the
/// new release and `b` is started; then three looks in a row at a ready `b` with `a` stood
/// down flip `current`, `previous` and `serving_instance` together. The node's service is
/// never restarted, `a` is never stopped and its slot never moves.
///
/// Mutants planted: the slot pointed at `current` instead of the new release; `a` stopped at
/// confirmation; the flip skipped (current stays); the node restart owed for this verb;
/// `serving_instance` not moved.
#[test]
fn a_frontend_update_starts_the_idle_instance_and_keeps_it_once_the_old_stood_down() {
    let w = World::new("happy", A_);
    let mut h = w.helper();
    w.put(&record(5, 1, 1, B));
    pass(&mut h, 0);

    assert_eq!(w.asked(), ["start b"]);
    assert_eq!(w.slot(B_), Some(to(B)));
    assert_eq!(w.slot(A_), Some(to(A)));
    let p = pending(&w).expect("pending");
    assert_eq!((p.idle, p.old, p.commit.as_str()), (B_, A_, B));
    assert_eq!(p.order_id, "1-5");
    assert_eq!(p.prior_slot, Some(to(A)));
    assert_eq!(p.deadline_s, T0 + 600);
    assert_eq!(p.phase, Phase::Confirming { looks: 0 });
    assert_eq!(w.status()["state"], "activated_unconfirmed");
    assert_eq!(w.state().current, Some(c(A)));
    assert_eq!(w.left(), 0);
    // The probe ran on the new release and on the serving instance's slot, both through the
    // paths the units use.
    let asked = w.probe.asked.borrow();
    assert!(asked[0].ends_with(format!("releases/{B}/kbf")), "{asked:?}");
    assert!(asked[1].ends_with("slots/a/kbf"), "{asked:?}");
    drop(asked);

    pass(&mut h, 1);
    pass(&mut h, 2);
    assert_eq!(
        pending(&w).expect("pending").phase,
        Phase::Confirming { looks: 2 }
    );
    assert_eq!(
        w.state().current,
        Some(c(A)),
        "not kept before the third look"
    );
    pass(&mut h, 3);

    let state = w.state();
    assert_eq!((state.current, state.previous), (Some(c(B)), Some(c(A))));
    assert_eq!(state.pending_confirm, None);
    let file = w.file();
    assert_eq!(file.serving_instance, Some(B_));
    assert_eq!(file.frontend_pending, None);
    assert_eq!(w.status()["state"], "confirmed");
    assert_eq!(w.status()["installed_commit"], B);
    assert_eq!(
        fs::read_link(w.root.join("usr/local/kbf/current"))
            .expect("link")
            .display()
            .to_string(),
        format!("releases/{B}")
    );
    // `a` keeps running its own release: a crash restart of it picks up nothing new.
    assert_eq!(w.slot(A_), Some(to(A)));
    assert_eq!(w.slot(B_), Some(to(B)));
    assert_eq!(w.asked(), ["start b"], "the old instance is never stopped");
    assert!(
        w.sim.borrow().other_runs.is_empty(),
        "the node's service is not restarted: {:?}",
        w.sim.borrow().other_runs
    );
    // Nothing is left to do: more passes change nothing.
    pass(&mut h, 4);
    assert_eq!(w.asked(), ["start b"]);
}

/// Catches looks that are not consecutive: a `/readyz` that fails between two good ones
/// starts the count again, so a flapping instance is not kept on three scattered looks.
///
/// Mutant planted: the count not reset on a failed look (kept after the third good look
/// of five).
#[test]
fn the_looks_must_follow_one_another() {
    let w = World::new("looks", A_);
    let mut h = w.helper();
    w.put(&record(5, 1, 1, B));
    pass(&mut h, 0);
    pass(&mut h, 1);
    pass(&mut h, 2);
    assert_eq!(
        pending(&w).expect("p").phase,
        Phase::Confirming { looks: 2 }
    );
    w.sim.borrow_mut().unit(B_).ready = false;
    pass(&mut h, 3);
    assert_eq!(
        pending(&w).expect("p").phase,
        Phase::Confirming { looks: 0 }
    );
    w.sim.borrow_mut().unit(B_).ready = true;
    pass(&mut h, 4);
    pass(&mut h, 5);
    assert!(pending(&w).is_some(), "two looks since the flap");
    pass(&mut h, 6);
    assert_eq!(w.file().serving_instance, Some(B_));
}

/// Catches an update kept while the old instance still serves: the new one is ready for
/// many looks, but `a` never stood down, so nothing is flipped, and when the window ends the
/// update is undone the cheap way: `b` is stopped, its slot restored, and `a` is never
/// touched (no stop, no start).
///
/// Mutants planted: the stood-down condition dropped (flipped on three looks); `a` stopped
/// in the revert; the window check dropped (waits for ever).
#[test]
fn an_update_is_not_kept_while_the_old_instance_serves_and_is_undone_when_the_window_ends() {
    let w = World::new("old-serves", A_);
    w.sim.borrow_mut().stands_down = false;
    let mut h = w.helper();
    w.put(&record(5, 1, 1, B));
    pass(&mut h, 0);
    for n in 1..=10 {
        pass(&mut h, n);
    }
    assert_eq!(w.state().current, Some(c(A)));
    assert!(pending(&w).is_some());

    h.cycle(T0 + 601).expect("past the window");
    assert_eq!(w.asked(), ["start b", "stop b"]);
    h.cycle(T0 + 604).expect("b is down");
    assert_eq!(w.asked(), ["start b", "stop b"], "a is untouched");
    assert_eq!(pending(&w), None);
    assert_eq!(
        w.slot(B_),
        Some(to(A)),
        "the slot is back on its prior release"
    );
    assert_eq!(w.state().current, Some(c(A)));
    assert_eq!(w.file().serving_instance, Some(A_));
    assert_eq!(w.status()["state"], "reverted");
    assert!(
        w.refusal().contains("not confirmed within the window"),
        "{}",
        w.refusal()
    );
}

/// Catches branch (a) of the revert wrong: the new instance never started and the old still
/// serves, so the only change is the slot going back. A slot that did not exist before is
/// removed, not left pointing at the release that failed. The old instance gets no command.
///
/// Mutants planted: the slot not restored; the slot restored to the new release; a prior
/// absence restored as a link; `a` stopped.
#[test]
fn a_start_that_fails_with_the_old_still_serving_only_puts_the_slot_back() {
    for (name, prior_link) in [("a-prior", true), ("a-none", false)] {
        let w = World::new(name, A_);
        if !prior_link {
            fs::remove_file(w.root.join("usr/local/kbf/slots/b")).expect("no slot");
        }
        w.sim.borrow_mut().start[1] = Start::Fails;
        let mut h = w.helper();
        w.put(&record(5, 1, 1, B));
        pass(&mut h, 0);
        assert_eq!(w.slot(B_), Some(to(B)), "moved before the start");
        pass(&mut h, 1);
        assert_eq!(w.asked(), ["start b"], "{name}: a is untouched");
        assert_eq!(pending(&w), None);
        assert_eq!(w.slot(B_), prior_link.then(|| to(A)), "{name}");
        assert_eq!(w.state().current, Some(c(A)));
        assert_eq!(w.file().serving_instance, Some(A_));
        assert_eq!(w.status()["state"], "reverted");
    }
}

/// Catches an instance that starts but never answers `/readyz` being kept or left: `b` is
/// `active` and not ready, so nothing is kept, and past the window it is stopped (asked once
/// it is seen up), then the slot goes back.
#[test]
fn a_new_instance_that_never_answers_is_stopped_and_the_slot_goes_back() {
    let w = World::new("never-ready", A_);
    {
        let mut sim = w.sim.borrow_mut();
        sim.start[1] = Start::NeverReady;
        sim.stands_down = false;
    }
    let mut h = w.helper();
    w.put(&record(5, 1, 1, B));
    pass(&mut h, 0);
    pass(&mut h, 1);
    assert_eq!(
        pending(&w).expect("p").phase,
        Phase::Confirming { looks: 0 }
    );
    h.cycle(T0 + 700).expect("late");
    h.cycle(T0 + 703).expect("down");
    assert_eq!(w.asked(), ["start b", "stop b"]);
    assert_eq!(w.slot(B_), Some(to(A)));
    assert_eq!(pending(&w), None);
}

/// Catches branch (b) of the revert: `b` became ready (so `a` stood down), then died inside
/// the window. `a` cannot retake the store, so it is stopped (asked without waiting), and
/// only once it is down is it started again; the slot goes back only after `a` serves
/// again, and `serving_instance` stays `a`. `current` never moved.
///
/// Mutants planted: `a` not restarted (the host is left with no frontend); the slot restored
/// before `a` is ready; `a` started while it is still running; `a` stopped when it already
/// serves.
#[test]
fn a_new_instance_that_dies_after_the_old_stood_down_restarts_the_old_and_waits_for_it() {
    let w = World::new("cold", A_);
    let mut h = w.helper();
    w.put(&record(5, 1, 1, B));
    pass(&mut h, 0);
    assert!(!w.sim.borrow_mut().unit(A_).ready, "a stood down");
    w.sim.borrow_mut().die(B_);

    pass(&mut h, 1);
    assert_eq!(w.asked(), ["start b", "stop a"]);
    assert_eq!(
        pending(&w).expect("p").phase,
        Phase::Reverting {
            deadline: T0 + 3 + 300,
            old_started: false
        }
    );
    pass(&mut h, 2);
    assert_eq!(w.asked(), ["start b", "stop a", "start a"]);
    assert_eq!(
        pending(&w).expect("p").phase,
        Phase::Reverting {
            deadline: T0 + 3 + 300,
            old_started: true
        }
    );
    assert_eq!(
        w.slot(B_),
        Some(to(B)),
        "not put back while a is being brought up"
    );
    pass(&mut h, 3);
    assert_eq!(pending(&w), None);
    assert_eq!(w.slot(B_), Some(to(A)));
    assert_eq!(w.asked(), ["start b", "stop a", "start a"]);
    assert_eq!(w.file().serving_instance, Some(A_));
    assert_eq!(w.state().current, Some(c(A)));
    assert_eq!(w.status()["state"], "reverted");
    assert!(
        w.refusal().contains("the new instance stopped"),
        "{}",
        w.refusal()
    );
}

/// Catches the old instance being stopped a second time while it comes back: after the
/// installer started it, `a` is `active` and not yet answering, which is not a unit that
/// stood down. It is waited for, not stopped again, and the slot stays until it answers; when
/// it answers the update is undone.
///
/// Mutant planted: `old_started` ignored (a second `stop a`, an endless stop and start).
#[test]
fn an_old_instance_that_is_coming_back_is_waited_for_not_stopped_again() {
    let w = World::new("cold-slow", A_);
    let mut h = w.helper();
    w.put(&record(5, 1, 1, B));
    pass(&mut h, 0);
    w.sim.borrow_mut().die(B_);
    w.sim.borrow_mut().start[0] = Start::NeverReady;
    pass(&mut h, 1);
    pass(&mut h, 2);
    for n in 3..8 {
        pass(&mut h, n);
    }
    assert_eq!(w.asked(), ["start b", "stop a", "start a"]);
    assert!(pending(&w).is_some());
    assert_eq!(w.slot(B_), Some(to(B)));
    w.sim.borrow_mut().unit(A_).ready = true;
    pass(&mut h, 8);
    assert_eq!(pending(&w), None);
    assert_eq!(w.slot(B_), Some(to(A)));
}

/// Catches an installer that kills or gives up silently: the old instance will not stop (a
/// stream that never ends), so after the bound the update is `NeedsOperator`, the record
/// stays, nothing is asked again, the status says `needs_operator`, and the exit status for
/// a one-shot run is 5. Nothing is killed: the only commands ever asked are start and a
/// `--no-block` stop.
///
/// Mutants planted: the bound not checked (waits for ever); the record cleared at the bound
/// (the host believes the update ended); a kill sent.
#[test]
fn an_old_instance_that_will_not_stop_needs_an_operator_and_is_never_killed() {
    let w = World::new("needs-operator", A_);
    w.sim.borrow_mut().stop_hangs[0] = true;
    let mut h = w.helper();
    w.put(&record(5, 1, 1, B));
    pass(&mut h, 0);
    w.sim.borrow_mut().die(B_);
    pass(&mut h, 1);
    assert_eq!(w.asked(), ["start b", "stop a"]);
    for n in 2..100 {
        pass(&mut h, n);
    }
    // 3 + 300 is the bound; the passes ran to 297 s.
    assert!(matches!(
        pending(&w).expect("p").phase,
        Phase::Reverting { .. }
    ));
    h.cycle(T0 + 3 + 300).expect("at the bound");
    let p = pending(&w).expect("the record stays");
    let Phase::NeedsOperator { reason } = &p.phase else {
        panic!("{:?}", p.phase)
    };
    assert!(reason.contains("nothing was killed"), "{reason}");
    assert!(
        w.refusal().starts_with("needs_operator: "),
        "{}",
        w.refusal()
    );
    h.cycle(T0 + 1000).expect("later");
    h.cycle(T0 + 2000).expect("later");
    assert_eq!(w.asked(), ["start b", "stop a"], "nothing else is asked");
    assert!(w.sim.borrow().other_runs.is_empty());
    assert_eq!(
        w.slot(B_),
        Some(to(B)),
        "the slot stays while the host is half done"
    );
    assert_eq!(Progress::NeedsOperator(String::new()).exit_code(), 5);
    assert_eq!(Progress::Reverted(String::new()).exit_code(), 0);
    assert_eq!(Progress::Waiting.exit_code(), 0);
}

/// Catches a second update accepted while the old instance of the first still drains: after
/// the first is kept `a` is `active` (not ready) beside the serving `b`, so a second record
/// is refused `frontend_busy`, not queued (its record is gone, nothing starts), and once `a`
/// exits a third is taken, with `a` now the idle instance and `slots/a` moved to the new
/// release.
///
/// Mutants planted: the busy check dropped; a refused record kept in the spool (queued);
/// the idle instance fixed at `b`.
#[test]
fn a_second_update_while_the_old_instance_drains_is_busy_and_not_queued() {
    let w = World::new("busy", A_);
    let mut h = w.helper();
    w.put(&record(5, 1, 1, B));
    for n in 0..4 {
        pass(&mut h, n);
    }
    assert_eq!(w.file().serving_instance, Some(B_));

    w.put(&record(6, 1, 2, C));
    pass(&mut h, 4);
    assert!(
        w.refusal().starts_with("frontend_busy: "),
        "{}",
        w.refusal()
    );
    assert_eq!(w.left(), 0, "refused, not queued");
    assert_eq!(w.asked(), ["start b"]);
    assert_eq!(pending(&w), None);
    pass(&mut h, 5);
    assert_eq!(w.asked(), ["start b"], "and not run later either");

    w.sim.borrow_mut().exits(A_);
    w.put(&record(7, 1, 3, C));
    pass(&mut h, 6);
    assert_eq!(w.asked(), ["start b", "start a"]);
    assert_eq!(w.slot(A_), Some(to(C)));
    assert_eq!(
        w.slot(B_),
        Some(to(B)),
        "the serving instance's slot does not move"
    );
    for n in 7..10 {
        pass(&mut h, n);
    }
    let state = w.state();
    assert_eq!((state.current, state.previous), (Some(c(C)), Some(c(B))));
    assert_eq!(w.file().serving_instance, Some(A_));
}

/// Catches a second record accepted while one is in flight: refused `frontend_busy`, its
/// record removed, the first update untouched and finished by its own passes.
#[test]
fn a_record_while_one_is_in_flight_is_refused_and_does_not_disturb_it() {
    let w = World::new("busy-pending", A_);
    let mut h = w.helper();
    w.put(&record(5, 1, 1, B));
    pass(&mut h, 0);
    w.put(&record(6, 1, 2, C));
    pass(&mut h, 1);
    assert!(
        w.refusal().starts_with("frontend_busy: "),
        "{}",
        w.refusal()
    );
    assert!(w.refusal().contains(B), "it names the update in flight");
    assert_eq!(w.left(), 0);
    assert_eq!(w.asked(), ["start b"]);
    assert_eq!(pending(&w).expect("p").commit.as_str(), B);
    for n in 2..6 {
        pass(&mut h, n);
    }
    assert_eq!(w.state().current, Some(c(B)));
    assert_eq!(w.asked(), ["start b"]);
}

/// Catches each refusal that must happen before anything is changed: the reason carries the
/// stable word, the record is acknowledged and gone, no unit is started, no slot moves, and
/// no pending record is written.
///
/// Mutants planted: the Envoy gate not asked; the protocol comparison dropped; the
/// systemd/hint comparison dropped; the down check dropped; the node-trial check dropped.
#[test]
fn a_refused_update_changes_nothing() {
    type Setup = fn(&World);
    let cases: Vec<(&str, Setup, &str)> = vec![
        (
            "envoy",
            |w| *w.gate.0.borrow_mut() = Err("envoy 1.30 is below 1.38".to_owned()),
            "envoy_range: envoy 1.30",
        ),
        (
            "protocol",
            |w| *w.probe.new.borrow_mut() = Ok(2),
            "handover_protocol: the new release speaks handover protocol 2, the serving instance 1",
        ),
        (
            "new-unreadable",
            |w| *w.probe.new.borrow_mut() = Err("no field".to_owned()),
            "handover_protocol: the new release: no field",
        ),
        (
            "old-unreadable",
            |w| *w.probe.old.borrow_mut() = Err("no field".to_owned()),
            "handover_protocol: the serving instance: no field",
        ),
        (
            "mismatch",
            |w| {
                let mut h = w.helper();
                let file = FrontendFile {
                    serving_instance: Some(B_),
                    frontend_pending: None,
                };
                super::pending::save(h.exec_mut(), &w.layout, &file).expect("save");
            },
            "serving_mismatch: the installer last recorded instance b as serving; systemd says a",
        ),
        ("down", |w| w.sim.borrow_mut().exits(A_), "frontend_down: "),
        (
            "both-up",
            |w| {
                let mut sim = w.sim.borrow_mut();
                sim.unit(B_).state = "active";
            },
            "frontend_busy: instance a is Active and instance b is Active",
        ),
        (
            "changing",
            |w| w.sim.borrow_mut().unit(B_).state = "deactivating",
            "frontend_busy: ",
        ),
        (
            "node-trial",
            |w| {
                let lock = crate::state::Locked::acquire(&w.layout).expect("lock");
                let mut s = lock.read().expect("r").expect("s");
                s.pending_confirm = Some(crate::state::PendingConfirm {
                    commit: c(C),
                    previous: Some(c(A)),
                    deadline: None,
                    order_id: None,
                    starts_left: Some(3),
                });
                lock.write(&s).expect("w");
            },
            "pending: ",
        ),
        (
            "fetch",
            |w| {
                *w.fetcher.fail.lock().expect("l") =
                    Some(FetchError::Unavailable("down".to_owned()))
            },
            "fetch: fetch: down",
        ),
        (
            "slot-not-a-link",
            |w| {
                let slot = w.root.join("usr/local/kbf/slots/b");
                fs::remove_file(&slot).expect("rm");
                fs::write(&slot, b"x").expect("file");
            },
            "slot: slots/b is there and is not a link",
        ),
    ];
    for (name, setup, want) in cases {
        let w = World::new(&format!("refuse-{name}"), A_);
        setup(&w);
        let mut h = w.helper();
        w.put(&record(5, 1, 1, B));
        pass(&mut h, 0);
        assert!(w.refusal().starts_with(want), "{name}: {}", w.refusal());
        assert_eq!(w.left(), 0, "{name}");
        assert!(w.asked().is_empty(), "{name}: {:?}", w.asked());
        assert_eq!(pending(&w), None, "{name}");
        if name != "slot-not-a-link" {
            assert_eq!(w.slot(B_), Some(to(A)), "{name}");
        }
        assert_eq!(
            w.status()["state"],
            if name == "node-trial" {
                "activated_unconfirmed"
            } else {
                "refused"
            },
            "{name}"
        );
    }
}

/// Catches a release that is not the one ordered, a component set that is not exactly
/// `kbf`, and facts the release does not declare: each is refused before a slot moves.
#[test]
fn the_release_must_be_the_one_ordered_with_exactly_the_kbf_component() {
    let w = World::new("shape", A_);
    let mut h = w.helper();
    let mut r = record(5, 1, 1, B);
    r.components[0].name = "kbf-daemon".to_owned();
    w.put(&r);
    pass(&mut h, 0);
    assert!(w.refusal().starts_with("components: "), "{}", w.refusal());

    // The record carries facts and the fetched release declares others.
    let mut r = record(6, 1, 2, B);
    r.expected = Some(Facts {
        platform: "linux-x86_64".to_owned(),
        protocol: 9,
        min_protocol: 6,
        state_schema: 3,
        profile: "release".to_owned(),
    });
    w.put(&r);
    pass(&mut h, 1);
    assert!(
        w.refusal()
            .starts_with("facts: the fetched release is not the one ordered"),
        "{}",
        w.refusal()
    );

    // Facts that match are accepted, and a release that is not the commit named is not.
    let mut r = record(7, 1, 3, B);
    r.expected = Some(Facts {
        platform: "linux-x86_64".to_owned(),
        protocol: 7,
        min_protocol: 6,
        state_schema: 3,
        profile: "release".to_owned(),
    });
    w.put(&r);
    *w.fetcher.tamper.lock().expect("l") = Some(|got| got.commit = crate::testkit::c(C));
    pass(&mut h, 2);
    assert!(w.refusal().contains("it is commit"), "{}", w.refusal());
    assert!(w.asked().is_empty());
    *w.fetcher.tamper.lock().expect("l") = None;
    w.put(&record(8, 1, 4, B).clone());
    let mut ok = record(9, 1, 5, B);
    ok.expected = r.expected.clone();
    w.put(&ok);
    pass(&mut h, 3);
    assert_eq!(w.asked(), ["start b"], "matching facts are accepted");
}

/// Catches an update to the release the serving instance runs already being started again:
/// nothing starts, and the status says so.
#[test]
fn an_update_to_what_already_serves_starts_nothing() {
    let w = World::new("already", A_);
    let mut h = w.helper();
    w.put(&record(5, 1, 1, A));
    pass(&mut h, 0);
    assert!(w.asked().is_empty());
    assert_eq!(pending(&w), None);
    assert!(w.refusal().contains("already runs"), "{}", w.refusal());
}

/// Catches a frontend order acted on by an installer that was not set up for one: it is
/// refused with its own word and nothing is touched.
#[test]
fn an_installer_without_a_frontend_refuses_the_record() {
    let w = World::new("unconfigured", A_);
    let exec = super::sim::SimExec {
        host: w.exec_host.clone(),
        sim: w.sim.clone(),
        steps: w.steps.clone(),
        kill_at: w.kill_at.clone(),
    };
    let mut h = crate::helper::cycle::Helper::new(
        w.layout.clone(),
        exec,
        crate::helper::cycle::Config {
            update_dir: kbf_host::HostPath::new("/var/lib/kbf-installer").expect("p"),
            orders_dir: kbf_host::HostPath::new("/var/lib/kbf-installer/orders").expect("p"),
            restart: crate::helper::restart::Restart::Systemd(NODE_UNIT.to_owned()),
            window_secs: 600,
            starts: 3,
        },
        Box::new(w.fetcher.clone()),
        Box::new(w.leases.clone()),
    );
    w.put(&record(5, 1, 1, B));
    pass(&mut h, 0);
    assert!(
        w.refusal().starts_with("frontend_update: "),
        "{}",
        w.refusal()
    );
    assert!(w.asked().is_empty());
    assert_eq!(w.left(), 0);

    // The refusal moved no floor: a lower id, once the host is set up, is taken.
    let mut set_up = w.helper();
    w.put(&record(3, 1, 2, B));
    pass(&mut set_up, 1);
    assert_eq!(w.asked(), ["start b"]);
}

/// Catches the node path and the frontend path touching one another. A node `activate`
/// restarts the node's service and never asks about `kbf-frontend@*`; and while a frontend
/// update is in flight a node `activate` is refused (it would move `current` under the
/// update's flip).
///
/// Mutants planted: the node activation restarting a frontend unit; the in-flight check
/// dropped from the node path.
#[test]
fn a_node_activation_never_touches_the_frontend_units_and_waits_for_a_frontend_update() {
    let w = World::new("node-vs-frontend", A_);
    let mut h = w.helper();
    w.put_node_activate(5, B);
    pass(&mut h, 0);
    assert_eq!(w.state().current, Some(c(B)));
    assert_eq!(
        w.sim.borrow().other_runs,
        [format!("restart -- {NODE_UNIT}")],
        "only the node's own service"
    );
    assert!(w.asked().is_empty(), "no frontend unit was asked");
    assert_eq!(w.file(), FrontendFile::default());

    let w = World::new("frontend-then-node", A_);
    let mut h = w.helper();
    w.put(&record(5, 1, 1, B));
    pass(&mut h, 0);
    w.put_node_activate(6, C);
    pass(&mut h, 1);
    assert!(
        w.refusal().contains("a frontend update is in flight"),
        "{}",
        w.refusal()
    );
    assert_eq!(w.state().current, Some(c(A)));
    assert!(w.sim.borrow().other_runs.is_empty());
}

/// Catches one floor for two sequences. A node's orders and the frontend's updates are
/// counted by different counters, so a high node id must not make a low frontend id a
/// replay, nor the reverse: each kind has its own (epoch, id) floor.
///
/// Mutant planted: one floor for both kinds (the frontend order 2 dropped as a replay of
/// the node's 9).
#[test]
fn each_kind_of_order_has_its_own_floor() {
    let w = World::new("floors", A_);
    w.leases.set(1); // node activations are refused, and acknowledged
    let mut h = w.helper();
    w.put_node_activate(9, B);
    pass(&mut h, 0);
    assert_eq!(w.status()["acked_order_id"], 9);
    w.put(&record(2, 1, 1, B));
    pass(&mut h, 1);
    assert_eq!(
        w.asked(),
        ["start b"],
        "id 2 is above the frontend's floor of 0"
    );
    for n in 2..6 {
        pass(&mut h, n);
    }
    // The frontend is at 2; a node order 3 is below the node's 9 and is dropped as a replay,
    // while a frontend order 3 acts.
    w.put_node_activate(3, C);
    pass(&mut h, 6);
    assert_eq!(w.status()["acked_order_id"], 9);
    w.sim.borrow_mut().exits(A_);
    w.put(&record(3, 1, 2, C));
    pass(&mut h, 7);
    assert_eq!(w.asked(), ["start b", "start a"]);

    let mut memory = crate::helper::memory::Memory::default();
    memory.acknowledge(Kind::Frontend, 4, 7);
    assert_eq!(memory.floor(Kind::Frontend), (4, 7));
    assert_eq!(memory.floor(Kind::Node), (0, 0));
    memory.acknowledge(Kind::Node, 5, 8);
    assert_eq!(memory.floor(Kind::Node), (5, 8));
    assert_eq!(memory.floor(Kind::Frontend), (4, 7));
}

/// Catches the same record read twice (the pass that wrote the pending record died before it
/// removed the file) being refused as busy against itself: it is the update in flight, so
/// nothing is started a second time and the status stays in flight.
///
/// Mutant planted: the order id not compared (the repeat refused `frontend_busy`).
#[test]
fn the_same_record_read_again_is_the_update_in_flight() {
    let w = World::new("repeat", A_);
    let mut h = w.helper();
    let r = record(5, 1, 1, B);
    w.put(&r);
    pass(&mut h, 0);
    // The helper's memory is lost (as if it died before acknowledging) and the file is back.
    fs::remove_file(w.root.join("var/lib/kbf-deploy/helper.json")).expect("memory");
    fs::write(
        w.root.join(UPDATE).join("orders").join(r.file_name()),
        r.to_bytes().expect("b"),
    )
    .expect("again");
    let mut again = w.helper();
    pass(&mut again, 1);
    assert_eq!(w.asked(), ["start b"]);
    assert!(!w.refusal().contains("frontend_busy"), "{}", w.refusal());
    assert_eq!(w.status()["state"], "activated_unconfirmed");
}

/// Catches a `systemctl` that cannot be read being taken for evidence: a unit state that
/// cannot be read is no reason to keep, revert or give up an update, so the pass waits and
/// changes nothing; when systemd answers again the update goes on.
///
/// Mutant planted: an unreadable new instance read as stopped (the update reverted while
/// systemd was only briefly unreachable).
#[test]
fn an_unreadable_unit_is_not_evidence_of_anything() {
    let w = World::new("unreadable", A_);
    let mut h = w.helper();
    w.put(&record(5, 1, 1, B));
    pass(&mut h, 0);
    let before = pending(&w);
    w.sim.borrow_mut().unreadable = true;
    for n in 1..6 {
        pass(&mut h, n);
    }
    assert_eq!(pending(&w), before);
    assert_eq!(w.asked(), ["start b"]);
    w.sim.borrow_mut().unreadable = false;
    for n in 6..10 {
        pass(&mut h, n);
    }
    assert_eq!(w.state().current, Some(c(B)));
}

/// Catches a frontend update read as a node order: the record becomes an order of the
/// frontend's kind with its components (the release's locators) and no claims when it
/// carries no facts, claims when it does, and every other verb is a node's.
///
/// Mutant planted: the verb mapped to `Activate` (the node path would swap and restart).
#[test]
fn a_frontend_update_record_reads_as_a_frontend_order() {
    use crate::helper::order::{self, Verb};
    let r = record(5, 1, 1, B);
    let o = order::parse(1, 5, &r.to_bytes().expect("bytes")).expect("parses");
    assert_eq!(
        (o.verb, o.verb.kind()),
        (Verb::FrontendUpdate, Kind::Frontend)
    );
    assert_eq!(o.components.len(), 1);
    assert_eq!(o.components[0].name, "kbf");
    assert_eq!((o.claims, o.window_s, o.generation), (None, 600, 1));
    let mut with = record(6, 1, 1, B);
    with.expected = Some(Facts {
        platform: "linux-x86_64".to_owned(),
        protocol: 7,
        min_protocol: 6,
        state_schema: 3,
        profile: "release".to_owned(),
    });
    let o = order::parse(1, 6, &with.to_bytes().expect("bytes")).expect("parses");
    assert_eq!(o.claims.expect("claims").protocol, 7);
    for verb in [Verb::Activate, Verb::Confirm, Verb::Revert] {
        assert_eq!(verb.kind(), Kind::Node);
    }
}
