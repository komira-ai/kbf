//! Tests of `image prime` and of `image setup --guest-user`, on a fake system: the guest's
//! frames are scripted, the VM is a fake. The clocks are the instant ones; a deadline is a
//! few milliseconds, and no test asserts how long anything took.

use std::path::Path;

use clap::Parser;
use kbf_guest::mkuser::{MkUserSummary, SHUTDOWN_NAME, SUMMARY_NAME, plan, step_frame_name};
use kbf_guest::sim::{Status, StepFrame, Who};
use serde_json::Value;

use crate::bundle::{Recorded, record_sizing};
use crate::cli::{Cli, Cmd, ImageCmd};
use crate::fake::{FakeSystem, Script, ctx, holds, tmp, work_with_base};
use crate::guest::{self, MKUSER_LABEL, mkuser_payload};
use crate::image::{SetupArgs, setup};
use crate::prime::{DEFAULT_DEADLINE, RESULTS_DIR, prime};
use crate::probe::Ctx;
use crate::report::Status as ReportStatus;
use crate::spec::Event;
use crate::system::Poll;
use kbf_guest::mkuser_prime::{RootSetup, Signal};

const LONG: std::time::Duration = std::time::Duration::from_secs(30);
const SHORT: std::time::Duration = std::time::Duration::from_millis(40);

/// A context whose work directory is the base itself, as `image prime` makes it.
fn prime_ctx<'a>(sys: &'a FakeSystem, work: &Path) -> Ctx<'a> {
    let mut c = ctx(sys, work);
    let base = work.join("base");
    c.work.clone_from(&base);
    c.base = base;
    let bin = work.join("kbf-guest-src");
    std::fs::write(&bin, "bin").expect("bin");
    c.guest_binary = Some(bin);
    c
}

fn step(index: usize, id: &str, status: Status, exit: Option<i32>) -> (String, String) {
    let f = StepFrame {
        step: id.to_owned(),
        index: index + 1,
        who: Who::Root,
        status,
        argv: vec!["/usr/bin/tool".to_owned(), id.to_owned()],
        exit,
        signal: None,
        ms: 10,
        deadline_secs: 120,
        stdout: format!("out of {id}"),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        note: None,
        skipped_because: None,
    };
    (
        step_frame_name(index, id),
        serde_json::to_string(&f).expect("json"),
    )
}

fn summary(ok: bool, already_ready: bool, n: usize) -> (String, String) {
    summary_root(
        ok,
        already_ready,
        n,
        Some(RootSetup::Done),
        Some(Signal::Yes),
    )
}

fn summary_root(
    ok: bool,
    already_ready: bool,
    n: usize,
    root_setup: Option<RootSetup>,
    signal: Option<Signal>,
) -> (String, String) {
    let s = MkUserSummary {
        complete: true,
        user: "kbfuser".to_owned(),
        steps: plan("kbfuser", false)
            .iter()
            .take(n)
            .map(|s| (s.id.clone(), Status::Ok))
            .collect(),
        ok,
        already_ready,
        created: !already_ready,
        powering_off: !already_ready,
        undelivered: Vec::new(),
        note: (!ok).then(|| "a step failed".to_owned()),
        root_setup,
        signal,
    };
    (
        SUMMARY_NAME.to_owned(),
        serde_json::to_string(&s).expect("json"),
    )
}

/// The frames of a run in which every step worked.
fn all_ok() -> Vec<(String, String)> {
    let ids: Vec<String> = plan("kbfuser", false)
        .iter()
        .map(|s| s.id.clone())
        .collect();
    let mut frames: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| step(i, id, Status::Ok, Some(0)))
        .collect();
    frames.push(summary(true, false, ids.len()));
    frames
}

/// A guest that sends `frames` as it starts and powers itself off.
fn powering_off(frames: &[(String, String)]) -> Script {
    frames
        .iter()
        .fold(Script::booted(), |s, (n, b)| s.with_guest_file(n, b))
        .then(Poll::Event(Event::Stopped { ms: 50 }))
}

/// How many steps a good run has.
fn n_steps() -> usize {
    plan("kbfuser", false).len()
}

fn results(work: &Path) -> std::path::PathBuf {
    work.join("base").join(RESULTS_DIR)
}

