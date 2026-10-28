//! A [`Host`] for tests: user records, launchd domains and processes in memory, with
//! a log of every call and failures on demand.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::autologin::CommandOutput;
use crate::helper::{Account, Host, LiveProcess, NewUser, Process};

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
    /// "command", "reset_password", "boot_session", "account", "processes", "kill_group").
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
    /// Whether `chflags` on the fake host has the effect `uchg` has: a file with the flag
    /// cannot be unlinked, and neither can a directory that holds one. Modelled by taking
    /// the write permission off the parent of each flagged path until `nouchg`.
    pub(crate) enforce_uchg: bool,
    /// The paths `chflags` has set `uchg` on.
    pub(crate) immutable: BTreeSet<PathBuf>,
    /// The process table `processes` shows (for `role-reap`).
    pub(crate) table: Vec<Process>,
    /// How many more `processes` calls succeed before every one fails; `None` for no
    /// such limit.
    pub(crate) tables_ok: Option<usize>,
    /// How many more `kill_group` calls leave their group alive.
    pub(crate) stubborn_groups: usize,
    /// The launchd domains that exist (`gui/<uid>` once a login has completed).
    pub(crate) domains: BTreeSet<String>,
}

impl State {
    /// The effect of a successful `chflags -h uchg|nouchg <paths>` when `enforce_uchg` is
    /// on: the parent of every path with the flag loses its write permission (so nothing
    /// in it can be unlinked, and a flagged directory cannot be removed from its parent)
    /// until the last flag under it is cleared.
    fn chflags(&mut self, args: &[&str]) {
        let flag = args[1];
        for path in &args[2..] {
            self.set_flag(PathBuf::from(path), flag == "uchg");
        }
    }

    /// Sets or clears `uchg` on `path` (and its effect, when `enforce_uchg` is on).
    fn set_flag(&mut self, path: PathBuf, on: bool) {
        use std::os::unix::fs::PermissionsExt as _;
        if on {
            self.immutable.insert(path.clone());
        } else {
            self.immutable.remove(&path);
        }
        if self.enforce_uchg {
            let parent = path.parent().unwrap();
            let held = self.immutable.iter().any(|p| p.parent() == Some(parent));
            let mode = std::fs::metadata(parent).unwrap().permissions().mode();
            let mode = if held { mode & !0o222 } else { mode | 0o200 };
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(mode)).unwrap();
        }
    }
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
        let mut state = self.state();
        if let Some(hook) = &state.on_command {
            hook(&text);
        }
        let output = match state.answers.get(&text) {
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
        };
        if output.success && program == crate::helper::CHFLAGS {
            state.chflags(args);
        }
        Ok(output)
    }

    fn account(&self, name: &str) -> io::Result<Option<Account>> {
        self.call("account", format!("account {name}"))?;
        Ok(self.state().users.get(name).map(|user| Account {
            uid: user.uid,
            gid: user.gid,
        }))
    }

    /// The password is `pw-<name>`.
    fn reset_password(&self, name: &str) -> io::Result<String> {
        self.call("reset_password", format!("reset_password {name}"))?;
        Ok(format!("pw-{name}"))
    }

    /// A real copy of the bytes into a new file (mode 0644, owned by the test).
    fn clone_file(
        &self,
        src: std::os::fd::BorrowedFd<'_>,
        dir: std::os::fd::BorrowedFd<'_>,
        name: &str,
    ) -> io::Result<()> {
        self.call("clone_file", format!("clone_file {name}"))?;
        let flags = rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC;
        let to = rustix::fs::openat(dir, name, flags, rustix::fs::Mode::from_raw_mode(0o644))?;
        let mut from = std::fs::File::from(src.try_clone_to_owned()?);
        io::copy(&mut from, &mut std::fs::File::from(to))?;
        // A clone of a file with `uchg` has it too.
        let (source, target) = (fd_path(&src), fd_path(&dir).join(name));
        let mut state = self.state();
        if state.immutable.contains(&source) {
            state.set_flag(target, true);
        }
        Ok(())
    }

    fn clear_user_flags(&self, file: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
        self.call("clear_user_flags", "clear_user_flags".to_owned())?;
        self.state().set_flag(fd_path(&file), false);
        Ok(())
    }

    fn boot_session(&self) -> io::Result<String> {
        self.call("boot_session", "boot_session".to_owned())?;
        Ok(self
            .state()
            .boot
            .clone()
            .unwrap_or_else(|| "boot-2".to_owned()))
    }

    fn processes(&self) -> io::Result<Vec<Process>> {
        self.call("processes", "processes".to_owned())?;
        let mut state = self.state();
        if let Some(left) = &mut state.tables_ok {
            if *left == 0 {
                return Err(io::Error::other("processes ran out"));
            }
            *left -= 1;
        }
        Ok(state.table.clone())
    }

    fn kill_group(&self, pgid: i32) -> io::Result<()> {
        self.call("kill_group", format!("kill_group {pgid}"))?;
        let mut state = self.state();
        if state.stubborn_groups > 0 {
            state.stubborn_groups -= 1;
        } else {
            state.table.retain(|process| process.pgid != pgid);
        }
        Ok(())
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

/// The path an open descriptor names: `/proc` on Linux, `F_GETPATH` on macOS (which
/// answers the path with its links resolved; [`scratch`] is already resolved).
fn fd_path(fd: &impl std::os::fd::AsFd) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let path = rustix::fs::getpath(fd).unwrap();
        PathBuf::from(std::ffi::OsStr::from_bytes(path.as_bytes()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        use std::os::fd::AsRawFd as _;
        std::fs::read_link(format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd())).unwrap()
    }
}

