//! A job's Command runs while the task is its subreaper, and what its processes leave behind is
//! reaped as it ends, not at the end of the run (`src/orphans.rs`). Its own test binary: the
//! subreaper reaps every child of its process.

#![cfg(target_os = "linux")]

use std::time::Duration;

use kbf_task::network::Layers;
use kbf_task::run::Job;

mod job_support;
use job_support::{Leftover, run_probe, scratch};

/// The Command (`orphan`) starts a process whose parent ends at once, lets it end, and itself
/// exits with 42 only once `/proc` has no entry for it, which is while the Command runs: nothing
/// else could have reaped it. The Command's own status is the one reported, and the lock and
/// pidfile arguments are the helper's, here unused.
#[test]
fn an_orphan_that_ends_while_the_command_runs_is_reaped_and_the_exit_code_is_the_commands() {
    let dir = scratch("reap-orphans");
    let ran = run_probe(&dir, "orphan", Job::Subreaper, Duration::from_secs(600));
    let _left = Leftover(&dir.join("pid"));
    assert_eq!(ran.layers, Layers::Job);
    assert!(!ran.timed_out);
    assert_eq!(ran.exit_code, 42);
}
