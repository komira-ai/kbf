//! `package prerm` and `package postrm`.
//!
//! `prerm` stops `kbf-node` unless the package is being upgraded (a new package changes
//! root-owned files only: the running release is the updater's, so the node keeps
//! serving). The unit drains on SIGTERM and has `TimeoutStopSec=infinity`, so `systemctl
//! stop` returns when the drain is done.
//!
//! `postrm` after a remove only reloads systemd if it holds a unit the package took
//! away: configuration, keys and data stay. After a purge it removes, in this order:
//! `/etc/kbf`, the enrollment token and `/var/lib/kbf/pki` (a key and a credential),
//! the copied trust files, then the lingering, the user's subordinate ranges, the user
//! and its group. The node's other state under `/var/lib/kbf` (hot store, metadata log,
//! data) and the releases with the deploy state stay: they are data, and a purge that
//! deleted a farm's cache by default would be a way to lose it. `--purge-data` adds them.
//! Data left owned by a removed user has an id no account holds; a later install makes a
//! new `kbf` user and `--purge-data` first (or a `chown`) hands the directories over.
//!
//! Directories are removed by listing the tree and removing each entry, children first:
//! the step that removes a path never recurses, and a link in the tree is removed as a
//! link, never followed.

use std::fs;
use std::io::Write;
use std::path::Path;

use kbf_host::exec::Exec;
use kbf_host::path::{HostPath, Root};
use kbf_host::plan::{Mode, Planner};
use kbf_host::step::Cmd;

use super::account::{SUBID_FILES, group_exists, kbf_user, linger_file, ranges};
use super::postinst::{needs_reload, systemd_running};
use super::subid::Range;
use super::{
    ETC, Env, GROUPDEL, LOGINCTL, PKI, PackageError, STATE, TOKEN, TRUST_DIR, UNIT, USER, USERDEL,
    USERMOD, path, phase, read_failed, say,
};
use crate::host::layout;
use crate::linux::host::{SYSTEMCTL, tool, unit_is};

/// What `postrm` was asked for.
#[derive(Clone, Copy, Debug)]
pub struct Postrm {
    pub purge: bool,
    pub purge_data: bool,
}

/// `prerm`: stop the node, unless this is an upgrade.
pub fn prerm<E: Exec>(
    exec: &mut E,
    upgrade: bool,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), PackageError> {
    let mut planner = Planner::new(&*exec);
    if upgrade {
        say(out, &format!("upgrade: {UNIT} keeps running"))?;
    } else if systemd_running(&*exec)? && unit_is(&*exec, "is-active", UNIT)? {
        planner.run(Cmd::new(tool(SYSTEMCTL)).arg("stop").arg(UNIT));
    } else {
        say(out, &format!("{UNIT} is not running"))?;
    }
    let plan = planner.finish();
    phase(exec, "stop", &plan, mode, out)
}

/// `postrm`: see the module docs.
pub fn postrm<E: Exec>(
    exec: &mut E,
    env: &Env,
    what: Postrm,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), PackageError> {
    let systemd = systemd_running(&*exec)?;
    let user = if what.purge { kbf_user(&*exec)? } else { None };

    let mut planner = Planner::new(&*exec);
    if what.purge {
        for tree in purge_trees(what.purge_data) {
            for entry in tree_entries(&env.root, &path(tree))? {
                planner.absent(entry)?;
            }
        }
        let linger = linger_file()?;
        if systemd && user.is_some() && present_linger(&*exec, &linger)? {
            planner.run(Cmd::new(tool(LOGINCTL)).arg("disable-linger").arg(USER));
        } else {
            planner.absent(linger)?;
        }
    }
    if systemd && needs_reload(&*exec)? {
        planner.run(Cmd::new(tool(SYSTEMCTL)).arg("daemon-reload"));
    }
    let files = planner.finish();
    let accounts = if what.purge {
        plan_user_removal(&*exec, systemd, user.as_ref().map(|u| u.uid), out)?
    } else {
        kbf_host::plan::Plan::default()
    };

    phase(exec, "files", &files, mode, out)?;
    if what.purge {
        phase(exec, "account", &accounts, mode, out)?;
        // userdel takes the user's own group with it on a host that makes one per user;
        // what is left is removed now that nothing has it as a primary group.
        if mode == Mode::Apply {
            let mut planner = Planner::new(&*exec);
            if group_exists(&*exec, USER)? {
                planner.run(Cmd::new(tool(GROUPDEL)).arg(USER));
            }
            let group = planner.finish();
            phase(exec, "group", &group, mode, out)?;
        } else {
            say(
                out,
                &format!("group {USER}: removed with the user, or after it if the host keeps it"),
            )?;
        }
    }
    Ok(())
}

