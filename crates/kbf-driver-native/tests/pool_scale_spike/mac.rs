//! The real Mac behind [`Mac`]: `dscl` for the records, the file system for the homes and
//! the `/var/folders` entries, `kbf_daemon::uid` for the processes. Every spawn is counted.

use std::ffi::CString;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kbf_daemon::uid;

use crate::block::{
    self, ASIDE, BLOCK, FIRST_UID, HOMES, Home, Mac, aside_index, in_block, index_of,
};
use crate::dscl;
use crate::spawn;

pub const DSCL: &str = "/usr/bin/dscl";
const DSCACHEUTIL: &str = "/usr/bin/dscacheutil";
const FOLDERS: &str = "/private/var/folders";
/// How long ending a user's processes may take.
const END_WITHIN: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct RealMac {
    spawns: AtomicU64,
}

/// What one timed spawn did.
pub struct Timed {
    pub took: Duration,
    pub stdout: usize,
}

impl RealMac {
    /// How many programs this has started.
    pub fn spawns(&self) -> u64 {
        self.spawns.load(Ordering::Relaxed)
    }

    /// `program args` with standard input closed; killed, and an error, if it has not
    /// exited within [`spawn::WITHIN`] (a hung `dscl` fails its phase).
    pub fn run(&self, program: &str, args: &[&str]) -> Result<Output, String> {
        self.spawns.fetch_add(1, Ordering::Relaxed);
        let mut cmd = Command::new(program);
        cmd.args(args);
        spawn::output(cmd, None, spawn::WITHIN)
    }

    /// `program args`, timed from before the spawn to after the output is read.
    pub fn timed(&self, program: &str, args: &[&str]) -> Result<Timed, String> {
        let started = Instant::now();
        let out = self.run(program, args)?;
        Ok(Timed {
            took: started.elapsed(),
            stdout: out.stdout.len(),
        })
    }

    /// stdout of a program that must exit 0.
    fn text(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let out = self.run(program, args)?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!(
                "{program} {args:?}: {} {}{}",
                out.status,
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// Names with their ids, from the two listings (a record without the attribute is in
    /// the first only).
    fn listed(&self, dir: &str, attr: &str) -> Result<Vec<(String, Option<u32>)>, String> {
        let names = dscl::parse_names(&self.text(DSCL, &[".", "-list", dir])?);
        let ids: std::collections::BTreeMap<String, Option<u32>> =
            dscl::parse_list(&self.text(DSCL, &[".", "-list", dir, attr])?)
                .into_iter()
                .collect();
        Ok(names
            .into_iter()
            .map(|n| {
                let id = ids.get(&n).copied().flatten();
                (n, id)
            })
            .collect())
    }

    /// Writes the records of users `indices` through one `dscl .` process that reads its
    /// commands from standard input ([`block::batch_script`]), after making their homes.
    pub fn create_users_in_one_process(&self, indices: &[u32]) -> Result<(), String> {
        for &i in indices {
            self.make_home(&block::user_name(i), block::uid_of(i), FIRST_UID)?;
        }
        self.spawns.fetch_add(1, Ordering::Relaxed);
        let mut cmd = Command::new(DSCL);
        cmd.arg(".");
        let script = block::batch_script(indices).into_bytes();
        let out = spawn::output(cmd, Some(script), spawn::WITHIN)?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if out.status.success() && !text.contains("DS Error") {
            Ok(())
        } else {
            Err(format!("{DSCL} . < script: {} {}", out.status, text.trim()))
        }
    }

    /// The name the user database gives `name`, through this process's own `getpwnam`.
    pub fn getpwnam(name: &str) -> Option<u32> {
        let name = CString::new(name).ok()?;
        // SAFETY: a zeroed passwd is a valid value for getpwnam_r to overwrite.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buffer = vec![0 as libc::c_char; 1 << 12];
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: `name` is NUL-terminated, every other pointer is to a live, writable value
        // of the right type, and `buffer.len()` is the buffer's size.
        let code = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                &raw mut entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                &raw mut found,
            )
        };
        (code == 0 && !found.is_null()).then_some(entry.pw_uid)
    }

    /// The resident size, in KiB, of `opendirectoryd`.
    pub fn opendirectoryd_rss_kb(&self) -> Option<u64> {
        let text = self.text("/bin/ps", &["-axo", "rss=,comm="]).ok()?;
        text.lines().find_map(|line| {
            let (rss, comm) = line.trim().split_once(char::is_whitespace)?;
            comm.trim()
                .ends_with("/opendirectoryd")
                .then(|| rss.parse().ok())?
        })
    }