/// Catches a prime that starts a guest it cannot read anything from, or on a directory that
/// is not a base: without the guest binary or a base it says what is missing and starts
/// nothing.
#[test]
fn without_a_guest_binary_or_a_base_prime_says_what_is_missing_and_starts_nothing() {
    let work = work_with_base("prime-nobin");
    let sys = FakeSystem::new();
    let mut c = prime_ctx(&sys, &work);
    c.guest_binary = None;
    let r = prime(&c, LONG);
    assert_eq!(r.status, ReportStatus::NeedsManual);
    assert!(r.reason.expect("reason").contains("--guest-user"));
    let empty = tmp("prime-nobase");
    let r = prime(&prime_ctx(&sys, &empty), LONG);
    assert_eq!(r.status, ReportStatus::NeedsManual);
    assert!(sys.starts.borrow().is_empty());
}

/// Catches a prime that boots a copy of the base (the user would be made in a clone and the
/// base stay without one), boots without graphics (no Aqua session), without the vsock
/// channel or the share, collects frames anywhere but `<base>/prime-results` (which must be
/// private to the host), or sizes the guest by the defaults when the base recorded its
/// sizes.
#[test]
fn prime_boots_the_base_itself_with_graphics_and_collects_into_the_base() {
    let work = work_with_base("prime-spec");
    record_sizing(
        &work.join("base"),
        &Recorded {
            cpus: 6,
            memory_mib: 16384,
            disk_gib: 200,
        },
    )
    .expect("record");
    let sys = FakeSystem::new();
    sys.script(powering_off(&all_ok()));
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Ok, "{r:?}");
    assert!(sys.cloned.borrow().is_empty(), "no clone is made");
    let starts = sys.starts.borrow();
    assert_eq!(starts.len(), 1);
    let (spec, run_as) = &starts[0];
    assert_eq!(spec.bundle, work.join("base"));
    assert_eq!(*run_as, None);
    assert!(
        holds(&[
            spec.graphics,
            spec.vsock_ports == [5000],
            spec.automount_dir.is_some(),
            spec.results_dir.as_deref() == Some(results(&work).as_path()),
            (spec.cpus, spec.memory_mib) == (6, 16384),
        ]),
        "{spec:?}"
    );
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(results(&work))
            .expect("meta")
            .permissions(),
    );
    assert_eq!(mode & 0o777, 0o700);
    // The share is removed; the frames stay.
    assert!(!work.join("base/prime-share").exists());
    assert!(!work.join("base/prime-tagged").exists());
    assert!(results(&work).join(SUMMARY_NAME).is_file());
}

/// Catches a report that loses a step, mixes their order, drops a field of the summary, or
/// does not say the guest powered itself off and what to run next; and a base whose files
/// the run left changed (the bundle's own files must be as they were).
#[test]
fn a_good_prime_is_reported_step_by_step_and_points_at_probe_e3() {
    let work = work_with_base("prime-good");
    let sys = FakeSystem::new();
    sys.script(powering_off(&all_ok()));
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Ok, "{r:?}");
    let d = &r.details;
    assert!(
        holds(&[
            d["outcome"] == "powered-off",
            d["steps_received"] == n_steps(),
            d["steps_ok"] == n_steps(),
            d["problems"] == serde_json::json!([]),
            d["summary_received"] == true,
            d["summary"]["ok"] == true,
            d["user"] == "kbfuser",
            d["stop"]["how"] == "the guest powered itself off",
            d["results_dir"] == results(&work).to_str().expect("p"),
            d["unreadable_frames"] == serde_json::json!([]),
        ]),
        "{d}"
    );
    let ids: Vec<&str> = d["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .map(|s| s["step"].as_str().expect("s"))
        .collect();
    let want: Vec<String> = plan("kbfuser", false)
        .iter()
        .map(|s| s.id.clone())
        .collect();
    assert_eq!(ids, want.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(d["next"].as_str().expect("next").contains("probe e3"));
    let line = r.summary.expect("summary");
    assert!(
        line.contains("kbfuser") && line.contains(&format!("{0} of {0}", n_steps())),
        "{line}"
    );
    assert_eq!(
        std::fs::read_to_string(work.join("base/disk.img")).expect("disk"),
        "contents of disk.img"
    );
}

/// Catches a guest that does not power off being waited on for ever, being left running, or
/// reported as a success: the run is a timeout error, the guest was force-stopped, the
/// frames that did arrive are in the report, and the message says the base may be unflushed.
#[test]
fn a_guest_that_does_not_power_off_is_stopped_at_the_deadline_and_reported_as_a_timeout() {
    let work = work_with_base("prime-timeout");
    let sys = FakeSystem::new();
    let steps = all_ok();
    sys.script(
        Script::booted()
            .deaf()
            .with_guest_file(&steps[0].0, &steps[0].1),
    );
    let r = prime(&prime_ctx(&sys, &work), SHORT);
    assert_eq!(r.status, ReportStatus::Error, "{r:?}");
    let reason = r.reason.expect("reason");
    assert!(
        reason.starts_with("timeout:") && reason.contains("not be flushed"),
        "{reason}"
    );
    let d = &r.details;
    assert!(
        holds(&[
            d["outcome"] == "timeout",
            d["steps_received"] == 1,
            d["summary_received"] == false,
            d["stop"]["how"] == "force-stopped after the guest did not shut down",
        ]),
        "{d}"
    );
    assert!(!work.join("base/prime-share").exists());
}

/// Catches an idempotent re-run being a wait: when the guest says the user is ready it
/// stays up, so the host must shut it down at once (not wait for the deadline) and report
/// success saying so; and a guest that ignores the shutdown is still stopped.
#[test]
fn a_user_that_is_ready_already_ends_the_run_at_once_with_a_shutdown() {
    let work = work_with_base("prime-ready");
    let sys = FakeSystem::new();
    sys.script(
        Script::booted()
            .acking_shutdown()
            .with_guest_file(SUMMARY_NAME, &summary(true, true, 0).1),
    );
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Ok, "{r:?}");
    let d = &r.details;
    assert!(
        holds(&[
            d["outcome"] == "already-ready",
            d["steps_received"] == 0,
            d["stop"]["how"] == "guest shut down (vsock)",
            d["stop"]["vsock_acked"] == true,
        ]),
        "{d}"
    );
    assert!(r.summary.expect("summary").contains("ready already"));
}

