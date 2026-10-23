//! The assertion on a Mac ([`crate::slots::assertion`] says what each probe judges): the
//! readers that run the tools, the warm-up that records the baseline, and the checks the
//! slot's clean and the hand-over to a lease make.
//!
//! - Everything is read as root and **never as the pool user**: no `simctl` or other
//!   CoreSimulator tool runs, because a simulator tool run as the uid makes files appear
//!   in its home a few seconds later, which the assertion would then find.
//! - The baseline is recorded when the daemon starts ([`warm_up`]): the user, once
//!   cleaned, runs `/usr/bin/true` through the launch an action uses, is cleaned again,
//!   and is read. A user that fails to reach a clean baseline is dirty, as one whose
//!   clean fails.
//! - After every clean the whole assertion runs against that baseline ([`after_clean`]);
//!   when the slot is handed to a lease the cheap probes run again
//!   ([`Slot::assert_ready`]), which sees what arrived after the clean's last look.
//! - A red result is the same as a failed clean: the slot is not offered, the differing
//!   lines are logged, and the recovery thread cleans and asserts it again.

use std::io;
use std::ops::RangeInclusive;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rustix::fs::{CWD, Dir, Mode, OFlags};

use kbf_daemon::RuntimeError;
use kbf_daemon::uid;
use kbf_mac_session::sweep::SweepPlan;

use super::host;
use super::{Ready, Slot, launch_command};
use crate::network::{Isolation, Network};
use crate::slots::assertion::{self, Facts, Job, Lines, Probe, Reader, Snapshot};
use crate::slots::clean::Outcome;
use crate::slots::{crash, probe};

const PS: &str = "/bin/ps";
const LS: &str = "/bin/ls";
const ID: &str = "/usr/bin/id";
const DSCL: &str = "/usr/bin/dscl";
const MOUNT: &str = "/sbin/mount";
const HDIUTIL: &str = "/usr/bin/hdiutil";
const IPCS: &str = "/usr/bin/ipcs";
const TRUE: &str = "/usr/bin/true";

/// The directory record's attributes the baseline holds.
const RECORD_ATTRS: [&str; 5] = [
    "RecordName",
    "UniqueID",
    "PrimaryGroupID",
    "NFSHomeDirectory",
    "UserShell",
];

/// How long one tool the readers run may take: a hung `hdiutil` or `launchctl` is a probe
/// that cannot run (red), never a lease or a recovery that waits for ever.
const READ_WITHIN: Duration = Duration::from_secs(60);

/// What `launchctl print` exits with for a target that is not there ("Could not find
/// service"). Any other failure is not "gone".
const NOT_FOUND: i32 = 113;

/// How long the processes probe waits before it looks a second time at a finding.
const LOOK_AGAIN: Duration = Duration::from_millis(500);

/// How long the warm-up command may take.
const WARM_UP_WITHIN: Duration = Duration::from_secs(60);

/// The directory the warm-up's profile is given as its lease directory: root's, and
/// nothing the user can write.
const WARM_UP_LEASE: &str = "/private/var/empty";

/// Reads one user's probes from the Mac.
pub(super) struct MacReader<'a> {
    ready: &'a Ready,
    /// The lease directory, while the slot has one.
    lease: Option<&'a Path>,
    /// launchd's jobs for the uid, read once for the processes and the domain.
    jobs: OnceLock<Result<Vec<Job>, String>>,
}

impl<'a> MacReader<'a> {
    pub(super) fn new(ready: &'a Ready, lease: Option<&'a Path>) -> Self {
        Self {
            ready,
            lease,
            jobs: OnceLock::new(),
        }
    }

    /// The labels of launchd's own services the baseline noted: they are not printed again.
    fn known(&self) -> &[String] {
        self.ready
            .baseline
            .get()
            .and_then(|b| b.lines(Probe::Services))
            .map_or(&[][..], Vec::as_slice)
    }

    fn jobs(&self) -> Result<&[Job], String> {
        self.jobs
            .get_or_init(|| jobs_of(self.ready.user.uid, self.known()))
            .as_deref()
            .map_err(Clone::clone)
    }

