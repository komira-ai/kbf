//! The `kbf` account `package postinst` makes: the group and system user, the block of
//! subordinate ids rootless `crun` maps for a container's users, and lingering (so
//! `/run/user/<uid>`, the container runtime's run directory, outlives a login session).
//!
//! Each part is planned only when the host differs, so a second run plans nothing. The
//! user is made with a home directory and no login and its directories are not created
//! here (a directory needs the user's ids, which a dry run does not have yet: see
//! [`super::postinst`]).

use std::io::Write;

use kbf_host::exec::{Entry, Exec};
use kbf_host::path::HostPath;
use kbf_host::plan::{Plan, Planner};
use kbf_host::step::{Cmd, Owner, Perms};
use kbf_host::user::{self, GETENT, RunUser};

use super::subid::{self, BLOCK, Need, Range};
use super::{
    GROUPADD, LINGER_DIR, LOGINCTL, NOLOGIN, PackageError, STATE, USER, USERADD, USERMOD, path, say,
};
use crate::linux::host::{inspect, parents, query, stat, tool};

const SUBUID: &str = "/etc/subuid";
const SUBGID: &str = "/etc/subgid";

/// The `kbf` user, if the host has one.
pub fn kbf_user<E: Exec>(exec: &E) -> Result<Option<RunUser>, PackageError> {
    Ok(user::lookup(exec, USER)?)
}

/// Whether the host has a group named `name` (`getent group`: exit 2 is no).
pub(crate) fn group_exists<E: Exec>(exec: &E, name: &str) -> Result<bool, PackageError> {
    let cmd = Cmd::new(tool(GETENT)).arg("group").arg("--").arg(name);
    let out = query(exec, &cmd)?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(2) => Ok(false),
        _ => Err(PackageError::Linux(crate::linux::HostError::Output {
            cmd: cmd.to_string(),
            out: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        })),
    }
}

/// The ranges of `file` (`/etc/subuid` or `/etc/subgid`); none when the file is not there.
pub(crate) fn ranges<E: Exec>(exec: &E, file: &str) -> Result<Vec<Range>, PackageError> {
    let host = path(file);
    match inspect(exec, &host)? {
        None => Ok(Vec::new()),
        Some(Entry::File { bytes, .. }) => {
            Ok(subid::parse(file, &String::from_utf8_lossy(&bytes))?)
        }
        Some(_) => Err(PackageError::Read {
            path: file.into(),
            source: std::io::Error::other("not a regular file"),
        }),
    }
}

/// The two files, with the `usermod` flag that adds a range to each.
pub(crate) const SUBID_FILES: [(&str, &str, &str); 2] = [
    (SUBUID, "--add-subuids", "--del-subuids"),
    (SUBGID, "--add-subgids", "--del-subgids"),
];

/// Plan the group, the user, the subordinate ranges and lingering. `systemd` is whether
/// systemd runs (lingering is then `loginctl`'s; without it the verb writes the file
/// `loginctl` would). Prints what it found, then the plan's steps are the caller's.
pub fn plan<E: Exec>(
    exec: &E,
    systemd: bool,
    root: Owner,
    out: &mut dyn Write,
) -> Result<Plan, PackageError> {
    let mut planner = Planner::new(exec);
    let user = kbf_user(exec)?;
    match &user {
        Some(u) if u.uid == 0 => return Err(PackageError::RootUser),
        Some(u) => say(
            out,
            &format!("user {USER}: uid {} gid {}, kept", u.uid, u.gid),
        )?,
        None => {
            if !group_exists(exec, USER)? {
                planner.run(Cmd::new(tool(GROUPADD)).arg("--system").arg(USER));
            }
            planner.run(
                Cmd::new(tool(USERADD))
                    .arg("--system")
                    .arg("--gid")
                    .arg(USER)
                    .arg("--home-dir")
                    .arg(STATE)
                    .arg("--no-create-home")
                    .arg("--shell")
                    .arg(NOLOGIN)
                    .arg("--comment")
                    .arg("kbf build farm node")
                    .arg(USER),
            );
        }
    }

    let uid = user.as_ref().map(|u| u.uid);
    let mut usermod = Cmd::new(tool(USERMOD));
    let mut adds = false;
    for (file, add, _) in SUBID_FILES {
        match subid::decide(file, &ranges(exec, file)?, USER, uid)? {
            Need::Present => say(out, &format!("{file}: {USER} has a range, kept"))?,
            Need::Add(start) => {
                let span = Range::span(start, BLOCK);
                say(out, &format!("{file}: {USER} gets {span}"))?;
                usermod = usermod.arg(add).arg(span);
                adds = true;
            }
        }
    }
    if adds {
        planner.run(usermod.arg(USER));
    }

    let linger = linger_file()?;
    if stat(exec, &linger)?.is_none() {
        if systemd {
            planner.run(Cmd::new(tool(LOGINCTL)).arg("enable-linger").arg(USER));
        } else {
            parents(exec, &mut planner, &linger)?;
            planner.file(linger, Perms::new(0o644), Some(root), Vec::new())?;
        }
    }
    Ok(planner.finish())
}

/// `/var/lib/systemd/linger/kbf`: the file whose presence is lingering.
pub(crate) fn linger_file() -> Result<HostPath, PackageError> {
    Ok(HostPath::new(&format!("{LINGER_DIR}/{USER}"))?)
}

#[cfg(test)]
#[path = "account_tests.rs"]
mod tests;
