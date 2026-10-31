//! Goldens of today's `decide`: for fixed fleets, desired states and times, the reply every
//! host gets, written out as literals. `decide_tests.rs` has one test per rule; these pin the
//! whole decision, so a later change that generalises the coordinator (`docs/design/host-updates.md`,
//! section 8, step 1) cannot move what `kbf` is told without moving a literal here, in the
//! same change, where a reviewer sees it.
//!
//! Each scenario is the coordinator's own loop: hosts are asked in the order listed, and an
//! `update` is noted with `Fleet::order` before the next host is asked, as `service.rs` does
//! under its lock. A scenario that asks without ordering says so. There is no canary in
//! today's decision (no host goes first for being a canary), so there is nothing to pin for
//! one; it arrives with the generalisation, which must leave these literals unchanged.

use std::time::Duration;

use kbf_types::HostRole;
use kbf_updater::wire::{Heartbeat, Outcome, Reply};

use crate::decide::decide;
use crate::desired::{Desired, KBF};
use crate::fleet::Fleet;
use crate::testutil::{beat, desired, fleet, id, last, secs, unhealthy, updating};

const NOW: Duration = Duration::from_secs(1_000);

fn say(reply: &Reply) -> String {
    match reply {
        Reply::Nothing => "nothing".to_owned(),
        Reply::Update { version } => format!("update {version}"),
    }
}

/// One line per host: the reply at `at`. With `order`, an update is noted as the coordinator
/// notes it before the next host is asked.
fn run(f: &mut Fleet, d: &Desired, hosts: &[&str], at: Duration, order: bool) -> Vec<String> {
    hosts
        .iter()
        .map(|host| {
            let reply = decide(d, f, &id(host), at);
            if let (true, Reply::Update { version }) = (order, &reply) {
                f.order(KBF, id(host), version.clone(), at);
            }
            format!("{host}: {}", say(&reply))
        })
        .collect()
}

fn all_roles(version: &str) -> Fleet {
    fleet(
        NOW,
        &[
            ("s1", HostRole::STORAGE, beat(version)),
            ("s2", HostRole::STORAGE, beat(version)),
            ("s3", HostRole::STORAGE, beat(version)),
            ("d1", HostRole::DAEMON, beat(version)),
            ("d2", HostRole::DAEMON, beat(version)),
            ("d3", HostRole::DAEMON, beat(version)),
            ("m1", HostRole::MAC_DAEMON, beat(version)),
            ("m2", HostRole::MAC_DAEMON, beat(version)),
            ("m3", HostRole::MAC_DAEMON, beat(version)),
            ("f1", HostRole::FRONTEND, beat(version)),
            ("f2", HostRole::FRONTEND, beat(version)),
        ],
    )
}

const ALL: [&str; 11] = [
    "s1", "s2", "s3", "d1", "d2", "d3", "m1", "m2", "m3", "f1", "f2",
];

/// The first pass of a roll with the default limits (1 storage, 2 daemons, 2 Macs, 1 frontend):
/// each role fills its slots and no more, and no frontend starts while any other host is behind.
#[test]
fn the_first_pass_of_a_roll_with_the_default_limits() {
    let mut f = all_roles("1.0");
    assert_eq!(
        run(&mut f, &desired("2.0", 1, 1), &ALL, NOW, true),
        [
            "s1: update 2.0",
            "s2: nothing",
            "s3: nothing",
            "d1: update 2.0",
            "d2: update 2.0",
            "d3: nothing",
            "m1: update 2.0",
            "m2: update 2.0",
            "m3: nothing",
            "f1: nothing",
            "f2: nothing",
        ]
    );
}

