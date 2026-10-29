//! The turn of a frontend that is a member of a pool, over the
//! fakes: its order, the wait for the other members to be ready and what starts it again, the
//! wait for the member to exit and its bound, every failure after the drain leaving the member
//! not serving, a restart of the updater in each phase, and a record of an earlier build's turn at a proxy.

use std::time::Duration;

use crate::api::{ErrorCode, HostRoll, Hosts, Phase, Roll, RollState};
use crate::fakes::{Event, draining, gone};
use crate::inventory::Inventory;
use crate::ports::{DrainStatus, Installed, PortError};
use crate::record::{FORMAT, Record, roll_id};
use crate::updater_tests::{KEY, Rig, all, request};
use crate::{Timing, Updater};

const TARGET: &str = "0.1.7+g0123456789ab-3";

/// The asks of the other member's readiness in a window that goes well: one at the start and
/// one every 5 s for the 30 s of the window.
const WINDOW: usize = 7;

/// The calls of a turn after the member was told to drain and has exited: the host is asked
/// what it has and installed on (member `a`, whose host is installed on), the unit is restarted,
/// and the member is asked if it is ready and what commit it runs. The second member's host has
/// the target by then.
const THEN_A: &str = "qy:fe1 in:fe1 rn:fe1,a rd:fe1 cm:fe1";
const THEN_B: &str = "qy:fe2 rn:fe2,b rd:fe2 cm:fe2";

/// A call as a word: `rd:` a member was asked if it is ready, `fd:` it was told to drain, `fs:`
/// its drain was read, `qy:` a query, `in:` an install of the target, `rn:` a restart as
/// `alias,unit`, `cm:` a commit, `ck:` safe-to-stop, `hl:` health.
fn ev(word: &str) -> Event {
    let (kind, rest) = word.split_once(':').unwrap_or((word, ""));
    let name = rest.to_owned();
    match kind {
        "rd" => Event::Ready(name),
        "fd" => Event::MemberDrain(name),
        "fs" => Event::MemberDrainStatus(name),
        "qy" => Event::Query(name),
        "ck" => Event::Check(name),
        "hl" => Event::Health(name),
        "in" => Event::Install(name, TARGET.to_owned()),
        "rn" => {
            let (alias, unit) = rest.split_once(',').unwrap_or_default();
            Event::Restart(alias.to_owned(), unit.to_owned())
        }
        _ => Event::Commit(name),
    }
}

fn seq(text: &str) -> Vec<Event> {
    text.split_whitespace().map(ev).collect()
}

/// `times` asks of the readiness of `peer`.
fn asks(peer: &str, times: usize) -> String {
    vec![format!("rd:{peer}"); times].join(" ")
}

/// The calls of the turn of member `a` on host `fe1`, and of member `b` on host `fe2`, for a
/// window of the other member's readiness that goes well and a member that exits at once.
fn turn_a() -> String {
    format!("{} fd:fe1 fs:fe1 {THEN_A}", asks("fe2", WINDOW))
}

fn turn_b() -> String {
    format!("{} fd:fe2 fs:fe2 {THEN_B}", asks("fe1", WINDOW))
}

fn no() -> PortError {
    PortError("no".into())
}

/// The status of a member that has ended its drain and is closing.
fn closing() -> DrainStatus {
    DrainStatus {
        closing: true,
        ..draining(0)
    }
}

/// The status of a member that answers without having been told to drain.
fn idle() -> DrainStatus {
    DrainStatus {
        draining: false,
        ..draining(0)
    }
}

/// Two members, `a` on host `fe1` and `b` on host `fe2`, which are one host, and then `extra`
/// lines.
fn inventory_with(extra: &str) -> Inventory {
    Inventory::parse(&format!(
        "fe1 fe.farm.example frontend {KEY} instance=a base=18000\n\
         fe2 fe.farm.example frontend {KEY} instance=b base=18010\n{extra}"
    ))
    .expect("inventory")
}

/// A rig whose hosts `fe1` and `fe2` are one host. The members answer as the
/// fakes do unless a test says otherwise: ready, and gone when their drain is read.
fn member_rig(name: &str) -> Rig {
    let rig = Rig::new(name);
    rig.inst.same_host("fe2", "fe1");
    rig
}

fn open_over(rig: &Rig) -> Updater {
    rig.open_inventory(inventory_with(""), rig.store())
}

