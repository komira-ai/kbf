//! Adoption after a proxy restart (FP section 6). The recorded intent says where the
//! update was; the store says who serves. Where they disagree the store wins: both
//! children are asked their role, the generation holder is active, and a candidate that
//! does not hold the generation is killed.
//!
//! Children outlive the proxy (`KillMode=process`), so they are adopted, not restarted.
//! A recorded process that is gone or whose pid now belongs to another process is not
//! ours: nothing signals it.

use super::model::{Failure, Halt, Intent, Phase, Plan, Slot, Start, Step};
use super::ports::Ports;
use super::swap::{Flow, Swap};
use crate::CommitId;

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
        if !self.old_alive && !self.new_alive {
            // Nobody serves: the recorded-active release (the old one) starts again.
            let (old, from) = (self.intent.old_slot, self.plan.from.clone());
            return match self.cold_start(old, &from)? {
                Ok(()) => self.fail(
                    Step::Restart,
                    "the proxy restarted before the swap finished and both children were gone; \
                     the old release was started again"
                        .into(),
                ),
                Err(e) => self.fail(Step::Restart, format!("both children were gone; {e}")),
            };
        }
        self.restore_old("the proxy restarted before the swap finished")
    }

    /// A frontend update that stopped at `switched` or later. The holder decides, and the
    /// recorded-active child is the new one: if it holds the generation the switch landed.
    /// If it is dead or does not hold it while the old child lives, the switch failed, and
    /// the update is reconciled by rolling back before anything is retired
    /// ([`Swap::failed_switch`]). If both children are dead, nobody serves and the new
    /// release starts again ([`Swap::cold_start`]). Once the root's confirm landed
    /// ([`Swap::confirmed`]) there is no rollback: a dead new child is started again
    /// beside a draining old one, and a live non-holder new child takes over.
    pub(super) fn recover_post_switch(&mut self) -> Result<Flow, Halt> {
        let new = self.intent.new_slot;
        // A rollback was recorded and its children's half has not finished (it sets
        // `retire` to the new slot when it hands over to the drain).
        if self.intent.rolling_back && self.intent.retire != Some(new) {
            return self.recover_rollback();
        }
        let role = self.role_of(new);
        if matches!(role, Some(r) if r.holder) {
            return self.complete_switch();
        }
        // The rollback landed and its drain was running: the drain goes on.
        if self.intent.rolling_back {
            return Ok(Flow::Go);
        }
        // A new child that was never recorded is not a recorded-active child that died.
        if self.intent.new_proc.is_none() {
            return Ok(Flow::Go);
        }
        let confirmed = self.confirmed();
        match (self.old_alive, role) {
            // Before the confirm the store's answer is a failed switch: roll back.
            (true, Some(_)) if !confirmed => {
                self.failed_switch("the new child does not hold the generation".into())
            }
            (true, None) if !self.new_alive && !confirmed => {
                self.failed_switch("the new child is gone while the old child lives".into())
            }
            // The new child is dead and there is nothing to roll back to, or the confirm
            // landed and `current` names the new release: start it again. A live old
            // child stands down and drains.
            (_, None) if !self.new_alive => {
                let (new, to) = (self.intent.new_slot, self.plan.to.clone());
                match self.cold_start(new, &to)? {
                    Ok(()) => {
                        self.note(
                            "the new child was gone: the new release was started again".into(),
                        );
                        self.stand_down_old();
                        Ok(Flow::Go)
                    }
                    Err(e) => self.fail(Step::Restart, format!("the new child was gone; {e}")),
                }
            }
            // The new child is alive and does not hold the generation, and either the old
            // child is gone (a cold start the proxy began before it stopped goes on) or the
            // confirm landed: the new child takes over. The old child stands down first.
            (_, Some(_)) => {
                self.stand_down_old();
                match self.cold_takeover(new)? {
                    Ok(()) => Ok(Flow::Go),
                    Err(e) => self.fail(Step::Restart, e),
                }
            }
            // Alive and silent: nothing proves it lost the generation. The bake sees it.
            (_, None) => Ok(Flow::Go),
        }
    }

    /// The root's `confirm` landed: its `current` names the new release, as
    /// [`Swap::read_root`] found at the restart. After it the store's answer is final: a
    /// restart never rolls back, it starts the recorded-active release again if it died.
    fn confirmed(&self) -> bool {
        self.root_confirmed
    }

    /// Ask the root whether the confirm landed, for an update that stopped at `baking` or
    /// later (before that no confirm was sent). The store wins over `update.json`, which is
    /// written after the confirm and so can lag it (a crash between them leaves `baking`
    /// or `nodes` unrecorded) or, if it disagrees the other way, is simply wrong. The
    /// disagreement is noted. A `confirm` failure recorded earlier is cleared when the root
    /// holds the release: a later try landed. If the root cannot be read nothing is guessed:
    /// the update ends `failed` at `restart` with both children as they are.
    pub(super) fn read_root(&mut self) -> Result<Flow, Halt> {
        if self.intent.phase < Phase::Baking || self.intent.rolling_back {
            return Ok(Flow::Go);
        }
        let current = match self.ports.orders.installed() {
            Ok(c) => c,
            Err(e) => {
                return self.fail(
                    Step::Restart,
                    format!(
                        "the proxy restarted and the root half's status could not be read ({e}); \
                         whether the confirm landed is unknown, so both children were left as they are"
                    ),
                );
            }
        };
        self.root_confirmed = current.as_ref() == Some(&self.plan.to);
        let recorded = self.intent.phase >= Phase::Nodes && !self.confirm_failed();
        if recorded != self.root_confirmed {
            self.note(format!(
                "update.json says the confirm {} but the root's current is {}: the root wins",
                if recorded { "landed" } else { "did not land" },
                current.as_ref().map_or("unset", |c| c.as_str()),
            ));
        }
        if self.root_confirmed && self.confirm_failed() {
            self.intent.failure = None;
            self.save()?;
        }
        Ok(Flow::Go)
    }

    /// Tell a live old child to stand down; a refusal changes nothing, the fence holds.
    fn stand_down_old(&mut self) {
        let old = self.intent.old_slot;
        if self.old_alive
            && let Err(e) = self.ports.admin.stand_down(old)
        {
            self.note(format!("stand-down of the old child failed: {e}"));
        }
    }

    /// The switch failed and the proxy stopped before it saw it: the old child is alive and
    /// the new one is dead or not the holder. Record the rollback and the failure first (a
    /// crash from here on resumes the rollback), then run it. The update ends `failed`
    /// with `failure.step == switch`, the name FP section 5 step 6 gives a switch that did
    /// not hold, not `rolled_back`, which is the operator's answer. A dead new child
    /// needs no signal; a live one is retired by SIGTERM once it is idle, never SIGKILL.
    fn failed_switch(&mut self, reason: String) -> Result<Flow, Halt> {
        self.note(format!("{reason}: rolling back"));
        self.intent.rolling_back = true;
        self.intent.retire = None;
        self.intent.failure = Some(Failure {
            step: Step::Switch,
            reason: format!("the switch did not hold after the proxy restarted: {reason}"),
        });
        self.save()?;
        self.recover_rollback()
    }

    /// Start `release` in `slot` where no child serves, wait until it is ready, make it
    /// take the generation (with no live holder the child bumps the epoch: FP section 8)
    /// and route to it. The process is recorded as soon as it exists. `Err` is the reason
    /// it did not come up.
    fn cold_start(&mut self, slot: Slot, release: &CommitId) -> Result<Result<(), String>, Halt> {
        let t = self.plan.timing;
        let ident = match self.ports.child.spawn(slot, release, Start::Candidate) {
            Ok(i) => i,
            Err(e) => return Ok(Err(format!("spawn: {e}"))),
        };
        if slot == self.intent.old_slot {
            self.intent.old_proc = Some(ident.clone());
            self.old_alive = true;
        } else {
            self.intent.new_proc = Some(ident.clone());
            self.new_alive = true;
        }
        self.save()?;
        let first = self.now();
        loop {
            if let Some(code) = self.ports.child.exited(&ident) {
                self.mark_gone(slot);
                return Ok(Err(format!("the new process exited with {code}")));
            }
            if matches!(self.ports.admin.ready(slot), Ok(r) if r.all()) {
                break;
            }
            if self.now().saturating_sub(first) >= t.ready_within {
                return Ok(Err(format!("not ready in {} s", t.ready_within.as_secs())));
            }
            self.sleep(t.poll)?;
        }
        self.cold_takeover(slot)
    }

    /// Make `slot`, the only live child, take the generation and serve.
    fn cold_takeover(&mut self, slot: Slot) -> Result<Result<(), String>, Halt> {
        let t = self.plan.timing;
        if let Err(e) = self.ports.admin.takeover(slot, t.takeover) {
            return Ok(Err(format!("takeover: {e}")));
        }
        self.release_held(slot)?;
        Ok(Ok(()))
    }

    /// The proxy stopped inside a rollback (the intent says so; no phase does). The store
    /// decides how far it got. If the new child still holds the generation the rollback
    /// never landed: the new child is put back and the update fails at `rollback`.
    /// Otherwise the old child has the generation or is about to: the rollback is run
    /// again (every call in it is idempotent) and the new child is retired, never killed.
    pub(super) fn recover_rollback(&mut self) -> Result<Flow, Halt> {
        let t = self.plan.timing;
        let new = self.intent.new_slot;
        if matches!(self.role_of(new), Some(r) if r.holder) {
            return self.rollback_failed("the proxy restarted before the rollback landed".into());
        }
        if !self.old_alive {
            self.intent.rolling_back = false;
            return self.fail(
                Step::Restart,
                "the proxy restarted during a rollback; the old child is gone".into(),
            );
        }
        if let Err(e) = self.ports.gate.hold(t.hold_max) {
            self.note(format!("hold for the rollback failed: {e}"));
        }
        self.rollback_swap()
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

    /// Before the switch, the old child is the active one again: kill a candidate that is
    /// not the holder, let a quiesced old child resume, and fail the update. Never called
    /// once the swap switched: that child served.
    fn restore_old(&mut self, reason: &str) -> Result<Flow, Halt> {
        debug_assert!(self.intent.phase < Phase::Switched);
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
