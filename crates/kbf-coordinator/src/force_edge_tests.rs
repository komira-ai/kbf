//! An operator's force at its edges: CI's write racing it or failing under it, an order that is
//! lost, a halt of the target retried, slots at their limits and boundaries, the last good
//! version without a bake, and other services beside it (`crate::force`).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Barrier};

use kbf_updater::wire::{Health, Heartbeat, Outcome, ServiceBeat, ServiceReply, UpdateResult};
use serde_json::json;

use crate::decide::Hold;
use crate::fleet::{DOWN_AFTER, ORDER_AGE};
use crate::force_tests::{
    GATED, agent, force, force_body, halted, hold_of, operate, restart, rig, send, update,
    updating_of,
};
use crate::ops::{OperateError, OpsError, OpsState, OpsStore};
use crate::service::SetError;
use crate::services::Services;
use crate::testutil::{OPERATOR, Rig, beat, ci_token, version};

const DESIRED_2: &str = r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#;

fn now(rig: &Rig) -> u64 {
    rig.clock.load(Ordering::SeqCst)
}

fn actions_from(rig: &Rig, from: u64) -> Vec<String> {
    rig.coordinator
        .ops()
        .state()
        .audit
        .into_iter()
        .filter(|e| e.seq >= from)
        .map(|e| e.action)
        .collect()
}

fn last_seq(rig: &Rig) -> u64 {
    rig.coordinator
        .ops()
        .state()
        .audit
        .last()
        .map_or(0, |e| e.seq)
}

fn desired_version(rig: &Rig) -> String {
    rig.coordinator
        .status(Some(OPERATOR))
        .expect("status")
        .services["kbf"]
        .version
        .to_string()
}

/// Makes the desired state's next writes fail: a directory where its temporary file goes.
fn break_desired(rig: &Rig) {
    std::fs::create_dir(rig.dir.path().join("desired.json.new")).expect("a directory");
}

fn mend_desired(rig: &Rig) {
    std::fs::remove_dir(rig.dir.path().join("desired.json.new")).expect("removed");
}

/// Catches: CI's write lifting a hold a force put on its version after CI's request was read:
/// the hold is decided on the state the change is applied to, so a hold that appears just
/// before CI's change is seen, the write is refused, and the mark, its hold and the desired
/// state are as they were.
#[tokio::test]
async fn ci_cannot_lift_a_hold_that_appeared_after_its_request_was_read() {
    let rig = rig("force-edge-race-hold", GATED);
    rig.set(DESIRED_2).await;
    rig.coordinator.ops().interleave(|state| {
        let marks = state.services.entry("kbf".to_owned()).or_default();
        marks.bad.push(version("1.6"));
        marks.bad_by_force.push(version("1.6"));
    });
    let body = r#"{"version":"1.6","min_up":{"storage":0,"frontend":0}}"#;
    let refused = rig
        .coordinator
        .set_desired(Some(&ci_token()), body.as_bytes())
        .await;
    assert!(matches!(refused, Err(SetError::HeldBad(v)) if v == version("1.6")));
    let marks = rig.coordinator.ops().marks("kbf");
    assert_eq!(marks.bad, [version("1.6")]);
    assert_eq!(marks.bad_by_force, [version("1.6")]);
    assert_eq!(desired_version(&rig), "2.0");
}

