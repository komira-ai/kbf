//! Rule `traverse`, moved from `kbf-deploy linux check-units`: the directory
//! `--hot-store=file://` names can be reached by the user kbf runs as. Every directory
//! above it that exists must give that user the execute bit, as its owner, through one of
//! its groups, or as other (the kernel's order: the first class that matches decides).
//! One that does not is how a `storage-and-daemon` unit, running as `kbf`, failed to start
//! with `--hot-store /var/lib/kbf-server/hot: Permission denied`: the parent is the server
//! account's, mode 0700.
//!
//! check-units judged the user named by the unit's `User=`; kbf judges its own process
//! (`geteuid`, `getegid`, `getgroups`), which `ExecStartPre=` runs as that user. Read from
//! `lstat` (a directory that is a symbolic link is not followed, so it is not judged; nor
//! are access control lists, capabilities or a mount's flags). The directory itself is not
//! judged (the install makes it the user's), and a missing directory and the ones below it
//! are made 0755 beside it. Root enters every directory and is not judged.

use std::io;
use std::path::{Path, PathBuf};

use super::host::{Kind, Lstat};
use super::{Host, Needs, Report};

pub(super) fn check(needs: &Needs, host: &dyn Host, report: &mut Report) {
    let (Some(dir), uid) = (&needs.hot_store, host.euid()) else {
        return;
    };
    if uid == 0 {
        return;
    }
    let (gid, groups) = (host.egid(), host.groups());
    let enter = |stat: &Lstat| can_enter(uid, gid, &groups, stat);
    match blocked_by(host, dir, enter) {
        Ok(None) => {}
        Ok(Some((above, stat))) => report.refuse(
            format!(
                "--hot-store=file://{} is below {} (mode {:04o}, uid {} gid {}), which uid {uid}, the user \
                 kbf runs as, cannot enter",
                dir.display(),
                above.display(),
                stat.stat.mode,
                stat.stat.uid,
                stat.gid,
            ),
            "kbf opens the hot store at its start, and the path to it passes through that directory",
            format!(
                "chmod o+x {}, or point --hot-store (hot-store in kbf.toml) at a directory \
                 uid {uid} can reach",
                above.display()
            ),
        ),
        Err((path, e)) => report.warnings.push(format!(
            "traverse: cannot read {} ({e}), so the path to --hot-store was not checked",
            path.display()
        )),
    }
}

/// The first existing directory above `dir` that `enter` refuses, with what `lstat` says
/// of it; none when every one that exists is enterable. A missing directory ends the walk
/// (so are the ones below it), and a path `lstat` cannot read is the error.
fn blocked_by(
    host: &dyn Host,
    dir: &Path,
    enter: impl Fn(&Lstat) -> bool,
) -> Result<Option<(PathBuf, Lstat)>, (PathBuf, io::Error)> {
    // `/` itself is not judged.
    let mut above: Vec<&Path> = dir
        .ancestors()
        .skip(1)
        .filter(|p| p.parent().is_some())
        .collect();
    above.reverse();
    for path in above {
        match host.lstat(path) {
            Ok(stat) if stat.stat.kind == Kind::Dir && !enter(&stat) => {
                return Ok(Some((path.to_owned(), stat)));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err((path.to_owned(), e)),
        }
    }
    Ok(None)
}

/// Whether a process with these ids has the execute bit on a directory with `stat`: the
/// owner's bit for the owner, else the group's for a member of its group, else other's.
fn can_enter(uid: u32, gid: u32, groups: &[u32], stat: &Lstat) -> bool {
    let bit = if stat.stat.uid == uid {
        0o100
    } else if stat.gid == gid || groups.contains(&stat.gid) {
        0o010
    } else {
        0o001
    };
    stat.stat.mode & bit != 0
}

#[cfg(test)]
#[path = "traverse_tests.rs"]
mod tests;