/// Catches a failed step being reported as a success because the guest powered off: the
/// run is an error naming each step that did not work and how, and the steps and summary
/// are still in the report.
#[test]
fn a_summary_that_is_not_ok_is_an_error_naming_the_steps_that_failed() {
    let work = work_with_base("prime-notok");
    let sys = FakeSystem::new();
    let mut frames = vec![
        step(0, "add-user", Status::Failed, Some(70)),
        step(1, "home", Status::Skipped, None),
        step(2, "pmset", Status::Timeout, None),
        step(3, "updates-check", Status::Error, None),
    ];
    frames.push(summary(false, false, 0));
    sys.script(powering_off(&frames));
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Error);
    let reason = r.reason.expect("reason");
    for part in [
        "kbfuser",
        "add-user (exit 70)",
        "home (skipped)",
        "pmset (timeout)",
        "updates-check (error)",
    ] {
        assert!(reason.contains(part), "{part}: {reason}");
    }
    assert_eq!(r.details["outcome"], "powered-off");
    assert_eq!(r.details["steps_received"], 4);
    assert_eq!(r.details["summary"]["ok"], false);
}

/// Catches a power-off command that failed in the guest going unnoticed: the guest's
/// `mkuser-shutdown.json` frame is read, and its failure is listed among the problems.
#[test]
fn the_frame_of_a_power_off_that_failed_is_read_and_listed() {
    let work = work_with_base("prime-shutdown");
    let sys = FakeSystem::new();
    let mut frames = all_ok();
    let mut f: StepFrame =
        serde_json::from_str(&step(12, "shutdown", Status::Failed, Some(1)).1).expect("frame");
    f.exit = Some(1);
    frames.push((
        SHUTDOWN_NAME.to_owned(),
        serde_json::to_string(&f).expect("json"),
    ));
    sys.script(powering_off(&frames));
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(
        r.status,
        ReportStatus::Ok,
        "the steps worked and the VM did stop"
    );
    assert_eq!(
        r.details["problems"],
        serde_json::json!(["shutdown (exit 1)"])
    );
    assert_eq!(r.details["shutdown_frame"]["exit"], 1);
}

/// Catches a guest that stops without ever sending the summary (a base without the daemon)
/// being reported as a success, and says what to check.
#[test]
fn a_guest_that_stops_without_a_summary_is_an_error_that_names_the_setup() {
    let work = work_with_base("prime-nosummary");
    let sys = FakeSystem::new();
    sys.script(powering_off(&[]));
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Error);
    let reason = r.reason.expect("reason");
    assert!(
        reason.contains("without sending") && reason.contains("--guest-user"),
        "{reason}"
    );
    assert_eq!(r.details["summary_received"], false);
}

