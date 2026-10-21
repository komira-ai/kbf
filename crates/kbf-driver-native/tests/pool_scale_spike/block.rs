//! The spike's users and what it may touch: a fixed uid block no other test uses, a name
//! pattern, a marker in the record, and a sweep that removes only what carries all three.
//!
//! Users are `kbf-scale-<i>` for `i < BLOCK`, uid `FIRST_UID + i`, real name [`MARK`],
//! home `HOMES/kbf-scale-<i>`; one group, [`GROUP`], gid `FIRST_UID`. A record is the
//! spike's ("ours") when its name, its uid, its real name and its home all agree with that,
//! or are not yet written (a record a killed run left half made). Anything else that holds
//! a uid or gid of the block, or is named like a test user but disagrees, is "foreign":
//! never changed, never removed, and a reason for [`start`] to refuse to run.
//!
//! The Mac is behind [`Mac`] so that every one of these rules is tested on Linux against a
//! fake; `mac.rs` is the real one.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// The uid of `kbf-scale-0` and the gid of [`GROUP`]. Above every range another test or
/// the lease helper uses (`kbf-mac-session`'s lease uids end at 60000; the other root
/// tests use 650 to 889).
pub const FIRST_UID: u32 = 61_000;
/// How many uids from [`FIRST_UID`] the spike owns.
pub const BLOCK: u32 = 1024;
/// The prefix of every test user's name.
pub const USER_PREFIX: &str = "kbf-scale-";
/// The test group.
pub const GROUP: &str = "kbf-scale-spike";
/// The real name every test user gets, written first.
pub const MARK: &str = "kbf-scale-spike-mark";
/// The directory of the test users' homes; `.noindex` keeps Spotlight (and its
/// `mdworker` as the user) out of it.
pub const HOMES: &str = "/private/var/kbf-scale-homes.noindex";
/// The shell every test user gets, as the pool's users do.
pub const SHELL: &str = "/usr/bin/false";

pub fn user_name(i: u32) -> String {
    format!("{USER_PREFIX}{i}")
}

pub fn uid_of(i: u32) -> u32 {
    FIRST_UID + i
}

pub fn home_path(name: &str) -> String {
    format!("{HOMES}/{name}")
}

pub fn in_block(uid: u32) -> bool {
    (FIRST_UID..FIRST_UID + BLOCK).contains(&uid)
}

/// `i` for `kbf-scale-<i>`, written without a sign or leading zero, `i < BLOCK`.
pub fn index_of(name: &str) -> Option<u32> {
    let digits = name.strip_prefix(USER_PREFIX)?;
    let i: u32 = digits.parse().ok()?;
    (i < BLOCK && i.to_string() == digits).then_some(i)
}

/// An entry of [`HOMES`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Home {
    pub name: String,
    pub uid: u32,
    /// A real directory, not a link or a file.
    pub is_dir: bool,
    pub empty: bool,
}

/// Everything the spike asks of the Mac. Every method that changes the Mac is called only
/// with something [`survey`] or the spike's own creation named.
pub trait Mac {
    /// `/Users` by name, with the uid the record has, if any.
    fn users(&self) -> Result<Vec<(String, Option<u32>)>, String>;
    /// `/Groups` by name, with the gid the record has, if any.
    fn groups(&self) -> Result<Vec<(String, Option<u32>)>, String>;
    /// The attributes of `record` among `wanted` that it has.
    fn attrs(&self, record: &str, wanted: &[&str]) -> Result<Vec<(String, String)>, String>;
    /// `dscl . -create record [attr value]`.
    fn create(&self, record: &str, attr: Option<(&str, &str)>) -> Result<(), String>;
    /// `dscl . -delete record`.
    fn delete(&self, record: &str) -> Result<(), String>;
    /// The entries of [`HOMES`] (none when it is not there).
    fn homes(&self) -> Result<Vec<Home>, String>;
    /// Makes [`HOMES`] (root's, mode 0755) if it is not there.
    fn make_homes_dir(&self) -> Result<(), String>;
    /// Makes `HOMES/name`, the user's, mode 0700.
    fn make_home(&self, name: &str, uid: u32, gid: u32) -> Result<(), String>;
    /// Removes `HOMES/name` and what is in it.
    fn remove_home(&self, name: &str) -> Result<(), String>;
    /// Removes [`HOMES`] when it is empty; nothing when it is not there.
    fn remove_homes_dir(&self);
    /// Whether [`HOMES`] is there.
    fn homes_dir_left(&self) -> bool;
    /// The entries of `/var/folders` (`<xx>/<hash>`) owned by a uid of the block.
    fn folders(&self) -> Result<Vec<(String, u32)>, String>;
    /// Removes one entry [`Mac::folders`] named.
    fn remove_folder(&self, path: &str) -> Result<(), String>;
    /// Ends every process of `uid`.
    fn end(&self, uid: u32) -> Result<(), String>;
    /// Flushes the user database's cache.
    fn flush(&self);
}

