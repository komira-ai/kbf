//! The host-step layer: what a program that configures a Linux host does to it, as data.
//!
//! A [`Planner`] looks at the host (read only) and records only the [`Step`]s that would change
//! it: a directory, a file, a user, a unit action or a command. [`execute`] then prints the
//! plan (`Mode::DryRun`, the default of every verb built on this crate) or carries it out
//! (`Mode::Apply`). A second run finds nothing to change and prints `nothing to do`.
//!
//! The host is the [`Host`] trait: [`RealHost`] is the machine, [`fake::FakeHost`] keeps a
//! file tree in memory and runs no program it was not told about, which is what the tests of
//! every verb use.
//!
//! # Rules the layer keeps
//!
//! * A plan line names a path, a size and a mode, never a file's contents: a step that
//!   writes a secret describes only its size. No step puts a value on a command line; a
//!   program that needs a secret reads it from a file.
//! * Every file is written to a temporary name in its directory with its mode and owner
//!   set, then renamed over the target, so a reader sees the old file or the new one, and
//!   the mode is exact whatever the umask.
//! * Nothing here reads an environment variable.

pub mod fake;
mod host;
mod plan;
mod real;

pub use host::{Cmd, Host, Kind, Meta, Output};
pub use plan::{Account, ExecError, Mode, Plan, Planner, Step, UnitVerb, execute, ids};
pub use real::RealHost;

/// Printed after a dry run.
pub const DRY_RUN_NOTE: &str = "dry run: nothing changed; re-run with --apply to carry it out";
/// Printed when the host already is as asked.
pub const NOTHING_TO_DO: &str = "nothing to do";
/// The unit manager's client.
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";
/// The account database lookup.
pub const GETENT: &str = "/usr/bin/getent";
/// Runs a program as another user.
pub const RUNUSER: &str = "/usr/sbin/runuser";
/// Creates a user.
pub const USERADD: &str = "/usr/sbin/useradd";
