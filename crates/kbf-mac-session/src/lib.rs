//! `kbf-mac-session`: the root helper that gives every Mac lease its own throwaway
//! user (docs/design/fleet-updates-security.md S4.2 and S4.3; fleet-updates.md 10.2,
//! layer L0): the verbs `user-create`, `run` (in the background, or in the lease user's
//! GUI session), `kill-uid` and `user-delete`, the GUI session switch (`session-login`,
//! `session-ready`, `session-idle`) and the boot reset. The leak scan is a later phase.
//!
//! - [`lease`]: lease ids, user names (`kbf-lease-<term>-<seq>`) and the uid range.
//! - [`grant`]: the MDM gate's signed admin grant.
//! - [`ledger`]: lease ids used, kept across reboots, so names and grants are used once.
//! - [`helper`]: the verbs' rules, over a [`helper::Host`] that does the OS's part.
//! - [`autologin`]: the session switch's intent record, `/etc/kcpassword`, the login
//!   window's preference and the restart.
//! - [`sweep`]: what `user-delete` removes, never following a link.
//! - [`spawn`]: starting a process as the lease user.
//! - [`proto`], [`server`], [`client`]: the socket, its wire format, the caller check.
//! - [`start`]: flags and start-up.
//! - `macos`: the host (OpenDirectory, launchd, libproc), the caller check by code
//!   signature, and the `as-user` mode a GUI run executes (`macos::gui`), on macOS
//!   only.
//!
//! Everything but `macos` builds and is tested on every platform; the binary serves
//! only on macOS.

pub mod autologin;
pub mod client;
pub mod grant;
pub mod helper;
pub mod lease;
pub mod ledger;
pub mod proto;
pub mod server;
pub mod spawn;
pub mod start;
pub mod sweep;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(test)]
mod testing;