/// Catches: a force coming between CI's desired write and the end of the force it replaces,
/// which would end the new force: CI's write is made under the fleet's lock, the lock a force
/// takes, so between the state's write and the end the lock is held and no force can be in
/// between; the force is ended only after the state is written.
#[tokio::test]
async fn ci_writes_its_desired_state_under_the_lock_a_force_takes() {
    let rig = rig("force-edge-race-lock", GATED);
    rig.set(DESIRED_2).await;
    force(&rig, "1.5", "");
    let pause = Arc::new(Barrier::new(2));
    rig.coordinator.pause_ci_write(Arc::clone(&pause));
    let token = ci_token();
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("a runtime");
            runtime.block_on(rig.coordinator.set_desired(
                Some(&token),
                r#"{"version":"1.6","min_up":{"storage":0,"frontend":0}}"#.as_bytes(),
            ))
        });
        pause.wait();
        assert!(
            rig.coordinator.fleet_held(),
            "CI writes under the fleet's lock"
        );
        // The state is written; the force is still recorded, and ends next.
        assert!(rig.coordinator.ops().marks("kbf").force.is_some());
        let file = std::fs::read_to_string(rig.dir.path().join("desired.json")).expect("desired");
        assert!(file.contains(r#""version": "1.6""#), "{file}");
        pause.wait();
        writer.join().expect("joined").expect("set");
    });
    assert_eq!(desired_version(&rig), "1.6");
    assert_eq!(rig.coordinator.ops().marks("kbf").force, None);
}

/// Catches: CI's write ending a force when its desired state then cannot be written, which
/// would leave the forced version desired under the ordinary gates: nothing of the force is
/// touched before the state is written, so it stands, with no entry in the audit; written once
/// the store is mended, it ends the force.
#[tokio::test]
async fn a_force_stands_when_cis_desired_state_cannot_be_written() {
    let rig = rig("force-edge-ci-fails", GATED);
    rig.set(DESIRED_2).await;
    force(&rig, "1.5", "");
    let before = rig.coordinator.ops().marks("kbf").force;
    let seq = last_seq(&rig);
    break_desired(&rig);
    let body = r#"{"version":"1.6","min_up":{"storage":0,"frontend":0}}"#;
    let failed = rig
        .coordinator
        .set_desired(Some(&ci_token()), body.as_bytes())
        .await;
    assert!(matches!(failed, Err(SetError::Store(_))));
    assert!(actions_from(&rig, seq + 1).is_empty());
    assert_eq!(rig.coordinator.ops().marks("kbf").force, before);
    assert_eq!(desired_version(&rig), "1.5");
    let answer = send(&rig, "d1", agent("2.0", Health::Healthy));
    assert!(answer.forced);
    mend_desired(&rig);
    rig.set(body).await;
    assert_eq!(actions_from(&rig, seq + 1), ["force_ended"]);
    assert_eq!(rig.coordinator.ops().marks("kbf").force, None);
}

