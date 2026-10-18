//! What root observes about the service and the processes on the host: whether the unit is
//! active, which process is its main one, and which release a process executes from.
//!
//! Two decisions rest on it. A confirm is accepted only if the new release is what runs
//! ([`accept_confirm`]): the unit is active, its main process's executable is inside the
//! pending release's directory, and (Linux) the first boot check has run. And `gc` never
//! removes a release a process executes from ([`in_use`]). Neither is taken from a record.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_host::{Cmd, Exec, HostPath};

use crate::CommitId;
use crate::helper::restart::Restart;
use crate::layout::{self, Layout, Platform};
use crate::run::{Runner, Spawn, SystemRunner};
use crate::state::PendingConfirm;

/// How long a `ps` may take.
const PS_TIMEOUT: Duration = Duration::from_secs(10);

/// Executables of the processes on the host.
pub trait Procs {
    /// The executable of process `pid`.
    fn exe_of(&self, pid: u32) -> io::Result<PathBuf>;
    /// The executable of every process whose executable can be read.
    fn executables(&self) -> io::Result<Vec<PathBuf>>;
}

/// The real host: `/proc` on Linux, `ps` on macOS.
pub struct HostProcs {
    platform: Platform,
    runner: Box<dyn Runner>,
}

impl HostProcs {
    pub fn new(platform: Platform) -> HostProcs {
        HostProcs::with_runner(platform, Box::new(SystemRunner))
    }

    pub fn with_runner(platform: Platform, runner: Box<dyn Runner>) -> HostProcs {
        HostProcs { platform, runner }
    }
}

impl Procs for HostProcs {
    fn exe_of(&self, pid: u32) -> io::Result<PathBuf> {
        match self.platform {
            Platform::Linux => std::fs::read_link(format!("/proc/{pid}/exe")),
            Platform::MacOs => {
                let out = ps(&*self.runner, &["-p", &pid.to_string(), "-o", "comm="])?;
                parse_ps(&out)
                    .into_iter()
                    .next()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such process"))
            }
        }
    }

    fn executables(&self) -> io::Result<Vec<PathBuf>> {
        match self.platform {
            Platform::Linux => {
                let mut found = Vec::new();
                for entry in std::fs::read_dir("/proc")? {
                    let name = entry?.file_name();
                    if name.to_str().is_none_or(|n| n.parse::<u32>().is_err()) {
                        continue;
                    }
                    // A process that exited meanwhile, or whose link root cannot read,
                    // has no executable to protect.
                    if let Ok(exe) = std::fs::read_link(Path::new("/proc").join(&name).join("exe"))
                    {
                        found.push(exe);
                    }
                }
                Ok(found)
            }
            Platform::MacOs => Ok(parse_ps(&ps(&*self.runner, &["-ax", "-o", "comm="])?)),
        }
    }
}

fn ps(runner: &dyn Runner, args: &[&str]) -> io::Result<Vec<u8>> {
    let spawn = Spawn {
        program: PathBuf::from("/bin/ps"),
        args: args.iter().map(OsString::from).collect(),
        env: Vec::new(),
        cwd: PathBuf::from("/"),
        run_as: None,
        timeout: PS_TIMEOUT,
    };
    let ran = runner.run(&spawn)?;
    if ran.code != Some(0) || ran.timed_out {
        return Err(io::Error::other("ps failed"));
    }
    Ok(ran.stdout)
}

/// The absolute paths among the lines `ps -o comm=` printed (a name that is not a path is a
/// kernel thread or a process whose path is hidden).
pub fn parse_ps(out: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(out)
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('/'))
        .map(PathBuf::from)
        .collect()
}

fn query<E: Exec>(exec: &E, program: &str, args: &[&str]) -> Result<std::process::Output, String> {
    let program = HostPath::new(program).expect("an absolute path");
    let cmd = args.iter().fold(Cmd::new(program), |c, a| c.arg(*a));
    exec.query(&cmd)
        .map_err(|e| format!("cannot ask the service manager: {e}"))
}

/// The main process of the service, if the unit is active (Linux) or the job is running
/// (macOS); otherwise why not.
pub fn main_pid<E: Exec>(exec: &E, restart: &Restart) -> Result<u32, String> {
    match restart {
        Restart::Systemd(unit) => {
            let out = query(
                exec,
                "/usr/bin/systemctl",
                &[
                    "show",
                    "--property=ActiveState",
                    "--property=MainPID",
                    "--",
                    unit,
                ],
            )?;
            if !out.status.success() {
                return Err(format!("systemctl cannot show {unit}"));
            }
            let text = String::from_utf8_lossy(&out.stdout);
            let field = |key: &str| {
                text.lines()
                    .find_map(|l| l.strip_prefix(key))
                    .map(str::trim)
            };
            match (field("ActiveState="), field("MainPID=")) {
                (Some("active"), Some(pid)) => match pid.parse::<u32>() {
                    Ok(0) | Err(_) => Err(format!("{unit} is active and has no main process")),
                    Ok(pid) => Ok(pid),
                },
                (Some(state), _) => Err(format!("{unit} is {state}, not active")),
                (None, _) => Err(format!("systemctl did not say whether {unit} is active")),
            }
        }
        Restart::Launchd(label) => {
            let out = query(
                exec,
                "/bin/launchctl",
                &["print", &format!("system/{label}")],
            )?;
            if !out.status.success() {
                return Err(format!("{label} is not loaded"));
            }
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .find_map(|l| l.trim().strip_prefix("pid = "))
                .and_then(|p| p.trim().parse::<u32>().ok())
                .filter(|p| *p != 0)
                .ok_or_else(|| format!("{label} is loaded and not running"))
        }
    }
}

