//! The turn of a frontend that is a member of the kbf-proxy pool, over the fakes: its order, the
//! rule that the last member that is up is never taken out, the waits and their bounds, every
//! failure leaving the member drained, and a restart of the updater in each phase.

use std::time::Duration;

use crate::api::{ErrorCode, HostRoll, Hosts, Phase, Roll, RollState};
use crate::fakes::{Event, pool_member, retired};
use crate::inventory::Inventory;
use crate::ports::{Installed, PoolMember, PortError, RetireStatus};
use crate::record::{FORMAT, Record, roll_id};
use crate::updater_tests::{KEY, Rig, all, request};
use crate::{STREAMS_BOUND, Timing, Updater};

const TARGET: &str = "0.1.7+g0123456789ab-3";

/// The calls of the turn of member `a` on host `fe1`, whose host is installed on, and of member
/// `b` on host `fe2`, which is the same host and has the target by then.
const TURN_A: &str = "pv pd:a pv rt:fe1 qy:fe1 in:fe1 rn:fe1,a pv cm:fe1 pu:a";
const TURN_B: &str = "pv pd:b pv rt:fe2 qy:fe2 rn:fe2,b pv cm:fe2 pu:b";

/// A call as a word: `pv` the pool was listed, `pd:` and `pu:` a member was drained and put
/// back, `rt:` and `rs:` a retire and its status, `qy:` a query, `in:` an install of the target,
/// `rn:` a restart as `alias,unit`, `cm:` a commit, `ck:` safe-to-stop, `hl:` health.
fn ev(word: &str) -> Event {
    let (kind, rest) = word.split_once(':').unwrap_or((word, ""));
    let name = rest.to_owned();
    match kind {
        "pv" => Event::PoolView,
        "pd" => Event::PoolDrain(name),
        "pu" => Event::PoolUndrain(name),
        "rt" => Event::Retire(name),
        "rs" => Event::RetireStatus(name),
        "qy" => Event::Query(name),
        "in" => Event::Install(name, TARGET.to_owned()),
        "rn" => {
            let (alias, unit) = rest.split_once(',').unwrap_or_default();
            Event::Restart(alias.to_owned(), unit.to_owned())
        }
        "cm" => Event::Commit(name),
        "ck" => Event::Check(name),
        _ => Event::Health(name),
    }
}

fn seq(text: &str) -> Vec<Event> {
    text.split_whitespace().map(ev).collect()
}

fn no() -> PortError {
    PortError("no".into())
}

fn up(name: &str) -> PoolMember {
    pool_member(name, false, true, 0)
}

/// A retired member with `streams` streams still open.
fn open(streams: u64) -> RetireStatus {
    RetireStatus {
        open_streams: streams,
        ..retired()
    }
}

/// Two members of the pool, `a` on host `fe1` and `b` on host `fe2`, which are one host, and
/// then `extra` lines.
fn inventory_with(extra: &str) -> Inventory {
    Inventory::parse(&format!(
        "fe1 fe.farm.example frontend {KEY} instance=a base=18000\n\
         fe2 fe.farm.example frontend {KEY} instance=b base=18010\n{extra}"
    ))
    .expect("inventory")
}

/// A rig whose proxy has the members `a` and `b`, up and ready, and whose hosts `fe1` and `fe2`
/// are one host.
fn member_rig(name: &str) -> Rig {
    let rig = Rig::new(name);
    rig.pool.set_members(&["a", "b"]);
    rig.inst.same_host("fe2", "fe1");
    rig
}

fn open_over(rig: &Rig, extra: &str) -> Updater {
    rig.open_inventory(inventory_with(extra), rig.store())
}

fn run(u: &Updater, hosts: Hosts) -> Roll {
    let started = u.start(&request(TARGET, hosts, "run-1")).expect("start");
    u.drive(&started.roll.id).expect("drive")
}

fn only(alias: &str) -> Hosts {
    Hosts::Aliases(vec![alias.to_owned()])
}

fn aliases(roll: &Roll) -> Vec<&str> {
    roll.hosts.iter().map(|h| h.alias.as_str()).collect()
}

fn phases(roll: &Roll) -> Vec<Phase> {
    roll.hosts.iter().map(|h| h.phase).collect()
}

