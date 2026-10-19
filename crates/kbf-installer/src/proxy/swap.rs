//! The swap orchestrator for a frontend host: FP section 5 steps 3 to 10, driven against
//! the traits of [`super::ports`]. The all-in-one variant is in [`super::aio`], recovery
//! after a restart in [`super::recover`], rollback and retire in [`super::retire`].
//!
//! One rule holds everywhere: record the phase (to `update.json`), then act, and make every
//! action safe to repeat. A [`Halt`] means the proxy stopped; nothing more is done.

use std::time::Duration;

use super::model::{
    Failure, Halt, Intent, Phase, Plan, ProcIdent, Request, RoleInfo, RoleState, Slot, Step,
};
use super::ports::Ports;

/// What a step tells its caller: go on, or the update ended in this terminal phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Flow {
    Go,
    End(Phase),
}

/// `go!(step)` runs a step and returns early from the caller if the update ended.
macro_rules! go {
    ($e:expr) => {
        if let Flow::End(p) = $e? {
            return Ok(Flow::End(p));
        }
    };
}
pub(super) use go;

/// Like `go!`, for the drivers that return the final phase: the update ended, so that is
/// the answer.
macro_rules! go_end {
    ($e:expr) => {
        if let Flow::End(p) = $e? {
            return Ok(p);
        }
    };
}
pub(super) use go_end;

/// One update in progress.
pub struct Swap<'a> {
    pub(super) ports: Ports<'a>,
    pub(super) plan: Plan,
    pub(super) intent: Intent,
    /// The slot that new streams go to, as far as this proxy has set it.
    pub(super) active: Slot,
    /// The recorded process is the live one (spawned, or adopted after a check). The swap
    /// signals only a process it holds this for, so a recycled pid is never touched.
    pub(super) old_alive: bool,
    pub(super) new_alive: bool,
    /// The root's `current` names the new release (read at a restart; see
    /// [`Swap::read_root`]). False on a swap that was not adopted after the bake began.
    pub(super) root_confirmed: bool,
    notes: Vec<String>,
}

