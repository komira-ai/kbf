//! The slots on macOS: each pool user resolved and cleaned at start, a slot taken per
//! lease, the action launched as its user, every process of its uid ended, and the
//! slot cleaned before it goes back ([`crate::slots`] says what a slot gets;
//! [`crate::slots::clean`] how it is cleaned; [`host`] the Mac's part of that;
//! [`crate::slots::book`] which slots are free or dirty).

use std::collections::BTreeSet;
use std::ffi::{CStr, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_daemon::RuntimeError;
use kbf_daemon::uid;
use tokio::process::Child;
use tokio::sync::OwnedSemaphorePermit;

use super::book::{Backoff, Book, Cleaner, CountHook, Held, Slotted};
use super::clean::{self, Mutant, Mutated};
use super::{AS_USER_ARG, Pool, SlotUser, probe};
use crate::network::{Isolation, Network};
use crate::procs;

pub mod host;

/// The program that starts a process in a user's bootstrap namespace.
pub const LAUNCHCTL: &str = kbf_mac_session::macos::gui::LAUNCHCTL;

/// The program that names a user's `/var/folders` entry.
const GETCONF: &str = "/usr/bin/getconf";

/// The program that says whether Spotlight indexes a volume.
const MDUTIL: &str = "/usr/bin/mdutil";

/// One pool user, resolved: its real home and its `/var/folders` entry, each by its
/// real path, and the sandbox rule that lets an action write both.
#[derive(Debug)]
pub struct Ready {
    pub user: SlotUser,
    pub home: PathBuf,
    pub folders: PathBuf,
    /// What the system gave the `/var/folders` entry, as found when the slot was resolved.
    pub entry: Entry,
    rules: String,
}

/// The `/var/folders` entry of a slot as the system made it, found when the slot was
/// resolved, before any lease: what the clean's last step gives its roots back. The
/// install-time record the Mac pool plan has in mind replaces this later; until then a
/// daemon restarted after a lease tampered with the entry would record the tampering,
/// minus a lock or `hidden`, which are never restored.
#[derive(Clone, Debug, Default)]
pub struct Entry {
    /// The user flags of the entry's root.
    pub flags: u32,
    /// Each of [`probe::SKELETON`]'s folders the entry had: its name, group and flags.
    pub folders: Vec<(String, u32, u32)>,
}

impl Slotted for Ready {
    fn uid(&self) -> u32 {
        self.user.uid
    }
}

/// The slots of a root daemon: one per pool user, each free, held by one lease, or
/// dirty ([`crate::slots::book`]).
pub struct Slots {
    book: Arc<Book<Ready>>,
    /// This binary, which a launch runs in its `__as-user` mode.
    program: PathBuf,
}

impl std::fmt::Debug for Slots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slots")
            .field("ready", &self.book.all())
            .field("dirty", &self.book.dirty())
            .finish_non_exhaustive()
    }
}

impl Slots {
    /// Resolves every user of `pool` ([`resolve`]), then cleans each slot
    /// ([`clean::clean`], every process of the uid ended `within` each kill): what a
    /// daemon killed before this one left, running or written, is gone before the
    /// daemon says `Hello`, and a slot that cannot be cleaned starts dirty (not offered,
    /// cleaned again). No uid is killed before the user database has said it is the pool
    /// user's.
    ///
    /// # Errors
    /// A user cannot be resolved, or its home lies in a place the clean sweeps.
    pub fn start(pool: &Pool, within: Duration) -> io::Result<Self> {
        Self::start_mutated(pool, within, None)
    }

    /// [`Slots::start`] with the clean made wrong on purpose when `mutant` is given
    /// ([`clean::Mutant`]): for the mutant tests of `tests/slot_clean.rs` alone. `None`
    /// is [`Slots::start`].
    ///
    /// # Errors
    /// As [`Slots::start`].
    #[doc(hidden)]
    pub fn start_mutated(
        pool: &Pool,
        within: Duration,
        mutant: Option<Mutant>,
    ) -> io::Result<Self> {
        let program = kbf_daemon::keeper::program()?;
        let mut ready = Vec::new();
        for user in pool.users() {
            ready.push(resolve(user, &program)?);
        }
        for slot in &ready {
            // A home in a place the clean sweeps would be swept away with the uid's files.
            let swept = probe::shared_plan().owned;
            if let Some(place) = swept.iter().find(|p| slot.home.starts_with(p)) {
                return Err(io::Error::other(format!(
                    "the home {} of {} is in {}, which the clean sweeps",
                    slot.home.display(),
                    slot.user.name,
                    place.display()
                )));
            }
            warn_if_indexed(&slot.home);
        }
        let cleaner = mac_cleaner(within, mutant);
        Ok(Self::with(ready, cleaner, program))
    }