/// Catches: a force whose order to a host that failed the target before is lost (or declined
/// by the agent's own backoff) never reaching that host again: while the host's beat shows no
/// attempt, it is ordered again, past the order's age too; once it is seen updating, its
/// acknowledgement goes, and the same failure after that attempt holds it.
#[tokio::test]
async fn a_lost_forced_order_is_given_again_until_the_host_is_seen_updating() {
    let rig = rig("force-edge-lost", GATED);
    rig.set(r#"{"version":"1.5","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    let mut failed = agent("2.0", Health::Healthy);
    failed.last_update = Some(UpdateResult::new(version("1.5"), Outcome::Failed));
    send(&rig, "d1", failed.clone());
    force(&rig, "1.5", "");
    for _ in 0..3 {
        assert_eq!(send(&rig, "d1", failed.clone()), update("1.5"));
    }
    rig.at(now(&rig) + ORDER_AGE.as_secs() + 1);
    assert_eq!(send(&rig, "d1", failed.clone()), update("1.5"));
    assert_eq!(rig.coordinator.ops().marks("kbf").failure_acked.len(), 1);
    send(&rig, "d1", updating_of(failed.clone(), "1.5"));
    assert!(rig.coordinator.ops().marks("kbf").failure_acked.is_empty());
    assert_eq!(send(&rig, "d1", failed), ServiceReply::Nothing);
    assert_eq!(hold_of(&rig, "d1"), Some(Hold::FailedHere));
}

/// Catches: a retried halt hidden by the acknowledgement of the halt before it: a host whose
/// halt of the target was cleared by the force is ordered the target again; once it is seen
/// updating that target, the acknowledgement ends, and the very same halt report after it halts
/// the service again (the force goes on for the others).
#[tokio::test]
async fn a_halt_of_the_forced_target_reported_again_after_a_retry_halts_again() {
    let rig = rig("force-edge-halt-retry", GATED);
    rig.set(r#"{"version":"1.5","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    let mut halting = agent("2.0", Health::Healthy);
    halting.last_update = Some(halted("1.5"));
    send(&rig, "s1", halting.clone());
    assert!(rig.coordinator.ops().marks("kbf").halt.is_some());
    force(&rig, "1.5", "");
    assert_eq!(send(&rig, "s1", halting.clone()), update("1.5"));
    assert_eq!(rig.coordinator.ops().marks("kbf").halt, None);
    let seq = last_seq(&rig);
    send(&rig, "s1", updating_of(halting.clone(), "1.5"));
    assert!(actions_from(&rig, seq + 1).contains(&"halt_ack_ended".to_owned()));
    send(&rig, "s1", halting);
    let halt = rig
        .coordinator
        .ops()
        .marks("kbf")
        .halt
        .expect("halted again");
    assert_eq!(halt.node_id, "s1");
}

/// Catches: a host that runs the forced version kept in a slot by what else its beat says: at
/// the forced version, still reporting the update or waiting for a promotion, it holds none,
/// and the next host of its role is ordered, while a host updating towards it holds one.
#[tokio::test]
async fn a_host_at_the_forced_version_frees_its_slot_whatever_else_it_reports() {
    let rig = rig("force-edge-slot-at", GATED);
    rig.set(DESIRED_2).await;
    for name in ["d1", "d2"] {
        send(&rig, name, agent("2.0", Health::Healthy));
    }
    force(&rig, "1.5", r#","max_updating":{"daemon":1}"#);
    assert_eq!(
        send(&rig, "d1", agent("2.0", Health::Healthy)),
        update("1.5")
    );
    send(
        &rig,
        "d1",
        updating_of(agent("2.0", Health::Healthy), "1.5"),
    );
    assert_eq!(hold_of(&rig, "d2"), Some(Hold::NoSlot));
    send(
        &rig,
        "d1",
        updating_of(agent("1.5", Health::Unhealthy), "1.5"),
    );
    assert_eq!(hold_of(&rig, "d2"), None);
    let mut pending = agent("1.5", Health::Unhealthy);
    pending.promotion_pending = true;
    send(&rig, "d1", pending);
    assert_eq!(
        send(&rig, "d2", agent("2.0", Health::Healthy)),
        update("1.5")
    );
}

/// The rig over a `kbf` whose daemon role caps itself at `cap` hosts.
fn capped(name: &str, cap: u32) -> Rig {
    let text = GATED.replace(
        "[service.roles.daemon]\n",
        &format!("[service.roles.daemon]\nmax_updating = {cap}\n"),
    );
    rig(name, &text)
}

async fn three_daemons(rig: &Rig) {
    rig.set(DESIRED_2).await;
    for name in ["d1", "d2", "d3"] {
        send(rig, name, agent("2.0", Health::Healthy));
    }
}

fn round(rig: &Rig) -> Vec<bool> {
    ["d1", "d2", "d3"]
        .iter()
        .map(|n| send(rig, n, agent("2.0", Health::Healthy)) == update("1.5"))
        .collect()
}

/// Catches: a force's pace taken from the wrong place: a role's own cap in the definition holds
/// when the request names none, a raise in the request wins over it (from 0, too), and the
/// largest accepted value is taken.
#[tokio::test]
async fn a_definitions_cap_holds_unless_the_force_raises_it() {
    let rig = capped("force-edge-cap", 1);
    three_daemons(&rig).await;
    force(&rig, "1.5", "");
    assert_eq!(round(&rig), [true, false, false]);

    let rig = capped("force-edge-cap-raised", 1);
    three_daemons(&rig).await;
    force(&rig, "1.5", r#","max_updating":{"daemon":2}"#);
    assert_eq!(round(&rig), [true, true, false]);

    let rig = capped("force-edge-cap-zero", 0);
    three_daemons(&rig).await;
    force(&rig, "1.5", "");
    assert_eq!(round(&rig), [false, false, false]);
    assert_eq!(hold_of(&rig, "d1"), Some(Hold::NoSlot));
    force(&rig, "1.5", r#","max_updating":{"daemon":10000}"#);
    assert_eq!(round(&rig), [true, true, true]);
}

/// Catches: slots held by hosts that no longer work on the force, in a farm of agents with and
/// without `forced-v1`: an order a host never reports frees its slot after its age, beats that
/// show no progress do not keep it longer, and a silent host of either kind holds none; the
/// next host is then ordered, never more than the limit at once.
#[tokio::test]
async fn slots_free_at_the_order_age_and_the_down_limit_in_a_mixed_farm() {
    let rig = rig("force-edge-mixed", GATED);
    rig.set(DESIRED_2).await;
    send(&rig, "d1", beat("2.0"));
    send(&rig, "d2", agent("2.0", Health::Healthy));
    send(&rig, "d3", agent("2.0", Health::Healthy));
    force(&rig, "1.5", r#","max_updating":{"daemon":1}"#);
    let start = now(&rig);
    // d2 is ordered and never reports it; its beats without progress do not extend the order.
    assert_eq!(
        send(&rig, "d2", agent("2.0", Health::Healthy)),
        update("1.5")
    );
    for step in [10, 60, 110] {
        rig.at(start + step);
        send(&rig, "d2", agent("2.0", Health::Healthy));
        assert_eq!(hold_of(&rig, "d3"), Some(Hold::NoSlot), "{step}");
    }
    rig.at(start + ORDER_AGE.as_secs());
    send(&rig, "d3", agent("2.0", Health::Healthy));
    assert_eq!(hold_of(&rig, "d3"), None);
    // d1, an agent without `forced-v1`, fails and holds its slot the ordinary way, until it is
    // silent past the down limit.
    let rig = self::rig("force-edge-mixed-legacy", GATED);
    rig.set(DESIRED_2).await;
    send(&rig, "d1", beat("2.0"));
    send(&rig, "d3", agent("2.0", Health::Healthy));
    force(&rig, "1.5", r#","max_updating":{"daemon":1}"#);
    let mut back = beat("2.0");
    back.health = Health::Unhealthy;
    back.last_update = Some(UpdateResult::new(version("1.5"), Outcome::Failed));
    send(&rig, "d1", back);
    let start = now(&rig);
    send(&rig, "d3", agent("2.0", Health::Healthy));
    assert_eq!(hold_of(&rig, "d3"), Some(Hold::NoSlot));
    rig.at(start + DOWN_AFTER.as_secs());
    assert_eq!(
        send(&rig, "d3", agent("2.0", Health::Healthy)),
        update("1.5")
    );
}

/// `GATED` with no bake.
fn unbaked() -> String {
    GATED.replace("bake_beats = 3\n", "")
}

/// Catches: a forced version recorded as the last good one on convergence alone: with no bake,
/// hosts that run it unhealthy record nothing, a farm with no live host records nothing, and
/// the first beat with every live host up on it records it. A force to `min_version` itself
/// forgets an older last good version, as any desired state of it does.
#[tokio::test]
async fn a_forced_version_is_the_last_good_one_only_once_every_live_host_serves_it() {
    let mut rig = rig("force-edge-good", &unbaked());
    rig.set(r#"{"version":"1.2","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    for name in ["d1", "s1", "f1"] {
        send(&rig, name, agent("1.2", Health::Healthy));
    }
    assert_eq!(
        rig.coordinator.ops().marks("kbf").last_good,
        Some(version("1.2"))
    );
    force(
        &rig,
        "1.5",
        r#","max_updating":{"daemon":1,"storage":1,"frontend":1}"#,
    );
    for name in ["d1", "s1", "f1"] {
        send(&rig, name, agent("1.5", Health::Unhealthy));
    }
    assert_eq!(
        rig.coordinator.ops().marks("kbf").last_good,
        Some(version("1.2"))
    );
    // Silent past the down limit, then a restart: no live host, nothing recorded.
    rig.at(now(&rig) + DOWN_AFTER.as_secs());
    restart(&mut rig, &unbaked());
    assert_eq!(
        rig.coordinator.ops().marks("kbf").last_good,
        Some(version("1.2"))
    );
    send(&rig, "f1", agent("1.5", Health::Unhealthy));
    for name in ["d1", "s1"] {
        send(&rig, name, agent("1.5", Health::Healthy));
    }
    assert_eq!(
        rig.coordinator.ops().marks("kbf").last_good,
        Some(version("1.2"))
    );
    send(&rig, "f1", agent("1.5", Health::Healthy));
    assert_eq!(
        rig.coordinator.ops().marks("kbf").last_good,
        Some(version("1.5"))
    );
    force(&rig, "1.0", "");
    send(&rig, "d1", agent("1.5", Health::Healthy));
    assert_eq!(rig.coordinator.ops().marks("kbf").last_good, None);
}

/// Catches: the status showing slots the forced decision does not count: under a force, a host
/// at the forced version that failed before and is not healthy shows no slot (the next host is
/// ordered), and `slots_held` agrees with the decision.
#[tokio::test]
async fn the_status_counts_slots_as_the_forced_decision_does() {
    let rig = rig("force-edge-status-slots", GATED);
    rig.set(DESIRED_2).await;
    for name in ["d1", "d2"] {
        send(&rig, name, agent("2.0", Health::Healthy));
    }
    force(&rig, "1.5", r#","max_updating":{"daemon":1}"#);
    let mut at = agent("1.5", Health::Unhealthy);
    at.last_update = Some(UpdateResult::new(version("1.9"), Outcome::Failed));
    send(&rig, "d1", at);
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    let kbf = &status.per_service["kbf"];
    let d1 = kbf
        .hosts
        .iter()
        .find(|h| h.host.node_id == "d1")
        .expect("d1");
    assert!(!d1.host.holds_slot);
    assert_eq!(kbf.slots_held, 0);
    assert!(!status.hosts.iter().any(|h| h.holds_slot));
    assert_eq!(hold_of(&rig, "d2"), None);
}

/// Catches: an unmark answered as made when its write failed, which would let CI set the
/// version: the answer is the store error, and the mark and its hold stand.
#[tokio::test]
async fn an_unmark_that_cannot_be_written_leaves_the_hold() {
    let mut rig = rig("force-edge-unmark-fails", GATED);
    let mut state = OpsState::default();
    let marks = state.services.entry("kbf".to_owned()).or_default();
    marks.bad.push(version("2.0"));
    marks.bad_by_force.push(version("2.0"));
    let writer = Box::new(|_: &OpsState| {
        Err(OpsError::Io(crate::desired::StoreError::Io {
            path: "ops.json".into(),
            source: std::io::Error::other("disk full"),
        }))
    });
    rig.coordinator = Rig::start(&rig.dir, &rig.clock)
        .with_services(Services::parse(GATED).expect("services"))
        .with_ops(OpsStore::with_writer(state, writer));
    let body = r#"{"action":"unmark_bad","service":"kbf","version":"2.0","reason":"fixed"}"#;
    assert!(matches!(operate(&rig, body), Err(OperateError::Store(_))));
    let marks = rig.coordinator.ops().marks("kbf");
    assert_eq!(marks.bad_by_force, [version("2.0")]);
    let refused = rig
        .coordinator
        .set_desired(Some(&ci_token()), DESIRED_2.as_bytes())
        .await;
    assert!(matches!(refused, Err(SetError::HeldBad(_))));
}

/// Catches: a force whose desired state cannot be written, over a farm with a desired state and
/// a force already, leaving something other than its marks: the version left is marked and
/// held, the new force (another target, then the same one) is recorded with its generation but
/// does not stand, the earlier force no longer stands either, and after a restart the force
/// asked again stands with the next generation.
#[tokio::test]
async fn a_force_whose_desired_state_fails_over_an_earlier_force_is_asked_again() {
    let mut rig = rig("force-edge-desired-fails", GATED);
    rig.set(DESIRED_2).await;
    force(&rig, "1.5", "");
    break_desired(&rig);
    for target in ["1.4", "1.5"] {
        let err = operate(&rig, &force_body(target, "")).expect_err("not written");
        assert!(matches!(err, OperateError::Desired(_)), "{target}");
    }
    let marks = rig.coordinator.ops().marks("kbf");
    assert_eq!(marks.force_generation, 3);
    assert_eq!(
        marks.force.as_ref().map(|f| f.version.clone()),
        Some(version("1.5"))
    );
    assert_eq!(marks.bad_by_force, [version("2.0")]);
    assert_eq!(desired_version(&rig), "1.5");
    mend_desired(&rig);
    restart(&mut rig, GATED);
    force(&rig, "1.4", "");
    let marks = rig.coordinator.ops().marks("kbf");
    assert_eq!(marks.force_generation, 4);
    assert_eq!(desired_version(&rig), "1.4");
    assert_eq!(marks.bad_by_force, [version("2.0"), version("1.5")]);
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert!(status.per_service["kbf"].forced.is_some());
}

/// A services file: [`GATED`]'s `kbf`, and `other`, on daemons, after `kbf`.
fn two_services() -> String {
    format!(
        "{GATED}\n[[service]]\nname = \"other\"\nafter = [\"kbf\"]\n\n[service.roles.daemon]\nmin_up = 0\n"
    )
}

/// `name`'s beat running `kbf` at `running` and `other` at `other`.
fn with_other(running: &str, other: &str) -> Heartbeat {
    let mut b = agent(running, Health::Healthy);
    let list = json!([{"name": "other", "version": other, "health": "healthy"}]);
    b.services = Some(serde_json::from_value::<Vec<ServiceBeat>>(list).expect("beats"));
    b
}

fn hold_in(rig: &Rig, service: &str, name: &str) -> Option<Hold> {
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    status.per_service[service]
        .hosts
        .iter()
        .find(|h| h.host.node_id == name)
        .and_then(|h| h.hold)
}

/// Catches: a force of one service touching another, or passing the order between services and
/// a host's one update at a time: forcing `kbf` leaves `other`'s desired state and its force as
/// they were; `other`, forced too, waits for `kbf`'s hosts, and a host with a `kbf` order is
/// busy for `other`; an excluded host gets no forced order.
#[tokio::test]
async fn a_force_keeps_to_its_service_and_the_order_between_services() {
    let rig = rig("force-edge-services", &two_services());
    let force_other = r#"{"action":"force","service":"other","version":"1.1","reason":"x"}"#;
    operate(&rig, force_other).expect("forced");
    let other_before = rig.coordinator.ops().marks("other");
    rig.set(DESIRED_2).await;
    let exclude = r#"{"action":"exclude","service":"kbf","node_id":"d3","reason":"disk swap"}"#;
    operate(&rig, exclude).expect("excluded");
    force(&rig, "1.5", r#","max_updating":{"daemon":1}"#);
    assert_eq!(rig.coordinator.ops().marks("other"), other_before);
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert_eq!(status.services["other"].version, version("1.1"));
    // d1 takes kbf's order; d2 waits for a kbf slot, and other waits for kbf.
    let answer = send(&rig, "d1", with_other("2.0", "1.0"));
    assert_eq!(answer, update("1.5"));
    assert_eq!(hold_in(&rig, "other", "d1"), Some(Hold::BusyElsewhere));
    send(&rig, "d2", with_other("2.0", "1.0"));
    assert_eq!(hold_in(&rig, "other", "d2"), Some(Hold::WaitingForService));
    assert_eq!(
        send(&rig, "d3", agent("2.0", Health::Healthy)),
        ServiceReply::Nothing
    );
    assert_eq!(
        hold_in(&rig, "kbf", "d3"),
        None,
        "an excluded host is not in kbf's status"
    );
}

/// An ops store over `state` whose writes succeed `ok` times and then fail.
fn failing_after(state: OpsState, ok: usize) -> OpsStore {
    let left = std::sync::atomic::AtomicUsize::new(ok);
    OpsStore::with_writer(
        state,
        Box::new(move |_: &OpsState| {
            if left.load(Ordering::SeqCst) == 0 {
                return Err(OpsError::Io(crate::desired::StoreError::Io {
                    path: "ops.json".into(),
                    source: std::io::Error::other("disk full"),
                }));
            }
            left.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }),
    )
}

/// Catches: a stop between CI's desired write and the end of the force (here the end cannot be
/// written, and the coordinator starts again from its files) leaving a force that stands again
/// when its version is desired later: the record left does not stand under CI's state, and the
/// next CI write, of the forced version itself, ends it before writing, so that version is
/// rolled under the ordinary rules.
#[tokio::test]
async fn a_force_left_by_a_stop_after_cis_write_does_not_stand_again() {
    let mut rig = rig("force-edge-end-fails", GATED);
    rig.set(DESIRED_2).await;
    force(&rig, "1.5", "");
    let state = rig.coordinator.ops().state();
    rig.coordinator = Rig::start(&rig.dir, &rig.clock)
        .with_services(Services::parse(GATED).expect("services"))
        .with_ops(failing_after(state, 0));
    rig.set(r#"{"version":"1.6","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    assert_eq!(desired_version(&rig), "1.6");
    restart(&mut rig, GATED);
    let marks = rig.coordinator.ops().marks("kbf");
    assert_eq!(
        marks.force.as_ref().map(|f| f.version.clone()),
        Some(version("1.5"))
    );
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert!(status.per_service["kbf"].forced.is_none());
    let seq = last_seq(&rig);
    rig.set(r#"{"version":"1.5","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    let audit = rig.coordinator.ops().state().audit;
    let ended: Vec<_> = audit.iter().filter(|e| e.seq > seq).collect();
    assert_eq!(ended.len(), 1);
    assert_eq!(
        ended[0].reason.as_deref(),
        Some("the force to 1.5 did not stand and ends: 1.5 is to be set")
    );
    assert_eq!(rig.coordinator.ops().marks("kbf").force, None);
    let answer = send(&rig, "d1", agent("2.0", Health::Healthy));
    assert!(!answer.forced);
}

/// Catches: an order given while the halt acknowledgement a new attempt should end is still on
/// disk: when that end cannot be written, the host is told nothing of the service, and the
/// acknowledgement stands.
#[tokio::test]
async fn an_attempt_whose_halt_acknowledgement_cannot_be_ended_is_told_nothing() {
    let mut rig = rig("force-edge-ack-end-fails", GATED);
    rig.set(r#"{"version":"1.5","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    let mut halting = agent("2.0", Health::Healthy);
    halting.last_update = Some(halted("1.5"));
    send(&rig, "s1", halting.clone());
    force(&rig, "1.5", "");
    let state = rig.coordinator.ops().state();
    rig.coordinator = Rig::start(&rig.dir, &rig.clock)
        .with_services(Services::parse(GATED).expect("services"))
        .with_ops(failing_after(state, 0));
    rig.at(now(&rig) + 31);
    let answer = send(&rig, "s1", updating_of(halting, "1.5"));
    assert_eq!(answer, ServiceReply::Nothing);
    assert_eq!(rig.coordinator.ops().marks("kbf").halt_acked.len(), 1);
}
