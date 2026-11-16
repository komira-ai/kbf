//! What `tests/job.rs` and `tests/job_group.rs` share: run `socket_probe` as a job's Command, and
//! look at the lock its leftover holds.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::time::Duration;

use kbf_proto::reapi::Command;
use kbf_task::network::Network;
use kbf_task::run::{Job, Ran, Run, run};
use kbf_testkit::dir::TestDir;

/// Ends the process the pidfile names when dropped, so a case that fails leaves nothing running.
pub struct Leftover<'a>(pub &'a Path);

impl Drop for Leftover<'_> {
    fn drop(&mut self) {
        let pid = std::fs::read_to_string(self.0)
            .ok()
            .and_then(|pid| pid.trim().parse::<i32>().ok())
            .and_then(rustix::process::Pid::from_raw);
        if let Some(pid) = pid {
            // Already gone is fine.
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }
}

/// Runs `socket_probe <mode> <lock> <pidfile>` as a job's Command under `job`, with `deadline`.
pub fn run_probe(dir: &TestDir, mode: &str, job: Job, deadline: Duration) -> Ran {
    let (lock, pidfile) = (dir.join("lock"), dir.join("pid"));
    let command = Command {
        arguments: [
            env!("CARGO_BIN_EXE_socket_probe"),
            mode,
            lock.to_str().expect("utf-8"),
            pidfile.to_str().expect("utf-8"),
        ]
        .map(str::to_owned)
        .to_vec(),
        ..Command::default()
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime
        .block_on(run(
            &command,
            &Run {
                root: &dir.join("root"),
                private: &dir.join("private"),
                stdout: &dir.join("stdout"),
                stderr: &dir.join("stderr"),
                deadline,
                network: Network::Denied,
                unreadable: &[],
                hidden: &[],
                relay: None,
                job: Some(job),
            },
        ))
        .expect("ran")
}

pub fn scratch(name: &str) -> TestDir {
    let dir = TestDir::under(std::env::temp_dir(), &format!("kbf-task-job-{name}"));
    std::fs::create_dir_all(dir.join("root")).expect("root");
    dir
}
