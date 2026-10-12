//! `kbf-mac-provision`: checks and applies the host settings and the worker profile of a
//! Mac node from a profile file (docs/design/mac-node-provisioning.md, sections 2, 3 and
//! 5). It is the Rust port of the script `tools/mac-provision/kbf-mac-provision`, with
//! the same profile format, item names, report lines and exit statuses:
//!
//! ```text
//! kbf-mac-provision check   --profile FILE [--keys K,K] [--root DIR]  # one PASS/FAIL line per item
//! kbf-mac-provision apply   --profile FILE [--keys K,K] [--root DIR]  # root: fix each FAIL, one CHANGE line per change
//! kbf-mac-provision print   --profile FILE                            # the resolved profile, its SHA-256, the node label
//! kbf-mac-provision restart --profile FILE [--root DIR]               # root: restart kbf-daemon, emptying its error log
//! kbf-mac-provision revert  [--root DIR]                              # root: put back the recorded power and update settings
//! ```
//!
//! Exit status: 0 when every item passes (or the verb did what it says), 1 when one
//! fails, 2 for bad usage, a refused profile, or a verb that refused or could not
//! finish. Two things the script does not do: `appstore_auto_update` also sets the
//! older `com.apple.commerce AutoUpdate`, and `apply` keeps a record of the power and
//! software update settings from before it first changed one, which `revert` restores
//! (see [`record`]).
//!
//! - [`profile`]: the profile file: `KEY=VALUE` lines over a closed key set; an unknown,
//!   repeated (other than `xcode`, `probe`, `expect_probe`), missing or malformed key is
//!   refused.
//! - [`host`]: the [`host::Host`] trait, everything a verb asks of the Mac (run a
//!   program, read a file's owner and mode, the effective uid, change a file), and
//!   [`host::RealHost`].
//! - [`items`]: the table of items, a check and a fix each, and the reports.
//! - [`launchd`]: the LaunchDaemon property list the profile renders, and the restart.
//! - [`record`]: the settings as `apply` first found them, and `revert`.
//! - [`cli`]: flags and exit statuses.
//!
//! A check only reads: the programs it runs are the read forms (`-get`, `-read`, `-p`,
//! `print`, `-T`), and it calls none of [`host::Host`]'s file changes.

pub mod cli;
pub mod host;
pub mod items;
pub mod launchd;
pub mod profile;
pub mod record;

mod accounts;
mod pattern;
mod software;
mod sshd;
