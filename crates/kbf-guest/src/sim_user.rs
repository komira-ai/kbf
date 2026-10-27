//! Who the simulator probe's user steps run as: the guest user `mkuser` recorded, the wish
//! for one ([`simpaths::USER_WANTED`]), and the wait for that user at the console.
//!
//! # Whose session
//! The user steps run as the user `kbf-guest mkuser` recorded in
//! [`simpaths::USER_READY`] (`image setup --guest-user`, `image prime`), once that user is at
//! the console; another user at the console is not used. With no record they run as
//! whoever owns the console. A guest that is to have a user ([`simpaths::USER_WANTED`]) and
//! has none yet runs nothing: the summary says to prime.
//!
//! [`crate::sim`] calls these and re-exports [`Wanted`].

use std::time::Duration;

use crate::sim::{CONSOLE_POLL, ConsoleUser, SimHost};
use crate::simpaths;

/// What the guest says about the user it is to have ([`simpaths::USER_WANTED`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wanted {
    /// There is no such file: no user is wanted.
    No,
    /// The file names this user.
    User(String),
    /// The file is there in some form but cannot be read as a valid user name (a link, a
    /// directory, too large, unreadable, a bad name): what is wanted is not known, which is
    /// not the same as nothing wanted. The text says why.
    Unreadable(String),
}

/// Waits for the console user, counting pauses: `None` when none appeared in time. With
/// `expected`, only that user counts (another user at the console is not waited on to leave:
/// the wait runs its course).
pub(crate) fn wait_console_user(
    host: &dyn SimHost,
    wait: Duration,
    expected: Option<&str>,
) -> Option<ConsoleUser> {
    let mut waited = Duration::ZERO;
    loop {
        if let Some(u) = host
            .console_user()
            .filter(|u| expected.is_none_or(|e| u.name == e))
        {
            return Some(u);
        }
        if waited >= wait {
            return None;
        }
        host.pause(CONSOLE_POLL);
        waited += CONSOLE_POLL;
    }
}

/// Why a run must not start while no user is recorded as ready: the guest is to have a user
/// (or whether it is cannot be known) and has none yet. `None`: no user is wanted.
pub(crate) fn unmet_wish(host: &dyn SimHost) -> Option<String> {
    match host.wanted_user() {
        Wanted::No => None,
        Wanted::User(wanted) => Some(format!(
            "the guest user {wanted} does not exist yet: run `kbf-vmm image prime` on this base first"
        )),
        Wanted::Unreadable(e) => Some(format!(
            "{} exists but does not name a user ({e}): whether the guest is to have one is not known, so nothing was run; fix or remove that file, then run `kbf-vmm image prime` if it names one",
            simpaths::USER_WANTED
        )),
    }
}
