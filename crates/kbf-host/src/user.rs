//! Running a tool as another user: who the user is, read from the host while planning,
//! and the switch the tool's process makes before it starts.
//!
//! A root plan that must act as a service user (a tool that reads or fills the kbf user's own files)
//! gives its [`Cmd`] a [`RunUser`]. Between fork and exec the child sets the user's
//! supplementary groups, then its group, then its uid, as `sudo -u` does. The groups
//! come first because each call needs the rights the next one gives up, and they are
//! set explicitly: `std::process::Command::uid` alone clears them (`setgroups(0)`), so
//! the tool would run without the groups a login gives the user.

use std::io;

use crate::exec::{Exec, succeeded};
use crate::path::HostPath;
use crate::step::Cmd;

/// `getent`, which reads the host's user database as a login does (files, LDAP, ...).
pub const GETENT: &str = "/usr/bin/getent";
/// `dscl`, which reads a Mac's directory (a Mac has no `getent`).
pub const DSCL: &str = "/usr/bin/dscl";
/// What `dscl` prints (to stderr, with `DS Error: -14136`) for a user that is not there.
const NOT_FOUND: &str = "eDSRecordNotFound";
/// `id`, for a user's groups (`id -G`).
pub const ID: &str = "/usr/bin/id";

/// A user a command runs as: the uid, the primary gid, every group a login gives the
/// user (the primary one included) and the home directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    pub home: String,
}

/// Why a user could not be looked up.
#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error("{cmd}: {source}")]
    Query { cmd: String, source: io::Error },
    #[error("{cmd} printed {out:?}, which is not what this kit reads")]
    Output { cmd: String, out: String },
}

/// Which tool reads the host's user database. A Mac has no `getent`; its directory is
/// read with `dscl`. The host picks it by its own OS ([`UserDb::native`]); a caller that
/// knows the platform it plans for (the installer's layout, a rehearsal under `--root`)
/// names it, so that a Linux test run exercises the Mac arm through its fake `Exec`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserDb {
    /// `getent passwd`.
    Getent,
    /// `dscl . -read /Users/<name>`.
    Dscl,
}

impl UserDb {
    /// The database of the OS this binary was built for.
    #[cfg(target_os = "macos")]
    pub const fn native() -> Self {
        Self::Dscl
    }

    /// The database of the OS this binary was built for.
    #[cfg(not(target_os = "macos"))]
    pub const fn native() -> Self {
        Self::Getent
    }
}

/// `name` as this host's user database has it: see [`lookup_on`].
pub fn lookup<E: Exec>(exec: &E, name: &str) -> Result<Option<RunUser>, UserError> {
    lookup_on(exec, name, UserDb::native())
}

