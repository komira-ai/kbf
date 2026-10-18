use super::*;
use kbf_host::testkit::{FakeExec, reply};

fn said(word: &'static str) -> FakeExec {
    let mut exec = FakeExec::new(0);
    exec.answer = Box::new(move |_| Ok(reply(0, &format!("{word}\n"), "")));
    exec
}

/// Catches an `ActiveState` word read as the wrong state: `active` runs; `inactive` and
/// `failed` are free to start; every other word (`activating`, `deactivating`, `reloading`,
/// one this does not know) is a unit in motion, which is never started over or counted as
/// stopped.
///
/// MUTANT-TODO: `failed` read as changing (a crashed instance could never be started).
#[test]
fn an_active_state_word_is_read_as_running_stopped_or_changing() {
    for (word, want) in [
        ("active", UnitState::Active),
        ("inactive", UnitState::Stopped),
        ("failed", UnitState::Stopped),
        ("activating", UnitState::Changing("activating".to_owned())),
        (
            "deactivating",
            UnitState::Changing("deactivating".to_owned()),
        ),
        ("reloading", UnitState::Changing("reloading".to_owned())),
        ("maintenance", UnitState::Changing("maintenance".to_owned())),
    ] {
        assert_eq!(
            state(&said(word), Instance::A).expect("reads"),
            want,
            "{word}"
        );
    }
}

/// Catches a wrong unit or property being asked: the question is `systemctl show` of the
/// instance's own unit, for `ActiveState` alone, and a tool that exits non-zero is an error,
/// not a state.
#[test]
fn the_state_is_asked_of_the_instances_own_unit_and_a_failing_tool_is_an_error() {
    let exec = said("active");
    state(&exec, Instance::B).expect("reads");
    let asked = exec.queries.borrow();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].program.as_str(), SYSTEMCTL);
    assert_eq!(
        asked[0].args,
        [
            "show",
            "--property=ActiveState",
            "--value",
            "--",
            "kbf-frontend@b.service"
        ]
    );
    let mut broken = FakeExec::new(0);
    broken.answer = Box::new(|_| Ok(reply(1, "", "no bus")));
    assert!(state(&broken, Instance::A).is_err());
}

/// Catches a stop that blocks (a draining instance has `TimeoutStopSec=infinity`, so
/// without `--no-block` the installer would sit as long as a build runs), a start that
/// names no unit, and a unit that is not the instance's.
///
/// MUTANT-TODO: `--no-block` dropped.
#[test]
fn start_waits_for_ready_and_stop_never_blocks() {
    let mut exec = FakeExec::new(0);
    start(&mut exec, Instance::A).expect("starts");
    stop(&mut exec, Instance::B).expect("stops");
    let words: Vec<Vec<String>> = exec
        .log
        .iter()
        .map(|s| match s {
            Step::Run(cmd) => cmd.args.clone(),
            other => panic!("not a command: {other}"),
        })
        .collect();
    assert_eq!(
        words,
        [
            vec!["start", "--", "kbf-frontend@a.service"],
            vec!["stop", "--no-block", "--", "kbf-frontend@b.service"]
        ]
    );
}

/// Catches an instance's name being anything but the table's: the unit and the slot are
/// spelled from `a` and `b` only, and the other of each is the other.
#[test]
fn the_two_instances_are_each_others_other() {
    assert_eq!(Instance::A.other(), Instance::B);
    assert_eq!(Instance::B.other(), Instance::A);
    assert_eq!(Instance::A.unit(), "kbf-frontend@a.service");
    assert_eq!(Instance::B.name(), "b");
}

/// Catches the unit running through `current` (a crash restart of the serving instance
/// would then take a release that is only being tried), and the other properties the
/// handover rests on: a notify unit with a finite start timeout, no stop deadline, no
/// `Conflicts=` between the instances (starting one must never stop the other), and a
/// command line `kbf` refuses is not retried.
///
/// MUTANT-TODO: `slots/%i` replaced by `current`; `Conflicts=kbf-frontend@%i.service`
/// added; `TimeoutStopSec=90`.
#[test]
fn the_unit_runs_each_instance_through_its_own_slot_and_never_follows_current() {
    // What the unit says, not what its comments say about what it does not.
    let body: String = UNIT_FILE
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| format!("{l}\n"))
        .collect();
    let unit = body.as_str();
    let line = |key: &str| {
        unit.lines()
            .find(|l| l.starts_with(key))
            .unwrap_or_else(|| panic!("no {key}"))
    };
    let exec = line("ExecStart=");
    assert!(
        exec.starts_with("ExecStart=/usr/local/kbf/slots/%i/kbf "),
        "{exec}"
    );
    assert!(exec.contains("--mode=frontend --instance=%i"), "{exec}");
    assert!(
        !unit.contains("current"),
        "an instance must not follow current"
    );
    assert!(!unit.contains("Conflicts="));
    assert_eq!(line("Type="), "Type=notify");
    assert_eq!(line("TimeoutStartSec="), "TimeoutStartSec=120");
    assert_eq!(line("TimeoutStopSec="), "TimeoutStopSec=infinity");
    assert_eq!(
        line("RestartPreventExitStatus="),
        "RestartPreventExitStatus=2"
    );
    assert_eq!(
        line("RuntimeDirectoryPreserve="),
        "RuntimeDirectoryPreserve=yes"
    );
}
