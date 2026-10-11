//! `kbf-deploy`: the core the deploy kits are built on.
//!
//! A kit asks the host what is there and builds a [`Plan`] of [`Step`]s that brings the
//! host to what it wants. By default [`execute`] prints each step as `DRY-RUN: <step>`
//! and changes nothing; with [`Mode::Apply`] (`--apply`) it prints `+ <step>` and does
//! it, stopping at the first failure. A plan made again after it was applied is empty.
//!
//! - [`path`]: [`HostPath`], an absolute path with no `..`, joined to a [`Root`] (`/` on
//!   a host, a scratch directory in tests).
//! - [`step`]: the steps, and the line each prints, which never shows file contents or
//!   environment values.
//! - [`plan`]: [`Planner`], which adds a file or directory step only when the host
//!   differs, and [`execute`], with its guards: a [`RunAs::Root`] plan applies only as
//!   root, a [`RunAs::User`] plan never as root, and applying as root first checks
//!   that the running binary and every directory above it are root's alone ([`trust`]).
//! - [`exec`]: the [`Exec`] trait and [`HostExec`], the real host. Commands run by
//!   absolute path with an empty environment plus the variables they name, never
//!   through a shell. Tests use a fake that records each step.
//! - [`config`]: `--config FILE`, `KEY=VALUE` lines over a closed key set; an unknown
//!   or repeated key is refused.

pub mod config;
pub mod exec;
pub mod path;
pub mod plan;
pub mod step;
pub mod trust;

#[cfg(test)]
pub(crate) mod testkit;

pub use config::{Config, ConfigError};
pub use exec::{Entry, Exec, HostExec};
pub use path::{HostPath, PathError, Root};
pub use plan::{DeployError, Mode, Plan, Planner, RunAs, execute};
pub use step::{Cmd, HttpUrl, Owner, Perms, Step, UrlError};
pub use trust::Untrusted;
