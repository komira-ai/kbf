//! The standing VM user's verbs: `role-run`, `role-session-ready` and `role-kill-uid`.
//!
//! The VM user (`--vm-user`) is created by provisioning, not by the helper, and is not
//! in the ledger: its uid is read from its account record each time, and used only if it
//! is an ordinary uid outside the lease range ([`VmUser::admits_uid`]), so a wrong or
//! missing record can never point a verb at root, a system account or a lease user.
//! Its name can never be a lease user's ([`VmUser`]), so nothing that deletes lease
//! users (`user-delete`, the boot reset) can reach it.
//!
//! The GUI session is one at a time. A whole-machine lease holds it from `session-login`
//! until `session-idle` (the intent record names the lease); the VM user holds it
//! otherwise. `role-run` and `role-session-ready` therefore answer for the VM user
//! only while the record does not name a lease. That a lease and VMs never run together
//! is the daemon's rule (it drains VMs before `session-login`); this is what stops a
//! process starting in a session that belongs to someone else.
//!
//! `role-kill-uid` ends the VM user's session along with its processes, so it is for
//! a Mac that is draining; `session-idle` brings the session back.

use std::os::fd::OwnedFd;
use std::process::Child;

use kbf_types::LeaseId;

use super::{Account, Helper};
use crate::autologin::{Intent, read_intent};
use crate::lease::VmUser;
use crate::proto::Session;
use crate::spawn::{self, Spawn};

/// The VM user's account, as a verb acts on it.
pub(super) struct Role<'a> {
    pub(super) name: &'a str,
    pub(super) account: Account,
}

/// Where the GUI session of the VM user stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Gui {
    /// `gui/<uid>` exists and no lease holds the session.
    Up,
    /// A lease holds the session.
    Leased(LeaseId),
    /// `gui/<uid>` does not exist.
    Down,
}

impl Helper {
    /// The VM user's account, checked.
    ///
    /// # Errors
    /// No VM user is configured, it has no account record, or its uid or gid is not
    /// one the helper will act on.
    pub(super) fn vm_role(&self) -> Result<Role<'_>, String> {
        let vm = self
            .settings
            .vm_user
            .as_ref()
            .ok_or("no VM user is configured on this Mac (--vm-user)")?;
        let name = vm.name();
        let account = self
            .host
            .account(name)
            .map_err(|why| format!("looking up the VM user {name}: {why}"))?
            .ok_or_else(|| format!("the VM user {name} has no account"))?;
        if !VmUser::admits_uid(self.settings.range, account.uid) || account.gid == 0 {
            return Err(format!(
                "the VM user {name} has uid {} and gid {}; the uid must be an ordinary one outside {}",
                account.uid, account.gid, self.settings.range
            ));
        }
        Ok(Role { name, account })
    }

    pub(super) fn vm_gui(&self, role: &Role<'_>) -> Result<Gui, String> {
        if let Some(Intent::Lease(lease)) = read_intent(&self.settings.session.intent)? {
            return Ok(Gui::Leased(lease));
        }
        let domain = format!("gui/{}", role.account.uid);
        let up = self
            .host
            .domain_exists(&domain)
            .map_err(|why| format!("asking launchd for {domain}: {why}"))?;
        Ok(if up { Gui::Up } else { Gui::Down })
    }

    /// `role-run <fd-passed argv>`: starts the process as the VM user in its GUI
    /// session. The descriptors are stdin, stdout, stderr and the working directory.
    ///
    /// # Errors
    /// Why it was refused, or why the process did not start.
    pub fn role_run(
        &self,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        fds: Vec<OwnedFd>,
    ) -> Result<Child, String> {
        spawn::check(&argv, &env)?;
        let count = fds.len();
        let [stdin, stdout, stderr, cwd]: [OwnedFd; 4] = fds.try_into().map_err(|_| {
            format!(
                "role-run takes stdin, stdout, stderr and the working directory; got {count} descriptors"
            )
        })?;
        let ledger = self.lock();
        let role = self.vm_role()?;
        let uid = role.account.uid;
        match self.vm_gui(&role)? {
            Gui::Up => {}
            Gui::Leased(lease) => {
                return Err(format!(
                    "lease {lease} holds the GUI session; session-idle first"
                ));
            }
            Gui::Down => {
                return Err(format!(
                    "gui/{uid} does not exist: the login of the VM user {} has not completed",
                    role.name
                ));
            }
        }
        let program = argv[0].clone();
        let child = spawn::spawn(Spawn {
            uid,
            gid: role.account.gid,
            home: self.settings.homes.join(role.name),
            user: role.name.to_owned(),
            argv,
            env,
            stdin,
            stdout,
            stderr,
            cwd,
            session: Session::Gui,
        })
        .map_err(|why| format!("starting {program}: {why}"))?;
        drop(ledger);
        tracing::info!(uid, pid = child.id(), "started {program} as the VM user");
        Ok(child)
    }

    /// `role-session-ready`: whether the VM user's GUI session is up (`gui/<uid>`
    /// exists and no lease holds the session).
    ///
    /// # Errors
    /// No VM user is configured or its account is not one the helper acts on, the
    /// intent record cannot be read, or launchd could not be asked.
    pub fn role_session_ready(&self) -> Result<bool, String> {
        let _ledger = self.lock();
        let role = self.vm_role()?;
        Ok(self.vm_gui(&role)? == Gui::Up)
    }

    /// `role-kill-uid`: as [`Self::kill_uid`] for the VM user. It does not look at the
    /// intent record, so it also ends the VM user's session while a lease holds the GUI.
    ///
    /// # Errors
    /// Why it was refused, or processes survived the kill rounds.
    pub fn role_kill_uid(&self) -> Result<(), String> {
        let _ledger = self.lock();
        let role = self.vm_role()?;
        self.kill_account(role.name, role.account.uid)?;
        tracing::info!(uid = role.account.uid, "no process of the VM user is left");
        Ok(())
    }
}

#[cfg(test)]
mod tests;
