//! The all-in-one swap (FP section 5a): one process owns frontend and storage, so the old
//! one stops before the new one starts, and the proxy holds new streams in between.
//!
//! Order: hold (up to `hold_max`, 300 s), quiesce the old process (it finishes its running
//! actions, however long that takes), stop it, migrate, start the new process, wait for
//! ready, release the held streams into it. A stream still held at `hold_max` fails with
//! `UNAVAILABLE` and the update goes on. `abort` is honoured until the old process is told
//! to stop.
//!
//! Assumption (not in FP): after `quiesce` reports `Quiesced` the proxy sends SIGTERM;
//! FP says only "the old process exits". Kept in [`Swap::stop_old`].

use super::model::{Halt, Mode, Phase, Request, Retired, RoleState, Slot, Step};
use super::swap::{Flow, Swap, go, go_end};

impl Swap<'_> {
    pub(super) fn run_aio(&mut self) -> Result<Phase, Halt> {
        let phase = self.intent.phase;
        if phase.is_terminal() {
            return Ok(phase);
        }
        if phase <= Phase::Preflight {
            go_end!(self.preflight());
            go_end!(self.hold_and_quiesce());
            go_end!(self.stop_old());
            go_end!(self.start_new());
        } else if phase < Phase::Switched {
            go_end!(self.recover_aio());
        } else {
            self.recover_aio_post_switch()?;
        }
        self.settle()
    }

    /// Hold, then quiesce the old process and wait until it is idle (or gone). Abort is
    /// honoured here: the old process resumes and the held streams go back to it.
    fn hold_and_quiesce(&mut self) -> Result<Flow, Halt> {
        let t = self.plan.timing;
        let old = self.intent.old_slot;
        self.set_phase(Phase::Holding)?;
        if let Err(e) = self.ports.gate.hold(t.hold_max) {
            return self.fail(Step::Hold, e.to_string());
        }
        self.set_phase(Phase::Quiescing)?;
        if let Err(e) = self.ports.admin.quiesce(old) {
            return self.aio_aborted(format!("quiesce: {e}"), Step::Switch);
        }
        loop {
            self.expire_held();
            if self.ports.child.exited(&self.old_ident()?).is_some() {
                self.old_alive = false;
                return Ok(Flow::Go);
            }
            if matches!(self.ports.admin.role(old), Ok(r) if r.state == RoleState::Quiesced) {
                return Ok(Flow::Go);
            }
            if let Some(Request::Abort) = self.next_request() {
                return self
                    .aio_aborted("aborted before the old process exited".into(), Step::Abort);
            }
            self.sleep(t.poll)?;
        }
    }

    /// The old process is still alive and was quiesced: bring it back.
    fn aio_aborted(&mut self, reason: String, step: Step) -> Result<Flow, Halt> {
        let old = self.intent.old_slot;
        let role = self.role_of(old);
        self.resume_old(role, old);
        self.release_held(old)?;
        self.fail(step, reason)
    }

    /// The point of no return: the old process is idle, so stop it (SIGTERM, no timeout)
    /// and wait for it to go. `abort` is refused from here.
    pub(super) fn stop_old(&mut self) -> Result<Flow, Halt> {
        self.set_phase(Phase::Switching)?;
        let old_slot = self.intent.old_slot;
        let ident = self.old_ident()?;
        let poll = self.plan.timing.poll;
        let mut terminated = false;
        while self.old_alive {
            if self.ports.child.exited(&ident).is_some() {
                self.old_alive = false;
                break;
            }
            if !terminated {
                if let Err(e) = self.ports.child.terminate(&ident) {
                    self.note(format!("SIGTERM to the old process failed: {e}"));
                }
                terminated = true;
            }
            self.expire_held();
            if let Some(Request::Abort) = self.next_request() {
                self.note("abort refused: the old process is stopping".into());
            }
            self.sleep(poll)?;
        }
        // The process is gone: no slot is active, and the slot closes once the streams that
        // were bound to it have drained (its sockets are gone, so they do). Only then may
        // the next process, or the old release again, open a slot (a new generation).
        self.ports.gate.clear_active();
        self.wait_closed(old_slot)?;
        Ok(Flow::Go)
    }

    /// Poll the gate's retire of `slot` until it is closed. A slot that is still busy at the
    /// hold bound is noted once as stuck; there is no timeout, like every wait on a process
    /// that served, and the streams held meanwhile keep expiring on their own.
    fn wait_closed(&mut self, slot: Slot) -> Result<(), Halt> {
        let (poll, bound) = (self.plan.timing.poll, self.plan.timing.hold_max);
        let started = self.now();
        let mut noted = false;
        loop {
            self.expire_held();
            let answer = self.ports.gate.retire(slot);
            if answer.is_closed() || answer == Retired::NeverOpened {
                return Ok(());
            }
            if !noted && self.now().saturating_sub(started) >= bound {
                self.note(format!(
                    "the stopped process's slot {slot:?} still has streams bound after the hold bound: {answer:?}"
                ));
                noted = true;
            }
            self.sleep(poll)?;
        }
    }

    /// Migrate, start the new process, wait for ready and release the held streams into it.
    /// If it cannot come up, the old release is started again: the old process is gone.
    pub(super) fn start_new(&mut self) -> Result<Flow, Halt> {
        let new = self.intent.new_slot;
        if !self.new_alive {
            if let Err(e) = self.ports.migrator.migrate_forward(&self.plan.to) {
                return self.aio_fallback(Step::Migrate, e.to_string());
            }
            let first = self.now();
            match self.ports.child.spawn(new, &self.plan.to) {
                Ok(ident) => self.hold_candidate(ident)?,
                Err(e) => return self.aio_fallback(Step::Start, format!("spawn: {e}")),
            }
            if let Err((step, reason)) = self.await_candidate(first, false)? {
                return self.aio_fallback(step, reason);
            }
        } else if let Err((step, reason)) = self.await_candidate(self.now(), false)? {
            return self.aio_fallback(step, reason);
        }
        self.set_phase(Phase::Switched)?;
        self.release_held(new)?;
        Ok(Flow::Go)
    }

    /// The new process would not come up. Start the old release again in the old slot so
    /// the host is not left without a frontend, release the held streams into it, and fail.
    fn aio_fallback(&mut self, step: Step, reason: String) -> Result<Flow, Halt> {
        let old = self.intent.old_slot;
        self.kill_candidate();
        match self.ports.child.spawn(old, &self.plan.from) {
            Ok(ident) => {
                self.intent.old_proc = Some(ident.clone());
                self.old_alive = true;
                self.save()?;
                let t = self.plan.timing;
                let first = self.now();
                loop {
                    if matches!(self.ports.admin.ready(old), Ok(r) if r.all()) {
                        self.release_held(old)?;
                        break;
                    }
                    if self.ports.child.exited(&ident).is_some()
                        || self.now().saturating_sub(first) >= t.ready_within
                    {
                        self.note("the old release did not come back".into());
                        self.ports.gate.expire();
                        break;
                    }
                    self.sleep(t.poll)?;
                }
            }
            Err(e) => self.note(format!("restarting the old release failed: {e}")),
        }
        self.fail(step, reason)
    }

    /// A restart before the new process served. Holding and quiescing: the hold is gone,
    /// the old process resumes if it lives. Switching: the old process was told to stop,
    /// so the update goes forward.
    fn recover_aio(&mut self) -> Result<Flow, Halt> {
        let old = self.intent.old_slot;
        if self.intent.phase < Phase::Switching && self.old_alive {
            let role = self.role_of(old);
            self.resume_old(role, old);
            self.release_held(old)?;
            return self.fail(
                Step::Restart,
                "the proxy restarted while the old process quiesced".into(),
            );
        }
        go!(self.stop_old());
        self.start_new()
    }

    fn recover_aio_post_switch(&mut self) -> Result<(), Halt> {
        let new = self.intent.new_slot;
        if self.new_alive {
            self.release_held(new)?;
        }
        Ok(())
    }

    fn old_ident(&self) -> Result<super::model::ProcIdent, Halt> {
        debug_assert_eq!(self.intent.mode, Mode::AllInOne);
        self.intent
            .old_proc
            .clone()
            .ok_or_else(|| Halt::Port("no old process is recorded".into()))
    }
}
