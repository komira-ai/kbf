//! Scenario B0 and the sentinel (docs/design/kbf-cicd.md, section 4.3): a run that is killed
//! with no chance to clean up (SIGKILL, which a runner sends a job it cancels) leaves no Buck2
//! daemon, no control socket and no farm process behind.

#![cfg(target_os = "linux")]

#[path = "beta_support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use kbf_harness::b0::{Ended, run};
use kbf_harness::stand_in::{FAKE_BUCK2, sibling_bin};

use support::*;

/// How many times the stand-in `buck2` was told to `kill` the daemon of loop `lane` of the
/// run in `work`.
fn kills(work: &Path, lane: usize) -> usize {
    kill_markers(work, lane)
        .iter()
        .filter_map(|marker| std::fs::read_to_string(marker).ok())
        .map(|text| text.lines().count())
        .sum()
}

/// Calls `found` until it gives something; a bound that ends a hang only.
fn until<T>(what: &str, mut found: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(found) = found() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether process `pid` is gone (a zombie is gone: it runs nothing).
fn gone(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, state)| state.trim_start().starts_with('Z'))
    })
}

/// The control socket the process `pid` (the frontend) was told, from its command line.
fn handover_socket(pid: rustix::process::Pid) -> PathBuf {
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", pid.as_raw_nonzero()))
        .expect("the frontend's command line");
    cmdline
        .split(|b| *b == 0)
        .filter_map(|word| {
            std::str::from_utf8(word)
                .ok()?
                .strip_prefix("--handover-socket=")
        })
        .map(PathBuf::from)
        .next()
        .expect("a control socket")
}

/// A `kbf-beta` that is killed when the test ends however it ends.
struct Beta(Child);

impl Drop for Beta {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Catches: a run that cannot start its sentinel going on regardless: it would promise a
/// cleanup it cannot do. It fails before it starts a farm, with the reason.
#[test]
fn a_run_whose_sentinel_cannot_start_fails_before_it_starts_a_farm() {
    let mut args = b0("sentinel-fails", "ok", 1);
    args.reaper = Some(PathBuf::from("/nonexistent/kbf-beta"));
    assert_eq!(run(&args).expect("a run"), Ended::Failed);
    let doc = document(&args);
    assert!(
        doc["error"]
            .as_str()
            .expect("error")
            .starts_with("the sentinel:"),
        "{doc}"
    );
    assert!(!args.work.join("farm").exists());
}

/// Catches: a sentinel that cleans up after a run that ended well: each loop kills its own
/// daemon once, and the sentinel, told `done`, kills none (a second kill per loop is a second
/// wait on a daemon, and on a wedged one a wasted bound). The run passes with it.
#[test]
fn a_healthy_run_with_a_sentinel_passes_and_the_sentinel_cleans_up_nothing() {
    let mut args = b0("sentinel-healthy", "ok", 3);
    args.reaper = Some(sibling_bin("kbf-beta"));
    assert_eq!(run(&args).expect("a run"), Ended::Passed);
    assert_eq!((kills(&args.work, 0), kills(&args.work, 1)), (1, 1));
}

/// Catches: the claim that a run leaves nothing behind if it is killed. `kbf-beta` is run as
/// the workflow runs it, over the stand-in `buck2` that notes every `kill` it is given, and is
/// sent SIGKILL under load (it runs no `Drop` and no handler). The farm's processes die with
/// it (the parent-death signal), and the sentinel, which does not, kills both loops' daemons
/// and removes the frontend's control socket. Nothing was killed while the run lived, so the
/// markers are the sentinel's. Mutant: no sentinel started (`reaper: None` in the CLI).
#[test]
fn a_kbf_beta_killed_by_sigkill_leaves_no_daemon_socket_or_farm_process() {
    let _slot = take_slot();
    let dir = scratch("sigkill");
    let (project, bins, work) = (dir.join("project"), dir.join("bin"), dir.join("work"));
    std::fs::create_dir_all(&project).expect("project");
    std::fs::create_dir_all(&bins).expect("bins");
    std::fs::write(project.join(".fake-buck2"), "ok").expect("mode");
    let fake = sibling_bin(FAKE_BUCK2);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_kbf"), bins.join("kbf")).expect("kbf");
    // Any file: the stand-in build runs no action.
    std::os::unix::fs::symlink(&fake, bins.join("kbf-m2-act")).expect("act");
    let mut beta = Beta(
        Command::new(sibling_bin("kbf-beta"))
            .args(["b0", "--duration-secs", "600", "--settle-secs", "1"])
            .args(["--concurrency", "2", "--build-timeout-secs", "60"])
            .args(["--pause-secs", "1", "--fill-kib", "4"])
            .arg("--bin-dir")
            .arg(&bins)
            .arg("--buck2")
            .arg(&fake)
            .arg("--project")
            .arg(&project)
            .arg("--work")
            .arg(&work)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("kbf-beta runs"),
    );
    wait_for_the_load(&work);
    let frontend = pid_of(&work, "frontend");
    let node = pid_of(&work, "node-1");
    let socket = handover_socket(frontend);
    assert!(socket.exists(), "the frontend made its control socket");
    assert_eq!(
        (kills(&work, 0), kills(&work, 1)),
        (0, 0),
        "killed too soon"
    );

    beta.0.kill().expect("SIGKILL");
    beta.0.wait().expect("reaped");

    until("both daemons to be killed", || {
        (kills(&work, 0) >= 1 && kills(&work, 1) >= 1).then_some(())
    });
    until("the control socket to be removed", || {
        (!socket.exists()).then_some(())
    });
    for pid in [frontend, node] {
        until("a farm process to die with the harness", || {
            gone(pid.as_raw_nonzero().get()).then_some(())
        });
    }
}
