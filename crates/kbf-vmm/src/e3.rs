//! E3: the iOS simulator inside a macOS guest.
//!
//! One clone of the base boots (more than one with `--graphics both`, one after the other:
//! a guest with Xcode is large, and the framework runs two at most). The guest's simulator
//! daemon (`kbf-guest sim`, installed by `image setup --xcode --sim-runtime`) runs its steps
//! and sends one result frame per step, then a summary (`kbf_guest::sim`). The host
//! collects the frames as they arrive, waits for the summary, the guest's end or the
//! simulator timeout, asks the guest to shut down over vsock, and prints one JSON object:
//! for each guest every step with its status, exit code, time and output tail, the
//! summary, how the guest was stopped, and where the screenshot was written.
//!
//! The base's recorded sizes (`image base --cpus --memory-mib`) size the guest; a base
//! without them gets the probes' default of two CPUs and 4 GiB, which the report warns is
//! too little for a simulator. `docs/spikes/macos-vm-probe.md` says how to read the steps.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use base64::Engine as _;
use kbf_guest::sim::{SCREENSHOT_NAME, STEP_PREFIX, SUMMARY_NAME, Status, StepFrame, SummaryFrame};
use serde_json::{Value, json};

use crate::bundle::{self, Identity, Recorded};
use crate::cli::Graphics;
use crate::gates::{Channels, attach_share, base_missing, clean};
use crate::guest::{self, Share};
use crate::probe::{Ctx, StopKind, Vm};
use crate::report::Report;
use crate::spec::VmSpec;

const PROBE: &str = "e3";

/// What the frames the host has received say so far.
#[derive(Default)]
struct Received {
    /// Step frames by frame name (which sorts in step order).
    steps: BTreeMap<String, StepFrame>,
    summary: Option<SummaryFrame>,
    screenshot: Option<Value>,
    /// Frames that were there and could not be read: name and why.
    unreadable: Vec<(String, String)>,
    /// Names already read: a frame is made whole by a rename, so it is read once.
    seen: BTreeSet<String>,
}

impl Received {
    /// Reads the frames in `dir` (the host's own directory, never the share) that have not
    /// been read yet. Each is read without following a link and within a size cap.
    fn look(&mut self, dir: &Path) {
        for name in names(dir) {
            let wanted =
                name.starts_with(STEP_PREFIX) || name == SUMMARY_NAME || name == SCREENSHOT_NAME;
            if !wanted || self.seen.contains(&name) {
                continue;
            }
            self.seen.insert(name.clone());
            let text = match guest::read_result(&dir.join(&name)) {
                Ok(t) => t,
                Err(e) => {
                    self.unreadable.push((name, e));
                    continue;
                }
            };
            let parsed = if name == SUMMARY_NAME {
                serde_json::from_str(&text).map(|s| self.summary = Some(s))
            } else if name == SCREENSHOT_NAME {
                serde_json::from_str(&text).map(|v| self.screenshot = Some(v))
            } else {
                serde_json::from_str(&text).map(|f| {
                    self.steps.insert(name.clone(), f);
                })
            };
            if let Err(e) = parsed {
                self.unreadable
                    .push((name, format!("not the expected JSON: {e}")));
            }
        }
    }
}

/// The names in `dir`, sorted; empty when it cannot be read.
pub(crate) fn names(dir: &Path) -> Vec<String> {
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

/// A screenshot frame's picture.
#[derive(Debug, PartialEq, Eq)]
pub struct Picture {
    pub bytes: Vec<u8>,
    /// Starts with the JPEG signature.
    pub jpeg: bool,
}

/// Decodes the body of a `sim-screenshot.json` frame.
///
/// # Errors
/// It is not base64, or the length it states differs from what decoded.
pub fn decode_screenshot(frame: &Value) -> Result<Picture, String> {
    let text = frame["base64"].as_str().ok_or("no `base64` field")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|e| format!("not base64: {e}"))?;
    let stated = frame["bytes"].as_u64().ok_or("no `bytes` field")?;
    if stated != bytes.len() as u64 {
        return Err(format!(
            "the frame states {stated} bytes and holds {}",
            bytes.len()
        ));
    }
    let jpeg = bytes.starts_with(&[0xFF, 0xD8, 0xFF]);
    Ok(Picture { bytes, jpeg })
}