/// Catches a VM that ends in an error (not a power-off) being taken for one: the report
/// is an error with the framework's message, and a helper that exits 0 after the guest's
/// own shutdown still counts as a power-off.
#[test]
fn a_vm_that_ends_in_an_error_is_not_a_power_off() {
    let work = work_with_base("prime-vmerror");
    let sys = FakeSystem::new();
    sys.script(Script::failing("the disk is locked", None));
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Error);
    assert!(r.reason.expect("reason").contains("the disk is locked"));
    assert_eq!(r.details["outcome"], "vm-ended-early");

    let work = work_with_base("prime-gone-zero");
    let sys = FakeSystem::new();
    let mut script = powering_off(&all_ok());
    script.events.pop();
    sys.script(script.then(Poll::Gone {
        code: Some(0),
        signal: None,
    }));
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Ok, "{r:?}");
    assert_eq!(r.details["outcome"], "powered-off");

    let work = work_with_base("prime-gone-signal");
    let sys = FakeSystem::new();
    let mut script = powering_off(&all_ok());
    script.events.pop();
    sys.script(script.then(Poll::Gone {
        code: None,
        signal: Some(9),
    }));
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.details["outcome"], "vm-ended-early");
}

/// Catches a guest that reaches the `stopped` state (the framework's state change, not its
/// stop event) being taken for a failed boot: it is the guest's own power-off, and an
/// `error` state is not.
#[test]
fn a_vm_that_goes_to_state_stopped_is_a_power_off_and_one_in_error_is_not() {
    for (state, status, outcome) in [
        ("stopped", ReportStatus::Ok, "powered-off"),
        ("error", ReportStatus::Error, "vm-ended-early"),
    ] {
        let work = work_with_base(&format!("prime-state-{state}"));
        let sys = FakeSystem::new();
        let mut script = powering_off(&all_ok());
        script.events.pop();
        sys.script(script.then(Poll::Event(Event::State {
            state: state.into(),
            ms: 50,
        })));
        let r = prime(&prime_ctx(&sys, &work), LONG);
        assert_eq!(r.status, status, "{state}: {r:?}");
        assert_eq!(r.details["outcome"], outcome, "{state}");
    }
}

/// Catches a summary that is not ok with every step frame fine (nothing to list) losing
/// the guest's own note: the reason repeats the note, not an empty list.
#[test]
fn a_summary_that_is_not_ok_with_no_failed_step_gives_the_guests_note() {
    let work = work_with_base("prime-notok-note");
    let sys = FakeSystem::new();
    let mut frames = all_ok();
    frames.pop();
    frames.push(summary(false, false, n_steps()));
    sys.script(powering_off(&frames));
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Error, "{r:?}");
    let reason = r.reason.expect("reason");
    assert!(reason.contains("a step failed"), "{reason}");
}

/// Catches a helper that cannot be started leaving the share directories behind (the next
/// run removes them, but a failure should not litter the base), or being reported as a
/// timeout or a success.
#[test]
fn a_helper_that_cannot_start_is_an_error_and_leaves_no_share() {
    let work = work_with_base("prime-nostart");
    let sys = FakeSystem::new();
    *sys.start_error.borrow_mut() = Some("no helper".to_owned());
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Error);
    assert!(r.reason.expect("reason").contains("no helper"));
    assert!(!work.join("base/prime-share").exists());
    assert!(!work.join("base/prime-tagged").exists());
    // The priming mark is not left behind by a run that never started.
    assert!(!results(&work).join(crate::sink::PRIME_MARKER).exists());
}

/// Catches the priming mark outliving its run on a way out the normal path does not take: the
/// guard removes it when dropped, a panic's unwinding included, and a missing file is fine.
#[test]
fn the_priming_mark_is_removed_when_its_guard_is_dropped_even_by_a_panic() {
    let dir = tmp("prime-mark");
    let share = crate::guest::Share {
        dir: dir.join("s"),
        tagged: dir.join("t"),
        received: dir.to_path_buf(),
    };
    let marker = dir.join(crate::sink::PRIME_MARKER);
    let mark = crate::prime::request_prime(&share).expect("mark");
    assert!(marker.exists());
    drop(mark);
    assert!(!marker.exists());
    let r = std::panic::catch_unwind(|| {
        let _mark = crate::prime::request_prime(&share).expect("mark");
        assert!(dir.join(crate::sink::PRIME_MARKER).exists());
        panic!("a bug between the mark and the end");
    });
    assert!(r.is_err());
    assert!(!marker.exists());
    // Dropped after the file is gone already: nothing to say.
    let mark = crate::prime::request_prime(&share).expect("mark");
    std::fs::remove_file(&marker).expect("rm");
    drop(mark);
}

