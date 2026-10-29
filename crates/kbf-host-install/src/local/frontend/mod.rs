//! A frontend that runs as a container: the backend of `kbf-host-install --serve
//! --frontend-container`.
//!
//! The release is an image. [`Frontend`] answers the same five requests as the other backends
//! ([`crate::local::backend::Backend`]), so the core's A/B, its pending marker and its
//! deadline revert are the ones the other hosts have:
//!
//! - **stage**: the image file of the release is fetched, checked against the release's
//!   `SHA256SUMS` and its attestation, and only then loaded ([`ImageSource`], and
//!   [`Runtime::load`]). Nothing runs differently, and a file that is not the release's is
//!   refused before the runtime sees it.
//! - **switch**: refuses when it can see that another frontend is not ready; asks the
//!   frontend to drain through its own operator API (`kbf cluster drain --frontend` inside
//!   the container, where the API is on loopback); and waits until the container has ended,
//!   as long as that takes. A configured `drain_secs` only stops the waiting: the switch
//!   then fails with the old container still running and draining, its restart policy put
//!   back, nothing stopped, removed or recorded. Only once the container has ended it writes
//!   the marker (the configuration the containers are made from, the images of both
//!   releases), stops it, removes it, makes a new one from the new image with the same
//!   configuration, and waits for `/readyz` to answer 200 and `/healthz` to name the
//!   release's commit. The trial window counts from the new container's start. When any of
//!   it fails after the marker, the container is made again from the old image and waited
//!   for the same way, and the switch fails.
//! - **confirm**: the marker is cleared and the old image is forgotten.
//! - **revert**: the container is made again from the old image, as after a failed switch.
//!   The core calls it when a release on trial is not confirmed by its deadline, whether or
//!   not the host was down meanwhile.
//!
//! What this program can see of the other frontends is `peers` of the configuration: their
//! operator APIs' `/readyz`. A peer that does not answer 200 refuses the switch before
//! anything is stopped. Without peers it knows only its own host, and the coordinator's order
//! (one frontend at a time, never the last ready one) is the guard.

pub mod config;
pub mod probe;
pub mod record;
pub mod runtime;

#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

pub use crate::pace::Pace;

use self::config::{FrontendConfig, STOP_SECS};
use self::probe::Probe;
use self::record::{Record, Slot, Trial};
use self::runtime::{Runtime, RuntimeError};
use super::answer::Code;
use super::backend::{Backend, Fault, Pending};
use crate::host::Host;
use crate::mac::release::is_commit;
use crate::request::{Channel, Version};

/// The repository the release's image is loaded as: `kbf-frontend:<commit>`.
pub const IMAGE_REPOSITORY: &str = "kbf-frontend";

/// Where the frontend's record is, below the private state directory.
pub const RECORD: &str = "frontend.json";

/// The names of the files one helper keeps, which are its alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Files {
    /// The record, in the private state directory.
    pub record: String,
    /// The A/B state, in the same directory.
    pub state: String,
    /// The install lock; none for the default configuration, whose lock is the host's.
    pub lock: Option<PathBuf>,
}

impl Files {
    /// The files of the helper for the container `bound` (a helper started with its own
    /// configuration file), or the default helper's, whose names do not change.
    pub fn of(bound: Option<&str>) -> Self {
        match bound {
            None => Self {
                record: RECORD.to_owned(),
                state: "state.json".to_owned(),
                lock: None,
            },
            Some(name) => Self {
                record: format!("frontend-{name}.json"),
                state: format!("state-frontend-{name}.json"),
                lock: Some(PathBuf::from(format!(
                    "/run/kbf-host-install-frontend-{name}.lock"
                ))),
            },
        }
    }
}

/// The tag the image of `commit` is loaded as.
pub fn image_tag(commit: &str) -> String {
    format!("{IMAGE_REPOSITORY}:{commit}")
}

/// The commit in an image reference `kbf-frontend:<commit>`.
pub fn commit_of_ref(reference: &str) -> Option<&str> {
    reference
        .strip_prefix(IMAGE_REPOSITORY)?
        .strip_prefix(':')
        .filter(|commit| is_commit(commit))
}

/// A release's image file, fetched and checked. The scratch directory that holds it is
/// removed when this is dropped.
pub struct FetchedImage {
    pub commit: String,
    pub file: PathBuf,
    scratch: Option<PathBuf>,
}

