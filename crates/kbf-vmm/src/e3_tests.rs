//! Tests of `probe e3` on a fake system: the guest's frames are scripted, the clone and
//! the VM are fakes. Nothing here waits on a clock; the timeouts are the instant ones.

use std::path::Path;

use base64::Engine as _;
use kbf_guest::sim::{
    SCREENSHOT_NAME, STEP_PREFIX, SUMMARY_NAME, Status, StepFrame, SummaryFrame, Who,
    step_frame_name,
};
use serde_json::{Value, json};

use crate::bundle::{Recorded, record_sizing};
use crate::cli::Graphics;
use crate::e3::{decode_screenshot, e3};
use crate::fake::{FakeSystem, Script, ctx, holds, work_with_base};
use crate::probe::Ctx;
use crate::report::Status as ReportStatus;

fn guest_ctx<'a>(sys: &'a FakeSystem, work: &Path) -> Ctx<'a> {
    let mut c = ctx(sys, work);
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
        ms: 10 * (index as u64 + 1),
        deadline_secs: 120,
        stdout: format!("out of {id}"),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        note: None,
        skipped_because: (status == Status::Skipped).then(|| "license".to_owned()),
    };
    (
        step_frame_name(index, id),
        serde_json::to_string(&f).expect("json"),
    )
}

const IDS: [&str; 5] = [
    "xcode-select",
    "license",
    "first-launch",
    "create",
    "shutdown",
];

fn summary(ok: bool, n: usize) -> (String, String) {
    let s = SummaryFrame {
        complete: true,
        steps: IDS
            .iter()
            .take(n)
            .map(|i| ((*i).to_owned(), Status::Ok))
            .collect(),
        ok,
        runtime: Some("RT".into()),
        device_type: Some("DT".into()),
        udid: Some("UDID".into()),
        screenshot_bytes: Some(4),
        undelivered: Vec::new(),
        note: None,
    };
    (
        SUMMARY_NAME.to_owned(),
        serde_json::to_string(&s).expect("json"),
    )
}

fn shot(bytes: &[u8]) -> (String, String) {
    (
        SCREENSHOT_NAME.to_owned(),
        json!({
            "format": "jpeg",
            "bytes": bytes.len(),
            "base64": base64::engine::general_purpose::STANDARD.encode(bytes),
        })
        .to_string(),
    )
}

fn all_ok() -> Vec<(String, String)> {
    let mut frames: Vec<_> = IDS
        .iter()
        .enumerate()
        .map(|(i, id)| step(i, id, Status::Ok, Some(0)))
        .collect();
    frames.push(shot(&[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]));
    frames.push(summary(true, IDS.len()));
    frames
}

fn guest_sending(frames: &[(String, String)]) -> Script {
    frames
        .iter()
        .fold(Script::booted().acking_shutdown(), |s, (n, b)| {
            s.with_guest_file(n, b)
        })
}

fn arm(r: &crate::report::Report, i: usize) -> &Value {
    &r.details["arms"][i]
}

/// Catches an E3 that starts a guest it cannot read anything from: without the guest
/// binary, or a base, it says what to give and starts nothing.
#[test]
fn without_a_guest_binary_or_a_base_e3_says_what_is_missing_and_starts_nothing() {
    let work = work_with_base("e3-nobin");
    let sys = FakeSystem::new();
    let r = e3(&ctx(&sys, &work), Graphics::On);
    assert_eq!(r.status, ReportStatus::NeedsManual);
    assert!(r.reason.expect("reason").contains("--guest-binary"));
    let empty = crate::fake::tmp("e3-nobase");
    let r = e3(&guest_ctx(&sys, &empty), Graphics::On);
    assert_eq!(r.status, ReportStatus::NeedsManual);
    assert!(sys.starts.borrow().is_empty());
}

