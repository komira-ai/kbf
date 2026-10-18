//! Adoption after a proxy restart (FP section 6). The recorded intent says where the
//! update was; the store says who serves. Where they disagree the store wins: both
//! children are asked their role, the generation holder is active, and a candidate that
//! does not hold the generation is killed.
//!
//! Children outlive the proxy (`KillMode=process`), so they are adopted, not restarted.
//! A recorded process that is gone or whose pid now belongs to another process is not
//! ours: nothing signals it.

use super::model::{Halt, Intent, Phase, Plan, Step};
use super::ports::Ports;
use super::swap::{Flow, Swap};

impl<'a> Swap<'a> {
    /// Take over an update recorded by a proxy that stopped. The recorded processes are
    /// checked by pid, start time and executable; those that fail are treated as gone.
    pub fn adopt(ports: Ports<'a>, plan: Plan, intent: Intent) -> Swap<'a> {
        let old_alive = match &intent.old_proc {
            Some(r) => ports.child.adopt(intent.old_slot, r).is_ok(),
            None => false,
        };
        let new_alive = match &intent.new_proc {
            Some(r) => ports.child.adopt(intent.new_slot, r).is_ok(),
            None => false,
        };
        let active = plan.old_slot;
        Swap::resumed(ports, plan, intent, active, old_alive, new_alive)
    }

    /// A frontend update that stopped at `starting` to `switching`. If the candidate
    /// holds the generation the takeover landed and the swap completes; otherwise the
    /// old child is active again and the update fails.
    pub(super) fn recover_pre_switch(&mut self) -> Result<Flow, Halt> {
        let new = self.intent.new_slot;
        if matches!(self.role_of(new), Some(r) if r.holder) {
            return self.complete_switch();
        }
        self.restore_old("the proxy restarted before the swap finished")
    }

    /// A frontend update that stopped at `switched` or later. The holder decides.
    pub(super) fn recover_post_switch(&mut self) -> Result<Flow, Halt> {
        let new = self.intent.new_slot;
        match self.role_of(new) {
            Some(r) if r.holder => self.complete_switch(),
            // Alive but not the holder, before the confirm: the store says the old child
            // (or nobody) serves, so the swap did not happen.
            Some(_) if self.intent.phase <= Phase::Baking => {
                self.restore_old("the new child does not hold the generation")
            }
            // Dead new child: the bake sees it and stays red for the operator's rollback.
            _ => Ok(Flow::Go),
        }
    }

    /// The candidate holds the generation: make its slot the active one and stand the
    /// old child down. Safe to repeat. Nothing is held (the hold died with the proxy).
    fn complete_switch(&mut self) -> Result<Flow, Halt> {
        let (old, new) = (self.intent.old_slot, self.intent.new_slot);
        if self.intent.phase < Phase::Switched {
            self.set_phase(Phase::Switched)?;
        }
        self.release_held(new)?;
        if self.old_alive
            && let Err(e) = self.ports.admin.stand_down(old)
        {
            self.note(format!("stand-down of the old child failed: {e}"));
        }
        Ok(Flow::Go)
    }

    /// The old child is the active one again: kill a candidate that is not the holder,
    /// let a quiesced or stood-down old child resume, and fail the update.
    fn restore_old(&mut self, reason: &str) -> Result<Flow, Halt> {
        let old = self.intent.old_slot;
        if !self.old_alive {
            // Nothing else can serve; leave the candidate, which may be the only child.
            return self.fail(Step::Restart, format!("{reason}; the old child is gone"));
        }
        self.kill_candidate();
        let role = self.role_of(old);
        self.resume_old(role, old);
        self.release_held(old)?;
        self.fail(Step::Restart, reason.to_string())
    }
}