/// Asserts that a member's turn goes drain, wait for the streams, retire, install, restart, wait
/// until it is ready on the target's commit, and undrain, in that order, with each call made
/// once, that the member is back in the pool at the end, and that a healthy turn waits for
/// nothing.
#[test]
fn a_member_turn_runs_in_the_order_drain_wait_retire_install_restart_ready_undrain() {
    let rig = member_rig("fe-order");
    let u = open_over(&rig, "");
    let roll = run(&u, only("fe1"));
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(phases(&roll), [Phase::Done]);
    assert_eq!(roll.hosts[0].attempts, 1);
    assert_eq!(rig.log.events(), seq(TURN_A));
    assert_eq!(rig.pool.table(), [up("a"), up("b")]);
    assert_eq!(rig.elapsed(), Duration::ZERO);
}

/// Asserts that with two members the second is rolled after the first is back in the pool, that
/// the install ran once for the host the two share, and that the second member's turn leaves
/// the install out because the host has the target.
#[test]
fn the_second_member_is_rolled_after_the_first_is_back_and_the_install_runs_once() {
    let rig = member_rig("fe-two");
    let u = open_over(&rig, "");
    let roll = run(&u, Hosts::default());
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(phases(&roll), [Phase::Done, Phase::Done]);
    assert_eq!(rig.log.events(), seq(&[TURN_A, TURN_B].join(" ")));
    assert_eq!(rig.log.installs(), ["fe1"]);
    assert_eq!(roll.hosts[0].attempts, 1);
    assert_eq!(roll.hosts[1].attempts, 0);
    assert_eq!(rig.pool.table(), [up("a"), up("b")]);
}

/// Asserts that the members are rolled before the other hosts, and that the other hosts are
/// rolled as they were: safe-to-stop, install and health follow the last member's undrain.
#[test]
fn members_are_rolled_before_the_other_hosts() {
    let rig = member_rig("fe-then-st");
    let u = open_over(&rig, &format!("st1 st1.farm.example storage {KEY}\n"));
    let roll = run(&u, Hosts::default());
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(rig.log.installs(), ["fe1", "st1"]);
    let want = [TURN_A, TURN_B, "ck:st1 in:st1 hl:st1"].join(" ");
    assert_eq!(rig.log.events(), seq(&want));
}

/// Asserts that a roll over a list that names a host before two members, whose plan moves both
/// members to the front, is read again as it was when the updater starts, and not refused as
/// a record that contradicts its request.
#[test]
fn a_list_naming_a_host_before_two_members_loads_again_with_the_members_first() {
    let rig = member_rig("fe-list-reload");
    let st1 = format!("st1 st1.farm.example storage {KEY}\n");
    let u = open_over(&rig, &st1);
    let list = ["st1", "fe1", "fe2"].map(str::to_owned).to_vec();
    let req = request(TARGET, Hosts::Aliases(list), "run-1");
    let started = u.start(&req).expect("start");
    assert_eq!(aliases(&started.roll), ["fe1", "fe2", "st1"]);
    let again = open_over(&rig, &st1);
    assert_eq!(again.current().expect("current"), Some(started.roll));
}

/// A named setup of the pool and the hosts, and the code the roll over every host then fails
/// with at the first member.
struct Case {
    name: &'static str,
    code: ErrorCode,
    /// The proxy has drained the member `a` when the roll fails, so it is out of the pool.
    drained: bool,
    setup: fn(&Rig),
}

/// A roll over every host after `setup`, and the rig it ran on, in a directory of its own named
/// from `prefix` and `name` (tests run in parallel, and a directory is not shared).
fn failing(prefix: &str, name: &str, setup: fn(&Rig)) -> (Rig, Roll) {
    let rig = member_rig(&format!("{prefix}-{name}"));
    setup(&rig);
    let u = open_over(&rig, "");
    let roll = run(&u, Hosts::default());
    (rig, roll)
}

