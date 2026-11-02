//! One live update at a time (`decide.rs`, rule 7) through a real [`crate::service::Coordinator`]:
//! a host that leaves a service mid-update still holds the slot, a halted service's holder
//! holds it, a restart resumes the rule from what the hosts report in the quiet start, and two
//! heartbeats that race for the one slot get one order between them. Every time is explicit;
//! nothing sleeps.

use std::sync::Barrier;

use kbf_updater::wire::{Heartbeat, ServiceBeat, ServiceReply};
use serde_json::{Value, json};

use crate::decide::Hold;
use crate::membership_tests::{V2, labels, operate, who};
use crate::services::Services;
use crate::testutil::{OPERATOR, Rig, beat, version};

/// `other`, on daemons in zone `a` (a selector), and `second`, on any daemon that announces it.
const SELECTED: &str = "[[service]]\nname = \"other\"\n[service.roles.daemon]\nmin_up = 0\n\
    [service.selector]\nlabels = { zone = \"a\" }\n\
    [[service]]\nname = \"second\"\n[service.roles.daemon]\nmin_up = 0\n";

/// `other`, which bakes for two beats, and `second`, both on daemons that announce them.
const PLAIN: &str = "[[service]]\nname = \"other\"\nbake_beats = 2\n\
    [service.roles.daemon]\nmin_up = 0\n\
    [[service]]\nname = \"second\"\n[service.roles.daemon]\nmin_up = 0\n";

fn start(rig: &Rig, text: &str) -> crate::service::Coordinator {
    let list = format!(
        "{}services = [\"kbf\", \"other\", \"second\"]\n",
        kbf_oidc::testkit::config_text(kbf_oidc::testkit::STATIC_ISSUER)
    );
    Rig::start_with_allow_list(&rig.dir, &rig.clock, &list)
        .with_services(Services::parse(text).expect("services"))
}

/// A rig past the quiet start over `text`, `other` and `second` desired at 3.0 (kbf has no
/// desired state, so it orders nothing).
async fn rig(name: &str, text: &str) -> Rig {
    let mut rig = Rig::new(name);
    rig.coordinator = start(&rig, text);
    for service in ["other", "second"] {
        rig.set(&format!(
            r#"{{"service":"{service}","version":"3.0","min_up":{{"storage":0,"frontend":0}}}}"#
        ))
        .await;
    }
    rig.at(2_000);
    rig
}

/// One service entry of a heartbeat: `name` at `running`, healthy, with the fields of `extra`.
fn entry(name: &str, running: &str, extra: &Value) -> Value {
    let mut one = json!({"name": name, "version": running, "health": "healthy"});
    for (key, value) in extra.as_object().expect("an object") {
        one[key] = value.clone();
    }
    one
}

/// A v2 agent's heartbeat, kbf at 1.0, with the service `entries`.
fn beat_of(entries: &[Value]) -> Heartbeat {
    let mut b = beat("1.0");
    b.agent_features = vec![V2.to_owned()];
    b.services = Some(
        serde_json::from_value::<Vec<ServiceBeat>>(Value::Array(entries.to_vec())).expect("beats"),
    );
    b
}

fn send(rig: &Rig, at: u64, host: &str, entries: &[Value]) -> ServiceReply {
    rig.at(at);
    rig.coordinator.heartbeat(who(host), beat_of(entries)).reply
}

fn order(service: &str) -> ServiceReply {
    ServiceReply::Update {
        service: Some(service.to_owned()),
        version: version("3.0"),
    }
}

/// The hold of `host` under `service` in the status.
fn hold(rig: &Rig, service: &str, host: &str) -> Option<Hold> {
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    status.per_service[service]
        .hosts
        .iter()
        .find(|h| h.host.node_id == host)
        .and_then(|h| h.hold)
}

fn other_at(running: &str) -> Value {
    entry("other", running, &json!({}))
}

fn other_updating() -> Value {
    entry("other", "1.0", &json!({"updating": "3.0"}))
}

fn second_behind() -> Value {
    entry("second", "1.0", &json!({}))
}

/// A way of leaving `other`: its name, what takes `d1` out, and what `d1` beats after.
type Leaving<'a> = (&'a str, &'a dyn Fn(&Rig), Vec<Value>);

