//! An operator's force (`docs/design/gamma-cicd.md`, C1): the host runs the target, for good.
//!
//! The ordinary A/B ([`super::update`]) puts the target on trial and goes back from it when it
//! does not show healthy. A forced order is the operator saying the target is the release to
//! run, healthy or not, so none of that applies: the helper's `activate` makes the target the
//! release that keeps running, with no trial, no deadline and no revert. In this order, each
//! step only if the one before it worked:
//!
//! 1. **The trial the helper has**, if it has one. The helper refuses an `install` while a
//!    release is on trial, so this comes first. A trial of the target is left for `activate`
//!    to end. A trial of another release is ended by `activate` too when the helper keeps the
//!    target; when it does not list the target (it keeps no such release, or its list is
//!    unavailable or cut, which is not told apart: the target is unknown either way) the trial
//!    is reverted, so the target can be staged.
//! 2. **Stage**, only if the helper does not list the target among the releases it keeps:
//!    `install` fetches and verifies it and switches nothing.
//! 3. **Drain** the node as the ordinary update does: in-flight work is never cut, and a drain
//!    bound is a deferral.
//! 4. **Activate** the target, naming the commit it must be: the helper's answer must name that
//!    commit, under whatever label the helper keeps for it. A helper that predates the request
//!    refuses it as `bad_command`: then a trial of another release that `activate` was to end is
//!    reverted first (the helper's `install` is refused while a release is on trial), the target
//!    is staged (`install`, which on such a helper
//!    may fetch it again: its `switch` takes only a release that `install` staged, and a
//!    release it kept from an earlier update is not staged), switched and confirmed at once,
//!    with no wait between, so the helper's trial cannot go back from it.
//! 5. **Publish** the result at once: the status says the target runs, `updating` is cleared
//!    and the last update is a success, before the health wait, so the next heartbeat frees
//!    the coordinator's slot.
//! 6. **Health** is waited for only to decide the undrain: a healthy target gets the node
//!    back; one that is not healthy stays, the node stays drained, and the result is still a
//!    success, because the host runs what the operator asked for. A node that cannot be given
//!    back stays drained and the result is still a success. The heartbeat's `health` says the
//!    rest.
//!
//! The agent writes a `forced-<unit>` mark (`super::hold::Marks`) before a forced update and
//! removes it when the update ends. A restart in the middle that finds the helper's trial of
//! that target (an older helper, between `switch` and `confirm`) runs the update again as a
//! force: the trial is confirmed, not reverted. A restart after `install` and before
//! `activate` leaves the host on its old release with nothing pending; the mark is removed at
//! start, and the coordinator sends the order again when it ages out. One limit remains: the
//! mark is written before the order runs, and a mark that cannot be written is logged and the
//! force goes on without it, so a restart in the window of an older helper then resumes the
//! trial as an ordinary order, which reverts it if it is not healthy by the helper's deadline.

use super::hold::Features;
use super::local::{HostState, LocalError, Trial};
use super::update::{Done, Update, result};
use crate::ports::{Health, Placed};
use crate::version::Version;
use crate::wire::{Health as WireHealth, Outcome};

/// Why an older helper's way to the same end did not get there.
#[derive(Debug, thiserror::Error)]
enum Fallback {
    #[error("the install that stages the target failed: {0}")]
    Stage(LocalError),
    #[error("the trial of another release could not be reverted: {0}")]
    Revert(LocalError),
    #[error("the switch failed: {0}")]
    Switch(LocalError),
    #[error("the confirm failed: {0}")]
    Confirm(LocalError),
}

/// What an older helper's way to the same end still has to do.
#[derive(Clone, Copy)]
struct Legacy {
    /// Another release is on trial and must be put back first.
    other_trial: bool,
    /// The target is staged (by this force) or on trial.
    staged: bool,
    /// The target is on trial already: it needs only the confirm.
    on_trial: bool,
}

/// Whether `trial` is the release `target` names.
fn is_trial_of(trial: &Trial, target: &Version) -> bool {
    trial.version.as_ref() == Some(target)
        || target.commit().is_some_and(|c| trial.commit.starts_with(c))
}

