//! What the turn of a frontend that is a member of a pool (an inventory line with `instance=`
//! and `base=`) shares with the rest of the roll: the order of its phases, the install that
//! covers every member of a host, and the check of the commit a member runs. The turn itself is
//! in `updater_no_proxy.rs`. A child of the roll state machine in `updater.rs`.

use super::{Halt, InstallError, Updater};
use crate::api::{ErrorCode, Phase};
use crate::health::names;
use crate::inventory::Host;
use crate::ports::{Health, Installed, PortError};
use crate::version::Version;

/// The phases of a member's turn, in the order it goes through them.
const TURN: [Phase; 5] = [
    Phase::Pending,
    Phase::Draining,
    Phase::Installing,
    Phase::Restarting,
    Phase::WaitingHealthy,
];

/// Whether a turn found in `found` still has the work of `phase` to do: it has not got past
/// it. A phase that is no part of the turn (`checking`, which a member's record never holds)
/// is taken for the start of it.
pub(super) fn pending(found: Phase, phase: Phase) -> bool {
    let place = |p: Phase| TURN.iter().position(|q| *q == p);
    place(found) <= place(phase)
}

/// `result`'s error as a halt with `code`: what failed, then the port's own words.
pub(super) fn or_fail<T>(
    result: Result<T, PortError>,
    code: ErrorCode,
    why: &str,
) -> Result<T, Halt> {
    result.map_err(|e| Halt::Fail(code, format!("{why}: {e}")))
}

impl Updater {
    /// Install the target on the host unless it has it. The package covers every member of the
    /// host, so a member after the first, and a member found here after a restart of the updater
    /// whose install had finished, install nothing. An install the host reports as in progress
    /// (one the updater started before it restarted) is waited out, for at most
    /// `Timing::install_bound`, and not run a second time beside it.
    pub(super) fn install_once(
        &self,
        idx: usize,
        host: &Host,
        target: &Version,
    ) -> Result<(), Halt> {
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

    /// Whether the member `name` of `host` reports a build of the commit that begins with
    /// `commit`, which is asked of the member itself.
    pub(super) fn runs_commit(
        &self,
        host: &Host,
        name: &str,
        commit: &str,
    ) -> Result<Health, PortError> {
        let built = self.ports.frontend.commit(host)?;
        if names(&built, commit) {
            return Ok(Health::Healthy);
        }
        Ok(Health::Waiting(format!(
            "member {name} runs build {built}, not the target's"
        )))
    }
}