/// Catches the host trusting what the guest wrote: a frame that is not valid JSON must not
/// be read as a result and not stop the run; it is listed with why. Frames of the other
/// probes are ignored.
#[test]
fn frames_that_are_not_ours_or_not_valid_are_listed_or_ignored() {
    let work = work_with_base("prime-hostile");
    let sys = FakeSystem::new();
    let mut frames = all_ok();
    frames.push((step_frame_name(20, "junk"), "not json".to_owned()));
    frames.push(("sim-summary.json".to_owned(), "{}".to_owned()));
    frames.push(("other.json".to_owned(), "{}".to_owned()));
    sys.script(powering_off(&frames));
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Ok, "{r:?}");
    let bad = r.details["unreadable_frames"].as_array().expect("list");
    assert_eq!(bad.len(), 1, "{bad:?}");
    assert_eq!(bad[0]["frame"], step_frame_name(20, "junk"));
    assert!(
        bad[0]["error"]
            .as_str()
            .expect("e")
            .contains("expected JSON")
    );
    assert_eq!(r.details["steps_received"], n_steps());
}

/// Catches `image setup --guest-user` writing the wrong daemon or the wrong wish: the
/// payload is exactly the root launch daemon (once, from the guest disk, no restart, no
/// user, the name in its arguments, the host's vsock port) and the one-line wish file that
/// holds the name; a bad name writes nothing.
#[test]
fn the_user_payload_is_a_root_one_shot_daemon_and_the_wish() {
    let files = mkuser_payload("kbfuser").expect("payload");
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].path, "Library/LaunchDaemons/com.kbf.mkuser.plist");
    assert_eq!(MKUSER_LABEL, "com.kbf.mkuser");
    let plist = String::from_utf8(files[0].contents.clone()).expect("utf8");
    assert!(
        holds(&[
            plist.contains("<string>com.kbf.mkuser</string>"),
            plist.contains(&format!("<string>{}</string>", guest::BOOTSTRAP)),
            plist.contains("<string>mkuser</string>"),
            plist.contains("<string>--name</string>"),
            plist.contains("<string>kbfuser</string>"),
            plist.contains("<string>--vsock-port</string>"),
            plist.contains("<string>5000</string>"),
            plist.contains("<key>RunAtLoad</key>"),
            plist.contains("<string>/var/log/com.kbf.mkuser.log</string>"),
            !plist.contains("UserName"),
            !plist.contains("KeepAlive"),
            !plist.contains(guest::GUEST_SHARE),
        ]),
        "{plist}"
    );
    assert_eq!(plist.matches("<string>").count(), 8, "{plist}");
    assert_eq!(files[0].mode, 0o644);
    assert_eq!(files[1].path, "private/var/kbf-sim/user-wanted");
    assert_eq!(files[1].contents, b"kbfuser\n");
    for bad in ["", "Root", "root", "a b", "../x", "<x>"] {
        assert!(mkuser_payload(bad).is_err(), "{bad:?}");
    }
    // Neither daemon nor wish is in the payload of every base.
    assert!(
        guest::offline_payload(Some(Vec::new()))
            .iter()
            .all(|f| !f.path.contains("mkuser") && !f.path.contains("user-wanted"))
    );
}

fn setup_args(user: Option<&str>) -> SetupArgs {
    SetupArgs {
        guest_user: user.map(str::to_owned),
        ..SetupArgs::default()
    }
}

/// Catches `--guest-user` not reaching the disk, being refused without a binary when the
/// disk already has one, writing when the name is bad, or the report not telling what to run
/// next.
#[test]
fn setup_with_a_guest_user_writes_the_daemon_and_the_wish_and_says_to_prime() {
    let work = work_with_base("setup-user");
    let base = work.join("base");
    let sys = FakeSystem::new();
    sys.uid.set(0);
    let r = setup(&sys, &base, &setup_args(Some("kbfuser")));
    assert_eq!(r.status, ReportStatus::Ok, "{r:?}");
    let calls = sys.offline_calls.borrow();
    assert_eq!(
        calls[0].1,
        [
            "Library/LaunchDaemons/com.kbf.mkuser.plist",
            "private/var/kbf-sim/user-wanted"
        ]
    );
    assert_eq!(r.details["guest_user"], "kbfuser");
    let next = r.details["next"].as_str().expect("next");
    assert!(
        next.contains("image prime") && next.contains("probe e3"),
        "{next}"
    );
    drop(calls);

    // With a binary, the user files follow the probe payload.
    let bin = work.join("guest-src");
    std::fs::write(&bin, "bin").expect("bin");
    let mut a = setup_args(Some("kbfuser"));
    a.guest_binary = Some(bin);
    let r = setup(&sys, &base, &a);
    assert_eq!(r.status, ReportStatus::Ok, "{r:?}");
    let paths = sys.offline_calls.borrow()[1].1.clone();
    assert!(paths.contains(&"usr/local/libexec/kbf-guest".to_owned()));
    assert_eq!(
        paths[paths.len() - 2..],
        [
            "Library/LaunchDaemons/com.kbf.mkuser.plist",
            "private/var/kbf-sim/user-wanted"
        ]
    );

    // A bad name writes nothing; no user and no binary is still the old refusal.
    let before = sys.offline_calls.borrow().len();
    let r = setup(&sys, &base, &setup_args(Some("Root")));
    assert_eq!(r.status, ReportStatus::Error);
    assert!(r.reason.expect("reason").contains("--guest-user"));
    let r = setup(&sys, &base, &setup_args(None));
    assert_eq!(r.status, ReportStatus::NeedsManual);
    assert_eq!(sys.offline_calls.borrow().len(), before);
}