/// `path` with links resolved when it can be, so a root that is itself under a link
/// compares with what the kernel reports.
fn resolved(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

/// Whether `exe` is a file inside `dir`: by path components, so a sibling directory with the
/// same prefix is not inside, and not an executable the kernel reports as deleted.
fn inside(exe: &Path, dir: &Path) -> bool {
    let deleted = exe
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(" (deleted)"));
    !deleted && resolved(exe).starts_with(resolved(dir))
}

/// The checks a confirm passes: the first boot check has run (Linux; macOS arms at the
/// swap and has no boot check), the unit is active, and the executable of its main process
/// is inside the pending release's directory.
pub fn accept_confirm<E: Exec>(
    exec: &E,
    procs: &dyn Procs,
    host: &Layout,
    restart: &Restart,
    pending: &PendingConfirm,
) -> Result<(), String> {
    if host.platform() == Platform::Linux && pending.deadline.is_none() {
        return Err(format!(
            "{} has not been started since the swap: the first boot check has not run",
            pending.commit
        ));
    }
    let pid = main_pid(exec, restart)?;
    let exe = procs
        .exe_of(pid)
        .map_err(|e| format!("cannot read the executable of process {pid}: {e}"))?;
    let dir = host.join(&layout::release_dir(&pending.commit));
    if inside(&exe, &dir) {
        Ok(())
    } else {
        Err(format!(
            "the service's main process {pid} runs {}, not a file in {}",
            exe.display(),
            dir.display()
        ))
    }
}

/// The releases some process executes from. An error is a host this cannot be told on, and
/// the caller must then remove nothing.
pub fn in_use(procs: &dyn Procs, host: &Layout) -> io::Result<BTreeSet<CommitId>> {
    let releases = resolved(&host.join(&layout::path(layout::RELEASES)));
    Ok(procs
        .executables()?
        .iter()
        .filter_map(|exe| {
            let below = resolved(exe).strip_prefix(&releases).ok()?.to_owned();
            CommitId::new(below.components().next()?.as_os_str().to_str()?).ok()
        })
        .collect())
}

/// Test doubles: a service manager that answers `systemctl show` and `launchctl print`
/// as the test sets it and records every question, and a table of process executables.
#[cfg(test)]
pub(crate) mod fake {
    use std::cell::RefCell;
    use std::io;
    use std::path::PathBuf;
    use std::rc::Rc;

    use kbf_host::testkit::{Answer, reply};

    use super::Procs;

    #[derive(Default)]
    pub struct World {
        /// The unit's `ActiveState`.
        pub active: String,
        pub main_pid: u32,
        /// Executable by pid.
        pub exes: Vec<(u32, PathBuf)>,
        /// Every command asked, as `program arg arg`.
        pub asked: Vec<String>,
        /// The job is loaded (launchd).
        pub loaded: bool,
        /// `executables` cannot be told.
        pub blind: bool,
    }

    /// The service manager and the process table, one handle shared by the host's
    /// answers and the helper's [`Procs`].
    #[derive(Clone, Default)]
    pub struct ServiceRecorder(pub Rc<RefCell<World>>);

    impl ServiceRecorder {
        /// A service that is active with main process `pid`, running `exe`.
        pub fn running(pid: u32, exe: PathBuf) -> ServiceRecorder {
            let r = ServiceRecorder::default();
            {
                let mut w = r.0.borrow_mut();
                w.active = "active".to_owned();
                w.main_pid = pid;
                w.exes = vec![(pid, exe)];
                w.loaded = true;
            }
            r
        }

        pub fn asked(&self) -> Vec<String> {
            self.0.borrow().asked.clone()
        }

        /// The host's command answers: `systemctl show` and `launchctl print`; anything
        /// else (a restart) exits 0.
        pub fn answer(&self) -> Answer {
            let world = self.0.clone();
            Box::new(move |cmd| {
                let line = format!("{} {}", cmd.program.as_str(), cmd.args.join(" "));
                let mut w = world.borrow_mut();
                w.asked.push(line);
                if cmd.program.as_str().ends_with("systemctl") && cmd.args.first().map(String::as_str) == Some("show")
                {
                    let text = format!("ActiveState={}\nMainPID={}\n", w.active, w.main_pid);
                    return Ok(reply(0, &text, ""));
                }
                if cmd.program.as_str().ends_with("launchctl") && cmd.args.first().map(String::as_str) == Some("print")
                {
                    if !w.loaded {
                        return Ok(reply(113, "", "not found"));
                    }
                    let pid = if w.main_pid == 0 {
                        String::new()
                    } else {
                        format!("\tpid = {}\n", w.main_pid)
                    };
                    return Ok(reply(0, &format!("system/x = {{\n\tstate = running\n{pid}}}\n"), ""));
                }
                Ok(reply(0, "", ""))
            })
        }
    }

    impl Procs for ServiceRecorder {
        fn exe_of(&self, pid: u32) -> io::Result<PathBuf> {
            self.0
                .borrow()
                .exes
                .iter()
                .find(|(p, _)| *p == pid)
                .map(|(_, e)| e.clone())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such process"))
        }

        fn executables(&self) -> io::Result<Vec<PathBuf>> {
            let w = self.0.borrow();
            if w.blind {
                return Err(io::Error::other("cannot list processes"));
            }
            Ok(w.exes.iter().map(|(_, e)| e.clone()).collect())
        }
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