    /// Slots over `ready`, each cleaned by `cleaner` first: a slot whose clean leaves
    /// it dirty is not free.
    fn with(ready: Vec<Ready>, cleaner: Cleaner<Ready>, program: PathBuf) -> Self {
        Self {
            book: Book::start(ready, cleaner, Backoff::DAEMON),
            program,
        }
    }

    /// How many slots can run a lease: all but the dirty.
    #[must_use]
    pub fn count(&self) -> usize {
        self.book.count()
    }

    /// Calls `hook` with the number of free slots now and whenever it changes: a slot
    /// went dirty, or the recovery thread ([`crate::slots::book`]) cleaned one again.
    /// That is how the node's report follows the slots without waiting for the Xcode
    /// survey to change.
    pub fn on_count(&self, hook: CountHook) {
        self.book.on_count(hook);
    }

    /// A free slot, waiting for one while every slot is held.
    ///
    /// # Errors
    /// None in practice: the semaphore is never closed, and a permit always finds a
    /// free slot.
    pub async fn take(&self) -> Result<Slot, RuntimeError> {
        let (index, permit) = self.book.take().await.map_err(RuntimeError::Failed)?;
        Ok(Slot {
            book: Arc::clone(&self.book),
            index,
            permit: Some(permit),
            program: self.program.clone(),
        })
    }
}

/// The cleaner of a root daemon's slots: [`host::MacSlot`] through [`clean::clean`],
/// wrapped in [`Mutated`] when `mutant` is given (only a test gives one).
fn mac_cleaner(within: Duration, mutant: Option<Mutant>) -> Cleaner<Ready> {
    Arc::new(move |ready: &Ready| {
        let slot = host::MacSlot { ready, within };
        match mutant {
            None => clean::clean(&slot),
            Some(mutant) => clean::clean(&Mutated::new(&slot, mutant)),
        }
    })
}

/// Logs a warning when Spotlight indexes the volume `path` is on: `mdworker` would
/// start as the slot's user to index what a lease wrote, and the kill would race it.
/// Provisioning turns indexing off on the pool's volume (`kbf_volume`).
fn warn_if_indexed(path: &Path) {
    let said = Command::new(MDUTIL)
        .arg("-s")
        .arg(path)
        .env_clear()
        .stdin(Stdio::null())
        .output();
    let said = said.map(|out| String::from_utf8_lossy(&out.stdout).into_owned());
    if !said.as_deref().is_ok_and(|text| text.contains("disabled")) {
        tracing::warn!(
            path = %path.display(),
            ?said,
            "Spotlight may index this pool home's volume"
        );
    }
}

/// One lease's slot. Given back ([`Slot::release`]) or dropped, it is cleaned first and
/// free again only if the clean passed ([`clean::clean`]); a dirty slot stays out of the
/// pool until a clean passes, and the node reports one slot fewer meanwhile.
pub struct Slot {
    book: Arc<Book<Ready>>,
    index: usize,
    /// `None` once the slot has been given back.
    permit: Option<OwnedSemaphorePermit>,
    program: PathBuf,
}

impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slot")
            .field("index", &self.index)
            .field("ready", self.ready())
            .finish_non_exhaustive()
    }
}