impl Update<'_> {
    /// [`Update::run_for`] for an order that is an operator's force.
    pub(super) fn run_forced_for(&self, target: &Version, features: Features) -> Done {
        self.finish(target, features, |target, started| {
            self.forced(target, started)
        })
    }

    fn forced(&self, target: &Version, started: u64) -> Done {
        if let Some(why) = self.ports.health.refusal(self.host) {
            return self.fail(target, &why);
        }
        let state = match self.ports.local.query() {
            Ok(state) => state,
            Err(e) => return self.fail(target, &e),
        };
        if state.pending.is_none() && state.runs(target) {
            return self.already(target, state.commit);
        }
        let (commit, staged_now) = match self.settle_trial_and_stage(target, &state) {
            Ok(found) => found,
            Err(done) => return *done,
        };
        let placed = match self.drained(target) {
            Ok(placed) => placed,
            Err(done) => return *done,
        };
        let on_trial = state
            .pending
            .as_ref()
            .is_some_and(|trial| is_trial_of(trial, target));
        // A trial of another release is still there when the target is kept: `activate` ends it,
        // but an older helper's `install` is refused while it is.
        let other_trial = state.pending.is_some() && !on_trial && !staged_now;
        match self.ports.local.activate(target, Some(&commit)) {
            Ok(activated) => self.settle(target, placed, activated.commit, started),
            Err(LocalError::Refused(code)) if code == "bad_command" => {
                self.log(&format!(
                    "update to {target}: the helper does not know activate; staging, switching and confirming at once"
                ));
                let steps = Legacy {
                    other_trial,
                    staged: staged_now || on_trial,
                    on_trial,
                };
                match self.switch_and_confirm(target, steps) {
                    Ok(()) => self.settle(target, placed, commit, started),
                    Err(e) => {
                        self.log(&format!("update to {target}: {e}"));
                        self.back_on_a(target, placed)
                    }
                }
            }
            Err(e) => {
                self.log(&format!("update to {target}: the activation failed: {e}"));
                self.back_on_a(target, placed)
            }
        }
    }

    /// The host runs the target already: done, with nothing to drain. The commit that runs
    /// goes with the result, so the agent keeps saying the target while it runs under a label
    /// the helper keeps for it.
    fn already(&self, target: &Version, commit: Option<String>) -> Done {
        let mut done = Done::from(result(target, Outcome::Succeeded));
        done.activated = commit;
        done
    }

    /// Steps 1 and 2: the commit the target is, and whether `install` staged it now; or the end
    /// of the update (a failure, or the host found already running the target).
    fn settle_trial_and_stage(
        &self,
        target: &Version,
        state: &HostState,
    ) -> Result<(String, bool), Box<Done>> {
        if let Some(trial) = &state.pending
            && is_trial_of(trial, target)
        {
            return Ok((trial.commit.clone(), false));
        }
        if let Some(commit) = state.kept_commit(target) {
            return Ok((commit.to_owned(), false));
        }
        if state.pending.is_some() {
            self.log(&format!(
                "update to {target}: another release is on trial and the target is not listed among the releases the host keeps; reverting the trial first"
            ));
            match self.ports.local.revert() {
                Ok(()) => {}
                Err(LocalError::Refused(code)) if code == "nothing_pending" => {}
                Err(e) => {
                    return Err(Box::new(
                        self.fail(target, &format!("the trial could not be reverted: {e}")),
                    ));
                }
            }
        }
        match self.ports.local.install(target, self.channel) {
            Ok(staged) if staged.current => {
                Err(Box::new(self.already(target, Some(staged.commit))))
            }
            Ok(staged) => Ok((staged.commit, true)),
            Err(e) => Err(Box::new(self.fail(target, &e))),
        }
    }

    /// An older helper's way to the same end: another release's trial put back, the target
    /// staged, switched, and confirmed before anything can wait on the trial. What is already
    /// done (a trial of the target, a target staged by this force) is not done again.
    fn switch_and_confirm(&self, target: &Version, steps: Legacy) -> Result<(), Fallback> {
        if steps.other_trial {
            match self.ports.local.revert() {
                Ok(()) => {}
                Err(LocalError::Refused(code)) if code == "nothing_pending" => {}
                Err(e) => return Err(Fallback::Revert(e)),
            }
        }
        if !steps.staged {
            self.ports
                .local
                .install(target, self.channel)
                .map_err(Fallback::Stage)?;
        }
        if !steps.on_trial {
            self.ports.local.switch(target).map_err(Fallback::Switch)?;
        }
        self.ports.local.confirm(target).map_err(Fallback::Confirm)
    }

    /// The target runs, for good: say so now, then see whether it is healthy, which decides
    /// only whether the node goes back into placement.
    fn settle(&self, target: &Version, placed: Placed, commit: String, started: u64) -> Done {
        self.status.update(|s| {
            s.version = target.clone();
            // Not known healthy until it has been looked at.
            s.health = WireHealth::Unhealthy;
            s.updating = None;
            let mut published = result(target, Outcome::Succeeded);
            published.started_unix = Some(started);
            s.last_update = Some(published);
        });
        match self.wait_healthy(target, self.timing.health_timeout) {
            Health::Healthy => {
                if let Err(e) = self.give_back(placed) {
                    self.log(&format!(
                        "update to {target}: forced, and it runs healthy, but the node stays drained: {e}"
                    ));
                }
            }
            Health::Waiting(why) | Health::Abandoned(why) => {
                self.log(&format!(
                    "update to {target}: forced, and it runs but is not healthy: {why}; it stays, and the node stays drained"
                ));
            }
        }
        let mut done = Done::from(result(target, Outcome::Succeeded));
        done.activated = Some(commit);
        done
    }
}
