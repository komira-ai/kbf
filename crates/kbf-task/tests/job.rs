//! A job's Command is started directly (`Run::job`), and the task ends and reaps what the Command
//! left (`src/run.rs`, `Job`).
//!
//! Each case runs the `socket_probe` helper as a real Command and takes the lock its `linger`
//! mode's leftover holds: the lock is free once that process is gone. `Job::Subreaper` reaps every
//! child of its process, so the cases that use it are one test (the tests of a file share a
//! process), and `Job::Group` is in `job_group.rs`.

#![cfg(target_os = "linux")]

use std::time::Duration;

use kbf_task::network::Layers;
use kbf_task::run::Job;

mod job_support;
use job_support::{Leftover, run_probe, scratch};

/// Whether the lock is free now.
fn lock_is_free(dir: &kbf_testkit::dir::TestDir) -> bool {
    let file = std::fs::File::open(dir.join("lock")).expect("lock");
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok()
}

/// `linger` leaves a process in a group of its own: only the subreaper reaches it, as it is handed
/// to this process when its parent ends. It is gone and reaped when the run returns.
#[test]
fn a_process_that_left_the_group_is_gone_when_the_run_returns() {
    let dir = scratch("ends");
    let ran = run_probe(&dir, "linger", Job::Subreaper, Duration::from_secs(600));
    let _left = Leftover(&dir.join("pid"));
    assert_eq!(ran.layers, Layers::Job);
    assert_eq!(ran.exit_code, 0);
    assert!(lock_is_free(&dir), "a process of the Command is running");
}
