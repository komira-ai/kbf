//! The cold start on the in-memory world: one ordered log behind every port, a virtual
//! clock, so each test can say what was called, in which order and how long apart.

use std::cell::{Cell, RefCell};

use super::*;
use crate::proxy::cold::ReadyRule;
use crate::proxy::fakes::{Script, Takeover, World, secs};
use crate::proxy::model::{Start, Timing};
use crate::proxy::ports::RootOrders as _;
use crate::testkit::{A, c};

struct Src<'a> {
    world: &'a World,
    unrunnable: Cell<Option<Reason>>,
}

impl Source for Src<'_> {
    fn installed(&self) -> Result<Option<CommitId>, PortError> {
        self.world.installed()
    }

    fn runnable(&self, _release: &CommitId) -> Result<(), Reason> {
        self.unrunnable.get().map_or(Ok(()), Err)
    }
}

#[derive(Default)]
struct Notes(RefCell<Vec<(Progress, Option<String>)>>);

impl Report for Notes {
    fn progress(&self, progress: &Progress, detail: Option<&str>) {
        self.0
            .borrow_mut()
            .push((*progress, detail.map(str::to_owned)));
    }
}

impl Notes {
    fn states(&self) -> Vec<&'static str> {
        self.0.borrow().iter().map(|(p, _)| p.state).collect()
    }

    fn last(&self) -> Progress {
        self.0.borrow().last().expect("a report").0
    }

    fn waits(&self) -> Vec<u64> {
        self.0
            .borrow()
            .iter()
            .filter_map(|(p, _)| p.next_try_s)
            .collect()
    }
}

fn retry(attempts: u32) -> Retry {
    Retry {
        attempts,
        first_wait: secs(2),
        max_wait: secs(30),
    }
}

struct Run {
    world: World,
    notes: Notes,
    src_reason: Cell<Option<Reason>>,
}

impl Run {
    fn new() -> Run {
        Run {
            world: World::new(),
            notes: Notes::default(),
            src_reason: Cell::new(None),
        }
    }

    fn boot(&self, retry: Retry) -> Booted {
        let w = &self.world;
        let src = Src {
            world: w,
            unrunnable: Cell::new(self.src_reason.get()),
        };
        let keep = || {
            w.log_event("hold kept");
            true
        };
        ColdBoot {
            cold: Cold {
                child: w,
                admin: w,
                gate: w,
                clock: w,
                timing: Timing::frontend(),
                ready: ReadyRule::Readyz,
            },
            source: &src,
            migrator: w,
            report: &self.notes,
            retry,
            hold: &keep,
        }
        .run()
    }
}

/// Catches the wrong start: the installed release runs in slot A as a candidate, the hold is
/// taken before the process exists, the generation is taken before the streams are released
/// into the slot, and it is released into the slot A.
#[test]
fn a_cold_start_runs_the_installed_release_as_a_candidate_and_releases_after_the_takeover() {
    let run = Run::new();
    assert_eq!(run.boot(retry(3)), Booted::Serving(Slot::A));
    let w = &run.world;
    assert_eq!(w.st.borrow().starts, vec![(Slot::A, Start::Candidate)]);
    let at = |prefix: &str| {
        w.pos(prefix)
            .unwrap_or_else(|| panic!("no {prefix}: {:?}", w.log()))
    };
    assert!(at("hold kept") < at("migrate"), "{:?}", w.log());
    assert!(at("migrate") < at("spawn"), "{:?}", w.log());
    assert!(at("spawn") < at("admin:takeover"), "{:?}", w.log());
    assert!(at("admin:takeover") < at("release_into A"), "{:?}", w.log());
    assert_eq!(w.count("spawn"), 1);
    assert_eq!(run.notes.states(), ["starting", "serving"]);
    assert_eq!(w.st.borrow().procs[0].commit, c(A));
}

/// Catches a boot that asks for the three-part readiness: the proxy's admin client answers
/// `/readyz` only, so a child whose health and capabilities probes read false (they always do
/// today) must still be started.
#[test]
fn a_child_is_ready_when_its_readyz_says_so() {
    let run = Run::new();
    run.world.st.borrow_mut().ready_gap = Some(1);
    assert_eq!(run.boot(retry(3)), Booted::Serving(Slot::A));
    let run = Run::new();
    run.world.st.borrow_mut().ready_gap = Some(0);
    assert_eq!(run.boot(retry(2)), Booted::GaveUp(Reason::NotReady));
}

