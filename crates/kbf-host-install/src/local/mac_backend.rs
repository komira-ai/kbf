//! A Mac: the kit tree and the launchd job of the daemon ([`crate::mac`]).
//!
//! The release is checked and laid out as `releases/<commit>/kbf` by the same code as the
//! ssh path's install (fetch from GitHub as `nobody`, `gh attestation verify` offline,
//! Mach-O, quarantine, signature, the daemon's own `--check`); only the swap differs. The
//! ssh path's apply job moves the links and restarts the daemon in a launchd job of its own
//! and ends; here `switch` writes the pending marker, `<work>/pending.json` (root's, 0600),
//! before the first link moves, and the same swap code moves the links and restarts the
//! daemon. The marker names the release that ran and the links as they were, so a revert,
//! by the deadline or by request, puts both links back and starts the old release without
//! the new one running. The marker is on disk: the first tick after a reboot reads it.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::answer::Code;
use super::backend::{Backend, Fault, Pending};
use crate::mac::apply::{self, Plan};
use crate::mac::channel::MacAllowlist;
use crate::mac::kit::{self, Links};
use crate::mac::layout::{BINARY, MacHost};
use crate::mac::retain;
use crate::mac::{self, conf, stage};
use crate::outcome::{FailureCode, Outcome, Refusal};
use crate::request::{Channel, Version};
use crate::runner::Runner;
use kbf_installer::retain::Kept;

/// What a switch leaves behind until it is confirmed or reverted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    /// The release on trial.
    commit: String,
    /// The release that ran, and the link `previous` named, as they were.
    from: String,
    from_previous: Option<String>,
    deadline_unix: u64,
}

/// A Mac as the backend of an update.
pub struct MacBackend {
    host: MacHost,
    runner: Box<dyn Runner>,
}

impl MacBackend {
    pub fn new(host: MacHost, runner: Box<dyn Runner>) -> Self {
        Self { host, runner }
    }

    fn marker_file(&self) -> PathBuf {
        self.host.pending_marker()
    }

    fn read_marker(&self) -> Result<Option<Marker>, Fault> {
        let unavailable = |why: String| Fault::failed(Code::StateUnavailable, why);
        match fs::symlink_metadata(self.marker_file()) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(unavailable(format!("pending.json: {e}"))),
            Ok(_) => {}
        }
        let text = self
            .host
            .trust
            .read(&self.marker_file())
            .map_err(|e| unavailable(format!("pending.json: {e}")))?;
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| unavailable(format!("pending.json: {e}")))
    }

    fn write_marker(&self, marker: &Marker) -> Result<(), Fault> {
        let unavailable = |why: String| Fault::failed(Code::StateUnavailable, why);
        let text = serde_json::to_string(marker).map_err(|e| unavailable(e.to_string()))?;
        let aside = self.host.work.join("pending.json.new");
        let _ = fs::remove_file(&aside);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&aside)
            .and_then(|mut file| {
                file.write_all(text.as_bytes())?;
                file.sync_all()
            })
            .and_then(|()| fs::rename(&aside, self.marker_file()))
            .map_err(|e| unavailable(format!("pending.json: {e}")))
    }

    fn clear_marker(&self) -> Result<(), Fault> {
        match fs::remove_file(self.marker_file()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(Fault::failed(
                Code::StateUnavailable,
                format!("pending.json: {e}"),
            )),
            _ => Ok(()),
        }
    }
}

/// The refusal of the ssh path's code, as the socket tells it.
fn refusal_code(refusal: Refusal) -> Code {
    match refusal {
        Refusal::ChannelNotAllowed => Code::ChannelNotAllowed,
        Refusal::ConfigUnsafe | Refusal::KeyUntrusted => Code::ConfigUnsafe,
        Refusal::UnknownRelease => Code::UnknownRelease,
        Refusal::AmbiguousRelease => Code::AmbiguousRelease,
        Refusal::FetchFailed => Code::FetchFailed,
        _ => Code::ConfigInvalid,
    }
}

/// What the stage's outcome means to the socket: a refusal or a check that failed changed
/// nothing; a failure to make the stage is a failure.
fn stage_fault(outcome: Outcome) -> Fault {
    let detail = outcome.stderr_detail().unwrap_or_default();
    match outcome {
        Outcome::Refused(refusal) | Outcome::RefusedBecause(refusal, _) => {
            Fault::refused(refusal_code(refusal), detail)
        }
        Outcome::Failed(failure) if failure.code == FailureCode::VerifyFailed => {
            Fault::refused(Code::VerifyFailed, detail)
        }
        _ => Fault::failed(Code::StageFailed, detail),
    }
}

