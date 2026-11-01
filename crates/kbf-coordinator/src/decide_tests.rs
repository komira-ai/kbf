//! One test per rule of the decision, each with the hosts that make the rule matter.

use std::time::Duration;

use kbf_types::HostRole;
use kbf_updater::wire::{Outcome, Reply};

use crate::decide::decide;
use crate::desired::Desired;
use crate::fleet::Fleet;
use crate::testutil::{beat, desired, fleet, id, last, secs, unhealthy, updating, version};

const NOW: Duration = Duration::from_secs(1_000);

fn update(target: &str) -> Reply {
    Reply::Update {
        version: version(target),
    }
}

fn ask(d: &Desired, f: &Fleet, host: &str) -> Reply {
    decide(d, f, &id(host), NOW)
}

/// Catches: a host that runs an older version than the desired one not being told to update,
/// and a reply that carries anything but the desired version.
#[test]
fn a_host_behind_the_desired_version_is_told_to_update() {
    let f = fleet(NOW, &[("d1", HostRole::DAEMON, beat("1.0"))]);
    assert_eq!(ask(&desired("2.0", 1, 1), &f, "d1"), update("2.0"));
}

/// Catches: a host that already runs the desired version being told to update.
#[test]
fn a_host_at_the_desired_version_is_told_nothing() {
    let f = fleet(NOW, &[("d1", HostRole::DAEMON, beat("2.0"))]);
    assert_eq!(ask(&desired("2.0", 1, 1), &f, "d1"), Reply::Nothing);
}

/// Catches: a host that is in the middle of an update being told to update again.
#[test]
fn a_host_that_is_updating_is_told_nothing() {
    let f = fleet(
        NOW,
        &[("d1", HostRole::DAEMON, updating(beat("1.0"), "2.0"))],
    );
    assert_eq!(ask(&desired("2.0", 1, 1), &f, "d1"), Reply::Nothing);
}

/// Catches: a host the coordinator has never heard from being told anything.
#[test]
fn a_host_that_has_not_heartbeated_is_told_nothing() {
    let f = fleet(NOW, &[("d1", HostRole::DAEMON, beat("1.0"))]);
    assert_eq!(ask(&desired("2.0", 1, 1), &f, "d2"), Reply::Nothing);
}

/// Catches: a role's limit not counting the hosts that are updating, so a third daemon
/// starts while two are updating; and a slot not freeing when an update ends.
#[test]
fn a_role_at_its_limit_waits_for_a_slot() {
    let busy = |d2: kbf_updater::wire::Heartbeat| {
        fleet(
            NOW,
            &[
                ("d1", HostRole::DAEMON, updating(beat("1.0"), "2.0")),
                ("d2", HostRole::DAEMON, d2),
                ("d3", HostRole::DAEMON, beat("1.0")),
            ],
        )
    };
    let d = desired("2.0", 1, 1);
    assert_eq!(
        ask(&d, &busy(updating(beat("1.0"), "2.0")), "d3"),
        Reply::Nothing,
        "two daemons updating is the default limit"
    );
    assert_eq!(ask(&d, &busy(beat("2.0")), "d3"), update("2.0"));
}

/// Catches: the limits taken from the wrong role (a storage host updating holding a
/// daemon's slot), the defaults changed (1, 1, 2, 2), and the limit not following the
/// desired state.
#[test]
fn the_limit_is_per_role_and_follows_the_desired_state() {
    let f = fleet(
        NOW,
        &[
            ("s1", HostRole::STORAGE, updating(beat("1.0"), "2.0")),
            ("s2", HostRole::STORAGE, beat("1.0")),
            ("s3", HostRole::STORAGE, beat("1.0")),
            ("d1", HostRole::DAEMON, beat("1.0")),
            ("m1", HostRole::MAC_DAEMON, updating(beat("1.0"), "2.0")),
            ("m2", HostRole::MAC_DAEMON, beat("1.0")),
            ("m3", HostRole::MAC_DAEMON, updating(beat("1.0"), "2.0")),
        ],
    );
    let mut d = desired("2.0", 1, 1);
    assert_eq!(
        ask(&d, &f, "d1"),
        update("2.0"),
        "storage's slot is not a daemon's"
    );
    assert_eq!(
        ask(&d, &f, "s2"),
        Reply::Nothing,
        "one storage host at a time"
    );
    assert_eq!(ask(&d, &f, "m2"), Reply::Nothing, "two Macs at a time");
    d.max_updating.set("storage", 2);
    d.max_updating.set("mac-daemon", 3);
    assert_eq!(ask(&d, &f, "s2"), update("2.0"));
    assert_eq!(ask(&d, &f, "m2"), update("2.0"));
    d.max_updating.set("mac-daemon", 0);
    assert_eq!(
        ask(&d, &f, "m2"),
        Reply::Nothing,
        "a limit of zero holds the role"
    );
}

