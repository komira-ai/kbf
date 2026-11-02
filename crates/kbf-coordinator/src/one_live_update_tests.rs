//! One live update at a time across every service and role (`decide.rs`, rule 7): a host
//! holding an update slot for any service holds the next order of every service, whatever
//! each role's `max_updating` says, and a restart mid-update resumes under the same rule.
//! Every time is explicit; the fleets are built beat by beat.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_oidc::testkit::{STATIC_ISSUER, config_text};
use kbf_types::HostRole;
use kbf_updater::wire::{Health, Heartbeat, Outcome, ServiceBeat, ServiceReply};

use crate::decide::{Decision, Hold, decide_why};
use crate::desired::Desired;
use crate::fleet::Fleet;
use crate::identity::Identity;
use crate::services::Services;
use crate::testutil::{Rig, beat, id, last, secs, unhealthy, updating, version};

/// `a` runs on daemons and bakes for two beats; `b` runs on daemons and storage hosts; `wide`
/// lets three daemons update at once by its own number. None keeps a host up.
const SERVICES: &str = "[[service]]\nname = \"a\"\nbake_beats = 2\n\
                        [service.roles.daemon]\nmin_up = 0\n\
                        [[service]]\nname = \"b\"\n\
                        [service.roles.daemon]\nmin_up = 0\n\
                        [service.roles.storage]\nmin_up = 0\n\
                        [[service]]\nname = \"wide\"\n\
                        [service.roles.daemon]\nmax_updating = 3\nmin_up = 0\n";

fn services() -> Services {
    Services::parse(SERVICES).expect("services")
}

