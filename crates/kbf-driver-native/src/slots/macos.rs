//! The slots on macOS: each pool user resolved at start, a slot taken per lease, the
//! action launched as its user, and every process of its uid ended
//! ([`crate::slots`] says what a slot gets).

use std::collections::BTreeSet;
use std::ffi::{CStr, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_daemon::RuntimeError;
use kbf_daemon::uid;
use tokio::process::Child;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::{AS_USER_ARG, Pool, SlotUser};
use crate::network::{Isolation, Network};
use crate::procs;

/// The program that starts a process in a user's bootstrap namespace.
pub const LAUNCHCTL: &str = kbf_mac_session::macos::gui::LAUNCHCTL;

/// The program that names a user's `/var/folders` entry.
const GETCONF: &str = "/usr/bin/getconf";

/// One pool user, resolved: its real home and its `/var/folders` entry, each by its
/// real path, and the sandbox rule that lets an action write both.
#[derive(Debug)]
pub struct Ready {
    pub user: SlotUser,
    pub home: PathBuf,
    pub folders: PathBuf,
    rules: String,
}

/// The slots of a root daemon: one per pool user, each free or held by one lease.
#[derive(Debug)]
pub struct Slots {
    ready: Arc<Vec<Ready>>,
    free: Arc<Mutex<BTreeSet<usize>>>,
    permits: Arc<Semaphore>,
    /// This binary, which a launch runs in its `__as-user` mode.
    program: PathBuf,
}

impl Slots {
    /// Resolves every user of `pool` ([`resolve`]), then ends every process of each
    /// uid ([`uid::end`], `within` each): whatever a daemon killed before this one
    /// left running is gone before the daemon says `Hello`. No uid is killed before
    /// the user database has said it is the pool user's.
    ///
    /// # Errors
    /// A user cannot be resolved, or processes of a uid survived.
    pub fn start(pool: &Pool, within: Duration) -> io::Result<Self> {
        let program = kbf_daemon::keeper::program()?;
        let mut ready = Vec::new();
        for user in pool.users() {
            ready.push(resolve(user, &program)?);
        }
        for user in pool.users() {
            uid::end(user.uid, within).map_err(io::Error::other)?;
        }
        let count = ready.len();
        Ok(Self {
            ready: Arc::new(ready),
            free: Arc::new(Mutex::new((0..count).collect())),
            permits: Arc::new(Semaphore::new(count)),
            program,
        })
    }

    /// How many slots there are.
    #[must_use]
    pub fn count(&self) -> usize {
        self.ready.len()
    }

    /// A free slot, waiting for one while every slot is held.
    ///
    /// # Errors
    /// None in practice: the semaphore is never closed, and a permit always finds a
    /// free slot.
    pub async fn take(&self) -> Result<Slot, RuntimeError> {
        let permit = Arc::clone(&self.permits).acquire_owned().await;
        let permit = permit.map_err(|e| RuntimeError::Failed(format!("slots: {e}")))?;
        let index = lock(&self.free).pop_first();
        let index = index.ok_or_else(|| RuntimeError::Failed("slots: none free".to_owned()))?;
        Ok(Slot {
            ready: Arc::clone(&self.ready),
            free: Arc::clone(&self.free),
            index,
            permit: Some(permit),
            healthy: Arc::new(AtomicBool::new(true)),
            program: self.program.clone(),
        })
    }
}

/// One lease's slot. Dropped, it is free again, unless ending its processes failed:
/// then it stays out of the pool (one slot fewer until the daemon restarts) and is
/// logged, so no lease runs beside what survived.
#[derive(Debug)]
pub struct Slot {
    ready: Arc<Vec<Ready>>,
    free: Arc<Mutex<BTreeSet<usize>>>,
    index: usize,
    permit: Option<OwnedSemaphorePermit>,
    healthy: Arc<AtomicBool>,
    program: PathBuf,
}

impl Slot {
    /// The slot's user, home and folders.
    #[must_use]
    pub fn ready(&self) -> &Ready {
        &self.ready[self.index]
    }

    /// The program and arguments that run `program` with `args` as the slot's user:
    /// `launchctl asuser <uid> <this binary> __as-user <uid> <gid> -- ` and then the
    /// sandbox's wrap ([`Isolation::wrap_slot`]), whose write rules add the slot's home
    /// and folders.
    ///
    /// # Errors
    /// The slot's uid is 0 (never, past [`Pool::new`]: checked again here, where the
    /// action would start).
    pub fn launch(
        &self,
        isolation: &Isolation,
        network: Network,
        lease: &Path,
        program: PathBuf,
        args: &[String],
    ) -> Result<(PathBuf, Vec<String>), RuntimeError> {
        let ready = self.ready();
        let (uid, gid) = (ready.user.uid, ready.user.gid);
        if uid == 0 {
            return Err(RuntimeError::Failed(
                "refusing to run an action as root".to_owned(),
            ));
        }
        let (program, args) = isolation.wrap_slot(network, lease, &ready.rules, program, args);
        let mut launch = vec![
            AS_USER_ARG.to_owned(),
            uid.to_string(),
            gid.to_string(),
            "--".to_owned(),
            program.to_string_lossy().into_owned(),
        ];
        launch.extend(args);
        Ok((self.program.clone(), launch))
    }

    /// Makes the lease directory `dir`, and everything in it, the slot user's (no link
    /// followed), so the action can write its inputs, outputs and own directories.
    ///
    /// # Errors
    /// An entry could not be read or given to the user.
    pub async fn hand_over(&self, dir: &Path) -> Result<(), RuntimeError> {
        let (uid, gid) = (self.ready().user.uid, self.ready().user.gid);
        let path = dir.to_owned();
        let handed = tokio::task::spawn_blocking(move || chown_tree(&path, uid, gid)).await;
        handed
            .map_err(io::Error::other)
            .and_then(|done| done)
            .map_err(|e| RuntimeError::Failed(format!("hand {} over: {e}", dir.display())))
    }

    /// The watch over the action's processes: every process of the slot's uid.
    #[must_use]
    pub fn watch(&self, record: PathBuf) -> UidWatch {
        UidWatch {
            uid: self.ready().user.uid,
            record,
            healthy: Arc::clone(&self.healthy),
            armed: true,
        }
    }

    /// Gives the slot back, once its lease has ended and the clean of its lease
    /// directory has run, whether or not that removed everything (what is left lies in
    /// that lease's directory, and the lease reports the failed clean). Step 4 of
    /// docs/design/macos-end-state.md cleans the slot here first (its home,
    /// `/var/folders` entry, keychains, crontab, `at` jobs, disk images and IPC
    /// objects, then the post-clean check), and keeps a slot whose clean failed out of
    /// the pool; today the slot goes back as it is.
    pub fn release(self) {
        drop(self);
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let permit = self.permit.take();
        if self.healthy.load(Ordering::SeqCst) {
            lock(&self.free).insert(self.index);
            drop(permit);
        } else {
            let uid = self.ready().user.uid;
            tracing::error!(
                uid,
                "processes of the slot survived: the slot stays out of the pool"
            );
            if let Some(permit) = permit {
                permit.forget();
            }
        }
    }
}

/// The processes of an action that runs as a slot's user: every process of the uid.
/// Dropped while armed (the daemon dropped the run), it ends them, blocking.
#[derive(Debug)]
pub struct UidWatch {
    uid: u32,
    record: PathBuf,
    healthy: Arc<AtomicBool>,
    armed: bool,
}

impl UidWatch {
    /// The physical footprint of every process whose effective uid is the slot's, in
    /// bytes.
    ///
    /// # Errors
    /// The process table could not be read.
    pub async fn measure(&self) -> Result<u64, RuntimeError> {
        let uid = self.uid;
        let used = tokio::task::spawn_blocking(move || {
            let pids = uid::effective(uid)?;
            Ok::<_, io::Error>(
                pids.into_iter()
                    .filter_map(procs::footprint)
                    .fold(0_u64, u64::saturating_add),
            )
        })
        .await;
        used.map_err(io::Error::other)
            .and_then(|used| used)
            .map_err(|e| RuntimeError::Failed(format!("memory of uid {uid}: {e}")))
    }

    /// Ends every process of the uid ([`uid::end`], `wait` at most), then reaps the
    /// leader, `child`: once nothing of the uid is left the leader has exited too, or
    /// it is `launchctl`, which exits once its child has; one still there after `wait`
    /// is SIGKILLed by pid, safe because it is this process's unreaped child. Removes
    /// the run record. Survivors fail the lease, keep their record and keep the slot
    /// out of the pool.
    ///
    /// # Errors
    /// Processes of the uid survived, or the leader could not be reaped.
    pub async fn end(&mut self, child: &mut Child, wait: Duration) -> Result<(), RuntimeError> {
        let uid = self.uid;
        let ended = tokio::task::spawn_blocking(move || uid::end(uid, wait)).await;
        let ended = ended
            .map_err(|e| e.to_string())
            .and_then(|e| e.map_err(|e| e.to_string()));
        if let Err(why) = ended {
            self.healthy.store(false, Ordering::SeqCst);
            return Err(RuntimeError::Failed(why));
        }
        self.armed = false;
        let _ = std::fs::remove_file(&self.record);
        if tokio::time::timeout(wait, child.wait()).await.is_err() {
            tracing::warn!(
                uid,
                "the action's leader outlived its uid's processes; killing it"
            );
            let _ = child.start_kill();
        }
        child
            .wait()
            .await
            .map(drop)
            .map_err(|e| RuntimeError::Failed(format!("reap the action: {e}")))
    }
}

impl Drop for UidWatch {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let ended = uid::end(self.uid, kbf_daemon::keeper::UID_WITHIN);
        match &ended {
            Ok(()) => {
                let _ = std::fs::remove_file(&self.record);
            }
            Err(_) => self.healthy.store(false, Ordering::SeqCst),
        }
        tracing::warn!(
            uid = self.uid,
            ?ended,
            "run dropped; ended the slot's processes"
        );
    }
}

