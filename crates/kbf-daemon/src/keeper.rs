//! The keeper mode of the `kbf-daemon` binary (issue #155): the process each run gets
//! that ends the run when the daemon dies.
//!
//! The drivers start a keeper for each run by starting this same binary again, as
//! `kbf-keeper __keeper <mode>` ([`command`]), with its stdin a pipe only the daemon
//! holds the other end of. The binary's `main` calls [`dispatch`] before it parses its
//! flags, so the keeper never reads the daemon's command line. A keeper:
//!
//! 1. ignores SIGHUP, SIGINT and SIGTERM, so a service manager that stops the
//!    daemon's unit with SIGTERM does not end it before it has read end-of-file;
//! 2. writes [`READY`] on stdout. [`started`] waits for that line, so a driver's
//!    spawn returns once the keeper ignores those signals, and a program that is not
//!    a keeper (a binary whose `main` does not call [`dispatch`]) is an error, never a
//!    run left without its keeper;
//! 3. reads stdin until end-of-file, ignoring what it reads. The daemon never writes
//!    the pipe; the kernel closes the daemon's end when the daemon dies, however it
//!    dies;
//! 4. then acts by its [`Mode`]: `group` SIGKILLs its own process group (the native
//!    driver's keeper is a member of its action's); `cgroup <kill> <tries>` writes
//!    `1` to the file `kill`, a lease cgroup's `cgroup.kill`, up to `tries` times a
//!    second apart, and stops once that file cannot be opened. It never creates it.
//!
//! **The program after an update.** kbf-updater replaces the binary by renaming a new
//! file over it while the daemon runs. On Linux [`program`] is `/proc/self/exe`, which
//! names the file the daemon was started from even after a rename replaced it. On
//! macOS, which has no such name, it is [`std::env::current_exe`], which there names
//! the path the daemon was started by: after a swap, the new binary. So the argument
//! list is an interface between versions: every list an older daemon starts a keeper
//! with ([`Mode::args`]) must still mean the same to a newer binary, and the tests pin
//! each list.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read as _, Write as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::Duration;

/// The argument after argv0 that selects the keeper mode.
pub const ARG: &str = "__keeper";

/// The keeper's argv0, its name in the process table.
pub const NAME: &str = "kbf-keeper";

/// What a keeper writes on stdout once it ignores the stop signals.
pub const READY: &[u8] = b"kbf-keeper: ready\n";

/// What a keeper does once its stdin reaches end-of-file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// SIGKILL the keeper's own process group.
    Group,
    /// Write `1` to `kill` (a cgroup's `cgroup.kill`) up to `tries` times, a second
    /// apart, while it can be opened.
    Cgroup { kill: PathBuf, tries: u32 },
}

impl Mode {
    /// The keeper's arguments after argv0, [`ARG`] first.
    #[must_use]
    pub fn args(&self) -> Vec<OsString> {
        match self {
            Mode::Group => vec![ARG.into(), "group".into()],
            Mode::Cgroup { kill, tries } => vec![
                ARG.into(),
                "cgroup".into(),
                kill.into(),
                tries.to_string().into(),
            ],
        }
    }

    /// The mode the arguments after [`ARG`] name, or why they name none.
    fn parse(args: &[OsString]) -> Result<Self, String> {
        let args: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
        match args[..] {
            [mode] if mode == "group" => Ok(Mode::Group),
            [mode, kill, tries] if mode == "cgroup" => {
                let tries = tries.to_str().and_then(|t| t.parse().ok());
                let tries = tries.ok_or_else(|| format!("tries {:?} is not a count", args[2]))?;
                let kill = PathBuf::from(kill);
                Ok(Mode::Cgroup { kill, tries })
            }
            _ => Err(format!(
                "usage: {ARG} group | {ARG} cgroup <cgroup.kill> <tries>, not {args:?}"
            )),
        }
    }
}

/// The program a keeper is started from: this process's own binary, as
/// `/proc/self/exe` (see the module docs for what that names after an update). It
/// never fails on Linux; the `Result` is macOS's.
#[cfg(target_os = "linux")]
pub fn program() -> io::Result<PathBuf> {
    Ok(PathBuf::from("/proc/self/exe"))
}

/// The program a keeper is started from: this process's own binary, as the path it
/// was started by (see the module docs for what that names after an update).
///
/// # Errors
/// The binary's path could not be read.
#[cfg(not(target_os = "linux"))]
pub fn program() -> io::Result<PathBuf> {
    std::env::current_exe()
}

/// The command that starts a keeper in `mode` from `program`: argv0 [`NAME`], stdin
/// and stdout pipes, stderr discarded, in `/`. The caller sets its process group,
/// spawns it and calls [`started`].
#[must_use]
pub fn command(program: &Path, mode: &Mode) -> Command {
    let mut command = Command::new(program);
    command
        .arg0(NAME)
        .args(mode.args())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .current_dir("/");
    command
}

/// Waits for a spawned keeper's [`READY`] line, then closes its stdout.
///
/// # Errors
/// The program ended, or wrote anything else first: it is not a keeper.
pub fn started(keeper: &mut Child) -> io::Result<()> {
    let mut answer = [0; READY.len()];
    let read = match keeper.stdout.take() {
        Some(mut stdout) => stdout.read_exact(&mut answer),
        None => Err(io::ErrorKind::BrokenPipe.into()),
    };
    if read.is_ok() && answer == READY {
        return Ok(());
    }
    Err(io::Error::other(
        "the keeper's program did not answer as a keeper: is it a kbf-daemon binary?",
    ))
}

