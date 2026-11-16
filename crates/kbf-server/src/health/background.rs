//! The probes of `/readyz` asked in the background, so that an answer costs a lookup.
//!
//! A [`Prober`] owns the [`Probe`]s of a [`Checks`](super::Checks) and asks each one on its
//! own, every `--readyz-probe-interval-ms`, whatever else is slow: a store that takes 2 s to
//! answer delays that check's next answer, not `/readyz` and not the other checks. A caller
//! of `/readyz` (the load balancer's health check, whose own timeout is a second) reads the
//! last answer ([`Prober::failing`]); it never waits on a dependency.
//!
//! A check fails only after `--readyz-fail-after` probes in a row have failed, and passes
//! again at the first probe that passes, so one slow or lost answer does not take a frontend
//! out of its load balancer. Until a check has either passed or failed that many times in a
//! row it fails with `not probed yet`: a frontend that has not asked is not ready.
//!
//! Each change of a check from not failing to failing (or from `not probed yet` to a failure) and
//! from failing to passing is logged with the check and the reason
//! of the probe, and counted in `kbf_readyz_transitions_total{check,to}` on `GET /metrics`
//! ([`Prober::render`]).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::future::join_all;

use super::{Failing, Probe};

/// How often each probe is asked again, from the end of its last answer, unless
/// `--readyz-probe-interval-ms` says otherwise.
pub const PROBE_INTERVAL: Duration = Duration::from_millis(1000);

/// How many probes of a check must fail in a row for the check to fail, unless
/// `--readyz-fail-after` says otherwise.
pub const FAIL_AFTER: u32 = 3;

/// The reason a check gives before its first answer.
const NOT_PROBED: &str = "not probed yet";

/// What a check has answered so far.
struct State {
    /// Probes that have failed in a row.
    streak: u32,
    /// Why the check fails now; `None` while it passes.
    failing: Option<String>,
}

/// One probe, with its name, what it last answered and how often that changed.
struct Slot {
    check: &'static str,
    probe: Arc<dyn Probe>,
    state: Mutex<State>,
    /// Times the check went from passing to failing.
    to_failing: AtomicU64,
    /// Times the check went from failing to passing (the first pass included).
    to_ready: AtomicU64,
}

impl Slot {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The probes of a frontend, asked in the background; see the module.
pub struct Prober {
    fail_after: u32,
    slots: Vec<Slot>,
}

impl Prober {
    /// A prober of `probes` that fails a check after `fail_after` failed probes in a row (at
    /// least 1).
    #[must_use]
    pub fn new(probes: Vec<(&'static str, Arc<dyn Probe>)>, fail_after: u32) -> Self {
        let slots = probes
            .into_iter()
            .map(|(check, probe)| Slot {
                check,
                probe,
                state: Mutex::new(State {
                    streak: 0,
                    failing: Some(NOT_PROBED.to_owned()),
                }),
                to_failing: AtomicU64::new(0),
                to_ready: AtomicU64::new(0),
            })
            .collect();
        Self {
            fail_after: fail_after.max(1),
            slots,
        }
    }

    /// The checks that fail now and are not in `skip`, in the order they were given. Reads
    /// the last answers; asks no probe.
    #[must_use]
    pub fn failing(&self, skip: &[&str]) -> Vec<Failing> {
        self.slots
            .iter()
            .filter(|slot| !skip.contains(&slot.check))
            .filter_map(|slot| {
                let reason = slot.state().failing.clone()?;
                Some(Failing {
                    check: slot.check,
                    reason,
                })
            })
            .collect()
    }

    /// Asks every probe once, all at the same time, and records the answers.
    pub async fn round(&self) {
        join_all(self.slots.iter().map(|slot| self.ask(slot))).await;
    }

    /// Asks every probe again `interval` after its last answer, each on its own clock, until
    /// the future is dropped.
    pub async fn run(self: Arc<Self>, interval: Duration) {
        let this = &*self;
        join_all(this.slots.iter().map(|slot| async move {
            loop {
                tokio::time::sleep(interval).await;
                this.ask(slot).await;
            }
        }))
        .await;
    }

    /// Asks `slot`'s probe and records the answer.
    async fn ask(&self, slot: &Slot) {
        let answer = slot.probe.probe().await;
        let mut state = slot.state();
        let before = state.failing.clone();
        match answer {
            Ok(()) => {
                state.streak = 0;
                state.failing = None;
            }
            Err(reason) => {
                state.streak = state.streak.saturating_add(1);
                if state.streak >= self.fail_after {
                    state.failing = Some(reason);
                }
            }
        }
        match (before.as_deref(), state.failing.as_deref()) {
            (None | Some(NOT_PROBED), Some(reason)) if reason != NOT_PROBED => {
                slot.to_failing.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    check = slot.check,
                    reason,
                    failed_in_a_row = state.streak,
                    "readiness: the check fails"
                );
            }
            (Some(_), None) => {
                slot.to_ready.fetch_add(1, Ordering::Relaxed);
                tracing::info!(check = slot.check, "readiness: the check passes");
            }
            _ => {}
        }
    }

    /// Appends `kbf_readyz_transitions_total{check,to}` to `out`, one sample per check and
    /// direction.
    pub fn render(&self, out: &mut String) {
        use std::fmt::Write as _;
        let _ = writeln!(
            out,
            "# HELP kbf_readyz_transitions_total Times a readiness check went from passing to failing (to=\"failing\") or from failing to passing (to=\"ready\"; the first pass counts)"
        );
        let _ = writeln!(out, "# TYPE kbf_readyz_transitions_total counter");
        for slot in &self.slots {
            for (to, count) in [("failing", &slot.to_failing), ("ready", &slot.to_ready)] {
                let _ = writeln!(
                    out,
                    "kbf_readyz_transitions_total{{check=\"{}\",to=\"{to}\"}} {}",
                    slot.check,
                    count.load(Ordering::Relaxed)
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