    fn roots(&self) -> Vec<&Path> {
        let mut roots = vec![self.ready.home.as_path(), self.ready.folders.as_path()];
        roots.extend(self.lease);
        roots
    }

    /// The processes of the uid that are not launchd's own. A finding is looked at once
    /// more, with the jobs read again: one of launchd's own services that ended between
    /// the process table and the jobs (an idle on-demand daemon) is in the first look
    /// alone, a lease's process is in both.
    fn processes(&self) -> Result<Lines, String> {
        let found = self.processes_by(self.jobs()?)?;
        if found.is_empty() {
            return Ok(found);
        }
        std::thread::sleep(LOOK_AGAIN);
        let jobs = jobs_of(self.ready.user.uid, self.known())?;
        self.processes_by(&jobs)
    }

    fn processes_by(&self, jobs: &[Job]) -> Result<Lines, String> {
        let uid = self.ready.user.uid;
        // The processes first, then the jobs: a process started in between is in
        // neither, never in the first alone.
        let table = tool(PS, &["-axo", "pid=,ruid=,uid=,svuid=,stat=,comm="])?;
        let procs = assertion::parse_ps(&String::from_utf8_lossy(&table))?;
        let own = assertion::own_pids(jobs);
        Ok(assertion::uid_processes(&procs, uid, &own))
    }

    fn ipc(&self) -> Result<Lines, String> {
        let user = &self.ready.user;
        let table = tool(IPCS, &["-a"])?;
        let rows = assertion::ipc_rows(&String::from_utf8_lossy(&table), &user.name, user.uid);
        Ok(rows
            .iter()
            .map(|r| format!("{} {}", r.kind, r.id))
            .collect())
    }

    fn mounts(&self) -> Result<Lines, String> {
        let mounted = String::from_utf8_lossy(&tool(MOUNT, &[])?).into_owned();
        let images = assertion::hdiutil_images(&tool(HDIUTIL, &["info", "-plist"])?)?;
        let user = &self.ready.user;
        Ok(assertion::mount_lines(
            &mounted,
            &images,
            (user.uid, &user.name),
            &self.roots(),
        ))
    }

    fn home(&self) -> Result<Lines, String> {
        let ready = self.ready;
        let facts = facts_of(&ready.home)?;
        Ok(assertion::home_lines(
            &facts,
            (ready.user.uid, ready.user.gid),
        ))
    }

    fn entry(&self) -> Result<Lines, String> {
        let folders = &self.ready.folders;
        let (root, names) = facts_and_names(folders)?;
        let mut children = Vec::new();
        for name in names {
            let path = folders.join(&name);
            let is_dir = std::fs::symlink_metadata(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?
                .is_dir();
            let facts = if is_dir {
                facts_of(&path)?
            } else {
                Facts::default()
            };
            children.push((name, is_dir, facts));
        }
        Ok(assertion::entry_lines(
            &root,
            &children,
            &probe::SKELETON,
            self.ready.user.uid,
        ))
    }

    fn shared(&self) -> Result<Lines, String> {
        let user = &self.ready.user;
        let mut lines = host::owned_entries(&probe::shared_plan(), &user.name, user.uid);
        let scope = crash::Scope {
            uid: user.uid,
            pool: self.ready.pool.clone(),
        };
        lines.extend(crash::found(&host::crash_dirs(), &scope));
        Ok(lines)
    }

    fn schedules(&self) -> Result<Lines, String> {
        let user = &self.ready.user;
        Ok(host::owned_entries(
            &SweepPlan::macos_schedules(),
            &user.name,
            user.uid,
        ))
    }

    fn record(&self) -> Result<Lines, String> {
        let name = &self.ready.user.name;
        let record = format!("/Users/{name}");
        let mut args = vec![".", "-read", record.as_str()];
        args.extend(RECORD_ATTRS);
        let said = tool(DSCL, &args)?;
        let mut lines = assertion::dscl_lines(&String::from_utf8_lossy(&said), &RECORD_ATTRS);
        let id = tool(ID, &[name])?;
        lines.push(format!("id {}", String::from_utf8_lossy(&id).trim()));
        Ok(lines)
    }
}

impl Reader for MacReader<'_> {
    fn read(&self, probe: Probe) -> Result<Lines, String> {
        match probe {
            Probe::Processes => self.processes(),
            Probe::Domain => Ok(assertion::foreign_jobs(self.jobs()?)),
            Probe::Services => Ok(assertion::own_labels(self.jobs()?)),
            Probe::Ipc => self.ipc(),
            Probe::Mounts => self.mounts(),
            Probe::Home => self.home(),
            Probe::Entry => self.entry(),
            Probe::Shared => self.shared(),
            Probe::Schedules => self.schedules(),
            Probe::Record => self.record(),
        }
    }
}

