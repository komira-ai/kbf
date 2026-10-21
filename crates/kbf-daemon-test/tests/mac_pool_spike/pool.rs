//! The pool users `kbf-pool-<i>` of the spike, made with `dscl` the way kbf-mac-provision
//! makes them (names, uids `first + i`, the primary group id `first` for all of them: what
//! `kbf_driver_native::slots::Pool::provisioned` mandates, which is what `kbf daemon
//! --pool-users N --pool-first-uid FIRST` builds), and removed again.
//!
//! The spike runs on a hosted runner, and on a developer's Mac. It must never touch a pool
//! somebody provisioned: every user and group it makes carries [`MARK`] in its `Comment`,
//! and the sweep before a run removes only what carries it. A `kbf-pool-<n>` user without
//! the mark, or any other user at one of the uids, stops the run before anything is changed.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use kbf_daemon::uid;

/// Where the pool's uids start (and the group id of all of them). The other root tests of
/// the repository take 650 to 679 (kbf-mac-session) and 690 to 789 (kbf-driver-native's
/// pool tests); the daemon needs at least 600.
pub const FIRST_UID: u32 = 790;

/// The comment on every record the spike makes.
const MARK: &str = "kbf-daemon-test-mac-pool-spike";

/// The group the users have as their primary group (a record of its own, as provisioning
/// makes `kbfpool`; another name, so a provisioned group is never this one).
const GROUP: &str = "kbf-pool-spike-group";

/// Where the homes are: a root-owned folder Spotlight does not index (a `.noindex` name),
/// as the daemon wants (it warns about an indexed home), and outside every place the clean
/// of a slot sweeps.
pub const HOMES: &str = "/private/var/kbf-pool-spike-homes.noindex";

const DSCL: &str = "/usr/bin/dscl";

/// One pool user.
#[derive(Clone, Debug)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

/// The users, deleted with their processes, group and homes when dropped.
pub struct Pool {
    pub users: Vec<User>,
}

