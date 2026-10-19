//! Step 10 (drain and retire) and the operator's rollback.
//!
//! A child that has served is only ever told SIGTERM, and only when it is provably idle:
//! its own counts are zero and the gate's `retire` closed the slot.
//! There is no timeout and no SIGKILL on this path: a stuck stream stays visible in the
//! status until the operator force-retires it (U8b).

use super::model::{CrashPoint, Halt, Mode, Phase, Retired, RoleState, Slot, Step};
use super::swap::{Flow, Swap};

impl Swap<'_> {
    /// Step 10: drain the old child, then retire it.
    pub(super) fn drain(&mut self) -> Result<Phase, Halt> {
        let slot = self.intent.retire.unwrap_or(self.intent.old_slot);
        self.intent.retire = Some(slot);
        self.set_phase(Phase::Draining)?;
        self.retire(slot)?;
        let end = self.end_phase();
        self.set_phase(end)?;
        Ok(end)
    }

    /// Wait until `slot` is idle, send SIGTERM once, and wait for the exit. Idle means all
    /// of: the slot is not the active one, the proxy holds no permit on it, and the child
    /// reports no open execution and no queued operation. If the child cannot be asked, it
    /// is not provably idle. A child that exits by itself ends the wait at once.
    pub(super) fn retire(&mut self, slot: Slot) -> Result<(), Halt> {
        if !self.slot_alive(slot) {
            return Ok(());
        }
        let Some(ident) = self.slot_proc(slot) else {
            return Ok(());
        };
        self.ports.recorder.crash_point(CrashPoint::WhileDraining)?;
        let poll = self.plan.timing.poll;
        let mut terminated = false;
        loop {
            if self.ports.child.exited(&ident).is_some() {
                self.mark_gone(slot);
                return Ok(());
            }
            if !terminated && self.idle(slot) && self.ports.gate.retire(slot) == Retired::Closed {
                if let Err(e) = self.ports.child.terminate(&ident) {
                    self.note(format!("SIGTERM to {slot:?} failed: {e}"));
                }
                terminated = true;
            }
            self.sleep(poll)?;
        }
    }

    /// The child's own counts are zero and the slot is not the active one. The proxy's
    /// half, the permits, is the gate's `retire`: it is the commit, so it runs last.
    fn idle(&self, slot: Slot) -> bool {
        slot != self.active && matches!(self.ports.admin.role(slot), Ok(r) if r.idle())
    }

    pub(super) fn mark_gone(&mut self, slot: Slot) {
        if slot == self.intent.old_slot {
            self.old_alive = false;
        } else {
            self.new_alive = false;
        }
    }

    /// The operator's rollback (FP section 5 step 8): only between `switched` and the
    /// confirm, while the old child lives. Returns `Go` when it is refused, so the bake
    /// goes on. Once it takes the hold it records `rolling_back` before it touches either
    /// child, so a restart finishes it ([`Swap::recover_rollback`]) instead of mistaking
    /// the new child, which served, for an unserved candidate. The new child stands down,
    /// the old one resumes (full reload, then re-take), the staged release is dropped, and
    /// the new child drains and retires like any other.
    pub(super) fn rollback(&mut self) -> Result<Flow, Halt> {
        let t = self.plan.timing;
        let old_up = self.old_alive
            && self
                .intent
                .old_proc
                .as_ref()
                .is_some_and(|i| self.ports.child.exited(i).is_none());
        if self.intent.mode != Mode::Frontend || !old_up {
            self.note("rollback refused: the old child is not running".into());
            return Ok(Flow::Go);
        }
        if let Err(e) = self.ports.gate.hold(t.hold_max) {
            self.note(format!("rollback refused: the hold failed: {e}"));
            return Ok(Flow::Go);
        }
        self.intent.rolling_back = true;
        self.save()?;
        self.rollback_swap()
    }

    /// The children's half of the rollback, safe to repeat after a restart.
    pub(super) fn rollback_swap(&mut self) -> Result<Flow, Halt> {
        let t = self.plan.timing;
        let (old, new) = (self.intent.old_slot, self.intent.new_slot);
        if self.new_alive
            && let Err(e) = self.ports.admin.stand_down(new)
        {
            self.note(format!("stand-down of the new child failed: {e}"));
        }
        self.ports
            .recorder
            .crash_point(CrashPoint::RollbackAfterStandDown)?;
        let resumed = self.ports.admin.resume(old).map_err(|e| e.to_string());
        self.ports
            .recorder
            .crash_point(CrashPoint::RollbackAfterResume)?;
        let t0 = self.now();
        let mut outcome = resumed;
        while outcome.is_ok() {
            self.expire_held();
            let up = self.ports.admin.role(old);
            if matches!(up, Ok(r) if r.holder() && r.state == RoleState::Active) {
                break;
            }
            if self.now().saturating_sub(t0) >= t.takeover {
                outcome = Err("the old child did not take the generation back".into());
                break;
            }
            self.sleep(t.tick)?;
        }
        if let Err(reason) = outcome {
            return self.rollback_failed(reason);
        }
        self.release_held(old)?;
        self.revert_staged();
        self.intent.retire = Some(new);
        self.drain().map(Flow::End)
    }

    /// The rollback did not land: put the new child back, which stood down, and fail. The
    /// new child served, so the staged release stays.
    pub(super) fn rollback_failed(&mut self, reason: String) -> Result<Flow, Halt> {
        let new = self.intent.new_slot;
        let serving =
            matches!(self.role_of(new), Some(r) if r.state == RoleState::Active && r.holder());
        if self.new_alive
            && !serving
            && let Err(e) = self.ports.admin.resume(new)
        {
            self.note(format!("resume of the new child failed: {e}"));
        }
        self.release_held(new)?;
        self.intent.rolling_back = false;
        self.fail(Step::Rollback, reason)
    }
}