/// Catches the flags not parsing or not reaching the verbs: `--guest-user` with a bad name
/// refused by the parser, `image prime` taking its base and binary, its default deadline of
/// an hour, a deadline of zero refused, and the dispatch passing all three on.
#[test]
fn the_flags_parse_and_prime_is_dispatched_with_its_deadline() {
    assert!(
        Cli::try_parse_from([
            "kbf-vmm",
            "image",
            "setup",
            "--base",
            "b",
            "--guest-user",
            "Bad Name"
        ])
        .is_err()
    );
    let cli = Cli::try_parse_from([
        "kbf-vmm",
        "image",
        "setup",
        "--base",
        "b",
        "--guest-user",
        "kbfuser",
    ])
    .expect("parse");
    match &cli.command {
        Cmd::Image(ImageCmd::Setup { guest_user, .. }) => {
            assert_eq!(guest_user.as_deref(), Some("kbfuser"));
        }
        other => panic!("{other:?}"),
    }
    let cli = Cli::try_parse_from([
        "kbf-vmm",
        "image",
        "prime",
        "--base",
        "b",
        "--guest-binary",
        "g",
    ])
    .expect("parse");
    match &cli.command {
        Cmd::Image(ImageCmd::Prime {
            base,
            guest_binary,
            deadline_secs,
            shutdown_secs,
        }) => {
            assert_eq!(*shutdown_secs, 60);
            assert_eq!(base, Path::new("b"));
            assert_eq!(guest_binary, Path::new("g"));
            assert_eq!(*deadline_secs, DEFAULT_DEADLINE.as_secs());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(DEFAULT_DEADLINE.as_secs(), 3600);
    for bad in ["0", "86401", "x"] {
        assert!(
            Cli::try_parse_from([
                "kbf-vmm",
                "image",
                "prime",
                "--base",
                "b",
                "--guest-binary",
                "g",
                "--deadline-secs",
                bad
            ])
            .is_err(),
            "{bad}"
        );
    }
    assert!(Cli::try_parse_from(["kbf-vmm", "image", "prime", "--base", "b"]).is_err());

    // Dispatch: the base is the work directory, the binary is copied into the share, the
    // deadline is honoured (a guest that stays up is stopped), and the line is the report.
    let work = work_with_base("prime-app");
    let base = work.join("base");
    let bin = work.join("kbf-guest-src");
    std::fs::write(&bin, "bin").expect("bin");
    let sys = FakeSystem::new();
    sys.script(powering_off(&all_ok()));
    let cli = Cli::parse_from([
        "kbf-vmm",
        "image",
        "prime",
        "--base",
        base.to_str().expect("p"),
        "--guest-binary",
        bin.to_str().expect("p"),
    ]);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = crate::app::run(
        cli,
        Some(&sys),
        crate::probe::Timing::instant(),
        &mut out,
        &mut err,
    );
    assert_eq!(format!("{code:?}"), "ExitCode(unix_exit_status(0))");
    let v: Value = serde_json::from_slice(&out).expect("json");
    assert_eq!(v["probe"], "image-prime");
    assert_eq!(v["status"], "ok");
    assert_eq!(sys.starts.borrow()[0].0.bundle, base);
    // A failed prime exits 1.
    let sys = FakeSystem::new();
    sys.script(powering_off(&[]));
    let cli = Cli::parse_from([
        "kbf-vmm",
        "image",
        "prime",
        "--base",
        base.to_str().expect("p"),
        "--guest-binary",
        bin.to_str().expect("p"),
    ]);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = crate::app::run(
        cli,
        Some(&sys),
        crate::probe::Timing::instant(),
        &mut out,
        &mut err,
    );
    assert_eq!(format!("{code:?}"), "ExitCode(unix_exit_status(1))");
}

/// Catches a forced stop of a guest that was only slow: at the deadline the guest is first
/// asked to shut down, and one that does is not force-stopped (its disk is flushed); the
/// run is still a timeout error, since it did not power off by itself in time.
#[test]
fn a_guest_still_up_at_the_deadline_is_asked_to_shut_down_before_it_is_stopped() {
    let work = work_with_base("prime-slow");
    let sys = FakeSystem::new();
    let steps = all_ok();
    sys.script(Script::booted().with_guest_file(&steps[0].0, &steps[0].1));
    let r = prime(&prime_ctx(&sys, &work), SHORT);
    assert_eq!(r.status, ReportStatus::Error, "{r:?}");
    assert!(r.reason.expect("reason").starts_with("timeout:"));
    assert_eq!(r.details["outcome"], "timeout");
    assert_eq!(r.details["stop"]["how"], "guest shut down (requestStop)");
}

/// Catches `--shutdown-secs` going nowhere: its default is 60 and a given value parses.
#[test]
fn the_wait_for_a_requested_shutdown_is_a_flag_with_a_default() {
    for (extra, want) in [(vec![], 60), (vec!["--shutdown-secs", "5"], 5)] {
        let mut argv = vec![
            "kbf-vmm",
            "image",
            "prime",
            "--base",
            "b",
            "--guest-binary",
            "g",
        ];
        argv.extend(extra);
        let cli = Cli::try_parse_from(argv).expect("parse");
        match &cli.command {
            Cmd::Image(ImageCmd::Prime { shutdown_secs, .. }) => assert_eq!(*shutdown_secs, want),
            other => panic!("{other:?}"),
        }
    }
    assert!(
        Cli::try_parse_from([
            "kbf-vmm",
            "image",
            "prime",
            "--base",
            "b",
            "--guest-binary",
            "g",
            "--shutdown-secs",
            "0"
        ])
        .is_err()
    );
}

fn audit_frame_json(status: &str, exit: Option<i32>, counts: (u64, u64, u64)) -> (String, String) {
    (
        kbf_guest::mkuser_audit::AUDIT_NAME.to_owned(),
        serde_json::json!({
            "step": "log-audit",
            "status": status,
            "exit": exit,
            "signal": null,
            "matched_lines": counts.0,
            "password_hits": counts.1,
            "window_hits": counts.2,
        })
        .to_string(),
    )
}

/// Catches a prime report that drops the guest's log-audit counts, reports them from the
/// wrong fields, counts the audit frame among the steps (it is not one), or lets the audit
/// change the verdict: the same run with a failed audit is still ok, and the counts and the
/// audit's own status are reported as they came.
#[test]
fn the_log_audit_counts_are_reported_and_do_not_change_the_verdict() {
    for (status, exit, counts) in [("ok", Some(0), (4, 2, 3)), ("failed", Some(64), (0, 0, 0))] {
        let work = work_with_base("prime-audit");
        let sys = FakeSystem::new();
        let mut frames = all_ok();
        frames.push(audit_frame_json(status, exit, counts));
        sys.script(powering_off(&frames));
        let r = prime(&prime_ctx(&sys, &work), LONG);
        assert_eq!(r.status, ReportStatus::Ok, "{r:?}");
        let d = &r.details;
        assert!(
            holds(&[
                d["log_audit_received"] == true,
                d["log_audit"]["status"] == status,
                d["log_audit"]["exit"] == serde_json::json!(exit),
                d["log_audit"]["matched_lines"] == counts.0,
                d["log_audit"]["password_hits"] == counts.1,
                d["log_audit"]["window_hits"] == counts.2,
                d["steps_received"] == n_steps(),
                d["problems"] == serde_json::json!([]),
            ]),
            "{d}"
        );
    }
}

/// Catches a report that invents counts when the guest sent no audit (an older guest
/// binary), and an audit frame that is not the expected JSON being swallowed.
#[test]
fn without_a_log_audit_frame_the_report_says_none_arrived_and_a_bad_one_is_listed() {
    let work = work_with_base("prime-noaudit");
    let sys = FakeSystem::new();
    sys.script(powering_off(&all_ok()));
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Ok, "{r:?}");
    assert_eq!(r.details["log_audit_received"], false);
    assert!(r.details["log_audit"].is_null());

    let work = work_with_base("prime-badaudit");
    let sys = FakeSystem::new();
    let mut frames = all_ok();
    frames.push((
        kbf_guest::mkuser_audit::AUDIT_NAME.to_owned(),
        "{\"step\": 1}".to_owned(),
    ));
    sys.script(powering_off(&frames));
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.details["log_audit_received"], false);
    assert_eq!(
        r.details["unreadable_frames"][0]["frame"],
        "mkuser-audit.json"
    );
}