/// Writes `bytes` to the new file `path` (any file of ours from an earlier run is replaced;
/// a link is not followed), readable by the owner only.
pub(crate) fn write_picture(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    guest::remove_leftover(path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(bytes))
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// How the host stopped a guest.
struct Stop {
    how: StopKind,
    /// The helper's report of its `shutdown` command over vsock: whether the guest acked.
    command: Option<(bool, String)>,
}

struct Arm {
    name: String,
    graphics: bool,
    clone: PathBuf,
    share: Share,
    sized: Option<Recorded>,
    received: Received,
    stop: Option<Stop>,
}

/// Starts the clone `name`, sized as the base was built.
fn start_arm(ctx: &Ctx<'_>, name: &str, graphics: bool) -> Result<(Vm, Arm), String> {
    let share = guest::prepare_share(ctx, name)?;
    let sized = bundle::recorded_sizing(&ctx.base);
    let started = ctx.start_clone(
        name,
        Identity::Keep,
        |spec: &mut VmSpec| {
            attach_share(spec, &share, Channels::VSOCK);
            spec.graphics = graphics;
            if let Some(r) = &sized {
                spec.cpus = r.cpus;
                spec.memory_mib = r.memory_mib;
            }
        },
        None,
    );
    match started {
        Ok((vm, _)) => Ok((
            vm,
            Arm {
                name: name.to_owned(),
                graphics,
                clone: ctx.work.join(name),
                share,
                sized,
                received: Received::default(),
                stop: None,
            },
        )),
        Err(e) => {
            for suffix in ["-share", "-tagged", "-results"] {
                let _ = std::fs::remove_dir_all(ctx.work.join(format!("{name}{suffix}")));
            }
            Err(e)
        }
    }
}

/// Reads frames as they arrive until the summary has, the guest has ended or the simulator
/// timeout is over.
fn observe(ctx: &Ctx<'_>, vm: &mut Vm, arm: &mut Arm) {
    let began = Instant::now();
    loop {
        vm.watch(ctx.timing.tick, ctx.timing.tick, false);
        arm.received.look(&arm.share.received);
        if arm.received.summary.is_some()
            || vm.ended.is_some()
            || began.elapsed() >= ctx.timing.sim_timeout
        {
            // A frame that arrived with the last event.
            arm.received.look(&arm.share.received);
            return;
        }
    }
}

/// Asks the guest to shut down over vsock (the helper falls back to the framework's stop
/// request), so that its disk is flushed.
fn stop_gracefully(ctx: &Ctx<'_>, vm: &mut Vm, arm: &mut Arm) {
    if vm.ended.is_some() {
        return;
    }
    vm.request_shutdown();
    let how = vm.finish_shutdown(&ctx.timing, ctx.timing.shutdown_timeout);
    arm.stop = Some(Stop {
        how,
        command: vm.shutdown_command(),
    });
}

fn step_json(f: &StepFrame) -> Value {
    serde_json::to_value(f).unwrap_or(Value::Null)
}

/// The steps that did not succeed, as `step (how)`.
fn problems(received: &Received) -> Vec<String> {
    received
        .steps
        .values()
        .filter(|f| f.status != Status::Ok)
        .map(|f| match f.status {
            Status::Failed => match (f.exit, f.signal) {
                (Some(code), _) => format!("{} (exit {code})", f.step),
                (None, Some(sig)) => format!("{} (signal {sig})", f.step),
                (None, None) => format!("{} (failed)", f.step),
            },
            Status::Timeout => format!("{} (timeout)", f.step),
            Status::Error => format!("{} (error)", f.step),
            Status::Skipped | Status::Ok => format!("{} (skipped)", f.step),
        })
        .collect()
}

fn arm_json(ctx: &Ctx<'_>, vm: &Vm, arm: &Arm, picture: &Value) -> Value {
    let r = &arm.received;
    let ok = r.steps.values().filter(|f| f.status == Status::Ok).count();
    json!({
        "name": arm.name,
        "graphics": arm.graphics,
        "clone": arm.clone,
        "share": arm.share.dir,
        "sizing": arm.sized.map(|s| json!({"cpus": s.cpus, "memory_mib": s.memory_mib, "disk_gib": s.disk_gib})),
        "sizing_warning": if arm.sized.is_none() {
            json!("the base records no sizes (built before `image base --cpus --memory-mib`): the guest ran with the probes' default of 2 CPUs and 4 GiB, which a simulator does not fit")
        } else { Value::Null },
        "started_ms": vm.started_ms(),
        "stayed_up": vm.started_ms().is_some() && vm.ended.is_none(),
        "steps_received": r.steps.len(),
        "steps_ok": ok,
        "problems": problems(r),
        "steps": r.steps.values().map(step_json).collect::<Vec<_>>(),
        "summary": r.summary,
        "summary_received": r.summary.is_some(),
        "screenshot": picture,
        "unreadable_frames": r.unreadable.iter().map(|(n, e)| json!({"frame": n, "error": e})).collect::<Vec<_>>(),
        "stop": arm.stop.as_ref().map(|stop| json!({
            "how": stop.how.name(),
            "vsock_acked": stop.command.as_ref().map(|(acked, _)| acked),
            "vsock_detail": stop.command.as_ref().map(|(_, detail)| detail),
        })),
        "trace": vm.trace_json(),
        "results_files": names(&arm.share.received),
        "helper_log": vm.proc.diagnostics(),
        "harvest": format!(
            "sudo kbf-vmm image harvest --bundle {} --out {}-harvest",
            arm.clone.display(),
            arm.clone.display()
        ),
        "kept": ctx.keep,
    })
}

/// Decodes the screenshot frame and writes the picture to `path`; what happened, as JSON.
pub(crate) fn picture_json(frame: &Value, path: &Path) -> Value {
    match decode_screenshot(frame) {
        Err(e) => json!({"written": null, "error": e}),
        Ok(p) => match write_picture(path, &p.bytes) {
            Ok(()) => json!({"written": path, "bytes": p.bytes.len(), "jpeg_signature": p.jpeg}),
            Err(e) => json!({"written": null, "bytes": p.bytes.len(), "error": e}),
        },
    }
}

/// The screenshot the guest sent, written next to the clones; what happened, as JSON.
fn save_picture(ctx: &Ctx<'_>, arm: &Arm) -> Value {
    arm.received
        .screenshot
        .as_ref()
        .map_or(Value::Null, |frame| {
            picture_json(
                frame,
                &ctx.work.join(format!("{}-screenshot.jpg", arm.name)),
            )
        })
}

fn one_line(arm: &Value) -> String {
    let kind = if arm["graphics"] == json!(true) {
        "graphics"
    } else {
        "headless"
    };
    let name = arm["name"].as_str().unwrap_or_default();
    if arm["steps_received"] == json!(0) {
        return format!(
            "{name} ({kind}): no step frame arrived (is the base set up with `image setup --xcode --sim-runtime`, and, with `--guest-user`, primed with `image prime`?)"
        );
    }
    let problems: Vec<&str> = arm["problems"]
        .as_array()
        .map_or_else(Vec::new, |a| a.iter().filter_map(Value::as_str).collect());
    let shot = arm["screenshot"]["bytes"].as_u64().map_or_else(
        || "no screenshot".to_owned(),
        |b| format!("screenshot {b} bytes"),
    );
    let summary = if arm["summary_received"] == json!(true) {
        ""
    } else {
        ", no summary (the run did not end)"
    };
    format!(
        "{name} ({kind}): {} of {} steps ok{}, {shot}{summary}",
        arm["steps_ok"],
        arm["steps_received"],
        if problems.is_empty() {
            String::new()
        } else {
            format!("; not ok: {}", problems.join(", "))
        },
    )
}

/// E3: see the module documentation.
#[must_use]
pub fn e3(ctx: &Ctx<'_>, graphics: Graphics) -> Report {
    if let Some(r) = base_missing(ctx, PROBE) {
        return r;
    }
    if ctx.guest_binary.is_none() {
        return Report::needs_manual(
            PROBE,
            "e3 reads the frames the guest's simulator daemon sends, so it needs --guest-binary (a kbf-guest built for macOS) and a base set up with `image setup --guest-binary --xcode --sim-runtime`",
            json!({"guest_binary": null}),
        );
    }
    let wanted: &[(&str, bool)] = match graphics {
        Graphics::On => &[("e3-on", true)],
        Graphics::Off => &[("e3-off", false)],
        Graphics::Both => &[("e3-off", false), ("e3-on", true)],
    };
    let mut arms = Vec::new();
    for (name, with_graphics) in wanted {
        let (mut vm, mut arm) = match start_arm(ctx, name, *with_graphics) {
            Ok(started) => started,
            Err(e) => {
                // The arms that finished cost a full run each: they stay in the report.
                let mut r = Report::error(PROBE, e);
                r.details = json!({"arms": arms, "clones_kept": ctx.keep});
                return r;
            }
        };
        observe(ctx, &mut vm, &mut arm);
        stop_gracefully(ctx, &mut vm, &mut arm);
        let picture = save_picture(ctx, &arm);
        let report = arm_json(ctx, &vm, &arm, &picture);
        clean(ctx, std::slice::from_mut(&mut vm));
        arms.push(report);
    }
    let summary = arms.iter().map(one_line).collect::<Vec<_>>().join("; ");
    Report::ok(
        PROBE,
        summary,
        json!({
            "arms": arms,
            "clones_kept": ctx.keep,
            "next": if ctx.keep {
                "run the `harvest` command of each arm as root to read the guest's own logs (/var/log/com.kbf.sim.log holds the daemon's)"
            } else {
                "the clones were removed; run again with --keep to harvest the guests' disks"
            },
        }),
    )
}
