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
//! - `slot_reusable`: from the stop request to the next successful start (retrying while
//!   the guest limit refuses), so it includes the wait for the slot. The last run has no
//!   next start and reports `stop_to_exit`.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::bundle::{self, Identity};
use crate::guest::{self, Share};
use crate::probe::{Ctx, Vm};
use crate::report::{Report, summary_json};
use crate::spec::{TaggedShare, VmSpec};

/// One run's measurements in milliseconds; `None` is a marker that never appeared.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Run {
    pub clone: f64,
    pub running: f64,
    pub system: Option<f64>,
    pub login: Option<f64>,
    pub aqua: Option<f64>,
    pub stop_to_exit: f64,
    pub slot_reusable: Option<f64>,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// When each marker first showed up, in ms after `t0`.
fn await_markers(
    ctx: &Ctx<'_>,
    vm: &mut Vm,
    share: &Share,
    t0: Instant,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    let dir = share.results();
    let (mut system, mut login, mut aqua) = (None, None, None);
    let grace = ctx.timing.tick * 10;
    let mut core_done: Option<Instant> = None;
    loop {
        let now = ms(t0.elapsed());
        for (slot, name) in [
            (&mut system, guest::READY_SYSTEM),
            (&mut login, guest::READY_LOGIN),
            (&mut aqua, guest::READY_AQUA),
        ] {
            if slot.is_none() && dir.join(name).exists() {
                *slot = Some(now);
            }
        }
        if system.is_some() && login.is_some() && core_done.is_none() {
            core_done = Some(Instant::now());
        }
        let out_of_time = t0.elapsed() >= ctx.timing.guest_timeout;
        let aqua_over = core_done.is_some_and(|t| aqua.is_some() || t.elapsed() >= grace);
        if out_of_time || aqua_over {
            return (system, login, aqua);
        }
        vm.watch(ctx.timing.tick, ctx.timing.tick, false);
    }
}

/// Removes a run's directories.
fn remove(ctx: &Ctx<'_>, name: &str) {
    for suffix in ["", "-share", "-tagged"] {
        let _ = std::fs::remove_dir_all(ctx.work.join(format!("{name}{suffix}")));
    }
}

/// Runs `runs` boots; `cold` empties the page cache before each.
fn measure(ctx: &Ctx<'_>, label: &str, runs: u32, cold: bool) -> Result<Vec<Run>, String> {
    let mut out: Vec<Run> = Vec::new();
    let mut previous: Option<Instant> = None;
    for i in 0..runs {
        if cold {
            ctx.sys.purge_cache()?;
        }
        let name = format!("bench-{label}-{i}");
        let share = guest::prepare_share(ctx, &name)?;
        let dest = ctx.work.join(&name);
        let clone = bundle::clone_bundle(ctx.sys, &ctx.base, &dest, Identity::Keep)
            .map_err(|e| e.to_string())?;
        let mut spec = VmSpec::minimal(dest);
        spec.automount_dir = Some(share.dir.clone());
        spec.tagged = Some(TaggedShare {
            tag: guest::TAG.to_owned(),
            path: share.tagged.clone(),
        });
        let t0 = Instant::now();
        let started = ctx.spawn_retrying(&name, &spec)?;
        let mut vm = started.vm;
        let running = ms(t0.elapsed());
        // The slot the previous run stopped is reusable once this start worked.
        if let (Some(stopped_at), Some(prev)) = (previous.take(), out.last_mut()) {
            prev.slot_reusable = Some(ms(stopped_at.elapsed()));
        }
        let (system, login, aqua) = await_markers(ctx, &mut vm, &share, t0);
        let stop_at = Instant::now();
        vm.stop(&ctx.timing);
        let stop_to_exit = ms(stop_at.elapsed());
        remove(ctx, &name);
        previous = Some(stop_at);
        out.push(Run {
            clone: ms(clone),
            running,
            system,
            login,
            aqua,
            stop_to_exit,
            slot_reusable: None,
        });
    }
    out.last_mut()
        .into_iter()
        .for_each(|last| last.slot_reusable = Some(last.stop_to_exit));
    Ok(out)
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
        "slot_reusable_ms": summary_json(&column(runs, |r| r.slot_reusable)),
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
    let cold = measure(ctx, "cold", runs, true);
    let warm = match measure(ctx, "warm", runs, false) {
        Ok(w) => w,
        Err(e) => return Report::error("probe-bench", format!("warm runs: {e}")),
    };
    let cold_json = match &cold {
        Ok(r) => stats(r),
        Err(e) => {
            json!({"status": "needs-manual", "reason": format!("cold runs need root for `purge`, or failed: {e}")})
        }
    };
    let details = json!({"cold": cold_json, "warm": stats(&warm)});
    let guest_seen = saw_guest(&warm) && cold.as_ref().map_or(true, |c| saw_guest(c));
    if guest_seen {
        Report::ok(
            "probe-bench",
            format!(
                "{runs} warm runs{}",
                if cold.is_ok() {
                    " and the same cold"
                } else {
                    ""
                }
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
