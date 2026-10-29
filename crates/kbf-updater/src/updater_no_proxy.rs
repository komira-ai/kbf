//! The turn of a frontend that is a member of a pool (an inventory line with `instance=` and
//! `base=`). A child of the roll state machine in `updater.rs`. The frontends sit behind
//! some load balancer that the updater knows nothing about, which stops sending to a member
//! whose `/readyz` answers 503, and a member drains itself (`POST /v1/frontend/drain`) in a way
//! that does not depend on which balancer it is: `/readyz` 503, no new work, a wait for the
//! executions open on it, and its exit.
//!
//! The member is rolled alone, and never when another member could not take its place. Its
//! turn, in the order of its phases:
//!
//! 1. `pending`: every other member of the inventory must answer `/readyz` 200 each time it is
//!    asked, for `Timing::peer_ready` in a row (a miss starts the window again), within that and
//!    `Timing::healthy_frontend` more; else the roll fails with `unsafe_to_stop` and nothing has
//!    been changed. A member that is alone in the inventory is refused the same way;
//! 2. `draining`: the member is told to drain, and the roll waits until its operator API
//!    refuses the connection (it has exited) or reports the drain phase `closing`, however long
//!    that takes unless `Timing::frontend_drain` is set. A frontend that reports no `closing`
//!    phase is waited for until it exits. A member that answers but is not draining fails the
//!    roll at once. The frontend in this tree puts no bound on a drain that the endpoint began
//!    (`--shutdown-timeout-secs` bounds only one that a signal began), so with a
//!    `Timing::frontend_drain` set, an execution that runs past it fails the roll with the
//!    member left draining;
//! 3. `installing`: the package is installed on the host, unless the host already has the
//!    target, which is the case for every member of the host after the first;
//! 4. `restarting`: the host restarts the member's unit, which starts the new version;
//! 5. `waiting_healthy`: the roll waits until the member answers `/readyz` 200 and reports the
//!    commit of the target, for at most `Timing::healthy_frontend`.
//!
//! Nothing is undone at the end: a drained frontend has exited, and the restart made a new
//! process. Every failure after the drain ends the roll with the member named as not serving.
//! A member whose turn was found half done after a restart of the updater is told to drain again
//! only in `draining` (a drain does nothing to a member that is draining, and a member that has
//! exited refuses the connection, which is taken for a member with nothing left to drain); in
//! `installing` the host is asked what it has, and `restarting` and
//! `waiting_healthy` go on without a drain, which would take down the new process. The record
//! says `draining` before the drain is sent, so a member found in `draining` that answers that it
//! is not draining is checked against the other members first, as in `pending`.
//!
//! A member whose operator API refuses the connection when it is first told to drain is taken
//! for one that an earlier roll left stopped, and the turn goes on from the install. That is also
//! what a wrong `base` in the inventory looks like: the unit is then restarted though it was
//! never drained, and the wait for readiness fails, since it asks at the same wrong address.

use super::frontend::{or_fail, pending};
use super::{Halt, Updater, noted};
use crate::api::{ErrorCode, Phase};
use crate::health::commit_of;
use crate::inventory::Host;
use crate::ports::{Health, PortError};
use crate::version::Version;

impl Updater {
    /// The turn of the member `name`, listed as `host`, from the phase `found` the record holds.
    /// `Ok` when the member runs the target and is ready.
    pub(super) fn no_proxy_turn(
        &self,
        idx: usize,
        host: &Host,
        name: &str,
        target: &Version,
        found: Phase,
    ) -> Result<(), Halt> {
        // The phases of a turn at a proxy that this one has no place for: an earlier build began
        // the roll with a proxy. It is not guessed at.
        if matches!(found, Phase::Retiring | Phase::Undraining) {
            return Err(Halt::Fail(
                ErrorCode::SwapAbandoned,
                format!(
                    "{} was left by a turn at a proxy ({found:?}), which this build does not \
                     take; abort the roll",
                    host.alias
                ),
            ));
        }
        // Only a turn that has not begun is refused for leaving the others too few: one found
        // under way has taken the member out already. The record says `draining` before the
        // drain is sent, so it proves a drain only if the member says it is draining.
        let begun = match found {
            Phase::Pending => false,
            Phase::Draining => self.is_draining(host),
            _ => true,
        };
        if !begun {
            self.peers_serve(host, name)?;
        }
        if pending(found, Phase::Draining) {
            self.mark(idx, Phase::Draining, false)?;
            self.begin_drain(host, name)?;
            self.wait_exit(host, name)?;
        }
        let note = format!("frontend member {name} ({}) is not serving", host.alias);
        self.no_proxy_steps(idx, host, name, target, found)
            .map_err(|halt| noted(halt, &note))
    }

    /// The steps of the turn after the drain that have not been done, each preceded by its
    /// phase in the record, for a member that has exited.
    fn no_proxy_steps(
        &self,
        idx: usize,
        host: &Host,
        name: &str,
        target: &Version,
        found: Phase,
    ) -> Result<(), Halt> {
        if pending(found, Phase::Installing) {
            self.install_once(idx, host, target)?;
        }
        if pending(found, Phase::Restarting) {
            self.mark(idx, Phase::Restarting, false)?;
            or_fail(
                self.ports.installer.restart_frontend(host, name),
                ErrorCode::RestartFailed,
                "the unit could not be restarted",
            )?;
        }
        self.mark(idx, Phase::WaitingHealthy, false)?;
        let bound = self.timing.healthy_frontend;
        self.wait_until(bound, || self.member_up(host, name, target))
    }