fn run(u: &Updater, hosts: Hosts) -> Roll {
    let started = u.start(&request(TARGET, hosts, "run-1")).expect("start");
    u.drive(&started.roll.id).expect("drive")
}

fn only(alias: &str) -> Hosts {
    Hosts::Aliases(vec![alias.to_owned()])
}

fn phases(roll: &Roll) -> Vec<Phase> {
    roll.hosts.iter().map(|h| h.phase).collect()
}

/// Asserts that a member's turn without a proxy goes in the order: the other member is found
/// ready for the whole window, the member is told to drain, its exit is waited for, the host is
/// installed on, the unit is restarted, and the member is waited for until it is ready on the
/// target's commit. It also asserts that the only repeated calls are the seven asks of the other
/// member's readiness, that no other call is made, that the record ends `done` with one
/// install, and that the window is the only wait.
#[test]
fn a_turn_without_a_proxy_goes_peers_drain_exit_install_restart_ready() {
    let rig = member_rig("np-order");
    let u = open_over(&rig);
    let roll = run(&u, only("fe1"));
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(phases(&roll), [Phase::Done]);
    assert_eq!(roll.hosts[0].attempts, 1);
    assert_eq!(rig.log.events(), seq(&turn_a()));
    assert_eq!(rig.elapsed(), Duration::from_secs(30));
}

/// Asserts that with two members on one host the second is rolled after the first is ready
/// again, each after the other is found ready for a window, that the host is installed on once,
/// at the first member, and that the second member's turn leaves the install out because the
/// host has the target.
#[test]
fn two_members_on_one_host_roll_one_after_the_other_with_one_install() {
    let rig = member_rig("np-two");
    let u = open_over(&rig);
    let roll = run(&u, Hosts::default());
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(phases(&roll), [Phase::Done, Phase::Done]);
    let want = format!("{} {}", turn_a(), turn_b());
    assert_eq!(rig.log.events(), seq(&want));
    assert_eq!(rig.log.installs(), ["fe1"]);
    assert_eq!(roll.hosts[0].attempts, 1);
    assert_eq!(roll.hosts[1].attempts, 0);
    assert_eq!(rig.elapsed(), Duration::from_secs(60));
}

/// Asserts that another member that is not ready for the whole wait, whether it answers 503 or
/// cannot be asked, refuses the turn: the roll fails at the member with `unsafe_to_stop` once the
/// window and the frontend health bound have passed (330 s), saying that the others were not
/// all ready for the window, and nothing but the asks of the other member's readiness was
/// called: no drain, no install, no restart. The later member stays pending.
#[test]
fn peers_that_are_not_ready_for_the_whole_wait_refuse_the_turn_without_a_drain() {
    let cases: [(Result<bool, PortError>, &str); 2] = [
        (Ok(false), "fe2 is not ready"),
        (Err(no()), "fe2 could not be asked: no"),
    ];
    for (i, (answer, text)) in cases.into_iter().enumerate() {
        let rig = member_rig(&format!("np-refused-{i}"));
        rig.front.script_ready("fe2", vec![], answer);
        let u = open_over(&rig);
        let roll = run(&u, Hosts::default());
        assert_eq!(roll.state, RollState::Failed, "{text}");
        let failure = roll.failure.clone().expect("failure");
        assert_eq!(failure.alias, "fe1", "{text}");
        assert_eq!(failure.code, ErrorCode::UnsafeToStop, "{text}");
        let detail = &failure.detail;
        let window = "not all ready for 30 s in a row";
        assert!(detail.contains(window), "{detail}");
        assert!(detail.contains(text), "{detail}");
        assert_eq!(phases(&roll), [Phase::Failed, Phase::Pending], "{text}");
        let asked = Event::Ready("fe2".to_owned());
        assert!(rig.log.events().iter().all(|e| *e == asked), "{text}");
        assert_eq!(rig.elapsed(), Duration::from_secs(330), "{text}");
    }
}

/// Asserts that a member that misses once in the window starts the window again: the other
/// member answers ready four times, not ready once, and then ready, and the turn goes on only
/// 30 s after the miss was followed by a ready answer (55 s from the first ask, 12 asks), and
/// not at 30 s from the first ask.
#[test]
fn a_peer_that_misses_once_starts_the_window_again() {
    let rig = member_rig("np-flap");
    let ready = vec![Ok(true), Ok(true), Ok(true), Ok(true), Ok(false)];
    rig.front.script_ready("fe2", ready, Ok(true));
    let u = open_over(&rig);
    let roll = run(&u, only("fe1"));
    assert_eq!(roll.state, RollState::Done);
    let want = format!("{} fd:fe1 fs:fe1 {THEN_A}", asks("fe2", 12));
    assert_eq!(rig.log.events(), seq(&want));
    assert_eq!(rig.elapsed(), Duration::from_secs(55));
}

