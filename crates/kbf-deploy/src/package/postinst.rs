//! `package postinst`: the host brought to what the package's units need, in this order,
//! each part planned only where the host differs.
//!
//! 1. The dependencies are checked and the package's own files are read; a clash with a
//!    subordinate id range, a shipped release that differs from the one installed, or a
//!    deploy state that cannot be read refuses the verb before any step runs.
//! 2. `account`: the group, the user, its subordinate ids and lingering ([`super::account`]).
//! 3. `directories`: `/etc/kbf` (root's, group `kbf`, 0750, where the operator puts
//!    `kbf.toml`), `/var/lib/kbf` and its `hot`, `meta` and `data` (the user's, 0700).
//!    `pki/` is the node's to make when it enrolls. They are planned once the user
//!    exists, because a directory step needs its ids: a dry run on a host without the
//!    user lists them instead of planning them.
//! 4. `files`: the trust files and the shipped release ([`super::release`]), under the
//!    deploy lock when applied, with the deploy state written last.
//! 5. `units`: `systemctl daemon-reload` when systemd says a unit changed on disk, and
//!    `systemctl enable kbf-node.service` once there is something for the node to start
//!    from: `/etc/kbf/kbf.toml` and either the token file or a `node.json` from an earlier
//!    enrollment. It is not started: `systemctl start kbf-node` is the operator's, after
//!    the configuration is checked. With no running systemd (a container build) this part
//!    is skipped and says so.

use std::io::Write;

use kbf_host::exec::Exec;
use kbf_host::plan::{Mode, Planner};
use kbf_host::step::{Cmd, Owner, Perms};
use kbf_host::user::RunUser;

use super::release::{self, present};
use super::{
    CRUN, Env, NEWGIDMAP, NEWUIDMAP, PackageError, STATE, SYSTEMD_RUNNING, UNIT, account, path,
    phase, say,
};
use crate::host::state::{self, Locked};
use crate::linux::host::{SYSTEMCTL, checked, stat, tool, unit_is};

/// The directories (path, mode, whether root owns them with group `kbf`; the user owns
/// the rest).
pub(crate) const DIRECTORIES: [(&str, u32, bool); 5] = [
    (super::ETC, 0o750, true),
    (STATE, 0o700, false),
    ("/var/lib/kbf/hot", 0o700, false),
    ("/var/lib/kbf/meta", 0o700, false),
    ("/var/lib/kbf/data", 0o700, false),
];

/// `postinst`: see the module docs.
pub fn run<E: Exec>(
    exec: &mut E,
    env: &Env,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), PackageError> {
    check_dependencies(&*exec, env)?;
    let systemd = systemd_running(&*exec)?;
    let trust = release::read_trust(&env.root)?;
    let shipped = release::read_shipped(&env.root)?;

    // The lock is held from the read of the deploy state to its write, as `host install`
    // holds it; a dry run reads without it and writes nothing.
    let lock = match mode {
        Mode::Apply => Some(Locked::acquire(&env.root)?),
        Mode::DryRun => None,
    };
    let state = match &lock {
        Some(lock) => lock.read()?,
        None => state::read_unlocked(&env.root)?,
    }
    .unwrap_or_default();

    // Everything that can refuse is planned before the first step runs.
    let accounts = account::plan(&*exec, systemd, env.root_owner, out)?;
    let mut planner = Planner::new(&*exec);
    release::plan_trust(&mut planner, env.root_owner, &trust)?;
    let next = release::plan_release(&*exec, &mut planner, env.root_owner, &shipped, &state, out)?;
    let files = planner.finish();

    phase(exec, "account", &accounts, mode, out)?;
    let directories = plan_directories(&*exec, env.root_owner, mode, out)?;
    phase(exec, "directories", &directories, mode, out)?;
    phase(exec, "files", &files, mode, out)?;
    if mode == Mode::Apply {
        release::verify_copy(&*exec, &shipped)?;
    }
    if let (Some(lock), Some(next)) = (lock, next) {
        lock.write(&next)?;
        say(
            out,
            &format!("state: written to {}", crate::host::layout::STATE_FILE),
        )?;
    }
    let units = plan_units(&*exec, systemd, out)?;
    phase(exec, "units", &units, mode, out)
}