/// Asserts that a member that is the last one up is refused before anything is changed: another
/// member that is draining or not ready, a member the proxy does not list and a pool that cannot
/// be read each fail the roll at it with the pool listed once and no drain made, and no later
/// member is touched.
#[test]
fn a_member_that_would_leave_the_pool_empty_is_refused_without_a_drain() {
    let cases = [
        Case {
            name: "other-drained",
            code: ErrorCode::UnsafeToStop,
            drained: false,
            setup: |rig| rig.pool.set_draining("b", true),
        },
        Case {
            name: "other-not-ready",
            code: ErrorCode::UnsafeToStop,
            drained: false,
            setup: |rig| rig.pool.set_ready("b", false),
        },
        Case {
            name: "not-listed",
            code: ErrorCode::UnknownMember,
            drained: false,
            setup: |rig| rig.pool.set_members(&["b"]),
        },
        Case {
            name: "unreadable",
            code: ErrorCode::SafeToStopUnavailable,
            drained: false,
            setup: |rig| rig.pool.script_views(vec![Err(no())]),
        },
    ];
    for case in cases {
        let (rig, roll) = failing("fe-refused", case.name, case.setup);
        assert_eq!(roll.state, RollState::Failed, "{}", case.name);
        let failure = roll.failure.clone().expect("failure");
        assert_eq!((failure.alias.as_str(), failure.code), ("fe1", case.code));
        let detail = &failure.detail;
        assert!(!detail.contains("left drained"), "{detail}");
        assert_eq!(phases(&roll), [Phase::Failed, Phase::Pending]);
        assert_eq!(rig.log.events(), seq("pv"), "{}", case.name);
    }
}

/// Asserts that a member an earlier roll left drained is rolled again while one other member is
/// up (the rule asks that some other member serve, and counts the member itself for nothing,
/// since a member that is out of service is taken out of nothing), and is back in the pool at
/// the end.
#[test]
fn a_member_left_drained_by_an_earlier_roll_is_rolled_while_another_is_up() {
    let rig = member_rig("fe-earlier");
    rig.pool.set_draining("a", true);
    let u = open_over(&rig, "");
    let roll = run(&u, only("fe1"));
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(rig.pool.table(), [up("a"), up("b")]);
}

/// Asserts that a member found `installing` whose host reports the install begun before the
/// restart as still in progress waits for it, one poll apart, and makes no second install beside
/// it: the host reports the target on the next ask, and the turn goes on to the restart.
#[test]
fn an_install_still_in_progress_after_a_restart_is_waited_for_and_not_repeated() {
    let rig = member_rig("fe-install-wait");
    rig.pool.set_draining("a", true);
    let target = Installed::Version(TARGET.to_owned());
    rig.inst.set_installed("fe1", target);
    rig.inst.script_query("fe1", vec![Ok(Installed::InProgress)]);
    rig.store().save(&record(Phase::Installing)).expect("save");
    let u = open_over(&rig, "");
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done);
    let want = seq("pd:a qy:fe1 qy:fe1 rn:fe1,a pv cm:fe1 pu:a");
    assert_eq!(rig.log.events()[..want.len()], want[..]);
    assert!(rig.log.installs().is_empty());
    assert_eq!(rig.elapsed(), Duration::from_secs(5));
}

/// Asserts that an install that stays in progress fails the roll at the member when the install
/// bound is out, with `install_failed`, the host and the bound in the detail and the member left
/// drained, and that no install is made beside it.
#[test]
fn an_install_that_stays_in_progress_fails_the_turn_at_the_install_bound() {
    let rig = member_rig("fe-install-stuck");
    rig.inst.set_installed("fe1", Installed::InProgress);
    let u = open_over(&rig, "");
    let roll = run(&u, only("fe1"));
    assert_eq!(roll.state, RollState::Failed);
    let failure = roll.failure.expect("failure");
    assert_eq!(failure.code, ErrorCode::InstallFailed);
    let detail = &failure.detail;
    let stuck = "an install on fe1 was still in progress after 900 s";
    assert!(detail.contains(stuck), "{detail}");
    assert!(detail.contains("is left drained"), "{detail}");
    assert_eq!(rig.elapsed(), Timing::default().install_bound);
    assert!(rig.log.installs().is_empty());
    assert!(rig.pool.table()[0].draining);
}