/// Asserts that a member that is the only one in the inventory is refused before any call,
/// with `unsafe_to_stop`: there is no other member to serve while it is rolled.
#[test]
fn a_member_with_no_other_member_is_refused_without_a_call() {
    let rig = member_rig("np-alone");
    let alone = format!("fe1 fe.farm.example frontend {KEY} instance=a base=18000\n");
    let alone = Inventory::parse(&alone).expect("inventory");
    let u = rig.open_inventory(alone, rig.store());
    let roll = run(&u, only("fe1"));
    assert_eq!(roll.state, RollState::Failed);
    let failure = roll.failure.expect("failure");
    assert_eq!(failure.code, ErrorCode::UnsafeToStop);
    let detail = &failure.detail;
    assert!(detail.contains("no other member"), "{detail}");
    assert!(rig.log.events().is_empty());
    assert_eq!(rig.elapsed(), Duration::ZERO);
}

/// Asserts that, with no drain deadline (the default), a member that is still draining after
/// the old 1200 s is waited for: 400 reads 5 s apart (2000 s) say two executions are open, then
/// the member has exited, and the turn goes on to the install and the restart and is done.
#[test]
fn with_no_drain_deadline_a_drain_is_waited_for_past_1200_seconds() {
    let rig = member_rig("np-long-drain");
    let mut reads = vec![Ok(draining(2)); 400];
    reads.push(Err(gone()));
    rig.front.script_drain_status("fe1", reads, Err(gone()));
    let u = open_over(&rig);
    let roll = run(&u, only("fe1"));
    assert_eq!(roll.state, RollState::Done, "{:?}", roll.failure);
    assert!(rig.elapsed() >= Duration::from_secs(2000));
    assert_eq!(rig.log.installs(), ["fe1"]);
    let events = rig.log.events();
    let polls = events.iter().filter(|e| **e == ev("fs:fe1")).count();
    assert_eq!(polls, 401);
}

/// Asserts that a drain that never ends fails the turn at the drain deadline the operator set
/// (1200 s after the window), with `drain_failed`, the number of executions still open, the
/// bound and the member named as left draining, before any install or restart, and that the
/// wait reads the member's drain once every 5 s.
#[test]
fn a_drain_that_never_ends_fails_the_turn_with_the_member_left_draining() {
    let rig = member_rig("np-stuck");
    let first = Ok(draining(2));
    rig.front.script_drain_status("fe1", vec![], first);
    let timing = Timing {
        frontend_drain: Some(Duration::from_secs(1200)),
        ..Timing::default()
    };
    let u = rig.open_inventory_timed(inventory_with(""), rig.store(), timing);
    let roll = run(&u, Hosts::default());
    assert_eq!(roll.state, RollState::Failed);
    let failure = roll.failure.clone().expect("failure");
    assert_eq!(failure.code, ErrorCode::DrainFailed);
    let detail = &failure.detail;
    let open = "member a was still draining after 1200 s: 2 executions were still open";
    assert!(detail.contains(open), "{detail}");
    let note = "frontend member a (fe1) is left draining";
    assert!(detail.contains(note), "{detail}");
    assert_eq!(phases(&roll), [Phase::Failed, Phase::Pending]);
    let t = Timing::default();
    assert_eq!(rig.elapsed(), t.peer_ready + Duration::from_secs(1200));
    let events = rig.log.events();
    let (queried, restarted) = (ev("qy:fe1"), ev("rn:fe1,a"));
    assert!(rig.log.installs().is_empty());
    assert!(!events.contains(&queried));
    assert!(!events.contains(&restarted));
    let polls = events.iter().filter(|e| **e == ev("fs:fe1")).count();
    assert_eq!(polls, 241);
}

/// A roll over `fe1` in which the drain of member `a` reads as `first` and then as `then`.
fn drained_by(
    name: &str,
    first: Vec<Result<DrainStatus, PortError>>,
    then: Result<DrainStatus, PortError>,
) -> (Rig, Roll) {
    let rig = member_rig(name);
    rig.front.script_drain_status("fe1", first, then);
    let u = open_over(&rig);
    let roll = run(&u, only("fe1"));
    (rig, roll)
}

