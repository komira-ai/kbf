//! A fake `kbf-mac-session` for the whole-machine runtime: every verb is logged; the
//! GUI session comes up after a set number of questions; a verb can fail, panic, or
//! hold until released (the userspace restart a session switch makes, which ends the
//! daemon mid-call); `run` starts the action as this test's own user, in its own
//! process group, which `kill-uid` kills.

use std::collections::BTreeSet;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use kbf_driver_native::whole_machine::{Exit, Session};
use kbf_types::LeaseId;

/// How long a held verb waits for its release before it gives up (so a test that
/// forgets to release never hangs).
const HOLD: Duration = Duration::from_secs(20);

#[derive(Debug, Default)]
pub struct State {
    /// Every verb, in order, as `<verb> [lease]`.
    pub log: Vec<String>,
    /// How many `session-ready` questions answer false before one answers true.
    pub ready_after: usize,
    asked: usize,
    /// How many questions had been asked when `run` was called.
    pub asked_at_run: Option<usize>,
    /// Verbs that fail.
    pub fail: BTreeSet<&'static str>,
    /// Verbs that panic.
    pub panic: BTreeSet<&'static str>,
    /// Verbs that wait until [`FakeSession::release`].
    pub hold: BTreeSet<&'static str>,
    released: bool,
    /// The leases whose users the boot reset says it deleted.
    pub deleted_by_reset: Vec<LeaseId>,
    /// The process group of the action that runs.
    group: Option<i32>,
    /// The runtime's phase, logged with each verb once set.
    pub phase: Option<tokio::sync::watch::Receiver<kbf_types::NodePhase>>,
}

/// Clones share their state.
#[derive(Clone, Debug, Default)]
pub struct FakeSession(Arc<(Mutex<State>, Condvar)>);

impl FakeSession {
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.0
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Lets held verbs return.
    pub fn release(&self) {
        self.state().released = true;
        self.0.1.notify_all();
    }

    /// The verbs so far, each repeated `session-ready` once; clears the log.
    pub fn verbs(&self) -> Vec<String> {
        let mut log = std::mem::take(&mut self.state().log);
        log.dedup();
        log
    }

    fn verb(&self, name: &'static str, lease: Option<LeaseId>) -> Result<(), String> {
        let mut state = self.state();
        let mut line = match lease {
            Some(lease) => format!("{name} {lease}"),
            None => name.to_owned(),
        };
        if let Some(phase) = &state.phase {
            line = format!("{line} ({})", phase.borrow().name());
        }
        state.log.push(line);
        assert!(!state.panic.contains(name), "{name} panicked, as asked");
        if state.hold.contains(name) {
            let (state_after, _) = self
                .0
                .1
                .wait_timeout_while(state, HOLD, |s| !s.released)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = state_after;
        }
        if state.fail.contains(name) {
            return Err(format!("{name} failed"));
        }
        Ok(())
    }
}

impl Session for FakeSession {
    type Running = Child;

    fn boot_reset(&self, held: Option<LeaseId>) -> Result<Vec<LeaseId>, String> {
        self.verb("boot-reset", held)?;
        Ok(self.state().deleted_by_reset.clone())
    }

    fn user_create(&self, lease: LeaseId) -> Result<u32, String> {
        self.verb("user-create", Some(lease))?;
        Ok(rustix::process::getuid().as_raw())
    }

    fn share(&self, lease: LeaseId, dir: &Path) -> Result<(), String> {
        assert!(dir.is_dir(), "shared before it was made");
        self.verb("share", Some(lease))
    }

    fn session_login(&self, lease: LeaseId) -> Result<(), String> {
        self.verb("session-login", Some(lease))
    }

    fn session_ready(&self, lease: LeaseId) -> Result<bool, String> {
        self.verb("session-ready", Some(lease))?;
        let mut state = self.state();
        state.asked += 1;
        Ok(state.asked > state.ready_after)
    }

    fn run(
        &self,
        lease: LeaseId,
        argv: &[String],
        env: &[(String, String)],
        fds: [OwnedFd; 4],
    ) -> Result<Child, String> {
        self.verb("run", Some(lease))?;
        {
            let mut state = self.state();
            state.asked_at_run = Some(state.asked);
        }
        let [stdin, stdout, stderr, cwd] = fds;
        let cwd_fd = cwd.as_raw_fd();
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .process_group(0);
        // SAFETY: fchdir is async-signal-safe; `cwd` stays open until spawn returns.
        unsafe {
            command.pre_exec(move || {
                if libc::fchdir(cwd_fd) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        let child = command.spawn().map_err(|e| e.to_string())?;
        drop(cwd);
        self.state().group = i32::try_from(child.id()).ok();
        Ok(child)
    }

    fn wait(&self, mut running: Child) -> Result<Exit, String> {
        let status = running.wait().map_err(|e| e.to_string())?;
        self.verb("wait", None)?;
        Ok(Exit {
            code: status.code(),
            signal: status.signal(),
        })
    }

    fn kill_uid(&self, lease: LeaseId) -> Result<(), String> {
        self.verb("kill-uid", Some(lease))?;
        if let Some(group) = self.state().group.take() {
            // SAFETY: kill takes plain numbers; a group already gone is ESRCH.
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
        Ok(())
    }

    fn session_idle(&self) -> Result<(), String> {
        self.verb("session-idle", None)
    }

    fn user_delete(&self, lease: LeaseId) -> Result<(), String> {
        self.verb("user-delete", Some(lease))
    }
}