/// The limits the desired state sets replace the defaults per role; zero holds a role.
#[test]
fn the_limits_of_the_desired_state_replace_the_defaults_per_role() {
    let mut f = all_roles("1.0");
    let mut d = desired("2.0", 1, 1);
    d.max_updating.set("storage", 2);
    d.max_updating.set("daemon", 1);
    d.max_updating.set("mac-daemon", 0);
    assert_eq!(
        run(&mut f, &d, &ALL, NOW, true),
        [
            "s1: update 2.0",
            "s2: update 2.0",
            "s3: nothing",
            "d1: update 2.0",
            "d2: nothing",
            "d3: nothing",
            "m1: nothing",
            "m2: nothing",
            "m3: nothing",
            "f1: nothing",
            "f2: nothing",
        ]
    );
}

/// A roll in progress, asked without ordering: a host that is updating or at the version is
/// told nothing, a host in a full role waits, a daemon with a free slot goes, and the frontends
/// wait while a storage host is still updating.
#[test]
fn a_roll_in_progress() {
    let mut f = fleet(
        NOW,
        &[
            ("s1", HostRole::STORAGE, updating(beat("1.0"), "2.0")),
            ("s2", HostRole::STORAGE, beat("2.0")),
            ("s3", HostRole::STORAGE, beat("1.0")),
            ("d1", HostRole::DAEMON, beat("2.0")),
            ("d2", HostRole::DAEMON, updating(beat("1.0"), "2.0")),
            ("d3", HostRole::DAEMON, beat("1.0")),
            ("m1", HostRole::MAC_DAEMON, beat("2.0")),
            ("f1", HostRole::FRONTEND, beat("1.0")),
            ("f2", HostRole::FRONTEND, beat("1.0")),
        ],
    );
    let hosts = ["s1", "s2", "s3", "d1", "d2", "d3", "m1", "f1", "f2"];
    assert_eq!(
        run(&mut f, &desired("2.0", 1, 1), &hosts, NOW, false),
        [
            "s1: nothing",
            "s2: nothing",
            "s3: nothing",
            "d1: nothing",
            "d2: nothing",
            "d3: update 2.0",
            "m1: nothing",
            "f1: nothing",
            "f2: nothing",
        ]
    );
}

/// Every other role at the version: the frontends go one at a time, and the second waits for
/// the first's slot.
#[test]
fn frontends_go_last_and_one_at_a_time() {
    let mut f = fleet(
        NOW,
        &[
            ("s1", HostRole::STORAGE, beat("2.0")),
            ("d1", HostRole::DAEMON, beat("2.0")),
            ("m1", HostRole::MAC_DAEMON, beat("2.0")),
            ("f1", HostRole::FRONTEND, beat("1.0")),
            ("f2", HostRole::FRONTEND, beat("1.0")),
            ("f3", HostRole::FRONTEND, beat("1.0")),
        ],
    );
    assert_eq!(
        run(
            &mut f,
            &desired("2.0", 1, 1),
            &["f1", "f2", "f3"],
            NOW,
            true
        ),
        ["f1: update 2.0", "f2: nothing", "f3: nothing"]
    );
}

/// The last frontend that is up is never told to update, even with a minimum of zero; the
/// storage minimum counts the others that are up.
#[test]
fn the_minimums_hold_whatever_the_limits_say() {
    // f2 has been silent for 900 s: f1 is the last frontend up.
    let mut f = fleet(NOW, &[("f1", HostRole::FRONTEND, beat("1.0"))]);
    f.record(id("f2"), HostRole::FRONTEND, beat("1.0"), secs(100));
    assert_eq!(
        run(&mut f, &desired("2.0", 0, 0), &["f1"], NOW, false),
        ["f1: nothing"]
    );
    // Two storage hosts, a minimum of two others up: nobody goes. With one more, the first does.
    let mut two = fleet(
        NOW,
        &[
            ("s1", HostRole::STORAGE, beat("1.0")),
            ("s2", HostRole::STORAGE, beat("1.0")),
        ],
    );
    assert_eq!(
        run(&mut two, &desired("2.0", 2, 1), &["s1", "s2"], NOW, true),
        ["s1: nothing", "s2: nothing"]
    );
    let mut three = fleet(
        NOW,
        &[
            ("s1", HostRole::STORAGE, beat("1.0")),
            ("s2", HostRole::STORAGE, beat("1.0")),
            ("s3", HostRole::STORAGE, beat("1.0")),
        ],
    );
    assert_eq!(
        run(
            &mut three,
            &desired("2.0", 2, 1),
            &["s1", "s2", "s3"],
            NOW,
            true
        ),
        ["s1: update 2.0", "s2: nothing", "s3: nothing"]
    );
}