/// Asserts that the wait for the streams polls until the REAPI and the operator streams are both
/// 0, one poll apart, and goes on to the retire then.
#[test]
fn the_wait_for_streams_polls_until_both_listeners_are_empty() {
    let rig = member_rig("fe-streams");
    rig.pool.script_streams("a", vec![(1, 1), (0, 1)], (0, 0));
    let u = open_over(&rig, "");
    let roll = run(&u, only("fe1"));
    assert_eq!(roll.state, RollState::Done);
    let want = "pv pd:a pv pv pv rt:fe1 qy:fe1 in:fe1 rn:fe1,a pv cm:fe1 pu:a";
    assert_eq!(rig.log.events(), seq(want));
    assert_eq!(rig.elapsed(), Duration::from_secs(10));
}

/// Asserts that streams that never end fail the roll when the bound is out, before the retire,
/// with the number open, the bound, and the member left drained, and that the member stays
/// drained.
#[test]
fn a_drain_wait_that_times_out_fails_before_the_retire_and_leaves_the_member_drained() {
    let rig = member_rig("fe-timeout");
    rig.pool.script_streams("a", vec![], (0, 3));
    let u = open_over(&rig, "");
    let roll = run(&u, Hosts::default());
    assert_eq!(roll.state, RollState::Failed);
    let failure = roll.failure.expect("failure");
    assert_eq!(failure.code, ErrorCode::DrainFailed);
    let open_for = "3 streams were still open on member a after 900 s";
    assert!(failure.detail.contains(open_for), "{}", failure.detail);
    let note = "frontend member a (fe1) is left drained";
    assert!(failure.detail.contains(note), "{}", failure.detail);
    assert_eq!(rig.elapsed(), STREAMS_BOUND);
    let events = rig.log.events();
    assert!(!events.iter().any(|e| matches!(e, Event::Retire(_))));
    assert!(rig.pool.table()[0].draining);
}

/// Asserts that the retire is waited on until the member reports no stream open, one poll
/// apart, asking its status each time, before the host is installed on.
#[test]
fn the_retire_is_waited_on_until_the_member_has_no_stream_open() {
    let rig = member_rig("fe-retire-wait");
    rig.front.script_retire("fe1", vec![], Ok(open(2)));
    rig.front.script_status("fe1", vec![Ok(open(1))], Ok(retired()));
    let u = open_over(&rig, "");
    let roll = run(&u, only("fe1"));
    assert_eq!(roll.state, RollState::Done);
    let want = "pv pd:a pv rt:fe1 rs:fe1 rs:fe1 qy:fe1 in:fe1 rn:fe1,a pv cm:fe1 pu:a";
    assert_eq!(rig.log.events(), seq(want));
    assert_eq!(rig.elapsed(), Duration::from_secs(10));
}

/// Asserts that a pool that cannot be read, a member that is not ready yet, and a member that
/// reports another commit or cannot be asked are each waited on, one poll apart, until the member
/// is ready on the target's commit, and that the commit is asked only of a member that is ready.
#[test]
fn the_wait_for_readiness_goes_on_through_a_pool_and_a_member_that_do_not_answer_yet() {
    let rig = member_rig("fe-ready-wait");
    let table = vec![up("a"), up("b")];
    let drained = vec![pool_member("a", true, true, 0), up("b")];
    let not_ready = vec![pool_member("a", true, false, 0), up("b")];
    rig.pool.script_views(vec![
        Ok(table),
        Ok(drained.clone()),
        Ok(drained.clone()),
        Err(no()),
        Ok(drained),
        Ok(not_ready),
    ]);
    rig.front.script_commit(
        "fe1",
        vec![Ok("fedcba98".to_owned()), Err(no())],
        Ok("0123456789ab".to_owned()),
    );
    let u = open_over(&rig, "");
    let roll = run(&u, only("fe1"));
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(rig.elapsed(), Duration::from_secs(20));
    let asked = Event::Commit("fe1".to_owned());
    let commits = rig.log.events().iter().filter(|e| **e == asked).count();
    assert_eq!(commits, 3);
}

