//! The local mode: `kbf-host-install --serve`, an update of a host by the account that
//! owns the socket, without ssh and without the apt package.
//!
//! The ssh forced command ([`crate::host`], [`crate::mac`]) is unchanged and is reached the
//! same way as before. This is the other door, for `kbf-updater`'s agent mode
//! (`docs/design/updates.md`, section 4): a unix socket, mode 0600, and five requests
//! ([`request`]) that make up one host's A/B:
//!
//! - `install` fetches the release the version names, verifies it on this host and lays it
//!   out beside the one that runs; no link moves;
//! - `switch` puts it in place on trial: a pending marker with a deadline is written
//!   first, the links move and the service restarts on it;
//! - `confirm` keeps it;
//! - `revert` goes back to the release that ran;
//! - `query` says what runs, what is on trial, and how the last update ended.
//!
//! This program owns the revert. A release on trial that is not confirmed by its deadline is
//! put back by this program ([`core::Ab::tick`]), whether or not the release, the updater
//! or the host is still up, and the first tick after a start does the same for a deadline
//! that passed while the host was down. The platform does the rest ([`backend::Backend`]):
//! [`linux`] on the layout of `kbf-installer`, [`package`] on the `kbf` package (a Linux
//! daemon host whose units run `/usr/bin/kbf`; `--layout deb` or `--layout rpm`),
//! [`mac_backend`] on the kit tree.

pub mod answer;
pub mod backend;
pub mod core;
pub mod production;
pub mod request;
pub mod serve;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod units;

// Compiled and tested everywhere, used by the program on Linux only.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod frontend;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
mod linux_image;
// A Linux host that runs the `kbf` package: the update is the package manager's.
#[cfg(target_os = "linux")]
pub mod package;
#[cfg(target_os = "linux")]
pub mod package_format;
#[cfg(target_os = "linux")]
pub mod package_store;
// Compiled and tested everywhere, used by the program on a Mac only.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub mod mac_backend;

#[cfg(test)]
mod answer_tests;
#[cfg(test)]
mod core_tests;
#[cfg(all(test, target_os = "linux"))]
mod linux_tests;
#[cfg(test)]
mod mac_backend_tests;
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod package_tests;
#[cfg(test)]
mod production_tests;
#[cfg(test)]
mod request_tests;
#[cfg(test)]
mod serve_tests;
#[cfg(test)]
mod timing_golden_tests;
#[cfg(test)]
mod units_tests;