/// A host that failed the desired version is not ordered it again, healthy or not; a host that
/// failed another version is not held back; a new desired version lifts the block. With a
/// daemon limit of 1 the failed host that is unhealthy (d2) holds the only slot, so nobody else
/// goes until it heartbeats healthy; the host that failed and is healthy (d1) holds none.
#[test]
fn a_failed_host_is_not_ordered_again() {
    let failed = |b: Heartbeat, target: &str| last(b, target, Outcome::Failed);
    let hosts = ["d1", "d2", "d3", "d4"];
    let with = |d2_healthy: bool| {
        let d2 = failed(beat("1.0"), "2.0");
        fleet(
            NOW,
            &[
                ("d1", HostRole::DAEMON, failed(beat("1.0"), "2.0")),
                (
                    "d2",
                    HostRole::DAEMON,
                    if d2_healthy { d2 } else { unhealthy(d2) },
                ),
                ("d3", HostRole::DAEMON, beat("1.0")),
                ("d4", HostRole::DAEMON, failed(beat("1.0"), "1.5")),
            ],
        )
    };
    let mut f = with(false);
    assert_eq!(
        run(&mut f, &desired("2.0", 1, 1), &hosts, NOW, false),
        [
            "d1: nothing",
            "d2: nothing",
            "d3: update 2.0",
            "d4: update 2.0"
        ]
    );
    assert_eq!(
        run(&mut f, &desired("3.0", 1, 1), &hosts, NOW, false),
        [
            "d1: update 3.0",
            "d2: update 3.0",
            "d3: update 3.0",
            "d4: update 3.0"
        ]
    );
    let mut one = desired("2.0", 1, 1);
    one.max_updating.set("daemon", 1);
    assert_eq!(
        run(&mut f, &one, &hosts, NOW, false),
        ["d1: nothing", "d2: nothing", "d3: nothing", "d4: nothing"],
        "the unhealthy failed host holds the only slot"
    );
    let mut f = with(true);
    assert_eq!(
        run(&mut f, &one, &hosts, NOW, false),
        [
            "d1: nothing",
            "d2: nothing",
            "d3: update 2.0",
            "d4: update 2.0"
        ],
        "healthy again, it holds none"
    );
}

/// Each of storage, daemons and Macs, alone behind, keeps both frontends waiting; the host
/// that is behind goes. (Macs are the role the frontend wait names last.)
#[test]
fn one_role_behind_keeps_the_frontends_waiting() {
    for (role, host) in [
        (HostRole::STORAGE, "s1"),
        (HostRole::DAEMON, "d1"),
        (HostRole::MAC_DAEMON, "m1"),
    ] {
        let mut f = fleet(
            NOW,
            &[
                ("s1", HostRole::STORAGE, beat("2.0")),
                ("d1", HostRole::DAEMON, beat("2.0")),
                ("m1", HostRole::MAC_DAEMON, beat("2.0")),
                ("f1", HostRole::FRONTEND, beat("1.0")),
                ("f2", HostRole::FRONTEND, beat("1.0")),
            ],
        );
        f.record(id(host), role.clone(), beat("1.0"), NOW);
        assert_eq!(
            run(
                &mut f,
                &desired("2.0", 0, 1),
                &[host, "f1", "f2"],
                NOW,
                false
            ),
            [
                format!("{host}: update 2.0"),
                "f1: nothing".to_owned(),
                "f2: nothing".to_owned()
            ],
            "{role}"
        );
    }
}

