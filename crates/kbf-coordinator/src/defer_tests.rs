//! Deferrals: a host that declined an update for a reason about its moment is left alone for
//! a backoff, holds no slot, does not make its service `blocked`, and is ordered again at
//! the end of the wait. Every time is explicit; nothing sleeps.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::HostRole;
use kbf_updater::wire::{DeferredReport, Heartbeat, Outcome, ServiceReply, Token};

use crate::converge::{State, converge_service};
use crate::decide::{Decision, Hold, decide_why};
use crate::defer::DeferBackoff;
use crate::desired::Desired;
use crate::fleet::Fleet;
use crate::identity::Identity;
use crate::services::{ServiceDef, Services, ServicesError};
use crate::testutil::{Rig, beat, desired, id, last, secs, unhealthy, updating, version};

/// One service on daemons, one host updating at a time, with `extra` lines in its table.
fn text(extra: &str) -> String {
    format!(
        "[[service]]\nname = \"svc\"\n{extra}\n[service.roles.daemon]\nmax_updating = 1\nmin_up = 0\n"
    )
}

/// A heartbeat of a daemon running `running` that says it declined `target`.
fn deferred_beat(running: &str, target: &str, retry_after_secs: u64) -> Heartbeat {
    let mut b = beat(running);
    b.deferred = Some(DeferredReport {
        target: version(target),
        reason: Token::parse("drain_bound").expect("a token"),
        retry_after_secs,
    });
    b
}

/// The coordinator's view of one service, driven by hand: heartbeats in, decisions out.
struct Farm {
    services: Services,
    want: BTreeMap<String, Desired>,
    fleet: Fleet,
}

impl Farm {
    fn new(extra: &str) -> Self {
        Self {
            services: Services::parse(&text(extra)).expect("services"),
            want: BTreeMap::from([("svc".to_owned(), desired("2.0", 0, 0))]),
            fleet: Fleet::new(Duration::ZERO),
        }
    }

    fn desire(&mut self, target: &str) {
        self.want.insert("svc".to_owned(), desired(target, 0, 0));
    }

    fn beat(&mut self, host: &str, b: Heartbeat, at: u64) {
        self.fleet
            .record_in("svc", id(host), HostRole::DAEMON, b, secs(at));
    }

    /// The decision for `host`, without ordering anything.
    fn hold(&self, host: &str, at: u64) -> Decision {
        decide_why(
            &self.services,
            &self.want,
            "svc",
            &self.fleet,
            &id(host),
            secs(at),
        )
    }

    /// The decision for `host`, noting the order as the coordinator does.
    fn ask(&mut self, host: &str, at: u64) -> Decision {
        let decision = self.hold(host, at);
        if let Decision::Update { version } = &decision {
            self.fleet.order("svc", id(host), version.clone(), secs(at));
        }
        decision
    }
}

fn update(target: &str) -> Decision {
    Decision::Update {
        version: version(target),
    }
}

fn held(why: Hold) -> Decision {
    Decision::Hold(why)
}

/// Catches: a deferral that leaves the order standing, so the slot is held for the whole
/// order age (120 s) by a host that is doing nothing: with one slot, the order is ended by
/// the report and another host of the role is ordered at once. The other host is refused
/// for want of a slot until then, and the deferred host holds none.
#[test]
fn a_deferred_report_ends_the_order_and_frees_the_slot() {
    let mut farm = Farm::new("");
    farm.beat("d1", beat("1.0"), 1_000);
    farm.beat("d2", beat("1.0"), 1_000);
    assert_eq!(farm.ask("d1", 1_000), update("2.0"));
    assert_eq!(farm.hold("d2", 1_001), held(Hold::NoSlot));
    farm.beat("d1", deferred_beat("1.0", "2.0", 0), 1_005);
    let host = farm.fleet.get_in("svc", &id("d1")).expect("d1");
    assert!(!farm.fleet.is_ordered("svc", &id("d1"), secs(1_005)));
    assert!(!farm.fleet.holds_slot("svc", &id("d1"), host, secs(1_005)));
    assert_eq!(farm.ask("d2", 1_005), update("2.0"));
    assert_eq!(farm.hold("d1", 1_006), held(Hold::Deferred));
}

