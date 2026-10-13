//! The Mac's part of a slot's clean ([`crate::slots::clean`]): each step for one pool
//! user, the check of what is left, and the recreate of the user.
//!
//! - Bootout and kill are [`kbf_daemon::uid`]'s; the bootout then boots out, one by
//!   one, each job `launchctl print user/<uid>` still lists but launchd's own agents,
//!   which every user domain has and the domain's bootout leaves loaded. A job is
//!   launchd's own when its plist lies in the sealed system folders (`path` in
//!   `launchctl print user/<uid>/<label>`, [`OWN_PLISTS`]), never by its label, which a
//!   lease chooses. The crontab, `at` jobs and the shared places are swept by uid with
//!   `kbf_mac_session::sweep`, which never follows a link and stays on one filesystem.
//! - The check reads, never removes. Processes that launchd lists as running one of
//!   its own agents are not counted ([`leftover_processes`]): launchd starts them again
//!   as the uid on demand after the kill.
//! - Not reached: Background Task Management entries (macOS removes none for one uid).
//! - Disk images: every image `hdiutil info -plist` says the uid attached is detached
//!   (`hdiutil detach -force`).
//! - System V IPC: every message queue, shared memory segment and semaphore set
//!   `ipcs` lists with the user as owner or creator is removed (`ipcrm`). POSIX named
//!   semaphores and shared memory are not reached: macOS lists them nowhere.
//! - The home is emptied, whoever owns what is in it, and made the user's again, mode
//!   0700 (made anew if it is gone); every folder of the `/var/folders` entry (`0`,
//!   `C`, `T`, ...) is emptied and kept.
//! - The recreate moves the home and the `/var/folders` entry aside (renamed
//!   `.kbf-recreated-<user>-<time>` beside them, root's, mode 0700, so the uid cannot
//!   reach what they hold; nothing removes them), deletes the user record and makes it
//!   again with the same attributes (`dscl`, then `dscacheutil -flushcache`), makes an
//!   empty home, and resolves the user again: the `/var/folders` entry comes back
//!   through `getconf` as the user, never made by root with the wrong owner.

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_daemon::uid;
use kbf_mac_session::sweep::{self, SweepPlan, Swept};
use rustix::fs::{CWD, Mode, OFlags};

use super::{Ready, resolve};
use crate::slots::clean::{Left, Os, Step};

/// The prefix of the name a recreate moves a home or `/var/folders` entry aside to.
pub const ASIDE: &str = ".kbf-recreated-";

const HDIUTIL: &str = "/usr/bin/hdiutil";
const IPCS: &str = "/usr/bin/ipcs";
const IPCRM: &str = "/usr/bin/ipcrm";
const DSCL: &str = "/usr/bin/dscl";
const DSCACHEUTIL: &str = "/usr/bin/dscacheutil";

/// The wait between the check's two looks.
const LOOK_PAUSE: Duration = Duration::from_millis(200);

/// The user record attributes a recreate keeps, as `dscl` names them. `GeneratedUID`
/// keeps the user's `/var/folders` entry at the same path.
const KEPT: [&str; 8] = [
    "GeneratedUID",
    "RealName",
    "UniqueID",
    "PrimaryGroupID",
    "UserShell",
    "NFSHomeDirectory",
    "IsHidden",
    "Password",
];

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// One slot's user, to clean: `within` bounds the kill, `program` is this binary, which
/// a recreate runs in its `__as-user` mode to resolve the user again.
pub struct MacSlot<'a> {
    pub ready: &'a Ready,
    pub within: Duration,
    pub program: &'a Path,
}