impl<'a> Swap<'a> {
    /// Start an update from the `staged` phase: write the first intent record.
    /// `old_proc` is the serving child, kept so that a restarted proxy can adopt it.
    pub fn begin(
        ports: Ports<'a>,
        plan: Plan,
        old_proc: Option<ProcIdent>,
    ) -> Result<Swap<'a>, Halt> {
        let intent = Intent::begin(&plan, old_proc, ports.clock.now().as_secs());
        ports.recorder.record(&intent)?;
        let old_alive = intent.old_proc.is_some();
        let active = plan.old_slot;
        Ok(Swap {
            ports,
            plan,
            intent,
            active,
            old_alive,
            new_alive: false,
            root_confirmed: false,
            notes: Vec::new(),
        })
    }

    pub(super) fn resumed(
        ports: Ports<'a>,
        plan: Plan,
        intent: Intent,
        active: Slot,
        old_alive: bool,
        new_alive: bool,
    ) -> Swap<'a> {
        Swap {
            ports,
            plan,
            intent,
            active,
            old_alive,
            new_alive,
            root_confirmed: false,
            notes: Vec::new(),
        }
    }

    pub fn intent(&self) -> &Intent {
        &self.intent
    }

    /// What the swap chose to carry on past: calls that failed without ending the update
    /// (a stand-down the fence makes harmless, a `desired` post that is repeated).
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// Drive the update from the phase the intent names to a terminal phase. A fresh
    /// swap starts at `staged`; an adopted one resumes where the record says.
    pub fn run(&mut self) -> Result<Phase, Halt> {
        match self.intent.mode {
            super::model::Mode::Frontend => self.run_frontend(),
            super::model::Mode::AllInOne => self.run_aio(),
        }
    }

    fn run_frontend(&mut self) -> Result<Phase, Halt> {
        let phase = self.intent.phase;
        if phase.is_terminal() {
            return Ok(phase);
        }
        if phase <= Phase::Preflight {
            go_end!(self.preflight());
            go_end!(self.start());
            go_end!(self.switch());
        } else if phase < Phase::Switched {
            go_end!(self.recover_pre_switch());
        } else {
            go_end!(self.read_root());
            go_end!(self.recover_post_switch());
        }
        self.settle()
    }

    /// Bake, confirm, nodes, and (on a frontend) drain and retire the old child.
    pub(super) fn settle(&mut self) -> Result<Phase, Halt> {
        // A confirm the root already holds is not asked for again.
        if self.intent.phase <= Phase::Baking && !self.root_confirmed {
            go_end!(self.bake());
        }
        // Nodes follow a confirmed release only: with the confirm failed the host still
        // boots the old `current`, so the nodes are not sent to the new one.
        if self.intent.phase <= Phase::Nodes && !self.confirm_failed() {
            go_end!(self.nodes());
        }
        if self.intent.mode == super::model::Mode::AllInOne {
            let end = self.end_phase();
            self.set_phase(end)?;
            return Ok(end);
        }
        self.drain()
    }

    // ---- bookkeeping ----

    pub(super) fn now(&self) -> Duration {
        self.ports.clock.now()
    }

    pub(super) fn note(&mut self, text: String) {
        self.notes.push(text);
    }

    pub(super) fn sleep(&self, d: Duration) -> Result<(), Halt> {
        self.ports.clock.sleep(d)
    }

    /// Record the intent as it stands.
    pub(super) fn save(&self) -> Result<(), Halt> {
        self.ports.recorder.record(&self.intent)
    }

    /// Record a new phase, before the action it announces.
    pub(super) fn set_phase(&mut self, phase: Phase) -> Result<(), Halt> {
        self.intent.phase = phase;
        self.intent.phase_started = self.now().as_secs();
        self.save()
    }

    /// The `confirm` order could not be delivered; the failure is recorded and the update
    /// goes on to retire the old child.
    pub(super) fn confirm_failed(&self) -> bool {
        matches!(&self.intent.failure, Some(f) if f.step == Step::Confirm)
    }

    /// The phase a swap that got through the drain ends in.
    pub(super) fn end_phase(&self) -> Phase {
        if self.intent.rolling_back {
            // A switch that failed behind the proxy's back recorded its failure
            // (`switch`) before it rolled back: that is `failed`. The operator's rollback
            // records none, and is the only `rolled_back`.
            if self.intent.failure.is_some() {
                Phase::Failed
            } else {
                Phase::RolledBack
            }
        } else if self.confirm_failed() {
            Phase::Failed
        } else {
            Phase::Done
        }
    }

    /// End the update as failed at `step`. Before the switch the staged release is dropped:
    /// nothing flips. After it the new child has served, and the staged release stays, so
    /// the new child is never left running a release the host would not boot.
    pub(super) fn fail(&mut self, step: Step, reason: String) -> Result<Flow, Halt> {
        if self.intent.phase < Phase::Switched {
            self.revert_staged();
        }
        self.intent.failure = Some(Failure { step, reason });
        self.set_phase(Phase::Failed)?;
        Ok(Flow::End(Phase::Failed))
    }

    pub(super) fn revert_staged(&mut self) {
        if let Err(e) = self.ports.orders.revert(&self.plan.to) {
            self.note(format!("revert of the staged release failed: {e}"));
        }
    }

    pub(super) fn next_request(&mut self) -> Option<Request> {
        self.ports.requests.next()
    }

    pub(super) fn slot_proc(&self, slot: Slot) -> Option<ProcIdent> {
        if slot == self.intent.old_slot {
            self.intent.old_proc.clone()
        } else {
            self.intent.new_proc.clone()
        }
    }

    pub(super) fn slot_alive(&self, slot: Slot) -> bool {
        if slot == self.intent.old_slot {
            self.old_alive
        } else {
            self.new_alive
        }
    }

    /// What `slot` says about itself, if it is ours and answers.
    pub(super) fn role_of(&self, slot: Slot) -> Option<RoleInfo> {
        if !self.slot_alive(slot) {
            return None;
        }
        self.ports.admin.role(slot).ok()
    }

    /// Kill the candidate: it never served. A candidate we do not hold (never spawned, or
    /// a recycled pid after a restart) is left alone. Every caller is before the switch:
    /// after it the new child has served, and a served child only gets SIGTERM, after it
    /// drained (FP section 5 step 10).
    pub(super) fn kill_candidate(&mut self) {
        if !self.new_alive {
            return;
        }
        debug_assert!(
            self.intent.phase < Phase::Switched,
            "a child that served is never killed"
        );
        if let Some(ident) = self.intent.new_proc.clone()
            && let Err(e) = self.ports.child.kill_unserved(&ident)
        {
            self.note(format!("killing the candidate failed: {e}"));
        }
        self.new_alive = false;
    }

    /// Release the hold into `slot` after failing the streams that waited too long.
    pub(super) fn release_held(&mut self, slot: Slot) -> Result<(), Halt> {
        self.expire_held();
        self.ports
            .gate
            .release_into(slot)
            .map_err(|e| Halt::Port(format!("release into {slot:?}: {e}")))?;
        self.active = slot;
        Ok(())
    }

    /// Fail the held streams that waited out `hold_max`. Polled in every wait under a hold.
    pub(super) fn expire_held(&mut self) {
        let n = self.ports.gate.expire();
        if n > 0 {
            self.note(format!("{n} held streams expired with UNAVAILABLE"));
        }
    }

    // ---- steps ----

    /// The staged release is the one this update names, and the forward migrations are
    /// applied before any candidate exists (CEO 14:58Z).
    pub(super) fn preflight(&mut self) -> Result<Flow, Halt> {
        match self.ports.orders.staged() {
            Ok(Some(facts)) if facts.commit == self.plan.to => {}
            Ok(_) => {
                return self.fail(
                    Step::Stage,
                    "the staged release is not this update's".into(),
                );
            }
            Err(e) => return self.fail(Step::Stage, format!("status.json: {e}")),
        }
        self.set_phase(Phase::Preflight)?;
        if let Some(Request::Abort) = self.next_request() {
            return self.fail(Step::Abort, "aborted before the candidate started".into());
        }
        if self.intent.mode == super::model::Mode::Frontend
            && let Err(e) = self.ports.migrator.migrate_forward(&self.plan.to)
        {
            return self.fail(Step::Migrate, e.to_string());
        }
        Ok(Flow::Go)
    }

    /// Step 4 and 5: start the candidate in the idle slot and wait until it is ready.
    pub(super) fn start(&mut self) -> Result<Flow, Halt> {
        self.set_phase(Phase::Starting)?;
        let first = self.now();
        let new_slot = self.intent.new_slot;
        match self.ports.child.spawn(new_slot, &self.plan.to) {
            Ok(ident) => self.hold_candidate(ident)?,
            Err(e) => return self.fail(Step::Start, format!("spawn: {e}")),
        }
        match self.await_candidate(first, true)? {
            Ok(()) => {
                self.set_phase(Phase::Ready)?;
                Ok(Flow::Go)
            }
            Err((step, reason)) => {
                self.kill_candidate();
                self.fail(step, reason)
            }
        }
    }

    /// Keep a freshly spawned candidate in the record.
    pub(super) fn hold_candidate(&mut self, ident: ProcIdent) -> Result<(), Halt> {
        self.intent.new_proc = Some(ident);
        self.new_alive = true;
        self.save()
    }

    /// Wait for the candidate. An exit inside `early_exit` of a spawn earns one respawn;
    /// a second exit, or one later, fails; so does `ready_within` since `first`.
    pub(super) fn await_candidate(
        &mut self,
        first: Duration,
        abortable: bool,
    ) -> Result<Result<(), (Step, String)>, Halt> {
        let t = self.plan.timing;
        let new_slot = self.intent.new_slot;
        let mut spawned_at = first;
        loop {
            let Some(ident) = self.intent.new_proc.clone() else {
                return Err(Halt::Port("no candidate process is recorded".into()));
            };
            if let Some(code) = self.ports.child.exited(&ident) {
                self.new_alive = false;
                let early = self.now().saturating_sub(spawned_at) < t.early_exit;
                if early && !self.intent.respawned {
                    self.intent.respawned = true;
                    self.save()?;
                    match self.ports.child.spawn(new_slot, &self.plan.to) {
                        Ok(again) => {
                            self.hold_candidate(again)?;
                            spawned_at = self.now();
                            continue;
                        }
                        Err(e) => return Ok(Err((Step::Start, format!("respawn: {e}")))),
                    }
                }
                return Ok(Err((
                    Step::Start,
                    format!("the candidate exited with {code} before it was ready"),
                )));
            }
            if matches!(self.ports.admin.ready(new_slot), Ok(r) if r.all()) {
                return Ok(Ok(()));
            }
            if self.now().saturating_sub(first) >= t.ready_within {
                return Ok(Err((
                    Step::Ready,
                    format!(
                        "the candidate was not ready in {} s",
                        t.ready_within.as_secs()
                    ),
                )));
            }
            if abortable && let Some(Request::Abort) = self.next_request() {
                return Ok(Err((
                    Step::Abort,
                    "aborted while the candidate started".into(),
                )));
            }
            self.sleep(t.poll)?;
        }
    }

    /// Step 6: hold, quiesce the old child, take over on the candidate, swap, release.
    pub(super) fn switch(&mut self) -> Result<Flow, Halt> {
        let t = self.plan.timing;
        let (old, new) = (self.intent.old_slot, self.intent.new_slot);
        if let Some(Request::Abort) = self.next_request() {
            self.kill_candidate();
            return self.fail(Step::Abort, "aborted before the hold".into());
        }
        self.set_phase(Phase::Holding)?;
        if let Err(e) = self.ports.gate.hold(t.hold_max) {
            self.kill_candidate();
            return self.fail(Step::Hold, e.to_string());
        }
        self.set_phase(Phase::Quiescing)?;
        if let Err((step, reason)) = self.quiesce_old()? {
            return self.switch_failed(step, reason);
        }
        self.set_phase(Phase::Switching)?;
        let t0 = self.now();
        let result = self.ports.admin.takeover(new, t.takeover);
        self.ports
            .recorder
            .crash_point(super::model::CrashPoint::BeforeTakeoverReturns)?;
        let took = self.now().saturating_sub(t0);
        match result {
            Err(e) => return self.switch_failed(Step::Switch, format!("takeover: {e}")),
            Ok(()) if took > t.takeover => {
                return self.switch_failed(
                    Step::Switch,
                    format!(
                        "takeover took {} ms, over the {} ms limit",
                        took.as_millis(),
                        t.takeover.as_millis()
                    ),
                );
            }
            Ok(()) => {}
        }
        self.ports
            .recorder
            .crash_point(super::model::CrashPoint::AfterTakeoverBeforeSwap)?;
        self.set_phase(Phase::Switched)?;
        self.release_held(new)?;
        // Step 7: the old child keeps serving its open streams from `done/` rows. A
        // stand-down that fails changes nothing, because the fence refuses its writes.
        if let Err(e) = self.ports.admin.stand_down(old) {
            self.note(format!("stand-down of the old child failed: {e}"));
        }
        Ok(Flow::Go)
    }

    /// Ask the old child to quiesce and wait for `Quiesced` inside the budget. An abort
    /// that arrives during the wait ends the update as `abort`, not `switch`.
    fn quiesce_old(&mut self) -> Result<Result<(), (Step, String)>, Halt> {
        let t = self.plan.timing;
        let old = self.intent.old_slot;
        let t0 = self.now();
        if let Err(e) = self.ports.admin.quiesce(old) {
            return Ok(Err((Step::Switch, format!("quiesce: {e}"))));
        }
        loop {
            self.expire_held();
            let waited = self.now().saturating_sub(t0);
            if matches!(self.ports.admin.role(old), Ok(r) if r.state == RoleState::Quiesced) {
                return Ok(if waited > t.quiesce {
                    Err((
                        Step::Switch,
                        format!("quiesce took {} ms", waited.as_millis()),
                    ))
                } else {
                    Ok(())
                });
            }
            if waited >= t.quiesce {
                return Ok(Err((
                    Step::Switch,
                    format!("the old child did not quiesce in {} s", t.quiesce.as_secs()),
                )));
            }
            if let Some(Request::Abort) = self.next_request() {
                return Ok(Err((
                    Step::Abort,
                    "aborted while the old child quiesced".into(),
                )));
            }
            self.sleep(t.tick)?;
        }
    }

    /// The switch did not complete (FP section 5 step 6 "Failure"): kill the candidate
    /// first, then read the roles, then let the old child resume, then release the hold
    /// into it. The old child resumes from the store, not from warm memory.
    pub(super) fn switch_failed(&mut self, step: Step, reason: String) -> Result<Flow, Halt> {
        let (old, new) = (self.intent.old_slot, self.intent.new_slot);
        self.kill_candidate();
        let old_role = self.ports.admin.role(old).ok();
        let _ = self.ports.admin.role(new);
        self.resume_old(old_role, old);
        self.release_held(old)?;
        self.fail(step, reason)
    }

    /// Resume `slot` unless it already serves as the active child.
    pub(super) fn resume_old(&mut self, role: Option<RoleInfo>, slot: Slot) {
        let serving = matches!(role, Some(r) if r.state == RoleState::Active && r.holder);
        if serving || !self.slot_alive(slot) {
            return;
        }
        if let Err(e) = self.ports.admin.resume(slot) {
            self.note(format!("resume of {slot:?} failed: {e}"));
        }
    }

    /// Step 8: wait out the bake, then ask the root half to flip (`confirm`). A dead new
    /// child makes the bake red: there is no automatic rollback, the operator asks for one.
    pub(super) fn bake(&mut self) -> Result<Flow, Halt> {
        let t = self.plan.timing;
        if self.intent.bake_started.is_none() {
            self.intent.bake_started = Some(self.now().as_secs());
        }
        self.set_phase(Phase::Baking)?;
        let started = self.intent.bake_started.unwrap_or_default();
        loop {
            match self.next_request() {
                Some(Request::Rollback) => go!(self.rollback()),
                Some(Request::Abort) => {
                    self.note("abort refused: the swap already switched".into())
                }
                None => {}
            }
            let red = self.new_exited();
            if !red && self.now().as_secs() >= started + t.bake.as_secs() {
                break;
            }
            self.sleep(t.poll)?;
        }
        self.confirm()?;
        Ok(Flow::Go)
    }

    /// Send the `confirm` order, again with a doubling wait if the root half refuses or
    /// cannot be reached. The new child serves, so the update does not undo itself: when
    /// every try fails, `failure{step: confirm}` is recorded, the staged release is kept,
    /// the nodes are not sent, and the old child is still drained and retired. The update
    /// then ends `failed`. A later success clears the failure.
    fn confirm(&mut self) -> Result<(), Halt> {
        let t = self.plan.timing;
        let mut wait = t.poll;
        for attempt in 1..=t.confirm_attempts {
            match self.ports.orders.confirm(&self.plan.to) {
                Ok(()) => {
                    self.ports
                        .recorder
                        .crash_point(super::model::CrashPoint::AfterConfirmLanded)?;
                    if self.confirm_failed() {
                        self.intent.failure = None;
                        self.save()?;
                    }
                    return Ok(());
                }
                Err(e) => {
                    self.intent.failure = Some(Failure {
                        step: Step::Confirm,
                        reason: format!("{e} (try {attempt} of {})", t.confirm_attempts),
                    });
                    self.save()?;
                }
            }
            if attempt < t.confirm_attempts {
                self.sleep(wait)?;
                wait = (wait * 2).min(t.confirm_backoff_max);
            }
        }
        Ok(())
    }

    fn new_exited(&self) -> bool {
        match &self.intent.new_proc {
            Some(ident) => !self.new_alive || self.ports.child.exited(ident).is_some(),
            None => true,
        }
    }

    /// Step 9: the new child updates the nodes. A failure here does not undo the frontend.
    pub(super) fn nodes(&mut self) -> Result<Flow, Halt> {
        if !self.plan.nodes {
            return Ok(Flow::Go);
        }
        self.set_phase(Phase::Nodes)?;
        match self
            .ports
            .admin
            .post_desired(self.intent.new_slot, &self.plan.to)
        {
            Ok(()) => {
                self.intent.nodes_posted = true;
                self.save()?;
            }
            Err(e) => self.note(format!("posting desired failed: {e}")),
        }
        Ok(Flow::Go)
    }
}