fn want(target: &str) -> Desired {
    let text = format!(r#"{{"version":"{target}","min_up":{{"storage":0,"frontend":0}}}}"#);
    Desired::parse(text.as_bytes()).expect("parsed")
}

/// Every service of [`SERVICES`] wants 2.0.
fn all() -> BTreeMap<String, Desired> {
    ["a", "b", "wide"]
        .into_iter()
        .map(|s| (s.to_owned(), want("2.0")))
        .collect()
}

fn why(f: &Fleet, all: &BTreeMap<String, Desired>, service: &str, host: &str, at: u64) -> Decision {
    decide_why(&services(), all, service, f, &id(host), secs(at))
}

fn go() -> Decision {
    Decision::Update {
        version: version("2.0"),
    }
}

fn held(why: Hold) -> Decision {
    Decision::Hold(why)
}

/// The decision, noted as an order when it is one, as the coordinator does under its lock.
fn ask(f: &mut Fleet, service: &str, host: &str, at: u64) -> Decision {
    let decision = why(f, &all(), service, host, at);
    if let Decision::Update { version } = &decision {
        f.order(service, id(host), version.clone(), secs(at));
    }
    decision
}

fn record(f: &mut Fleet, service: &str, host: &str, role: HostRole, b: Heartbeat, at: u64) {
    f.record_in(service, id(host), role, b, secs(at));
}

/// Behaviour: with `a` behind on `d1` and `b` behind on `d2`, the first asked is ordered and
/// the other service waits through the order, the update and the bake, and is ordered once
/// `d1` has beaten healthy twice at 2.0. The same holds on one host: `d1` behind on `b` waits
/// while it bakes `a`. Catches the slot counted per service, so two services take two hosts
/// down together, and a bake of one service not holding another's order.
#[test]
fn two_services_with_pending_updates_order_one_host_and_the_second_waits_for_the_first() {
    let mut f = Fleet::new(Duration::ZERO);
    record(&mut f, "a", "d1", HostRole::DAEMON, beat("1.0"), 100);
    record(&mut f, "b", "d1", HostRole::DAEMON, beat("1.0"), 100);
    record(&mut f, "b", "d2", HostRole::DAEMON, beat("1.0"), 100);
    assert_eq!(ask(&mut f, "a", "d1", 100), go());
    assert_eq!(ask(&mut f, "b", "d2", 100), held(Hold::NoSlot), "ordered");

    record(
        &mut f,
        "a",
        "d1",
        HostRole::DAEMON,
        updating(beat("1.0"), "2.0"),
        115,
    );
    assert_eq!(ask(&mut f, "b", "d2", 115), held(Hold::NoSlot), "updating");

    record(&mut f, "a", "d1", HostRole::DAEMON, beat("2.0"), 130);
    assert_eq!(ask(&mut f, "b", "d2", 130), held(Hold::Baking), "one beat");
    assert_eq!(
        ask(&mut f, "b", "d1", 130),
        held(Hold::Baking),
        "its own bake"
    );

    record(&mut f, "a", "d1", HostRole::DAEMON, beat("2.0"), 145);
    assert_eq!(ask(&mut f, "b", "d2", 145), go(), "baked and released");
    assert_eq!(ask(&mut f, "b", "d1", 145), held(Hold::NoSlot), "d2 has it");
}

/// Behaviour: `d1` failed `a` and is unhealthy. It holds the one slot against `a` on another
/// daemon, `b` on a storage host and `b` on itself, so no service orders anything. Only `d1`
/// may be told a new version of the service it failed (its own slot is not counted against
/// it). Once it heartbeats healthy, every service orders again. Catches a failure of one
/// service or role not holding the next order of the others.
#[test]
fn a_failed_unhealthy_host_blocks_every_services_next_order() {
    let failed = unhealthy(last(beat("1.0"), "2.0", Outcome::Failed));
    let mut f = Fleet::new(Duration::ZERO);
    record(&mut f, "a", "d1", HostRole::DAEMON, failed.clone(), 100);
    record(&mut f, "b", "d1", HostRole::DAEMON, beat("1.0"), 100);
    record(&mut f, "a", "d2", HostRole::DAEMON, beat("1.0"), 100);
    record(&mut f, "b", "s1", HostRole::STORAGE, beat("1.0"), 100);
    let blocked = [("a", "d2"), ("b", "s1"), ("b", "d1")];
    for (service, host) in blocked {
        assert_eq!(
            why(&f, &all(), service, host, 100),
            held(Hold::NoSlot),
            "{service} on {host}"
        );
    }
    assert_eq!(why(&f, &all(), "a", "d1", 100), held(Hold::FailedHere));
    let mut newer = all();
    newer.insert("a".to_owned(), want("3.0"));
    assert_eq!(
        why(&f, &newer, "a", "d1", 100),
        Decision::Update {
            version: version("3.0")
        }
    );
    assert_eq!(why(&f, &newer, "a", "d2", 100), held(Hold::NoSlot));

    let mut healthy = failed;
    healthy.health = Health::Healthy;
    record(&mut f, "a", "d1", HostRole::DAEMON, healthy, 115);
    for (service, host) in blocked {
        assert_eq!(
            why(&f, &all(), service, host, 115),
            go(),
            "{service} on {host}"
        );
    }
}

/// Behaviour: `wide` allows three daemons by its own number, yet of four daemons behind one
/// is ordered at a time: each round asks every host, then the one ordered reports updating and
/// then done. Every host is ordered exactly once, and never two in one round. Catches a
/// `max_updating` above one still widening a roll.
#[test]
fn one_service_with_max_updating_three_still_updates_one_host_at_a_time() {
    let hosts = ["d1", "d2", "d3", "d4"];
    let mut f = Fleet::new(Duration::ZERO);
    for host in hosts {
        record(&mut f, "wide", host, HostRole::DAEMON, beat("1.0"), 100);
    }
    let mut order = Vec::new();
    for round in 0..hosts.len() as u64 {
        let at = 100 + round * 30;
        let ordered: Vec<&str> = hosts
            .into_iter()
            .filter(|host| ask(&mut f, "wide", host, at) == go())
            .collect();
        assert_eq!(ordered.len(), 1, "round {round}: {ordered:?}");
        let host = ordered[0];
        order.push(host);
        let busy = updating(beat("1.0"), "2.0");
        record(&mut f, "wide", host, HostRole::DAEMON, busy, at + 10);
        for other in hosts.into_iter().filter(|h| *h != host) {
            assert_eq!(
                why(&f, &all(), "wide", other, at + 10),
                if f.get_in("wide", &id(other)).expect("known").beat.version == version("2.0") {
                    held(Hold::UpToDate)
                } else {
                    held(Hold::NoSlot)
                },
                "{other} while {host} updates"
            );
        }
        record(&mut f, "wide", host, HostRole::DAEMON, beat("2.0"), at + 20);
    }
    assert_eq!(order, hosts);
}

const RESTART_SERVICES: &str =
    "[[service]]\nname = \"other\"\n[service.roles.daemon]\nmin_up = 0\n";

fn who(host: &str) -> Identity {
    Identity {
        node_id: id(host),
        role: HostRole::DAEMON,
    }
}

/// `top`, also running `other` at `running`, updating it to `to` when given.
fn with_other(mut top: Heartbeat, running: &str, to: Option<&str>) -> Heartbeat {
    top.services = Some(vec![ServiceBeat {
        name: "other".to_owned(),
        version: version(running),
        health: Health::Healthy,
        last_update: None,
        updating: to.map(|t| updating(beat(running), t).updating.expect("updating")),
        step: None,
        commit: None,
        step_secs: Default::default(),
        promotion_pending: false,
        deferred: None,
    }]);
    top
}

fn start(rig: &Rig) -> crate::service::Coordinator {
    let list = config_text(STATIC_ISSUER) + "services = [\"kbf\", \"other\"]\n";
    Rig::start_with_allow_list(&rig.dir, &rig.clock, &list)
        .with_services(Services::parse(RESTART_SERVICES).expect("services"))
}

/// Behaviour: `d1` is ordered `other` and reports updating it, so `d2`, behind on kbf, is
/// held. The coordinator restarts mid-update and forgets the order. After the quiet start `d2`
/// is still held (`no_slot`), because `d1`'s beats in the quiet period said it is updating;
/// once `d1` reports `other` done, `d2` is ordered kbf. Catches a restarted coordinator
/// counting slots per service again, or ordering on what it forgot.
#[tokio::test]
async fn a_restart_mid_update_resumes_with_the_one_update_rule() {
    let mut rig = Rig::new("one-live-update-restart");
    rig.coordinator = start(&rig);
    for body in [
        r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#,
        r#"{"service":"other","version":"3.0","min_up":{"storage":0,"frontend":0}}"#,
    ] {
        rig.set(body).await;
    }
    let kbf_order = ServiceReply::Update {
        service: None,
        version: version("2.0"),
    };
    let beat_at = |rig: &Rig, at: u64, host: &str, b: Heartbeat| {
        rig.at(at);
        rig.coordinator.heartbeat(who(host), b).reply
    };

    let first = beat_at(&rig, 2_000, "d1", with_other(beat("2.0"), "1.0", None));
    assert_eq!(
        first,
        ServiceReply::Update {
            service: Some("other".to_owned()),
            version: version("3.0"),
        }
    );
    let d1_updating = with_other(beat("2.0"), "1.0", Some("3.0"));
    beat_at(&rig, 2_010, "d1", d1_updating.clone());
    let before = beat_at(&rig, 2_010, "d2", beat("1.0"));
    assert_eq!(before, ServiceReply::Nothing, "held before the restart");

    rig.at(2_020);
    rig.coordinator = start(&rig);
    beat_at(&rig, 2_035, "d1", d1_updating.clone());
    let quiet = beat_at(&rig, 2_035, "d2", beat("1.0"));
    assert_eq!(quiet, ServiceReply::Nothing, "the quiet start");

    beat_at(&rig, 2_050, "d1", d1_updating);
    let resumed = beat_at(&rig, 2_050, "d2", beat("1.0"));
    assert_eq!(resumed, ServiceReply::Nothing, "held after the restart");
    let status = rig
        .coordinator
        .status(Some(crate::testutil::OPERATOR))
        .expect("status");
    let hold = status.per_service["kbf"]
        .hosts
        .iter()
        .find(|h| h.host.node_id == "d2")
        .and_then(|h| h.hold);
    assert_eq!(hold, Some(Hold::NoSlot));

    beat_at(&rig, 2_065, "d1", with_other(beat("2.0"), "3.0", None));
    let released = beat_at(&rig, 2_065, "d2", beat("1.0"));
    assert_eq!(released, kbf_order, "ordered once the update is done");
}