/// Catches: a wait that ends early or late, and a repeated report moving it: the host that
/// declined at 1000 is held for the first wait (300 s) however often it repeats the report,
/// is held one second before the end and ordered at it.
#[test]
fn a_deferred_host_is_not_ordered_before_not_before_and_is_at_it() {
    let mut farm = Farm::new("");
    farm.beat("d1", deferred_beat("1.0", "2.0", 0), 1_000);
    farm.beat("d1", deferred_beat("1.0", "2.0", 0), 1_100);
    farm.beat("d1", deferred_beat("1.0", "2.0", 600), 1_200);
    assert_eq!(farm.hold("d1", 1_299), held(Hold::Deferred));
    assert_eq!(farm.hold("d1", 1_300), update("2.0"));
}

/// Catches: a backoff that does not grow, grows without a bound, or counts the episodes of
/// another host: each time the host is ordered after its wait and declines again the wait
/// doubles, 300, 600, 1200, 2400, then 3600 and 3600 again.
#[test]
fn the_backoff_doubles_per_episode_and_stops_at_the_cap() {
    let mut farm = Farm::new("");
    farm.beat("d1", beat("1.0"), 1_000);
    let mut at = 1_000;
    for wait in [300, 600, 1_200, 2_400, 3_600, 3_600] {
        assert_eq!(farm.ask("d1", at), update("2.0"), "ordered at {at}");
        let opened = at + 5;
        farm.beat("d1", deferred_beat("1.0", "2.0", 0), opened);
        assert_eq!(
            farm.hold("d1", opened + wait - 1),
            held(Hold::Deferred),
            "held for {wait}"
        );
        at = opened + wait;
    }
    farm.beat("d2", deferred_beat("1.0", "2.0", 0), at);
    assert_eq!(farm.hold("d2", at + 299), held(Hold::Deferred));
    assert_eq!(farm.hold("d2", at + 300), update("2.0"));
}

/// Catches: the host's own `retry_after_secs` cut by the cap, so the host is ordered before
/// the time it asked for and declines again: only the grown backoff is capped. 900 s wins
/// over 300, 3601 and 100000 s are waited in full, and 10 s does not hurry the host.
#[test]
fn a_retry_after_over_the_cap_is_honoured() {
    for (retry_after, wait) in [(900, 900), (3_601, 3_601), (100_000, 100_000), (10, 300)] {
        let mut farm = Farm::new("");
        farm.beat("d1", deferred_beat("1.0", "2.0", retry_after), 1_000);
        assert_eq!(
            farm.hold("d1", 1_000 + wait - 1),
            held(Hold::Deferred),
            "{retry_after}"
        );
        assert_eq!(
            farm.hold("d1", 1_000 + wait),
            update("2.0"),
            "{retry_after}"
        );
    }
}

/// Catches: a deferral of one version holding back another: a report for a version that is
/// not the desired one holds nothing and does not end an order for the desired one; a
/// deferral of the desired version is lifted by setting another version, and a report for
/// the new version starts again at the first wait.
#[test]
fn a_deferral_for_another_target_is_ignored_and_a_new_version_lifts_it() {
    let mut farm = Farm::new("");
    farm.beat("d1", beat("1.0"), 1_000);
    farm.beat("d2", beat("1.0"), 1_000);
    assert_eq!(farm.ask("d1", 1_000), update("2.0"));
    farm.beat("d1", deferred_beat("1.0", "1.5", 0), 1_005);
    assert_eq!(farm.hold("d2", 1_006), held(Hold::NoSlot));
    farm.beat("d2", deferred_beat("1.0", "1.5", 0), 1_006);
    assert_eq!(farm.hold("d2", 1_007), held(Hold::NoSlot));
    farm.beat("d1", beat("2.0"), 1_010);
    farm.beat("d2", deferred_beat("1.0", "2.0", 0), 1_020);
    assert_eq!(farm.hold("d2", 1_021), held(Hold::Deferred));
    farm.desire("3.0");
    assert_eq!(farm.hold("d2", 1_021), update("3.0"));
    farm.beat("d2", deferred_beat("1.0", "3.0", 0), 1_030);
    assert_eq!(farm.hold("d2", 1_329), held(Hold::Deferred));
    assert_eq!(farm.hold("d2", 1_330), update("3.0"));
}