/// Catches: the last frontend that is up being told to update: with the other frontend
/// silent, unhealthy or updating the asking one is the last, and with it up it is not; and
/// the minimum of frontends being allowed to fall below one.
#[test]
fn the_last_frontend_that_is_up_is_never_told_to_update() {
    let d = desired("2.0", 1, 0);
    let with = |other: kbf_updater::wire::Heartbeat, seen| {
        let mut f = fleet(NOW, &[("f1", HostRole::FRONTEND, beat("1.0"))]);
        f.record(id("f2"), HostRole::FRONTEND, other, seen);
        f
    };
    assert_eq!(ask(&d, &with(beat("1.0"), NOW), "f1"), update("2.0"));
    assert_eq!(
        ask(&d, &with(beat("1.0"), secs(900)), "f1"),
        Reply::Nothing,
        "f2 silent"
    );
    assert_eq!(
        ask(&d, &with(unhealthy(beat("1.0")), NOW), "f1"),
        Reply::Nothing
    );
    let busy = updating(beat("1.0"), "2.0");
    assert_eq!(
        ask(&d, &with(busy, NOW), "f1"),
        Reply::Nothing,
        "f2 is down while updating"
    );
    let alone = fleet(NOW, &[("f1", HostRole::FRONTEND, beat("1.0"))]);
    assert_eq!(ask(&d, &alone, "f1"), Reply::Nothing, "the only frontend");
}

/// Catches: the frontend minimum not checked against the frontends that are up: with a
/// minimum of two, one of three may update while two stay up, and none of two may.
#[test]
fn a_frontend_updates_only_while_the_minimum_stays_up() {
    let d = desired("2.0", 1, 2);
    let three = fleet(
        NOW,
        &[
            ("f1", HostRole::FRONTEND, beat("1.0")),
            ("f2", HostRole::FRONTEND, beat("1.0")),
            ("f3", HostRole::FRONTEND, beat("1.0")),
        ],
    );
    assert_eq!(ask(&d, &three, "f1"), update("2.0"));
    let two_up = fleet(
        NOW,
        &[
            ("f1", HostRole::FRONTEND, beat("1.0")),
            ("f2", HostRole::FRONTEND, beat("1.0")),
            ("f3", HostRole::FRONTEND, unhealthy(beat("1.0"))),
        ],
    );
    assert_eq!(ask(&d, &two_up, "f1"), Reply::Nothing);
}

/// Catches: the storage minimum not checked, or checked against hosts that are not up:
/// with a minimum of two, a storage host updates only while two others are up, and a host
/// that is unhealthy or updating is not up.
#[test]
fn a_storage_host_updates_only_while_the_minimum_stays_up() {
    let mut d = desired("2.0", 2, 1);
    d.max_updating.set("storage", 3);
    let with = |third: kbf_updater::wire::Heartbeat| {
        fleet(
            NOW,
            &[
                ("s1", HostRole::STORAGE, beat("1.0")),
                ("s2", HostRole::STORAGE, beat("1.0")),
                ("s3", HostRole::STORAGE, third),
            ],
        )
    };
    assert_eq!(ask(&d, &with(beat("1.0")), "s1"), update("2.0"));
    assert_eq!(ask(&d, &with(unhealthy(beat("1.0"))), "s1"), Reply::Nothing);
    assert_eq!(
        ask(&d, &with(updating(beat("1.0"), "2.0")), "s1"),
        Reply::Nothing
    );
}

/// Catches: the minimum counted against the hosts the coordinator knows instead of the
/// heartbeats it has: after a restart a storage host that has not heartbeated yet is not
/// known at all, and a known one that has gone quiet is down; both leave the minimum unmet.
#[test]
fn the_minimum_counts_live_heartbeats_not_known_hosts() {
    let mut d = desired("2.0", 3, 1);
    d.max_updating.set("storage", 3);
    let two_heard = fleet(
        NOW,
        &[
            ("s1", HostRole::STORAGE, beat("1.0")),
            ("s2", HostRole::STORAGE, beat("1.0")),
        ],
    );
    assert_eq!(
        ask(&d, &two_heard, "s1"),
        Reply::Nothing,
        "the third is missing"
    );
    let mut quiet_third = two_heard.clone();
    quiet_third.record(id("s3"), HostRole::STORAGE, beat("1.0"), secs(900));
    assert_eq!(
        ask(&d, &quiet_third, "s1"),
        Reply::Nothing,
        "the third went quiet"
    );
    let mut back = two_heard;
    back.record(id("s3"), HostRole::STORAGE, beat("1.0"), NOW);
    back.record(id("s4"), HostRole::STORAGE, beat("1.0"), NOW);
    assert_eq!(ask(&d, &back, "s1"), update("2.0"), "three others are up");
}

