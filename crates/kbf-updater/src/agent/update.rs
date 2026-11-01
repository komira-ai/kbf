//! One host's A/B (`docs/design/updates.md`, section 4), driven from the updater, with the
//! host's own helper doing the root work.
//!
//! In this order, and each step only if the one before it worked:
//!
//! 1. **Fetch and verify** B on the host: the helper fetches the release the version names
//!    from the channel it is configured for, checks the attestation on the host (signer
//!    `komira-ai/kbf` `artifacts.yml`, hosted runners, the full commit, the channel's branch)
//!    and lays it out beside A. An unattested, mismatched or unknown build is refused here,
//!    before the node is touched. The updater never takes a URL or a path from the
//!    coordinator's reply: the version is the only input, and it is looked up by the helper.
//! 2. **Drain** the node at the frontend and wait until it holds no lease, however long that
//!    takes (`NodeDrain`: cordon, drain, uncordon over the operator API; a cordon is durable).
//!    A `--drain-deadline-secs` that passes first fails the update with the node left drained
//!    and nothing switched; the order is tried again after `--retry-secs`.
//! 3. **Switch**: the helper puts B in place on trial, with a pending marker and a deadline on
//!    its disk, and restarts the service on it.
//! 4. **Health**: the node's health check (`node_health`) until B shows healthy, for no longer
//!    than the health timeout and never past the helper's deadline.
//! 5. **Confirm** B if it is healthy. If it is not, **revert**: the helper owns that, and does
//!    it by itself at the deadline whatever this process is doing; here it is asked at once.
//! 6. **Undrain** only if the node is healthy: on B after a confirm, or on A after a revert. A
//!    node that is not healthy stays drained. A node that was out of placement before step 2
//!    (an operator drained or cordoned it) is drained and waited for as any other but is never
//!    undrained: it is left out, and the log says so. "Before step 2" means before the
//!    updater's own drain: a node this updater drained on an earlier attempt (the drain
//!    deadline passed, an undrain failed, the agent restarted) is still the updater's, and a
//!    retry or a resume gives it back (`cordon`: the drain leaves a mark on disk).
//!
//! The result is [`UpdateResult`]: the target and whether it succeeded. It goes into the next
//! heartbeat.
//!
//! A restart of the updater in the middle finds the helper's pending marker and finishes the
//! trial from the health step: it does not fetch, drain or switch a second time.

use std::time::Duration;

use super::flags::Timing;
use super::local::{Local, LocalError, Trial};
use super::status::{Shared, version_of_commit};
use crate::inventory::Host;
use crate::ports::{Clock, Drain, Health, HealthCheck, Placed, PortError};
use crate::version::Version;
use crate::wire::{Outcome, UpdateResult};

/// What an update needs of the outside, as ports.
pub struct Ports<'a> {
    pub local: &'a dyn Local,
    pub drain: &'a dyn Drain,
    pub health: &'a dyn HealthCheck,
    pub clock: &'a dyn Clock,
    /// Unix seconds now: the helper's deadline is in them.
    pub unix_now: &'a dyn Fn() -> u64,
    pub log: &'a dyn Fn(&str),
}

/// The update of one host.
pub struct Update<'a> {
    pub ports: Ports<'a>,
    pub host: &'a Host,
    pub channel: &'a str,
    pub timing: Timing,
    pub status: &'a Shared,
}

fn result(target: &Version, outcome: Outcome) -> UpdateResult {
    UpdateResult::new(target.clone(), outcome)
}