/// Catches starting what nobody named: with no `installed_commit`, an unreadable
/// `status.json` or a release that may not run, nothing is spawned or migrated, and the
/// reason carries the fix when it is the operator's.
#[test]
fn a_release_that_is_not_named_or_may_not_run_is_never_started() {
    let table = [
        (Reason::NoInstalledRelease, true, false),
        (Reason::StatusUnreadable, false, true),
        (Reason::BinaryMissing, false, false),
        (Reason::BinaryNotExecutable, false, false),
        (Reason::NotVerified, false, false),
    ];
    for (reason, none, err) in table {
        let run = Run::new();
        {
            let mut st = run.world.st.borrow_mut();
            if none {
                st.installed = None;
            }
            st.installed_err = err;
        }
        if !none && !err {
            run.src_reason.set(Some(reason));
        }
        assert_eq!(run.boot(retry(2)), Booted::GaveUp(reason), "{reason:?}");
        let w = &run.world;
        assert_eq!(w.count("spawn"), 0, "{reason:?}: {:?}", w.log());
        assert_eq!(w.count("migrate"), 0, "{reason:?}");
        assert_eq!(w.count("release_into"), 0, "{reason:?}");
        let last = run.notes.last();
        assert_eq!(last.state, "gave-up");
        assert_eq!(last.reason, Some(reason.word()));
        assert_eq!(last.fix.is_some(), reason.fix().is_some());
        assert!(
            reason.fix().is_some(),
            "{reason:?} is the operator's to fix"
        );
    }
}

/// Catches a retry that loses the error: the child exits before it is ready, the boot waits
/// the first wait, starts a second process, and serves from that one. The reasons are in
/// the report, in order.
#[test]
fn a_child_that_exits_is_started_again_after_the_wait_and_then_serves() {
    let run = Run::new();
    run.world.st.borrow_mut().scripts.push_back(Script {
        ready_after: None,
        exit_after: Some(secs(1)),
    });
    assert_eq!(run.boot(retry(3)), Booted::Serving(Slot::A));
    let w = &run.world;
    assert_eq!(w.count("spawn"), 2, "{:?}", w.log());
    assert_eq!(
        run.notes.states(),
        ["starting", "retrying", "starting", "serving"]
    );
    let reasons: Vec<_> = run
        .notes
        .0
        .borrow()
        .iter()
        .filter_map(|(p, _)| p.reason)
        .collect();
    assert_eq!(reasons, ["exited-before-ready"]);
    assert_eq!(run.notes.waits(), [2]);
    // 1 s for the first process to be seen gone, 2 s of wait, 2 s of the second's readiness.
    assert_eq!(w.elapsed(), secs(1 + 2 + 2), "{:?}", w.log());
}

/// Catches an endless loop and a lost last reason: the attempts are spent, the waits double
/// and stop at the cap, nothing is released, and each process that never answered was
/// killed (unserved) before the next was started.
#[test]
fn the_boot_gives_up_after_the_cap_with_the_last_reason_and_the_streams_held() {
    let run = Run::new();
    {
        let mut st = run.world.st.borrow_mut();
        for _ in 0..6 {
            st.scripts.push_back(Script {
                ready_after: None,
                exit_after: None,
            });
        }
    }
    let rule = Retry {
        attempts: 6,
        first_wait: secs(2),
        max_wait: secs(5),
    };
    assert_eq!(run.boot(rule), Booted::GaveUp(Reason::NotReady));
    let w = &run.world;
    assert_eq!(w.count("spawn"), 6, "no seventh try: {:?}", w.log());
    assert_eq!(w.count("kill pid"), 6, "{:?}", w.log());
    assert_eq!(w.count("terminate"), 0, "{:?}", w.log());
    assert_eq!(w.count("release_into"), 0, "the streams stay held");
    assert_eq!(run.notes.waits(), [2, 4, 5, 5, 5]);
    let last = run.notes.last();
    assert_eq!((last.state, last.attempt, last.attempts), ("gave-up", 6, 6));
    assert_eq!(last.reason, Some("not-ready"));
    assert_eq!(last.next_try_s, None);
    for pair in w.st.borrow().procs.windows(2) {
        assert!(!pair[0].alive, "the first process is stopped first");
    }
}

