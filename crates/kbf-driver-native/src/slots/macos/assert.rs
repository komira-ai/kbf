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

use std::ffi::CString;
use std::io;
use std::ops::RangeInclusive;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use kbf_daemon::RuntimeError;
use kbf_daemon::uid;
use kbf_mac_session::sweep::SweepPlan;

use super::host::{self, output};
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

/// How long the warm-up command may take.
const WARM_UP_WITHIN: Duration = Duration::from_secs(60);

/// The directory the warm-up's profile is given as its lease directory: root's, and
/// nothing the user can write.
const WARM_UP_LEASE: &str = "/private/var/empty";

/// `XATTR_NOFOLLOW` of `listxattr(2)`.
const XATTR_NOFOLLOW: libc::c_int = 0x0001;

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

    fn jobs(&self) -> Result<&[Job], String> {
        self.jobs
            .get_or_init(|| jobs_of(self.ready.user.uid))
            .as_deref()
            .map_err(Clone::clone)
    }

    fn roots(&self) -> Vec<&Path> {
        let mut roots = vec![self.ready.home.as_path(), self.ready.folders.as_path()];
        roots.extend(self.lease);
        roots
    }

    fn processes(&self) -> Result<Lines, String> {
        let uid = self.ready.user.uid;
        // The processes first, then the jobs: a process started in between is in
        // neither, never in the first alone.
        let table = output(PS, &["-axo", "pid=,ruid=,uid=,svuid=,stat=,comm="])?;
        let procs = assertion::parse_ps(&String::from_utf8_lossy(&table))?;
        let own = assertion::own_pids(self.jobs()?);
        Ok(assertion::uid_processes(&procs, uid, &own))
    }

    fn ipc(&self) -> Result<Lines, String> {
        let user = &self.ready.user;
        let table = output(IPCS, &["-a"])?;
        let rows = assertion::ipc_rows(&String::from_utf8_lossy(&table), &user.name, user.uid);
        Ok(rows
            .iter()
            .map(|r| format!("{} {}", r.kind, r.id))
            .collect())
    }

    fn mounts(&self) -> Result<Lines, String> {
        let mounted = String::from_utf8_lossy(&output(MOUNT, &[])?).into_owned();
        let images = assertion::hdiutil_images(&output(HDIUTIL, &["info", "-plist"])?)?;
        let uid = self.ready.user.uid;
        Ok(assertion::mount_lines(
            &mounted,
            &images,
            uid,
            &self.roots(),
        ))
    }

    fn home(&self) -> Result<Lines, String> {
        let ready = self.ready;
        let facts = facts_of(&ready.home, true)?;
        Ok(assertion::home_lines(
            &facts,
            (ready.user.uid, ready.user.gid),
        ))
    }

    fn entry(&self) -> Result<Lines, String> {
        let folders = &self.ready.folders;
        let root = facts_of(folders, false)?;
        let mut children = Vec::new();
        let names =
            std::fs::read_dir(folders).map_err(|e| format!("{}: {e}", folders.display()))?;
        for name in names {
            let name = name.map_err(|e| format!("{}: {e}", folders.display()))?;
            let path = name.path();
            let is_dir = std::fs::symlink_metadata(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?
                .is_dir();
            let facts = if is_dir {
                facts_of(&path, true)?
            } else {
                Facts::default()
            };
            children.push((
                name.file_name().to_string_lossy().into_owned(),
                is_dir,
                facts,
            ));
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
        let said = output(DSCL, &args)?;
        let mut lines = assertion::dscl_lines(&String::from_utf8_lossy(&said), &RECORD_ATTRS);
        let id = output(ID, &[name])?;
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
/// its plist is launchd's own. A domain that is not there has none; `launchctl` that
/// cannot be run is an error, and so is a listing no job could be read from.
fn jobs_of(uid: u32) -> Result<Vec<Job>, String> {
    let Some(domain) = print(&format!("user/{uid}"))? else {
        return Ok(Vec::new());
    };
    let mut jobs = Vec::new();
    for (pid, label) in probe::listed_services(&domain) {
        // A service gone since the listing is no job.
        let Some(printed) = print(&format!("user/{uid}/{label}"))? else {
            continue;
        };
        let own = probe::plist_path(&printed).is_some_and(probe::is_own_plist);
        jobs.push(Job { pid, label, own });
    }
    Ok(jobs)
}

/// What `launchctl print <target>` says; `None` when it exits non-zero (the target is not
/// there).
fn print(target: &str) -> Result<Option<String>, String> {
    let out = Command::new(uid::LAUNCHCTL)
        .args(["print", target])
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{} print {target}: {e}", uid::LAUNCHCTL))?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// What is read of the directory `path` (without following a link), its names as well
/// when `entries` says so.
fn facts_of(path: &Path, entries: bool) -> Result<Facts, String> {
    let at = |e: io::Error| format!("{}: {e}", path.display());
    let meta = std::fs::symlink_metadata(path).map_err(at)?;
    let listed = output(LS, &["-led", &path.to_string_lossy()])?;
    let mut names = Vec::new();
    if entries {
        for entry in std::fs::read_dir(path).map_err(at)? {
            names.push(
                entry
                    .map_err(at)?
                    .file_name()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    Ok(Facts {
        uid: meta.uid(),
        gid: meta.gid(),
        mode: meta.mode() & 0o7777,
        flags: std::os::macos::fs::MetadataExt::st_flags(&meta),
        acl: assertion::acl_entries(&String::from_utf8_lossy(&listed)),
        xattrs: xattrs_of(path).map_err(at)?,
        entries: names,
    })
}

/// The names of the extended attributes of `path`, not following a link.
fn xattrs_of(path: &Path) -> io::Result<Vec<String>> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("a path with a NUL"))?;
    // SAFETY: `c` is a NUL-terminated path; a null buffer of size 0 only asks for the size.
    let size = unsafe { libc::listxattr(c.as_ptr(), std::ptr::null_mut(), 0, XATTR_NOFOLLOW) };
    let size = usize::try_from(size).map_err(|_| io::Error::last_os_error())?;
    let mut names = vec![0_u8; size];
    if size == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: `names` is a writable buffer of `size` bytes, which the call fills at most.
    let got =
        unsafe { libc::listxattr(c.as_ptr(), names.as_mut_ptr().cast(), size, XATTR_NOFOLLOW) };
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
        let red = assertion::assert(&reader, Some(baseline), &assertion::ALL);
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
        if !self.asserted {
            return Ok(Vec::new());
        }
        self.assert_probes(&assertion::CHEAP, Some(lease.to_owned()))
            .await
    }

    /// Runs every probe of the assertion, as after a clean: for the tests of
    /// `tests/slot_assert.rs`.
    ///
    /// # Errors
    /// As [`Slot::assert_ready`].
    #[doc(hidden)]
    pub async fn assert_everything(&self) -> Result<Lines, RuntimeError> {
        self.assert_probes(&assertion::ALL, None).await
    }

    async fn assert_probes(
        &self,
        probes: &'static [Probe],
        lease: Option<std::path::PathBuf>,
    ) -> Result<Lines, RuntimeError> {
        let (book, index) = (std::sync::Arc::clone(&self.book), self.index);
        let red = tokio::task::spawn_blocking(move || {
            let ready = book.ready(index);
            let reader = MacReader::new(ready, lease.as_deref());
            assertion::assert(&reader, ready.baseline.get(), probes)
        })
        .await;
        red.map_err(|e| RuntimeError::Failed(format!("the assertion did not finish: {e}")))
    }
}

/// The pool's uids of a [`Ready`] that has not been given them.
pub(super) fn no_pool() -> RangeInclusive<u32> {
    0..=0
}
