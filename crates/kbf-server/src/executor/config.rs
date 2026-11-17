//! What the executor is configured with, and what follows from it: the constraints of a job,
//! the wall limit of a job, and the timers of the calls it makes.

use std::time::Duration;

use js_proto::v1::{Constraints, ExecMode, Isolation};
use ring::rand::{SecureRandom, SystemRandom};

use crate::executor::mac::{MacPool, MacPoolError};
use crate::executor::out::PutDirRoots;

/// The default of [`Config::failed_stderr_log_bytes`]: enough for the line a tool dies with.
pub const FAILED_STDERR_LOG_BYTES: usize = 512;

/// The longest the keys of a put directory may be asked to stay good. The store refuses more,
/// and the executor does not ask: [`Config::key_ceiling`] stays under it, and the longest job's
/// wall limit under that.
pub const KEY_LIMIT: Duration = Duration::from_secs(4 * 3600);

/// The default of [`Config::live`]: how long the liveness row of a process outlives its last
/// renewal, and so how soon after a process ends another takes up its unfinished operations.
pub const LIVE: Duration = Duration::from_secs(20);

/// The default of [`Config::finished_retention`]: how long a finished operation is answered
/// by name from memory.
pub const FINISHED_RETENTION: Duration = Duration::from_secs(600);

/// What a put-directory job's root is read from (`--executor-input-tree`,
/// docs/design/actions-as-jobs.md, 4.7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum InputTree {
    /// The action tree written as tierstore tree manifests, the job's layer the host directory
    /// `<mp>/trees/<digest>` that tierstore's file system serves. The default.
    #[default]
    Manifests,
    /// The action tree written as one tierstore tree list, the job's layer tierstore's
    /// mount provider with the list's key: the host's agent asks the provider for the tree
    /// as soon as the job is placed, and its files are fetched while the sandbox is made. A Mac
    /// job's tree is always [`InputTree::Manifests`]: its runtime takes no provider yet.
    Provider,
}

/// The executor's settings. Every duration is an upper or lower bound the executor holds
/// itself to; [`Config::validate`] refuses a set that cannot work.
#[derive(Clone, Debug)]
pub struct Config {
    /// The name of this farm's frontends: the value of the `kbf-frontend` label on their jobs
    /// and hashed into the jobs' ids. Every frontend over one state store gives the same.
    pub frontend: String,
    /// Which process this is: the value of the `kbf-instance` label on its jobs and the name
    /// of its liveness row, made up at start.
    pub instance: String,
    /// The CPU architecture a job asks its host for.
    pub arch: String,
    /// The longest action timeout the executor runs.
    pub max_run: Duration,
    /// What a job's wall limit adds to the action's timeout: fetch, pack and upload. The task
    /// tool's own deadline bounds only the action's command, so a fetch of a large input tree
    /// or an upload that outlasts this margin ends the job at its wall limit, and is answered
    /// to the caller as a timeout and not rerun: raise it for slower stores.
    pub run_margin: Duration,
    /// The timeout of an action that names none: at most [`Config::max_run`].
    pub default_timeout: Duration,
    /// The most bytes of a failed job's standard error the frontend writes to its log, at WARN,
    /// the end of the stream. `0` writes none; the whole attached output stays with the result
    /// either way.
    pub failed_stderr_log_bytes: usize,
    /// How often the job of an operation is read back, and a submit whose answer was lost is
    /// sent again.
    pub poll: Duration,
    /// How long one call to the job service may take before it counts as unanswered, so a
    /// service that never answers holds an operation for this and not for the client's own
    /// 30 s a call.
    pub call_timeout: Duration,
    /// How long the inputs of one run (its action, its input tree published into the store,
    /// its put directory) may take to prepare; a preparation past it is a run that ended for a
    /// reason of the farm's.
    pub prepare_limit: Duration,
    /// The most bytes of a pack the frontend takes.
    pub max_pack_bytes: u64,
    /// The most bytes of one blob in a pack.
    pub max_blob_bytes: u64,
    /// The most records of a one-pass object (`--task-max-object-entries`).
    pub max_object_entries: usize,
    /// The most bytes of a one-pass object's manifest (`--task-max-object-manifest-bytes`).
    pub max_object_manifest_bytes: u64,
    /// The bytes of a one-pass object's tail each record may account for
    /// (`--task-object-bytes-per-entry`).
    pub object_bytes_per_entry: u64,
    /// The longest a put directory's keys may be good for.
    pub key_ceiling: Duration,
    /// The host directories of the put directory of every run (`--task-out-root`). `None`, the
    /// default, refuses every Linux and Mac run as the caller's: a run has nowhere to commit.
    /// A job has no network.
    pub task_out: Option<PutDirRoots>,
    /// What a Linux put-directory job's root is read from ([`InputTree`];
    /// `--executor-input-tree`).
    pub input_tree: InputTree,
    /// How long the liveness row of this process lives after a renewal, on the store's clock
    /// (`--executor-live-secs`). It is renewed every quarter of it; once it has expired,
    /// another frontend of the store takes up the process's unfinished operations.
    pub live: Duration,
    /// How long a finished operation is answered by name from memory
    /// (`--finished-retention-secs`); over a state store its `done/` row answers it for at
    /// least ten minutes on every frontend.
    pub finished_retention: Duration,
    /// The Macs (`--executor-mac-*`): `None`, the default, refuses every action whose platform
    /// asks for macOS ([`crate::executor::mac`]).
    pub mac: Option<MacPool>,
}