impl Os for MacSlot<'_> {
    fn run(&self, step: Step) -> Result<(), String> {
        let user = &self.ready.user;
        match step {
            Step::Bootout => {
                uid::bootout(user.uid, uid::BOOTOUT_WITHIN);
                // What the domain's bootout leaves loaded, but launchd's own agents.
                let mut failed = Vec::new();
                for service in services_of(user.uid).iter().filter(|s| !s.own) {
                    let target = format!("user/{}/{}", user.uid, service.label);
                    if let Err(why) = run(uid::LAUNCHCTL, &["bootout", &target]) {
                        failed.push(why);
                    }
                }
                joined(failed)
            }
            Step::Schedules => complete(&sweep::sweep(
                &SweepPlan::macos_schedules(),
                &user.name,
                user.uid,
            )),
            Step::Kill => uid::kill_all(user.uid, self.within).map_err(|e| e.to_string()),
            Step::Images => {
                let mut failed = Vec::new();
                for device in images_of(user.uid)? {
                    if let Err(why) = run(HDIUTIL, &["detach", "-force", &device]) {
                        failed.push(why);
                    }
                }
                joined(failed)
            }
            Step::Ipc => {
                let mut failed = Vec::new();
                for (kind, id) in ipc_of(&user.name, user.uid)? {
                    if let Err(why) = run(IPCRM, &[kind.flag(), &id]) {
                        failed.push(why);
                    }
                }
                joined(failed)
            }
            Step::Home => wipe_home(self.ready),
            Step::Folders => {
                let mut failed = Vec::new();
                for folder in folders_in(&self.ready.folders)? {
                    if let Err(why) = complete(&sweep::empty(&folder, user.uid)) {
                        failed.push(why);
                    }
                }
                joined(failed)
            }
            Step::Shared => complete(&sweep::sweep(&shared(), &user.name, user.uid)),
        }
    }

    fn left(&self) -> Vec<Left> {
        let user = &self.ready.user;
        let mut left = Vec::new();
        let mut note = |step, what: String| left.push(Left { step, what });
        // The processes first, then the services: a process started in between is in
        // neither list, never in the first alone.
        let pids = uid::processes(user.uid);
        let services = services_of(user.uid);
        match pids.map(|pids| not_launchds_own(pids, &services)) {
            Ok(pids) => {
                if !pids.is_empty() {
                    note(Step::Kill, format!("processes {:?}", named(&pids)));
                }
            }
            Err(why) => note(Step::Kill, format!("processes: {why}")),
        }
        let jobs: Vec<&str> = (services.iter())
            .filter(|s| !s.own)
            .map(|s| s.label.as_str())
            .collect();
        if !jobs.is_empty() {
            note(
                Step::Bootout,
                format!("jobs in user/{}: {jobs:?}", user.uid),
            );
        }
        for (step, plan) in [
            (Step::Schedules, SweepPlan::macos_schedules()),
            (Step::Shared, shared()),
        ] {
            // Looked at, not swept: a check that removed what it found would hide a
            // skipped step.
            let found = owned_entries(&plan, &user.name, user.uid);
            if !found.is_empty() {
                note(step, format!("{found:?}"));
            }
        }
        match images_of(user.uid) {
            Ok(images) if images.is_empty() => {}
            Ok(images) => note(Step::Images, format!("attached {images:?}")),
            Err(why) => note(Step::Images, why),
        }
        match ipc_of(&user.name, user.uid) {
            Ok(objects) if objects.is_empty() => {}
            Ok(objects) => note(Step::Ipc, format!("{objects:?}")),
            Err(why) => note(Step::Ipc, why),
        }
        if let Err(why) = home_is_empty(self.ready) {
            note(Step::Home, why);
        }
        match folders_in(&self.ready.folders) {
            Ok(folders) => {
                for folder in folders {
                    let entries = list(&folder);
                    if !matches!(&entries, Ok(names) if names.is_empty()) {
                        note(Step::Folders, format!("{}: {entries:?}", folder.display()));
                    }
                }
            }
            Err(why) => note(Step::Folders, why),
        }
        left
    }

    fn pause(&self) {
        std::thread::sleep(LOOK_PAUSE);
    }

    fn recreate(&self) -> Result<(), String> {
        let ready = self.ready;
        let name = &ready.user.name;
        let when = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let aside = format!(
            "{ASIDE}{name}-{}.{:09}",
            when.as_secs(),
            when.subsec_nanos()
        );
        for dir in [&ready.home, &ready.folders] {
            move_aside(dir, ready.user.uid, &aside)
                .map_err(|e| format!("move {} aside: {e}", dir.display()))?;
        }
        let record = format!("/Users/{name}");
        let kept = attributes(&record)?;
        run(DSCL, &[".", "-delete", &record])?;
        run(DSCL, &[".", "-create", &record])?;
        for (attribute, values) in &kept {
            for (i, value) in values.iter().enumerate() {
                let verb = if i == 0 { "-create" } else { "-append" };
                run(DSCL, &[".", verb, &record, attribute, value])?;
            }
        }
        run(DSCACHEUTIL, &["-flushcache"])?;
        let made = make_home(&ready.home, ready.user.uid, ready.user.gid);
        made.map_err(|e| format!("make the home {}: {e}", ready.home.display()))?;
        let again = resolve(&ready.user, self.program).map_err(|e| e.to_string())?;
        if (&again.home, &again.folders) != (&ready.home, &ready.folders) {
            return Err(format!(
                "the user made again resolves to {} and {}, not {} and {}",
                again.home.display(),
                again.folders.display(),
                ready.home.display(),
                ready.folders.display()
            ));
        }
        tracing::warn!(user = %name, aside, "made the slot's user again");
        Ok(())
    }
}