/// Catches: a host counted as down too early or too late: 44 seconds of silence is still
/// live, 45 is down.
#[test]
fn forty_five_seconds_without_a_heartbeat_means_down() {
    let d = desired("2.0", 1, 1);
    let with = |seen: Duration| {
        fleet(NOW, &[("s1", HostRole::STORAGE, beat("1.0"))])
            .tap(|f| f.record(id("s2"), HostRole::STORAGE, beat("1.0"), seen))
    };
    assert_eq!(ask(&d, &with(NOW - secs(44)), "s1"), update("2.0"));
    assert_eq!(ask(&d, &with(NOW - secs(45)), "s1"), Reply::Nothing);
}

trait Tap: Sized {
    fn tap(self, f: impl FnOnce(&mut Self)) -> Self;
}

impl Tap for Fleet {
    fn tap(mut self, f: impl FnOnce(&mut Self)) -> Self {
        f(&mut self);
        self
    }
}

/// Catches: an order given while the coordinator is still within 30 seconds of starting,
/// and one held back after them.
#[test]
fn no_order_is_given_within_thirty_seconds_of_starting() {
    let d = desired("2.0", 1, 1);
    let mut f = Fleet::new(secs(1_000));
    f.record(id("d1"), HostRole::DAEMON, beat("1.0"), secs(1_029));
    assert_eq!(decide(&d, &f, &id("d1"), secs(1_000)), Reply::Nothing);
    assert_eq!(decide(&d, &f, &id("d1"), secs(1_029)), Reply::Nothing);
    assert_eq!(decide(&d, &f, &id("d1"), secs(1_030)), update("2.0"));
}

/// Catches: a frontend updating while a storage, daemon or Mac host that is up still runs
/// another version or is updating, and storage and daemons not going first; a host that is
/// down (it can never finish) not being waited for.
#[test]
fn storage_and_daemons_go_before_frontends() {
    let d = desired("2.0", 0, 1);
    let with = |other: (&str, kbf_types::HostRole, kbf_updater::wire::Heartbeat), seen| {
        let mut f = fleet(
            NOW,
            &[
                ("f1", HostRole::FRONTEND, beat("1.0")),
                ("f2", HostRole::FRONTEND, beat("1.0")),
            ],
        );
        f.record(id(other.0), other.1, other.2, seen);
        f
    };
    for role in [HostRole::STORAGE, HostRole::DAEMON, HostRole::MAC_DAEMON] {
        assert_eq!(
            ask(&d, &with(("x1", role.clone(), beat("1.0")), NOW), "f1"),
            Reply::Nothing,
            "{role} behind"
        );
        let busy = updating(beat("1.0"), "2.0");
        assert_eq!(
            ask(&d, &with(("x1", role.clone(), busy), NOW), "f1"),
            Reply::Nothing,
            "{role} updating"
        );
        assert_eq!(
            ask(&d, &with(("x1", role.clone(), beat("2.0")), NOW), "f1"),
            update("2.0")
        );
        assert_eq!(
            ask(
                &d,
                &with(("x1", role.clone(), beat("1.0")), secs(900)),
                "f1"
            ),
            update("2.0"),
            "{role} down"
        );
        let f = with(("x1", role.clone(), beat("1.0")), NOW);
        assert_eq!(
            ask(&d, &f, "x1"),
            update("2.0"),
            "{role} goes while frontends wait"
        );
    }
}

/// Catches: a failed host giving its slot back before it heartbeats healthy (a bad version
/// would then roll on through the role), and keeping it after.
#[test]
fn a_failed_host_keeps_its_slot_until_it_heartbeats_healthy() {
    let mut d = desired("2.0", 1, 1);
    d.max_updating.set("daemon", 1);
    let failed = last(beat("1.0"), "2.0", Outcome::Failed);
    let f = |d1: kbf_updater::wire::Heartbeat| {
        fleet(
            NOW,
            &[
                ("d1", HostRole::DAEMON, d1),
                ("d2", HostRole::DAEMON, beat("1.0")),
            ],
        )
    };
    assert_eq!(ask(&d, &f(unhealthy(failed.clone())), "d2"), Reply::Nothing);
    assert_eq!(ask(&d, &f(failed), "d2"), update("2.0"));
}