/// Asserts that the wait for the member's exit goes on through a drain that still has executions
/// open and through a read that times out, one poll apart, and ends when the member refuses the
/// connection, and that it ends at once when the member reports that it is closing, and that the
/// turn then goes on with the install, the restart and the wait for readiness.
#[test]
fn the_wait_for_the_exit_ends_on_a_refused_connection_or_a_closing_status() {
    let timeout = PortError::unreachable("timed out");
    let busy = vec![Ok(draining(3)), Err(timeout), Ok(draining(1))];
    let (rig, roll) = drained_by("np-exit-refused", busy, Err(gone()));
    assert_eq!(roll.state, RollState::Done);
    let polls = ["fs:fe1"; 4].join(" ");
    let want = format!("{} fd:fe1 {polls} {THEN_A}", asks("fe2", WINDOW));
    assert_eq!(rig.log.events(), seq(&want));
    assert_eq!(rig.elapsed(), Duration::from_secs(45));

    let (rig, roll) = drained_by("np-exit-closing", vec![Ok(draining(2))], Ok(closing()));
    assert_eq!(roll.state, RollState::Done);
    let polls = ["fs:fe1"; 2].join(" ");
    let want = format!("{} fd:fe1 {polls} {THEN_A}", asks("fe2", WINDOW));
    assert_eq!(rig.log.events(), seq(&want));
    assert_eq!(rig.elapsed(), Duration::from_secs(35));
}

/// Asserts that a member whose operator API refuses the drain's connection has exited or was
/// left stopped by an earlier roll, and has nothing to drain: the turn goes on from the drain as
/// for a member that exits at once, with the install, the restart and the wait for readiness.
#[test]
fn a_member_that_refuses_the_connection_of_the_drain_has_nothing_to_drain() {
    let rig = member_rig("np-drain-gone");
    rig.front.script_member_drain("fe1", vec![], Err(gone()));
    let u = open_over(&rig);
    let roll = run(&u, only("fe1"));
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(rig.log.events(), seq(&turn_a()));
}

/// Asserts that a drain the member answers with a failure fails the turn with `drain_failed`
/// and the member's words, before the wait for its exit, and that it is not said to be left
/// draining, as it may not be.
#[test]
fn a_drain_that_fails_fails_the_turn_without_saying_the_member_is_left_draining() {
    let rig = member_rig("np-drain-refused");
    rig.front.script_member_drain("fe1", vec![], Err(no()));
    let u = open_over(&rig);
    let roll = run(&u, only("fe1"));
    assert_eq!(roll.state, RollState::Failed);
    let failure = roll.failure.expect("failure");
    assert_eq!(failure.code, ErrorCode::DrainFailed);
    let detail = &failure.detail;
    let said = "the member would not drain: no";
    assert!(detail.contains(said), "{detail}");
    assert!(!detail.contains("left draining"), "{detail}");
    let want = format!("{} fd:fe1", asks("fe2", WINDOW));
    assert_eq!(rig.log.events(), seq(&want));
}

/// Asserts that a drain the member does not answer (a timeout) fails the turn with
/// `drain_failed`, the member's words and the member named as possibly draining, because the
/// drain may have begun, before the wait for its exit and before any install or restart.
#[test]
fn a_drain_that_is_not_answered_fails_the_turn_naming_the_member_as_possibly_draining() {
    let rig = member_rig("np-drain-silent");
    let silent = PortError::unreachable("timed out");
    rig.front.script_member_drain("fe1", vec![], Err(silent));
    let u = open_over(&rig);
    let roll = run(&u, only("fe1"));
    assert_eq!(roll.state, RollState::Failed);
    let failure = roll.failure.expect("failure");
    assert_eq!(failure.code, ErrorCode::DrainFailed);
    let detail = &failure.detail;
    let said = "the member did not answer the drain: unreachable: timed out";
    assert!(detail.contains(said), "{detail}");
    let note = "frontend member a (fe1) may be draining";
    assert!(detail.contains(note), "{detail}");
    let want = format!("{} fd:fe1", asks("fe2", WINDOW));
    assert_eq!(rig.log.events(), seq(&want));
}