impl Pool {
    /// Removes what an earlier run left, then makes `count` users from [`FIRST_UID`] and
    /// waits until the user database names them.
    ///
    /// # Panics
    /// When a record the spike did not make is in the way, or a command fails.
    pub fn make(count: u32) -> Self {
        sweep(count);
        std::fs::create_dir_all(HOMES).expect("the homes folder");
        std::fs::set_permissions(HOMES, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let gid = FIRST_UID.to_string();
        let group = format!("/Groups/{GROUP}");
        // The guard exists before the first record: a failure below drops it, which
        // removes whatever was made. `Comment` is set first on every record, so a record
        // that exists is always marked and the next run's sweep can remove it.
        let mut pool = Self { users: Vec::new() };
        for attr in [
            vec![],
            vec!["Comment", MARK],
            vec!["PrimaryGroupID", gid.as_str()],
            vec!["Password", "*"],
        ] {
            dscl(&[&[".", "-create", group.as_str()][..], &attr[..]].concat());
        }
        for i in 0..count {
            let name = format!("kbf-pool-{i}");
            let uid = FIRST_UID + i;
            let home = Path::new(HOMES).join(&name);
            // Pushed first: a failure below still removes what was made.
            pool.users.push(User {
                name: name.clone(),
                uid,
                gid: FIRST_UID,
                home: home.clone(),
            });
            std::fs::create_dir(&home).expect("a home");
            std::os::unix::fs::chown(&home, Some(uid), Some(FIRST_UID)).expect("chown");
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).expect("chmod");
            let record = format!("/Users/{name}");
            let (uid_text, home_text) = (uid.to_string(), home.to_string_lossy().into_owned());
            for attr in [
                vec![],
                vec!["Comment", MARK],
                vec!["UniqueID", uid_text.as_str()],
                vec!["PrimaryGroupID", gid.as_str()],
                vec!["UserShell", "/usr/bin/false"],
                vec!["NFSHomeDirectory", home_text.as_str()],
                vec!["IsHidden", "1"],
                vec!["Password", "*"],
            ] {
                dscl(&[&[".", "-create", record.as_str()][..], &attr[..]].concat());
            }
        }
        let _ = Command::new("/usr/bin/dscacheutil")
            .arg("-flushcache")
            .status();
        for user in &pool.users {
            let give_up = Instant::now() + Duration::from_secs(30);
            while name_of(user.uid).as_deref() != Some(user.name.as_str()) {
                assert!(
                    Instant::now() < give_up,
                    "uid {} is not {} after 30 s",
                    user.uid,
                    user.name
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        pool
    }

    /// `--pool-users`.
    pub fn count(&self) -> usize {
        self.users.len()
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        for user in &self.users {
            remove_user(&user.name, user.uid, &user.home);
        }
        remove_group();
        let _ = Command::new("/usr/bin/dscacheutil")
            .arg("-flushcache")
            .status();
    }
}

/// Ends every process of `uid`, deletes the user `name` and its home. Each step is tried
/// whether the one before it worked; what fails is printed, never raised (this runs while
/// a failed test unwinds).
fn remove_user(name: &str, uid: u32, home: &Path) {
    if let Err(why) = uid::end(uid, Duration::from_secs(10)) {
        eprintln!("SPIKE cleanup: {why}");
    }
    let record = format!("/Users/{name}");
    if let Err(why) = try_dscl(&[".", "-delete", record.as_str()]) {
        eprintln!("SPIKE cleanup: {why}");
    }
    let _ = Command::new("/usr/bin/chflags")
        .args(["-R", "nouchg"])
        .arg(home)
        .status();
    if let Err(why) = std::fs::remove_dir_all(home)
        && why.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("SPIKE cleanup: remove {}: {why}", home.display());
    }
}

fn remove_group() {
    let group = format!("/Groups/{GROUP}");
    if let Err(why) = try_dscl(&[".", "-delete", group.as_str()]) {
        eprintln!("SPIKE cleanup: {why}");
    }
}

/// Removes the users and group an earlier run of the spike left (a killed test binary runs
/// no destructor), after checking that nothing else is in the way.
///
/// # Panics
/// A `kbf-pool-<n>` user that is not the spike's, a user with one of the uids that is not
/// the spike's, or a group with the group id that is not the spike's.
fn sweep(count: u32) {
    let wanted = FIRST_UID..FIRST_UID + count;
    let users = list("/Users", "UniqueID");
    for (name, uid) in &users {
        let is_pool = name
            .strip_prefix("kbf-pool-")
            .is_some_and(|n| n.parse::<u32>().is_ok());
        let in_the_way = is_pool || uid.parse().is_ok_and(|u| wanted.contains(&u));
        assert!(
            !in_the_way || attribute(&format!("/Users/{name}"), "Comment") == MARK,
            "refusing to run: the user {name} (uid {uid}) is in the way and was not made by this \
             test; this is a Mac with a provisioned pool, or another test's users"
        );
    }
    for (name, gid) in list("/Groups", "PrimaryGroupID") {
        assert!(
            gid != FIRST_UID.to_string()
                || attribute(&format!("/Groups/{name}"), "Comment") == MARK,
            "refusing to run: the group {name} has the group id {gid}, and was not made by this test"
        );
    }
    for (name, uid) in users {
        let is_pool = name.starts_with("kbf-pool-");
        if is_pool && attribute(&format!("/Users/{name}"), "Comment") == MARK {
            let uid = uid.parse().expect("a numeric uid");
            eprintln!("SPIKE sweep: {name} (uid {uid}) is left from an earlier run");
            remove_user(&name, uid, &Path::new(HOMES).join(&name));
        }
    }
    if attribute(&format!("/Groups/{GROUP}"), "Comment") == MARK {
        eprintln!("SPIKE sweep: group {GROUP} is left from an earlier run");
        remove_group();
    }
    // A home whose user is gone.
    if let Ok(entries) = std::fs::read_dir(HOMES) {
        for entry in entries.flatten() {
            let _ = Command::new("/usr/bin/chflags")
                .args(["-R", "nouchg"])
                .arg(entry.path())
                .status();
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// `dscl . -list <path> <attr>`: name to value.
fn list(path: &str, attr: &str) -> BTreeMap<String, String> {
    let out = Command::new(DSCL)
        .args([".", "-list", path, attr])
        .output()
        .expect("dscl");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            Some((words.next()?.to_owned(), words.next()?.to_owned()))
        })
        .collect()
}

/// The first value of `attr` of directory record `record`, or empty.
fn attribute(record: &str, attr: &str) -> String {
    let out = Command::new(DSCL)
        .args([".", "-read", record, attr])
        .output()
        .expect("dscl");
    if !out.status.success() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // `Comment: value`, or `Comment:` and the value on the next line.
    text.split_once(':')
        .map(|(_, value)| value.trim().to_owned())
        .unwrap_or_default()
}

/// The name the user database gives `uid`.
fn name_of(uid: u32) -> Option<String> {
    let out = Command::new("/usr/bin/id")
        .args(["-un", &uid.to_string()])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn try_dscl(args: &[&str]) -> Result<(), String> {
    let out = Command::new(DSCL)
        .args(args)
        .output()
        .map_err(|e| format!("dscl {args:?}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "dscl {args:?}: {}{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn dscl(args: &[&str]) {
    try_dscl(args).unwrap_or_else(|why| panic!("{why}"));
}