/// Catches: a host that failed the desired version being told it again as soon as it is
/// healthy (a retry loop on a bad version), and the block outliving a new desired version;
/// a host that failed another version being held back by its own old failure.
#[test]
fn a_host_that_failed_a_version_is_not_told_it_again() {
    let failed = last(beat("1.0"), "2.0", Outcome::Failed);
    let f = fleet(NOW, &[("d1", HostRole::DAEMON, failed.clone())]);
    assert_eq!(ask(&desired("2.0", 1, 1), &f, "d1"), Reply::Nothing);
    assert_eq!(ask(&desired("3.0", 1, 1), &f, "d1"), update("3.0"));
    let own = fleet(NOW, &[("d1", HostRole::DAEMON, unhealthy(failed))]);
    assert_eq!(
        ask(&desired("3.0", 1, 1), &own, "d1"),
        update("3.0"),
        "its own slot"
    );
    let succeeded = last(beat("1.0"), "2.0", Outcome::Succeeded);
    let ok = fleet(NOW, &[("d1", HostRole::DAEMON, succeeded)]);
    assert_eq!(ask(&desired("2.0", 1, 1), &ok, "d1"), update("2.0"));
}

/// Catches: a rollback (the desired version older than the one the hosts run) not being
/// ordered, or ordered under other rules than an update: the limit, the minimum and the
/// order of roles all hold.
#[test]
fn a_rollback_is_an_older_desired_version_under_the_same_rules() {
    let d = desired("1.0", 2, 1);
    let f = fleet(
        NOW,
        &[
            ("s1", HostRole::STORAGE, beat("2.0")),
            ("s2", HostRole::STORAGE, beat("2.0")),
            ("s3", HostRole::STORAGE, beat("2.0")),
            ("f1", HostRole::FRONTEND, beat("2.0")),
            ("f2", HostRole::FRONTEND, beat("2.0")),
        ],
    );
    assert_eq!(ask(&d, &f, "s1"), update("1.0"));
    assert_eq!(
        ask(&d, &f, "f1"),
        Reply::Nothing,
        "storage rolls back first"
    );
    let busy = fleet(
        NOW,
        &[
            ("s1", HostRole::STORAGE, beat("2.0")),
            ("s2", HostRole::STORAGE, updating(beat("2.0"), "1.0")),
            ("s3", HostRole::STORAGE, beat("2.0")),
        ],
    );
    assert_eq!(
        ask(&d, &busy, "s1"),
        Reply::Nothing,
        "one storage host at a time"
    );
    let thin = fleet(
        NOW,
        &[
            ("s1", HostRole::STORAGE, beat("2.0")),
            ("s2", HostRole::STORAGE, beat("2.0")),
        ],
    );
    assert_eq!(ask(&d, &thin, "s1"), Reply::Nothing, "the minimum holds");
}

/// Catches: a host whose role the code does not name being ordered to update: it is
/// recorded but gets nothing, however far behind it is.
#[test]
fn a_role_the_code_does_not_name_is_ordered_nothing() {
    let cache: HostRole = "cache".parse().expect("well formed");
    let f = fleet(NOW, &[("c1", cache, beat("1.0"))]);
    assert_eq!(ask(&desired("2.0", 1, 1), &f, "c1"), Reply::Nothing);
}

/// Catches: a definition that names a role the code has no constant for being refused the
/// slots of the four: a role the definition names is ordered by the default limits (one at
/// a time, none required up), and a host whose role it does not name is ordered nothing.
#[test]
fn a_definition_that_names_an_unnamed_role_orders_it_by_the_default() {
    use crate::decide::decide_in;
    use crate::desired::KBF;
    use crate::services::{RoleDef, ServiceDef, Services};

    let cache: HostRole = "cache".parse().expect("well formed");
    let mut def = ServiceDef::kbf();
    def.roles.insert(cache.name().to_owned(), RoleDef::plain());
    let services = Services::default().with(def);
    let d = desired("2.0", 1, 1);
    let wants = std::collections::BTreeMap::from([(KBF.to_owned(), d.clone())]);
    let f = fleet(NOW, &[("c1", cache, beat("1.0"))]);
    assert_eq!(
        decide_in(&services, &wants, KBF, &f, &id("c1"), NOW),
        update("2.0")
    );
    let other: HostRole = "queue".parse().expect("well formed");
    let f = fleet(NOW, &[("q1", other, beat("1.0"))]);
    assert_eq!(
        decide_in(&services, &wants, KBF, &f, &id("q1"), NOW),
        Reply::Nothing
    );
    let behind = fleet(NOW, &[("d1", HostRole::DAEMON, beat("1.0"))]);
    assert_eq!(
        decide_in(&services, &wants, KBF, &behind, &id("d1"), NOW),
        update("2.0")
    );
}