/// Catches the priming boot not being marked for the sink to answer yes to (the guest then
/// leaves the root setup to `sim` on every clone), a mark in the guest's reach (the share), a
/// mark left behind in the base afterwards, and the guest's and the sink's names for the
/// question drifting apart.
#[test]
fn the_priming_boot_is_marked_in_the_results_directory_and_the_mark_is_withdrawn() {
    let work = work_with_base("prime-request");
    let sys = FakeSystem::new();
    sys.script(powering_off(&all_ok()));
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Ok, "{r:?}");
    let seen = sys.results_at_start.borrow();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].contains(&crate::sink::PRIME_MARKER.to_owned()),
        "{:?}",
        seen[0]
    );
    assert!(!results(&work).join(crate::sink::PRIME_MARKER).exists());
    assert_eq!(
        crate::sink::PRIME_QUESTION,
        kbf_guest::mkuser_prime::QUESTION
    );
    assert_eq!(kbf_guest::mkuser_prime::ANSWER_YES, "yes");
}

/// Catches a guest step allowed to run longer than the host waits for the whole priming
/// boot: with the defaults, no single step of the root setup may outlast `--deadline-secs`.
#[test]
fn no_guest_step_may_outlast_the_default_deadline() {
    use clap::Parser;
    #[derive(clap::Parser)]
    struct W {
        #[command(flatten)]
        a: kbf_guest::mkuser::MkUserArgs,
    }
    let a = W::parse_from(["w", "--name", "kbfuser"]).a;
    let c = kbf_guest::mkuser::Config::from(&a);
    for d in [c.quick, c.audit, c.first_launch, c.runtime_add] {
        assert!(d <= DEFAULT_DEADLINE, "{d:?}");
    }
}

