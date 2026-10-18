//! The Envoy versions kbf supports, and the check of the one installed on a host.
//!
//! Envoy is a prerequisite: an operator installs it, and nothing here fetches, pins or
//! unpacks one. [`check_installed`] runs the installed binary's `--version` and
//! [`check_output`] reads the text it printed; both refuse a version outside
//! [`SUPPORTED`], or text that names none, with a message that says what to do. The
//! installer's `check` and the Envoy unit's `ExecStartPre` call them.
//!
//! The range is what the tests ran: CI runs the real-Envoy tests (`crates/kbf-it/tests/
//! envoy_box`) on 1.39.3 (the CI-only pin in `.github/workflows/ci.yml`), and the swap
//! spike (`enable_reuse_port` with a graceful drain) ran 1.38.4 into 1.39.3. So 1.38.0 up
//! to, not including, 1.40.0.

use std::ffi::OsString;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use crate::run::{Runner, Spawn};

/// An Envoy release, `major.minor.patch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EnvoyVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl fmt::Display for EnvoyVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

const fn v(major: u32, minor: u32, patch: u32) -> EnvoyVersion {
    EnvoyVersion {
        major,
        minor,
        patch,
    }
}

/// The supported versions: the first, up to and not including the second.
pub const SUPPORTED: (EnvoyVersion, EnvoyVersion) = (v(1, 38, 0), v(1, 40, 0));

/// How long `envoy --version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Why the installed Envoy is refused.
#[derive(Debug, thiserror::Error)]
pub enum VersionError {
    #[error("cannot run {path} --version: {source}")]
    Run {
        path: String,
        source: std::io::Error,
    },
    #[error("{path} --version did not finish ({why})")]
    Failed { path: String, why: String },
    #[error(
        "cannot read an Envoy version from {output:?} (want a line like `envoy  version: <sha>/1.39.3/Clean/RELEASE/BoringSSL`)"
    )]
    Unparsable { output: String },
    #[error(
        "Envoy {found} is outside the versions kbf supports ({min} up to, not including, {below}); install one in that range (kbf never fetches Envoy)"
    )]
    OutOfRange {
        found: EnvoyVersion,
        min: EnvoyVersion,
        below: EnvoyVersion,
    },
}

/// The version `envoy --version` printed: a line `<argv0>  version: <sha>/<version>/...`
/// (Envoy prints the path it was run by, so the first word is not always `envoy`); the
/// version is the second `/`-separated field after `version:`, three plain numbers (a
/// `-dev` or any other suffix is refused), and the first field a hexadecimal revision.
fn parse(output: &str) -> Option<EnvoyVersion> {
    let rest = output
        .lines()
        .find_map(|l| l.split_once("  version:").map(|(_, rest)| rest))?;
    let mut fields = rest.trim().split('/');
    let sha = fields.next()?;
    if sha.len() < 7 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut parts = fields.next()?.split('.');
    let mut number = || {
        let p = parts.next()?;
        (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse().ok())
            .flatten()
    };
    let version = v(number()?, number()?, number()?);
    parts.next().is_none().then_some(version)
}

/// Check the text `envoy --version` printed; the version when it is supported.
pub fn check_output(output: &str) -> Result<EnvoyVersion, VersionError> {
    let found = parse(output).ok_or_else(|| VersionError::Unparsable {
        output: output.trim().chars().take(200).collect(),
    })?;
    let (min, below) = SUPPORTED;
    if found < min || found >= below {
        return Err(VersionError::OutOfRange { found, min, below });
    }
    Ok(found)
}

/// Run `envoy --version` through `runner` and check what it printed.
pub fn check_installed(runner: &dyn Runner, envoy: &Path) -> Result<EnvoyVersion, VersionError> {
    let path = envoy.display().to_string();
    let spawn = Spawn {
        program: envoy.to_owned(),
        args: vec![OsString::from("--version")],
        env: Vec::new(),
        cwd: "/".into(),
        run_as: None,
        timeout: VERSION_TIMEOUT,
    };
    let ran = runner.run(&spawn).map_err(|source| VersionError::Run {
        path: path.clone(),
        source,
    })?;
    if ran.timed_out || ran.code != Some(0) {
        let why = if ran.timed_out {
            "timed out".to_owned()
        } else {
            format!("exit {:?}", ran.code)
        };
        return Err(VersionError::Failed { path, why });
    }
    check_output(&String::from_utf8_lossy(&ran.stdout))
}