/// Catches a graphics mode that reaches the wrong guest or the wrong number of guests:
/// `on` and `off` start one, `both` starts the headless one first and the other after it is
/// stopped (never two at once), and each guest has the share and the vsock port.
#[test]
fn the_graphics_mode_picks_the_guests_and_runs_them_one_after_the_other() {
    for (mode, want) in [
        (Graphics::On, vec![("e3-on", true)]),
        (Graphics::Off, vec![("e3-off", false)]),
        (Graphics::Both, vec![("e3-off", false), ("e3-on", true)]),
    ] {
        let work = work_with_base("e3-modes");
        let sys = FakeSystem::new();
        for _ in &want {
            sys.script(guest_sending(&all_ok()));
        }
        let r = e3(&guest_ctx(&sys, &work), mode);
        assert_eq!(r.status, ReportStatus::Ok, "{r:?}");
        let starts = sys.starts.borrow();
        assert_eq!(starts.len(), want.len(), "{mode:?}");
        for ((spec, _), (name, graphics)) in starts.iter().zip(&want) {
            assert_eq!(spec.graphics, *graphics, "{name}");
            assert!(spec.bundle.ends_with(name), "{name}");
            assert!(spec.automount_dir.is_some() && spec.results_dir.is_some());
            assert_eq!(spec.vsock_ports, [5000]);
        }
        let names: Vec<_> = r.details["arms"]
            .as_array()
            .expect("arms")
            .iter()
            .map(|a| a["name"].as_str().expect("n"))
            .collect();
        assert_eq!(names, want.iter().map(|w| w.0).collect::<Vec<_>>());
    }
}

/// Catches a guest sized by the probes' defaults when the base recorded what it was built
/// for (a simulator needs the memory), and a base without a record not being told so.
#[test]
fn the_guest_is_sized_as_the_base_was_built() {
    let work = work_with_base("e3-size");
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
    sys.script(guest_sending(&all_ok()));
    let r = e3(&guest_ctx(&sys, &work), Graphics::On);
    let spec = sys.starts.borrow()[0].0.clone();
    assert_eq!((spec.cpus, spec.memory_mib), (6, 16384));
    assert_eq!(
        arm(&r, 0)["sizing"],
        json!({"cpus": 6, "memory_mib": 16384, "disk_gib": 200})
    );
    assert!(arm(&r, 0)["sizing_warning"].is_null());

    let work = work_with_base("e3-size-none");
    let sys = FakeSystem::new();
    sys.script(guest_sending(&all_ok()));
    let r = e3(&guest_ctx(&sys, &work), Graphics::On);
    let spec = sys.starts.borrow()[0].0.clone();
    assert_eq!((spec.cpus, spec.memory_mib), (2, 4096));
    assert!(
        arm(&r, 0)["sizing_warning"]
            .as_str()
            .expect("warning")
            .contains("default of 2 CPUs")
    );
    assert!(arm(&r, 0)["sizing"].is_null());
}

/// Catches a summary that loses a step, mixes their order, does not carry a step's own
/// fields through, or does not say the run ended; and the screenshot that is not written
/// where the report says, with the bytes the guest sent, for the owner only.
#[test]
fn a_good_run_is_reported_step_by_step_with_the_screenshot_written() {
    let work = work_with_base("e3-good");
    let sys = FakeSystem::new();
    sys.script(guest_sending(&all_ok()));
    let r = e3(&guest_ctx(&sys, &work), Graphics::On);
    assert_eq!(r.status, ReportStatus::Ok, "{r:?}");
    let a = arm(&r, 0);
    assert!(
        holds(&[
            a["steps_received"] == json!(5),
            a["steps_ok"] == json!(5),
            a["problems"] == json!([]),
            a["summary_received"] == json!(true),
            a["summary"]["ok"] == json!(true),
            a["summary"]["udid"] == json!("UDID"),
        ]),
        "{a}"
    );
    let ids: Vec<&str> = a["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .map(|s| s["step"].as_str().expect("s"))
        .collect();
    assert_eq!(ids, IDS);
    assert_eq!(a["steps"][1]["argv"], json!(["/usr/bin/tool", "license"]));
    assert_eq!(
        (a["steps"][1]["exit"].as_i64(), a["steps"][1]["ms"].as_u64()),
        (Some(0), Some(20))
    );
    let written = a["screenshot"]["written"].as_str().expect("path");
    assert_eq!(
        std::fs::read(written).expect("picture"),
        [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]
    );
    assert_eq!(a["screenshot"]["jpeg_signature"], json!(true));
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(written).expect("meta").permissions(),
    );
    assert_eq!(mode & 0o777, 0o600);
    assert!(Path::new(written).starts_with(&*work), "{written}");
    let line = r.summary.clone().expect("summary");
    assert!(
        line.contains("5 of 5 steps ok") && line.contains("screenshot 7 bytes"),
        "{line}"
    );
    // The frames were read from the host's private directory, and nothing was left running.
    assert!(a["results_files"].as_array().expect("files").len() >= 7);
}

