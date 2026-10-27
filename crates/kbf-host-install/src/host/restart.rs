//! `restart frontend=<name>`: restart one frontend unit and wait until it is active.
//!
//! The process sshd starts does the work itself, holding the install lock from before the
//! restart until it has an answer, so while it runs a restart and an install do not overlap
//! (an install that finds the lock held is `busy`, and so is a restart that finds an
//! install's). A restart is nothing `dpkg` could be left half way by, so it needs no unit of
//! its own. Nothing here notices a dropped ssh connection: the process goes on with the
//! restart and the wait, holds the lock until it ends (so a retry is `busy` until then), and
//! its answer is lost. If something outside ends the process, the lock goes with it and
//! systemd carries on with the restart. Either way the client asks again, and the next
//! `query` or restart finds the unit as systemd left it.

use super::{Host, fail, step, take_install_lock};
use crate::outcome::{FailureCode, Outcome};
use crate::pace::Pace;
use crate::plan;
use crate::request::Unit;
use crate::runner::Runner;

/// `restart`: not started when the audit log cannot be opened (no restart goes unrecorded),
/// nor while the install lock is held.
pub(super) fn run(host: &Host, runner: &dyn Runner, unit: &Unit, audit_ok: bool) -> Outcome {
    restart(host, runner, unit, audit_ok).unwrap_or_else(|outcome| outcome)
}

fn restart(
    host: &Host,
    runner: &dyn Runner,
    unit: &Unit,
    audit_ok: bool,
) -> Result<Outcome, Outcome> {
    if !audit_ok {
        return Err(fail(
            FailureCode::AuditUnavailable,
            "the audit log cannot be opened; nothing was run",
        ));
    }
    let _lock = take_install_lock(host)?;
    let command = plan::restart_unit(unit);
    step(runner, &command, FailureCode::RestartFailed)?;
    wait_active(runner, unit, host.restart_wait, &host.pace)
}

/// Ask whether `unit` is active until it says so, or `wait` is over. A look that cannot be
/// made or that does not say `active` is a unit that is not active yet.
fn wait_active(
    runner: &dyn Runner,
    unit: &Unit,
    wait: plan::Wait,
    pace: &Pace,
) -> Result<Outcome, Outcome> {
    let deadline = (pace.now)() + wait.timeout;
    loop {
        let active = runner
            .run(&plan::unit_active(unit))
            .is_ok_and(|exec| exec.success());
        if active {
            return Ok(Outcome::Restarted {
                unit: unit.as_str().to_owned(),
            });
        }
        if (pace.now)() >= deadline {
            return Err(fail(
                FailureCode::RestartTimeout,
                format!(
                    "{} was not active {} seconds after the restart",
                    unit.as_str(),
                    wait.timeout.as_secs()
                ),
            ));
        }
        (pace.sleep)(wait.interval);
    }
}