/// The places the shared sweep covers: launchd's per-uid files, and every entry the
/// uid owns in the `Shared` folder under `/Users`, `/private/tmp` and
/// `/private/var/tmp`. Not all of `/private/var/folders`: the uid can write only its
/// own entry there, which [`Step::Folders`] empties.
pub(super) fn shared() -> SweepPlan {
    let mut plan = SweepPlan::macos();
    plan.owned
        .retain(|place| place != Path::new("/private/var/folders"));
    plan
}

/// Whether a sweep was complete.
fn complete(swept: &Swept) -> Result<(), String> {
    joined(swept.errors.clone())
}

fn joined(failed: Vec<String>) -> Result<(), String> {
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("; "))
    }
}

/// Runs `program` with `args` and a plain environment; why it failed, if it did.
fn run(program: &str, args: &[&str]) -> Result<(), String> {
    output(program, args).map(drop)
}

/// `program`'s stdout, if it exited 0.
fn output(program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new(program)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    Err(format!(
        "{program} {args:?}: {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// The devices (`/dev/diskN`) of the disk images `uid` attached, from
/// `hdiutil info -plist`: each image's own entry, whose detach detaches its partitions.
fn images_of(uid: u32) -> Result<Vec<String>, String> {
    let out = output(HDIUTIL, &["info", "-plist"])?;
    let info = plist::Value::from_reader_xml(out.as_slice())
        .map_err(|e| format!("{HDIUTIL} info -plist: {e}"))?;
    let images = info
        .as_dictionary()
        .and_then(|d| d.get("images"))
        .and_then(plist::Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let mut devices = Vec::new();
    for image in images.iter().filter_map(plist::Value::as_dictionary) {
        let owner = image
            .get("owner-uid")
            .and_then(plist::Value::as_unsigned_integer);
        if owner != Some(u64::from(uid)) {
            continue;
        }
        let entities = image
            .get("system-entities")
            .and_then(plist::Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let whole = entities
            .iter()
            .filter_map(plist::Value::as_dictionary)
            .filter_map(|e| e.get("dev-entry").and_then(plist::Value::as_string))
            .min_by_key(|dev| dev.len());
        if let Some(dev) = whole {
            devices.push(dev.to_owned());
        }
    }
    Ok(devices)
}

/// A System V IPC object's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ipc {
    Queue,
    Memory,
    Semaphores,
}

impl Ipc {
    fn flag(self) -> &'static str {
        match self {
            Self::Queue => "-q",
            Self::Memory => "-m",
            Self::Semaphores => "-s",
        }
    }
}

/// The System V IPC objects `ipcs -a` lists with `name` (or `uid`, for a name the
/// user database does not give) as owner or creator: `T ID KEY MODE OWNER GROUP
/// CREATOR CGROUP ...` per line.
fn ipc_of(name: &str, uid: u32) -> Result<Vec<(Ipc, String)>, String> {
    let out = output(IPCS, &["-a"])?;
    let text = String::from_utf8_lossy(&out);
    let uid = uid.to_string();
    let ours = |who: Option<&str>| who.is_some_and(|w| w == name || w == uid);
    let mut objects = Vec::new();
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let kind = match words.first() {
            Some(&"q") => Ipc::Queue,
            Some(&"m") => Ipc::Memory,
            Some(&"s") => Ipc::Semaphores,
            _ => continue,
        };
        let Some(id) = words.get(1).filter(|id| id.parse::<i64>().is_ok()) else {
            continue;
        };
        if ours(words.get(4).copied()) || ours(words.get(6).copied()) {
            objects.push((kind, (*id).to_owned()));
        }
    }
    Ok(objects)
}

/// Empties the home and makes it the user's, mode 0700, with no user flag on it (made
/// anew where it is gone).
fn wipe_home(ready: &Ready) -> Result<(), String> {
    let (uid, gid) = (ready.user.uid, ready.user.gid);
    let mut failed = sweep::empty(&ready.home, uid).errors;
    let fixed = match open_dir(&ready.home) {
        Ok(dir) => own(&dir, uid, (uid, gid, 0o700)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => make_home(&ready.home, uid, gid),
        Err(e) => Err(e),
    };
    if let Err(why) = fixed {
        failed.push(format!("{}: {why}", ready.home.display()));
    }
    joined(failed)
}

/// Whether the home is an empty directory of the user's, mode 0700, without flags.
fn home_is_empty(ready: &Ready) -> Result<(), String> {
    let home = &ready.home;
    let want = (ready.user.uid, ready.user.gid, 0o700, 0);
    let found = rustix::fs::lstat(home)
        .map(|s| {
            (
                s.st_uid,
                s.st_gid,
                u32::from(s.st_mode) & 0o7777,
                s.st_flags,
            )
        })
        .map_err(|e| format!("{}: {e}", home.display()))?;
    if found != want {
        return Err(format!(
            "{}: owner, group, mode and flags {found:?}, not {want:?}",
            home.display()
        ));
    }
    match list(home) {
        Ok(names) if names.is_empty() => Ok(()),
        entries => Err(format!("{}: {entries:?}", home.display())),
    }
}

/// The folders of the `/var/folders` entry `folders` (`0`, `C`, `T`, ...).
fn folders_in(folders: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(folders).map_err(|e| format!("{}: {e}", folders.display()))?;
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", folders.display()))?;
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

/// The names in the directory `path`.
fn list(path: &Path) -> io::Result<Vec<String>> {
    std::fs::read_dir(path)?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect()
}

/// `path`, a directory, opened without following a link.
fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(CWD, path, DIRECTORY, Mode::empty())?)
}

/// Gives the open directory `dir` to `uid`:`gid` with `mode`, first clearing the user
/// flags that would stop that (`uchg`, `uappnd`) if `departing` (the slot's uid) owns
/// it: another owner's lock stays, and the change fails.
fn own(dir: &OwnedFd, departing: u32, (uid, gid, mode): (u32, u32, u32)) -> io::Result<()> {
    let stat = rustix::fs::fstat(dir)?;
    let locks = libc::UF_IMMUTABLE | libc::UF_APPEND;
    if stat.st_uid == departing && stat.st_flags & locks != 0 {
        // SAFETY: a plain call on a descriptor this function borrows.
        let code =
            unsafe { libc::fchflags(std::os::fd::AsRawFd::as_raw_fd(dir), stat.st_flags & !locks) };
        if code != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    rustix::fs::fchown(
        dir,
        Some(rustix::fs::Uid::from_raw(uid)),
        Some(rustix::fs::Gid::from_raw(gid)),
    )?;
    rustix::fs::fchmod(dir, Mode::from_raw_mode(mode as _))?;
    Ok(())
}

/// Makes the home `home`, empty, the user's, mode 0700, in its root-owned parent.
fn make_home(home: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let (Some(parent), Some(name)) = (home.parent(), home.file_name().and_then(|n| n.to_str()))
    else {
        return Err(io::Error::other("not a path to a home"));
    };
    kbf_mac_session::helper::make_home(parent, name, uid, gid)
}

/// Renames the directory `dir` of `departing` to `name` beside it and makes it root's, mode 0700, so
/// the uid that owned it cannot reach what it holds. Nothing to do if it is gone.
fn move_aside(dir: &Path, departing: u32, name: &str) -> io::Result<()> {
    let Some(parent) = dir.parent() else {
        return Err(io::Error::other("no parent"));
    };
    let aside = parent.join(name);
    match std::fs::rename(dir, &aside) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        renamed => renamed?,
    }
    own(&open_dir(&aside)?, departing, (0, 0, 0o700))
}

/// The values of the [`KEPT`] attributes of the user record `record`, from
/// `dscl -plist . -read`.
fn attributes(record: &str) -> Result<Vec<(&'static str, Vec<String>)>, String> {
    let out = output(DSCL, &["-plist", ".", "-read", record])?;
    let read = plist::Value::from_reader_xml(out.as_slice())
        .map_err(|e| format!("{DSCL} -read {record}: {e}"))?;
    let read = read
        .as_dictionary()
        .ok_or_else(|| format!("{DSCL} -read {record}: not a dictionary"))?;
    let mut kept = Vec::new();
    for attribute in KEPT {
        // `dsAttrTypeStandard:UniqueID`, `dsAttrTypeNative:IsHidden`.
        let values = read
            .iter()
            .find_map(|(key, value)| (key.rsplit(':').next() == Some(attribute)).then_some(value));
        let values: Vec<String> = values
            .and_then(plist::Value::as_array)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .filter_map(plist::Value::as_string)
            .map(str::to_owned)
            .collect();
        if !values.is_empty() {
            kept.push((attribute, values));
        }
    }
    if !kept.iter().any(|(a, _)| *a == "UniqueID") {
        return Err(format!("{DSCL} -read {record}: no UniqueID"));
    }
    Ok(kept)
}

/// The processes of `uid`, by effective or real uid ([`uid::processes`]), that a lease
/// could have left: all but those launchd lists as running one of its own agents in
/// `user/<uid>` ([`Service::own`]). launchd starts those again on demand after the kill
/// (`distnoted`, measured on the CI runner), as the uid: fresh processes, holding
/// nothing of a lease.
///
/// # Errors
/// The process table could not be read.
pub fn leftover_processes(uid: u32) -> io::Result<Vec<i32>> {
    let pids = uid::processes(uid)?;
    Ok(not_launchds_own(pids, &services_of(uid)))
}

/// `pids` without those `services` lists as running one of launchd's own agents.
fn not_launchds_own(mut pids: Vec<i32>, services: &[Service]) -> Vec<i32> {
    pids.retain(|pid| !services.iter().any(|s| s.own && s.pid == Some(*pid)));
    pids
}

/// The folders whose plists are launchd's own: on the sealed system volume, or
/// Apple's under `/Library` (Rosetta's agents), neither of which a pool user can write.
const OWN_PLISTS: [&str; 2] = ["/System/", "/Library/Apple/"];

/// One service of a launchd domain, as `launchctl print` lists it.
struct Service {
    /// Its process, while it runs.
    pid: Option<i32>,
    label: String,
    /// Whether it is one of launchd's own per-user agents, which launchd loads into
    /// every user domain, which the domain's bootout leaves loaded, and which it starts
    /// again on demand: its plist is in one of [`OWN_PLISTS`]. A job a lease
    /// bootstrapped or submitted is not, whatever its label: its plist is the lease's
    /// (or it has none).
    own: bool,
}

/// The services `launchctl print user/<uid>` lists: none when the domain is gone (or
/// cannot be printed, which the kill and the other checks then stand in for). Each is
/// printed on its own for its plist's path ([`Service::own`]).
fn services_of(uid: u32) -> Vec<Service> {
    let Some(domain) = launchctl_print(&format!("user/{uid}")) else {
        return Vec::new();
    };
    // `services = {`, one `<pid, 0 when not running> <last exit or -> <label>` per line,
    // then `}`.
    let listed: Vec<(Option<i32>, String)> = domain
        .lines()
        .skip_while(|line| line.trim() != "services = {")
        .skip(1)
        .take_while(|line| line.trim() != "}")
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let label = (*words.last()?).to_owned();
            let pid = (words.first()?.parse().ok()).filter(|pid: &i32| *pid > 0);
            Some((pid, label))
        })
        .collect();
    listed
        .into_iter()
        .map(|(pid, label)| {
            let printed = launchctl_print(&format!("user/{uid}/{label}"));
            let own = printed
                .as_deref()
                .and_then(plist_path)
                .is_some_and(|path| OWN_PLISTS.iter().any(|folder| path.starts_with(folder))) || label.starts_with("com.apple.");
            Service { pid, label, own }
        })
        .collect()
}