/// `user` resolved: its uid must carry its name in the user database, its home must
/// be a directory of its own, and `getconf DARWIN_USER_DIR`, run as the user through
/// the same launch as an action ([`program`] in its `__as-user` mode), names its
/// `/var/folders` entry (`.../0/`, whose parent holds `0`, `C` and `T`).
///
/// # Errors
/// Any of that fails, or a path holds a character the sandbox profile cannot quote.
fn resolve(user: &SlotUser, program: &Path) -> io::Result<Ready> {
    let (name, home) = passwd(user.uid)?;
    if name != user.name {
        return Err(io::Error::other(format!(
            "uid {} is {name:?} in the user database, not the pool user {}",
            user.uid, user.name
        )));
    }
    let home = std::fs::canonicalize(&home)
        .map_err(|e| io::Error::other(format!("home {} of {name}: {e}", home.display())))?;
    let owner = std::fs::symlink_metadata(&home)?;
    if !owner.is_dir() || owner.uid() != user.uid {
        return Err(io::Error::other(format!(
            "home {} of {name} is not a directory of uid {}",
            home.display(),
            user.uid
        )));
    }
    let out = Command::new(LAUNCHCTL)
        .arg("asuser")
        .arg(user.uid.to_string())
        .arg(program)
        .arg(AS_USER_ARG)
        .arg(user.uid.to_string())
        .arg(user.gid.to_string())
        .args(["--", GETCONF, "DARWIN_USER_DIR"])
        .env_clear()
        .stdin(Stdio::null())
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let dir = Path::new(text.trim());
    let parent = dir
        .parent()
        .filter(|_| out.status.success() && dir.is_absolute());
    let folders = parent
        .map(std::fs::canonicalize)
        .transpose()?
        .ok_or_else(|| {
            io::Error::other(format!(
                "{GETCONF} DARWIN_USER_DIR as {name}: {} {:?} {}",
                out.status,
                text.trim(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        })?;
    let rules = format!(
        "(allow file-write* (subpath {}) (subpath {}))\n",
        quoted(&home)?,
        quoted(&folders)?
    );
    Ok(Ready {
        user: user.clone(),
        home,
        folders,
        rules,
    })
}

/// `path` as a sandbox profile string literal.
fn quoted(path: &Path) -> io::Result<String> {
    let text = path.to_str().unwrap_or("\"");
    if text.contains(['"', '\\']) {
        return Err(io::Error::other(format!(
            "{} cannot be named in a sandbox profile",
            path.display()
        )));
    }
    Ok(format!("\"{text}\""))
}

/// The name and home directory the user database gives `uid`.
fn passwd(uid: u32) -> io::Result<(String, PathBuf)> {
    // SAFETY: a zeroed passwd is a valid value for getpwuid_r to overwrite.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 1 << 16];
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is to a live, writable value of the right type, and
    // `buffer.len()` is the buffer's size.
    let code = unsafe {
        libc::getpwuid_r(
            uid,
            &raw mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &raw mut found,
        )
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code));
    }
    if found.is_null() || entry.pw_name.is_null() || entry.pw_dir.is_null() {
        return Err(io::Error::other(format!("no user has uid {uid}")));
    }
    // SAFETY: getpwuid_r found the entry: both are NUL-terminated strings in `buffer`,
    // which lives until the copies are made.
    let (name, dir) = unsafe { (CStr::from_ptr(entry.pw_name), CStr::from_ptr(entry.pw_dir)) };
    Ok((
        name.to_string_lossy().into_owned(),
        PathBuf::from(OsStr::from_bytes(dir.to_bytes())),
    ))
}

/// Gives `root` and everything under it to `uid`:`gid`, never following a link.
fn chown_tree(root: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let mut pending: Vec<PathBuf> = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        std::os::unix::fs::lchown(&path, Some(uid), Some(gid))?;
        if std::fs::symlink_metadata(&path)?.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        }
    }
    Ok(())
}

