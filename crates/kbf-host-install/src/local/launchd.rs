//! Restarting a Mac daemon's launchd job on the installer layout: `launchctl kickstart -k`,
//! then a look every interval until the job's process runs the release's own binary or the
//! wait is over.
//!
//! The job names `releases/current/kbf`, so a restart after the links moved starts the
//! release they name. That the process runs it is read from the process, not from the
//! link: `launchctl print` gives its pid, and `lsof` names the file the kernel mapped as its
//! program, links resolved. A process that runs another release, or none, is not up yet.

use std::path::Path;

use crate::mac::tools;
use crate::pace::Pace;
use crate::plan::Wait;
use crate::runner::Runner;

/// Kickstart the job `label` and wait until its process runs `program`.
///
/// # Errors
/// Why not, in a line for the log: the kickstart failed, or the job did not run `program`
/// within the wait.
pub fn restart_and_wait(
    runner: &dyn Runner,
    label: &str,
    program: &Path,
    wait: Wait,
    pace: &Pace,
) -> Result<(), String> {
    let limit = wait.timeout;
    match runner.run(&tools::kickstart(label, limit)) {
        Ok(exec) if exec.success() => {}
        Ok(exec) => {
            return Err(format!(
                "launchctl kickstart system/{label}: exit status {:?}: {}",
                exec.code,
                exec.stderr_text()
            ));
        }
        Err(e) => return Err(format!("launchctl kickstart system/{label}: {e}")),
    }
    let deadline = (pace.now)() + wait.timeout;
    loop {
        if runs(runner, label, program, limit) {
            return Ok(());
        }
        if (pace.now)() >= deadline {
            return Err(format!(
                "system/{label} did not run {} {} seconds after the restart",
                program.display(),
                wait.timeout.as_secs()
            ));
        }
        (pace.sleep)(wait.interval);
    }
}

/// Whether the process of the job `label` has `program` mapped as its program. (`lsof`
/// exits 1 when it only warns, with its listing intact, so its output is read whatever its
/// status.)
fn runs(runner: &dyn Runner, label: &str, program: &Path, limit: std::time::Duration) -> bool {
    let Some(pid) = runner
        .run(&tools::print(label, limit))
        .ok()
        .filter(crate::runner::Exec::success)
        .and_then(|print| tools::print_pid(&print.stdout_text()))
    else {
        return false;
    };
    runner.run(&tools::lsof_text(pid, limit)).is_ok_and(|open| {
        tools::lsof_paths(&open.stdout_text())
            .into_iter()
            .any(|path| Path::new(path) == program)
    })
}
