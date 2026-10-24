//! The turn of a frontend that is a member of the kbf-proxy pool (an inventory line with
//! `instance=` and `base=`), a child of the roll state machine in `updater.rs`.
//!
//! The member is rolled alone, and never when taking it out would leave the proxy nothing to
//! send to: some other member must be up and ready. A member that is itself drained or not
//! ready, which takes nothing out of service, is rolled while one other member serves. Its
//! turn, in the order of its phases:
//!
//! 1. `draining`: the proxy is told to give the member no new stream (`drain`), and the roll
//!    waits until the REAPI and operator streams open on it are 0, for at most
//!    [`STREAMS_BOUND`];
//! 2. `retiring`: the member retires (`POST /v1/frontend/retire`: it hands its work to the
//!    other frontends), and the roll waits until the member reports no stream open, for at most
//!    [`STREAMS_BOUND`];
//! 3. `installing`: the package is installed on the host, unless the host already has the
//!    target, which is the case for every member of the host after the first;
//! 4. `restarting`: the host restarts the member's unit;
//! 5. `waiting_healthy`: the roll waits until the proxy lists the member ready and the member
//!    reports the commit of the target, for at most `Timing::healthy_frontend`;
//! 6. `undraining`: the proxy is told to give the member streams again.
//!
//! Every failure after the drain ends the roll with the member left drained and its name in the
//! detail; the roll never undrains a member it could not prove healthy. A drain the proxy
//! refuses leaves nothing to say that about, and the detail does not. A member whose turn was
//! found half done after a restart of the updater is drained again, whatever the proxy says,
//! and the turn goes on from the phase the record holds.

use super::{Halt, InstallError, STREAMS_BOUND, Updater, noted};
use crate::api::{ErrorCode, Phase};
use crate::health::{commit_of, names};
use crate::inventory::Host;
use crate::ports::{Health, Installed, PortError};
use crate::version::Version;

/// The phases of a member's turn, in the order it goes through them.
const TURN: [Phase; 7] = [
    Phase::Pending,
    Phase::Draining,
    Phase::Retiring,
    Phase::Installing,
    Phase::Restarting,
    Phase::WaitingHealthy,
    Phase::Undraining,
];

/// Whether a turn found in `found` still has the work of `phase` to do: it has not got past
/// it. A phase that is no part of the turn (`checking`, which a member's record never holds)
/// is taken for the start of it.
fn pending(found: Phase, phase: Phase) -> bool {
    let place = |p: Phase| TURN.iter().position(|q| *q == p);
    place(found) <= place(phase)
}

/// `result`'s error as a halt with `code`: what failed, then the port's own words.
fn or_fail<T>(result: Result<T, PortError>, code: ErrorCode, why: &str) -> Result<T, Halt> {
    result.map_err(|e| Halt::Fail(code, format!("{why}: {e}")))
}

/// The halt for a member the proxy does not list.
fn unknown(name: &str) -> Halt {
    Halt::Fail(
        ErrorCode::UnknownMember,
        format!("the proxy has no member {name}"),
    )
}

impl Updater {
    /// The turn of the member `name`, listed as `host`, from the phase `found` the record holds.
    /// `Ok` when the member runs the target and is back in the pool.
    pub(super) fn frontend_turn(
        &self,
        idx: usize,
        host: &Host,
        name: &str,
        target: &Version,
        found: Phase,
    ) -> Result<(), Halt> {
        // Only a turn that has not begun is refused for leaving the proxy nothing to send to: one
        // found under way has drained the member already.
        if found == Phase::Pending {
            self.another_serves(name)?;
        }
        if pending(found, Phase::Draining) {
            self.mark(idx, Phase::Draining, false)?;
        }
        // Whatever phase the turn is found in, the member is drained (again): a drain does
        // nothing to a member that is drained. A refused drain is no member left drained, so
        // only a failure after it says so.
        self.pool_drain(name)?;
        let note = format!("frontend member {name} ({}) is left drained", host.alias);
        self.frontend_steps(idx, host, name, target, found)
            .map_err(|halt| noted(halt, &note))
    }

    /// The steps of the turn that have not been done, each preceded by its phase in the record,
    /// for a member the proxy has drained.
    fn frontend_steps(
        &self,
        idx: usize,
        host: &Host,
        name: &str,
        target: &Version,
        found: Phase,
    ) -> Result<(), Halt> {
        if pending(found, Phase::Draining) {
            self.wait_streams(name)?;
        }
        if pending(found, Phase::Retiring) {
            self.mark(idx, Phase::Retiring, false)?;
            self.retire(host)?;
        }
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
        if pending(found, Phase::WaitingHealthy) {
            self.mark(idx, Phase::WaitingHealthy, false)?;
            let bound = self.timing.healthy_frontend;
            self.wait_until(bound, || self.member_health(host, name, target))?;
        }
        self.mark(idx, Phase::Undraining, false)?;
        or_fail(
            self.ports.pool.undrain(name),
            ErrorCode::UndrainFailed,
            "the proxy would not put the member back",
        )
    }