/// Asserts that a member that answers the drain status as not draining, though it was told to
/// drain, fails the turn at its first read with `drain_failed` and says so, and is not waited
/// for until the bound; it is not said to be left draining, and nothing is installed or
/// restarted.
#[test]
fn a_member_that_answers_that_it_is_not_draining_fails_the_turn_at_once() {
    let (rig, roll) = drained_by("np-idle", vec![], Ok(idle()));
    assert_eq!(roll.state, RollState::Failed);
    let failure = roll.failure.expect("failure");
    assert_eq!(failure.code, ErrorCode::DrainFailed);
    let detail = &failure.detail;
    let said = "member a answered the drain status but is not draining";
    assert!(detail.contains(said), "{detail}");
    assert!(!detail.contains("left draining"), "{detail}");
    let want = format!("{} fd:fe1 fs:fe1", asks("fe2", WINDOW));
    assert_eq!(rig.log.events(), seq(&want));
    assert_eq!(rig.elapsed(), Duration::from_secs(30));
}

/// A named setup of the rig, the code the roll over every host then fails with at the first
/// member, and the words its detail has.
struct Case {
    name: &'static str,
    code: ErrorCode,
    text: &'static str,
    setup: fn(&Rig),
}

/// Every way a member's turn fails after it has exited, each with the code it fails with.
fn failures() -> Vec<Case> {
    vec![
        Case {
            name: "query-fails",
            code: ErrorCode::InstallFailed,
            text: "could not be asked what it has installed",
            setup: |rig| rig.inst.script_query("fe1", vec![Err(no())]),
        },
        Case {
            name: "install-refused",
            code: ErrorCode::InstallFailed,
            text: "no; frontend member a (fe1) is not serving",
            setup: |rig| rig.inst.script_install("fe1", vec![], Err(no())),
        },
        Case {
            name: "install-silent",
            code: ErrorCode::InstallFailed,
            text: "fe1 did not answer the install",
            setup: |rig| {
                let silent = PortError::unreachable("timeout");
                rig.inst.script_install("fe1", vec![], Err(silent));
            },
        },
        Case {
            name: "restart-fails",
            code: ErrorCode::RestartFailed,
            text: "the unit could not be restarted: no",
            setup: |rig| rig.inst.script_restart("fe1", vec![], Err(no())),
        },
        Case {
            name: "never-ready",
            code: ErrorCode::HealthTimeout,
            text: "300 s: member a is not ready",
            setup: |rig| rig.front.script_ready("fe1", vec![], Ok(false)),
        },
        Case {
            name: "ready-unreadable",
            code: ErrorCode::HealthTimeout,
            text: "300 s: no",
            setup: |rig| rig.front.script_ready("fe1", vec![], Err(no())),
        },
        Case {
            name: "wrong-build",
            code: ErrorCode::HealthTimeout,
            text: "runs build fedcba9876543210, not the target's",
            setup: |rig| {
                let other = Ok("fedcba9876543210".to_owned());
                rig.front.script_commit("fe1", vec![], other);
            },
        },
        Case {
            name: "commit-unreadable",
            code: ErrorCode::HealthTimeout,
            text: "300 s: no",
            setup: |rig| rig.front.script_commit("fe1", vec![], Err(no())),
        },
    ]
}

/// A roll over every host after `setup`, and the rig it ran on, in a directory of its own named
/// from `prefix` and `name` (tests run in parallel, and a directory is not shared).
fn failing(prefix: &str, name: &str, setup: fn(&Rig)) -> (Rig, Roll) {
    let rig = member_rig(&format!("{prefix}-{name}"));
    setup(&rig);
    let u = open_over(&rig);
    let roll = run(&u, Hosts::default());
    (rig, roll)
}

/// Asserts that each failure after the member has exited (a host that cannot be asked what it
/// has, an install that is refused or does not answer, a restart that fails, a member that is
/// never ready, is on another build, or cannot be asked) fails the roll at that member with its
/// code and the words of the cause, names the member as not serving, leaves the later member
/// pending and untouched but for the asks of its readiness.
#[test]
fn each_failure_after_the_exit_stops_the_roll_with_the_member_named_as_not_serving() {
    for case in failures() {
        let (rig, roll) = failing("np-fail", case.name, case.setup);
        assert_eq!(roll.state, RollState::Failed, "{}", case.name);
        let failure = roll.failure.clone().expect("failure");
        let label = format!("{}: {}", case.name, failure.detail);
        assert_eq!(failure.alias, "fe1", "{label}");
        assert_eq!(failure.code, case.code, "{label}");
        assert!(failure.detail.contains(case.text), "{label}");
        let note = "frontend member a (fe1) is not serving";
        assert!(failure.detail.contains(note), "{label}");
        assert_eq!(phases(&roll), [Phase::Failed, Phase::Pending], "{label}");
        let touched = rig.log.touching("fe2");
        let ready = Event::Ready("fe2".to_owned());
        assert!(touched.iter().all(|e| *e == ready), "{label}");
    }
}