/// Catches: a host that went on to update still counted in the episodes of its earlier
/// deferral: once it reports that it is updating, the next deferral of the same version is
/// the first again.
#[test]
fn an_update_that_starts_ends_the_episodes() {
    let mut farm = Farm::new("");
    farm.beat("d1", deferred_beat("1.0", "2.0", 0), 1_000);
    assert_eq!(farm.ask("d1", 1_300), update("2.0"));
    farm.beat("d1", deferred_beat("1.0", "2.0", 0), 1_305);
    farm.beat("d1", updating(beat("1.0"), "2.0"), 1_900);
    farm.beat("d1", deferred_beat("1.0", "2.0", 0), 2_000);
    assert_eq!(farm.hold("d1", 2_299), held(Hold::Deferred));
    assert_eq!(farm.hold("d1", 2_300), update("2.0"));
}

/// Catches: the wait read from a constant and not the definition: a service that sets
/// `defer_backoff` gets its own initial, factor and cap, 10, 30, 90, then 100 s.
#[test]
fn the_definitions_backoff_is_the_one_used() {
    let mut farm = Farm::new("defer_backoff = { initial_secs = 10, factor = 3, max_secs = 100 }");
    farm.beat("d1", beat("1.0"), 1_000);
    let mut at = 1_000;
    for wait in [10, 30, 90, 100] {
        assert_eq!(farm.ask("d1", at), update("2.0"));
        farm.beat("d1", deferred_beat("1.0", "2.0", 0), at + 1);
        assert_eq!(farm.hold("d1", at + wait), held(Hold::Deferred));
        at += wait + 1;
    }
}

/// Catches: a deferral of one service holding the host's other services, or one host's
/// deferral holding another host: the deferral is kept per host and service.
#[test]
fn a_deferral_holds_its_own_host_and_service_only() {
    let mut farm = Farm::new("");
    farm.beat("d1", deferred_beat("1.0", "2.0", 0), 1_000);
    farm.beat("d2", beat("1.0"), 1_000);
    assert_eq!(farm.hold("d2", 1_001), update("2.0"));
    let other = Services::parse(&format!(
        "{}[[service]]\nname = \"other\"\n[service.roles.daemon]\nmax_updating = 1\nmin_up = 0\n",
        text("")
    ))
    .expect("services");
    let want = BTreeMap::from([("other".to_owned(), desired("2.0", 0, 0))]);
    farm.fleet.record_in(
        "other",
        id("d1"),
        HostRole::DAEMON,
        beat("1.0"),
        secs(1_000),
    );
    assert_eq!(
        decide_why(&other, &want, "other", &farm.fleet, &id("d1"), secs(1_001)),
        update("2.0")
    );
}

/// Catches: a deferral read as a failure: the deferred host is in progress and the service
/// is not blocked, the host holds no slot and shows no failed update.
#[test]
fn a_deferred_service_is_in_progress_not_blocked() {
    let mut farm = Farm::new("");
    farm.beat("d1", deferred_beat("1.0", "2.0", 0), 1_000);
    farm.beat("d2", beat("2.0"), 1_000);
    let def = farm.services.get("svc").expect("svc").clone();
    let want = farm.want.get("svc").cloned();
    let now = secs(1_001);
    let convergence = converge_service(&def, want.as_ref(), &farm.fleet, now);
    assert_eq!(convergence.state, State::InProgress);
    assert_eq!(convergence.roles[0].failed, 0);
    let host = farm.fleet.get_in("svc", &id("d1")).expect("d1");
    assert!(!host.holds_slot());
    assert!(farm.fleet.is_up("svc", &id("d1"), host, now));
}

