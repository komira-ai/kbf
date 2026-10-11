//! `kbf-deploy`: the deploy tool. The binary's subcommands are the kits; this library
//! holds them and what they share beyond `kbf-host`.
//!
//! A kit that changes a host does it the one way `kbf-host` gives: it builds a
//! `kbf_host::Plan` of steps and carries it out with `kbf_host::execute`, which prints
//! each step as `DRY-RUN: <step>` by default and does it only with `--apply`.
//!
//! - [`config`]: `--config FILE`, `KEY=VALUE` lines over a closed key set; an unknown
//!   or repeated key is refused.
//! - [`unit`]: a systemd unit file read as systemd reads it (sections, `\` continuations,
//!   the last assignment wins), shared with the docs check in `kbf-it`.
//! - [`template`]: strict `@KEY@` substitution; a placeholder with no value is refused.
//!
//! The kits:
//! - [`linux`]: a converged storage node's systemd units, rendered from compiled-in
//!   templates (`linux render-units`) and checked against the host's priorities
//!   (`linux check-units`). It writes only into the directory it is given.

pub mod config;
pub mod linux;
pub mod template;
pub mod unit;

pub use config::{Config, ConfigError};
