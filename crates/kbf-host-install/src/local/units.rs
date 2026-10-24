//! Restarting the units a release runs under: `systemctl restart`, then a look every
//! interval until the unit says it is active or the wait is over.

use std::thread;
use std::time::Instant;

use crate::plan::{self, Wait};
use crate::request::Unit;
use crate::runner::Runner;

/// Restart `unit` and wait until it is active.
///
/// # Errors
/// Why not, in a line for the log: the restart failed, or the unit was not active within
/// the wait.
pub fn restart_and_wait(runner: &dyn Runner, unit: &Unit, wait: Wait) -> Result<(), String> {
    let restart = plan::restart_unit(unit);
    match runner.run(&restart) {
        Ok(exec) if exec.success() => {}
        Ok(exec) => {
            return Err(format!(
                "systemctl restart {}: exit status {:?}: {}",
                unit.as_str(),
                exec.code,
                exec.stderr_text()
            ));
        }
        Err(e) => return Err(format!("systemctl restart {}: {e}", unit.as_str())),
    }
    let deadline = Instant::now() + wait.timeout;
    loop {
        let active = runner
            .run(&plan::unit_active(unit))
            .is_ok_and(|exec| exec.success());
        if active {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{} was not active {} seconds after the restart",
                unit.as_str(),
                wait.timeout.as_secs()
            ));
        }
        thread::sleep(wait.interval);
    }
}
