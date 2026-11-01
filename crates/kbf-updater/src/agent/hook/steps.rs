//! The five steps, each over the ports and the record.
//!
//! Today's update (`agent::update`) is one function that fetches, drains, switches, waits for
//! health, confirms or reverts, and gives the node back. Here the same calls are split at the
//! steps of `docs/design/host-updates.md`, section 4.3, and made in the same order:
//!
//! | step | calls |
//! |---|---|
//! | `prepare` | the health precondition, `query`, `install`, the placement read, `drain`, `leases` until none |
//! | `cutover` | `switch` |
//! | `verify` | `check_held` until healthy, abandoned or out of time |
//! | `rollback` | `revert`, `query`, `check_held` on the old release, `undrain` |
//! | `cleanup` | `confirm`, `undrain` |
//!
//! Every step decides from the record ([`super::record`]), not from the order it was called in.
//! The node is given back only when the record says it was in placement before this update's
//! own drain, and the placement is read once, before the drain, and written before it.

use std::fmt::Display;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use update_hook::{Class, Invocation, Outcome, Params, Report, Step};

use super::HookWorld;
use super::record::{Record, Store, Was};
use crate::agent::flags::Timing;
use crate::agent::local::{LocalError, Trial};
use crate::agent::status::version_of_commit;
use crate::agent::update::{Waits, wait_healthy};
use crate::ports::Health;
use crate::version::Version;

/// How a step ended: the exit code and the last line.
#[derive(Debug, PartialEq, Eq)]
pub struct Done {
    pub outcome: Outcome,
    pub report: Report,
}

impl Done {
    fn with(outcome: Outcome, report: Report) -> Done {
        Done { outcome, report }
    }

    /// Success.
    fn ok() -> Done {
        Done::with(Outcome::Success, Report::default())
    }

    /// Success of the step that is the commit: the old release cannot come back.
    fn committed() -> Done {
        Done::with(
            Outcome::Success,
            Report {
                reversible: false,
                ..Report::default()
            },
        )
    }

    /// Failure. `reason` is a token of `[a-z0-9_]`.
    pub fn failed(reason: &str, class: Class, detail: impl Display) -> Done {
        Done::with(
            Outcome::Failure,
            Report {
                class,
                reason: Some(reason.to_owned()),
                detail: Some(detail.to_string()),
                ..Report::default()
            },
        )
    }

    /// The host is busy: nothing irreversible happened, try again after `retry_after_secs`. A
    /// deferral is about the host's moment, not the release: the environment's class.
    fn deferred(reason: &str, retry_after_secs: u64, detail: impl Display) -> Done {
        Done::with(
            Outcome::Deferred,
            Report {
                class: Class::Environment,
                reason: Some(reason.to_owned()),
                retry_after_secs: Some(retry_after_secs),
                detail: Some(detail.to_string()),
                ..Report::default()
            },
        )
    }

    /// A failure of the helper: of the environment when it could not be reached.
    fn helper(reason: &str, e: &LocalError) -> Done {
        let class = match e {
            LocalError::Unreachable(_) => Class::Environment,
            _ => Class::Release,
        };
        Done::failed(reason, class, e)
    }
}

/// A step that ends, or ends early: both are a [`Done`].
type Flow = Result<Done, Done>;

/// The node was out of placement before the update: this is what the log says when it stays.
const LEFT_OUT: &str = "node was out of placement before the roll; left out";

/// One run of one step.
pub struct Hook<'a> {
    world: &'a HookWorld,
    store: &'a Store,
    params: &'a Params,
    channel: &'a str,
    timing: Timing,
    target: Version,
    digest: String,
    /// The record as it is on disk, if there is one.
    rec: Option<Record>,
    /// Standard error: the log of the step.
    err: &'a mut dyn Write,
}

/// What a run needs besides the world: who is updating, to what, and where the log goes.
pub struct Run<'a> {
    pub store: &'a Store,
    pub params: &'a Params,
    pub channel: &'a str,
    pub timing: Timing,
    pub target: Version,
    pub digest: String,
    pub err: &'a mut dyn Write,
}