impl Slot {
    /// The slot's user, home and folders.
    #[must_use]
    pub fn ready(&self) -> &Ready {
        self.book.ready(self.index)
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
            "asuser".to_owned(),
            uid.to_string(),
            self.program.to_string_lossy().into_owned(),
            AS_USER_ARG.to_owned(),
            uid.to_string(),
            gid.to_string(),
            "--".to_owned(),
            program.to_string_lossy().into_owned(),
        ];
        launch.extend(args);
        Ok((PathBuf::from(LAUNCHCTL), launch))
    }

    /// Makes the lease directory `dir`, and everything in it, the slot user's (no link
    /// followed), so the action can write its inputs, outputs and own directories, and
    /// the directory itself the user's alone (0700, [`super::LEASE_DIR_MODE`]).
    ///
    /// # Errors
    /// An entry could not be read or given to the user.
    pub async fn hand_over(&self, dir: &Path) -> Result<(), RuntimeError> {
        let (uid, gid) = (self.ready().user.uid, self.ready().user.gid);
        let path = dir.to_owned();
        let handed = tokio::task::spawn_blocking(move || super::hand_tree(&path, uid, gid)).await;
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
            seen: Arc::default(),
            armed: true,
        }
    }

    /// Cleans the slot and gives it back ([`clean::clean`]: free again if the clean
    /// passed, dirty if not), once its lease has ended and the clean of its lease
    /// directory has run, whether or not that removed everything (the shared sweep
    /// removes what of the uid is left there, and the lease reports the failed clean).
    pub async fn release(mut self) {
        let Some(back) = self.back() else {
            return;
        };
        if let Err(why) = tokio::task::spawn_blocking(move || back.give()).await {
            tracing::error!(%why, "the clean of a slot did not finish: it is dirty");
        }
    }

    /// What giving the slot back needs, once.
    fn back(&mut self) -> Option<Held<Ready>> {
        let permit = self.permit.take()?;
        Some(self.book.held(self.index, permit))
    }
}