impl Update<'_> {
    fn log(&self, line: &str) {
        (self.ports.log)(line);
    }

    /// Run the update to `target`, or finish the one the helper is in the middle of.
    pub fn run(&self, target: &Version) -> UpdateResult {
        self.status.update(|s| s.updating = Some(target.clone()));
        let done = self.run_inner(target);
        self.status.update(|s| {
            s.updating = None;
            s.last_update = Some(done.clone());
        });
        done
    }

    fn run_inner(&self, target: &Version) -> UpdateResult {
        let failed = |why: &dyn std::fmt::Display| {
            self.log(&format!("update to {target}: {why}"));
            result(target, Outcome::Failed)
        };
        if let Some(why) = self.ports.health.refusal(self.host) {
            return failed(&why);
        }
        let state = match self.ports.local.query() {
            Ok(state) => state,
            Err(e) => return failed(&e),
        };
        // A restart in the middle of an A/B: the helper has B on trial. Finish that trial.
        if let Some(trial) = state.pending {
            return self.resume(target, &trial);
        }
        if state.version.as_ref() == Some(target) {
            return result(target, Outcome::Succeeded);
        }
        // 1. Fetch and verify. Nothing of the node has changed if this fails.
        let staged = match self.ports.local.install(target, self.channel) {
            Ok(staged) => staged,
            Err(e) => return failed(&e),
        };
        if staged.current {
            // The host runs this release already (the helper now knows its version's text).
            return result(target, Outcome::Succeeded);
        }
        // 2. Drain.
        let placed = match self.drain() {
            Ok(placed) => placed,
            Err(e) => return failed(&format!("the node could not be drained: {e}")),
        };
        // 3. Switch.
        let deadline = match self.ports.local.switch(target) {
            Ok(deadline) => deadline,
            Err(e) => {
                self.log(&format!("update to {target}: the switch failed: {e}"));
                return self.back_on_a(target, placed);
            }
        };
        // 4 to 6.
        self.trial(target, deadline, placed)
    }

    /// The helper has `trial` on trial: finish it from the health step.
    fn resume(&self, asked: &Version, trial: &Trial) -> UpdateResult {
        let Some(target) = trial.version.clone() else {
            // A release on trial whose version this host never named: not one to keep.
            self.log(&format!(
                "update to {asked}: the helper has {} on trial, which was not staged by it; reverting",
                trial.commit
            ));
            return self.back_on_a(asked, Placed::In);
        };
        self.log(&format!(
            "update to {target}: resuming the trial the helper has"
        ));
        self.status.update(|s| s.updating = Some(target.clone()));
        let deadline = trial.deadline_unix.unwrap_or(0);
        // The drain was made by the process that began this A/B, perhaps this one before it
        // restarted. The node reads as in placement if the updater cordoned it (its mark is on
        // disk), and as out if an operator did. If it cannot be read, nothing says it is ours.
        let placed = self.ports.drain.placed(self.host).unwrap_or_else(|e| {
            self.log(&format!(
                "update to {target}: the node could not be read, so it is not given back; uncordon it \
                 by hand if it should serve: {e}"
            ));
            Placed::Out
        });
        self.trial(&target, deadline, placed)
    }

    /// Steps 4 to 6 for `target` on trial until `deadline` (Unix seconds).
    fn trial(&self, target: &Version, deadline: u64, placed: Placed) -> UpdateResult {
        let left = Duration::from_secs(deadline.saturating_sub((self.ports.unix_now)()));
        let bound = self
            .timing
            .health_timeout
            .min(left.saturating_sub(self.timing.margin));
        match self.wait_healthy(target, bound) {
            Health::Healthy => match self.ports.local.confirm(target) {
                Ok(()) => return self.keep(target, placed),
                Err(e) => self.log(&format!("update to {target}: the confirm failed: {e}")),
            },
            Health::Waiting(why) => self.log(&format!(
                "update to {target}: not healthy after the time allowed: {why}"
            )),
            Health::Abandoned(why) => {
                self.log(&format!("update to {target}: it will not get there: {why}"));
            }
        }
        self.back_on_a(target, placed)
    }

    /// Give the node back, unless it was out of placement before the roll: that one is left
    /// as it was found, and the log says so.
    fn give_back(&self, placed: Placed) -> Result<(), PortError> {
        match placed {
            Placed::In => self.ports.drain.undrain(self.host),
            Placed::Out => {
                self.log("node was out of placement before the roll; left out");
                Ok(())
            }
        }
    }

    /// B is confirmed: the node goes back into placement, if it was in it.
    fn keep(&self, target: &Version, placed: Placed) -> UpdateResult {
        match self.give_back(placed) {
            Ok(()) => result(target, Outcome::Succeeded),
            Err(e) => {
                // On B and healthy, and not given back: the update is not done.
                self.log(&format!(
                    "update to {target}: confirmed, and the node stays drained: {e}"
                ));
                result(target, Outcome::Failed)
            }
        }
    }

    /// B did not work: the helper puts A back, and the node is given back only if A is
    /// healthy.
    fn back_on_a(&self, target: &Version, placed: Placed) -> UpdateResult {
        match self.ports.local.revert() {
            Ok(()) => {}
            // Nothing on trial: the helper went back by itself, or the switch never held.
            Err(LocalError::Refused(code)) if code == "nothing_pending" => {}
            Err(e) => self.log(&format!("update to {target}: the revert failed: {e}")),
        }
        let running = match self.ports.local.query() {
            Ok(state) => state
                .version
                .or_else(|| state.commit.as_deref().and_then(version_of_commit)),
            Err(e) => {
                self.log(&format!(
                    "update to {target}: the host does not say what runs: {e}"
                ));
                None
            }
        };
        let healthy = match running {
            Some(a) => self.wait_healthy(&a, self.timing.health_timeout) == Health::Healthy,
            None => false,
        };
        if healthy {
            if let Err(e) = self.give_back(placed) {
                self.log(&format!(
                    "update to {target}: back on A, and the node stays drained: {e}"
                ));
            }
        } else {
            self.log(&format!(
                "update to {target}: A is not healthy: the node stays drained"
            ));
        }
        result(target, Outcome::Failed)
    }

    /// Read whether the node is in placement, then drain it and wait until it holds no lease,
    /// however long that takes, or until the drain deadline, if the operator set one, is out.
    /// At that deadline the host is not switched: the update fails with the node left drained
    /// (its leases run on), and the same order is tried again after the retry time, which
    /// drains the node again and waits again. What comes back is whether the node was in
    /// placement before this drain: a node the updater drained on an earlier attempt is.
    fn drain(&self) -> Result<Placed, PortError> {
        let placed = self.ports.drain.placed(self.host)?;
        self.ports
            .drain
            .drain(self.host, self.timing.drain_deadline)?;
        let start = self.ports.clock.now();
        loop {
            let leases = match self.ports.drain.leases(self.host) {
                Ok(leases) => leases,
                Err(e) => {
                    // The node is drained by us and is not going to be switched.
                    let _ = self.give_back(placed);
                    return Err(e);
                }
            };
            let waited = self.ports.clock.now().saturating_sub(start);
            if leases == 0 {
                return Ok(placed);
            }
            if let Some(deadline) = self.timing.drain_deadline.filter(|d| waited >= *d) {
                return Err(PortError::other(format!(
                    "the node still holds {leases} leases after the drain deadline of {} s; \
                     nothing was switched, the leases run on and the node is left drained",
                    deadline.as_secs()
                )));
            }
            self.ports.clock.sleep(self.timing.drain_poll);
        }
    }

    /// The node's health for `target`, held drained, until it is healthy, abandoned, or
    /// `bound` is out.
    fn wait_healthy(&self, target: &Version, bound: Duration) -> Health {
        let start = self.ports.clock.now();
        loop {
            let seen = match self.ports.health.check_held(self.host, target) {
                Ok(Health::Waiting(why)) => Health::Waiting(why),
                Ok(done) => return done,
                Err(e) => Health::Waiting(e.to_string()),
            };
            if self.ports.clock.now().saturating_sub(start) >= bound {
                return seen;
            }
            self.ports.clock.sleep(self.timing.health_poll);
        }
    }
}