/// The jobs of `user/<uid>`: each service `launchctl print user/<uid>` lists, with whether
/// it is launchd's own. A domain that is not there has none. A launchctl that cannot be
/// run or says anything but "not found", and a listed service that cannot be read and is
/// still listed on a second look, are errors: a job that is not seen is never green.
///
/// A service whose label the baseline noted (`known`) is launchd's own and is not printed
/// again, which keeps the read to the few services that are new. Any other is printed and
/// is launchd's own only if its plist is ([`assertion::own_plist`]).
fn jobs_of(uid: u32, known: &[String]) -> Result<Vec<Job>, String> {
    let domain_target = format!("user/{uid}");
    let Some(domain) = print(&domain_target)? else {
        return Ok(Vec::new());
    };
    let mut jobs = Vec::new();
    for (pid, label) in probe::listed_services(&domain) {
        if known.contains(&label) {
            jobs.push(Job {
                pid,
                label,
                own: true,
            });
            continue;
        }
        let Some(printed) = print(&format!("{domain_target}/{label}"))? else {
            // Gone since the listing, or not readable: the listing says which.
            let again = print(&domain_target)?.unwrap_or_default();
            if probe::listed_services(&again)
                .iter()
                .any(|(_, l)| *l == label)
            {
                return Err(format!("the job {label:?} is listed and cannot be printed"));
            }
            continue;
        };
        let own = probe::plist_path(&printed)
            .is_some_and(|path| assertion::own_plist(path, &label, &probe::OWN_PLISTS, 0));
        jobs.push(Job { pid, label, own });
    }
    Ok(jobs)
}

/// What `launchctl print <target>` says; `None` when the target is not there
/// ([`NOT_FOUND`]).
///
/// # Errors
/// `launchctl` did not run or finish, or exited any other way (a domain being torn down
/// is not an absent one).
fn print(target: &str) -> Result<Option<String>, String> {
    let mut command = Command::new(uid::LAUNCHCTL);
    command.args(["print", target]);
    let (status, stdout, stderr) = run_within(&mut command, READ_WITHIN)
        .map_err(|e| format!("{} print {target}: {e}", uid::LAUNCHCTL))?;
    if status.success() {
        return Ok(Some(String::from_utf8_lossy(&stdout).into_owned()));
    }
    if status.code() == Some(NOT_FOUND) {
        return Ok(None);
    }
    Err(format!(
        "{} print {target}: {status}: {}",
        uid::LAUNCHCTL,
        String::from_utf8_lossy(&stderr).trim()
    ))
}

/// `program`'s stdout, if it exited 0 within [`READ_WITHIN`].
fn tool(program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut command = Command::new(program);
    command.args(args);
    let (status, stdout, stderr) =
        run_within(&mut command, READ_WITHIN).map_err(|e| format!("{program}: {e}"))?;
    if status.success() {
        return Ok(stdout);
    }
    Err(format!(
        "{program} {args:?}: {status}: {}",
        String::from_utf8_lossy(&stderr).trim()
    ))
}

/// Runs `command` with a plain environment and no input; its status and output. It is
/// killed, and the call fails, if it has not finished within `within`.
fn run_within(
    command: &mut Command,
    within: Duration,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), String> {
    let mut child = command
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < within => {
                std::thread::sleep(Duration::from_millis(5));
            }
            other => {
                // The readers end with the pipes; they are not waited for here, as a
                // child of the tool may hold them open.
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("did not finish within {within:?}: {other:?}"));
            }
        }
    };
    let join = |t: std::thread::JoinHandle<Vec<u8>>| t.join().unwrap_or_default();
    Ok((status, join(out), join(err)))
}

