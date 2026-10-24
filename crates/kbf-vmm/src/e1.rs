//! E1: why a booted guest never reaches its own userspace.
//!
//! Two clones of the base boot at once, one headless and one (with `--graphics`) with a
//! graphics device. For the whole guest wait the host records every helper event with
//! its time, samples the helper's CPU, the framework's VM service processes and the
//! clone's disk allocation, and watches for the frames the launch daemons of the guest
//! deliver over vsock (two ready markers, two probe answers and the share diagnosis; the
//! report gives the time of each and lists the missing ones, and the first to arrive is the
//! guest marker). The run ends when every frame has come or the guest wait is over, and the
//! guests are then asked to shut down before they are force-stopped. With `--keep` the clones stay, and `image harvest` then reads
//! the guest's own logs off their disks. `docs/spikes/macos-vm-probe.md` maps each
//! outcome to a cause.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::bundle::{self, Identity};
use crate::gates::{Channels, attach_share, base_missing, clean};
use crate::guest::{self, Share};
use crate::matrix::{self, Prepared};
use crate::probe::{Ctx, StopKind, Vm};
use crate::report::Report;
use crate::sampler::{self, Sample};
use crate::spec::VmSpec;

/// One guest of the pair.
struct Arm {
    name: String,
    graphics: bool,
    clone: PathBuf,
    share: Share,
    /// The matrix shares of this VM (empty without `--matrix`).
    matrix: Vec<Prepared>,
    samples: Vec<Sample>,
    /// Every expected frame that has arrived, with the host time it was first seen at, in
    /// arrival order. The first is the guest marker.
    frames: Vec<(&'static str, u64)>,
    /// How the VM ended when the host stopped it, and how long that took.
    stop: Option<(StopKind, u64)>,
}

/// Every frame a healthy guest delivers over vsock: the system and login markers, the
/// two probe answers and the share diagnosis. A run waits for all of them, within the
/// guest timeout, and reports the ones that never came.
const EXPECTED: [&str; 5] = [
    guest::READY_SYSTEM,
    guest::READY_LOGIN,
    guest::ROOT_RESULT,
    guest::USER_RESULT,
    kbf_guest::diag::REPORT_NAME,
];

impl Arm {
    /// Records the expected frames that are in the directory the host received them in and
    /// not yet seen, at host time `ms`. The share is not looked at: a file the guest puts
    /// there is not a frame it sent over vsock, and a guest can write anything to it.
    fn look(&mut self, ms: u64) {
        let dir = &self.share.received;
        for name in EXPECTED {
            let seen = self.frames.iter().any(|(n, _)| *n == name);
            if !seen && std::fs::symlink_metadata(dir.join(name)).is_ok() {
                self.frames.push((name, ms));
            }
        }
    }

    fn complete(&self) -> bool {
        self.frames.len() == EXPECTED.len()
    }

    fn missing(&self) -> Vec<&'static str> {
        EXPECTED
            .into_iter()
            .filter(|n| !self.frames.iter().any(|(f, _)| f == n))
            .collect()
    }
}