/// Why a [`Config`] is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A name that cannot be a label value.
    #[error("the frontend id must be 1 to 63 of letters, digits, '-', '_' and '.'")]
    Frontend,
    /// A job whose put directory's keys would not stay good for its wall limit.
    #[error("the longest job's wall limit exceeds the key ceiling")]
    KeyCeiling,
    /// A default timeout of zero or over the longest run.
    #[error("the default timeout must be above zero and no more than the longest run")]
    Timeout,
    /// A bound that is zero.
    #[error(
        "the poll interval, the call timeout, the preparation limit and the liveness time must be above zero"
    )]
    Bounds,
    /// A key ceiling or a job longer than a put directory's keys may be good for.
    #[error("the key ceiling and the longest job's wall limit must be at most 4 hours")]
    KeyLimit,
    /// The pool of Macs is not one.
    #[error("{0}")]
    Mac(#[from] MacPoolError),
}

impl Config {
    /// A config for the frontends named `frontend`, with the defaults of every other setting.
    #[must_use]
    pub fn new(frontend: &str) -> Self {
        Self {
            frontend: frontend.to_owned(),
            instance: instance_id(),
            arch: "x86_64".to_owned(),
            max_run: Duration::from_secs(3000),
            run_margin: Duration::from_secs(300),
            default_timeout: Duration::from_secs(3000),
            failed_stderr_log_bytes: FAILED_STDERR_LOG_BYTES,
            poll: Duration::from_millis(250),
            call_timeout: Duration::from_secs(5),
            prepare_limit: Duration::from_secs(300),
            max_pack_bytes: 128 << 20,
            max_blob_bytes: 64 << 20,
            max_object_entries: 1 << 20,
            max_object_manifest_bytes: 4 << 20,
            object_bytes_per_entry: 1024,
            key_ceiling: KEY_LIMIT,
            task_out: None,
            input_tree: InputTree::Manifests,
            live: LIVE,
            finished_retention: FINISHED_RETENTION,
            mac: None,
        }
    }

    /// Checks the settings against each other.
    ///
    /// # Errors
    /// The first setting that breaks a rule.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let name_ok = |c: char| c.is_ascii_alphanumeric() || "-_.".contains(c);
        let frontend =
            (1..=63).contains(&self.frontend.len()) && self.frontend.chars().all(name_ok);
        let longest = self.max_run + self.run_margin;
        let checks = [
            (frontend, ConfigError::Frontend),
            (
                !self.default_timeout.is_zero() && self.default_timeout <= self.max_run,
                ConfigError::Timeout,
            ),
            (self.key_ceiling >= longest, ConfigError::KeyCeiling),
            (self.key_ceiling <= KEY_LIMIT, ConfigError::KeyLimit),
            (
                [self.poll, self.call_timeout, self.prepare_limit, self.live]
                    .iter()
                    .all(|bound| !bound.is_zero()),
                ConfigError::Bounds,
            ),
        ];
        let first = checks
            .into_iter()
            .find_map(|(ok, error)| (!ok).then_some(error));
        match (first, &self.mac) {
            (Some(error), _) => Err(error),
            (None, Some(mac)) => Ok(mac.validate()?),
            (None, None) => Ok(()),
        }
    }

    /// What a Linux job asks of its host beside resources: the system and the architecture,
    /// in a container. The task tool and its static binary run
    /// nowhere else.
    #[must_use]
    pub fn constraints(&self) -> Constraints {
        Constraints {
            os: "linux".to_owned(),
            arch: self.arch.clone(),
            exec: ExecMode::Container.into(),
            // No ISA level, no accelerator, and confinement left unspecified (contained):
            // the request's bytes stay those an older job service also reads.
            isa_level: String::new(),
            accelerator: None,
            isolation: Isolation::Unspecified.into(),
        }
    }

    /// The wall limit of a job of an action of `timeout`: the action's own, and nothing cuts
    /// it shorter, so a job stopped at it is the action's timeout.
    #[must_use]
    pub fn wall(&self, timeout: Duration) -> Duration {
        timeout + self.run_margin
    }
}

/// An id of this process, in hex: 128 random bits from the operating system's source, then
/// the process id and the time. A source that fails (which nothing here can repair) leaves
/// the bits zero, and the process id and the time still tell two processes apart.
fn instance_id() -> String {
    let mut bytes = [0_u8; 16];
    let _ = SystemRandom::new().fill(&mut bytes);
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{:032x}{:08x}{:x}",
        u128::from_be_bytes(bytes),
        std::process::id(),
        since.as_nanos()
    )
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
