//! The host's settings: what only root may decide about how a release is checked.
//!
//! `installer.conf` in the trust directory (`/usr/share/kbf/trust/` on Linux, [`crate::layout::Layout::settings`]) holds
//! `KEY=VALUE` lines (blank lines and `#` comments are skipped; every key at most once;
//! a key this build does not know is refused):
//!
//! ```text
//! SOURCE_REFS=refs/heads/main       branches a release may be attested for, space separated
//! RUN_AS_UID=65534                  who untrusted bytes are handled as (never 0)
//! RUN_AS_GID=65534
//! ```
//!
//! Every key has a default (the table above, `nobody`; on macOS `nobody` is uid and gid
//! 4294967294), and a host with no file uses them. A file that is there must be root's
//! alone ([`crate::trust`]); a file that is not is refused, not ignored, since the
//! settings it may hold decide what is believed.
//!
//! Neither the inbox nor the frontend supplies a ref: the refs a release may come from are
//! this host's own setting, so a hostile offer cannot name a branch it likes. The one other
//! file that may touch the list is `installer.toml`, whose `source_refs` can only narrow it
//! ([`Settings::narrowed_to`]).

use std::time::Duration;

use kbf_host::exec::{Entry, Exec, Kind};
use kbf_host::path::HostPath;

use crate::attest::{Signer, SourceRef};
use crate::binary::Arch;
use crate::layout::Layout;
use crate::refusal::{Refusal, SettingsFault, TrustFault};
use crate::run::{RunAs, nobody};
use crate::trust::trusted_chain;

/// The branch a release is built from unless the host's settings say otherwise.
pub const DEFAULT_REF: &str = "refs/heads/main";

/// How long `gh`, and each other tool the stage runs, may take.
pub const TOOL_TIMEOUT: Duration = Duration::from_secs(120);

/// How the installer checks a release on this host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// The branches an attestation may name as the release's source, in order.
    pub source_refs: Vec<SourceRef>,
    /// The user untrusted bytes are handled as: `gh` reads the tarball and the staged
    /// binary runs `--version --json` as it.
    pub run_as: RunAs,
    /// The CPU this host runs, which a staged binary must be built for.
    pub arch: Arch,
    /// How long each tool may take.
    pub timeout: Duration,
    /// The repositories and workflows an attestation may name, tried in order. Not a
    /// setting of `installer.conf`: the default is kbf's, and only a release channel of
    /// the host-install allowlist replaces it.
    pub signers: Vec<Signer>,
}

impl Settings {
    /// The settings of a host with no settings file.
    pub fn defaults(host: &Layout) -> Settings {
        Settings {
            source_refs: vec![SourceRef::new(DEFAULT_REF).expect("the default ref is a ref")],
            run_as: nobody(host.platform()),
            arch: Arch::current(),
            timeout: TOOL_TIMEOUT,
            signers: vec![Signer::current(), Signer::former()],
        }
    }

    /// The host's settings: the file when there is one (and it is trusted and valid),
    /// else the defaults. `trust` answers who owns what and reads the file: the host's
    /// own [`kbf_host::HostExec`], or a test's.
    pub fn load(trust: &dyn Exec, host: &Layout) -> Result<Settings, Refusal> {
        Settings::load_from(trust, host, &host.settings())
    }

    /// [`Settings::load`] for the settings file at `file`.
    pub fn load_from(
        trust: &dyn Exec,
        host: &Layout,
        file: &HostPath,
    ) -> Result<Settings, Refusal> {
        let untrusted = |fault| Refusal::Untrusted {
            path: file.as_str().into(),
            fault,
        };
        let read = trust
            .inspect(file)
            .map_err(|e| untrusted(TrustFault::Unreadable(e)))?;
        let Some(entry) = read else {
            return Ok(Settings::defaults(host));
        };
        // A link, a directory or a device where the file should be.
        let Entry::File { bytes, .. } = entry else {
            return Err(untrusted(TrustFault::Link));
        };
        trusted_chain(trust, file, Kind::File)?;
        let refuse = |fault| Refusal::Settings {
            path: file.as_str().into(),
            fault,
        };
        let text = String::from_utf8(bytes).map_err(|_| refuse(SettingsFault::NotUtf8))?;
        parse(&text, Settings::defaults(host)).map_err(refuse)
    }

    /// These settings with `installer.toml`'s `source_refs` applied: the refs of the trust
    /// files that `wanted` also names, in the trust files' order. An empty `wanted` is no
    /// narrowing. A ref `wanted` names that the trust files do not allow is dropped, so the
    /// result is never wider than the trust files' list.
    ///
    /// # Errors
    /// [`NoCommonRef`] when `wanted` is not empty and names none the trust files allow: the
    /// host would accept no release, and the file cannot add a ref to make one.
    pub fn narrowed_to(mut self, wanted: &[String]) -> Result<Settings, NoCommonRef> {
        if wanted.is_empty() {
            return Ok(self);
        }
        self.source_refs
            .retain(|allowed| wanted.iter().any(|w| w == allowed.as_str()));
        if self.source_refs.is_empty() {
            return Err(NoCommonRef);
        }
        Ok(self)
    }
}

/// `installer.toml`'s `source_refs` names no ref that `installer.conf` allows.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "installer.toml source_refs names no ref that installer.conf's SOURCE_REFS allows (it may narrow that list, never widen it)"
)]
pub struct NoCommonRef;

/// `text` applied over `settings`.
fn parse(text: &str, mut settings: Settings) -> Result<Settings, SettingsFault> {
    let mut seen: Vec<&'static str> = Vec::new();
    let mut run_as = settings.run_as;
    for (index, line) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(SettingsFault::Syntax { line: line_no });
        };
        let key = match key.trim() {
            "SOURCE_REFS" => "SOURCE_REFS",
            "RUN_AS_UID" => "RUN_AS_UID",
            "RUN_AS_GID" => "RUN_AS_GID",
            other => {
                return Err(SettingsFault::UnknownKey {
                    line: line_no,
                    key: other.to_owned(),
                });
            }
        };
        if seen.contains(&key) {
            return Err(SettingsFault::Repeated { line: line_no, key });
        }
        seen.push(key);
        let value = value.trim();
        let number = |line: usize| {
            value
                .parse::<u32>()
                .map_err(|_| SettingsFault::NotANumber { line, key })
        };
        match key {
            "SOURCE_REFS" => {
                let refs: Result<Vec<SourceRef>, SettingsFault> = value
                    .split_whitespace()
                    .map(|r| {
                        SourceRef::new(r).map_err(|_| SettingsFault::BadRef {
                            line: line_no,
                            value: r.to_owned(),
                        })
                    })
                    .collect();
                settings.source_refs = refs?;
            }
            "RUN_AS_UID" => run_as.uid = number(line_no)?,
            _ => run_as.gid = number(line_no)?,
        }
    }
    if settings.source_refs.is_empty() {
        return Err(SettingsFault::NoRefs);
    }
    if run_as.uid == 0 || run_as.gid == 0 {
        return Err(SettingsFault::RunAsRoot);
    }
    settings.run_as = run_as;
    Ok(settings)
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