/// Reads `pipe` to its end on a thread of its own, so that a tool that prints more than a
/// pipe holds does not block.
fn drain<R: io::Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

/// Opens the directory `path` without following a link in its last component.
fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    rustix::fs::openat(CWD, path, flags, Mode::empty()).map_err(io::Error::from)
}

/// What is read of the directory `path` (not following a link): everything through one
/// descriptor, so that a link put in its place after the open is not followed to list some
/// other directory.
fn facts_of(path: &Path) -> Result<Facts, String> {
    facts_and_names(path).map(|(facts, names)| Facts {
        entries: names,
        ..facts
    })
}

/// [`facts_of`] without the names, and the names of the directory apart.
fn facts_and_names(path: &Path) -> Result<(Facts, Vec<String>), String> {
    let at = |e: io::Error| format!("{}: {e}", path.display());
    let fd = open_dir(path).map_err(at)?;
    let file = std::fs::File::from(fd);
    let meta = file.metadata().map_err(at)?;
    let fd = OwnedFd::from(file);
    let mut names = Vec::new();
    for entry in Dir::read_from(&fd).map_err(|e| at(e.into()))? {
        let entry = entry.map_err(|e| at(e.into()))?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(String::from_utf8_lossy(name).into_owned());
        }
    }
    let listed = tool(LS, &["-led", &path.to_string_lossy()])?;
    let facts = Facts {
        uid: meta.uid(),
        gid: meta.gid(),
        mode: meta.mode() & 0o7777,
        flags: std::os::macos::fs::MetadataExt::st_flags(&meta),
        acl: assertion::acl_entries(&String::from_utf8_lossy(&listed)),
        xattrs: xattrs_of(&fd).map_err(at)?,
        entries: Vec::new(),
    };
    Ok((facts, names))
}

/// The names of the extended attributes of the open directory `fd`.
pub(super) fn xattrs_of(fd: &OwnedFd) -> io::Result<Vec<String>> {
    let raw = fd.as_raw_fd();
    // SAFETY: `raw` is a descriptor `fd` keeps open; a null buffer of size 0 only asks for
    // the size.
    let size = unsafe { libc::flistxattr(raw, std::ptr::null_mut(), 0, 0) };
    let size = usize::try_from(size).map_err(|_| io::Error::last_os_error())?;
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut names = vec![0_u8; size];
    // SAFETY: `names` is a writable buffer of `size` bytes, which the call fills at most.
    let got = unsafe { libc::flistxattr(raw, names.as_mut_ptr().cast(), size, 0) };
    let got = usize::try_from(got).map_err(|_| io::Error::last_os_error())?;
    names.truncate(got);
    Ok(names
        .split(|b| *b == 0)
        .filter(|n| !n.is_empty())
        .map(|n| String::from_utf8_lossy(n).into_owned())
        .collect())
}

/// What the clean of the slot `ready` leaves to assert, after it passed: with a baseline,
/// every probe against it; without one, the warm-up that makes it. `clean` is the slot's
/// own clean, and `program` this binary, which the warm-up launches through.
pub(super) fn after_clean(ready: &Ready, program: &Path, clean: &dyn Fn() -> Outcome) -> Outcome {
    let dirty = |why: String| Outcome::Dirty { why };
    if let Some(baseline) = ready.baseline.get() {
        let reader = MacReader::new(ready, None);
        let red = assertion::assert(&reader, Some(baseline), &assertion::ALL, &|| false);
        return if red.is_empty() {
            Outcome::Clean
        } else {
            dirty(format!("assertion: {}", red.join("; ")))
        };
    }
    match warm_up(ready, program, clean) {
        Ok(()) => Outcome::Clean,
        Err(why) => dirty(format!("baseline: {why}")),
    }
}