/// Every way a member's turn fails after it is begun, each with the code it fails with.
fn failures() -> Vec<Case> {
    vec![
        Case {
            name: "drain-refused",
            code: ErrorCode::DrainFailed,
            drained: false,
            setup: |rig| rig.pool.script_drain("a", vec![], Err(no())),
        },
        Case {
            name: "wait-unreadable",
            code: ErrorCode::DrainFailed,
            drained: true,
            setup: |rig| {
                let table = vec![up("a"), up("b")];
                rig.pool.script_views(vec![Ok(table), Err(no())]);
            },
        },
        Case {
            name: "wait-vanished",
            code: ErrorCode::UnknownMember,
            drained: true,
            setup: |rig| {
                let table = vec![Ok(vec![up("a"), up("b")]), Ok(vec![up("b")])];
                rig.pool.script_views(table);
            },
        },
        Case {
            name: "retire-refused",
            code: ErrorCode::RetireFailed,
            drained: true,
            setup: |rig| rig.front.script_retire("fe1", vec![], Err(no())),
        },
        Case {
            name: "retire-streams-stuck",
            code: ErrorCode::RetireFailed,
            drained: true,
            setup: |rig| {
                rig.front.script_retire("fe1", vec![], Ok(open(1)));
                rig.front.script_status("fe1", vec![], Ok(open(1)));
            },
        },
        Case {
            name: "retire-status-unreadable",
            code: ErrorCode::RetireFailed,
            drained: true,
            setup: |rig| {
                rig.front.script_retire("fe1", vec![], Ok(open(1)));
                rig.front.script_status("fe1", vec![], Err(no()));
            },
        },
        Case {
            name: "retire-not-done",
            code: ErrorCode::RetireFailed,
            drained: true,
            setup: |rig| {
                let not_retired = RetireStatus {
                    retired: false,
                    ..retired()
                };
                rig.front.script_retire("fe1", vec![], Ok(not_retired));
            },
        },
        Case {
            name: "query-fails",
            code: ErrorCode::InstallFailed,
            drained: true,
            setup: |rig| rig.inst.script_query("fe1", vec![Err(no())]),
        },
        Case {
            name: "install-refused",
            code: ErrorCode::InstallFailed,
            drained: true,
            setup: |rig| rig.inst.script_install("fe1", vec![], Err(no())),
        },
        Case {
            name: "install-silent",
            code: ErrorCode::InstallFailed,
            drained: true,
            setup: |rig| {
                let silent = PortError::unreachable("timeout");
                rig.inst.script_install("fe1", vec![], Err(silent));
            },
        },
        Case {
            name: "restart-fails",
            code: ErrorCode::RestartFailed,
            drained: true,
            setup: |rig| rig.inst.script_restart("fe1", vec![], Err(no())),
        },
        Case {
            name: "never-ready",
            code: ErrorCode::HealthTimeout,
            drained: true,
            setup: |rig| rig.pool.set_ready("a", false),
        },
        Case {
            name: "wrong-build",
            code: ErrorCode::HealthTimeout,
            drained: true,
            setup: |rig| {
                let other = Ok("fedcba9876543210".to_owned());
                rig.front.script_commit("fe1", vec![], other);
            },
        },
        Case {
            name: "commit-unreadable",
            code: ErrorCode::HealthTimeout,
            drained: true,
            setup: |rig| rig.front.script_commit("fe1", vec![], Err(no())),
        },
        Case {
            name: "dropped-from-pool",
            code: ErrorCode::SwapAbandoned,
            drained: true,
            setup: |rig| {
                let both = vec![up("a"), up("b")];
                let drained = vec![pool_member("a", true, true, 0), up("b")];
                rig.pool.script_views(vec![Ok(both), Ok(drained), Ok(vec![up("b")])]);
            },
        },
        Case {
            name: "undrain-refused",
            code: ErrorCode::UndrainFailed,
            drained: true,
            setup: |rig| rig.pool.script_undrain("a", vec![], Err(no())),
        },
    ]
}

