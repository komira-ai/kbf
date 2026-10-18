//! What the installer asks of the frontend instances besides systemd: whether one answers
//! `/readyz` (one loopback `GET` through the host's [`Exec`], which bounds it), and which
//! handover protocol a binary speaks (its own `--version --json`, run as the unprivileged
//! user the other probes use).

use std::path::Path;
use std::time::Duration;

use kbf_host::{Exec, HttpUrl};

use crate::binary::Version;
use crate::layout::Platform;
use crate::run::{Runner, Spawn, nobody};

/// How long a probe of a binary may run.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// A readiness URL from a flag: `http://` to a loopback address written as a number
/// (`127.0.0.1`, `[::1]`). The probe is a local question; a name or another host is refused.
///
/// # Errors
/// Why the text is not one.
pub fn loopback(text: &str) -> Result<HttpUrl, String> {
    let url = HttpUrl::new(text).map_err(|e| e.to_string())?;
    match url.host().parse::<std::net::IpAddr>() {
        Ok(ip) if ip.is_loopback() => Ok(url),
        _ => Err(format!(
            "{text:?} is not on a loopback address (127.0.0.1 or [::1])"
        )),
    }
}

/// Whether `url` answers 2xx now. An instance that does not answer, or answers anything
/// else, is not ready.
pub fn ready<E: Exec>(exec: &E, url: &HttpUrl) -> bool {
    exec.http_status(url)
        .is_ok_and(|status| (200..300).contains(&status))
}

/// The handover protocol a release's `kbf` speaks.
pub trait HandoverProbe {
    /// Run `binary --version --json` and say what it reports as `handover_protocol`.
    ///
    /// # Errors
    /// Why the binary could not say: it did not run, did not exit 0, printed something
    /// else, or reports no handover protocol (a release from before it was reported cannot
    /// be handed over to).
    fn handover_protocol(&self, binary: &Path) -> Result<u64, String>;
}

/// The probe that runs the binary, through the installer's own [`Runner`].
pub struct RunnerProbe<R: Runner> {
    pub runner: R,
    pub platform: Platform,
}

impl<R: Runner> HandoverProbe for RunnerProbe<R> {
    fn handover_protocol(&self, binary: &Path) -> Result<u64, String> {
        let ran = self
            .runner
            .run(&Spawn {
                program: binary.to_owned(),
                args: vec!["--version".into(), "--json".into()],
                env: Vec::new(),
                cwd: "/".into(),
                run_as: Some(nobody(self.platform)),
                timeout: PROBE_TIMEOUT,
            })
            .map_err(|e| format!("{} did not run: {e}", binary.display()))?;
        if ran.timed_out || ran.code != Some(0) {
            return Err(format!(
                "{} --version --json did not finish with exit 0",
                binary.display()
            ));
        }
        let version = Version::parse(&ran.stdout).map_err(|e| e.to_string())?;
        version
            .handover_protocol
            .ok_or_else(|| format!("{} does not report a handover protocol", binary.display()))
    }
}

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;
