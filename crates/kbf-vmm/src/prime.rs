//! `image prime`: the one boot of the base bundle itself in which the guest gets its user.
//!
//! `image setup --guest-user <name>` stages a launch daemon (`kbf-guest mkuser`); this boots
//! the base (not a clone) with graphics, as the console user, with the host listening on the
//! results channel like every probe. The guest's daemon creates the user, sets auto-login,
//! turns off sleep, lock and updates, sends one result frame per step and a summary
//! (`kbf_guest::mkuser`), then powers the guest off. The host collects the frames into
//! `<base>/prime-results` (private to the host, written only through the sink that checks
//! every name, like the probes' results), waits for the power-off for at most the deadline
//! (a guest that is still up then is asked to shut down and, if it does not within
//! `--shutdown-secs`, stopped by force; the run is a timeout either way), and prints one JSON
//! object: each step with its status, exit code, time and output tail, the summary, how the
//! guest ended, the counts of the guest's log audit (does its log hold the generated
//! password; `kbf_guest::mkuser_audit`; informational, it never changes the verdict), and what to run next. A clone of the base made afterwards boots into the
//! user's session.
//!
//! The same boot also runs the simulator's root setup, so that no clone repeats it: the guest's
//! `mkuser` steps `xcode-select`, `license`, `first-launch` and each runtime's add or import
//! come before the ready marker, and `primed.json` records them (`kbf_guest::sim_primed`). They
//! are steps like the user's, so they are in `steps` and `problems` of the report, and a
//! failed one leaves no ready marker. Their time counts against `--deadline-secs`.
//!
//! Run on a base whose user is ready already and whose root setup is done, the guest says so
//! and stays up; the host then shuts it down at once. When the user is ready but the root setup
//! is not (a base primed earlier, or restaged since), the guest runs those steps alone and
//! powers off.
//!
//! The base is changed by this run (a user is added to its disk): it is the one verb of the
//! probes that writes to the base. A run that did not end in a power-off may leave the
//! base's disk unflushed; the report says so, and running `prime` again starts the
//! daemon's steps again (it is idempotent).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use kbf_guest::mkuser::{FRAME_PREFIX, MkUserSummary, SHUTDOWN_NAME, SUMMARY_NAME};
use kbf_guest::mkuser_audit::{AUDIT_NAME, AuditFrame};
use kbf_guest::sim::{Status, StepFrame};
use serde_json::{Value, json};

use crate::bundle::{self, Recorded};
use crate::e3::names;
use crate::gates::{Channels, attach_share, base_missing};
use crate::guest::{self, Share};
use crate::probe::{Ctx, StopKind, Vm};
use crate::report::Report;
use crate::spec::VmSpec;

const PROBE: &str = "image-prime";
/// How long the guest has, by default, to create its user and power off (the flag
/// `--deadline-secs` sets it).
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(20 * 60);
/// The directory in the base the frames are collected into (`<name>-results` of
/// [`guest::prepare_share`] for the name `prime`).
pub const RESULTS_DIR: &str = "prime-results";

/// What the frames the host has received say so far.
#[derive(Default)]
struct Received {
    /// Step frames by frame name (which sorts in step order).
    steps: BTreeMap<String, StepFrame>,
    summary: Option<MkUserSummary>,
    /// The frame the guest sends when its power-off command did not work.
    shutdown: Option<StepFrame>,
    /// The log audit: counts of the password in the guest's log, and how the audit ended.
    audit: Option<AuditFrame>,
    /// Frames that were there and could not be read: name and why.
    unreadable: Vec<(String, String)>,
    /// Names already read: a frame is made whole by a rename, so it is read once.
    seen: BTreeSet<String>,
}