/// The keeper mode, when the process's arguments ask for it: runs it and returns its
/// exit status (2 for arguments that name no mode). `None` otherwise. A binary that
/// starts keepers calls this first in `main`.
#[must_use]
pub fn dispatch() -> Option<ExitCode> {
    code(&std::env::args_os().collect::<Vec<_>>()).map(ExitCode::from)
}

/// [`dispatch`] for a test binary, which has no `main` of its own: exits with the
/// keeper's status when the arguments ask for the keeper mode. Called by
/// [`keeper_test_hook!`](crate::keeper_test_hook) before the test harness starts.
#[doc(hidden)]
pub fn hook() {
    if let Some(code) = code(&std::env::args_os().collect::<Vec<_>>()) {
        std::process::exit(i32::from(code));
    }
}

/// Makes the test binary it is expanded in answer as a keeper, as the `kbf-daemon`
/// binary does: a driver's tests start their keepers from the running binary
/// ([`program`]), which is the test binary. It runs [`hook`] before the test harness
/// reads its arguments, from the platform's list of start-up functions.
#[macro_export]
macro_rules! keeper_test_hook {
    () => {
        const _: () = {
            extern "C" fn kbf_keeper_hook() {
                $crate::keeper::hook();
            }
            #[used]
            #[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
            #[cfg_attr(
                target_vendor = "apple",
                unsafe(link_section = "__DATA,__mod_init_func,mod_init_funcs")
            )]
            static KBF_KEEPER_HOOK: extern "C" fn() = kbf_keeper_hook;
        };
    };
}

/// The keeper's exit status when `args` (argv0 first) ask for it; `None` otherwise.
fn code(args: &[OsString]) -> Option<u8> {
    if args.get(1)? != ARG {
        return None;
    }
    Some(match Mode::parse(&args[2..]) {
        Ok(mode) => run(&mode),
        Err(why) => {
            eprintln!("{NAME}: {why}");
            2
        }
    })
}

/// Steps 1 to 4 of the module docs; the exit status.
fn run(mode: &Mode) -> u8 {
    for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGTERM] {
        // SAFETY: SIG_IGN installs no handler, so no code of this process runs when
        // the signal comes. It cannot fail for these three (EINVAL is for SIGKILL,
        // SIGSTOP and numbers out of range). rustix offers no way to set a signal's
        // disposition outside its runtime module, which is for libc implementations.
        unsafe { libc::signal(signal, libc::SIG_IGN) };
    }
    // `comm` would otherwise be `exe`, from `/proc/self/exe`; a failure costs a name.
    #[cfg(target_os = "linux")]
    let _ = rustix::thread::set_name(c"kbf-keeper");
    // A failed write leaves the daemon without the READY line: it refuses the run,
    // and kills this process when it drops its keeper.
    let _ = io::stdout().write_all(READY);
    // A read error ends the wait as end-of-file does: the daemon cannot be heard.
    let _ = io::copy(&mut io::stdin().lock(), &mut io::sink());
    match mode {
        Mode::Group => {
            // Kills this process too: it returns only if the kill failed.
            let _ = rustix::process::kill_current_process_group(rustix::process::Signal::KILL);
            1
        }
        Mode::Cgroup { kill, tries } => {
            for _ in 0..*tries {
                // Without `create`: once the cgroup is gone the keeper is done.
                let opened = std::fs::OpenOptions::new().write(true).open(kill);
                let Ok(mut file) = opened else { break };
                let _ = file.write_all(b"1\n");
                std::thread::sleep(Duration::from_secs(1));
            }
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    /// The argument lists of the keepers this version starts. Catches a change to
    /// either (an old daemon's keeper argv that a newer binary would no longer run as
    /// the same keeper: on macOS an old daemon starts its keepers from the binary an
    /// update swapped in) and a mode whose list does not parse back to it.
    #[test]
    fn the_keeper_argv_is_pinned() {
        let kill = "/sys/fs/cgroup/kbf/actions/lease-1-2/cgroup.kill";
        let golden = [
            (Mode::Group, args(&["__keeper", "group"])),
            (
                Mode::Cgroup {
                    kill: PathBuf::from(kill),
                    tries: 30,
                },
                args(&["__keeper", "cgroup", kill, "30"]),
            ),
        ];
        for (mode, argv) in golden {
            assert_eq!(mode.args(), argv, "{mode:?}");
            assert_eq!(Mode::parse(&argv[1..]), Ok(mode));
        }
    }

    /// Catches a binary that runs as a daemon, or as a keeper, when its arguments do
    /// not ask for the keeper mode, and a keeper mode that does anything for arguments
    /// that name no mode (status 2, and nothing run).
    #[test]
    fn only_the_keeper_arguments_select_the_keeper() {
        assert_eq!(code(&args(&["kbf-daemon"])), None);
        assert_eq!(code(&args(&["kbf-daemon", "--driver", "native"])), None);
        assert_eq!(code(&args(&["kbf-daemon", "--", "__keeper"])), None);
        for bad in [
            &["__keeper"][..],
            &["__keeper", "groups"],
            &["__keeper", "group", "x"],
            &["__keeper", "group", "x", "y"],
            &["__keeper", "cgroup", "/k"],
            &["__keeper", "cgroup", "/k", "-1"],
            &["__keeper", "cgroup", "/k", "30", "x"],
        ] {
            let argv = [&["kbf-keeper"][..], bad].concat();
            assert_eq!(code(&args(&argv)), Some(2), "{bad:?}");
        }
    }
}