/// What is on the Mac, sorted into the spike's and the rest.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Survey {
    /// The indexes of the test users that exist.
    pub users: Vec<u32>,
    /// The test group exists.
    pub group: bool,
    /// Names in [`HOMES`] that are test homes.
    pub homes: Vec<String>,
    /// `/var/folders` entries of test uids, with the uid.
    pub folders: Vec<(String, u32)>,
    /// What holds the block's ids or the spike's names and is not the spike's.
    pub foreign: Vec<String>,
}

/// Reads the Mac and says which records are the spike's. Changes nothing.
///
/// # Errors
/// A directory or file listing could not be read: nothing can be called ours then.
pub fn survey(mac: &dyn Mac) -> Result<Survey, String> {
    let mut s = Survey::default();
    // Uids the rest of the Mac holds (or may hold) in the block: what is owned by them is
    // not the spike's.
    let mut foreign_uids = BTreeSet::new();
    for (name, uid) in mac.users()? {
        let held = uid.filter(|u| in_block(*u));
        let Some(i) = index_of(&name) else {
            if let Some(u) = held {
                s.foreign
                    .push(format!("user {name} holds uid {u} of the test block"));
                foreign_uids.insert(u);
            }
            continue;
        };
        let want = uid_of(i);
        if let Some(u) = uid.filter(|u| *u != want) {
            s.foreign.push(format!(
                "user {name} is named like a test user but has uid {u}, not {want}"
            ));
            foreign_uids.extend(held);
            continue;
        }
        let attrs = mac.attrs(&format!("/Users/{name}"), &["RealName", "NFSHomeDirectory"])?;
        let get = |k: &str| attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        let marked = get("RealName").is_none_or(|v| v == MARK);
        let housed = get("NFSHomeDirectory").is_none_or(|v| v == home_path(&name));
        if marked && housed {
            s.users.push(i);
        } else {
            s.foreign.push(format!(
                "user {name} has the name and uid of a test user but real name {:?} and home {:?}",
                get("RealName"),
                get("NFSHomeDirectory")
            ));
            foreign_uids.insert(want);
        }
    }
    for (name, gid) in mac.groups()? {
        if name == GROUP {
            if gid.is_none_or(|g| g == FIRST_UID) {
                s.group = true;
            } else {
                s.foreign
                    .push(format!("group {name} has gid {gid:?}, not {FIRST_UID}"));
            }
        } else if let Some(g) = gid.filter(|g| in_block(*g)) {
            s.foreign
                .push(format!("group {name} holds gid {g} of the test block"));
        }
    }
    for h in mac.homes()? {
        let Some(i) = index_of(&h.name) else {
            s.foreign
                .push(format!("{HOMES}/{} is not a test home", h.name));
            continue;
        };
        let want = uid_of(i);
        let mine = h.is_dir && (h.uid == want || (h.uid == 0 && h.empty));
        if mine && !foreign_uids.contains(&want) {
            s.homes.push(h.name);
        } else {
            s.foreign.push(format!(
                "{HOMES}/{} is not a directory of uid {want} (uid {}, directory {})",
                h.name, h.uid, h.is_dir
            ));
        }
    }
    for (path, uid) in mac.folders()? {
        if foreign_uids.contains(&uid) {
            s.foreign
                .push(format!("{path} is owned by foreign uid {uid}"));
        } else {
            s.folders.push((path, uid));
        }
    }
    Ok(s)
}

/// What a [`sweep`] did.
#[derive(Debug, Default)]
pub struct Sweep {
    pub users: usize,
    pub homes: usize,
    pub folders: usize,
    pub group: bool,
    /// What it left alone because it is not the spike's.
    pub foreign: Vec<String>,
    /// What it could not remove.
    pub errors: Vec<String>,
    pub took: Duration,
}

/// Removes every record, home and `/var/folders` entry that [`survey`] calls the spike's,
/// ending the processes of a user first (a user whose processes survive is left, with its
/// home), and nothing else.
///
/// # Errors
/// The Mac could not be read.
pub fn sweep(mac: &dyn Mac) -> Result<Sweep, String> {
    let started = Instant::now();
    let s = survey(mac)?;
    let mut out = Sweep {
        foreign: s.foreign.clone(),
        ..Sweep::default()
    };
    let mut kept = BTreeSet::new();
    for &i in &s.users {
        let (name, uid) = (user_name(i), uid_of(i));
        if let Err(why) = mac.end(uid) {
            out.errors.push(format!("end uid {uid}: {why}"));
            kept.insert(name);
        } else if let Err(why) = mac.delete(&format!("/Users/{name}")) {
            out.errors.push(format!("delete {name}: {why}"));
        } else {
            out.users += 1;
        }
    }
    for name in s.homes.iter().filter(|n| !kept.contains(*n)) {
        match mac.remove_home(name) {
            Ok(()) => out.homes += 1,
            Err(why) => out.errors.push(format!("remove home {name}: {why}")),
        }
    }
    let kept_uids: BTreeSet<u32> = kept
        .iter()
        .filter_map(|n| index_of(n))
        .map(uid_of)
        .collect();
    for (path, uid) in s.folders.iter().filter(|(_, u)| !kept_uids.contains(u)) {
        match mac.remove_folder(path) {
            Ok(()) => out.folders += 1,
            Err(why) => out.errors.push(format!("remove {path} (uid {uid}): {why}")),
        }
    }
    if s.group {
        match mac.delete(&format!("/Groups/{GROUP}")) {
            Ok(()) => out.group = true,
            Err(why) => out.errors.push(format!("delete group {GROUP}: {why}")),
        }
    }
    mac.flush();
    mac.remove_homes_dir();
    out.took = started.elapsed();
    Ok(out)
}