/// Catches SIGKILL for a process a takeover reached: it may have taken the generation, so it
/// is asked to stop, never killed; and the refusal is the reason.
#[test]
fn a_takeover_that_fails_ends_the_process_with_sigterm_not_sigkill() {
    let run = Run::new();
    run.world.st.borrow_mut().takeover = Takeover::Err;
    assert_eq!(run.boot(retry(2)), Booted::GaveUp(Reason::TakeoverFailed));
    let w = &run.world;
    assert_eq!(w.count("terminate"), 2, "{:?}", w.log());
    assert_eq!(w.count("kill pid"), 0, "{:?}", w.log());
    assert_eq!(w.count("release_into"), 0);
}

/// Catches the next try starting while the SIGTERM'd process of the last one is still
/// exiting: it takes three seconds to exit, longer than the two-second wait between the
/// tries, and the second try must still find its slot free (not "spawn-failed"). The
/// process is stopped for the boot's purposes when it is gone, not when it is told to go.
#[test]
fn a_try_does_not_start_before_the_process_of_the_last_one_is_gone() {
    let run = Run::new();
    {
        let mut st = run.world.st.borrow_mut();
        st.takeover = Takeover::Err;
        st.term_takes = secs(3);
    }
    assert_eq!(run.boot(retry(3)), Booted::GaveUp(Reason::TakeoverFailed));
    let w = &run.world;
    assert_eq!(w.count("spawn A refused"), 0, "{:?}", w.log());
    assert_eq!(w.count("spawn A "), 3, "{:?}", w.log());
    let at = |prefix: &str| {
        w.pos(prefix)
            .unwrap_or_else(|| panic!("no {prefix}: {:?}", w.log()))
    };
    assert!(at("terminate") < at("await_exit"), "{:?}", w.log());
}

/// Catches a process that ignores SIGTERM stalling the boot for good: the wait for it ends
/// at its limit and the try after it is refused for the slot, with its own word.
#[test]
fn a_process_that_does_not_exit_is_waited_for_a_while_and_then_the_try_fails() {
    let run = Run::new();
    {
        let mut st = run.world.st.borrow_mut();
        st.takeover = Takeover::Err;
        st.term_takes = secs(3600);
    }
    assert_eq!(run.boot(retry(2)), Booted::GaveUp(Reason::SpawnFailed));
    let w = &run.world;
    assert_eq!(w.count("await_exit"), 1, "{:?}", w.log());
    assert_eq!(w.count("spawn A refused: live child"), 1, "{:?}", w.log());
}

/// Catches a gate that cannot route after the takeover landed: its reason is its own.
#[test]
fn a_gate_that_refuses_the_release_is_a_failed_attempt_with_its_own_word() {
    let run = Run::new();
    run.world.st.borrow_mut().release_err = true;
    assert_eq!(run.boot(retry(1)), Booted::GaveUp(Reason::ReleaseFailed));
    assert_eq!(run.notes.last().reason, Some("release-failed"));
}

/// Catches a migration that is skipped or runs after the start: it runs before the process
/// exists, and a failed one starts nothing.
#[test]
fn a_failed_migration_starts_no_child() {
    let run = Run::new();
    run.world.st.borrow_mut().migrate_err = Some("down".into());
    assert_eq!(run.boot(retry(2)), Booted::GaveUp(Reason::MigrationFailed));
    let w = &run.world;
    assert_eq!(w.count("migrate"), 2);
    assert_eq!(w.count("spawn"), 0);
    // The port's own text is for the log; the report carries the word only.
    let notes = run.notes.0.borrow();
    let last = notes.last().expect("report");
    assert_eq!(last.1.as_deref(), Some("down"));
    assert!(
        !serde_json::to_string(&last.0)
            .expect("json")
            .contains("down")
    );
}

/// Catches a spawn that fails once and ends the boot: the second try goes through.
#[test]
fn a_spawn_that_fails_is_tried_again() {
    let run = Run::new();
    run.world.st.borrow_mut().spawn_fail_nth.push((Slot::A, 1));
    assert_eq!(run.boot(retry(3)), Booted::Serving(Slot::A));
    assert_eq!(run.notes.waits(), [2]);
    assert_eq!(run.notes.0.borrow()[1].0.reason, Some("spawn-failed"));
}

/// Catches a boot that outlives the proxy: a stop during the wait between tries ends it
/// with nothing more started and nothing signalled.
#[test]
fn a_shutdown_stops_the_boot_between_tries() {
    let run = Run::new();
    run.world.st.borrow_mut().installed = None;
    run.world.set_limit(secs(1));
    assert_eq!(run.boot(retry(5)), Booted::Stopped);
    let w = &run.world;
    assert_eq!(w.count("spawn"), 0);
    assert_eq!(w.count("terminate") + w.count("kill pid"), 0);
}