/// Catches a priming boot that never got a yes being reported as a success: the guest
/// then ran no root setup and wrote no record, every clone pays for it, and the base looks
/// primed. A summary that says the setup was skipped, or says nothing (an older guest), is an
/// error naming the question; one that says done, or that nothing was staged, is a success.
#[test]
fn a_prime_whose_guest_got_no_yes_is_an_error() {
    let ids = plan("kbfuser", false).len();
    let run_with = |root: Option<RootSetup>, signal: Option<Signal>| {
        let work = work_with_base("prime-unseen");
        let sys = FakeSystem::new();
        let mut frames = all_ok();
        frames.pop();
        frames.push(summary_root(true, false, ids, root, signal));
        sys.script(powering_off(&frames));
        prime(&prime_ctx(&sys, &work), LONG)
    };
    for (root, signal) in [
        (Some(RootSetup::Skipped), Some(Signal::No)),
        (
            Some(RootSetup::Skipped),
            Some(Signal::NoAnswer("x".to_owned())),
        ),
        (
            Some(RootSetup::Skipped),
            Some(Signal::Refused("no".to_owned())),
        ),
        (None, None),
    ] {
        let r = run_with(root, signal);
        assert_eq!(r.status, ReportStatus::Error, "{root:?}");
        let why = r.reason.expect("reason");
        assert!(why.contains("did not get a yes"), "{why}");
        assert!(
            why.contains("root_setup:") && why.contains("signal:"),
            "{why}"
        );
    }
    for root in [RootSetup::Done, RootSetup::NotNeeded] {
        let r = run_with(Some(root), Some(Signal::Yes));
        assert_eq!(r.status, ReportStatus::Ok, "{root:?} {r:?}");
    }
    // An already-ready base that did not see the request is no better.
    let work = work_with_base("prime-unseen-ready");
    let sys = FakeSystem::new();
    sys.script(
        Script::booted()
            .with_guest_file(
                &summary_root(true, true, 0, Some(RootSetup::Skipped), Some(Signal::No)).0,
                &summary_root(true, true, 0, Some(RootSetup::Skipped), Some(Signal::No)).1,
            )
            .then(Poll::Event(Event::Stopped { ms: 50 })),
    );
    let r = prime(&prime_ctx(&sys, &work), LONG);
    assert_eq!(r.status, ReportStatus::Error, "{r:?}");
}