/// An order holds its slot for 120 s without the host reporting it: the host that asks again is
/// told again, the other waits at 119 s and goes at 120 s. The hosts keep heartbeating at the
/// time of each question.
#[test]
fn an_order_ages_out_after_120_seconds() {
    let mut f = fleet(
        NOW,
        &[
            ("s1", HostRole::STORAGE, beat("1.0")),
            ("s2", HostRole::STORAGE, beat("1.0")),
        ],
    );
    let d = desired("2.0", 0, 1);
    let mut lines = Vec::new();
    for (at, host) in [
        (1_000, "s1"),
        (1_000, "s2"),
        (1_010, "s1"),
        (1_119, "s2"),
        (1_120, "s2"),
    ] {
        let at = secs(at);
        f.record(id("s1"), HostRole::STORAGE, beat("1.0"), at);
        f.record(id("s2"), HostRole::STORAGE, beat("1.0"), at);
        let line = run(&mut f, &d, &[host], at, true);
        lines.push(format!("t={} {}", at.as_secs(), line[0]));
    }
    assert_eq!(
        lines,
        [
            "t=1000 s1: update 2.0",
            "t=1000 s2: nothing",
            "t=1010 s1: update 2.0",
            "t=1119 s2: nothing",
            "t=1120 s2: update 2.0",
        ]
    );
}

/// No order in the first 30 s after the coordinator starts; a host silent for 45 s is down and
/// no longer counts toward the minimum, one silent for 44 s does.
#[test]
fn thirty_seconds_of_quiet_and_forty_five_of_silence() {
    let mut f = Fleet::new(secs(970));
    f.record(id("d1"), HostRole::DAEMON, beat("1.0"), secs(999));
    let d = desired("2.0", 1, 1);
    assert_eq!(run(&mut f, &d, &["d1"], secs(999), false), ["d1: nothing"]);
    assert_eq!(
        run(&mut f, &d, &["d1"], secs(1_000), false),
        ["d1: update 2.0"]
    );
    for (seen, want) in [(956, "update 2.0"), (955, "nothing")] {
        let mut f = fleet(NOW, &[("s1", HostRole::STORAGE, beat("1.0"))]);
        f.record(id("s2"), HostRole::STORAGE, beat("1.0"), secs(seen));
        assert_eq!(
            run(&mut f, &d, &["s1"], NOW, false),
            [format!("s1: {want}")],
            "s2 last seen at {seen}"
        );
    }
}

/// A rollback is an older desired version under the same rules: storage goes first, one host
/// at a time, the frontends wait; with too few storage hosts for the minimum nobody goes.
#[test]
fn a_rollback_follows_the_same_rules() {
    let mut f = fleet(
        NOW,
        &[
            ("s1", HostRole::STORAGE, beat("2.0")),
            ("s2", HostRole::STORAGE, beat("2.0")),
            ("s3", HostRole::STORAGE, beat("2.0")),
            ("f1", HostRole::FRONTEND, beat("2.0")),
            ("f2", HostRole::FRONTEND, beat("2.0")),
        ],
    );
    assert_eq!(
        run(
            &mut f,
            &desired("1.0", 2, 1),
            &["s1", "s2", "s3", "f1", "f2"],
            NOW,
            true
        ),
        [
            "s1: update 1.0",
            "s2: nothing",
            "s3: nothing",
            "f1: nothing",
            "f2: nothing"
        ]
    );
    let mut thin = fleet(
        NOW,
        &[
            ("s1", HostRole::STORAGE, beat("2.0")),
            ("s2", HostRole::STORAGE, beat("2.0")),
        ],
    );
    assert_eq!(
        run(&mut thin, &desired("1.0", 2, 1), &["s1", "s2"], NOW, true),
        ["s1: nothing", "s2: nothing"]
    );
}
