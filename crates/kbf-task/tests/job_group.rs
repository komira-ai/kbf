//! `Job::Group`: a process left in the Command's own process group is killed when the Command ends
//! (`linger-in-group`; one that left the group is for the subreaper, `tests/job.rs`). Its own test
//! binary: it shares no process with the subreaper's cases.

#![cfg(target_os = "linux")]

use std::time::Duration;

use kbf_task::run::Job;

mod job_support;
use job_support::{Leftover, run_probe, scratch};

#[test]
fn a_process_left_in_the_group_is_killed_when_the_run_returns() {
    let dir = scratch("group");
    let ran = run_probe(
        &dir,
        "linger-in-group",
        Job::Group,
        Duration::from_secs(600),
    );
    let _left = Leftover(&dir.join("pid"));
    assert_eq!(ran.exit_code, 0);
    // SIGKILL is delivered asynchronously: wait for the lock, which the process holds until it
    // is gone (this waits for as long as it runs, and a failing case is one that never ends).
    let file = std::fs::File::open(dir.join("lock")).expect("lock");
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive).expect("flock");
}

/// The same when the run ends at its deadline: the Command (`hold`, which waits for ever) is
/// killed with its group and the run says so. With a deadline of zero it may be killed before it
/// took its lock, which leaves nothing to wait for.
#[test]
fn a_command_still_running_at_the_deadline_is_killed_with_its_group() {
    let dir = scratch("group-deadline");
    let ran = run_probe(&dir, "hold", Job::Group, Duration::ZERO);
    let _left = Leftover(&dir.join("pid"));
    assert!(ran.timed_out);
    if let Ok(file) = std::fs::File::open(dir.join("lock")) {
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive).expect("flock");
    }
}