impl Drop for Slot {
    /// A slot dropped without [`Slot::release`] (the daemon dropped the run) is cleaned
    /// and given back on a thread of its own.
    fn drop(&mut self) {
        let Some(back) = self.back() else {
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("kbf-slot-clean".to_owned())
            .spawn(move || back.give());
        if let Err(why) = spawned {
            tracing::error!(%why, "no thread to clean a dropped slot: it is dirty");
        }
    }
}

/// The processes of an action that runs as a slot's user: every process of the uid.
/// Dropped while armed (the daemon dropped the run), it ends them, blocking.
#[derive(Debug)]
pub struct UidWatch {
    uid: u32,
    record: PathBuf,
    /// Every pid of the uid any measurement saw: whose a sandbox denial is.
    seen: Arc<Mutex<BTreeSet<i32>>>,
    armed: bool,
}

impl UidWatch {
    /// Every pid of the uid seen so far.
    #[must_use]
    pub fn seen(&self) -> BTreeSet<i32> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The physical footprint of every process whose effective uid is the slot's, in
    /// bytes.
    ///
    /// # Errors
    /// The process table could not be read.
    pub async fn measure(&self) -> Result<u64, RuntimeError> {
        let (uid, seen) = (self.uid, Arc::clone(&self.seen));
        let used = tokio::task::spawn_blocking(move || {
            let pids = uid::effective(uid)?;
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(pids.iter().copied());
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
    /// the run record. Survivors fail the lease and keep their record; the slot's clean
    /// kills again, and leaves the slot dirty if they still survive.
    ///
    /// # Errors
    /// Processes of the uid survived, or the leader could not be reaped.
    pub async fn end(&mut self, child: &mut Child, wait: Duration) -> Result<(), RuntimeError> {
        let uid = self.uid;
        let ended = tokio::task::spawn_blocking(move || uid::end(uid, wait)).await;
        let ended = ended
            .map_err(|e| e.to_string())
            .and_then(|e| e.map_err(|e| e.to_string()));
        // Ended or not, the drop does not kill again: the slot's clean does.
        self.armed = false;
        if let Err(why) = ended {
            return Err(RuntimeError::Failed(why));
        }
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
        if ended.is_ok() {
            let _ = std::fs::remove_file(&self.record);
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
    let entry = entry_of(&folders)?;
    Ok(Ready {
        user: user.clone(),
        home,
        folders,
        entry,
        rules,
    })
}

/// The `/var/folders` entry `folders` as it is now: its root's flags, and the group and
/// flags of each of [`probe::SKELETON`]'s folders it has.
fn entry_of(folders: &Path) -> io::Result<Entry> {
    let root = std::fs::symlink_metadata(folders)?;
    let mut entry = Entry {
        flags: std::os::macos::fs::MetadataExt::st_flags(&root),
        folders: Vec::new(),
    };
    for name in probe::SKELETON {
        let Ok(meta) = std::fs::symlink_metadata(folders.join(name)) else {
            continue;
        };
        if meta.is_dir() {
            let flags = std::os::macos::fs::MetadataExt::st_flags(&meta);
            entry.folders.push((name.to_owned(), meta.gid(), flags));
        }
    }
    Ok(entry)
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::slots::clean::Outcome;

    /// The pool users of [`bookkeeping`]: no user database behind them.
    fn ready(count: usize) -> Vec<Ready> {
        (0..count)
            .map(|i| Ready {
                user: SlotUser {
                    name: format!("kbf-unit-slot-{i}"),
                    uid: 700 + u32::try_from(i).expect("small"),
                    gid: 700,
                },
                home: PathBuf::from("/nonexistent"),
                folders: PathBuf::from("/nonexistent"),
                entry: Entry::default(),
                rules: String::new(),
            })
            .collect()
    }

    /// Slots over `count` users that no test resolves or kills, cleaned by a stand-in
    /// that answers `outcomes` in turn (then `Clean`) and counts its calls: the slot
    /// hand-over alone (which user is in which slot, what `Debug` says, the permits).
    fn bookkeeping(count: usize, outcomes: Vec<Outcome>) -> (Slots, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let queue = Mutex::new(std::collections::VecDeque::from(outcomes));
        let counted = Arc::clone(&calls);
        let cleaner: Cleaner<Ready> = Arc::new(move |_: &Ready| {
            counted.fetch_add(1, Ordering::SeqCst);
            let next = queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front();
            next.unwrap_or(Outcome::Clean)
        });
        let slots = Slots::with(ready(count), cleaner, PathBuf::from("/nonexistent"));
        (slots, calls)
    }

    fn dirty() -> Outcome {
        Outcome::Dirty {
            why: "test".to_owned(),
        }
    }

    /// Catches a slot handed to a lease that is not the one the book gave (the lease
    /// would run as another uid), a `Debug` that hides which slot is dirty, a dropped
    /// slot that goes back uncleaned, and one that does not go back at all: of two
    /// slots the dirty one is never handed out, and the other, dropped, is cleaned and
    /// free again.
    #[tokio::test]
    async fn a_dirty_slot_is_not_handed_out_and_a_dropped_slot_goes_back_cleaned() {
        // The start: slot 0 dirty, slot 1 clean; the dropped slot 1 clean; the sibling's
        // second look at slot 0 dirty again.
        let (slots, calls) = bookkeeping(2, vec![dirty(), Outcome::Clean, Outcome::Clean, dirty()]);
        assert_eq!(slots.count(), 1);
        let slot = slots.take().await.expect("the other slot");
        assert_eq!(slot.ready().user.uid, 701);
        assert!(format!("{slot:?}").contains("kbf-unit-slot-1"), "{slot:?}");
        assert!(format!("{slots:?}").contains("dirty: {0}"), "{slots:?}");
        drop(slot);
        let slot = tokio::time::timeout(Duration::from_secs(5), slots.take())
            .await
            .expect("a dropped slot goes back once cleaned")
            .expect("taken");
        assert_eq!(slot.ready().user.uid, 701);
        assert!(
            calls.load(Ordering::SeqCst) >= 3,
            "the drop cleaned the slot"
        );
        slot.release().await;
    }

    /// Catches a slot lease handed a simulator (it would be made in root's device set,
    /// which the slot user does not see), and a slot lease refused although it asked
    /// for none.
    #[tokio::test]
    async fn a_slot_lease_is_refused_a_simulator() {
        let (slots, _) = bookkeeping(1, Vec::new());
        let runner = crate::slots::Runner::Slot(slots.take().await.expect("slot"));
        let refused = runner.allows_simulator(true).expect_err("a simulator");
        assert!(refused.to_string().contains("pool user"), "{refused}");
        assert!(runner.allows_simulator(false).is_ok());
        runner.release().await;
    }

    /// Catches a memory watch that measures nothing (a lease past its memory limit
    /// would never be stopped): over this process's own uid, which runs this test, it
    /// measures more than nothing.
    #[tokio::test]
    async fn the_watch_measures_the_uids_processes() {
        let watch = UidWatch {
            uid: rustix::process::geteuid().as_raw(),
            record: PathBuf::from("/nonexistent"),
            seen: Arc::default(),
            armed: false,
        };
        let used = watch.measure().await.expect("measured");
        assert!(used > 1 << 20, "this uid's processes use {used} bytes");
        assert!(
            watch
                .seen()
                .contains(&i32::try_from(std::process::id()).expect("pid")),
            "the measurement saw this process"
        );
    }
}