/// Asserts that each failure after the turn is begun (a refused drain, a wait for streams that
/// cannot read the pool or has lost the member, a retire that is refused, never ends or is not
/// done, an install that cannot be asked, is refused or does not answer, a restart that fails, a
/// member that is never ready, is on another build, cannot be asked or is gone from the pool,
/// and an undrain that is refused) fails the roll at that member with its code, touches no later
/// member, and, whenever the proxy had drained the member, leaves it out of the pool and names
/// it as left drained; a drain the proxy refuses is not said to have left it drained.
#[test]
fn each_failure_stops_the_roll_with_the_member_drained() {
    for case in failures() {
        let (rig, roll) = failing("fe-fail", case.name, case.setup);
        assert_eq!(roll.state, RollState::Failed, "{}", case.name);
        let failure = roll.failure.clone().expect("failure");
        let label = format!("{}: {}", case.name, failure.detail);
        assert_eq!(failure.alias, "fe1", "{label}");
        assert_eq!(failure.code, case.code, "{label}");
        let note = "frontend member a (fe1) is left drained";
        assert_eq!(failure.detail.contains(note), case.drained, "{label}");
        assert_eq!(phases(&roll), [Phase::Failed, Phase::Pending], "{label}");
        assert_eq!(rig.pool.table()[0].draining, case.drained, "{label}");
        assert!(rig.log.touching("fe2").is_empty(), "{label}");
        assert!(rig.log.touching("b").is_empty(), "{label}");
    }
}

/// The detail a roll over every host fails with in the failure called `name`, and the rig.
fn detail_of(name: &str) -> (Rig, String) {
    let list = failures();
    let case = list.iter().find(|c| c.name == name).expect(name);
    let (rig, roll) = failing("fe-detail", name, case.setup);
    (rig, roll.failure.expect("failure").detail)
}

/// Asserts the bound and the reason of the failures that wait: a member that is never ready fails
/// after the frontend's health bound with the reason it was not ready, a member on another build
/// says which, and a retire that never ends fails after the streams bound with the number open.
#[test]
fn the_waits_fail_with_their_bounds_and_reasons() {
    let (rig, detail) = detail_of("never-ready");
    let reason = "300 s: member a is not ready;";
    assert!(detail.starts_with(reason), "{detail}");
    assert_eq!(rig.elapsed(), Timing::default().healthy_frontend);
    let (_, detail) = detail_of("wrong-build");
    let build = "runs build fedcba9876543210, not the target's";
    assert!(detail.contains(build), "{detail}");
    let (rig, detail) = detail_of("retire-streams-stuck");
    let stuck = "1 streams were still open after the retire";
    assert!(detail.contains(stuck), "{detail}");
    assert_eq!(rig.elapsed(), STREAMS_BOUND);
}

/// Asserts that a version that names no commit cannot be shown to be running: the member is
/// restarted, and the roll fails at it with `swap_abandoned` and the member left drained.
#[test]
fn a_version_that_names_no_commit_fails_the_member_after_its_restart() {
    let rig = member_rig("fe-no-commit");
    let u = open_over(&rig, "");
    let req = request("2.0", Hosts::default(), "k");
    let started = u.start(&req).expect("start");
    let roll = u.drive(&started.roll.id).expect("drive");
    let failure = roll.failure.expect("failure");
    assert_eq!(failure.code, ErrorCode::SwapAbandoned);
    let detail = &failure.detail;
    assert!(detail.contains("names no commit"), "{detail}");
    let restarted = Event::Restart("fe1".to_owned(), "a".to_owned());
    assert!(rig.log.events().contains(&restarted));
    assert!(rig.pool.table()[0].draining);
}

/// A record of a roll over everything in which `fe1` is in `first` and `fe2` has not begun.
fn record(first: Phase) -> Record {
    let host = |alias: &str, phase| HostRoll {
        alias: alias.to_owned(),
        phase,
        attempts: 0,
        detail: None,
    };
    Record {
        format: FORMAT,
        seq: 1,
        request: all(TARGET, "run-1"),
        roll: Roll {
            id: roll_id(1),
            state: RollState::Running,
            hosts: vec![host("fe1", first), host("fe2", Phase::Pending)],
            failure: None,
        },
        resumes: 0,
        abort_requested: false,
    }
}

/// A phase a record can hold `fe1` in, what the proxy and the host look like then, and the calls
/// the resumed turn starts with.
struct Resume {
    from: Phase,
    /// The proxy has the member drained already.
    drained: bool,
    /// The host has the target installed already.
    installed: bool,
    expect: &'static str,
}