/// Asserts the bound of the wait for readiness: a member that is never ready fails after the
/// frontend health bound of 300 s, which begins at the restart, so 330 s from the first ask of
/// the other member, with the reason it was not ready.
#[test]
fn a_member_that_is_never_ready_fails_after_the_window_and_the_health_bound() {
    let list = failures();
    let case = list.iter().find(|c| c.name == "never-ready");
    let case = case.expect("case");
    let (rig, roll) = failing("np-bound", case.name, case.setup);
    let detail = roll.failure.expect("failure").detail;
    let reason = "300 s: member a is not ready;";
    assert!(detail.starts_with(reason), "{detail}");
    let t = Timing::default();
    assert_eq!(rig.elapsed(), t.peer_ready + t.healthy_frontend);
}

/// Asserts that a version that names no commit cannot be shown to be running: the member is
/// restarted, and the roll fails at it with `swap_abandoned` and the member named as not
/// serving.
#[test]
fn a_version_that_names_no_commit_fails_the_member_after_its_restart() {
    let rig = member_rig("np-no-commit");
    let u = open_over(&rig);
    let req = request("2.0", Hosts::default(), "k");
    let started = u.start(&req).expect("start");
    let roll = u.drive(&started.roll.id).expect("drive");
    let failure = roll.failure.expect("failure");
    assert_eq!(failure.code, ErrorCode::SwapAbandoned);
    let detail = &failure.detail;
    assert!(detail.contains("names no commit"), "{detail}");
    assert!(detail.contains("is not serving"), "{detail}");
    let restarted = Event::Restart("fe1".to_owned(), "a".to_owned());
    assert!(rig.log.events().contains(&restarted));
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

/// A phase a record can hold `fe1` in, what the host and the member look like then, and the
/// calls the resumed turn starts with.
struct Resume {
    from: Phase,
    /// The host has the target installed already.
    installed: bool,
    /// The member still has an execution open in its drain when the updater is back.
    open: bool,
    expect: &'static str,
}

/// Asserts that an updater started over a record that holds the member in each phase goes on
/// from that phase and not before it: in `draining` the member's drain is read, it is told to
/// drain again and its exit is waited for, with the other member not asked while it is draining
/// or has exited; in `installing` the host is asked what it
/// has and installed on only if it does not have the target; in `restarting` and
/// `waiting_healthy` the member is not told to drain, which would take down the new process;
/// and the roll finishes with both members done.
#[test]
fn a_restart_of_the_updater_in_each_phase_resumes_from_that_phase() {
    let resumes = [
        Resume {
            from: Phase::Draining,
            installed: false,
            open: true,
            expect: "fs:fe1 fd:fe1 fs:fe1 fs:fe1 qy:fe1 in:fe1 rn:fe1,a rd:fe1 cm:fe1",
        },
        Resume {
            from: Phase::Draining,
            installed: false,
            open: false,
            expect: "fs:fe1 fd:fe1 fs:fe1 qy:fe1 in:fe1 rn:fe1,a rd:fe1 cm:fe1",
        },
        Resume {
            from: Phase::Installing,
            installed: false,
            open: false,
            expect: "qy:fe1 in:fe1 rn:fe1,a rd:fe1 cm:fe1",
        },
        Resume {
            from: Phase::Installing,
            installed: true,
            open: false,
            expect: "qy:fe1 rn:fe1,a rd:fe1 cm:fe1",
        },
        Resume {
            from: Phase::Restarting,
            installed: true,
            open: false,
            expect: "rn:fe1,a rd:fe1 cm:fe1",
        },
        Resume {
            from: Phase::WaitingHealthy,
            installed: true,
            open: false,
            expect: "rd:fe1 cm:fe1",
        },
    ];
    for (i, resume) in resumes.iter().enumerate() {
        let rig = member_rig(&format!("np-resume-{i}"));
        if resume.installed {
            let target = Installed::Version(TARGET.to_owned());
            rig.inst.set_installed("fe1", target);
        }
        if resume.open {
            let open = vec![Ok(draining(1)), Ok(draining(1))];
            rig.front.script_drain_status("fe1", open, Err(gone()));
        }
        rig.store().save(&record(resume.from)).expect("save");
        let u = open_over(&rig);
        let roll = u.drive("roll-1").expect("drive");
        let label = format!("{i} {:?}", resume.from);
        assert_eq!(roll.state, RollState::Done, "{label}");
        let want = seq(resume.expect);
        let got = rig.log.events();
        assert_eq!(got[..want.len()], want[..], "{label}");
        let told = got.contains(&ev("fd:fe1"));
        assert_eq!(told, resume.from == Phase::Draining, "{label}");
        assert_eq!(rig.store().load().expect("l").expect("r").resumes, 1);
        assert_eq!(phases(&roll), [Phase::Done, Phase::Done], "{label}");
    }
}

/// Asserts that a member found in `draining` whose drain status says it is not draining (the
/// updater stopped after it wrote the record and before it sent the drain) is checked against the
/// other members like a turn that has not begun: when the other member is not ready for the whole
/// wait the turn fails with `unsafe_to_stop` and the member is never told to drain, and when it
/// is ready the turn goes on with the drain, the exit, the install and the restart.
#[test]
fn a_member_found_draining_that_was_never_told_to_drain_waits_for_the_others() {
    let rig = member_rig("np-undrained-unready");
    let first = vec![Ok(idle())];
    rig.front.script_drain_status("fe1", first, Err(gone()));
    rig.front.script_ready("fe2", vec![], Ok(false));
    rig.store().save(&record(Phase::Draining)).expect("save");
    let u = open_over(&rig);
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Failed);
    let failure = roll.failure.expect("failure");
    assert_eq!(failure.code, ErrorCode::UnsafeToStop);
    let events = rig.log.events();
    assert_eq!(events[0], ev("fs:fe1"));
    let asked = ev("rd:fe2");
    assert!(events[1..].iter().all(|e| *e == asked), "{events:?}");

    let rig = member_rig("np-undrained-ready");
    let first = vec![Ok(idle())];
    rig.front.script_drain_status("fe1", first, Err(gone()));
    rig.store().save(&record(Phase::Draining)).expect("save");
    let u = open_over(&rig);
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done);
    let text = format!("fs:fe1 {} fd:fe1 fs:fe1 {THEN_A}", asks("fe2", WINDOW));
    let want = seq(&text);
    let got = rig.log.events();
    assert_eq!(got[..want.len()], want[..]);
}

