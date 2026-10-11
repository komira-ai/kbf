//! The `linux` kit: a converged storage node's systemd units (MinIO's drop-in,
//! kbf-server, kbf-daemon and the `kbf.slice` they share; docs/deploy/storage-node.md).
//!
//! - `kbf-deploy linux render-units --config FILE --out DIR [--kbf-uid N]` renders the
//!   four files from the templates compiled in from `templates/linux/` into `DIR`, laid
//!   out as under `/etc/systemd/system`: `kbf.slice`, `<MINIO_UNIT>.d/kbf.conf`,
//!   `kbf-server.service`, `kbf-daemon.service`. It writes nothing else, so it needs no
//!   `--apply`.
//! - `kbf-deploy linux check-units --config FILE --dir DIR [--installable]` checks
//!   them against the host's priorities ([`check`] lists the rules) and prints one
//!   `FAIL <rule>: <reason>` line per broken rule.
//!
//! Exit codes: 0 rendered, or every rule holds; 1 a rule is broken; 2 refused (a bad
//! setting, a template placeholder with no value, a file that cannot be read or
//! written), with a `REFUSED:` line on stderr.
//!
//! The settings are a closed set of `KEY=VALUE` lines ([`config::KEYS`],
//! `templates/linux/config.example`).

pub mod check;
pub mod config;
pub mod render;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

pub use check::{CheckError, Failure, Report};
pub use config::{LinuxConfig, LinuxError, MetaLog};
pub use render::{Templates, Units};

use crate::template::TemplateError;

/// Why a verb was refused (exit 2).
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error(transparent)]
    Config(#[from] LinuxError),
    #[error(transparent)]
    Template(#[from] TemplateError),
    #[error(transparent)]
    Check(#[from] CheckError),
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// `render-units`: render the built-in templates with the settings in `config` into
/// `out`, and return the line to print.
pub fn render_units(config: &Path, out: &Path, kbf_uid: Option<u32>) -> Result<String, Refusal> {
    let c = LinuxConfig::load(config)?;
    let units = render::render(&c, kbf_uid, &Templates::BUILT_IN)?;
    let dir = out.join(&units.minio_dir);
    std::fs::create_dir_all(&dir).map_err(|source| Refusal::Write { path: dir, source })?;
    for (name, text) in units.files() {
        let path = out.join(name);
        std::fs::write(&path, text).map_err(|source| Refusal::Write { path, source })?;
    }
    let names = units.files().map(|(name, _)| name).join(" ");
    Ok(format!(
        "rendered {names} into {} (builds get {} GiB)",
        out.display(),
        c.actions_gib
    ))
}

/// `check-units`: check the files in `dir` against the settings in `config`.
pub fn check_units(config: &Path, dir: &Path, installable: bool) -> Result<Report, Refusal> {
    let c = LinuxConfig::load(config)?;
    Ok(check::check_dir(&c, dir, installable)?)
}
