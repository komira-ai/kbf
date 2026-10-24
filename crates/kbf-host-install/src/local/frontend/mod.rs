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
//! - **switch**: refuses when it can see that another frontend is not ready, writes the marker
//!   (the configuration the containers are made from, the images of both releases, the
//!   deadline), and then, in this order: asks the frontend to drain through its own operator
//!   API (`kbf cluster drain --frontend` inside the container, where the API is on loopback),
//!   waits until the container has ended or the drain deadline is out, stops it, removes it,
//!   makes a new one from the new image with the same configuration, and waits for `/readyz`
//!   to answer 200 and `/healthz` to name the release's commit. When any of it fails, the
//!   container is made again from the old image and waited for the same way, and the switch
//!   fails.
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

/// How the waits tell time and pause: the real clock in the program, a made-up one in tests.
pub struct Pace {
    /// How long to pause between two looks.
    pub poll: Duration,
    /// A monotonic reading.
    pub now: Box<dyn Fn() -> Duration>,
    pub sleep: Box<dyn Fn(Duration)>,
}

impl Pace {
    /// The clock of the machine.
    pub fn real() -> Self {
        let start = std::time::Instant::now();
        Self {
            poll: Duration::from_secs(2),
            now: Box::new(move || start.elapsed()),
            sleep: Box::new(std::thread::sleep),
        }
    }
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
        FrontendConfig::load(&self.host.trust, &self.config_file)
            .map_err(|why| Fault::refused(Code::ConfigInvalid, why))
    }

    fn record(&self) -> Result<Record, Fault> {
        Record::load(&self.host.trust, &self.record_file).map_err(state_fault)
    }

    fn save(&self, record: &Record) -> Result<(), Fault> {
        record.save(&self.record_file).map_err(state_fault)
    }

    /// Wait until the container has ended, or the drain deadline is out: a drained frontend
    /// ends by itself when the work it had is done.
    fn wait_ended(&self, config: &FrontendConfig) -> Result<(), String> {
        let start = (self.pace.now)();
        loop {
            let running = self
                .runtime
                .container(&config.run.name)
                .map_err(|e| e.to_string())?
                .is_some_and(|c| c.running);
            let waited = (self.pace.now)().saturating_sub(start);
            if !running || waited >= Duration::from_secs(config.drain_secs) {
                return Ok(());
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

    /// The container is the new release's: drained, stopped, made again from its image, and
    /// ready.
    fn replace(&self, trial: &Trial) -> Result<(), String> {
        let (config, name) = (&trial.config, &trial.config.run.name);
        let say = |e: RuntimeError| e.to_string();
        let running = self
            .runtime
            .container(name)
            .map_err(say)?
            .is_some_and(|c| c.running);
        // The container ends by itself once drained; a policy that restarts it would bring
        // the old release back before the new one is made.
        self.runtime.set_restart(name, "no").map_err(say)?;
        if running {
            self.runtime.exec(name, &drain_argv(config)).map_err(say)?;
            self.wait_ended(config)?;
        }
        self.runtime.stop(name, STOP_SECS).map_err(say)?;
        self.runtime.remove(name).map_err(say)?;
        self.runtime.run(&config.run, &trial.image).map_err(say)?;
        self.wait_ready(config, &trial.commit)
    }

    /// The container is the old release's again, from its image and the configuration of the
    /// switch, and ready.
    fn restore(&self, trial: &Trial) -> Result<(), String> {
        let say = |e: RuntimeError| e.to_string();
        let name = &trial.config.run.name;
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
        if config.switch_secs() > window_secs {
            return Err(Fault::refused(
                Code::ConfigInvalid,
                format!(
                    "a switch can take {} s (drain, stop, ready) and the window is {window_secs} s",
                    config.switch_secs()
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
        let trial = Trial {
            commit: commit.to_owned(),
            image: image.clone(),
            from,
            from_image: container.image_id,
            deadline_unix: Some(now + window_secs),
            config,
        };
        // The marker first: whatever happens from here, the old release is put back from it.
        record.pending = Some(trial.clone());
        self.save(&record)?;
        let moved = self.replace(&trial).and_then(|()| {
            record.current = Some(Slot {
                commit: commit.to_owned(),
                image,
            });
            record.save(&self.record_file)
        });
        let Err(why) = moved else {
            return Ok(now + window_secs);
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

/// The frontend container of this host as the program runs it.
#[cfg(target_os = "linux")]
pub fn production()
-> Frontend<super::linux::Linux<kbf_host::HostExec>, runtime::Docker, probe::Http> {
    Frontend {
        source: super::linux::production(Vec::new()),
        runtime: runtime::Docker::new(
            Box::new(crate::runner::SystemRunner),
            Path::new("/usr/bin/docker").to_path_buf(),
        ),
        probe: probe::Http {
            limit: Duration::from_secs(5),
        },
        host: Host::production(),
        config_file: PathBuf::from(config::CONFIG),
        record_file: Path::new(super::production::LINUX_STATE_DIR).join(RECORD),
        pace: Pace::real(),
    }
}

#[cfg(test)]
mod config_tests;
#[cfg(test)]
mod fake;
#[cfg(test)]
mod probe_tests;
#[cfg(test)]
mod record_tests;
#[cfg(test)]
mod runtime_tests;
#[cfg(test)]
mod tests;