/// Asserts that a record that holds the member in a phase only a turn at a proxy has (`retiring`
/// or `undraining`) fails the turn with `swap_abandoned`, telling the operator to abort the
/// roll, before any call is made: an earlier build began the roll at a proxy, and the turn is not
/// guessed at.
#[test]
fn a_record_in_a_phase_of_a_turn_at_a_proxy_fails_the_turn_before_any_call() {
    let found = [Phase::Retiring, Phase::Undraining];
    for (i, phase) in found.into_iter().enumerate() {
        let rig = member_rig(&format!("np-proxy-phase-{i}"));
        rig.store().save(&record(phase)).expect("save");
        let u = open_over(&rig);
        let roll = u.drive("roll-1").expect("drive");
        assert_eq!(roll.state, RollState::Failed, "{phase:?}");
        let failure = roll.failure.expect("failure");
        assert_eq!(failure.code, ErrorCode::SwapAbandoned, "{phase:?}");
        let detail = &failure.detail;
        assert!(detail.contains("abort the roll"), "{detail}");
        assert!(rig.log.events().is_empty(), "{phase:?}");
    }
}

/// Asserts that a member found `installing` after a restart of the updater, whose host reports
/// the install begun before the restart as still in progress, waits for it one poll apart and
/// makes no second install beside it: the host reports the target on the next ask, and the turn
/// goes on to the restart and the wait for readiness without a drain, and the second member is
/// then rolled as its turn goes.
#[test]
fn an_install_still_in_progress_after_a_restart_is_waited_for_and_not_repeated() {
    let rig = member_rig("np-install-wait");
    let target = Installed::Version(TARGET.to_owned());
    rig.inst.set_installed("fe1", target);
    rig.inst
        .script_query("fe1", vec![Ok(Installed::InProgress)]);
    rig.store().save(&record(Phase::Installing)).expect("save");
    let u = open_over(&rig);
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done);
    let want = seq("qy:fe1 qy:fe1 rn:fe1,a rd:fe1 cm:fe1");
    assert_eq!(rig.log.events()[..want.len()], want[..]);
    assert!(rig.log.installs().is_empty());
    assert!(!rig.log.events().contains(&ev("fd:fe1")));
    // One poll of the install, then the window the second member's turn waits.
    assert_eq!(rig.elapsed(), Duration::from_secs(5 + 30));
}