/// Catches a failed step reported as fine: the problems list names each step that was not
/// ok with how it failed, and the one-line summary says them.
#[test]
fn failed_timed_out_and_skipped_steps_are_named_with_their_cause() {
    let work = work_with_base("e3-bad");
    let sys = FakeSystem::new();
    let frames = vec![
        step(0, "xcode-select", Status::Ok, Some(0)),
        step(1, "license", Status::Failed, Some(69)),
        step(2, "first-launch", Status::Skipped, None),
        step(3, "create", Status::Timeout, None),
        step(4, "shutdown", Status::Error, None),
        summary(false, 1),
    ];
    sys.script(guest_sending(&frames));
    let r = e3(&guest_ctx(&sys, &work), Graphics::Off);
    let a = arm(&r, 0);
    assert_eq!(
        a["problems"],
        json!([
            "license (exit 69)",
            "first-launch (skipped)",
            "create (timeout)",
            "shutdown (error)"
        ])
    );
    assert_eq!(a["steps_ok"], 1);
    assert!(a["screenshot"].is_null());
    let line = r.summary.clone().expect("summary");
    assert!(
        line.contains("1 of 5 steps ok; not ok: license (exit 69)"),
        "{line}"
    );
    assert!(line.contains("no screenshot"), "{line}");
    // A probe that found a failing guest still ran: it is an answer, not an error.
    assert_eq!(r.status, ReportStatus::Ok);
}

/// Catches a silent guest reported as a pass, or the wait ending with a panic: with no
/// frames the report says so and what to check, and the guest is still stopped.
#[test]
fn a_guest_that_sends_nothing_is_reported_as_silent_and_stopped() {
    let work = work_with_base("e3-silent");
    let sys = FakeSystem::new();
    sys.script(Script::booted().acking_shutdown());
    let r = e3(&guest_ctx(&sys, &work), Graphics::On);
    assert_eq!(r.status, ReportStatus::Ok);
    let a = arm(&r, 0);
    assert!(
        holds(&[
            a["steps_received"] == json!(0),
            a["summary_received"] == json!(false),
            a["stop"]["how"] == json!("guest shut down (vsock)")
        ]),
        "{a}"
    );
    assert!(
        r.summary
            .expect("summary")
            .contains("no step frame arrived")
    );
}

/// Catches frames from a run that did not end passing for a complete one.
#[test]
fn steps_without_a_summary_are_reported_as_a_run_that_did_not_end() {
    let work = work_with_base("e3-nosummary");
    let sys = FakeSystem::new();
    sys.script(guest_sending(&[step(
        0,
        "xcode-select",
        Status::Ok,
        Some(0),
    )]));
    let r = e3(&guest_ctx(&sys, &work), Graphics::On);
    assert_eq!(arm(&r, 0)["summary_received"], json!(false));
    assert!(
        r.summary
            .expect("summary")
            .contains("no summary (the run did not end)")
    );
}

/// Catches a frame the guest wrote badly crashing the probe or being silently dropped: it
/// is listed with the reason, the good frames still count, and a file that is not one of
/// ours is not looked at.
#[test]
fn unreadable_frames_are_listed_and_the_rest_still_count() {
    let work = work_with_base("e3-garbage");
    let sys = FakeSystem::new();
    let mut frames = all_ok();
    frames.push((step_frame_name(7, "garbage"), "{not json".to_owned()));
    frames.push((
        format!("{STEP_PREFIX}08-huge.json"),
        "x".repeat((crate::guest::MAX_RESULT_BYTES + 1) as usize),
    ));
    frames.push(("something-else.json".to_owned(), "{not json".to_owned()));
    sys.script(guest_sending(&frames));
    let r = e3(&guest_ctx(&sys, &work), Graphics::On);
    let a = arm(&r, 0);
    assert_eq!(a["steps_received"], 5);
    let bad: Vec<(&str, &str)> = a["unreadable_frames"]
        .as_array()
        .expect("list")
        .iter()
        .map(|u| {
            (
                u["frame"].as_str().expect("f"),
                u["error"].as_str().expect("e"),
            )
        })
        .collect();
    assert_eq!(bad.len(), 2, "{bad:?}");
    assert!(
        bad.iter()
            .any(|(f, e)| f.contains("garbage") && e.contains("not the expected JSON"))
    );
    assert!(
        bad.iter()
            .any(|(f, e)| f.contains("huge") && e.contains("larger than"))
    );
}