impl<'a> Hook<'a> {
    pub fn new(world: &'a HookWorld, run: Run<'a>) -> Hook<'a> {
        Hook {
            world,
            store: run.store,
            params: run.params,
            channel: run.channel,
            timing: run.timing,
            target: run.target,
            digest: run.digest,
            rec: None,
            err: run.err,
        }
    }

    fn log(&mut self, line: impl Display) {
        // A log that cannot be written is no reason to fail the step.
        let _ = writeln!(self.err, "kbf update-hook: {line}");
    }

    /// Runs the step `inv` names and says how it ended.
    pub fn step(&mut self, inv: &Invocation) -> Done {
        self.rec = match self.store.load() {
            Ok(rec) => rec,
            Err(e) => return Done::failed("record_unreadable", Class::Environment, e),
        };
        let flow = match inv.step {
            Step::Prepare => self.prepare(inv.artifact.as_deref()),
            Step::Cutover => self.cutover(),
            Step::Verify => self.verify(),
            Step::Rollback => self.rollback(),
            Step::Cleanup => self.cleanup(),
        };
        let (Ok(done) | Err(done)) = flow;
        done
    }

    // ---- the record ----

    /// The record, if it is of this update.
    fn mine(&self) -> Option<&Record> {
        self.rec
            .as_ref()
            .filter(|rec| rec.is_of(self.target.as_str(), &self.digest))
    }

    /// The record of this update, which a step after `prepare` needs.
    fn require(&self) -> Result<Record, Done> {
        match self.mine() {
            None => Err(Done::failed(
                "not_prepared",
                Class::Release,
                "this update has no record: prepare has not run",
            )),
            Some(rec) if rec.rolled_back => Err(Done::failed(
                "rolled_back",
                Class::Release,
                "this update was rolled back",
            )),
            Some(rec) if !rec.prepared => Err(Done::failed(
                "not_prepared",
                Class::Release,
                "prepare has not finished",
            )),
            Some(rec) => Ok(rec.clone()),
        }
    }

    /// Changes the record and keeps it, if it changed.
    fn update(&mut self, change: impl FnOnce(&mut Record)) -> Result<(), Done> {
        let mut next = self
            .mine()
            .cloned()
            .unwrap_or_else(|| Record::new(self.target.as_str(), &self.digest));
        change(&mut next);
        if self.mine() == Some(&next) {
            return Ok(());
        }
        self.store
            .save(&next)
            .map_err(|e| Done::failed("record_unwritable", Class::Environment, e))?;
        self.rec = Some(next);
        Ok(())
    }

    // ---- prepare ----

    fn prepare(&mut self, artifact: Option<&Path>) -> Flow {
        let world = self.world;
        // The argument is the staged release, byte for byte (the contract's rule).
        if self.params.staged_release().as_deref() != artifact {
            return Err(Done::failed(
                "artifact_mismatch",
                Class::Release,
                "the artifact is not the staged release of the params",
            ));
        }
        // A record of another update, or of one that is over, is not this attempt's.
        if self
            .rec
            .as_ref()
            .is_some_and(|rec| !rec.is_of(self.target.as_str(), &self.digest) || rec.is_over())
        {
            self.rec = None;
        }
        if self
            .rec
            .as_ref()
            .is_some_and(|rec| rec.prepared || rec.current)
        {
            return Ok(Done::ok());
        }
        if let Some(why) = world.health.refusal(&world.host) {
            return Err(self.stopped(Done::failed("refused", Class::Environment, why)));
        }
        let state = world
            .local
            .query()
            .map_err(|e| self.stopped(Done::helper("helper_unreachable", &e)))?;
        // A trial the helper has already: this update's own, from before a restart.
        if let Some(trial) = state.pending {
            return self.adopt(&trial);
        }
        if state.version.as_ref() == Some(&self.target) {
            self.update(Record::made_current)?;
            return Ok(Done::ok());
        }
        let staged = world
            .local
            .install(&self.target, self.channel)
            .map_err(|e| self.stopped(Done::helper("install_failed", &e)))?;
        if staged.current {
            // The host runs this release already (the helper now knows its version's text).
            self.update(Record::made_current)?;
            return Ok(Done::ok());
        }
        self.drain()?;
        self.update(|rec| rec.prepared = true)?;
        Ok(Done::ok())
    }

    /// Logs why `prepare` stops, as today's update does, and passes the end on.
    fn stopped(&mut self, done: Done) -> Done {
        let why = done.report.detail.clone().unwrap_or_default();
        self.log(format!("update to {}: {why}", self.target));
        done
    }

    /// The helper has a trial on: finish it if it is this update's, with the node as it reads.
    fn adopt(&mut self, trial: &Trial) -> Flow {
        let world = self.world;
        if trial.version.as_ref() != Some(&self.target) {
            return Err(self.stopped(Done::failed(
                "foreign_trial",
                Class::Release,
                format!(
                    "the helper has {} on trial, which this update did not start",
                    trial.commit
                ),
            )));
        }
        self.log(format!(
            "update to {}: resuming the trial the helper has",
            self.target
        ));
        // The drain was made by the process that began this A/B. The node reads as in
        // placement if the updater cordoned it (its mark is on disk), and as out if an
        // operator did. If it cannot be read, nothing says it is ours.
        let placement = match self.mine().and_then(|rec| rec.placement) {
            Some(placement) => placement,
            None => match world.drain.placed(&world.host) {
                Ok(placed) => placed.into(),
                Err(e) => {
                    self.log(format!(
                        "update to {}: the node could not be read, so it is not given back: {e}",
                        self.target
                    ));
                    Was::Out
                }
            },
        };
        let deadline = trial.deadline_unix.unwrap_or(0);
        self.update(|rec| {
            rec.placement = Some(placement);
            rec.drain_begun = true;
            rec.prepared = true;
            rec.switch_begun = true;
            rec.deadline_unix = Some(deadline);
        })?;
        Ok(Done::ok())
    }

    /// Reads whether the node is in placement, then drains it and waits until it holds no
    /// lease, however long that takes, or until the drain deadline, if the operator set one, is
    /// out. At that deadline nothing is switched: the step exits `75` with the node left
    /// drained (its leases run on).
    fn drain(&mut self) -> Result<(), Done> {
        let world = self.world;
        // The placement is read before the drain. A record that has it already is a resume: a
        // node recorded as in placement is read again, because an operator may have drained it
        // since (the cordon marks of the drain tell a node drained by this updater, which reads
        // as in placement, from one that is not). A node recorded as out stays out.
        let recorded = self.mine().and_then(|rec| rec.placement);
        if recorded != Some(Was::Out) {
            let placed = world.drain.placed(&world.host).map_err(|e| {
                self.stopped(Done::failed("node_unreadable", Class::Environment, e))
            })?;
            if recorded.is_none() || Was::from(placed) == Was::Out {
                self.update(|rec| rec.placement = Some(placed.into()))?;
            }
        }
        // The mark goes first: a step killed in the drain finds it, and `rollback` gives the
        // node back. A drain begun again is a drain to give back again.
        self.update(|rec| {
            rec.drain_begun = true;
            rec.given_back = false;
        })?;
        if let Err(e) = world.drain.drain(&world.host, self.timing.drain_deadline) {
            // An answer that was lost may be a drain that landed: the mark stays. A refusal, or
            // a call that never reached the frontend, clears it.
            if !e.is_unreachable() || e.is_refused() {
                self.update(|rec| rec.drain_begun = false)?;
            }
            return Err(self.stopped(Done::failed(
                "drain_failed",
                Class::Environment,
                format!("the node could not be drained: {e}"),
            )));
        }
        let start = world.clock.now();
        loop {
            let leases = world.drain.leases(&world.host).map_err(|e| {
                self.stopped(Done::failed("leases_unreadable", Class::Environment, e))
            })?;
            let waited = world.clock.now().saturating_sub(start);
            if leases == 0 {
                return Ok(());
            }
            if let Some(deadline) = self.timing.drain_deadline.filter(|d| waited >= *d) {
                let why = format!(
                    "the node still holds {leases} leases after the drain deadline of {} s; \
                     nothing was switched and the leases run on",
                    deadline.as_secs()
                );
                self.log(format!("update to {}: {why}", self.target));
                // What this update placed is lifted before the deferral (3.6): the deferral
                // must not depend on a later step.
                self.give_back()?;
                return Err(Done::deferred(
                    "drain_bound",
                    self.timing.retry.as_secs(),
                    why,
                ));
            }
            world.clock.sleep(self.timing.drain_poll);
        }
    }

    // ---- cutover ----

    fn cutover(&mut self) -> Flow {
        let world = self.world;
        let rec = self.require()?;
        if rec.current || rec.deadline_unix.is_some() {
            return Ok(Done::ok());
        }
        if rec.switch_begun {
            // Killed inside the switch, perhaps: the helper may have the trial already.
            let state = world
                .local
                .query()
                .map_err(|e| Done::helper("helper_unreachable", &e))?;
            if let Some(trial) = state
                .pending
                .filter(|trial| trial.version.as_ref() == Some(&self.target))
            {
                let deadline = trial.deadline_unix.unwrap_or(0);
                self.update(|rec| rec.deadline_unix = Some(deadline))?;
                return Ok(Done::ok());
            }
        } else {
            self.update(|rec| rec.switch_begun = true)?;
        }
        match world.local.switch(&self.target) {
            Ok(deadline) => {
                self.update(|rec| rec.deadline_unix = Some(deadline))?;
                Ok(Done::ok())
            }
            Err(e) => {
                self.log(format!("update to {}: the switch failed: {e}", self.target));
                Err(Done::helper("switch_failed", &e))
            }
        }
    }

    // ---- verify ----

    fn verify(&mut self) -> Flow {
        let rec = self.require()?;
        if rec.current {
            self.update(|rec| rec.verified = true)?;
            return Ok(Done::ok());
        }
        let Some(deadline) = rec.deadline_unix else {
            return Err(Done::failed(
                "not_cut_over",
                Class::Release,
                "cutover has not run",
            ));
        };
        // Never past the helper's own deadline, less the margin kept for going back.
        let left = Duration::from_secs(deadline.saturating_sub((self.world.unix_now)()));
        let bound = self
            .timing
            .health_timeout
            .min(left.saturating_sub(self.timing.margin));
        match self.wait_healthy(&self.target.clone(), bound) {
            Health::Healthy => {
                self.update(|rec| rec.verified = true)?;
                Ok(Done::ok())
            }
            Health::Waiting(why) => {
                self.log(format!(
                    "update to {}: not healthy after the time allowed: {why}",
                    self.target
                ));
                Err(Done::failed("not_healthy", Class::Release, why))
            }
            Health::Abandoned(why) => {
                self.log(format!(
                    "update to {}: it will not get there: {why}",
                    self.target
                ));
                Err(Done::failed("abandoned", Class::Release, why))
            }
        }
    }

    fn wait_healthy(&self, version: &Version, bound: Duration) -> Health {
        let world = self.world;
        wait_healthy(
            &*world.health,
            &*world.clock,
            &world.host,
            version,
            Waits {
                bound,
                poll: self.timing.health_poll,
            },
        )
    }

    // ---- rollback ----

    fn rollback(&mut self) -> Flow {
        let Some(rec) = self.mine().cloned() else {
            // Nothing of this update was done: nothing to undo.
            return Ok(Done::ok());
        };
        if rec.rolled_back || rec.current {
            return Ok(Done::ok());
        }
        if rec.confirmed || (rec.confirm_begun && self.confirmed_in_fact()?) {
            return Err(Done::with(
                Outcome::Failure,
                Report {
                    reason: Some("committed".to_owned()),
                    reversible: false,
                    detail: Some(
                        "the helper has confirmed the release: it cannot go back".to_owned(),
                    ),
                    ..Report::default()
                },
            ));
        }
        if rec.switch_begun {
            self.back_on_a()?;
        } else {
            // Nothing was switched: the node is given back, as an unreadable lease count does
            // today, with no health to wait for.
            self.give_back()?;
        }
        self.update(|rec| rec.rolled_back = true)?;
        Ok(Done::ok())
    }

    /// The helper has no trial and runs the target: it was confirmed before this step was
    /// killed or run again.
    fn confirmed_in_fact(&mut self) -> Result<bool, Done> {
        let state = self
            .world
            .local
            .query()
            .map_err(|e| Done::helper("helper_unreachable", &e))?;
        Ok(state.pending.is_none() && state.version.as_ref() == Some(&self.target))
    }

    /// B did not work: the helper puts A back, and the node is given back only if A is healthy.
    /// `Err` says what did not work; the node is then as it was left.
    fn back_on_a(&mut self) -> Result<(), Done> {
        let world = self.world;
        let mut failure: Option<Done> = None;
        match world.local.revert() {
            Ok(()) => {}
            // Nothing on trial: the helper went back by itself, or the switch never held.
            Err(LocalError::Refused(code)) if code == "nothing_pending" => {}
            Err(e) => {
                self.log(format!("update to {}: the revert failed: {e}", self.target));
                failure = Some(Done::helper("revert_failed", &e));
            }
        }
        let running = match world.local.query() {
            Ok(state) => state
                .version
                .or_else(|| state.commit.as_deref().and_then(version_of_commit)),
            Err(e) => {
                self.log(format!(
                    "update to {}: the host does not say what runs: {e}",
                    self.target
                ));
                None
            }
        };
        let healthy = match running {
            Some(a) => self.wait_healthy(&a, self.timing.health_timeout) == Health::Healthy,
            None => false,
        };
        if healthy {
            if let Err(done) = self.give_back() {
                failure.get_or_insert(done);
            }
        } else {
            self.log(format!(
                "update to {}: A is not healthy: the node stays drained",
                self.target
            ));
            failure.get_or_insert(Done::failed(
                "a_unhealthy",
                Class::Release,
                "the old release is not healthy: the node stays drained",
            ));
        }
        failure.map_or(Ok(()), Err)
    }

    /// Gives the node back, if this update drained it and it was in placement before: a node
    /// that was out of placement is left out, and the log says so.
    fn give_back(&mut self) -> Result<(), Done> {
        let world = self.world;
        // With no record nothing was drained: there is nothing to give back.
        let rec = self.mine().cloned().unwrap_or_default();
        if rec.given_back || !rec.drain_begun {
            return Ok(());
        }
        if rec.placement == Some(Was::In) {
            if let Err(e) = world.drain.undrain(&world.host) {
                self.log(format!(
                    "update to {}: the node stays drained: {e}",
                    self.target
                ));
                return Err(Done::failed("undrain_failed", Class::Environment, e));
            }
        } else {
            self.log(LEFT_OUT);
        }
        self.update(|rec| rec.given_back = true)
    }

    // ---- cleanup ----

    /// A failure of `cleanup` after the commit, or with a confirm that may have landed, says it
    /// is not reversible; the failure of a confirm the helper refused, which was reverted,
    /// says it is, and the updater's `rollback` that follows finds nothing left to do.
    fn cleanup(&mut self) -> Flow {
        let flow = self.cleanup_steps();
        match flow {
            Err(mut done) if self.mine().is_some_and(|r| r.confirmed || r.confirm_begun) => {
                done.report.reversible = false;
                Err(done)
            }
            other => other,
        }
    }

    fn cleanup_steps(&mut self) -> Flow {
        let rec = self.require()?;
        if !rec.verified {
            return Err(Done::failed(
                "not_verified",
                Class::Release,
                "verify has not succeeded: nothing is finished",
            ));
        }
        if rec.current {
            return Ok(Done::committed());
        }
        if !rec.confirmed {
            self.confirm(&rec)?;
        }
        self.give_back()?;
        Ok(Done::committed())
    }

    /// Confirms the target with the helper: the commit. The mark goes first. A confirm the
    /// helper refuses is reverted like any other release that did not work.
    fn confirm(&mut self, before: &Record) -> Result<(), Done> {
        let world = self.world;
        self.update(|rec| rec.confirm_begun = true)?;
        if !(before.confirm_begun && self.confirmed_in_fact()?)
            && let Err(e) = world.local.confirm(&self.target)
        {
            self.log(format!(
                "update to {}: the confirm failed: {e}",
                self.target
            ));
            if matches!(e, LocalError::Unreachable(_)) {
                // The answer was lost: the confirm may have landed, so the release is not put
                // back. The mark stays, and a step run again looks at the helper.
                return Err(Done::helper("confirm_unknown", &e));
            }
            self.update(|rec| rec.confirm_begun = false)?;
            if self.back_on_a().is_ok() {
                self.update(|rec| rec.rolled_back = true)?;
            }
            return Err(Done::helper("confirm_refused", &e));
        }
        self.update(|rec| rec.confirmed = true)
    }
}