/// The trees a purge removes, each at the path it is at.
fn purge_trees(data: bool) -> Vec<&'static str> {
    if data {
        // /var/lib/kbf holds the token and the keys as well as the data.
        vec![ETC, TRUST_DIR, STATE, layout::KBF_DIR, layout::STATE_DIR]
    } else {
        vec![ETC, TRUST_DIR, TOKEN, PKI]
    }
}

fn present_linger<E: Exec>(exec: &E, linger: &HostPath) -> Result<bool, PackageError> {
    super::release::present(exec, linger)
}

/// Stop the user's manager, drop its subordinate ranges, remove the account.
fn plan_user_removal<E: Exec>(
    exec: &E,
    systemd: bool,
    uid: Option<u32>,
    out: &mut dyn Write,
) -> Result<kbf_host::plan::Plan, PackageError> {
    let mut planner = Planner::new(exec);
    let Some(uid) = uid else {
        say(out, &format!("user {USER}: not there"))?;
        return Ok(planner.finish());
    };
    if systemd {
        planner.run(
            Cmd::new(tool(SYSTEMCTL))
                .arg("stop")
                .arg(format!("user@{uid}.service")),
        );
    }
    let mut usermod = Cmd::new(tool(USERMOD));
    let mut dels = false;
    for (file, _, del) in SUBID_FILES {
        let mine = ranges(exec, file)?
            .into_iter()
            .filter(|r| r.owner == USER || r.owner == uid.to_string());
        for r in mine {
            usermod = usermod.arg(del).arg(Range::span(r.start, r.count));
            dels = true;
        }
    }
    if dels {
        planner.run(usermod.arg(USER));
    }
    // No --force and no --remove: a user still logged in is a refusal, and the home
    // directory is data.
    planner.run(Cmd::new(tool(USERDEL)).arg(USER));
    Ok(planner.finish())
}

/// `top` and everything below it, children before parents. A link is an entry and is
/// not followed; nothing is listed for a path that is not there.
pub(crate) fn tree_entries(root: &Root, top: &HostPath) -> Result<Vec<HostPath>, PackageError> {
    let mut out = Vec::new();
    let full = root.join(top);
    if fs::symlink_metadata(&full).is_err() {
        return Ok(out);
    }
    walk(&full, top.as_str(), &mut out)?;
    out.push(top.clone());
    Ok(out)
}

fn walk(dir: &Path, host: &str, out: &mut Vec<HostPath>) -> Result<(), PackageError> {
    if !fs::symlink_metadata(dir)
        .map_err(read_failed(dir))?
        .is_dir()
    {
        return Ok(());
    }
    for entry in fs::read_dir(dir).map_err(read_failed(dir))? {
        let entry = entry.map_err(read_failed(dir))?;
        let file = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| PackageError::NotUtf8(file.clone()))?;
        let child = format!("{host}/{name}");
        walk(&file, &child, out)?;
        out.push(HostPath::new(&child)?);
    }
    Ok(())
}

#[cfg(test)]
#[path = "remove_tests.rs"]
mod tests;