/// Catches a summary read from the share: the guest can write anything there, so only what
/// came over vsock into the host's own directory counts.
#[test]
fn a_summary_written_into_the_share_is_not_a_frame() {
    let work = work_with_base("e3-share");
    let sys = FakeSystem::new();
    let (name, body) = summary(true, 5);
    sys.script(Script::booted().with_share_file(&name, &body));
    let r = e3(&guest_ctx(&sys, &work), Graphics::On);
    assert_eq!(arm(&r, 0)["summary_received"], json!(false));
}

/// Catches a frame that arrives while the host is already waiting being missed, and a wait
/// that stops before the summary.
#[test]
fn frames_that_arrive_later_are_read_and_the_wait_ends_at_the_summary() {
    let work = work_with_base("e3-late");
    let sys = FakeSystem::new();
    let mut s = Script::booted().acking_shutdown();
    for (i, id) in IDS.iter().enumerate() {
        let (n, b) = step(i, id, Status::Ok, Some(0));
        s = s.delivering_after(2 + i as u32, &n, &b);
    }
    let (n, b) = summary(true, 5);
    s = s.delivering_after(8, &n, &b);
    sys.script(s);
    let mut c = guest_ctx(&sys, &work);
    // Long enough that only the summary can end the wait in this test.
    c.timing.sim_timeout = std::time::Duration::from_secs(30);
    let r = e3(&c, Graphics::On);
    let a = arm(&r, 0);
    assert_eq!(
        (
            a["steps_received"].as_u64(),
            a["summary_received"].as_bool()
        ),
        (Some(5), Some(true)),
        "{a}"
    );
}

/// Catches a screenshot taken on trust: bad base64 and a stated length that differs are
/// errors, nothing is written for them, and a picture without the JPEG signature is written
/// but said not to have it.
#[test]
fn a_bad_screenshot_frame_is_reported_and_not_written() {
    let png = [0x89, b'P', b'N', b'G'];
    let cases: Vec<(&str, (String, String), bool, &str)> = vec![
        ("not-base64", (SCREENSHOT_NAME.to_owned(), json!({"format":"jpeg","bytes":3,"base64":"%%%"}).to_string()), false, "not base64"),
        ("short", (SCREENSHOT_NAME.to_owned(), json!({"format":"jpeg","bytes":9,"base64":base64::engine::general_purpose::STANDARD.encode([1u8,2,3])}).to_string()), false, "states 9 bytes and holds 3"),
        ("png", shot(&png), true, ""),
    ];
    for (name, frame, written, error) in cases {
        let work = work_with_base(&format!("e3-shot-{name}"));
        let sys = FakeSystem::new();
        let mut frames = all_ok();
        frames.retain(|(n, _)| n != SCREENSHOT_NAME);
        frames.push(frame);
        sys.script(guest_sending(&frames));
        let r = e3(&guest_ctx(&sys, &work), Graphics::On);
        let s = &arm(&r, 0)["screenshot"];
        assert_eq!(s["written"].is_string(), written, "{name}: {s}");
        if written {
            assert_eq!(s["jpeg_signature"], json!(false));
        } else {
            assert!(
                s["error"].as_str().expect("error").contains(error),
                "{name}: {s}"
            );
            assert!(!work.join("e3-on-screenshot.jpg").exists());
        }
    }
    assert!(
        decode_screenshot(&json!({"bytes": 1}))
            .unwrap_err()
            .contains("base64")
    );
    assert!(
        decode_screenshot(&json!({"base64": "AQ=="}))
            .unwrap_err()
            .contains("bytes")
    );
    assert_eq!(
        decode_screenshot(&json!({"base64": "AQID", "bytes": 3}))
            .expect("ok")
            .bytes,
        [1, 2, 3]
    );
}