/// `name` as `db` has it: the uid, gid and home from `getent passwd` or `dscl`, the groups
/// from `id -G`. `None` when there is no such user (`getent` exits 2; `dscl` fails with
/// `eDSRecordNotFound`).
pub fn lookup_on<E: Exec>(exec: &E, name: &str, db: UserDb) -> Result<Option<RunUser>, UserError> {
    let record = match db {
        UserDb::Getent => getent(exec, name)?,
        UserDb::Dscl => dscl(exec, name)?,
    };
    let Some((uid, gid, home)) = record else {
        return Ok(None);
    };
    let id = Cmd::new(tool(ID)).arg("-G").arg("--").arg(name);
    let query = |cmd: &Cmd| {
        succeeded(cmd, exec.query(cmd)?)
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let text = query(&id).map_err(|source| UserError::Query {
        cmd: id.to_string(),
        source,
    })?;
    let groups: Result<Vec<u32>, _> = text.split_whitespace().map(str::parse).collect();
    let groups = groups
        .ok()
        .filter(|g| g.contains(&gid))
        .ok_or(UserError::Output {
            cmd: id.to_string(),
            out: text.clone(),
        })?;
    Ok(Some(RunUser {
        name: name.to_owned(),
        uid,
        gid,
        groups,
        home,
    }))
}

/// The uid, gid and home of `name` from `getent passwd`.
fn getent<E: Exec>(exec: &E, name: &str) -> Result<Option<(u32, u32, String)>, UserError> {
    let getent = Cmd::new(tool(GETENT)).arg("passwd").arg("--").arg(name);
    let out = exec.query(&getent).map_err(|source| UserError::Query {
        cmd: getent.to_string(),
        source,
    })?;
    if out.status.code() == Some(2) {
        return Ok(None);
    }
    let out = succeeded(&getent, out).map_err(|source| UserError::Query {
        cmd: getent.to_string(),
        source,
    })?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let bad = || UserError::Output {
        cmd: getent.to_string(),
        out: text.clone(),
    };
    // name:password:uid:gid:gecos:home:shell
    let fields: Vec<&str> = text.trim_end_matches('\n').split(':').collect();
    let [n, _, uid, gid, _, home, _] = fields[..] else {
        return Err(bad());
    };
    let (Ok(uid), Ok(gid)) = (uid.parse(), gid.parse()) else {
        return Err(bad());
    };
    if n != name {
        return Err(bad());
    }
    Ok(Some((uid, gid, home.to_owned())))
}

/// The uid, gid and home of `name` from `dscl . -read /Users/<name>`. `dscl` prints the
/// attributes in its own order, not the order asked, one `Key: value` per line, so they are
/// read by key. A value `dscl` cannot fit on the line (a home with a space) comes as `Key:`
/// and then one line indented by a space; that is read as the value. Any other indented
/// line, a repeated or unasked key, a missing key and a non-numeric id are refused.
fn dscl<E: Exec>(exec: &E, name: &str) -> Result<Option<(u32, u32, String)>, UserError> {
    let cmd = [
        "UniqueID",
        "PrimaryGroupID",
        "NFSHomeDirectory",
        "UserShell",
    ]
    .into_iter()
    .fold(
        Cmd::new(tool(DSCL))
            .arg(".")
            .arg("-read")
            .arg(format!("/Users/{name}")),
        Cmd::arg,
    );
    let out = exec.query(&cmd).map_err(|source| UserError::Query {
        cmd: cmd.to_string(),
        source,
    })?;
    if !out.status.success()
        && (String::from_utf8_lossy(&out.stderr).contains(NOT_FOUND)
            || String::from_utf8_lossy(&out.stdout).contains(NOT_FOUND))
    {
        return Ok(None);
    }
    let out = succeeded(&cmd, out).map_err(|source| UserError::Query {
        cmd: cmd.to_string(),
        source,
    })?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let bad = || UserError::Output {
        cmd: cmd.to_string(),
        out: text.clone(),
    };
    // dscl prints `Key: value`, or `Key:` and the value on the next line after one space,
    // with LF line ends. Anything else (other whitespace, CR, an empty value) is not its
    // output.
    let exact = |after: &str| {
        let value = after.strip_prefix(' ')?;
        (!value.is_empty() && value.trim() == value).then(|| value.to_owned())
    };
    if text.contains('\r') {
        return Err(bad());
    }
    let (mut uid, mut gid, mut home, mut shell) = (None, None, None, None);
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some((key, value)) = line.split_once(':') else {
            return Err(bad());
        };
        let value = if value.is_empty() {
            lines.next().and_then(exact)
        } else {
            exact(value)
        };
        let Some(value) = value else {
            return Err(bad());
        };
        let slot = match key {
            "UniqueID" => &mut uid,
            "PrimaryGroupID" => &mut gid,
            "NFSHomeDirectory" => &mut home,
            "UserShell" => &mut shell,
            _ => return Err(bad()),
        };
        if slot.replace(value).is_some() {
            return Err(bad());
        }
    }
    let (Some(uid), Some(gid), Some(home), Some(_)) = (uid, gid, home, shell) else {
        return Err(bad());
    };
    let digits = |v: &str| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit());
    let (true, true) = (digits(&uid), digits(&gid)) else {
        return Err(bad());
    };
    let (Ok(uid), Ok(gid)) = (uid.parse(), gid.parse()) else {
        return Err(bad());
    };
    Ok(Some((uid, gid, home)))
}

fn tool(path: &str) -> HostPath {
    HostPath::new(path).expect("a tool's path is absolute")
}

/// What a child runs between fork and exec to become `user`: [`become_user`] on a copy of
/// its ids, made here in the parent so the child allocates nothing.
pub(crate) fn switch_to(user: &RunUser) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    let (uid, gid, groups) = (user.uid, user.gid, user.groups.clone());
    move || become_user(uid, gid, &groups)
}

/// Switch the calling process to `uid`, `gid` and `groups`: the supplementary groups,
/// then the group, then the user. Called in a child between fork and exec, so it makes
/// three system calls and allocates nothing.
pub(crate) fn become_user(uid: u32, gid: u32, groups: &[u32]) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let n: libc::size_t = groups.len();
    #[cfg(not(target_os = "linux"))]
    let n = libc::c_int::try_from(groups.len())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `groups` holds `n` gids for the call to read; it does not keep the pointer.
    if unsafe { libc::setgroups(n, groups.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: setgid and setuid take any number; one the caller may not take fails.
    if unsafe { libc::setgid(gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::setuid(uid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
#[path = "user_tests.rs"]
mod tests;