    /// What the pool's check asks of one user (kbf-mac-provision `pool.rs`,
    /// `user_problems`): six attribute reads, the authority read, two group-membership
    /// tests, the listing `holder` scans for the uid, and `ls -ldn` of the home. Returns
    /// how long it took and how many programs that was.
    pub fn check_user(&self, i: u32) -> Result<(Duration, u64), String> {
        let name = block::user_name(i);
        let record = format!("/Users/{name}");
        let home = block::home_path(&name);
        let before = self.spawns();
        let started = Instant::now();
        for attr in [
            "UniqueID",
            "PrimaryGroupID",
            "UserShell",
            "NFSHomeDirectory",
            "IsHidden",
            "Password",
            "AuthenticationAuthority",
        ] {
            self.timed(DSCL, &[".", "-read", &record, attr])?;
        }
        for group in ["admin", "wheel"] {
            self.timed(
                "/usr/sbin/dseditgroup",
                &["-o", "checkmember", "-m", &name, group],
            )?;
        }
        self.timed(DSCL, &[".", "-list", "/Users", "UniqueID"])?;
        self.timed("/bin/ls", &["-ldn", &home])?;
        Ok((started.elapsed(), self.spawns() - before))
    }
}

fn check_name(name: &str) -> Result<u32, String> {
    index_of(name).ok_or_else(|| format!("{name:?} is not a test user's name"))
}

/// An entry of [`HOMES`] the spike may remove: a test user's name, or what a recreate
/// moved aside of one.
fn check_home_name(name: &str) -> Result<(), String> {
    if index_of(name).is_some() || aside_index(name).is_some() {
        Ok(())
    } else {
        Err(format!("{name:?} is not a test home"))
    }
}

impl Mac for RealMac {
    fn users(&self) -> Result<Vec<(String, Option<u32>)>, String> {
        let users = self.listed("/Users", "UniqueID")?;
        // A listing with no `root` is not a listing of this Mac's users: read nothing
        // rather than conclude there is nothing of ours.
        if !users.iter().any(|(n, id)| n == "root" && *id == Some(0)) {
            return Err(format!(
                "dscl . -list /Users UniqueID did not list root with uid 0 ({} records)",
                users.len()
            ));
        }
        Ok(users)
    }

    fn groups(&self) -> Result<Vec<(String, Option<u32>)>, String> {
        let groups = self.listed("/Groups", "PrimaryGroupID")?;
        if !groups.iter().any(|(n, id)| n == "wheel" && *id == Some(0)) {
            return Err(format!(
                "dscl . -list /Groups PrimaryGroupID did not list wheel with gid 0 ({} records)",
                groups.len()
            ));
        }
        Ok(groups)
    }

    fn attrs(&self, record: &str, wanted: &[&str]) -> Result<Vec<(String, String)>, String> {
        let mut args = vec![".", "-read", record];
        args.extend(wanted);
        let out = self.run(DSCL, &args)?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        // dscl exits non-zero and says `No such key` when the record lacks an attribute;
        // anything else that fails (a record that vanished, the directory not answering)
        // is not "absent".
        if out.status.success() || text.contains("No such key") {
            Ok(dscl::parse_attrs(&text, wanted))
        } else {
            Err(format!("{DSCL} {args:?}: {} {}", out.status, text.trim()))
        }
    }

    fn create(&self, record: &str, attr: Option<(&str, &str)>) -> Result<(), String> {
        let mut args = vec![".", "-create", record];
        if let Some((a, v)) = attr {
            args.extend([a, v]);
        }
        self.text(DSCL, &args).map(drop)
    }

    fn delete(&self, record: &str) -> Result<(), String> {
        let ours = record
            .strip_prefix("/Users/")
            .is_some_and(|n| index_of(n).is_some())
            || record == format!("/Groups/{}", block::GROUP);
        if !ours {
            return Err(format!(
                "refusing to delete {record}: not a record of the spike"
            ));
        }
        self.text(DSCL, &[".", "-delete", record]).map(drop)
    }