/// Asserts that an updater started over a record that holds the member in each phase drains the
/// member again whatever the proxy says, then goes on from that phase and not before it,
/// repeats no install the host has, and finishes the roll with both members back in the pool.
#[test]
fn a_restart_of_the_updater_in_each_phase_resumes_with_the_drain_asserted() {
    let resumes = [
        Resume {
            from: Phase::Draining,
            drained: false,
            installed: false,
            expect: "pd:a pv rt:fe1 qy:fe1 in:fe1 rn:fe1,a pv cm:fe1 pu:a",
        },
        Resume {
            from: Phase::Retiring,
            drained: true,
            installed: false,
            expect: "pd:a rt:fe1 qy:fe1 in:fe1 rn:fe1,a pv cm:fe1 pu:a",
        },
        Resume {
            from: Phase::Installing,
            drained: true,
            installed: false,
            expect: "pd:a qy:fe1 in:fe1 rn:fe1,a pv cm:fe1 pu:a",
        },
        Resume {
            from: Phase::Installing,
            drained: true,
            installed: true,
            expect: "pd:a qy:fe1 rn:fe1,a pv cm:fe1 pu:a",
        },
        Resume {
            from: Phase::Restarting,
            drained: true,
            installed: true,
            expect: "pd:a rn:fe1,a pv cm:fe1 pu:a",
        },
        Resume {
            from: Phase::WaitingHealthy,
            drained: true,
            installed: true,
            expect: "pd:a pv cm:fe1 pu:a",
        },
        Resume {
            from: Phase::Undraining,
            drained: true,
            installed: true,
            expect: "pd:a pu:a",
        },
    ];
    for (i, resume) in resumes.iter().enumerate() {
        let rig = member_rig(&format!("fe-resume-{i}"));
        rig.pool.set_draining("a", resume.drained);
        if resume.installed {
            let target = Installed::Version(TARGET.to_owned());
            rig.inst.set_installed("fe1", target);
        }
        rig.store().save(&record(resume.from)).expect("save");
        let u = open_over(&rig, "");
        let roll = u.drive("roll-1").expect("drive");
        let label = format!("{:?}", resume.from);
        assert_eq!(roll.state, RollState::Done, "{label}");
        let want = seq(resume.expect);
        let got = rig.log.events();
        assert_eq!(got[..want.len()], want[..], "{label}");
        assert_eq!(rig.store().load().expect("l").expect("r").resumes, 1);
        assert_eq!(rig.pool.table(), [up("a"), up("b")], "{label}");
    }
}

/// An old record, as a build from before the member phases wrote it: a host that is done and a
/// host `waiting_healthy`, with none of the new phases.
const OLD_RECORD: &str = r#"{
  "format": 1,
  "seq": 1,
  "request": {"package": "kbf", "version": "0.1.7+g0123456789ab-3", "hosts": "all",
              "idempotency_key": "run-1"},
  "roll": {"id": "roll-1", "state": "running",
           "hosts": [{"alias": "fe1", "phase": "done", "attempts": 1, "detail": null},
                     {"alias": "fe2", "phase": "waiting_healthy", "attempts": 1,
                      "detail": null}],
           "failure": null},
  "resumes": 0,
  "abort_requested": false
}"#;

/// Asserts that a record written before the member phases existed still parses and checks, with
/// no format raise, that it resumes on a member as the same phases did, and that the phases the
/// member turn adds are written as snake case words.
#[test]
fn an_old_roll_record_still_parses_and_resumes() {
    let rec: Record = serde_json::from_str(OLD_RECORD).expect("old record");
    assert_eq!(rec.format, FORMAT);
    assert_eq!(rec.check(), Ok(()));
    let rig = member_rig("fe-old-record");
    std::fs::write(rig.dir.join("roll.json"), OLD_RECORD).expect("write");
    let loaded = rig.store().load().expect("load").expect("record");
    assert_eq!(loaded, rec);
    let u = open_over(&rig, "");
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(rig.log.events(), seq("pd:b pv cm:fe2 pu:b"));
    let words = [
        (Phase::Draining, "\"draining\""),
        (Phase::Retiring, "\"retiring\""),
        (Phase::Restarting, "\"restarting\""),
        (Phase::Undraining, "\"undraining\""),
    ];
    for (phase, word) in words {
        assert_eq!(serde_json::to_string(&phase).expect("json"), word);
    }
}
