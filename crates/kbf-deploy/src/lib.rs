//! `kbf-deploy`: the deploy tool. The binary's subcommands are the kits; this library
//! holds them, the command line and what they share beyond `kbf-host`.
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
//! - [`envoy`]: a client front's Kubernetes objects and Envoy config, rendered from
//!   the settings (`envoy render`), byte for byte the same for the same settings.

pub mod cli;
pub mod config;
pub mod envoy;
pub mod linux;
pub mod minio;
pub mod template;
pub mod unit;

pub use config::{Config, ConfigError};
