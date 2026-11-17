//! What root observes about the service and the processes on the host: whether the unit is
//! active, which process is its main one, and which release a process executes from.
//!
//! Two decisions rest on it. A confirm is accepted only if the new release is what runs
//! ([`accept_confirm`]): the unit is active, its main process's executable is inside the
//! pending release's directory, and the first boot check has run. And `gc` never
//! removes a release a process executes from ([`in_use`]). Neither is taken from a record.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use kbf_host::{Cmd, Exec, HostPath};

use crate::CommitId;
use crate::helper::restart::Restart;
use crate::layout::Layout;
use crate::state::PendingConfirm;

/// Executables of the processes on the host.
pub trait Procs {
    /// The executable of process `pid`.
    fn exe_of(&self, pid: u32) -> io::Result<PathBuf>;
    /// The executable of every process whose executable can be read.
    fn executables(&self) -> io::Result<Vec<PathBuf>>;
}

/// The real host: `/proc`.
pub struct HostProcs;

impl Procs for HostProcs {
    fn exe_of(&self, pid: u32) -> io::Result<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/exe"))
    }

    fn executables(&self) -> io::Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            let name = entry?.file_name();
            if name.to_str().is_none_or(|n| n.parse::<u32>().is_err()) {
                continue;
            }
            // A process that exited meanwhile, or whose link root cannot read, has no
            // executable to protect.
            if let Ok(exe) = std::fs::read_link(Path::new("/proc").join(&name).join("exe")) {
                found.push(exe);
            }
        }
        Ok(found)
    }
}

fn query<E: Exec>(exec: &E, program: &str, args: &[&str]) -> Result<std::process::Output, String> {
    let program = HostPath::new(program).expect("an absolute path");
    let cmd = args.iter().fold(Cmd::new(program), |c, a| c.arg(*a));
    exec.query(&cmd)
        .map_err(|e| format!("cannot ask the service manager: {e}"))
}

/// The main process of the service, if the unit is active; otherwise why not.
pub fn main_pid<E: Exec>(exec: &E, restart: &Restart) -> Result<u32, String> {
    let Restart::Systemd(unit) = restart;
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

/// The checks a confirm passes: the first boot check has run, the unit is active, and the
/// executable of its main process is inside the pending release's directory.
pub fn accept_confirm<E: Exec>(
    exec: &E,
    procs: &dyn Procs,
    host: &Layout,
    restart: &Restart,
    pending: &PendingConfirm,
) -> Result<(), String> {
    if pending.deadline_wall.is_none() {
        return Err(format!(
            "{} has not been started since the swap: the first boot check has not run",
            pending.commit
        ));
    }
    let pid = main_pid(exec, restart)?;
    let exe = procs
        .exe_of(pid)
        .map_err(|e| format!("cannot read the executable of process {pid}: {e}"))?;
    let dir = host.join(&host.release_dir(&pending.commit));
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
    let releases = resolved(&host.join(&host.releases()));
    Ok(procs
        .executables()?
        .iter()
        .filter_map(|exe| {
            let below = resolved(exe).strip_prefix(&releases).ok()?.to_owned();
            CommitId::new(below.components().next()?.as_os_str().to_str()?).ok()
        })
        .collect())
}

/// Test doubles: a service manager that answers `systemctl show`
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
            }
            r
        }

        pub fn asked(&self) -> Vec<String> {
            self.0.borrow().asked.clone()
        }

        /// The host's command answers: `systemctl show`; anything else (a restart) exits 0.
        pub fn answer(&self) -> Answer {
            let world = self.0.clone();
            Box::new(move |cmd| {
                let line = format!("{} {}", cmd.program.as_str(), cmd.args.join(" "));
                let mut w = world.borrow_mut();
                w.asked.push(line);
                if cmd.program.as_str().ends_with("systemctl")
                    && cmd.args.first().map(String::as_str) == Some("show")
                {
                    let text = format!("ActiveState={}\nMainPID={}\n", w.active, w.main_pid);
                    return Ok(reply(0, &text, ""));
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