impl FetchedImage {
    /// The file `file` of `commit`, in a scratch directory that is removed with this value
    /// (none for a file that is not).
    pub fn new(commit: String, file: PathBuf, scratch: Option<PathBuf>) -> Self {
        Self {
            commit,
            file,
            scratch,
        }
    }
}

impl Drop for FetchedImage {
    fn drop(&mut self) {
        if let Some(dir) = &self.scratch {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Where a release's image file comes from.
pub trait ImageSource {
    /// Fetch the image file of the release `version` names in `channel` and check it: its
    /// sha256 against the release's `SHA256SUMS` and its attestation.
    ///
    /// # Errors
    /// A refusal for anything the configuration or the release does not allow; nothing but
    /// scratch has changed either way.
    fn fetch(&self, version: &Version, channel: &Channel) -> Result<FetchedImage, Fault>;
}

/// The frontend container of this host, as a backend of an update.
pub struct Frontend<S: ImageSource, R: Runtime, P: Probe> {
    pub source: S,
    pub runtime: R,
    pub probe: P,
    /// The paths of this program's own files and who is trusted to own them.
    pub host: Host,
    /// `host-install-frontend.toml`.
    pub config_file: PathBuf,
    /// `frontend.json`.
    pub record_file: PathBuf,
    /// The container this helper was started for, when its configuration is not the default
    /// file: the files above were named after it, so a configuration that names another
    /// container is refused.
    pub bound_name: Option<String>,
    pub pace: Pace,
}

fn runtime_fault(e: RuntimeError) -> Fault {
    Fault::failed(Code::RuntimeFailed, e.to_string())
}

fn state_fault(why: String) -> Fault {
    Fault::failed(Code::StateUnavailable, why)
}

/// The command that asks the frontend to drain, run inside its container, where the operator
/// API is on loopback and the token file is mounted.
fn drain_argv(config: &FrontendConfig) -> Vec<String> {
    [
        "/usr/bin/kbf",
        "cluster",
        "drain",
        "--frontend",
        "--token-file",
        &config.drain_token_file,
    ]
    .map(str::to_owned)
    .to_vec()
}

impl<S: ImageSource, R: Runtime, P: Probe> Frontend<S, R, P> {
    fn config(&self) -> Result<FrontendConfig, Fault> {
        let config = FrontendConfig::load(&self.host.trust, &self.config_file)
            .map_err(|why| Fault::refused(Code::ConfigInvalid, why))?;
        match &self.bound_name {
            Some(bound) if *bound != config.run.name => Err(Fault::refused(
                Code::ConfigInvalid,
                format!(
                    "the container is {:?} and this helper was started for {bound:?}; restart it",
                    config.run.name
                ),
            )),
            _ => Ok(config),
        }
    }

    fn record(&self) -> Result<Record, Fault> {
        Record::load(&self.host.trust, &self.record_file).map_err(state_fault)
    }

    fn save(&self, record: &Record) -> Result<(), Fault> {
        record.save(&self.record_file).map_err(state_fault)
    }

    /// Wait until the container has ended: a drained frontend ends by itself when the work it
    /// had is done. With no `drain_secs` that takes as long as it takes. With one, a container
    /// that still runs when it is out is reported as `Ok(false)`: the caller stops waiting,
    /// and nothing here ends anything.
    fn wait_ended(&self, config: &FrontendConfig) -> Result<bool, String> {
        let start = (self.pace.now)();
        loop {
            let running = self
                .runtime
                .container(&config.run.name)
                .map_err(|e| e.to_string())?
                .is_some_and(|c| c.running);
            if !running {
                return Ok(true);
            }
            let waited = (self.pace.now)().saturating_sub(start);
            if config
                .drain_secs
                .is_some_and(|secs| waited >= Duration::from_secs(secs))
            {
                return Ok(false);
            }
            (self.pace.sleep)(self.pace.poll);
        }
    }

    /// Wait until the frontend answers `/readyz` with 200 and `/healthz` names `commit`.
    fn wait_ready(&self, config: &FrontendConfig, commit: &str) -> Result<(), String> {
        let start = (self.pace.now)();
        loop {
            let ready = probe::ready(&self.probe, config.api);
            let said = probe::commit(&self.probe, config.api);
            let names = said
                .as_deref()
                .is_some_and(|said| said.len() >= 12 && commit.starts_with(said));
            if ready && names {
                return Ok(());
            }
            let waited = (self.pace.now)().saturating_sub(start);
            if waited >= Duration::from_secs(config.ready_secs) {
                return Err(format!(
                    "the frontend is not ready on {} after {} s (ready: {ready}, commit: {})",
                    commit.get(..12).unwrap_or(commit),
                    config.ready_secs,
                    said.as_deref().unwrap_or("none"),
                ));
            }
            (self.pace.sleep)(self.pace.poll);
        }
    }

    /// The old container is drained and has ended on its own. Nothing is stopped or removed
    /// here: when the drain is not finished (a configured `drain_secs` is out), or any step
    /// fails, the container keeps running what it has and only its restart policy is put back
    /// to the configured one, so the switch can be asked for again.
    fn drain(&self, config: &FrontendConfig) -> Result<(), Fault> {
        let name = &config.run.name;
        let say = |e: RuntimeError| Fault::failed(Code::SwitchFailed, e.to_string());
        let running = self
            .runtime
            .container(name)
            .map_err(say)?
            .is_some_and(|c| c.running);
        if !running {
            return Ok(());
        }
        // The container ends by itself once drained; a policy that restarts it would bring
        // the old release back before the new one is made.
        self.runtime.set_restart(name, "no").map_err(say)?;
        let waited = self
            .runtime
            .exec(name, &drain_argv(config))
            .map_err(|e| e.to_string())
            .and_then(|()| self.wait_ended(config));
        let why = match waited {
            Ok(true) => return Ok(()),
            Ok(false) => format!(
                "the frontend {name} is still draining after {} s (drain_secs); nothing was \
                 stopped, and it keeps its work",
                config.drain_secs.unwrap_or_default()
            ),
            Err(why) => why,
        };
        // Best effort: a container that cannot be told its policy again is still running.
        let _ = self.runtime.set_restart(name, &config.run.restart);
        Err(Fault::failed(Code::SwitchFailed, why))
    }

    /// The container is the new release's: stopped (it has ended after its drain), removed,
    /// made again from its image, and ready. The trial window counts from the new container's
    /// start, never from the drain: the deadline goes onto the marker as soon as `run`
    /// returns, and is the one answered.
    fn replace(
        &self,
        record: &mut Record,
        trial: &Trial,
        window_secs: u64,
        unix_now: &dyn Fn() -> u64,
    ) -> Result<u64, String> {
        let (config, name) = (&trial.config, &trial.config.run.name);
        let say = |e: RuntimeError| e.to_string();
        self.runtime.stop(name, STOP_SECS).map_err(say)?;
        self.runtime.remove(name).map_err(say)?;
        self.runtime.run(&config.run, &trial.image).map_err(say)?;
        let deadline = unix_now() + window_secs;
        record.pending = Some(Trial {
            deadline_unix: Some(deadline),
            ..trial.clone()
        });
        record.save(&self.record_file)?;
        self.wait_ready(config, &trial.commit)?;
        Ok(deadline)
    }

    /// At the start of the program: a drain that was cut short by a crash of the program (after
    /// the restart policy was switched off, before the marker) leaves the old container with no
    /// policy, and with nothing to say so; if it later exits, no frontend runs. With no release
    /// on trial and the container's policy off, the configured policy is put back.
    ///
    /// Chosen over a "draining" marker because the state is already in the container (nothing
    /// else turns its policy off outside a switch, and a switch in progress holds a marker
    /// from the moment the container has ended), so it needs no new record field and cannot go
    /// stale.
    ///
    /// # Errors
    /// The file, the record or the runtime could not be read, or the policy not set.
    pub fn recover_restart_policy(&self) -> Result<Option<String>, String> {
        if self.record().map_err(|f| f.detail)?.pending.is_some() {
            return Ok(None);
        }
        let config = FrontendConfig::load_for_start(&self.host.trust, &self.config_file)?;
        let name = &config.run.name;
        let Some(container) = self.runtime.container(name).map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        if !matches!(container.restart.as_str(), "" | "no") {
            return Ok(None);
        }
        self.runtime
            .set_restart(name, &config.run.restart)
            .map_err(|e| e.to_string())?;
        Ok(Some(format!(
            "the restart policy of {name} was off with no switch under way (a drain cut short); set back to {}",
            config.run.restart
        )))
    }

    /// The container is the old release's again, from its image and the configuration of the
    /// switch, and ready.
    fn restore(&self, trial: &Trial) -> Result<(), String> {
        let say = |e: RuntimeError| e.to_string();
        let name = &trial.config.run.name;
        // The image first: a container that runs is better than none, so when the old image
        // was pruned the running one is left alone.
        if self
            .runtime
            .image_id(&trial.from_image)
            .map_err(say)?
            .is_none()
        {
            return Err(format!(
                "the previous image {} is gone from docker",
                trial.from_image
            ));
        }
        self.runtime.remove(name).map_err(say)?;
        self.runtime
            .run(&trial.config.run, &trial.from_image)
            .map_err(say)?;
        self.wait_ready(&trial.config, &trial.from)
    }
}

impl<S: ImageSource, R: Runtime, P: Probe> Backend for Frontend<S, R, P> {
    fn lock_file(&self) -> PathBuf {
        self.host.install_lock.clone()
    }

    fn node_id(&self) -> Option<String> {
        crate::node_id::read(
            &self.host.trust,
            &self.host.node_config,
            &self.host.hostname,
        )
    }

    fn current(&self) -> Result<Option<String>, Fault> {
        if let Some(slot) = self.record()?.current {
            return Ok(Some(slot.commit));
        }
        // Before the first switch the record knows nothing: the container says what it was
        // made from.
        let config = self.config()?;
        let container = self
            .runtime
            .container(&config.run.name)
            .map_err(runtime_fault)?;
        Ok(container
            .as_ref()
            .and_then(|c| commit_of_ref(&c.image_ref))
            .map(str::to_owned))
    }

    fn pending(&self) -> Result<Option<Pending>, Fault> {
        Ok(self.record()?.pending.map(|trial| Pending {
            commit: trial.commit,
            from: trial.from,
            deadline_unix: trial.deadline_unix,
            activation: String::new(),
        }))
    }

    fn has(&self, commit: &str) -> bool {
        let Ok(record) = self.record() else {
            return false;
        };
        let Some(id) = record.images.get(commit) else {
            return false;
        };
        matches!(self.runtime.image_id(id), Ok(Some(_)))
    }

    fn stage(&self, version: &Version, channel: &Channel) -> Result<String, Fault> {
        let fetched = self.source.fetch(version, channel)?;
        self.runtime.load(&fetched.file).map_err(runtime_fault)?;
        let tag = image_tag(&fetched.commit);
        let id = self
            .runtime
            .image_id(&tag)
            .map_err(runtime_fault)?
            .ok_or_else(|| {
                Fault::failed(Code::StageFailed, format!("the image file holds no {tag}"))
            })?;
        let mut record = self.record()?;
        record.images.insert(fetched.commit.clone(), id);
        record.prune(&fetched.commit);
        self.save(&record)?;
        Ok(fetched.commit.clone())
    }

    fn switch(&self, commit: &str, window_secs: u64, now: u64) -> Result<u64, Fault> {
        let config = self.config()?;
        // The window starts when the new container does, so it holds the wait for readiness
        // and nothing of the drain.
        if config.ready_secs > window_secs {
            return Err(Fault::refused(
                Code::ConfigInvalid,
                format!(
                    "a new container can take {} s to be ready and the window is {window_secs} s",
                    config.ready_secs
                ),
            ));
        }
        let mut record = self.record()?;
        let not_staged =
            || Fault::refused(Code::NotStaged, "that release is not loaded; install it");
        let image = record.images.get(commit).cloned().ok_or_else(not_staged)?;
        if !self.has(commit) {
            return Err(not_staged());
        }
        let from = self
            .current()?
            .ok_or_else(|| Fault::refused(Code::NoCurrent, "no frontend container runs now"))?;
        if from == commit {
            return Err(Fault::refused(
                Code::NotStaged,
                "that release is the one that runs",
            ));
        }
        let container = self
            .runtime
            .container(&config.run.name)
            .map_err(runtime_fault)?
            .ok_or_else(|| Fault::refused(Code::NoCurrent, "there is no container of that name"))?;
        if let Some(peer) = config
            .peers
            .iter()
            .find(|peer| !probe::ready(&self.probe, **peer))
        {
            return Err(Fault::refused(
                Code::PeerNotReady,
                format!("the frontend at {peer} is not ready"),
            ));
        }
        let begun = (self.pace.now)();
        let unix_now = || now + (self.pace.now)().saturating_sub(begun).as_secs();
        // The drain first, with nothing recorded: until the old container has ended it is
        // untouched and there is nothing to put back. It may take any time; one that is not
        // done by a configured `drain_secs` stops the switch here.
        self.drain(&config)?;
        let trial = Trial {
            commit: commit.to_owned(),
            image: image.clone(),
            from,
            from_image: container.image_id,
            deadline_unix: Some(unix_now() + window_secs),
            config,
        };
        // The marker before the old container is stopped: whatever happens from here, the
        // old release is put back from it.
        record.pending = Some(trial.clone());
        self.save(&record)?;
        let moved = self
            .replace(&mut record, &trial, window_secs, &unix_now)
            .and_then(|deadline| {
                record.current = Some(Slot {
                    commit: commit.to_owned(),
                    image,
                });
                record.save(&self.record_file).map(|()| deadline)
            });
        let why = match moved {
            Ok(deadline) => return Ok(deadline),
            Err(why) => why,
        };
        match self.restore(&trial) {
            Ok(()) => {
                record.current = Some(Slot {
                    commit: trial.from.clone(),
                    image: trial.from_image.clone(),
                });
                record.pending = None;
                // The old release runs; a record that cannot be written is the lesser news.
                let _ = self.save(&record);
                Err(Fault::failed(Code::SwitchFailed, why))
            }
            Err(back) => Err(Fault::failed(
                Code::RevertFailed,
                format!("{why}; going back: {back}"),
            )),
        }
    }

    fn confirm(&self, pending: &Pending) -> Result<(), Fault> {
        let mut record = self.record()?;
        let trial = record
            .pending
            .take()
            .filter(|trial| trial.commit == pending.commit)
            .ok_or_else(|| {
                state_fault("the release on trial is not the one confirmed".to_owned())
            })?;
        record.current = Some(Slot {
            commit: trial.commit.clone(),
            image: trial.image.clone(),
        });
        record.prune(&trial.commit);
        self.save(&record)?;
        // The old image is of no use now; a rollback is an install of an older release.
        if trial.from_image != trial.image {
            let _ = self.runtime.remove_image(&trial.from_image);
        }
        Ok(())
    }

    fn revert(&self, _pending: &Pending) -> Result<(), Fault> {
        let mut record = self.record()?;
        let trial = record
            .pending
            .clone()
            .ok_or_else(|| state_fault("no release is on trial".to_owned()))?;
        self.restore(&trial)
            .map_err(|why| Fault::failed(Code::RevertFailed, why))?;
        record.pending = None;
        record.prune(&trial.from);
        record.current = Some(Slot {
            commit: trial.from,
            image: trial.from_image,
        });
        self.save(&record)
    }
}

/// The frontend container of this host as the program runs it, from the configuration
/// `config_file`. `bound` is the container's name when that is not the default file: the
/// record and the lock are then this container's own.
#[cfg(target_os = "linux")]
pub fn production(
    config_file: PathBuf,
    bound: Option<String>,
) -> Frontend<super::linux::Linux<kbf_host::HostExec>, runtime::Docker, probe::Http> {
    let files = Files::of(bound.as_deref());
    let mut host = Host::production();
    host.install_lock = files.lock.unwrap_or(host.install_lock);
    let mut source = super::linux::production(Vec::new());
    source.scratch_tag = bound.clone();
    Frontend {
        source,
        runtime: runtime::Docker::new(
            Box::new(crate::runner::SystemRunner),
            Path::new("/usr/bin/docker").to_path_buf(),
        ),
        probe: probe::Http {
            limit: Duration::from_secs(5),
            now: crate::pace::real_now(),
        },
        host,
        config_file,
        record_file: Path::new(super::production::LINUX_STATE_DIR).join(files.record),
        bound_name: bound,
        pace: Pace::real(),
    }
}

#[cfg(test)]
mod config_tests;
#[cfg(test)]
mod drain_tests;
#[cfg(test)]
mod fake;
#[cfg(test)]
mod probe_tests;
#[cfg(test)]
mod record_tests;
#[cfg(test)]
mod runtime_tests;
#[cfg(test)]
mod several_tests;
#[cfg(test)]
mod tests;