/// Catches `--keep` removing the clone, and the lack of it leaving the clone and the shares
/// behind; the screenshot is the probe's output, so it stays either way.
#[test]
fn the_clone_is_removed_unless_kept_and_the_screenshot_stays() {
    for keep in [false, true] {
        let work = work_with_base(&format!("e3-keep-{keep}"));
        let sys = FakeSystem::new();
        sys.script(guest_sending(&all_ok()));
        let mut c = guest_ctx(&sys, &work);
        c.keep = keep;
        let r = e3(&c, Graphics::On);
        assert_eq!(work.join("e3-on").exists(), keep);
        assert_eq!(work.join("e3-on-share").exists(), keep);
        assert_eq!(work.join("e3-on-results").exists(), keep);
        assert!(work.join("e3-on-screenshot.jpg").exists());
        assert_eq!(r.details["clones_kept"], keep);
    }
}

/// Catches a guest that cannot be started reported as a finished probe.
#[test]
fn a_guest_that_cannot_start_is_an_error_and_leaves_no_share_behind() {
    let work = work_with_base("e3-start");
    let sys = FakeSystem::new();
    *sys.start_error.borrow_mut() = Some("no hypervisor".into());
    let r = e3(&guest_ctx(&sys, &work), Graphics::Both);
    assert_eq!(r.status, ReportStatus::Error);
    assert!(r.reason.expect("reason").contains("no hypervisor"));
    assert!(!work.join("e3-off-share").exists() && !work.join("e3-off-results").exists());
    assert_eq!(sys.starts.borrow().len(), 0);
}

/// Catches how the guest was stopped going unreported: a guest that acks the vsock shutdown
/// is stopped that way, one that does not answer falls back and says why.
#[test]
fn the_stop_is_asked_over_vsock_and_the_report_says_how_it_went() {
    for (script, how, acked) in [
        (
            guest_sending(&all_ok()),
            "guest shut down (vsock)",
            json!(true),
        ),
        (
            all_ok().iter().fold(
                Script::booted().unreachable_by_vsock("no listener"),
                |s, (n, b)| s.with_guest_file(n, b),
            ),
            "guest shut down (requestStop)",
            json!(false),
        ),
    ] {
        let work = work_with_base("e3-stop");
        let sys = FakeSystem::new();
        sys.script(script);
        let r = e3(&guest_ctx(&sys, &work), Graphics::On);
        let stop = &arm(&r, 0)["stop"];
        assert_eq!(
            (stop["how"].as_str(), &stop["vsock_acked"]),
            (Some(how), &acked),
            "{stop}"
        );
    }
}

/// Catches a frame name or count the host's own sink would refuse: with the most runtimes a
/// setup stages (and the longest names) every frame the guest can send has a name the sink
/// accepts, they are all distinct, and they fit the files one VM may create.
#[test]
fn the_frames_of_the_largest_plan_fit_the_sinks_name_and_file_limits() {
    use kbf_guest::sim::{Inputs, max_frames, plan};
    // Both kinds, at the longest name each may have (the import step's id is longer than the
    // add step's).
    let longest = |i: usize| {
        let suffix = if i.is_multiple_of(2) {
            ".dmg"
        } else {
            kbf_guest::sim::BUNDLE_SUFFIX
        };
        format!(
            "{i}{}{suffix}",
            "n".repeat(kbf_guest::sim::MAX_STAGED_NAME_BYTES - 1 - suffix.len())
        )
    };
    let steps = plan(&Inputs {
        runtimes: (0..crate::stage::MAX_RUNTIMES).map(longest).collect(),
        app: Some("App.app".to_owned()),
    });
    let mut names: Vec<String> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| step_frame_name(i, &s.id))
        .collect();
    names.push(SCREENSHOT_NAME.to_owned());
    names.push(SUMMARY_NAME.to_owned());
    for n in &names {
        assert_eq!(crate::sink::check_name(n), Ok(()), "{n}");
    }
    let distinct: std::collections::BTreeSet<_> = names.iter().collect();
    assert_eq!(distinct.len(), names.len());
    assert_eq!(names.len(), max_frames(crate::stage::MAX_RUNTIMES));
    // Room is left for the markers and results other daemons of the image send.
    assert!(
        names.len() + 8 <= crate::sink::MAX_FILES,
        "{} frames",
        names.len()
    );
}

