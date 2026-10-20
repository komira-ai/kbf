//! `kbf-keeper-swap`, for `tests/keeper.rs` only: a process that starts a keeper after
//! its own binary was renamed over, as an update's swap does while a daemon runs.
//!
//! `kbf-keeper-swap <replacement> <cgroup.kill>` renames `replacement` over this
//! binary, then starts a `cgroup` keeper of `cgroup.kill` (one try) from
//! [`keeper::program`], as the container driver does, waits for its ready line, closes
//! its stdin, and waits for it. Exits 0 once the keeper has exited 0; 1, with the
//! reason on stderr, otherwise; 2 on a usage error. Run with the keeper arguments, it
//! is the keeper.

use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use kbf_daemon::keeper::{self, Mode};

fn main() -> ExitCode {
    keeper::hook();
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let [replacement, kill] = &args[..] else {
        eprintln!("usage: kbf-keeper-swap <replacement> <cgroup.kill>");
        return ExitCode::from(2);
    };
    match swap_then_keep(replacement, kill) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf-keeper-swap: {e}");
            ExitCode::FAILURE
        }
    }
}

fn swap_then_keep(replacement: &Path, kill: &Path) -> io::Result<()> {
    std::fs::rename(replacement, std::env::current_exe()?)?;
    let mode = Mode::Cgroup {
        kill: kill.to_owned(),
        tries: 1,
    };
    let mut keeper = keeper::command(&keeper::program()?, &mode)
        .process_group(0)
        .spawn()?;
    let started = keeper::started(&mut keeper);
    drop(keeper.stdin.take());
    let status = keeper.wait()?;
    started?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("the keeper exited with {status}")))
    }
}
