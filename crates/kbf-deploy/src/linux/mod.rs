//! The `linux` kit: the systemd units of one mode of the one `kbf` binary (and the
//! `kbf.slice` they share), its build images and its swapfile.
//!
//! - `kbf-deploy linux render-units --config FILE --out DIR --mode MODE [--kbf-uid N]`
//!   renders `kbf.slice` and `kbf-<mode>.service` from the templates compiled in from
//!   `templates/linux/` into `DIR`, laid out as under `/etc/systemd/system`
//!   ([`modes`]). It writes nothing else, so it needs no `--apply`.
//! - `kbf-deploy linux check-units --config FILE --dir DIR --mode MODE [--installable]
//!   [--kbf-bin PATH]` checks the two, each with its drop-ins (`<name>.d/*.conf`),
//!   against [`modes`]' rules (docs/deploy/kbf-modes.md) and prints one
//!   `FAIL <rule>: <reason>` line per broken rule. It also runs
//!   `<kbf> check-args <mode> <flags...>` (`--kbf-bin PATH`, or the unit's own binary
//!   when it is installed; a `SKIP parse:` line when it is not).
//!
//! Exit codes: 0 rendered, or every rule holds; 1 a rule is broken; 2 refused (a bad
//! setting, a template placeholder with no value, a file that cannot be read or
//! written, a `--kbf-bin` that cannot be run), with a `REFUSED:` line on stderr.
//!
//! Two verbs change the host, as root. Each checks the host first and prints its plan
//! as `DRY-RUN:` lines; `--apply` carries it out, and a re-run after an apply plans
//! nothing. They exit 0 done (or nothing to do), 1 a step or the check after an apply
//! failed, 2 refused.
//! - `kbf-deploy linux install-units --config FILE --mode MODE [--bin-dir DIR]
//!   [--apply]` ([`install_mode`]): `kbf` (with `--bin-dir`), `kbf.slice` and one built
//!   mode's unit, the directories its flags name, and the unit started.
//! - `kbf-deploy linux swap --config FILE [--kubelet-allows-swap] [--remove] [--apply]`
//!   ([`swap`]): the swapfile, active and in `/etc/fstab`.
//!
//! `kbf-deploy linux meta-backup --meta-dir DIR [--hot-dir DIR] --out FILE [--apply]`
//! copies kbf-server's metadata log into one archive in a brief stop (as root), and
//! `linux meta-verify --archive FILE --into DIR` extracts and checks one ([`meta`]).
//!
//! `kbf-deploy linux verify-cert --row ROW --cert FILE --ca-cert FILE [--node-id ID]`
//! ([`verify_cert`]) checks, read only, that a certificate is its row of the farm's
//! certificate table: its one role, its extended key usage, its chain to the CA.
//!
//! The settings are a closed set of `KEY=VALUE` lines ([`config::KEYS`],
//! `templates/linux/config.example`).

pub mod config;
pub mod cpuset;
pub mod credential;
mod glob;
pub mod handover;
pub mod host;
pub mod install_mode;
mod limits;
pub mod meta;
pub mod modes;
pub mod restart;
mod runtime;
pub mod shared;
pub mod stop;
pub mod stores;
pub mod swap;
mod traverse;
pub mod verify_cert;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

pub use config::{LinuxConfig, LinuxError};
pub use host::HostError;
pub use modes::Mode;
pub use shared::{CheckError, Failure, Report};

use crate::template::TemplateError;

/// Why a verb was refused (exit 2).
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error(transparent)]
    Config(#[from] LinuxError),
    #[error(transparent)]
    Template(#[from] TemplateError),
    #[error(transparent)]
    Render(#[from] modes::RenderError),
    #[error(transparent)]
    Check(#[from] CheckError),
    /// `--kbf-bin` is not an absolute path.
    #[error("--kbf-bin: {0}")]
    KbfBin(#[from] kbf_host::path::PathError),
    #[error(transparent)]
    Parse(modes::ParseError),
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// `render-units`: `kbf.slice` and the unit of `mode`, rendered with the settings
/// in `config` into `out`, and the line to print.
pub fn render_mode_unit(
    config: &Path,
    out: &Path,
    kbf_uid: Option<u32>,
    mode: Mode,
) -> Result<String, Refusal> {
    let c = LinuxConfig::load(config)?;
    let slice = shared::render_slice(&c, kbf_uid)?;
    let unit = modes::render(&c, kbf_uid, mode)?;
    std::fs::create_dir_all(out).map_err(|source| Refusal::Write {
        path: out.to_owned(),
        source,
    })?;
    let name = mode.unit_name();
    for (file, text) in [(shared::SLICE, &slice), (name.as_str(), &unit)] {
        let path = out.join(file);
        std::fs::write(&path, text).map_err(|source| Refusal::Write { path, source })?;
    }
    Ok(format!(
        "rendered {} {name} into {} (kbf {mode})",
        shared::SLICE,
        out.display()
    ))
}

/// `check-units`: check `kbf-<mode>.service` and `kbf.slice` in `dir`, each
/// with its drop-ins (`<name>.d/*.conf`), against the settings in `config`, on the host
/// `exec` reads: a built mode's flags with `kbf_bin` or the unit's own kbf
/// ([`modes::check_on_host`]).
pub fn check_mode_unit<E: kbf_host::exec::Exec>(
    exec: &E,
    config: &Path,
    dir: &Path,
    mode: Mode,
    installable: bool,
    kbf_bin: Option<&Path>,
) -> Result<Report, Refusal> {
    let c = LinuxConfig::load(config)?;
    let unit = read_unit(dir, &mode.unit_name())?;
    let slice = read_unit(dir, shared::SLICE)?;
    let bin = kbf_bin
        .map(|p| kbf_host::path::HostPath::new(&p.to_string_lossy()))
        .transpose()?;
    modes::check_on_host(exec, &c, mode, &unit, &slice, installable, bin.as_ref()).map_err(|e| {
        match e {
            modes::ParseError::Unit { name, source } => CheckError::Unit { name, source }.into(),
            e @ modes::ParseError::Run { .. } => Refusal::Parse(e),
        }
    })
}

/// The unit `name` in `dir` and its drop-ins: every `*.conf` file in `<name>.d/`, by
/// file name (the order systemd applies them), named `<name>.d/<file>`.
fn read_unit(dir: &Path, name: &str) -> Result<modes::UnitText, CheckError> {
    let read = |path: PathBuf| {
        std::fs::read_to_string(&path).map_err(|source| CheckError::Read {
            path: path.display().to_string(),
            source,
        })
    };
    let mut unit = modes::UnitText::new(name, read(dir.join(name))?);
    let drop_ins = dir.join(format!("{name}.d"));
    let unreadable = |source| CheckError::Read {
        path: drop_ins.display().to_string(),
        source,
    };
    let entries = match std::fs::read_dir(&drop_ins) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(unit),
        Err(e) => return Err(unreadable(e)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let file = entry.map_err(unreadable)?.file_name();
        let file = file.to_string_lossy().into_owned();
        if file.ends_with(".conf") {
            files.push(file);
        }
    }
    files.sort();
    for file in files {
        let text = read(drop_ins.join(&file))?;
        unit = unit.with_drop_in(format!("{name}.d/{file}"), text);
    }
    Ok(unit)
}
