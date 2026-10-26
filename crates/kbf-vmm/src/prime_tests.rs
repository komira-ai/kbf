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
            d["steps_received"] == 12,
            d["steps_ok"] == 12,
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
        line.contains("kbfuser") && line.contains("12 of 12"),
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
    assert_eq!(r.details["steps_received"], 12);
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
/// 20 minutes, a deadline of zero refused, and the dispatch passing all three on.
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
        }) => {
            assert_eq!(base, Path::new("b"));
            assert_eq!(guest_binary, Path::new("g"));
            assert_eq!(*deadline_secs, DEFAULT_DEADLINE.as_secs());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(DEFAULT_DEADLINE.as_secs(), 1200);
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