    /// Fail unless some other member is up and ready, so that taking `name` out leaves the
    /// proxy something to send to. Whether `name` itself serves does not matter: a member that
    /// is drained or not ready is taken out of nothing, so a roll that an earlier failure
    /// stopped at it can be run again while one other member serves. Nothing has been changed
    /// when this fails.
    fn another_serves(&self, name: &str) -> Result<(), Halt> {
        let view = or_fail(
            self.ports.pool.view(),
            ErrorCode::SafeToStopUnavailable,
            "the proxy's pool could not be read",
        )?;
        if !view.iter().any(|m| m.name == name) {
            return Err(unknown(name));
        }
        if view.iter().filter(|m| m.name != name).any(|m| m.serves()) {
            return Ok(());
        }
        Err(Halt::Fail(
            ErrorCode::UnsafeToStop,
            format!("member {name} is the last one that is up and ready"),
        ))
    }

    fn pool_drain(&self, name: &str) -> Result<(), Halt> {
        or_fail(
            self.ports.pool.drain(name),
            ErrorCode::DrainFailed,
            "the proxy would not drain the member",
        )
    }

    /// Wait until the proxy shows no REAPI or operator stream open on the drained member `name`,
    /// for at most [`STREAMS_BOUND`].
    fn wait_streams(&self, name: &str) -> Result<(), Halt> {
        let start = self.now();
        loop {
            let view = or_fail(
                self.ports.pool.view(),
                ErrorCode::DrainFailed,
                "the proxy's pool could not be read",
            )?;
            let Some(member) = view.iter().find(|m| m.name == name) else {
                return Err(unknown(name));
            };
            let open = member.streams();
            if open == 0 {
                return Ok(());
            }
            if self.since(start) >= STREAMS_BOUND {
                let secs = STREAMS_BOUND.as_secs();
                return Err(Halt::Fail(
                    ErrorCode::DrainFailed,
                    format!("{open} streams were still open on member {name} after {secs} s"),
                ));
            }
            self.ports.clock.sleep(self.timing.poll);
        }
    }

    /// Retire the member and wait until it reports no stream open, for at most
    /// [`STREAMS_BOUND`]. It must say it is retired by then.
    fn retire(&self, host: &Host) -> Result<(), Halt> {
        let mut status = or_fail(
            self.ports.frontend.retire(host),
            ErrorCode::RetireFailed,
            "the member would not retire",
        )?;
        let start = self.now();
        while status.open_streams > 0 {
            if self.since(start) >= STREAMS_BOUND {
                let (open, secs) = (status.open_streams, STREAMS_BOUND.as_secs());
                return Err(Halt::Fail(
                    ErrorCode::RetireFailed,
                    format!("{open} streams were still open after the retire, after {secs} s"),
                ));
            }
            self.ports.clock.sleep(self.timing.poll);
            status = or_fail(
                self.ports.frontend.retire_status(host),
                ErrorCode::RetireFailed,
                "the member's retire could not be read",
            )?;
        }
        if status.retired {
            return Ok(());
        }
        let alias = &host.alias;
        Err(Halt::Fail(
            ErrorCode::RetireFailed,
            format!("{alias} answered the retire but is not retired"),
        ))
    }

    /// Install the target on the host unless it has it. The package covers every member of the
    /// host, so a member after the first, and a member found here after a restart of the updater
    /// whose install had finished, install nothing. An install the host reports as in progress
    /// (one the updater started before it restarted) is waited out, for at most
    /// `Timing::install_bound`, and not run a second time beside it.
    fn install_once(&self, idx: usize, host: &Host, target: &Version) -> Result<(), Halt> {
        let alias = &host.alias;
        let start = self.now();
        loop {
            let asked = self.ports.installer.query(host);
            let installed = or_fail(
                asked,
                ErrorCode::InstallFailed,
                "the host could not be asked what it has installed",
            )?;
            if installed == Installed::Version(target.as_str().to_owned()) {
                return Ok(());
            }
            if installed != Installed::InProgress {
                break;
            }
            if self.since(start) >= self.timing.install_bound {
                let secs = self.timing.install_bound.as_secs();
                return Err(Halt::Fail(
                    ErrorCode::InstallFailed,
                    format!("an install on {alias} was still in progress after {secs} s"),
                ));
            }
            self.ports.clock.sleep(self.timing.poll);
        }
        self.mark(idx, Phase::Installing, true)?;
        match self.install(host, target) {
            Ok(_) => Ok(()),
            Err(InstallError::Halt(halt)) => Err(halt),
            Err(InstallError::Unreachable) => Err(Halt::Fail(
                ErrorCode::InstallFailed,
                format!("{alias} did not answer the install, which may have run"),
            )),
        }
    }

    /// Whether the member `name` of `host` is ready at the proxy and built from the target's
    /// commit. A version that names no commit fails the wait; a member the proxy does not list
    /// is not waited for.
    fn member_health(
        &self,
        host: &Host,
        name: &str,
        target: &Version,
    ) -> Result<Health, PortError> {
        let commit = match commit_of(target) {
            Ok(commit) => commit,
            Err(health) => return Ok(health),
        };
        let view = self.ports.pool.view()?;
        let Some(member) = view.iter().find(|m| m.name == name) else {
            let why = format!("the proxy has no member {name}");
            return Ok(Health::Abandoned(why));
        };
        if !member.ready {
            return Ok(Health::Waiting(format!("member {name} is not ready")));
        }
        let built = self.ports.frontend.commit(host)?;
        if names(&built, commit) {
            return Ok(Health::Healthy);
        }
        Ok(Health::Waiting(format!(
            "member {name} runs build {built}, not the target's"
        )))
    }
}
