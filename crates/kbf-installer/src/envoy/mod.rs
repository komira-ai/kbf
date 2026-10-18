//! An example Envoy config for a frontend host: render its two files, check them, and write
//! them, for the operator to start an Envoy from once. The load balancer is the operator's:
//! the frontend update never calls this module and never edits, reloads or restarts one.
//!
//! One Envoy in front of two kbf instances, `a` and `b`, both listed for ever in a static
//! config. [`render`] turns [`BoxSettings`] (the ports, files and limits the host already
//! decides) into `envoy.json` and `envoy.args` (Envoy's hot-restart flags); [`check`] and
//! [`check_dir`] hold those files to the properties a long silent `Execute` depends on:
//! route timeout 0, no `max_stream_duration`, `Execute` and `WaitExecution` idle timeout
//! 0, breakers sized from `max_concurrent_streams`, a health failure that leaves open
//! streams alone, no retry on ByteStream or on `unavailable`, and a parent shutdown at
//! least as long as the longest action, listeners with `enable_reuse_port` true so a new
//! Envoy can bind beside the old ([`CheckFailure::NAMES`] lists every check).
//!
//! This is a library with no command line. The settings come from the caller; no file or
//! environment variable is read, and nothing but [`write_files`] writes. Envoy itself is a
//! prerequisite the operator installs: [`check_installed`] refuses one outside
//! [`SUPPORTED`], and nothing here fetches or pins a binary.
//!
//! Both files are JSON printed byte for byte as Python's `json.dumps(indent=2, sort_keys=True)`, so the same settings always give
//! the same bytes.
//!
//! The files are what `kbf-deploy envoy render --topology box` writes for the same
//! numbers (the golden in `tests/golden/envoy/box`); that tool is frozen and still holds
//! its copy.

mod check;
mod config;
mod failure;
mod json;
mod render;
mod rules;
mod version;

#[cfg(test)]
mod check_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod version_tests;

use std::path::{Path, PathBuf};

pub use config::{BoxSettings, Instance, SettingsError};
pub use failure::CheckFailure;
pub use render::{DRAIN_SECS, REAPI_CLUSTER, Rendered, WORKER_CLUSTER, render};
pub use rules::{HEALTH_SERVICES, METHODS};
pub use version::{EnvoyVersion, SUPPORTED, VersionError, check_installed, check_output};

/// The Envoy bootstrap file.
pub const ENVOY: &str = "envoy.json";
/// Envoy's hot-restart flags, which [`write_files`] writes beside [`ENVOY`].
pub const ENVOY_ARGS: &str = "envoy.args";

/// Why a render or a check could not be done.
#[derive(Debug, thiserror::Error)]
pub enum EnvoyError {
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not JSON: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Write `rendered` into `dir` (created if absent) as [`ENVOY`] and [`ENVOY_ARGS`].
pub fn write_files(dir: &Path, rendered: &Rendered) -> Result<(), EnvoyError> {
    std::fs::create_dir_all(dir).map_err(|source| EnvoyError::Write {
        path: dir.to_owned(),
        source,
    })?;
    for (name, text) in [(ENVOY, &rendered.envoy), (ENVOY_ARGS, &rendered.args)] {
        let path = dir.join(name);
        std::fs::write(&path, text).map_err(|source| EnvoyError::Write { path, source })?;
    }
    Ok(())
}

/// Check `rendered` against `settings`: every failed check, none when the files pass.
/// Refused when `settings` cannot be rendered, or when `rendered.envoy` is not JSON.
pub fn check(settings: &BoxSettings, rendered: &Rendered) -> Result<Vec<CheckFailure>, EnvoyError> {
    check_texts(settings, Path::new(ENVOY), &rendered.envoy, &rendered.args)
}

/// Check the files [`write_files`] wrote into `dir` against `settings`.
pub fn check_dir(settings: &BoxSettings, dir: &Path) -> Result<Vec<CheckFailure>, EnvoyError> {
    let read = |name: &str| {
        let path = dir.join(name);
        std::fs::read_to_string(&path).map_err(|source| EnvoyError::Read { path, source })
    };
    let envoy = read(ENVOY)?;
    let args = read(ENVOY_ARGS)?;
    check_texts(settings, &dir.join(ENVOY), &envoy, &args)
}

fn check_texts(
    settings: &BoxSettings,
    envoy_path: &Path,
    envoy: &str,
    args: &str,
) -> Result<Vec<CheckFailure>, EnvoyError> {
    settings.validate()?;
    let doc = serde_json::from_str(envoy).map_err(|source| EnvoyError::Parse {
        path: envoy_path.to_owned(),
        source,
    })?;
    Ok(check::check_box(&doc, args, settings))
}