impl Received {
    /// Reads the frames in `dir` (the host's own directory, never the share) that have not
    /// been read yet, each without following a link and within a size cap.
    fn look(&mut self, dir: &Path) {
        for name in names(dir) {
            if !name.starts_with(FRAME_PREFIX) || !self.seen.insert(name.clone()) {
                continue;
            }
            let text = match guest::read_result(&dir.join(&name)) {
                Ok(t) => t,
                Err(e) => {
                    self.unreadable.push((name, e));
                    continue;
                }
            };
            let parsed = if name == SUMMARY_NAME {
                serde_json::from_str(&text).map(|s| self.summary = Some(s))
            } else if name == AUDIT_NAME {
                serde_json::from_str(&text).map(|f| self.audit = Some(f))
            } else if name == SHUTDOWN_NAME {
                serde_json::from_str(&text).map(|f| self.shutdown = Some(f))
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

    /// The steps that did not succeed, as `step (how)`.
    fn problems(&self) -> Vec<String> {
        self.steps
            .values()
            .chain(&self.shutdown)
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
}

/// How the wait for the guest ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The guest powered itself off (the VM stopped, not asked to).
    PoweredOff,
    /// The guest said the user was ready already, and was shut down by the host.
    AlreadyReady,
    /// Still running at the deadline: stopped by the host.
    Timeout,
    /// The VM ended in a way that is not a power-off (a framework error, a crash).
    EndedEarly,
}

impl Outcome {
    fn name(self) -> &'static str {
        match self {
            Self::PoweredOff => "powered-off",
            Self::AlreadyReady => "already-ready",
            Self::Timeout => "timeout",
            Self::EndedEarly => "vm-ended-early",
        }
    }
}

/// Whether a helper's end (`Vm::ended`) is the guest's own power-off: the VM stopped or
/// went to `stopped`, or the helper exited with status 0.
fn is_power_off(ended: &str) -> bool {
    ended == "the VM stopped"
        || ended == "the VM went to state stopped"
        || ended.starts_with("helper ended (code Some(0),")
}

/// Removes the share directories (the frames stay in the base).
fn remove_share(ctx: &Ctx<'_>) {
    for suffix in ["-share", "-tagged"] {
        guest::remove_leftover(&ctx.work.join(format!("prime{suffix}")));
    }
}

/// Starts the base itself with the share and the results channel.
fn start(ctx: &Ctx<'_>, share: &Share, sized: Option<&Recorded>) -> Result<Vm, String> {
    let mut spec = VmSpec::minimal(ctx.base.clone());
    attach_share(&mut spec, share, Channels::VSOCK);
    // The user's session is what the guest is being given: it needs the graphics device.
    spec.graphics = true;
    if let Some(r) = sized {
        spec.cpus = r.cpus;
        spec.memory_mib = r.memory_mib;
    }
    Vm::spawn(ctx.sys, "prime", &spec, None)
}

/// Reads frames as they arrive until the VM ends, the guest reports the user ready already,
/// or the deadline is over.
fn wait(ctx: &Ctx<'_>, vm: &mut Vm, received: &mut Received, deadline: Duration) -> Outcome {
    let began = Instant::now();
    loop {
        vm.watch(ctx.timing.tick, ctx.timing.tick, false);
        received.look(&ctx.work.join(RESULTS_DIR));
        let ready = received.summary.as_ref().is_some_and(|s| s.already_ready);
        if ready || vm.ended.is_some() || began.elapsed() >= deadline {
            // A frame that arrived with the last event.
            received.look(&ctx.work.join(RESULTS_DIR));
            return if ready {
                Outcome::AlreadyReady
            } else if let Some(ended) = &vm.ended {
                if is_power_off(ended) {
                    Outcome::PoweredOff
                } else {
                    Outcome::EndedEarly
                }
            } else {
                Outcome::Timeout
            };
        }
    }
}

/// How the guest ended, as the report says it.
fn stop_json(how: Option<StopKind>, vm: &Vm) -> Value {
    how.map_or_else(
        || json!({"how": "the guest powered itself off"}),
        |how| {
            let command = vm.shutdown_command();
            json!({
                "how": how.name(),
                "vsock_acked": command.as_ref().map(|(acked, _)| acked),
                "vsock_detail": command.as_ref().map(|(_, detail)| detail),
            })
        },
    )
}

fn details(
    ctx: &Ctx<'_>,
    vm: &Vm,
    received: &Received,
    outcome: Outcome,
    stop: &Value,
    deadline: Duration,
    sized: Option<&Recorded>,
) -> Value {
    let ok = received
        .steps
        .values()
        .filter(|f| f.status == Status::Ok)
        .count();
    json!({
        "base": ctx.base,
        "outcome": outcome.name(),
        "deadline_secs": deadline.as_secs(),
        "elapsed_ms": vm.ms(),
        "started_ms": vm.started_ms(),
        "vm_ended": vm.ended,
        "sizing": sized.map(|s| json!({"cpus": s.cpus, "memory_mib": s.memory_mib, "disk_gib": s.disk_gib})),
        "user": received.summary.as_ref().map(|s| s.user.clone()),
        "summary": received.summary,
        "summary_received": received.summary.is_some(),
        "steps_received": received.steps.len(),
        "steps_ok": ok,
        "problems": received.problems(),
        "steps": received.steps.values().map(|f| serde_json::to_value(f).unwrap_or(Value::Null)).collect::<Vec<_>>(),
        "shutdown_frame": received.shutdown,
        "log_audit": received.audit,
        "log_audit_received": received.audit.is_some(),
        "unreadable_frames": received.unreadable.iter().map(|(n, e)| json!({"frame": n, "error": e})).collect::<Vec<_>>(),
        "stop": stop,
        "trace": vm.trace_json(),
        "results_dir": ctx.work.join(RESULTS_DIR),
        "results_files": names(&ctx.work.join(RESULTS_DIR)),
        "helper_log": vm.proc.diagnostics(),
    })
}

/// A report that failed: `reason`, with everything the run saw.
fn failed(reason: String, details: Value) -> Report {
    let mut r = Report::error(PROBE, reason);
    r.details = details;
    r
}

/// `image prime`: see the module documentation. `ctx.work` and `ctx.base` are the base
/// bundle; the guest binary is copied into the share.
#[must_use]
pub fn prime(ctx: &Ctx<'_>, deadline: Duration) -> Report {
    if let Some(r) = base_missing(ctx, PROBE) {
        return r;
    }
    if ctx.guest_binary.is_none() {
        return Report::needs_manual(
            PROBE,
            "prime reads the frames the guest's user daemon sends, so it needs --guest-binary (a kbf-guest built for macOS) and a base set up with `image setup --guest-binary --guest-user <name>`",
            json!({"guest_binary": null}),
        );
    }
    let share = match guest::prepare_share(ctx, "prime") {
        Ok(s) => s,
        Err(e) => return Report::error(PROBE, e),
    };
    let sized = bundle::recorded_sizing(&ctx.base);
    let mut vm = match start(ctx, &share, sized.as_ref()) {
        Ok(vm) => vm,
        Err(e) => {
            remove_share(ctx);
            return Report::error(PROBE, e);
        }
    };
    let mut received = Received::default();
    let outcome = wait(ctx, &mut vm, &mut received, deadline);
    // Who stops the guest: it did itself (a power-off), or the host does now.
    let how = match outcome {
        Outcome::PoweredOff | Outcome::EndedEarly => None,
        Outcome::AlreadyReady => {
            vm.request_shutdown();
            Some(vm.finish_shutdown(&ctx.timing, ctx.timing.shutdown_timeout))
        }
        // Asked first, so that a guest that is only slow flushes its disk; stopped by
        // force when it does not shut down within the shutdown wait.
        Outcome::Timeout => {
            vm.request_shutdown();
            Some(vm.finish_shutdown(&ctx.timing, ctx.timing.shutdown_timeout))
        }
    };
    // Reap the helper in every case; a no-op when it has gone.
    vm.stop(&ctx.timing);
    remove_share(ctx);
    let stop = stop_json(how, &vm);
    let d = details(
        ctx,
        &vm,
        &received,
        outcome,
        &stop,
        deadline,
        sized.as_ref(),
    );
    let problems = received.problems();
    match (outcome, &received.summary) {
        (Outcome::Timeout, _) => failed(
            format!(
                "timeout: the guest did not power off within {} s and was stopped; the base's disk may not be flushed, and the user may be missing (steps received: {}; run `prime` again, it is idempotent)",
                deadline.as_secs(),
                received.steps.len()
            ),
            d,
        ),
        (Outcome::EndedEarly, _) => failed(
            format!(
                "the VM ended before the guest powered off: {}",
                vm.ended.clone().unwrap_or_default()
            ),
            d,
        ),
        (_, None) => failed(
            "the guest stopped without sending the mkuser summary: is the base set up with `image setup --guest-binary --guest-user <name>`?".to_owned(),
            d,
        ),
        (_, Some(s)) if !s.ok => failed(
            format!(
                "the guest could not make user {}: {}",
                s.user,
                if problems.is_empty() {
                    s.note.clone().unwrap_or_else(|| "see the summary".to_owned())
                } else {
                    problems.join(", ")
                }
            ),
            d,
        ),
        (Outcome::AlreadyReady, Some(s)) => Report::ok(
            PROBE,
            format!("user {} was ready already; the guest was shut down", s.user),
            with_next(d, &s.user),
        ),
        (Outcome::PoweredOff, Some(s)) => Report::ok(
            PROBE,
            format!(
                "user {} is ready ({} of {} steps ok) and the guest powered off",
                s.user,
                d["steps_ok"],
                d["steps_received"]
            ),
            with_next(d, &s.user),
        ),
    }
}

fn with_next(mut d: Value, user: &str) -> Value {
    d["next"] = json!(format!(
        "clones of this base boot into the Aqua session of {user}: run `kbf-vmm probe e3 --guest-binary <kbf-guest> --work-dir <dir> --base <base>`"
    ));
    d
}