impl Backend for MacBackend {
    fn lock_file(&self) -> PathBuf {
        // In the work directory, which `mac::ensure_work` makes before the server serves.
        self.host.lock_file()
    }

    fn node_id(&self) -> Option<String> {
        conf::node_id(&self.host.kit_trust, &self.host.node_conf)
    }

    fn current(&self) -> Result<Option<String>, Fault> {
        kit::links(&self.host)
            .map(|links| links.current)
            .map_err(|why| Fault::failed(Code::StateUnavailable, why))
    }

    fn pending(&self) -> Result<Option<Pending>, Fault> {
        Ok(self.read_marker()?.map(|marker| Pending {
            commit: marker.commit,
            from: marker.from,
            deadline_unix: Some(marker.deadline_unix),
            activation: String::new(),
        }))
    }

    fn has(&self, commit: &str) -> bool {
        self.host
            .kit_trust
            .check_file(&self.host.release(commit).join(BINARY))
            .is_ok()
    }

    fn stage(&self, version: &Version, channel: &Channel) -> Result<String, Fault> {
        let host = &self.host;
        mac::ensure_work(host).map_err(stage_fault)?;
        mac::check_kit(host).map_err(stage_fault)?;
        let allowlist = MacAllowlist::load(&host.trust, &host.allowlist)
            .map_err(|r| Fault::refused(refusal_code(r), "the allowlist"))?;
        let channel = allowlist
            .channel(&host.trust, channel)
            .map_err(|e| Fault::refused(refusal_code(e.refusal()), e.to_string()))?;
        let staged = stage::stage(host, &*self.runner, &channel, version).map_err(stage_fault)?;
        let plan = Plan {
            commit: staged.commit.clone(),
            version: version.as_str().to_owned(),
            binary_sha256: staged.binary_sha256,
            before_current: None,
            before_previous: None,
        };
        let laid = apply::lay_out(host, &*self.runner, &plan, &host.stage().join(BINARY));
        let _ = fs::remove_dir_all(host.stage());
        laid.map_err(|why| Fault::failed(Code::StageFailed, why))?;
        Ok(staged.commit)
    }

    fn switch(&self, commit: &str, window_secs: u64, now: u64) -> Result<u64, Fault> {
        let before =
            kit::links(&self.host).map_err(|why| Fault::failed(Code::StateUnavailable, why))?;
        let Some(from) = before.current.clone() else {
            return Err(Fault::refused(
                Code::NoCurrent,
                "the kit has no current release",
            ));
        };
        let deadline_unix = now + window_secs;
        // The marker first: a kill between here and the confirm leaves a host that goes back.
        self.write_marker(&Marker {
            commit: commit.to_owned(),
            from,
            from_previous: before.previous.clone(),
            deadline_unix,
        })?;
        match apply::switch(&self.host, &*self.runner, commit, &before) {
            Ok(()) => Ok(deadline_unix),
            Err(why) => {
                self.clear_marker()?;
                let code = if why.ends_with(":rolled_back") || why == "links_failed" {
                    Code::SwitchFailed
                } else {
                    Code::RevertFailed
                };
                Err(Fault::failed(code, why))
            }
        }
    }

    fn confirm(&self, _pending: &Pending) -> Result<(), Fault> {
        self.clear_marker()
    }

    fn revert(&self, _pending: &Pending) -> Result<(), Fault> {
        let Some(marker) = self.read_marker()? else {
            return Ok(());
        };
        let before = Links {
            current: Some(marker.from),
            previous: marker.from_previous,
        };
        apply::switch_back(&self.host, &*self.runner, &before)
            .map_err(|tail| Fault::failed(Code::RevertFailed, tail))?;
        self.clear_marker()
    }

    fn kept(&self) -> Result<Vec<Kept>, Fault> {
        let on_trial = self.read_marker()?.map(|m| m.commit);
        retain::kept(&self.host, on_trial.as_deref())
            .map_err(|why| Fault::failed(Code::StateUnavailable, why))
    }

    fn retain_file(&self) -> Option<PathBuf> {
        Some(retain::file(&self.host))
    }

    fn prune(&self, keep: usize) -> Result<String, Fault> {
        retain::prune(&self.host, keep).map_err(|why| Fault::failed(Code::StateUnavailable, why))
    }
}