    fn homes_dir_is_link(&self) -> Result<bool, String> {
        match std::fs::symlink_metadata(HOMES) {
            Ok(meta) => Ok(meta.file_type().is_symlink()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(format!("{HOMES}: {e}")),
        }
    }

    fn homes(&self) -> Result<Vec<Home>, String> {
        let entries = match std::fs::read_dir(HOMES) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("{HOMES}: {e}")),
        };
        let mut homes = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| format!("{HOMES}: {e}"))?;
            let meta = std::fs::symlink_metadata(entry.path())
                .map_err(|e| format!("{}: {e}", entry.path().display()))?;
            let empty = meta.is_dir()
                && std::fs::read_dir(entry.path()).is_ok_and(|mut d| d.next().is_none());
            homes.push(Home {
                name: entry.file_name().to_string_lossy().into_owned(),
                uid: meta.uid(),
                is_dir: meta.is_dir(),
                empty,
            });
        }
        Ok(homes)
    }

    fn make_homes_dir(&self) -> Result<(), String> {
        let make = || -> std::io::Result<()> {
            std::fs::create_dir_all(HOMES)?;
            std::os::unix::fs::chown(HOMES, Some(0), Some(0))?;
            std::fs::set_permissions(HOMES, std::fs::Permissions::from_mode(0o755))
        };
        make().map_err(|e| format!("{HOMES}: {e}"))
    }

    fn make_home(&self, name: &str, uid: u32, gid: u32) -> Result<(), String> {
        check_name(name)?;
        let home = Path::new(HOMES).join(name);
        let make = || -> std::io::Result<()> {
            std::fs::create_dir(&home)?;
            std::os::unix::fs::chown(&home, Some(uid), Some(gid))?;
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))
        };
        make().map_err(|e| format!("{}: {e}", home.display()))
    }

    fn remove_home(&self, name: &str) -> Result<(), String> {
        check_home_name(name)?;
        let home = Path::new(HOMES).join(name);
        let meta =
            std::fs::symlink_metadata(&home).map_err(|e| format!("{}: {e}", home.display()))?;
        if !meta.is_dir() {
            return Err(format!("{} is not a directory", home.display()));
        }
        if index_of(name).is_none() && meta.uid() != 0 {
            return Err(format!(
                "{} was moved aside but is not root's",
                home.display()
            ));
        }
        if kbf_outputs::remove_tree(&home).is_err() {
            // A flag a lease left (uchg) stops the removal; clear them and try again.
            let _ = self.run(
                "/usr/bin/chflags",
                &["-R", "nouchg,nouappnd", &home.to_string_lossy()],
            );
            kbf_outputs::remove_tree(&home).map_err(|e| format!("{}: {e}", home.display()))?;
        }
        Ok(())
    }

    fn remove_homes_dir(&self) {
        let _ = std::fs::remove_dir(HOMES);
    }

    fn homes_dir_left(&self) -> bool {
        Path::new(HOMES).exists()
    }

    fn folders(&self) -> Result<Vec<(String, u32)>, String> {
        let mut found = Vec::new();
        let top = std::fs::read_dir(FOLDERS).map_err(|e| format!("{FOLDERS}: {e}"))?;
        for first in top {
            let first = first.map_err(|e| format!("{FOLDERS}: {e}"))?.path();
            if !std::fs::symlink_metadata(&first).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            let Ok(second) = std::fs::read_dir(&first) else {
                continue;
            };
            for entry in second.flatten() {
                let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                    continue;
                };
                let aside =
                    meta.uid() == 0 && entry.file_name().to_string_lossy().starts_with(ASIDE);
                if meta.is_dir() && (in_block(meta.uid()) || aside) {
                    found.push((entry.path().to_string_lossy().into_owned(), meta.uid()));
                }
            }
        }
        Ok(found)
    }

    fn remove_folder(&self, path: &str) -> Result<(), String> {
        let p = Path::new(path);
        let shape = p
            .strip_prefix(FOLDERS)
            .is_ok_and(|rest| rest.components().count() == 2);
        let meta = std::fs::symlink_metadata(p).map_err(|e| format!("{path}: {e}"))?;
        let aside = meta.uid() == 0
            && p.file_name()
                .is_some_and(|n| aside_index(&n.to_string_lossy()).is_some());
        if !shape || !meta.is_dir() || !(in_block(meta.uid()) || aside) {
            return Err(format!(
                "refusing to remove {path}: not a directory of a test uid, or moved aside from one"
            ));
        }
        kbf_outputs::remove_tree(p).map_err(|e| format!("{path}: {e}"))
    }

    fn end(&self, uid_: u32) -> Result<(), String> {
        if !(FIRST_UID..FIRST_UID + BLOCK).contains(&uid_) {
            return Err(format!("refusing to end the processes of uid {uid_}"));
        }
        let alive = uid::processes(uid_).map_err(|e| e.to_string())?;
        if alive.is_empty() {
            return Ok(());
        }
        uid::end(uid_, END_WITHIN).map_err(|e| e.to_string())
    }

    fn flush(&self) {
        let _ = self.run(DSCACHEUTIL, &["-flushcache"]);
    }
}