/// The names in `dir`, sorted; empty when it cannot be read.
fn names(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

/// Removes the share directories of `name` (a start that failed leaves them).
fn remove_shares(ctx: &Ctx<'_>, name: &str) {
    for suffix in ["-share", "-tagged", "-results"] {
        let _ = std::fs::remove_dir_all(ctx.work.join(format!("{name}{suffix}")));
    }
}

/// Starts the clone `name` with its share and, when `graphics`, a graphics device.
fn start_arm(ctx: &Ctx<'_>, name: &str, graphics: bool) -> Result<(Vm, Arm), String> {
    let share = guest::prepare_share(ctx, name)?;
    let shares = if ctx.matrix {
        matrix::prepare(ctx, name)
    } else {
        Vec::new()
    };
    let started = matrix::write_manifest(&share.dir, &shares).and_then(|()| {
        ctx.start_clone(
            name,
            Identity::Keep,
            |spec: &mut VmSpec| {
                attach_share(spec, &share, Channels::VSOCK);
                matrix::attach(spec, &shares);
                spec.graphics = graphics;
            },
            None,
        )
    });
    let vm = match started {
        Ok((vm, _)) => vm,
        Err(e) => {
            matrix::remove(ctx, &shares, false);
            return Err(e);
        }
    };
    let clone = ctx.work.join(name);
    Ok((
        vm,
        Arm {
            name: name.to_owned(),
            graphics,
            clone,
            share,
            matrix: shares,
            samples: Vec::new(),
            frames: Vec::new(),
            stop: None,
        },
    ))
}

/// Takes one sample of every guest from one reading of the process table. An unreadable
/// table is returned as an error text and the samples carry the disk only.
fn sample_all(ctx: &Ctx<'_>, vms: &[Vm], arms: &mut [Arm]) -> Option<String> {
    let (table, error) = match ctx.sys.process_table() {
        Ok(t) => (t, None),
        Err(e) => (String::new(), Some(e)),
    };
    for (vm, arm) in vms.iter().zip(arms.iter_mut()) {
        let disk = arm.clone.join(bundle::DISK);
        arm.samples
            .push(Sample::take(&table, vm.proc.pid(), &disk, vm.ms()));
    }
    error
}

/// Watches both guests until each has delivered every expected frame or ended, or the
/// guest wait is over; samples on a fixed beat, once at the start and once at the end.
fn observe(ctx: &Ctx<'_>, vms: &mut [Vm], arms: &mut [Arm]) -> Option<String> {
    let began = Instant::now();
    let mut next = Duration::ZERO;
    let mut table_error = None;
    loop {
        if began.elapsed() >= next {
            table_error = sample_all(ctx, vms, arms).or(table_error);
            next += ctx.timing.sample_every;
        }
        for (vm, arm) in vms.iter_mut().zip(arms.iter_mut()) {
            if arm.complete() {
                continue;
            }
            if vm.ended.is_none() {
                vm.watch(ctx.timing.tick, ctx.timing.tick, false);
            }
            arm.look(vm.ms());
        }
        let settled = vms
            .iter()
            .zip(arms.iter())
            .all(|(vm, arm)| arm.complete() || vm.ended.is_some());
        if settled || began.elapsed() >= ctx.timing.guest_timeout {
            break;
        }
    }
    sample_all(ctx, vms, arms).or(table_error)
}

/// Asks every guest to shut down at once, then gives each up to the shutdown timeout, so
/// that a guest's disk is flushed before the clone is harvested; a guest that does not
/// obey is force-stopped. The helper's request is `requestStopWithError` (the guest's
/// power-button request), not the framework's force stop.
fn stop_gracefully(ctx: &Ctx<'_>, vms: &mut [Vm], arms: &mut [Arm]) {
    for vm in vms.iter_mut().filter(|vm| vm.ended.is_none()) {
        vm.request_shutdown();
    }
    for (vm, arm) in vms.iter_mut().zip(arms.iter_mut()) {
        if vm.ended.is_some() {
            continue;
        }
        let asked = vm.ms();
        let how = vm.finish_shutdown(&ctx.timing);
        arm.stop = Some((how, vm.ms().saturating_sub(asked)));
    }
}

fn arm_json(ctx: &Ctx<'_>, vm: &Vm, arm: &Arm) -> Value {
    json!({
        "name": arm.name,
        "graphics": arm.graphics,
        "clone": arm.clone,
        "share": arm.share.dir,
        "started_ms": vm.started_ms(),
        "stayed_up": vm.started_ms().is_some() && vm.ended.is_none(),
        "guest_marker": arm.frames.first().map(|(name, _)| *name),
        "guest_marker_ms": arm.frames.first().map(|(_, ms)| *ms),
        "frames": arm.frames.iter().map(|(n, ms)| json!({"name": n, "ms": ms})).collect::<Vec<_>>(),
        "frames_missing": arm.missing(),
        "stop": arm.stop.map(|(how, ms)| json!({"how": how.name(), "ms": ms})),
        "trace": vm.trace_json(),
        "samples": arm.samples.iter().map(Sample::json).collect::<Vec<_>>(),
        "activity": sampler::summarize(&arm.samples),
        "results_files": names(&arm.share.received),
        "share_files": names(&arm.share.results()),
        "matrix_host": matrix::host_listing(&arm.matrix),
        "helper_log": vm.proc.diagnostics(),
        "harvest": format!(
            "sudo kbf-vmm image harvest --bundle {} --out {}-harvest",
            arm.clone.display(),
            arm.clone.display()
        ),
        "kept": ctx.keep,
    })
}

fn one_line(arm: &Value) -> String {
    format!(
        "{} ({}): started {}, guest marker {}, frames missing {}, disk blocks grown {}, ended {}",
        arm["name"].as_str().unwrap_or_default(),
        if arm["graphics"] == json!(true) {
            "graphics"
        } else {
            "headless"
        },
        arm["started_ms"],
        match arm["guest_marker"].as_str() {
            Some(name) => format!("{name} at {}", arm["guest_marker_ms"]),
            None => "none".to_owned(),
        },
        arm["frames_missing"],
        arm["activity"]["disk_blocks_512_grown"],
        arm["trace"]["ended"],
    )
}

/// E1: see the module documentation.
#[must_use]
pub fn e1(ctx: &Ctx<'_>) -> Report {
    if let Some(r) = base_missing(ctx, "e1") {
        return r;
    }
    if ctx.guest_binary.is_none() {
        return Report::needs_manual(
            "e1",
            "e1 looks for the marker a launch daemon of the guest writes, so it needs --guest-binary (a kbf-guest built for macOS) and a base with the probe launch daemons (`image setup`)",
            json!({"guest_binary": null}),
        );
    }
    let mut vms = Vec::new();
    let mut arms = Vec::new();
    for (name, graphics) in [("e1-x", false), ("e1-y", ctx.graphics)] {
        match start_arm(ctx, name, graphics) {
            Ok((vm, arm)) => {
                vms.push(vm);
                arms.push(arm);
            }
            Err(e) => {
                clean(ctx, &mut vms);
                for arm in &arms {
                    matrix::remove(ctx, &arm.matrix, false);
                }
                if !ctx.keep {
                    remove_shares(ctx, name);
                }
                return Report::error("e1", e);
            }
        }
    }
    let table_error = observe(ctx, &mut vms, &mut arms);
    stop_gracefully(ctx, &mut vms, &mut arms);
    let arm_reports: Vec<Value> = vms
        .iter()
        .zip(&arms)
        .map(|(vm, arm)| arm_json(ctx, vm, arm))
        .collect();
    clean(ctx, &mut vms);
    for arm in &arms {
        matrix::remove(ctx, &arm.matrix, ctx.keep);
    }
    let summary = arm_reports
        .iter()
        .map(one_line)
        .collect::<Vec<_>>()
        .join("; ");
    Report::ok(
        "e1",
        summary,
        json!({
            "arms": arm_reports,
            "process_table_error": table_error,
            "clones_kept": ctx.keep,
            "next": if ctx.keep {
                "run the `harvest` command of each arm as root, then read the results as docs/spikes/macos-vm-probe.md says"
            } else {
                "the clones were removed; run again with --keep to harvest the guests' disks"
            },
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeSystem, Script, ctx, work_with_base};
    use crate::report::Status;
    use crate::spec::Event;
    use crate::system::Poll;

    fn guest_ctx<'a>(sys: &'a FakeSystem, work: &Path) -> Ctx<'a> {
        let mut c = ctx(sys, work);
        let bin = work.join("kbf-guest-src");
        std::fs::write(&bin, "bin").expect("bin");
        c.guest_binary = Some(bin);
        c
    }

    /// A booted guest that has already delivered every expected frame.
    fn complete_guest() -> Script {
        EXPECTED
            .into_iter()
            .fold(Script::booted(), |s, f| s.with_guest_file(f, "{}"))
    }

    fn arm(r: &Report, i: usize) -> &Value {
        &r.details["arms"][i]
    }

    /// Catches an E1 that needs nothing it cannot use: without a guest binary the marker
    /// cannot arrive, so it says what to give instead of reporting a silent guest.
    #[test]
    fn without_a_guest_binary_e1_says_what_is_missing_and_starts_nothing() {
        let work = work_with_base("e1-nobin");
        let sys = FakeSystem::new();
        let r = e1(&ctx(&sys, &work));
        assert_eq!(r.status, Status::NeedsManual);
        assert!(r.reason.expect("reason").contains("--guest-binary"));
        assert!(sys.starts.borrow().is_empty());
    }

    /// Catches a pair that is not started together, a graphics flag that reaches the wrong
    /// VM (the first must stay headless), shares that are not attached, and a pair without
    /// `--graphics` that still gets a display.
    #[test]
    fn two_guests_start_together_and_only_the_second_may_have_graphics() {
        let work = work_with_base("e1-start");
        let sys = FakeSystem::new();
        let mut c = guest_ctx(&sys, &work);
        c.graphics = true;
        let r = e1(&c);
        assert_eq!(r.status, Status::Ok, "{r:?}");
        let starts = sys.starts.borrow();
        assert_eq!(starts.len(), 2);
        assert_eq!((starts[0].0.graphics, starts[1].0.graphics), (false, true));
        assert!(starts.iter().all(|(s, _)| s.automount_dir.is_some()));
        assert!(starts.iter().all(|(_, uid)| uid.is_none()));
        assert_eq!(arm(&r, 0)["graphics"], false);
        assert_eq!(arm(&r, 1)["graphics"], true);
        drop(starts);
        sys.starts.borrow_mut().clear();
        c.graphics = false;
        // The first run removed its clones; a new run may use the same names.
        let r = e1(&c);
        assert_eq!(r.status, Status::Ok);
        assert!(sys.starts.borrow().iter().all(|(s, _)| !s.graphics));
    }

    /// Catches an E1 whose guests cannot send their markers over vsock: each arm gets the
    /// vsock listener and a results directory that is its own share's `results/`, so the
    /// marker the guest sends lands where the host looks for it.
    #[test]
    fn each_arm_gets_its_own_results_directory_behind_the_vsock_listener() {
        let work = work_with_base("e1-vsock");
        let sys = FakeSystem::new();
        let r = e1(&guest_ctx(&sys, &work));
        assert_eq!(r.status, Status::Ok, "{r:?}");
        let starts = sys.starts.borrow();
        for (spec, name) in starts.iter().map(|(s, _)| s).zip(["e1-x", "e1-y"]) {
            assert_eq!(spec.vsock_ports, [crate::guest::VSOCK_PORT]);
            assert_eq!(spec.results_dir, Some(work.join(format!("{name}-results"))));
        }
    }

    /// Catches a report that drops the helper's events or their times: a silent guest must
    /// still show its started event and the beat of samples, with disk and CPU read.
    #[test]
    fn a_silent_guest_is_reported_with_its_events_samples_and_no_marker() {
        let work = work_with_base("e1-silent");
        let sys = FakeSystem::new();
        *sys.table.borrow_mut() = Ok(
            "1000 0:02.50 /usr/local/kbf-vmm/kbf-vmm\n1001 0:00.10 /usr/local/kbf-vmm/kbf-vmm\n900 0:30.00 /x/com.apple.Virtualization.VirtualMachine\n"
                .to_owned(),
        );
        let r = e1(&guest_ctx(&sys, &work));
        assert_eq!(r.status, Status::Ok);
        let x = arm(&r, 0);
        assert!(x["guest_marker_ms"].is_null());
        assert_eq!(x["started_ms"], 100);
        assert_eq!(x["stayed_up"], true);
        let ev = &x["trace"]["events"][0];
        assert_eq!(ev["event"]["event"], "started");
        assert!(ev["host_ms"].is_u64());
        let samples = x["samples"].as_array().expect("samples");
        assert!(samples.len() >= 2, "a first and a last sample at least");
        assert_eq!(samples[0]["helper_cpu_ms"], 2500);
        assert_eq!(arm(&r, 1)["samples"][0]["helper_cpu_ms"], 100);
        assert_eq!(samples[0]["vm_services"][0]["pid"], 900);
        assert!(samples[0]["disk_blocks_512"].as_u64().is_some());
        assert!(r.details["process_table_error"].is_null());
        let summary = r.summary.expect("summary");
        assert!(summary.contains("e1-x (headless)"), "{summary}");
        assert!(summary.contains("e1-y (headless)"), "{summary}");
    }

    /// Catches a wait that sits out the whole guest timeout after the VM stopped, and
    /// loses the reason: a stop event ends that arm at once, is reported with its time,
    /// and the other arm's marker is still found.
    #[test]
    fn a_stopped_vm_ends_its_wait_and_is_reported_while_the_other_arm_is_judged() {
        let work = work_with_base("e1-stopped");
        let sys = FakeSystem::new();
        sys.script(Script::booted().then(Poll::Event(Event::Stopped { ms: 7 })));
        sys.script(complete_guest());
        let mut c = guest_ctx(&sys, &work);
        c.timing.guest_timeout = Duration::from_secs(60);
        let began = Instant::now();
        let r = e1(&c);
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "did not wait out the timeout"
        );
        let x = arm(&r, 0);
        assert_eq!(x["stayed_up"], false);
        assert_eq!(x["trace"]["ended"], "the VM stopped");
        assert_eq!(x["trace"]["events"][1]["event"]["event"], "stopped");
        assert!(x["guest_marker_ms"].is_null());
        let y = arm(&r, 1);
        assert!(y["guest_marker_ms"].is_u64());
        assert_eq!(y["results_files"].as_array().map(Vec::len), Some(5));
        assert!(r.summary.expect("summary").contains("the VM stopped"));
    }

    /// Catches a guest marker that rests on the system job alone: the fifth Studio run had
    /// a headless guest deliver the login marker and both probe results while its system
    /// job never started, and the arm was reported silent. Any frame the guest delivers
    /// proves its userspace ran, and the report names which one arrived first.
    #[test]
    fn any_frame_the_guest_delivers_is_its_marker_and_the_report_names_it() {
        let work = work_with_base("e1-any-marker");
        let sys = FakeSystem::new();
        sys.script(Script::booted().with_guest_file(guest::READY_LOGIN, "{}"));
        sys.script(Script::booted().with_guest_file(guest::USER_RESULT, "{}"));
        let r = e1(&guest_ctx(&sys, &work));
        assert_eq!(arm(&r, 0)["guest_marker"], guest::READY_LOGIN);
        assert!(arm(&r, 0)["guest_marker_ms"].is_u64());
        assert_eq!(arm(&r, 1)["guest_marker"], guest::USER_RESULT);
        assert!(arm(&r, 1)["guest_marker_ms"].is_u64());
        let summary = r.summary.expect("summary");
        assert!(summary.contains("marker ready-login"), "{summary}");
        assert!(
            summary.contains("marker guest-probe-user.json"),
            "{summary}"
        );
        let work = work_with_base("e1-other-file");
        let sys = FakeSystem::new();
        sys.script(Script::booted().with_guest_file("vsock-test.log", "x"));
        let r = e1(&guest_ctx(&sys, &work));
        assert!(arm(&r, 0)["guest_marker"].is_null());
        assert!(arm(&r, 0)["guest_marker_ms"].is_null());
    }

    /// Catches a run that stops at the first frame, as run 6 did (three samples, then the
    /// VMs were stopped before the root probe, the login marker and the diagnosis came):
    /// the later frames arrive on their own polls, the run waits for them, and each is
    /// reported in arrival order with a time that does not go back, none missing.
    #[test]
    fn the_run_waits_for_every_frame_and_reports_the_time_of_each() {
        let work = work_with_base("e1-all-frames");
        let sys = FakeSystem::new();
        let guest = || {
            [
                (guest::READY_LOGIN, 20),
                (guest::ROOT_RESULT, 40),
                (guest::USER_RESULT, 60),
                (kbf_guest::diag::REPORT_NAME, 80),
            ]
            .into_iter()
            .fold(
                Script::booted().with_guest_file(guest::READY_SYSTEM, "{}"),
                |s, (name, after)| s.delivering_after(after, name, "{}"),
            )
        };
        sys.script(guest());
        sys.script(guest());
        let mut c = guest_ctx(&sys, &work);
        // Long enough never to be the reason the run ends; the frames end it.
        c.timing.guest_timeout = Duration::from_secs(600);
        let r = e1(&c);
        for i in 0..2 {
            let a = arm(&r, i);
            assert_eq!(a["frames_missing"], json!([]));
            let names: Vec<&str> = a["frames"]
                .as_array()
                .expect("frames")
                .iter()
                .map(|f| f["name"].as_str().expect("name"))
                .collect();
            assert_eq!(
                names,
                [
                    guest::READY_SYSTEM,
                    guest::READY_LOGIN,
                    guest::ROOT_RESULT,
                    guest::USER_RESULT,
                    kbf_guest::diag::REPORT_NAME
                ]
            );
            let times: Vec<u64> = a["frames"]
                .as_array()
                .expect("frames")
                .iter()
                .map(|f| f["ms"].as_u64().expect("ms"))
                .collect();
            assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
            assert_eq!(a["guest_marker"], guest::READY_SYSTEM);
        }
        let summary = r.summary.expect("summary");
        assert!(summary.contains("frames missing []"), "{summary}");
    }

    /// Catches a guest that fakes frames: a file the guest puts in the share is not a frame
    /// it sent over vsock (the share is the guest's to write), so it is not counted, is
    /// still listed as missing, and is shown apart from what was received.
    #[test]
    fn files_a_guest_puts_in_the_share_are_not_frames() {
        let work = work_with_base("e1-spoof");
        let sys = FakeSystem::new();
        let mut spoof = Script::booted();
        for f in EXPECTED {
            spoof = spoof.with_share_file(f, "{}");
        }
        sys.script(spoof);
        sys.script(Script::booted().with_guest_file(guest::READY_LOGIN, "{}"));
        let r = e1(&guest_ctx(&sys, &work));
        let x = arm(&r, 0);
        assert!(x["guest_marker"].is_null());
        assert_eq!(x["frames"], json!([]));
        assert_eq!(x["frames_missing"].as_array().map(Vec::len), Some(5));
        assert_eq!(x["results_files"], json!([]));
        assert_eq!(x["share_files"].as_array().map(Vec::len), Some(5));
        let y = arm(&r, 1);
        assert_eq!(y["guest_marker"], guest::READY_LOGIN);
        assert_eq!(y["share_files"], json!([]));
    }

    /// Catches a report that hides what did not arrive: the frames that never came are
    /// listed by name, in the order they are expected, and the ones that came are not.
    #[test]
    fn the_frames_that_never_came_are_listed_by_name() {
        let work = work_with_base("e1-missing");
        let sys = FakeSystem::new();
        sys.script(
            Script::booted()
                .with_guest_file(guest::READY_SYSTEM, "{}")
                .with_guest_file(guest::USER_RESULT, "{}"),
        );
        let r = e1(&guest_ctx(&sys, &work));
        assert_eq!(
            arm(&r, 0)["frames_missing"],
            json!([
                guest::READY_LOGIN,
                guest::ROOT_RESULT,
                kbf_guest::diag::REPORT_NAME
            ])
        );
        assert_eq!(
            arm(&r, 1)["frames_missing"].as_array().map(Vec::len),
            Some(5)
        );
        let summary = r.summary.expect("summary");
        assert!(summary.contains("ready-login"), "{summary}");
    }

    /// Catches a stop that is only a force stop, which leaves the guest's disk unflushed
    /// (run 6: every guest log empty in the harvest). The guest is asked to shut down
    /// first and the report says how each VM ended: shut down, force-stopped after it
    /// ignored the request, or killed when the helper would not exit either; a VM that had
    /// already ended is not asked.
    #[test]
    fn the_guests_are_asked_to_shut_down_before_they_are_forced() {
        let work = work_with_base("e1-shutdown");
        let sys = FakeSystem::new();
        sys.script(Script::booted());
        sys.script(Script::booted().deaf());
        let r = e1(&guest_ctx(&sys, &work));
        assert_eq!(arm(&r, 0)["stop"]["how"], "guest shut down");
        assert_eq!(
            arm(&r, 1)["stop"]["how"],
            "force-stopped after the guest did not shut down"
        );
        assert!(arm(&r, 1)["stop"]["ms"].is_u64());
        let work = work_with_base("e1-shutdown-kill");
        let sys = FakeSystem::new();
        let mut stuck = Script::booted().deaf();
        stuck.exits_on_stop = false;
        sys.script(stuck);
        sys.script(Script::booted().then(Poll::Event(Event::Stopped { ms: 7 })));
        let r = e1(&guest_ctx(&sys, &work));
        assert_eq!(arm(&r, 0)["stop"]["how"], "helper killed");
        assert!(arm(&r, 1)["stop"].is_null());
    }

    /// Catches an arm that has already written its marker being watched (and its helper
    /// polled) again while the other arm is still silent, and a start that failed being
    /// reported as a VM that stayed up.
    #[test]
    fn a_settled_arm_is_left_alone_while_the_other_is_still_waited_for() {
        let work = work_with_base("e1-one-silent");
        let sys = FakeSystem::new();
        sys.script(Script::booted());
        sys.script(Script::booted().with_guest_file(guest::READY_SYSTEM, "{}"));
        let r = e1(&guest_ctx(&sys, &work));
        assert!(arm(&r, 0)["guest_marker_ms"].is_null());
        assert!(arm(&r, 1)["guest_marker_ms"].is_u64());
        assert_eq!(arm(&r, 0)["stayed_up"], true);
        let work = work_with_base("e1-never-started");
        let sys = FakeSystem::new();
        sys.script(Script::failing("no display", None));
        let r = e1(&guest_ctx(&sys, &work));
        let x = arm(&r, 0);
        assert!(x["started_ms"].is_null());
        assert_eq!(x["stayed_up"], false);
        assert_eq!(x["trace"]["ended"], "no display");
    }

    /// Catches a helper that died without a word being lost: its exit and log tail are the
    /// reason, and the arm is reported not up.
    #[test]
    fn a_helper_that_exits_is_reported_with_its_log() {
        let work = work_with_base("e1-gone");
        let sys = FakeSystem::new();
        sys.script(
            Script::booted()
                .then(Poll::Gone {
                    code: Some(1),
                    signal: None,
                })
                .with_log("framework said no"),
        );
        let r = e1(&guest_ctx(&sys, &work));
        let x = arm(&r, 0);
        assert_eq!(x["stayed_up"], false);
        assert!(
            x["trace"]["ended"]
                .as_str()
                .expect("why")
                .contains("framework said no")
        );
        assert_eq!(x["helper_log"], "framework said no");
    }

    /// Catches `--keep` removing the clones (the harvest would have nothing to read) and
    /// its absence leaving them behind, and a report that does not say which happened.
    #[test]
    fn keep_leaves_the_clones_and_shares_and_without_it_they_are_removed() {
        let work = work_with_base("e1-keep");
        let sys = FakeSystem::new();
        let mut c = guest_ctx(&sys, &work);
        c.keep = true;
        let r = e1(&c);
        for n in ["e1-x", "e1-y"] {
            assert!(work.join(n).join("disk.img").is_file(), "{n}");
            assert!(work.join(format!("{n}-share")).is_dir(), "{n}");
        }
        assert_eq!(r.details["clones_kept"], true);
        assert!(
            arm(&r, 0)["harvest"]
                .as_str()
                .expect("cmd")
                .contains("image harvest --bundle")
        );
        assert!(
            r.details["next"]
                .as_str()
                .expect("next")
                .contains("harvest")
        );
        // The same names again: a kept clone is not overwritten.
        let again = e1(&c);
        assert_eq!(again.status, Status::Error);
        assert!(again.reason.expect("reason").contains("exists"));
        let work = work_with_base("e1-nokeep");
        let sys = FakeSystem::new();
        let r = e1(&guest_ctx(&sys, &work));
        assert!(!work.join("e1-x").exists());
        assert!(!work.join("e1-y-share").exists());
        assert_eq!(r.details["clones_kept"], false);
        assert!(r.details["next"].as_str().expect("next").contains("--keep"));
    }

    /// Catches an unreadable process table failing the whole run, or being silently
    /// ignored: the disk is still sampled and the failure is in the report.
    #[test]
    fn an_unreadable_process_table_is_reported_and_the_disk_is_still_sampled() {
        let work = work_with_base("e1-ps");
        let sys = FakeSystem::new();
        *sys.table.borrow_mut() = Err("ps not found".into());
        let r = e1(&guest_ctx(&sys, &work));
        assert_eq!(r.status, Status::Ok);
        assert_eq!(r.details["process_table_error"], "ps not found");
        let s = &arm(&r, 0)["samples"][0];
        assert!(s["helper_cpu_ms"].is_null());
        assert!(s["disk_blocks_512"].as_u64().is_some());
    }

    /// Catches a start failure that leaves the first guest running or its clone behind,
    /// and one that is not reported as an error.
    #[test]
    fn a_failed_start_stops_what_was_started_and_is_an_error() {
        let work = work_with_base("e1-fail");
        let sys = FakeSystem::new();
        sys.clone_budget.set(Some(1));
        let r = e1(&guest_ctx(&sys, &work));
        assert_eq!(r.status, Status::Error);
        assert!(!work.join("e1-y").exists());
        assert!(
            !work.join("e1-x").exists(),
            "the first clone was cleaned up"
        );
        assert!(
            !work.join("e1-y-share").exists(),
            "so was the failed one's share"
        );
    }

    /// Catches a matrix that is not attached to both guests (each guest needs its own host
    /// directories), a manifest the guest cannot read, host listings missing from the
    /// report, scratch left in the work directory after a run without `--keep`, and
    /// matrix shares attached when the matrix is off.
    #[test]
    fn the_matrix_is_attached_listed_and_cleaned_up() {
        let work = work_with_base("e1-matrix");
        let sys = FakeSystem::new();
        let mut c = guest_ctx(&sys, &work);
        c.matrix = true;
        c.keep = true;
        let r = e1(&c);
        assert_eq!(r.status, Status::Ok, "{r:?}");
        let starts = sys.starts.borrow();
        for (i, name) in ["e1-x", "e1-y"].iter().enumerate() {
            let tagged = &starts[i].0.tagged;
            // The main share and the probe share come first, then the matrix.
            assert_eq!(tagged[0].tag, guest::SHARE_TAG);
            assert_eq!(tagged[1].tag, guest::TAG);
            assert!(tagged.len() > 2 && tagged[2..].iter().all(|t| t.tag.starts_with("kbfmx")));
            let manifest =
                std::fs::read_to_string(work.join(format!("{name}-share")).join(matrix::MANIFEST))
                    .expect("manifest");
            let rows: Value = serde_json::from_str(&manifest).expect("json");
            assert_eq!(rows.as_array().expect("rows").len(), tagged.len() - 2);
            let listed = arm(&r, i)["matrix_host"].as_array().expect("listing");
            assert_eq!(listed.len(), 8, "every variant is listed, skipped or not");
            assert!(
                listed
                    .iter()
                    .any(|row| row["name"] == "base" && row["entries"] == json!([]))
            );
        }
        // Kept: the work directories stay for inspection.
        assert!(work.join("kbf-e1-x-mx-base").is_dir());
        drop(starts);
        let work = work_with_base("e1-matrix-clean");
        let mut c = guest_ctx(&sys, &work);
        c.matrix = true;
        assert_eq!(e1(&c).status, Status::Ok);
        assert!(!work.join("kbf-e1-x-mx-base").exists());
        assert!(!work.join("kbf-e1-y-mx-mode755").exists());
        // Off: only the two shares, and no listing rows.
        sys.starts.borrow_mut().clear();
        let work = work_with_base("e1-nomatrix");
        let mut c = guest_ctx(&sys, &work);
        c.matrix = false;
        let r = e1(&c);
        assert!(sys.starts.borrow().iter().all(|(s, _)| s.tagged.len() == 2));
        assert_eq!(arm(&r, 0)["matrix_host"], json!([]));
    }
}
