//! Real stand-in children for the tests of the real child port and of the supervisor: the
//! test binary itself, run through a link named `kbf-act`, plays a release's program
//! (`kbf_testkit::act`), so no shell and no program of the box is needed. Every test ends by
//! stopping what it started, through a guard that kills and reaps.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::child_real::{ProcChild, ident_of};
use super::model::{PortError, ProcIdent};
use super::ports::Child as _;
use crate::testkit::{A, B, c, host_root_on};

kbf_testkit::act_test_hook!();

/// A scratch host with `kbf-act` as the program of releases A and B, and a `ProcChild`
/// whose command line is whatever `steps` holds when a child starts.
pub(super) struct Rig {
    pub(super) child: ProcChild,
    pub(super) steps: Arc<Mutex<Vec<String>>>,
    pub(super) dir: PathBuf,
    pub(super) layout_root: PathBuf,
}

pub(super) fn rig(name: &str) -> Rig {
    let (layout, root) = host_root_on(name, crate::layout::Platform::current());
    for commit in [c(A), c(B)] {
        let release = layout.join(&layout.release_dir(&commit));
        fs::create_dir_all(&release).expect("release dir");
        symlink(
            std::env::current_exe().expect("test binary"),
            release.join("kbf-act"),
        )
        .expect("link");
    }
    let steps = Arc::new(Mutex::new(vec!["sleep".to_owned(), "600".to_owned()]));
    let seen = Arc::clone(&steps);
    let dir = root.join("children");
    let child = ProcChild::new(
        layout,
        dir.clone(),
        Box::new(move |_, _| {
            seen.lock()
                .expect("lock")
                .iter()
                .map(OsString::from)
                .collect()
        }),
    )
    .with_binary("kbf-act");
    Rig {
        child,
        steps,
        dir,
        layout_root: root,
    }
}

impl Rig {
    pub(super) fn set(&self, steps: &[&str]) {
        *self.steps.lock().expect("lock") = steps.iter().map(|s| (*s).to_owned()).collect();
    }

    /// A second `ProcChild` over the same host: the proxy after a restart.
    pub(super) fn restarted(&self) -> ProcChild {
        let layout = self.child.layout.clone();
        ProcChild::new(layout, self.dir.clone(), Box::new(|_, _| Vec::new())).with_binary("kbf-act")
    }
}

/// Kills (and reaps, for our own children) what a test started, whatever happens, and, when
/// the test did not panic, checks that none of it is left running: a stand-in child that
/// outlives its test sleeps for ten minutes and blocks whatever waits on the test's output.
pub(super) struct Cleanup<'a>(pub(super) &'a ProcChild, pub(super) Vec<ProcIdent>);

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        for ident in &self.1 {
            let _ = self.0.kill_unserved(ident);
        }
        // Up to 10 s for the killed processes to go.
        await_gone(&self.1, 2000, Duration::from_millis(5));
        let orphans = running(&self.1);
        // `&`, not `&&`: no short-circuit branch for the coverage ratchet to count.
        let leaked = !orphans.is_empty() & !std::thread::panicking();
        assert!(!leaked, "children left running by the test: {orphans:?}");
    }
}

/// The pids of `ids` that are still those processes: the same pid and start time, not
/// whatever took the pid since (a pid is also a thread id).
pub(super) fn running(ids: &[ProcIdent]) -> Vec<u32> {
    ids.iter()
        .filter(|i| ident_of(i.pid).is_ok_and(|now| now == **i))
        .map(|i| i.pid)
        .collect()
}

/// Sleeps `pause` up to `polls` times while any of `ids` still runs. No branch, so no
/// uncovered one.
pub(super) fn await_gone(ids: &[ProcIdent], polls: u32, pause: Duration) {
    (0..polls)
        .take_while(|_| !running(ids).is_empty())
        .for_each(|_| std::thread::sleep(pause));
}

/// The error of a spawn the test expects to fail. A spawn that wrongly succeeds is stopped
/// by a [`Cleanup`] while the test panics, so a defect cannot leave a child running. No
/// branch: the same line serves the pass and the panic.
pub(super) fn refused(
    child: &ProcChild,
    spawned: Result<ProcIdent, PortError>,
    what: &str,
) -> PortError {
    let started: Vec<ProcIdent> = spawned.as_ref().ok().cloned().into_iter().collect();
    let _stop = Cleanup(child, started);
    spawned.expect_err(what)
}

pub(super) fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out: {what}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub(super) fn alive(pid: u32) -> bool {
    // A zombie counts as gone: its state is Z.
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
        s.rsplit(')')
            .next()
            .is_some_and(|r| !r.trim_start().starts_with('Z'))
    })
}
