//! `kbf-deploy`: the deploy tool. The binary's subcommands are the kits; this library
//! holds them, the command line and the core they are built on.
//!
//! A kit that changes a host asks it what is there and builds a [`Plan`] of [`Step`]s
//! that brings the host to what it wants. By default [`execute`] prints each step as
//! `DRY-RUN: <step>` and changes nothing; with [`Mode::Apply`] (`--apply`) it prints
//! `+ <step>` and does it, stopping at the first failure. A plan made again after it
//! was applied is empty.
//!
//! - [`path`]: [`HostPath`], an absolute path with no `..`, joined to a [`Root`] (`/` on
//!   a host, a scratch directory in tests).
//! - [`step`]: the steps, and the line each prints, which never shows file contents,
//!   environment values, a command's input or a captured output.
//! - [`plan`]: [`Planner`], which adds a file or directory step only when the host
//!   differs (and says whether it did), and [`execute`], with its guards: a
//!   [`RunAs::Root`] plan applies only as root, a [`RunAs::User`] plan never as root,
//!   and applying as root first checks that the running binary and every directory
//!   above it are root's alone ([`trust`]).
//! - [`https`]: a small HTTPS client (rustls with ring, the host's trusted roots) for
//!   the kits that download or check over TLS.
//! - [`exec`]: the [`Exec`] trait and [`HostExec`], the real host. Commands run by
//!   absolute path with an empty environment plus the variables they name, never
//!   through a shell. A kit reads the host while planning with [`Exec::query`] (a
//!   command that changes nothing), [`Exec::http_status`] and [`Exec::stat`] (what is
//!   at a path, without reading it), so a dry run asks the same questions an apply
//!   does. Tests use a fake that records each step and query.
//! - [`user`]: a command run as another user ([`RunUser`], looked up with `getent` and
//!   `id -G`), whose child takes that user's groups, group and uid before it starts.
//! - [`config`]: `--config FILE`, `KEY=VALUE` lines over a closed key set; an unknown
//!   or repeated key is refused.
//! - [`unit`]: a systemd unit file read as systemd reads it (sections, `\` continuations,
//!   the last assignment wins), shared with the docs check in `kbf-it`.
//! - [`template`]: strict `@KEY@` substitution; a placeholder with no value is refused.
//! - [`cli`]: the `kbf-deploy` command line, one subcommand per kit.
//!
//! The kits:
//! - [`minio`]: `minio buckets` (kbf's buckets, their policy and a service account key
//!   file) and `minio nodeport` (a NodePort Service for a MinIO's S3 port).
//! - [`linux`]: a converged storage node's systemd units, rendered from compiled-in
//!   templates (`linux render-units`) and checked against the host's priorities
//!   (`linux check-units`); and, as root, the units installed and the services started
//!   (`linux install-units`), the pinned images pulled as the kbf user (`linux
//!   image-pull`) and the swapfile (`linux swap`).
//! - [`mdm`]: an Apple MDM on one Linux host: its pinned upstream archives fetched
//!   (`mdm fetch`), its services installed as root (`mdm install`), and read-only
//!   checks of what runs (`mdm verify`).

pub mod cli;
pub mod config;
pub mod exec;
pub mod https;
pub mod linux;
pub mod mdm;
pub mod minio;
pub mod path;
pub mod plan;
pub mod step;
pub mod template;
pub mod trust;
pub mod unit;
pub mod user;

#[cfg(test)]
pub(crate) mod testkit;

pub use config::{Config, ConfigError};
pub use exec::{Entry, Exec, HostExec, Kind, Meta};
pub use path::{HostPath, PathError, Root};
pub use plan::{DeployError, Mode, Plan, Planner, RunAs, execute};
pub use step::{Cmd, Filter, HttpUrl, Owner, Perms, Step, UrlError};
pub use trust::Untrusted;
pub use user::RunUser;