/// What `launchctl print <target>` says, if it succeeds.
fn launchctl_print(target: &str) -> Option<String> {
    let out = Command::new(uid::LAUNCHCTL)
        .args(["print", target])
        .env_clear()
        .stdin(Stdio::null())
        .output();
    let out = out.ok().filter(|out| out.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The plist path in what `launchctl print user/<uid>/<label>` says: its `path = `
/// line one level in (deeper ones belong to the job's nested dictionaries).
fn plist_path(printed: &str) -> Option<&str> {
    printed
        .lines()
        .find_map(|line| line.strip_prefix("\tpath = "))
        .map(str::trim_end)
}

/// What a sweep of `plan` for `name` and `uid` would remove, without removing it: the
/// named entries that exist, and each entry `uid` owns below the plan's folders (not
/// looked into: the sweep removes it whole), found as the sweep walks, never through
/// a link, on one filesystem and to [`sweep::MAX_DEPTH`]. A place that cannot be read
/// is listed too, as the sweep would fail there.
fn owned_entries(plan: &SweepPlan, name: &str, uid: u32) -> Vec<String> {
    let mut found = Vec::new();
    for template in &plan.named {
        let path = template
            .replace("{user}", name)
            .replace("{uid}", &uid.to_string());
        if std::fs::symlink_metadata(&path).is_ok() {
            found.push(path);
        }
    }
    for base in &plan.owned {
        if let Ok(meta) = std::fs::symlink_metadata(base)
            && meta.is_dir()
        {
            look(base, meta.dev(), uid, 0, &mut found);
        }
    }
    found
}

/// [`owned_entries`] below the directory `dir`, on device `dev`.
fn look(dir: &Path, dev: u64, uid: u32, depth: usize, found: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(why) => return found.push(format!("{}: {why}", dir.display())),
    };
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(why) => return found.push(format!("{}: {why}", dir.display())),
        };
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            // Gone since the listing.
            continue;
        };
        if meta.dev() != dev {
            continue;
        }
        if meta.uid() == uid {
            found.push(path.display().to_string());
        } else if meta.is_dir() && depth < sweep::MAX_DEPTH {
            look(&path, dev, uid, depth + 1, found);
        }
    }
}

/// Each of `pids` with its name, as libproc gives it (`?` for one that is gone).
fn named(pids: &[i32]) -> Vec<String> {
    pids.iter()
        .map(|&pid| {
            let mut name = [0_u8; 64];
            let size = u32::try_from(name.len()).unwrap_or(0);
            // SAFETY: `name` is a writable buffer of `size` bytes; the call writes at
            // most that many and returns how many it wrote.
            let wrote = unsafe { libc::proc_name(pid, name.as_mut_ptr().cast(), size) };
            let len = usize::try_from(wrote).unwrap_or(0).min(name.len());
            let text = String::from_utf8_lossy(&name[..len]);
            format!("{pid} {}", if text.is_empty() { "?" } else { &text })
        })
        .collect()
}