/// Catches the flag reaching a gate it is not for, and the e3 gate not being reachable by
/// name from the command line, in either case.
#[test]
fn the_e3_gate_parses_and_graphics_belongs_to_it_alone() {
    use crate::cli::{Cli, Cmd, Gate};
    use clap::Parser;
    for (arg, mode) in [
        ("on", Graphics::On),
        ("off", Graphics::Off),
        ("both", Graphics::Both),
    ] {
        let cli = Cli::parse_from([
            "kbf-vmm",
            "probe",
            "E3",
            "--work-dir",
            "w",
            "--graphics",
            arg,
        ]);
        assert!(
            matches!(&cli.command, Cmd::Probe(a) if a.gate == Gate::E3 && a.graphics == Some(mode))
        );
    }
    let cli = Cli::parse_from(["kbf-vmm", "probe", "e3", "--work-dir", "w"]);
    assert!(matches!(&cli.command, Cmd::Probe(a) if a.graphics.is_none()));
    assert!(
        Cli::try_parse_from([
            "kbf-vmm",
            "probe",
            "e3",
            "--work-dir",
            "w",
            "--graphics",
            "maybe"
        ])
        .is_err()
    );

    let mut out = Vec::new();
    let mut err = Vec::new();
    let sys = FakeSystem::new();
    let cli = Cli::parse_from([
        "kbf-vmm",
        "probe",
        "g1",
        "--work-dir",
        "w",
        "--graphics",
        "on",
    ]);
    let _ = crate::app::run(
        cli,
        Some(&sys),
        crate::probe::Timing::instant(),
        &mut out,
        &mut err,
    );
    assert!(String::from_utf8_lossy(&out).contains("e3 probe only"));
    assert!(sys.starts.borrow().is_empty());
}

fn step_with(
    index: usize,
    id: &str,
    status: Status,
    exit: Option<i32>,
    signal: Option<i32>,
) -> (String, String) {
    let (name, body) = step(index, id, status, exit);
    let mut v: Value = serde_json::from_str(&body).expect("json");
    v["signal"] = json!(signal);
    (name, v.to_string())
}

/// Catches a step killed by a signal, or failed without a code or signal, being named by
/// something else: the problems list says which.
#[test]
fn a_step_killed_by_a_signal_is_named_with_the_signal() {
    let work = work_with_base("e3-signal");
    let sys = FakeSystem::new();
    sys.script(guest_sending(&[
        step_with(0, "xcode-select", Status::Failed, None, Some(9)),
        step_with(1, "license", Status::Failed, None, None),
        summary(false, 0),
    ]));
    let r = e3(&guest_ctx(&sys, &work), Graphics::On);
    assert_eq!(
        arm(&r, 0)["problems"],
        json!(["xcode-select (signal 9)", "license (failed)"])
    );
}

/// Catches a guest that ends by itself (the helper exits, or the VM fails to start) making
/// the probe wait out the simulator timeout, ask a dead guest to shut down, or report it
/// as having stayed up.
#[test]
fn a_guest_that_ends_on_its_own_ends_the_wait_and_is_not_asked_to_shut_down() {
    use crate::system::Poll;
    let work = work_with_base("e3-ended");
    let sys = FakeSystem::new();
    sys.script(Script::booted().then(Poll::Gone {
        code: Some(1),
        signal: None,
    }));
    let mut c = guest_ctx(&sys, &work);
    // A wait that only the guest's end can finish.
    c.timing.sim_timeout = std::time::Duration::from_secs(600);
    let r = e3(&c, Graphics::On);
    let a = arm(&r, 0);
    assert_eq!(
        (a["stayed_up"].as_bool(), a["stop"].is_null()),
        (Some(false), true),
        "{a}"
    );

    let work = work_with_base("e3-failed-start");
    let sys = FakeSystem::new();
    sys.script(Script::failing("no display", None));
    let mut c = guest_ctx(&sys, &work);
    c.timing.sim_timeout = std::time::Duration::from_secs(600);
    let r = e3(&c, Graphics::On);
    let a = arm(&r, 0);
    assert_eq!(
        (a["started_ms"].is_null(), a["stayed_up"].as_bool()),
        (true, Some(false)),
        "{a}"
    );
    assert_eq!(a["steps_received"], 0);
}