/// Records the baseline of `ready`: the user runs `/usr/bin/true` through the launch an
/// action uses, is cleaned by `clean`, and is read. Nothing is recorded unless every
/// probe ran and found the user clean.
fn warm_up(ready: &Ready, program: &Path, clean: &dyn Fn() -> Outcome) -> Result<(), String> {
    let (command, args) = launch_command(
        ready,
        program,
        None,
        &Isolation::detect(),
        Network::Off,
        Path::new(WARM_UP_LEASE),
        TRUE.into(),
        &[],
    )
    .map_err(|e| e.to_string())?;
    let started = Instant::now();
    let mut child = Command::new(&command)
        .args(&args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{}: {e}", command.display()))?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < WARM_UP_WITHIN => {
                std::thread::sleep(Duration::from_millis(5));
            }
            other => {
                let _ = child.kill();
                let _ = uid::end(ready.user.uid, Duration::from_secs(2));
                let _ = child.wait();
                return Err(format!("{TRUE} did not finish: {other:?}"));
            }
        }
    };
    if !status.success() {
        return Err(format!("{TRUE} as the user: {status}"));
    }
    if let Outcome::Dirty { why } = clean() {
        return Err(format!("the clean after {TRUE}: {why}"));
    }
    let snapshot =
        assertion::record(&MacReader::new(ready, None)).map_err(|wrong| wrong.join("; "))?;
    tracing::debug!(
        uid = ready.user.uid,
        took = ?started.elapsed(),
        "baseline recorded"
    );
    // A second record (a clean that ran twice at once) keeps the first.
    let _ = ready.baseline.set(snapshot);
    Ok(())
}

/// The baseline of a slot, once recorded.
pub(super) type Baseline = OnceLock<Snapshot>;

impl Slot {
    /// Runs the cheap probes of the assertion for a lease whose directory is `lease`, as
    /// the slot is handed to it: the processes and launchd's jobs, System V IPC, mounts
    /// and attached images, the home and the `/var/folders` entry, against the baseline.
    /// The lines that differ; empty when the user is as the baseline says. The slot is
    /// then to be set aside ([`Slot::set_aside`]) if there are any. Always empty for slots
    /// started with a step of the clean left out (a test's, [`super::Slots::start_skipping`]),
    /// which are not asserted.
    ///
    /// # Errors
    /// The check did not finish.
    pub async fn assert_ready(&self, lease: &Path) -> Result<Lines, RuntimeError> {
        self.assert_ready_until(lease, &Arc::new(AtomicBool::new(false)))
            .await
    }

    /// [`Slot::assert_ready`] that stops before its next probe once `cancel` is set (a
    /// kill of the lease), and says so.
    ///
    /// # Errors
    /// As [`Slot::assert_ready`].
    pub async fn assert_ready_until(
        &self,
        lease: &Path,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Lines, RuntimeError> {
        if !self.asserted {
            return Ok(Vec::new());
        }
        self.assert_probes(&assertion::CHEAP, Some(lease.to_owned()), cancel)
            .await
    }

    /// Runs every probe of the assertion, as after a clean: for the tests of
    /// `tests/slot_assert.rs`.
    ///
    /// # Errors
    /// As [`Slot::assert_ready`].
    #[doc(hidden)]
    pub async fn assert_everything(&self) -> Result<Lines, RuntimeError> {
        self.assert_probes(&assertion::ALL, None, &Arc::new(AtomicBool::new(false)))
            .await
    }

    async fn assert_probes(
        &self,
        probes: &'static [Probe],
        lease: Option<std::path::PathBuf>,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Lines, RuntimeError> {
        let (book, index) = (Arc::clone(&self.book), self.index);
        let cancel = Arc::clone(cancel);
        let red = tokio::task::spawn_blocking(move || {
            let ready = book.ready(index);
            let reader = MacReader::new(ready, lease.as_deref());
            let stopped = || cancel.load(Ordering::SeqCst);
            assertion::assert(&reader, ready.baseline.get(), probes, &stopped)
        })
        .await;
        red.map_err(|e| RuntimeError::Failed(format!("the assertion did not finish: {e}")))
    }
}

/// The pool's uids of a [`Ready`] that has not been given them.
pub(super) fn no_pool() -> RangeInclusive<u32> {
    0..=0
}