/// Asserts that an install that stays in progress fails the turn at the member when the install
/// bound is out (900 s after the window of the other member's readiness), with `install_failed`,
/// the host and the bound in the detail and the member named as not serving, that no install is
/// made beside it, and that the unit is not restarted.
#[test]
fn an_install_that_stays_in_progress_fails_the_turn_at_the_install_bound() {
    let rig = member_rig("np-install-stuck");
    rig.inst.set_installed("fe1", Installed::InProgress);
    let u = open_over(&rig);
    let roll = run(&u, only("fe1"));
    assert_eq!(roll.state, RollState::Failed);
    let failure = roll.failure.expect("failure");
    assert_eq!(failure.code, ErrorCode::InstallFailed);
    let detail = &failure.detail;
    let stuck = "an install on fe1 was still in progress after 900 s";
    assert!(detail.contains(stuck), "{detail}");
    assert!(detail.contains("is not serving"), "{detail}");
    let t = Timing::default();
    assert_eq!(rig.elapsed(), t.peer_ready + t.install_bound);
    assert!(rig.log.installs().is_empty());
    assert!(!rig.log.events().contains(&ev("rn:fe1,a")));
}

/// Asserts that with a storage host in the inventory, listed after the two members, the members
/// are rolled first, each as its turn goes, and the storage host after them as a host that is
/// not a member: safe-to-stop, install and health, with the storage host's install the second of
/// the roll.
#[test]
fn members_are_rolled_before_the_other_hosts() {
    let rig = member_rig("np-then-storage");
    // The storage host is listed before the members on purpose: only the roll's own order
    // puts the members first.
    let inv = Inventory::parse(&format!(
        "st1 st1.farm.example storage {KEY}\n\
         fe1 fe.farm.example frontend {KEY} instance=a base=18000\n\
         fe2 fe.farm.example frontend {KEY} instance=b base=18010\n"
    ))
    .expect("inventory");
    let u = rig.open_inventory(inv, rig.store());
    let roll = run(&u, Hosts::default());
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(phases(&roll), [Phase::Done, Phase::Done, Phase::Done]);
    assert_eq!(rig.log.installs(), ["fe1", "st1"]);
    let want = [turn_a(), turn_b(), "ck:st1 in:st1 hl:st1".to_owned()].join(" ");
    assert_eq!(rig.log.events(), seq(&want));
}

/// An old record, as an earlier build wrote it: a host that is done and a host `waiting_healthy`.
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

/// Asserts that a record an earlier build wrote still parses and checks with no format raise,
/// that the roll resumes on the member in `waiting_healthy` with no drain, install or restart
/// (only its readiness and commit are asked), and that the phases of a member's turn are written
/// as snake case words.
#[test]
fn an_old_roll_record_still_parses_and_resumes() {
    let rec: Record = serde_json::from_str(OLD_RECORD).expect("old record");
    assert_eq!(rec.format, FORMAT);
    assert_eq!(rec.check(), Ok(()));
    let rig = member_rig("np-old-record");
    std::fs::write(rig.dir.join("roll.json"), OLD_RECORD).expect("write");
    let loaded = rig.store().load().expect("load").expect("record");
    assert_eq!(loaded, rec);
    let u = open_over(&rig);
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(rig.log.events(), seq("rd:fe2 cm:fe2"));
    let words = [
        (Phase::Draining, "\"draining\""),
        (Phase::Installing, "\"installing\""),
        (Phase::Restarting, "\"restarting\""),
        (Phase::WaitingHealthy, "\"waiting_healthy\""),
    ];
    for (phase, word) in words {
        assert_eq!(serde_json::to_string(&phase).expect("json"), word);
    }
}
