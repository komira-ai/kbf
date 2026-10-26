//! `probe-bench`: how long a VM lease's fixed costs take, cold and warm.
//!
//! Per run: clone the base, start a VM, wait for the guest's markers, stop the VM. The
//! page cache is emptied before each cold run (`purge`, needs root) and left alone for
//! warm runs.
//!
//! The markers are files the guest's launch daemons write into the shared folder
//! (`guest::offline_payload`). What each time means:
//! - `boot_to_running`: from the spawn of the helper to its `started` event. The framework
//!   says the VM is running; the guest may still be in firmware.
//! - `boot_to_system_ready`: to the first marker written by a process the guest's launchd
//!   starts before anyone logs in. This is the earliest the host can see guest userspace.
//! - `boot_to_login`: to the marker written once the login window process exists.
//! - `boot_to_aqua_ready`: to the marker an agent writes from a logged-in Aqua session; it
//!   only exists in an image with an auto-login user and the agent installed.
//! - `stop_to_exit`: from the stop request to the helper's exit.
//!
//! `stop_to_exit` is a force stop: the helper asks the framework to stop the VM at once
//! and exits when that completes, so the guest sees an unclean shutdown. Whether a slot
//! frees as fast is probe P2's question, not this bench's.
//!
//! Limits of the numbers: the host polls every 100 ms and the guest every 500 ms, so a
//! guest marker time is good to about 0.6 s; the host is otherwise idle, so every time is
//! a lower bound for a host that is compiling.

use std::time::Duration;

use serde_json::{Value, json};

use crate::bundle::{self, Identity};
use crate::gates::{Channels, attach_share};
use crate::guest::{self, Share};
use crate::probe::{Ctx, Step, Vm};
use crate::report::{Report, summary_json};
use crate::spec::VmSpec;
use crate::system::Clock;

/// One run's measurements in milliseconds; `None` is a marker that never appeared.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Run {
    pub clone: f64,
    pub running: f64,
    pub system: Option<f64>,
    pub login: Option<f64>,
    pub aqua: Option<f64>,
    pub stop_to_exit: f64,
    /// Why the run ended early (the helper died, or the VM stopped), when it did.
    pub failure: Option<String>,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// What the marker wait saw.
struct Markers {
    system: Option<f64>,
    login: Option<f64>,
    aqua: Option<f64>,
    /// Why the wait ended early: the helper died or the VM stopped.
    failure: Option<String>,
}

/// When each marker first showed up, in ms after `t0`. Existence is checked without
/// following links (the share is guest-writable).
fn await_markers(
    ctx: &Ctx<'_>,
    clock: &dyn Clock,
    vm: &mut Vm,
    share: &Share,
    t0: Duration,
) -> Markers {
    let dir = share.results();
    let mut seen = Markers {
        system: None,
        login: None,
        aqua: None,
        failure: None,
    };
    let grace = ctx.timing.tick * 10;
    let mut core_done: Option<Duration> = None;
    loop {
        let now = ms(clock.now().saturating_sub(t0));
        for (slot, name) in [
            (&mut seen.system, guest::READY_SYSTEM),
            (&mut seen.login, guest::READY_LOGIN),
            (&mut seen.aqua, guest::READY_AQUA),
        ] {
            if slot.is_none() && std::fs::symlink_metadata(dir.join(name)).is_ok() {
                *slot = Some(now);
            }
        }
        if seen.system.is_some() && seen.login.is_some() && core_done.is_none() {
            core_done = Some(clock.now());
        }
        let out_of_time = clock.now().saturating_sub(t0) >= ctx.timing.guest_timeout;
        let aqua_over = core_done
            .is_some_and(|t| seen.aqua.is_some() || clock.now().saturating_sub(t) >= grace);
        if out_of_time || aqua_over {
            return seen;
        }
        if let Step::Failed(b) = vm.watch(ctx.timing.tick, ctx.timing.tick, false) {
            seen.failure = b.failure;
            return seen;
        }
    }
}

/// Removes a run's directories.
fn remove(ctx: &Ctx<'_>, name: &str) {
    for suffix in ["", "-share", "-tagged", "-results"] {
        let _ = std::fs::remove_dir_all(ctx.work.join(format!("{name}{suffix}")));
    }
}

/// Runs `runs` boots; `cold` empties the page cache before each. The runs that finished
/// come back even when a later one broke the batch (the error is the second value). A
/// first run that sees no guest marker ends the batch: every further run would wait out
/// the whole guest timeout for the same reason.
fn measure(ctx: &Ctx<'_>, label: &str, runs: u32, cold: bool) -> (Vec<Run>, Option<String>) {
    let mut out: Vec<Run> = Vec::new();
    for i in 0..runs {
        match one_run(ctx, &format!("bench-{label}-{i}"), cold) {
            Ok(run) => {
                let silent = i == 0 && run.system.is_none();
                out.push(run);
                if silent {
                    return (
                        out,
                        Some(
                            "the first run saw no guest marker, so the remaining runs were not made"
                                .to_owned(),
                        ),
                    );
                }
            }
            Err(e) => return (out, Some(e)),
        }
    }
    (out, None)
}

