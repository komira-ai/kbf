//! `kbf-mac-provision`: checks the host settings and the worker profile of a Mac node
//! against a profile file (docs/design/mac-node-provisioning.md, sections 2, 3 and 5).
//! It is the Rust port of the script `tools/mac-provision/kbf-mac-provision`, with the
//! same profile format, item names, report lines and exit statuses, and it implements
//! two of the script's verbs:
//!
//! ```text
//! kbf-mac-provision check --profile FILE [--keys K,K] [--root DIR]  # one PASS/FAIL line per item
//! kbf-mac-provision print --profile FILE                            # the resolved profile, its SHA-256, the node label
//! ```
//!
//! `apply` and `restart` are still only in the script. Exit status: 0 when every item
//! passes, 1 when one fails, 2 for bad usage or a refused profile.
//!
//! - [`profile`]: the profile file: `KEY=VALUE` lines over a closed key set; an unknown,
//!   repeated (other than `xcode`, `probe`, `expect_probe`), missing or malformed key is
//!   refused.
//! - [`host`]: the [`host::Host`] trait, everything a check asks of the Mac (run a
//!   program, read a file's owner and mode, the effective uid), and [`host::RealHost`].
//! - [`items`]: the table of items, one check each, and the report.
//! - [`launchd`]: the LaunchDaemon property list the profile renders.
//! - [`cli`]: flags and exit statuses.
//!
//! A check only reads: [`host::Host`] has no method that changes the Mac, and the
//! programs it runs are the read forms (`-get`, `-read`, `-p`, `print`, `-T`).

pub mod cli;
pub mod host;
pub mod items;
pub mod launchd;
pub mod profile;

mod accounts;
mod pattern;
mod software;
mod sshd;