/// `d1` (zone `a`) is ordered `other` and reports updating it; then `leave` takes it out of
/// `other`, and `d1` beats `during` while it still updates.
async fn leaving_mid_update(name: &str, leave: impl Fn(&Rig), during: &[Value]) -> Rig {
    let rig = rig(name, SELECTED).await;
    labels(&rig, "d1", r#"{"zone":"a"}"#);
    assert_eq!(send(&rig, 2_000, "d1", &[other_at("1.0")]), order("other"));
    send(&rig, 2_010, "d1", &[other_updating()]);
    leave(&rig);
    send(&rig, 2_015, "d1", during);
    rig
}

/// Behaviour: `d1` is updating `other` when it leaves the service: excluded by the operator,
/// moved out of the selector's zone, or no longer announcing `other`. `second` on `d2` is held
/// (`no_slot`) through each, and is ordered once `d1`'s beats show `other` done at 3.0. Catches
/// a membership change forgetting a slot that an update still holds.
#[tokio::test]
async fn a_host_that_leaves_a_service_mid_update_still_holds_the_slot() {
    let exclude = |rig: &Rig| {
        operate(
            rig,
            r#"{"action":"exclude","service":"other","node_id":"d1","reason":"disk"}"#,
        );
    };
    let rezone = |rig: &Rig| labels(rig, "d1", r#"{"zone":"b"}"#);
    let cases: [Leaving; 3] = [
        ("excluded", &exclude, vec![other_updating()]),
        ("not selected", &rezone, vec![other_updating()]),
        ("not announced", &|_: &Rig| {}, vec![]),
    ];
    for (case, leave, during) in cases {
        let rig = leaving_mid_update(
            &format!("one-live-leave-{}", case.replace(' ', "-")),
            leave,
            &during,
        )
        .await;
        assert_eq!(
            send(&rig, 2_020, "d2", &[second_behind()]),
            ServiceReply::Nothing,
            "{case}"
        );
        assert_eq!(hold(&rig, "second", "d2"), Some(Hold::NoSlot), "{case}");
        send(&rig, 2_030, "d1", &during);
        assert_eq!(
            send(&rig, 2_035, "d2", &[second_behind()]),
            ServiceReply::Nothing,
            "{case}: still updating"
        );
        send(&rig, 2_045, "d1", &[other_at("3.0")]);
        assert_eq!(
            send(&rig, 2_050, "d2", &[second_behind()]),
            order("second"),
            "{case}: done"
        );
    }
}

/// Behaviour: `d1` is ordered `other` and excluded before it reports anything: the order still
/// holds `second` on `d2` until it ages out, at 119 s and not at 120 s. Catches an exclusion
/// forgetting an order the host may already be acting on.
#[tokio::test]
async fn an_order_kept_through_an_exclusion_ages_out_as_any_order() {
    let rig = rig("one-live-excluded-order", SELECTED).await;
    labels(&rig, "d1", r#"{"zone":"a"}"#);
    assert_eq!(send(&rig, 2_000, "d1", &[other_at("1.0")]), order("other"));
    operate(
        &rig,
        r#"{"action":"exclude","service":"other","node_id":"d1","reason":"disk"}"#,
    );
    send(&rig, 2_010, "d1", &[other_at("1.0")]);
    assert_eq!(
        send(&rig, 2_119, "d2", &[second_behind()]),
        ServiceReply::Nothing
    );
    send(&rig, 2_119, "d1", &[other_at("1.0")]);
    assert_eq!(send(&rig, 2_120, "d2", &[second_behind()]), order("second"));
}

/// Behaviour: `other` is halted (`d3` reported a halt) while `d1` updates it: `d1` still holds
/// the slot against `second` on `d2` until it is done. Catches a halted service's update
/// dropped from the count.
#[tokio::test]
async fn an_update_of_a_halted_service_still_holds_the_slot() {
    let rig = rig("one-live-halted", PLAIN).await;
    assert_eq!(send(&rig, 2_000, "d1", &[other_at("1.0")]), order("other"));
    send(&rig, 2_010, "d1", &[other_updating()]);
    let halt = json!({"last_update": {"target": "3.0", "outcome": "failed", "phase": "halt",
        "reason": "disk_tainted"}});
    send(&rig, 2_010, "d3", &[entry("other", "1.0", &halt)]);
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    let other = serde_json::to_value(&status.per_service["other"].convergence).expect("json");
    assert_eq!(other["state"], "halted");
    assert_eq!(
        send(&rig, 2_015, "d2", &[second_behind()]),
        ServiceReply::Nothing
    );
    send(&rig, 2_025, "d1", &[other_at("3.0")]);
    send(&rig, 2_040, "d1", &[other_at("3.0")]);
    assert_eq!(send(&rig, 2_045, "d2", &[second_behind()]), order("second"));
}

/// Behaviour: after a restart, a holder that reports in the quiet start holds the slot as
/// before, and frees it under its own rule only: a pending promotion when it is done and the
/// two beats of `other`'s bake follow, a failed unhealthy host when it beats healthy, a host
/// baking when it has beaten twice since the restart (the count starts again). Catches a
/// restarted coordinator ordering past a holder it heard in the quiet start.
#[tokio::test]
async fn a_restart_resumes_the_rule_for_every_kind_of_holder() {
    let failed =
        json!({"health": "unhealthy", "last_update": {"target": "3.0", "outcome": "failed"}});
    let cases = [
        (
            "pending",
            entry("other", "3.0", &json!({"promotion_pending": true})),
            vec![other_at("3.0"), other_at("3.0")],
        ),
        (
            "failed",
            entry("other", "1.0", &failed),
            vec![entry(
                "other",
                "1.0",
                &json!({"last_update": {"target": "3.0", "outcome": "failed"}}),
            )],
        ),
        ("baking", other_at("3.0"), vec![other_at("3.0")]),
    ];
    for (case, holder, release) in cases {
        let mut rig = rig(&format!("one-live-restart-{case}"), PLAIN).await;
        send(&rig, 2_000, "d1", std::slice::from_ref(&holder));
        assert_eq!(
            send(&rig, 2_000, "d2", &[second_behind()]),
            ServiceReply::Nothing,
            "{case} before"
        );
        rig.at(2_020);
        rig.coordinator = start(&rig, PLAIN);
        send(&rig, 2_035, "d1", std::slice::from_ref(&holder));
        assert_eq!(
            send(&rig, 2_050, "d2", &[second_behind()]),
            ServiceReply::Nothing,
            "{case} after the quiet start"
        );
        let mut at = 2_050;
        for (n, word) in release.iter().enumerate() {
            at += 15;
            assert_eq!(
                send(&rig, at, "d2", &[second_behind()]),
                ServiceReply::Nothing,
                "{case}: before release beat {n}"
            );
            send(&rig, at, "d1", std::slice::from_ref(word));
        }
        assert_eq!(
            send(&rig, at, "d2", &[second_behind()]),
            order("second"),
            "{case} released"
        );
    }
}

/// Behaviour, the limit of a restart: orders are not stored, so a host ordered before the
/// restart that stays silent through the whole quiet start is not known to hold the slot, and
/// `d2` is ordered `second` after it. Once `d1` reports `other` updating, the slot is held
/// again and `d3` waits. (A host reports an order within one 15 s heartbeat; the quiet start is
/// 30 s.) Catches the documented limit changing unseen.
#[tokio::test]
async fn a_holder_silent_through_the_quiet_start_is_not_known_after_a_restart() {
    let mut rig = rig("one-live-restart-silent", PLAIN).await;
    assert_eq!(send(&rig, 2_000, "d1", &[other_at("1.0")]), order("other"));
    rig.at(2_020);
    rig.coordinator = start(&rig, PLAIN);
    assert_eq!(send(&rig, 2_050, "d2", &[second_behind()]), order("second"));
    send(&rig, 2_055, "d1", &[other_updating()]);
    assert_eq!(
        send(&rig, 2_055, "d3", &[second_behind()]),
        ServiceReply::Nothing
    );
}

/// Behaviour: `d1` behind on `other` and `d2` behind on `second` heartbeat at once, released
/// together by a barrier, against 16 fresh coordinators: each time exactly one is ordered, and
/// the other is told nothing on its next beat. Catches the slot read and taken in two steps
/// that another heartbeat can come between.
#[tokio::test]
async fn racing_heartbeats_for_two_services_get_one_order() {
    for round in 0..16 {
        let rig = rig(&format!("one-live-race-{round}"), PLAIN).await;
        let barrier = Barrier::new(2);
        let (first, second) = std::thread::scope(|s| {
            let a = s.spawn(|| {
                barrier.wait();
                rig.coordinator
                    .heartbeat(who("d1"), beat_of(&[other_at("1.0")]))
                    .reply
            });
            let b = s.spawn(|| {
                barrier.wait();
                rig.coordinator
                    .heartbeat(who("d2"), beat_of(&[second_behind()]))
                    .reply
            });
            (a.join().expect("d1"), b.join().expect("d2"))
        });
        let ordered = [&first, &second]
            .iter()
            .filter(|r| ***r != ServiceReply::Nothing)
            .count();
        assert_eq!(ordered, 1, "round {round}: {first:?} {second:?}");
        let (waiting, entries) = if first == ServiceReply::Nothing {
            ("d1", vec![other_at("1.0")])
        } else {
            ("d2", vec![second_behind()])
        };
        assert_eq!(
            send(&rig, 2_000, waiting, &entries),
            ServiceReply::Nothing,
            "round {round}"
        );
    }
}