/// Catches a drift in the words: they are what an operator and a script match on.
#[test]
fn the_reason_words_are_fixed() {
    let words: Vec<_> = [
        Reason::NoInstalledRelease,
        Reason::StatusUnreadable,
        Reason::BinaryMissing,
        Reason::BinaryNotExecutable,
        Reason::NotVerified,
        Reason::MigrationFailed,
        Reason::SpawnFailed,
        Reason::ExitedBeforeReady,
        Reason::NotReady,
        Reason::TakeoverFailed,
        Reason::ReleaseFailed,
    ]
    .iter()
    .map(|r| r.word())
    .collect();
    assert_eq!(
        words,
        [
            "no-installed-release",
            "status-unreadable",
            "binary-missing",
            "binary-not-executable",
            "release-not-verified",
            "migration-failed",
            "spawn-failed",
            "exited-before-ready",
            "not-ready",
            "takeover-failed",
            "release-failed",
        ]
    );
}

/// Catches a report nobody can read: `boot.json` is the last progress, whole, and carries
/// the word and the fix.
#[test]
fn the_file_report_replaces_boot_json_with_the_last_progress() {
    let dir = kbf_host::testkit::scratch("proxy-boot-report");
    let report = FileReport::new(&dir);
    let mut progress = Progress {
        state: "retrying",
        attempt: 1,
        attempts: 10,
        reason: Some(Reason::NotVerified.word()),
        fix: Reason::NotVerified.fix(),
        next_try_s: Some(2),
    };
    report.progress(&progress, Some("/var/lib/x"));
    progress.state = "gave-up";
    progress.attempt = 10;
    progress.next_try_s = None;
    report.progress(&progress, None);
    let text = std::fs::read_to_string(dir.join(BOOT_FILE)).expect("boot.json");
    let json: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(json["state"], "gave-up");
    assert_eq!(json["attempt"], 10);
    assert_eq!(json["reason"], "release-not-verified");
    assert!(json["fix"].as_str().is_some_and(|f| f.contains("VERIFIED")));
    assert!(!text.contains("/var/lib/x"), "the detail is for the log");
    assert!(!dir.join("boot.json.tmp").exists());
}

/// Catches a changed production schedule (the tests use their own `retry()`): `Retry::boot()`
/// is ten attempts, the first wait 2 s, doubling and capped at 30 s, and the boot run with it
/// reports exactly that sequence of waits between its ten attempts.
#[test]
fn the_production_retry_is_ten_attempts_from_two_seconds_doubling_to_thirty() {
    let boot = Retry::boot();
    assert_eq!(boot.attempts, 10);
    assert_eq!(boot.first_wait, secs(2));
    assert_eq!(boot.max_wait, secs(30));
    let run = Run::new();
    run.world.st.borrow_mut().installed = None;
    assert_eq!(run.boot(boot), Booted::GaveUp(Reason::NoInstalledRelease));
    assert_eq!(run.notes.waits(), [2, 4, 8, 16, 30, 30, 30, 30, 30]);
    assert_eq!(run.notes.last().attempt, 10);
    assert_eq!(run.notes.last().attempts, 10);
}

/// Catches a boot that outlives the proxy while a child is being waited for: a stop during the
/// wait for ready ends the boot as stopped (not as a failed try that is retried), with the one
/// process it started left as it is, nothing signalled and no second try.
#[test]
fn a_shutdown_while_waiting_for_ready_stops_the_boot_and_leaves_the_child() {
    let run = Run::new();
    run.world.script(Script {
        ready_after: None,
        exit_after: None,
    });
    run.world.set_limit(secs(0));
    assert_eq!(run.boot(retry(5)), Booted::Stopped);
    let w = &run.world;
    assert_eq!(w.count("spawn"), 1);
    assert_eq!(w.count("terminate") + w.count("kill pid"), 0);
    assert_eq!(run.notes.states(), ["starting"]);
}

/// Catches a report that stops the boot when it cannot write, or says nothing: with a directory
/// that is not there, a progress with no reason, fix or wait leaves no file and does not panic.
#[test]
fn a_report_that_cannot_write_boot_json_goes_on() {
    let dir = kbf_host::testkit::scratch("proxy-boot-report-missing").join("absent");
    let report = FileReport::new(&dir);
    report.progress(
        &Progress {
            state: "starting",
            attempt: 1,
            attempts: 3,
            reason: None,
            fix: None,
            next_try_s: None,
        },
        None,
    );
    assert!(!dir.join(BOOT_FILE).exists());
}
