//! A [`Host`] for tests: user records, launchd domains and processes in memory, with
//! a log of every call and failures on demand.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::autologin::CommandOutput;
use crate::helper::{Host, LiveProcess, NewUser};

/// The fake clock's time.
pub(crate) const NOW: u64 = 1_800_000_000;

/// More pauses than any bounded wait of the helper takes: past this a wait has no
/// bound, and the test panics instead of hanging.
const PAUSES: usize = 10_000;

/// What runs on the fake host between two of the helper's steps.
pub(crate) type Hook = Box<dyn Fn(&str) + Send>;

#[derive(Default)]
pub(crate) struct State {
    /// Every call, in order, as text.
    pub(crate) log: Vec<String>,
    /// Users by name.
    pub(crate) users: BTreeMap<String, NewUser>,
    /// Uids some user record outside the helper's has.
    pub(crate) foreign: BTreeSet<u32>,
    /// Live processes per uid.
    pub(crate) procs: BTreeMap<u32, usize>,
    /// Answers `live_processes` gives, in order, before it reads `procs` again: a
    /// process that starts (or ends) between two looks.
    pub(crate) live_script: VecDeque<usize>,
    /// Per uid, how many more `kill_all` calls leave its processes alive.
    pub(crate) stubborn: BTreeMap<u32, usize>,
    /// A file each `kill_all` looks at, logging whether it is there: shows where the
    /// helper removes it among the kills.
    pub(crate) watch: Option<PathBuf>,
    /// Every `pause` so far.
    pub(crate) pauses: usize,
    /// Calls that fail, by name ("uid_taken", "make_home", "create_user",
    /// "delete_user", "domain_exists", "bootout gui", "kill_all", "live_processes",
    /// "command", "reset_password", "boot_session").
    pub(crate) fail: BTreeSet<&'static str>,
    /// What `command` answers, by the command's text (`program arg ...`). A command
    /// with no answer succeeds and prints nothing, except `fdesetup isactive`, which
    /// prints `false` (FileVault off), and `defaults read`, which fails as `defaults`
    /// does for a key that is not set.
    pub(crate) answers: BTreeMap<String, CommandOutput>,
    /// A file each `command` reads, logging what it holds: shows what the intent record
    /// said when each program ran.
    pub(crate) intent: Option<PathBuf>,
    /// What `boot_session` answers; `boot-2` when unset.
    pub(crate) boot: Option<String>,
    /// Runs with each command's text before it is answered: a change on the host
    /// between two of the helper's steps.
    pub(crate) on_command: Option<Hook>,
    /// The launchd domains that exist (`gui/<uid>` once a login has completed).
    pub(crate) domains: BTreeSet<String>,
}

/// The fake host; clones share their state.
#[derive(Clone, Default)]
pub(crate) struct FakeHost(pub(crate) Arc<Mutex<State>>);

impl FakeHost {
    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    fn call(&self, name: &'static str, text: String) -> io::Result<()> {
        let mut state = self.state();
        state.log.push(text);
        if state.fail.contains(name) {
            return Err(io::Error::other(format!("{name} failed")));
        }
        Ok(())
    }
}