/// `crun` runs, and `newuidmap` and `newgidmap` are root's setuid programs (what rootless
/// containers map a user's subordinate ids with). Nothing is installed.
fn check_dependencies<E: Exec>(exec: &E, env: &Env) -> Result<(), PackageError> {
    let missing = |what, why: String| PackageError::Dependency { what, why };
    let crun = Cmd::new(tool(CRUN)).arg("--version");
    match exec.query(&crun) {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            return Err(missing(
                "crun",
                format!("{} exited with {}", CRUN, out.status),
            ));
        }
        Err(e) => return Err(missing("crun", format!("{CRUN}: {e}"))),
    }
    for (what, program) in [("newuidmap", NEWUIDMAP), ("newgidmap", NEWGIDMAP)] {
        let host = path(program);
        match stat(exec, &host)? {
            Some(m) if m.perms.bits() & 0o4111 == 0o4111 && m.owner.uid == env.root_owner.uid => {}
            Some(_) => {
                return Err(missing(
                    what,
                    format!("{program} is not an executable setuid root"),
                ));
            }
            None => return Err(missing(what, format!("no {program}"))),
        }
    }
    Ok(())
}

/// Whether this host was booted with systemd (`sd_booted(3)`).
pub(crate) fn systemd_running<E: Exec>(exec: &E) -> Result<bool, PackageError> {
    present(exec, &path(SYSTEMD_RUNNING))
}

/// The user's directories. Without the user (a dry run before it exists) they are listed.
fn plan_directories<E: Exec>(
    exec: &E,
    root: Owner,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<kbf_host::plan::Plan, PackageError> {
    let mut planner = Planner::new(exec);
    match account::kbf_user(exec)? {
        Some(user) => {
            for (dir, bits, root_owned) in DIRECTORIES {
                planner.dir(
                    path(dir),
                    Perms::new(bits),
                    Some(owner(&user, root, root_owned)),
                )?;
            }
        }
        None if mode == Mode::DryRun => {
            for (dir, bits, root_owned) in DIRECTORIES {
                let who = if root_owned { "root:kbf" } else { "kbf:kbf" };
                say(
                    out,
                    &format!("{dir}: {bits:04o} {who}, planned once user kbf exists"),
                )?;
            }
        }
        None => return Err(crate::linux::HostError::NoUser(super::USER.to_owned()).into()),
    }
    Ok(planner.finish())
}

fn owner(user: &RunUser, root: Owner, root_owned: bool) -> Owner {
    Owner {
        uid: if root_owned { root.uid } else { user.uid },
        gid: user.gid,
    }
}

/// `daemon-reload` when systemd says it must, `enable` when the node has what it starts
/// from.
fn plan_units<E: Exec>(
    exec: &E,
    systemd: bool,
    out: &mut dyn Write,
) -> Result<kbf_host::plan::Plan, PackageError> {
    let mut planner = Planner::new(exec);
    if !systemd {
        say(out, "systemd is not running: units are left as they are")?;
        return Ok(planner.finish());
    }
    if needs_reload(exec)? {
        planner.run(Cmd::new(tool(SYSTEMCTL)).arg("daemon-reload"));
    }
    let configured = present(exec, &path(super::CONFIG))?;
    let enrolled = present(exec, &path(super::TOKEN))? || present(exec, &path(super::NODE_JSON))?;
    if configured && enrolled {
        if !unit_is(exec, "is-enabled", UNIT)? {
            planner.run(Cmd::new(tool(SYSTEMCTL)).arg("enable").arg(UNIT));
        }
    } else {
        say(
            out,
            &format!(
                "{UNIT} is not enabled: it needs {} and {} (or an enrolled node's {})",
                super::CONFIG,
                super::TOKEN,
                super::NODE_JSON
            ),
        )?;
    }
    Ok(planner.finish())
}

/// Whether systemd holds an older copy of the unit than the file on disk.
pub(crate) fn needs_reload<E: Exec>(exec: &E) -> Result<bool, PackageError> {
    let show = Cmd::new(tool(SYSTEMCTL))
        .arg("show")
        .arg("--property=NeedDaemonReload")
        .arg("--value")
        .arg(UNIT);
    Ok(checked(exec, &show)?.trim() == "yes")
}

#[cfg(test)]
#[path = "postinst_tests.rs"]
mod tests;