/// One boot: clone, start, wait for the markers, stop, remove.
fn one_run(ctx: &Ctx<'_>, name: &str, cold: bool) -> Result<Run, String> {
    if cold {
        ctx.sys.purge_cache()?;
    }
    let share = guest::prepare_share(ctx, name)?;
    let dest = ctx.work.join(name);
    let clone = bundle::clone_bundle(ctx.sys, &ctx.base, &dest, Identity::Keep)
        .map_err(|e| e.to_string())?;
    let mut spec = VmSpec::minimal(dest);
    spec.graphics = ctx.graphics;
    attach_share(&mut spec, &share, Channels::NONE);
    let clock = ctx.sys.clock();
    let t0 = clock.now();
    let started = ctx.spawn_retrying(name, &spec)?;
    let mut vm = started.vm;
    let running = ms(clock.now().saturating_sub(t0));
    let seen = await_markers(ctx, &*clock, &mut vm, &share, t0);
    let stop_at = clock.now();
    vm.stop(&ctx.timing);
    let stop_to_exit = ms(clock.now().saturating_sub(stop_at));
    remove(ctx, name);
    Ok(Run {
        clone: ms(clone),
        running,
        system: seen.system,
        login: seen.login,
        aqua: seen.aqua,
        stop_to_exit,
        failure: seen.failure,
    })
}

fn column(runs: &[Run], f: impl Fn(&Run) -> Option<f64>) -> Vec<f64> {
    runs.iter().filter_map(f).collect()
}

/// The statistics of one set of runs.
#[must_use]
pub fn stats(runs: &[Run]) -> Value {
    json!({
        "runs": runs.len(),
        "clone_ms": summary_json(&column(runs, |r| Some(r.clone))),
        "boot_to_running_ms": summary_json(&column(runs, |r| Some(r.running))),
        "boot_to_system_ready_ms": summary_json(&column(runs, |r| r.system)),
        "boot_to_login_ms": summary_json(&column(runs, |r| r.login)),
        "boot_to_aqua_ready_ms": summary_json(&column(runs, |r| r.aqua)),
        "stop_to_exit_ms": summary_json(&column(runs, |r| Some(r.stop_to_exit))),
        "failed_runs": runs.iter().filter(|r| r.failure.is_some()).count(),
        "failures": runs.iter().filter_map(|r| r.failure.clone()).collect::<Vec<_>>(),
    })
}

/// How many runs saw the system marker: none means the image has nothing that starts the
/// guest side.
fn saw_guest(runs: &[Run]) -> bool {
    runs.iter().any(|r| r.system.is_some())
}

/// `probe-bench`.
#[must_use]
pub fn run(ctx: &Ctx<'_>, runs: u32) -> Report {
    if let Err(e) = bundle::check(&ctx.base) {
        return Report::needs_manual(
            "probe-bench",
            format!("{e}; this needs a host with a prepared base bundle and a hypervisor"),
            json!({"base": ctx.base}),
        );
    }
    if runs == 0 {
        return Report::error("probe-bench", "--runs must be at least 1");
    }
    let (cold, cold_error) = measure(ctx, "cold", runs, true);
    let (warm, warm_error) = measure(ctx, "warm", runs, false);
    if let Some(e) = warm_error.as_ref().filter(|_| warm.is_empty()) {
        return Report::error("probe-bench", format!("warm runs: {e}"));
    }
    let with_error = |mut v: Value, error: Option<String>| {
        if let Some(e) = error {
            v["incomplete"] = json!(e);
        }
        v
    };
    let cold_ran = !cold.is_empty();
    let cold_json = match (cold.is_empty(), cold_error) {
        (true, e) => json!({
            "status": "needs-manual",
            "reason": format!("cold runs need root for `purge`, or failed: {}", e.unwrap_or_default()),
        }),
        (false, e) => with_error(stats(&cold), e),
    };
    let details = json!({
        "cold": cold_json,
        "warm": with_error(stats(&warm), warm_error),
        "method": {
            "runs_requested": runs,
            "host_poll_ms": u64::try_from(ctx.timing.tick.as_millis()).unwrap_or(u64::MAX),
            "guest_poll_ms": 500,
            "guest_timeout_ms": u64::try_from(ctx.timing.guest_timeout.as_millis()).unwrap_or(u64::MAX),
            "start_timeout_ms": u64::try_from(ctx.timing.start_timeout.as_millis()).unwrap_or(u64::MAX),
            "order": "cold runs first, then warm",
            "load": "none: the host was otherwise idle, so these are lower bounds for a loaded host",
            "stop": "force stop",
        },
    });
    let guest_seen = saw_guest(&warm) && (!cold_ran || saw_guest(&cold));
    if guest_seen {
        Report::ok(
            "probe-bench",
            format!(
                "{runs} warm runs{}",
                if cold_ran { " and the same cold" } else { "" }
            ),
            details,
        )
    } else {
        Report::needs_manual(
            "probe-bench",
            guest::no_agent_reason(ctx.guest_binary.is_some(), &["ready-system".to_owned()]),
            details,
        )
    }
}