fn lock(free: &Mutex<BTreeSet<usize>>) -> std::sync::MutexGuard<'_, BTreeSet<usize>> {
    // Each update is one set operation, so the set is consistent after a panic.
    free.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slots over `count` users that no test resolves or kills: the slot bookkeeping
    /// alone (which slot is free, how many permits), with no user database behind it.
    fn bookkeeping(count: usize) -> Slots {
        let ready = (0..count)
            .map(|i| Ready {
                user: SlotUser {
                    name: format!("kbf-unit-slot-{i}"),
                    uid: 700 + u32::try_from(i).expect("small"),
                    gid: 700,
                },
                home: PathBuf::from("/nonexistent"),
                folders: PathBuf::from("/nonexistent"),
                rules: String::new(),
            })
            .collect();
        Slots {
            ready: Arc::new(ready),
            free: Arc::new(Mutex::new((0..count).collect())),
            permits: Arc::new(Semaphore::new(count)),
            program: PathBuf::from("/nonexistent"),
        }
    }

    /// Catches a slot whose processes survived going back to the pool anyway (the next
    /// lease would run beside them), and one given back healthy that does not: a
    /// healthy slot is taken again at once, the survivor's is never.
    #[tokio::test]
    async fn a_slot_whose_kill_failed_stays_out_of_the_pool() {
        let slots = bookkeeping(1);
        let slot = slots.take().await.expect("the only slot");
        assert_eq!(slot.ready().user.uid, 700);
        slot.release();
        let slot = tokio::time::timeout(Duration::from_secs(5), slots.take())
            .await
            .expect("a healthy slot goes back")
            .expect("taken");
        slot.healthy.store(false, Ordering::SeqCst);
        slot.release();
        let again = tokio::time::timeout(Duration::from_millis(300), slots.take()).await;
        assert!(again.is_err(), "a slot whose kill failed was taken again");
        assert!(lock(&slots.free).is_empty(), "it is back in the free set");
    }

    /// Catches a slot lease handed a simulator (it would be made in root's device set,
    /// which the slot user does not see), and a slot lease refused although it asked
    /// for none.
    #[tokio::test]
    async fn a_slot_lease_is_refused_a_simulator() {
        let slots = bookkeeping(1);
        let runner = crate::slots::Runner::Slot(slots.take().await.expect("slot"));
        let refused = runner.allows_simulator(true).expect_err("a simulator");
        assert!(refused.to_string().contains("pool user"), "{refused}");
        assert!(runner.allows_simulator(false).is_ok());
        runner.release();
    }

    /// Catches a memory watch that measures nothing (a lease past its memory limit
    /// would never be stopped): over this process's own uid, which runs this test, it
    /// measures more than nothing.
    #[tokio::test]
    async fn the_watch_measures_the_uids_processes() {
        let watch = UidWatch {
            uid: rustix::process::geteuid().as_raw(),
            record: PathBuf::from("/nonexistent"),
            healthy: Arc::new(AtomicBool::new(true)),
            armed: false,
        };
        let used = watch.measure().await.expect("measured");
        assert!(used > 1 << 20, "this uid's processes use {used} bytes");
    }
}