/// Removes what an earlier run left, and refuses to go on over anything foreign.
///
/// # Errors
/// The Mac could not be read, something of the spike's could not be removed, or something
/// that is not the spike's holds a name or id it needs.
pub fn start(mac: &dyn Mac) -> Result<Sweep, String> {
    let swept = sweep(mac)?;
    let mut problems = swept.errors.clone();
    problems.extend(swept.foreign.iter().cloned());
    if problems.is_empty() {
        Ok(swept)
    } else {
        Err(format!(
            "refusing to run; nothing foreign was touched: {}",
            problems.join("; ")
        ))
    }
}

/// What is left of the spike's after a sweep, and what could not be told from it.
pub fn leftovers(mac: &dyn Mac) -> Vec<String> {
    let mut left = Vec::new();
    match survey(mac) {
        Err(why) => left.push(format!("cannot look: {why}")),
        Ok(s) => {
            left.extend(s.users.iter().map(|i| format!("user {}", user_name(*i))));
            left.extend(s.group.then(|| format!("group {GROUP}")));
            left.extend(s.homes.iter().map(|h| format!("home {h}")));
            left.extend(s.folders.iter().map(|(f, _)| format!("folder {f}")));
            left.extend(s.foreign.iter().map(|f| format!("foreign (untouched) {f}")));
        }
    }
    left.extend(mac.homes_dir_left().then(|| format!("directory {HOMES}")));
    left
}

/// The group, and the directory the homes go in.
///
/// # Errors
/// A record or directory could not be made.
pub fn prepare(mac: &dyn Mac) -> Result<(), String> {
    mac.make_homes_dir()?;
    let record = format!("/Groups/{GROUP}");
    mac.create(&record, None)?;
    mac.create(&record, Some(("PrimaryGroupID", &FIRST_UID.to_string())))?;
    mac.create(&record, Some(("Password", "*")))
}

/// Makes test user `i` the way the pool's users are made (kbf-mac-provision's `pool.rs`:
/// the record, then `UniqueID`, `PrimaryGroupID`, `UserShell`, `NFSHomeDirectory`,
/// `IsHidden`, `Password`, one `dscl` each), with the home first and the marker as the
/// first attribute, so a run killed at any point leaves something [`survey`] knows.
///
/// # Errors
/// The home or a record could not be made.
pub fn create_user(mac: &dyn Mac, i: u32) -> Result<(), String> {
    let name = user_name(i);
    mac.make_home(&name, uid_of(i), FIRST_UID)?;
    let record = format!("/Users/{name}");
    mac.create(&record, None)?;
    for (attr, value) in &user_attrs(i) {
        mac.create(&record, Some((attr, value)))?;
    }
    Ok(())
}

/// What test user `i`'s record gets after it is made, in the order it is written: the
/// marker first.
pub fn user_attrs(i: u32) -> [(&'static str, String); 7] {
    [
        ("RealName", MARK.to_owned()),
        ("UniqueID", uid_of(i).to_string()),
        ("PrimaryGroupID", FIRST_UID.to_string()),
        ("UserShell", SHELL.to_owned()),
        ("NFSHomeDirectory", home_path(&user_name(i))),
        ("IsHidden", "1".to_owned()),
        ("Password", "*".to_owned()),
    ]
}

/// What [`create_user`] asks of `dscl`, for one `dscl .` process that reads its commands
/// from standard input (the interactive form): the same record, the same attributes in
/// the same order, one command per line. The home is not part of it.
pub fn batch_script(indices: &[u32]) -> String {
    let mut script = String::new();
    for &i in indices {
        let record = format!("/Users/{}", user_name(i));
        script.push_str(&format!("create {record}\n"));
        for (attr, value) in user_attrs(i) {
            script.push_str(&format!("create {record} {attr} {value}\n"));
        }
    }
    script
}

/// Removes everything of the spike's when dropped (also while a panic unwinds), or when
/// [`Guard::finish`] says so. Made before anything is created.
pub struct Guard<'a> {
    mac: &'a dyn Mac,
    armed: bool,
}

impl<'a> Guard<'a> {
    pub fn new(mac: &'a dyn Mac) -> Self {
        Self { mac, armed: true }
    }

    /// The sweep, now.
    pub fn finish(mut self) -> Result<Sweep, String> {
        self.armed = false;
        sweep(self.mac)
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        match sweep(self.mac) {
            Ok(s) => eprintln!(
                "cleanup on drop: {} users, {} homes, {} folders removed; foreign {:?}; errors {:?}",
                s.users, s.homes, s.folders, s.foreign, s.errors
            ),
            Err(why) => eprintln!("cleanup on drop failed: {why}"),
        }
    }
}

#[cfg(test)]
mod tests;