impl Host for FakeHost {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(NOW)
    }

    fn uid_taken(&self, uid: u32) -> io::Result<bool> {
        self.call("uid_taken", format!("uid_taken {uid}"))?;
        let state = self.state();
        Ok(state.foreign.contains(&uid) || state.users.values().any(|u| u.uid == uid))
    }

    fn make_home(&self, homes: &Path, name: &str, uid: u32, gid: u32) -> io::Result<()> {
        self.call(
            "make_home",
            format!("make_home {} {uid}:{gid}", homes.join(name).display()),
        )
    }

    fn create_user(&self, user: &NewUser) -> io::Result<()> {
        self.call("create_user", format!("create_user {}", user.name))?;
        self.state().users.insert(user.name.clone(), user.clone());
        Ok(())
    }

    fn delete_user(&self, name: &str) -> io::Result<bool> {
        self.call("delete_user", format!("delete_user {name}"))?;
        Ok(self.state().users.remove(name).is_some())
    }

    fn domain_exists(&self, domain: &str) -> io::Result<bool> {
        self.call("domain_exists", format!("domain_exists {domain}"))?;
        Ok(self.state().domains.contains(domain))
    }

    fn bootout(&self, domain: &str) -> io::Result<()> {
        let name = if domain.starts_with("gui/") {
            "bootout gui"
        } else {
            "bootout user"
        };
        self.call(name, format!("bootout {domain}"))
    }

    fn kill_all(&self, uid: u32) -> io::Result<()> {
        let watched = match &self.state().watch {
            Some(file) if file.exists() => " (watched file present)",
            Some(_) => " (watched file absent)",
            None => "",
        };
        self.call("kill_all", format!("kill_all {uid}{watched}"))?;
        let mut state = self.state();
        let stubborn = state.stubborn.get(&uid).copied().unwrap_or(0);
        if stubborn > 0 {
            state.stubborn.insert(uid, stubborn - 1);
        } else {
            state.procs.remove(&uid);
        }
        Ok(())
    }

    /// `n` live processes are pids 1000 to 999+n, commands `proc0` and on.
    fn live_processes(&self, uid: u32) -> io::Result<Vec<LiveProcess>> {
        self.call("live_processes", format!("live_processes {uid}"))?;
        let mut state = self.state();
        let scripted = state.live_script.pop_front();
        let n = scripted.unwrap_or_else(|| state.procs.get(&uid).copied().unwrap_or(0));
        Ok((0..n)
            .map(|i| LiveProcess {
                pid: 1000 + i32::try_from(i).unwrap(),
                command: format!("proc{i}"),
            })
            .collect())
    }

    fn command(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        let text = std::iter::once(program)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        let seen = match &self.state().intent {
            Some(file) => match std::fs::read_to_string(file) {
                Ok(held) => format!(" (intent {})", held.trim_end()),
                Err(_) => " (no intent)".to_owned(),
            },
            None => String::new(),
        };
        self.call("command", format!("command {text}{seen}"))?;
        let state = self.state();
        if let Some(hook) = &state.on_command {
            hook(&text);
        }
        Ok(match state.answers.get(&text) {
            Some(answer) => answer.clone(),
            None if text == "/usr/bin/fdesetup isactive" => CommandOutput {
                success: false,
                stdout: "false\n".to_owned(),
                stderr: String::new(),
            },
            None if text.starts_with("/usr/bin/defaults read ") => CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: format!("The domain/default pair of ({text}) does not exist\n"),
            },
            None => CommandOutput {
                success: true,
                ..CommandOutput::default()
            },
        })
    }

    /// The password is `pw-<name>`.
    fn reset_password(&self, name: &str) -> io::Result<String> {
        self.call("reset_password", format!("reset_password {name}"))?;
        Ok(format!("pw-{name}"))
    }

    fn boot_session(&self) -> io::Result<String> {
        self.call("boot_session", "boot_session".to_owned())?;
        Ok(self
            .state()
            .boot
            .clone()
            .unwrap_or_else(|| "boot-2".to_owned()))
    }

    fn pause(&self) {
        let mut state = self.state();
        state.log.push("pause".to_owned());
        state.pauses += 1;
        assert!(
            state.pauses < PAUSES,
            "{PAUSES} pauses: a wait without a bound"
        );
    }
}

/// A fresh scratch directory for one test. Under `/tmp` with a short name, not the
/// per-user temporary folder: a socket path on macOS holds at most 104 bytes, and
/// macOS's per-user temporary folder alone takes about 50.
pub(crate) fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/kms-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Shows tracing output in test logs, so the log statements' arguments run.
pub(crate) fn trace() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
}
