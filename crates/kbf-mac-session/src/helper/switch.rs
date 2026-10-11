//! The GUI session switch, `session-login` and `session-idle` (S4.2; fleet-updates.md
//! 10.2, layer L2), and the boot reset.
//!
//! - `session-login <lease>` needs a live lease user and FileVault off. It writes the
//!   intent record naming the lease, gives the user a fresh random password, writes
//!   `/etc/kcpassword` for it, sets `autoLoginUser` to the user, and only then asks for
//!   a userspace restart. Refused while the record names another lease.
//! - `session-idle` clears `autoLoginUser` and `/etc/kcpassword`, removes the intent
//!   record, and asks for the same restart: the Mac rests at the login window.
//! - `boot-reset [held]` runs once per start of the helper, and every other verb is
//!   refused until it has succeeded: the daemon calls it before its `Hello`, naming the
//!   leases the server still holds for the node. The lease the intent record names is
//!   kept, with its auto-login, only if it is among them and its user is live; otherwise
//!   auto-login and `/etc/kcpassword` are cleared and the record removed. Then every
//!   other live lease user is killed and deleted (with the sweep of `user-delete`): a Mac
//!   that lost power mid-lease never stays logged in as a dead lease user. A reset that
//!   fails part-way can be repeated.
//!
//! The restart ends the daemon's connection, and the helper with it, so the reply to a
//! switch may never arrive; the daemon learns of the switch from its own restart.

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use kbf_types::LeaseId;

use super::Helper;
use crate::autologin::{self, read_intent, remove, write_intent, write_kcpassword};
use crate::lease::{parse_lease, user_name};
use crate::ledger::Ledger;

/// What the boot reset did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reset {
    /// The lease whose session was kept.
    pub kept: Option<LeaseId>,
    /// The leases whose users were deleted.
    pub deleted: Vec<LeaseId>,
}

impl Helper {
    /// Whether the boot reset has run since the helper started.
    #[must_use]
    pub fn reset_done(&self) -> bool {
        self.reset_done.load(Ordering::Acquire)
    }

    /// `session-login <lease>`: auto-login into the lease's user, then a userspace
    /// restart.
    ///
    /// # Errors
    /// Why it was refused or failed. A failure after the intent record was written
    /// leaves it, so a later `session-idle` or the boot reset clears what was set.
    pub fn session_login(&self, lease: &str) -> Result<(), String> {
        let lease = parse_lease(lease)?;
        let paths = &self.settings.session;
        let ledger = self.lock();
        self.live_uid(&ledger, lease)?;
        match read_intent(&paths.intent)? {
            Some(other) if other != lease => {
                return Err(format!(
                    "the session belongs to lease {other}; session-idle first"
                ));
            }
            _ => {}
        }
        autologin::require_filevault_off(self.host.as_ref())?;
        autologin::restart_userspace(self.host.as_ref())?;
        write_intent(&paths.intent, lease)
            .map_err(|why| format!("writing {}: {why}", paths.intent.display()))?;
        let name = user_name(lease);
        let password = self
            .host
            .reset_password(&name)
            .map_err(|why| format!("setting the password of {name}: {why}"))?;
        write_kcpassword(&paths.kcpassword, password.as_bytes())
            .map_err(|why| format!("writing {}: {why}", paths.kcpassword.display()))?;
        autologin::set_auto_login(self.host.as_ref(), paths, &name)?;
        drop(ledger);
        tracing::info!(lease = %lease, "auto-login set to {name}; restarting userspace");
        autologin::restart_userspace(self.host.as_ref())
    }

    /// `session-idle`: clears auto-login, then a userspace restart.
    ///
    /// # Errors
    /// Auto-login could not be cleared, the record removed, or the restart asked for.
    pub fn session_idle(&self) -> Result<(), String> {
        let ledger = self.lock();
        self.clear_session()?;
        drop(ledger);
        tracing::info!("auto-login cleared; restarting userspace");
        autologin::restart_userspace(self.host.as_ref())
    }

    /// Clears auto-login and `/etc/kcpassword`, then removes the intent record.
    fn clear_session(&self) -> Result<(), String> {
        let paths = &self.settings.session;
        autologin::clear_auto_login(self.host.as_ref(), paths)?;
        remove(&paths.intent)
            .map(drop)
            .map_err(|why| format!("removing {}: {why}", paths.intent.display()))
    }

    /// `boot-reset [held]`: see the module documentation.
    ///
    /// # Errors
    /// It ran before, a held lease id does not parse, or clearing auto-login, killing or
    /// deleting a leftover user failed (each failure is named; the rest still ran).
    pub fn boot_reset(&self, held: &[String]) -> Result<Reset, String> {
        let held: BTreeSet<LeaseId> = held
            .iter()
            .map(|lease| parse_lease(lease))
            .collect::<Result<_, _>>()?;
        let mut ledger = self.lock();
        if self.reset_done() {
            return Err("the boot reset ran already; it runs once per start".to_owned());
        }
        let intent = match read_intent(&self.settings.session.intent) {
            Ok(intent) => intent,
            Err(why) => {
                tracing::warn!("an unreadable intent record is stale: {why}");
                None
            }
        };
        let kept =
            intent.filter(|lease| held.contains(lease) && self.live_uid(&ledger, *lease).is_ok());
        if kept.is_none() {
            self.clear_session()?;
        }
        let mut deleted = Vec::new();
        let mut errors = Vec::new();
        for lease in ledger.live_leases() {
            if Some(lease) == kept {
                continue;
            }
            match self.remove_leftover(&mut ledger, lease) {
                Ok(()) => deleted.push(lease),
                Err(why) => errors.push(format!("lease {lease}: {why}")),
            }
        }
        if !errors.is_empty() {
            return Err(format!(
                "the boot reset is incomplete: {}",
                errors.join("; ")
            ));
        }
        self.reset_done.store(true, Ordering::Release);
        tracing::info!(kept = ?kept, deleted = deleted.len(), "boot reset done");
        Ok(Reset { kept, deleted })
    }

    /// Kills and deletes the user of a lease left from before this start.
    fn remove_leftover(&self, ledger: &mut Ledger, lease: LeaseId) -> Result<(), String> {
        self.kill_locked(ledger, lease)?;
        self.delete_locked(ledger, lease).map(drop)
    }
}

#[cfg(test)]
mod tests;