/// Catches a screenshot that cannot be written being reported as written, and one that
/// replaces nothing: a file of an earlier run is replaced, a missing directory is an error
/// naming the path.
#[test]
fn the_picture_is_written_over_an_earlier_one_and_a_failed_write_is_reported() {
    use crate::e3::{picture_json, write_picture};
    let dir = crate::fake::tmp("e3-picture");
    let frame = json!({"format": "jpeg", "bytes": 4, "base64": base64::engine::general_purpose::STANDARD.encode([0xFF, 0xD8, 0xFF, 1])});
    let path = dir.join("shot.jpg");
    std::fs::write(&path, "older and longer than the new picture").expect("old");
    let j = picture_json(&frame, &path);
    assert_eq!(
        (j["bytes"].as_u64(), j["jpeg_signature"].as_bool()),
        (Some(4), Some(true)),
        "{j}"
    );
    assert_eq!(std::fs::read(&path).expect("shot"), [0xFF, 0xD8, 0xFF, 1]);
    let bad = picture_json(&frame, &dir.join("none/shot.jpg"));
    assert!(bad["written"].is_null() && bad["bytes"] == 4, "{bad}");
    assert!(
        bad["error"].as_str().expect("error").starts_with("write "),
        "{bad}"
    );
    assert!(write_picture(&dir.join("none/x"), b"x").is_err());
}

/// Catches `probe e3` reached through the gate table giving the wrong graphics device: the
/// default follows `--headless`.
#[test]
fn the_gate_table_starts_e3_with_graphics_unless_headless() {
    use crate::cli::Gate;
    for graphics in [true, false] {
        let work = work_with_base("e3-gate");
        let sys = FakeSystem::new();
        sys.script(guest_sending(&all_ok()));
        let mut c = guest_ctx(&sys, &work);
        c.graphics = graphics;
        let r = crate::gates::run(&c, Gate::E3, None);
        assert_eq!(r.probe, "e3");
        assert_eq!(sys.starts.borrow()[0].0.graphics, graphics);
    }
}

/// Catches `--graphics` being ignored when the gate is asked for through the command line.
#[test]
fn the_command_line_graphics_mode_reaches_the_probe() {
    use crate::cli::Cli;
    use clap::Parser;
    let work = work_with_base("e3-cli");
    let bin = work.join("kbf-guest-src");
    std::fs::write(&bin, "bin").expect("bin");
    let sys = FakeSystem::new();
    sys.script(guest_sending(&all_ok()));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let cli = Cli::parse_from([
        "kbf-vmm",
        "probe",
        "e3",
        "--work-dir",
        work.to_str().expect("p"),
        "--guest-binary",
        bin.to_str().expect("p"),
        "--graphics",
        "off",
    ]);
    let _ = crate::app::run(
        cli,
        Some(&sys),
        crate::probe::Timing::instant(),
        &mut out,
        &mut err,
    );
    let line: Value = serde_json::from_slice(&out).expect("one json line");
    assert_eq!(
        (line["probe"].as_str(), line["status"].as_str()),
        (Some("e3"), Some("ok")),
        "{line}"
    );
    assert!(!sys.starts.borrow()[0].0.graphics);
}

/// Catches a second guest that cannot start discarding the first guest's finished results
/// (a full run): the error report must still carry that arm.
#[test]
fn a_second_guest_that_cannot_start_keeps_the_first_guests_results() {
    // How many clones one guest takes, measured on a run of its own.
    let measure_work = work_with_base("e3-second-measure");
    let measure = FakeSystem::new();
    measure.script(guest_sending(&all_ok()));
    let r = e3(&guest_ctx(&measure, &measure_work), Graphics::Off);
    assert_eq!(r.status, ReportStatus::Ok, "{r:?}");
    let per_guest = measure.cloned.borrow().len();

    let work = work_with_base("e3-second");
    let sys = FakeSystem::new();
    sys.script(guest_sending(&all_ok()));
    sys.clone_budget.set(Some(per_guest));
    let r = e3(&guest_ctx(&sys, &work), Graphics::Both);
    assert_eq!(r.status, ReportStatus::Error, "{r:?}");
    let arms = r.details["arms"].as_array().expect("arms");
    assert_eq!(arms.len(), 1, "{r:?}");
    assert_eq!(arms[0]["name"], "e3-off");
    assert_eq!(arms[0]["summary"]["ok"], true, "{:?}", arms[0]);
}
