//! The real host: `systemctl` and the `kbf` binaries, started with `std::process::Command`, no
//! shell, by absolute path.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::host::{Check, Host, HostError, UnitState};
use crate::{VersionError, native_version};

const SYSTEMCTL: &str = "/usr/bin/systemctl";
const KBF: &str = "/usr/bin/kbf";

/// The exit code of `kbf --check` that means not ready yet.
const NOT_READY: i32 = 75;
/// The exit code of `kbf --check` that means the configuration is refused.
const REFUSED: i32 = 2;

/// The host of a real run.
pub struct Systemd {
    systemctl: PathBuf,
    kbf: PathBuf,
}

impl Default for Systemd {
    fn default() -> Systemd {
        Systemd::new()
    }
}

impl Systemd {
    /// `/usr/bin/systemctl` and `/usr/bin/kbf`.
    pub fn new() -> Systemd {
        Systemd::with_programs(SYSTEMCTL.into(), KBF.into())
    }

    /// Other programs in their places (a test's).
    pub fn with_programs(systemctl: PathBuf, kbf: PathBuf) -> Systemd {
        Systemd { systemctl, kbf }
    }

    fn systemctl(&self, verb: &str, unit: &str) -> Result<Output, HostError> {
        run(&self.systemctl, &[OsStr::new(verb), OsStr::new(unit)])
    }

    fn systemctl_ok(&self, verb: &str, unit: &str) -> Result<(), HostError> {
        let output = self.systemctl(verb, unit)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(HostError(format!(
                "systemctl {verb} {unit} {}: {}",
                describe(&output),
                tail(&output.stderr)
            )))
        }
    }
}

fn run(program: &Path, args: &[&OsStr]) -> Result<Output, HostError> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| HostError(format!("cannot run {}: {e}", program.display())))
}

fn describe(output: &Output) -> String {
    match output.status.code() {
        Some(code) => format!("exited {code}"),
        None => "was killed by a signal".to_owned(),
    }
}

/// The last 300 characters of a stream, as text, for a log line.
fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let chars: Vec<char> = text.trim().chars().collect();
    chars[chars.len().saturating_sub(300)..].iter().collect()
}

/// The native version `kbf --version --json` names: its `version` and its `seq`.
///
/// # Errors
/// The text is not a JSON object with a text `version` and a whole `seq`, or the version is not
/// three numbers.
pub fn parse_version_output(text: &str) -> Result<String, VersionError> {
    let bad = |why: &str| VersionError::BadOutput(why.to_owned());
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|e| VersionError::BadOutput(e.to_string()))?;
    let version = value
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or_else(|| bad("no text field \"version\""))?;
    let seq = value
        .get("seq")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| bad("no whole-number field \"seq\""))?;
    native_version(version, seq)
}

impl Host for Systemd {
    fn check(&self, kbf: &Path, mode: &str) -> Result<Check, HostError> {
        let output = match run(
            kbf,
            &[OsStr::new(&format!("--mode={mode}")), OsStr::new("--check")],
        ) {
            Ok(output) => output,
            Err(e) => return Ok(Check::Failed(e.0)),
        };
        Ok(match output.status.code() {
            Some(0) => Check::Ready,
            Some(NOT_READY) => Check::NotReady,
            Some(REFUSED) => Check::Refused,
            _ => Check::Failed(format!(
                "{} --check {}: {}",
                kbf.display(),
                describe(&output),
                tail(&output.stderr)
            )),
        })
    }

    fn state(&self, unit: &str) -> Result<UnitState, HostError> {
        // `is-active` exits non-zero for every state but active; the word is the answer.
        let output = self.systemctl("is-active", unit)?;
        Ok(UnitState::parse(&String::from_utf8_lossy(&output.stdout)))
    }

    fn stop(&self, unit: &str) -> Result<(), HostError> {
        self.systemctl_ok("stop", unit)
    }

    fn start(&self, unit: &str) -> Result<(), HostError> {
        self.systemctl_ok("start", unit)
    }

    fn restart(&self, unit: &str) -> Result<(), HostError> {
        self.systemctl_ok("restart", unit)
    }

    fn installed_version(&self) -> Result<String, HostError> {
        let output = run(&self.kbf, &[OsStr::new("--version"), OsStr::new("--json")])?;
        if !output.status.success() {
            return Err(HostError(format!(
                "kbf --version --json {}: {}",
                describe(&output),
                tail(&output.stderr)
            )));
        }
        parse_version_output(&String::from_utf8_lossy(&output.stdout))
            .map_err(|e| HostError(e.to_string()))
    }
}