/// A fresh scratch directory for one test. Under `/tmp` with a short name, not the
/// per-user temporary folder: a socket path on macOS holds at most 104 bytes, and
/// macOS's per-user temporary folder alone takes about 50.
///
/// Under the resolved `/tmp` (`/private/tmp` on macOS), so a path the tests hand a
/// command is the one `F_GETPATH` answers for the same file. In the user's own group:
/// a new entry takes its directory's group on macOS and `/tmp` is `wheel`'s, which a
/// test user cannot give a file to, as the helper does when it gives a bundle back.
pub(crate) fn scratch(name: &str) -> kbf_testkit::dir::TestDir {
    let root = std::fs::canonicalize("/tmp").unwrap();
    let dir = kbf_testkit::dir::TestDir::under(root, &format!("kms-{name}"));
    std::os::unix::fs::chown(dir.path(), None, Some(rustix::process::getgid().as_raw())).unwrap();
    dir
}

/// Shows tracing output in test logs, so the log statements' arguments run.
pub(crate) fn trace() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
}

// This test binary answers as kbf-testkit's `kbf-act` when it is run through a link of
// that name: the program the tests start as a lease's process.
kbf_testkit::act_test_hook!();

/// The path of a link to this test binary named `kbf-act`, made once: run through it,
/// the binary runs kbf-testkit's act steps (`kbf_testkit::act`) instead of the tests.
pub(crate) fn act() -> String {
    static LINK: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    LINK.get_or_init(|| {
        // Beside the test binary, one link for every run of it (a directory made per run
        // would never be removed: nothing runs when a process ends). A link to another
        // build of the binary is replaced by renaming a new one over it, so a run that
        // is using the old one never finds it missing.
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap().join("kbf-mac-session-act");
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join(kbf_testkit::act::NAME);
        if std::fs::read_link(&link).ok().as_deref() != Some(exe.as_path()) {
            let new = dir.join(format!("new-{}", std::process::id()));
            let _ = std::fs::remove_file(&new);
            kbf_testkit::act::link_self(&new).unwrap();
            std::fs::rename(&new, &link).unwrap();
        }
        link
    })
    .display()
    .to_string()
}

/// `act()` and then `steps`, split at spaces: the argument vector of a process that
/// runs those act steps.
pub(crate) fn act_argv(steps: &str) -> Vec<String> {
    std::iter::once(act())
        .chain(steps.split(' ').map(str::to_owned))
        .collect()
}