/// Catches: the regression the deferral work could cause in the old path: an old agent has
/// no way to say deferred, so its declined update is a failed one, and that still holds the
/// slot, holds the order back and blocks the service.
#[test]
fn an_old_agents_failed_report_still_blocks() {
    let mut farm = Farm::new("");
    let failed = unhealthy(last(beat("1.0"), "2.0", Outcome::Failed));
    farm.beat("d1", failed, 1_000);
    assert_eq!(farm.hold("d1", 1_001), held(Hold::FailedHere));
    let host = farm.fleet.get_in("svc", &id("d1")).expect("d1");
    assert!(host.holds_slot());
    let def = farm.services.get("svc").expect("svc").clone();
    let want = farm.want.get("svc").cloned();
    let convergence = converge_service(&def, want.as_ref(), &farm.fleet, secs(1_001));
    assert_eq!(convergence.state, State::Blocked);
}

/// Catches: the backoff kept somewhere a restart does not clear, or the count restarted
/// wrongly: after two episodes the wait is 600 s; a coordinator that starts again knows
/// nothing, and the host's first report to it is the first episode (300 s).
#[tokio::test]
async fn a_restart_forgets_the_backoff() {
    let mut rig = Rig::new("defer-restart");
    rig.set(r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    let who = Identity {
        node_id: id("d1"),
        role: HostRole::DAEMON,
    };
    let say = |rig: &Rig, at: u64, b: Heartbeat| {
        rig.at(at);
        rig.coordinator.heartbeat(who.clone(), b).reply
    };
    let update = ServiceReply::Update {
        service: None,
        version: version("2.0"),
    };
    assert_eq!(say(&rig, 2_000, beat("1.0")), update);
    assert_eq!(
        say(&rig, 2_010, deferred_beat("1.0", "2.0", 0)),
        ServiceReply::Nothing
    );
    assert_eq!(say(&rig, 2_310, beat("1.0")), update);
    assert_eq!(
        say(&rig, 2_315, deferred_beat("1.0", "2.0", 0)),
        ServiceReply::Nothing
    );
    assert_eq!(
        say(&rig, 2_914, deferred_beat("1.0", "2.0", 0)),
        ServiceReply::Nothing
    );
    rig.at(3_000);
    rig.restart();
    assert_eq!(
        say(&rig, 3_100, deferred_beat("1.0", "2.0", 0)),
        ServiceReply::Nothing
    );
    assert_eq!(
        say(&rig, 3_399, deferred_beat("1.0", "2.0", 0)),
        ServiceReply::Nothing
    );
    assert_eq!(say(&rig, 3_400, deferred_beat("1.0", "2.0", 0)), update);
}

/// Catches: a services file that cannot set the backoff, or one that takes a value that
/// breaks it: the table is optional (the default is 300 s, doubling, capped at 3600 s), each
/// key is optional, and a zero or a factor over 16, a cap under the initial wait or over a
/// week, an unknown key and a wrong type are refused with the field named.
#[test]
fn the_backoff_is_a_definition_field_with_checked_values() {
    let parse = |extra: &str| Services::parse(&text(extra));
    let backoff = |extra: &str| {
        parse(extra)
            .expect("services")
            .get("svc")
            .expect("svc")
            .defer_backoff
    };
    let shipped = DeferBackoff {
        initial_secs: 300,
        factor: 2,
        max_secs: 3600,
    };
    assert_eq!(backoff(""), shipped);
    assert_eq!(DeferBackoff::default(), shipped);
    assert_eq!(ServiceDef::kbf().defer_backoff, shipped);
    assert_eq!(
        backoff("defer_backoff = { initial_secs = 60, factor = 4, max_secs = 7200 }"),
        DeferBackoff {
            initial_secs: 60,
            factor: 4,
            max_secs: 7200
        }
    );
    assert_eq!(
        backoff("defer_backoff = { initial_secs = 10 }"),
        DeferBackoff {
            initial_secs: 10,
            ..shipped
        }
    );
    assert_eq!(
        backoff("defer_backoff = { factor = 1, max_secs = 604800 }"),
        DeferBackoff {
            factor: 1,
            max_secs: 604_800,
            ..shipped
        }
    );
    for bad in [
        "defer_backoff = { initial_secs = 0 }",
        "defer_backoff = { factor = 0 }",
        "defer_backoff = { factor = 17 }",
        "defer_backoff = { max_secs = 299 }",
        "defer_backoff = { max_secs = 604801 }",
    ] {
        let refused = parse(bad).expect_err("refused");
        assert!(matches!(refused, ServicesError::Invalid(_)), "{bad}");
        assert!(refused.to_string().contains("svc: defer_backoff"), "{bad}");
    }
    for bad in [
        "defer_backoff = { cap = 5 }",
        "defer_backoff = 300",
        "defer_backoff = { factor = -2 }",
    ] {
        assert!(
            matches!(parse(bad).expect_err("refused"), ServicesError::Parse(_)),
            "{bad}"
        );
    }
    let kbf = "[[service]]\nname = \"kbf\"\ndefer_backoff = { initial_secs = 5 }\n[service.roles.daemon]\n";
    let replaced = Services::parse(kbf).expect("services");
    assert_eq!(
        replaced.get("kbf").expect("kbf").defer_backoff.initial_secs,
        5
    );
}

/// Catches: arithmetic that overflows or ignores the cap: the wait is the larger of the
/// capped backoff and the report's number, so only the backoff is bounded by the cap, even
/// for the largest episode count, factor and report.
#[test]
fn the_wait_is_bounded_whatever_the_inputs() {
    let b = DeferBackoff::default();
    let waits = |episodes, retry| b.wait(episodes, retry).as_secs();
    assert_eq!(waits(1, 0), 300);
    assert_eq!(waits(2, 0), 600);
    assert_eq!(waits(5, 0), 3_600);
    assert_eq!(waits(u32::MAX, 0), 3_600);
    assert_eq!(waits(1, 4_000), 4_000);
    assert_eq!(waits(u32::MAX, 10), 3_600);
    assert_eq!(waits(1, u64::MAX), u64::MAX);
    let wide = DeferBackoff {
        initial_secs: u64::MAX,
        factor: 16,
        max_secs: 604_800,
    };
    assert_eq!(wide.wait(u32::MAX, 0).as_secs(), 604_800);
    assert_eq!(wide.wait(u32::MAX, u64::MAX).as_secs(), u64::MAX);
    let flat = DeferBackoff {
        initial_secs: 7,
        factor: 1,
        max_secs: 100,
    };
    assert_eq!(flat.wait(50, 0).as_secs(), 7);
}

/// Catches: a report for a version other than the order's ending that order, or holding the
/// host for the desired version: the order stays (its slot is still held), and the host is
/// not held for the desired version.
#[test]
fn a_deferred_report_for_another_target_than_the_order_leaves_the_order() {
    let mut farm = Farm::new("");
    farm.beat("d1", beat("1.0"), 1_000);
    farm.beat("d2", beat("1.0"), 1_000);
    assert_eq!(farm.ask("d1", 1_000), update("2.0"));
    farm.beat("d1", deferred_beat("1.0", "1.5", 0), 1_005);
    assert!(farm.fleet.is_ordered("svc", &id("d1"), secs(1_006)));
    assert_eq!(farm.hold("d2", 1_006), held(Hold::NoSlot));
    farm.beat("d1", beat("1.0"), 1_130);
    assert_eq!(farm.hold("d1", 1_131), update("2.0"));
}