    /// Fail unless every other member of the inventory answers `/readyz` 200 each time it is
    /// asked for `Timing::peer_ready` in a row, so that taking `name` out leaves the others to
    /// carry what it carried. A member that is not ready, or cannot be asked, starts the window
    /// again; the wait for a whole window is given the window and `Timing::healthy_frontend`
    /// more. The member itself is not asked: whether it serves does not matter to taking it out.
    /// Nothing has been changed when this fails.
    fn peers_serve(&self, host: &Host, name: &str) -> Result<(), Halt> {
        let peers = self.inventory.peers(&host.alias);
        if peers.is_empty() {
            return Err(Halt::Fail(
                ErrorCode::UnsafeToStop,
                format!("member {name} has no other member to serve while it is rolled"),
            ));
        }
        let window = self.timing.peer_ready;
        let bound = window + self.timing.healthy_frontend;
        let start = self.now();
        let mut steady = None;
        let mut last = String::new();
        loop {
            let now = self.now();
            match self.first_not_ready(&peers) {
                None => {
                    let from = *steady.get_or_insert(now);
                    if now.saturating_sub(from) >= window {
                        return Ok(());
                    }
                }
                Some(why) => {
                    steady = None;
                    last = why;
                }
            }
            if self.since(start) >= bound {
                let secs = window.as_secs();
                return Err(Halt::Fail(
                    ErrorCode::UnsafeToStop,
                    format!("the other members were not all ready for {secs} s in a row: {last}"),
                ));
            }
            self.ports.clock.sleep(self.timing.poll);
        }
    }

    /// Why the first of `peers` that is not ready is not, if one is not.
    fn first_not_ready(&self, peers: &[&Host]) -> Option<String> {
        for peer in peers {
            match self.ports.frontend.ready(peer) {
                Ok(true) => {}
                Ok(false) => return Some(format!("{} is not ready", peer.alias)),
                Err(e) => return Some(format!("{} could not be asked: {e}", peer.alias)),
            }
        }
        None
    }

    /// Whether the member may be draining: false only if it answers that it is not. A member that
    /// has exited, or cannot be asked, may have been told to drain.
    fn is_draining(&self, host: &Host) -> bool {
        match self.ports.frontend.drain_status(host) {
            Ok(status) => status.draining,
            Err(_) => true,
        }
    }

    /// Tell the member to drain itself. A member whose operator API refuses the connection is not
    /// running, so it has nothing to drain: that is a member an earlier roll left stopped, or
    /// one that exited between the record and this call, and the turn goes on. A drain that is
    /// answered with a failure did not begin; one that is not answered (a timeout, a cut
    /// connection) may have, and the failure says so.
    fn begin_drain(&self, host: &Host, name: &str) -> Result<(), Halt> {
        match self.ports.frontend.drain(host) {
            Ok(()) => Ok(()),
            Err(e) if e.is_refused() => Ok(()),
            Err(e) if e.is_unreachable() => {
                let note = format!("frontend member {name} ({}) may be draining", host.alias);
                Err(Halt::Fail(
                    ErrorCode::DrainFailed,
                    format!("the member did not answer the drain: {e}; {note}"),
                ))
            }
            Err(e) => Err(Halt::Fail(
                ErrorCode::DrainFailed,
                format!("the member would not drain: {e}"),
            )),
        }
    }

    /// Wait until the draining member `name` has exited: its operator API refuses the
    /// connection, or says that it is closing. A member that answers otherwise, or not at all
    /// (a timeout, a 5xx), is waited for, however long that takes unless `Timing::frontend_drain`
    /// is set; the failure at that bound names the member as left draining. A member that answers that it is not draining
    /// fails the wait at once, without that note.
    fn wait_exit(&self, host: &Host, name: &str) -> Result<(), Halt> {
        let start = self.now();
        let bound = self.timing.frontend_drain;
        loop {
            let last = match self.ports.frontend.drain_status(host) {
                Err(e) if e.is_refused() => return Ok(()),
                Ok(status) if status.closing => return Ok(()),
                Ok(status) if !status.draining => {
                    return Err(Halt::Fail(
                        ErrorCode::DrainFailed,
                        format!("member {name} answered the drain status but is not draining"),
                    ));
                }
                Ok(status) => format!("{} executions were still open", status.open_executions),
                Err(e) => e.to_string(),
            };
            if let Some(bound) = bound.filter(|b| self.since(start) >= *b) {
                let secs = bound.as_secs();
                let note = format!("frontend member {name} ({}) is left draining", host.alias);
                return Err(Halt::Fail(
                    ErrorCode::DrainFailed,
                    format!("member {name} was still draining after {secs} s: {last}; {note}"),
                ));
            }
            self.ports.clock.sleep(self.timing.poll);
        }
    }

    /// Whether the member `name` of `host` answers `/readyz` 200 and is built from the target's
    /// commit. A version that names no commit fails the wait.
    fn member_up(&self, host: &Host, name: &str, target: &Version) -> Result<Health, PortError> {
        let commit = match commit_of(target) {
            Ok(commit) => commit,
            Err(health) => return Ok(health),
        };
        if !self.ports.frontend.ready(host)? {
            return Ok(Health::Waiting(format!("member {name} is not ready")));
        }
        self.runs_commit(host, name, commit)
    }
}
