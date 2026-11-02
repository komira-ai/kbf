//! An operator's force (`docs/design/gamma-cicd.md`, C1): the host runs the target, for good.
//!
//! The ordinary A/B ([`super::update`]) puts the target on trial and goes back from it when it
//! does not show healthy. A forced order is the operator saying the target is the release to
//! run, healthy or not, so none of that applies: the helper's `activate` makes the target the
//! release that keeps running, with no trial, no deadline and no revert. In this order, each
//! step only if the one before it worked:
//!
//! 1. **Stage**, only if the helper does not list the target among the releases it keeps (or
//!    lists none, as an older helper does): `install` fetches and verifies it and switches
//!    nothing.
//! 2. **Drain** the node as the ordinary update does: in-flight work is never cut, and a drain
//!    bound is a deferral.
//! 3. **Activate** the target. A helper that predates the request refuses it as `bad_command`:
//!    then the target is switched and confirmed at once, with no wait between, so the helper's
//!    trial cannot go back from it.
//! 4. **Health** is waited for only to decide the undrain: a healthy target gets the node
//!    back; one that is not healthy stays, the node stays drained, and the result is still a
//!    success, because the host runs what the operator asked for. The heartbeat's `health`
//!    says the rest.
//!
//! Nothing is kept on disk for a forced order: an agent that restarts before `activate`
//! leaves the host on its old release, and the coordinator sends the order again when it ages
//! out.

use super::hold::Features;
use super::local::LocalError;
use super::update::{Done, Update, result};
use crate::ports::{Health, Placed};
use crate::version::Version;
use crate::wire::Outcome;

impl Update<'_> {
    /// [`Update::run_for`] for an order that is an operator's force.
    pub fn run_forced_for(&self, target: &Version, features: Features) -> Done {
        self.finish(target, features, |target| self.forced(target))
    }

    fn forced(&self, target: &Version) -> Done {
        if let Some(why) = self.ports.health.refusal(self.host) {
            return self.fail(target, &why);
        }
        let state = match self.ports.local.query() {
            Ok(state) => state,
            Err(e) => return self.fail(target, &e),
        };
        let settled = state.pending.is_none();
        if settled && state.version.as_ref() == Some(target) {
            return result(target, Outcome::Succeeded).into();
        }
        if !state.keeps(target) {
            match self.ports.local.install(target, self.channel) {
                Ok(staged) if staged.current && settled => {
                    return result(target, Outcome::Succeeded).into();
                }
                Ok(_) => {}
                Err(e) => return self.fail(target, &e),
            }
        }
        let placed = match self.drained(target) {
            Ok(placed) => placed,
            Err(done) => return done,
        };
        match self.ports.local.activate(target) {
            Ok(_) => {}
            Err(LocalError::Refused(code)) if code == "bad_command" => {
                self.log(&format!(
                    "update to {target}: the helper does not know activate; switching and confirming at once"
                ));
                if let Err(e) = self.switch_and_confirm(target) {
                    self.log(&format!("update to {target}: {e}"));
                    return self.back_on_a(target, placed);
                }
            }
            Err(e) => {
                self.log(&format!("update to {target}: the activation failed: {e}"));
                return self.back_on_a(target, placed);
            }
        }
        self.settle(target, placed)
    }

    /// An older helper's way to the same end: the trial it opens is confirmed before anything
    /// can wait on it.
    fn switch_and_confirm(&self, target: &Version) -> Result<(), String> {
        self.ports
            .local
            .switch(target)
            .map_err(|e| format!("the switch failed: {e}"))?;
        self.ports
            .local
            .confirm(target)
            .map_err(|e| format!("the confirm failed: {e}"))
    }

    /// The target runs, for good. Whether it is healthy decides only whether the node goes
    /// back into placement.
    fn settle(&self, target: &Version, placed: Placed) -> Done {
        match self.wait_healthy(target, self.timing.health_timeout) {
            Health::Healthy => self.keep(target, placed),
            Health::Waiting(why) | Health::Abandoned(why) => {
                self.log(&format!(
                    "update to {target}: forced, and it runs but is not healthy: {why}; it stays, and the node stays drained"
                ));
                result(target, Outcome::Succeeded).into()
            }
        }
    }
}
