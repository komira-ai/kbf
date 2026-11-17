//! The Macs: an action whose platform names `os=macos` becomes a job of the job service's
//! macOS hosts, run by their agent's native runtime (a user of the agent's pool, under its
//! `sandbox-exec` profile), not by a `kbf-daemon` (docs/design/actions-as-jobs.md, section 14).
//!
//! # Which actions, and where the farm places them
//!
//! An action asks for a Mac by its platform: `OSFamily` `macos` (or `darwin`, `macosx`, `osx`)
//! or `os=macos`, read as the farm reads it ([`kbf_caps::Request::from_platform`]); every other
//! action is a Linux one ([`System::of`]). The farm places a Mac action only on a worker that
//! reports `os=macos`, so the executor registers a second set of virtual workers for the Macs
//! ([`MacPool`], `--executor-mac-hosts`, none by default), `js-mac:<frontend>-<n>`, each of the
//! shape the Linux ones declare, reporting `os=macos`, `arch=arm64` and the one driver `native`.
//! `native` serves an `action` lease only: no `whole_machine` lease is placed on a Mac worker,
//! as a put-directory job cannot be one.
//!
//! `os_version` is read too, by the platform-tags rule: `os_version=26` means macOS 26 or
//! newer. The job service has no constraint on a host's system version, so the executor holds
//! the rule itself against the least major version every Mac of the pool runs
//! (`--executor-mac-os-version`, 26 by default): an action that asks for a newer one is refused
//! as the caller's, naming both ([`check_version`]). A Linux action's `os_version` is not read.
//!
//! # The job
//!
//! A Mac job runs natively in its run directory `exec/`, the job's `/work`: there is no root file
//! system to replace and no layer (the job service would make a layered job a container one). So
//! the job ([`mac_spec`]) is:
//!
//! - **No layer; the image is the release's `kbf-task` bundle**, pinned by its darwin manifest
//!   digest (`--executor-mac-task-bundle`, `kbf-task@sha256:<hex>`; docs/artifacts.md, "The
//!   macOS kbf-task bundle"), and the command is left empty, so the bundle's entrypoint,
//!   `kbf-task`, runs, told `--materialize-work=.kbf-tree` ([`mac_argv`]). A job with no layer
//!   must name an image, and a Mac's agent runs only a bundle's program.
//! - **Two mounts**, each at `/work/<name>`, which a Mac's agent makes `exec/<name>`: the action
//!   tree `<mp>/trees/<digest>` read-only at [`TREE_TARGET`], and the put directory
//!   `<mp>/put/cas/b/<lease>/<nonce>` read-write at [`MAC_OUT_TARGET`]. Both `no_exec`: the
//!   task copies the inputs out of the tree into `exec/work` (APFS clones where the volume
//!   allows), runs the Command there, and commits its one object into the put directory, as a
//!   Linux job does. `<mp>` is the Macs' own (`--executor-mac-task-out-root`, by default
//!   `--task-out-root`).
//! - `os=macos`, `arch=arm64`, `exec=sandbox-exec`, the share the farm booked, no network, no
//!   file and no secret.
//!
//! The job service at kbf's pin does not yet run such a job on a Mac (it takes no native start
//! from the job API, and its Macs do not take mounts); a pool of no Macs, the default, sends
//! none.

use std::collections::BTreeMap;
use std::time::Duration;

use js_proto::v1::{
    Constraints, ExecMode, Isolation, JobSpec, Mount, MountMode, MountPropagation, Needs,
    NetworkLevel, mount,
};
use kbf_proto::reapi::{Digest, Platform};
use kbf_types::LeaseId;

use crate::executor::out::PutDirRoots;
use crate::executor::task::build_id;

/// The `os` a Mac reports, as the farm and the job service spell it.
pub const MAC_OS: &str = "macos";

/// The architecture of every Mac: kbf's macOS release is `macos-arm64` only.
pub(super) const MAC_ARCH: &str = "arm64";

/// The one driver a Mac worker reports: it serves an `action` lease (`LeaseKind::drivers`).
pub(super) const MAC_DRIVER: &str = "native";

/// Where a Mac job's action tree is mounted, read-only: `exec/.kbf-tree` on the Mac.
pub const TREE_TARGET: &str = "/work/.kbf-tree";

/// Where a Mac job's put directory is mounted, read-write: `exec/.kbf-out` on the Mac.
pub const MAC_OUT_TARGET: &str = "/work/.kbf-out";

/// The least macOS a Mac of the pool runs when `--executor-mac-os-version` is left out.
pub const DEFAULT_MAC_OS_VERSION: u32 = 26;

/// The hosts a virtual worker stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WorkerOs {
    /// The job service's Linux hosts: `js:<frontend>-<n>` (with shared ownership,
    /// `js:<frontend>.<instance>-<n>`, [`Config::worker_of`](super::Config::worker_of)).
    Linux,
    /// Its Macs: `js-mac:<frontend>-<n>` (with shared ownership,
    /// `js-mac:<frontend>.<instance>-<n>`).
    Mac,
}

/// The system an action asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum System {
    /// Linux: every action that does not ask for a Mac.
    #[default]
    Linux,
    /// macOS, at least `os_version` (a major version) when the action names one.
    Macos {
        /// The least major version the action takes.
        os_version: Option<u32>,
    },
}

/// The platform property of the least system version.
const OS_VERSION: &str = "os_version";

impl System {
    /// The system `platform` (the Action's, or else the Command's) asks for: macOS when it names
    /// `os=macos` the way the farm reads it, else Linux.
    ///
    /// # Errors
    /// A platform the farm refuses, or an `os_version` of a Mac action that is not a version
    /// (`<major>[.<minor>[.<patch>]]`, digits).
    pub fn of(platform: Option<&Platform>) -> Result<Self, String> {
        let properties = platform.map_or(&[][..], |p| p.properties.as_slice());
        let pairs = properties
            .iter()
            .map(|p| (p.name.as_str(), p.value.as_str()));
        let request = kbf_caps::Request::from_platform(pairs).map_err(|e| e.to_string())?;
        if request.exact("os") != Some(MAC_OS) {
            return Ok(Self::Linux);
        }
        let named = properties
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(OS_VERSION));
        let os_version = named.map(|p| major(&p.value)).transpose()?;
        Ok(Self::Macos { os_version })
    }
}

/// The major version of `value`: `26`, `26.1` and `26.0.1` are 26.
fn major(value: &str) -> Result<u32, String> {
    let not = || format!("the action's {OS_VERSION} {value:?} is not a version such as 26 or 26.1");
    let mut parts = value.split('.');
    let first = parts.next().unwrap_or_default();
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let rest: Vec<&str> = parts.collect();
    match digits(first) && rest.len() <= 2 && rest.iter().all(|part| digits(part)) {
        true => first.parse().map_err(|_| not()),
        false => Err(not()),
    }
}

/// The Macs of the executor: how many virtual workers stand for them, the task tool's bundle, the
/// least macOS each runs and the tier store's roots there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacPool {
    /// How many Mac virtual workers the farm places on (`--executor-mac-hosts`), each of the
    /// shape the Linux ones declare.
    pub hosts: usize,
    /// The release's `kbf-task` bundle, `NAME@sha256:<64 hex>` by its darwin manifest digest
    /// (`--executor-mac-task-bundle`): a Mac job's image.
    pub bundle: String,
    /// The least major macOS every Mac runs (`--executor-mac-os-version`).
    pub os_version: u32,
    /// The tier store's roots on the Macs (`--executor-mac-task-out-root`); `None`: those of
    /// `--task-out-root`.
    pub roots: Option<PutDirRoots>,
}

/// Why a [`MacPool`] is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MacPoolError {
    /// A pool of no worker: leave the pool out instead.
    #[error("a pool of Macs needs at least one virtual worker")]
    NoHosts,
    /// A bundle that is not pinned by a digest.
    #[error("the Macs' task bundle {0:?} is not pinned by an @sha256 digest (NAME@sha256:HEX)")]
    Bundle(String),
    /// A least macOS of 0.
    #[error("the least macOS of the Macs must be a major version above 0")]
    OsVersion,
}

impl MacPool {
    /// Checks the pool.
    ///
    /// # Errors
    /// The first [`MacPoolError`].
    pub fn validate(&self) -> Result<(), MacPoolError> {
        if self.hosts == 0 {
            return Err(MacPoolError::NoHosts);
        }
        if !pinned(&self.bundle) {
            return Err(MacPoolError::Bundle(self.bundle.clone()));
        }
        match self.os_version {
            0 => Err(MacPoolError::OsVersion),
            _ => Ok(()),
        }
    }

    /// The roots of a Mac job: the pool's own, or `linux` (`--task-out-root`'s).
    pub(super) fn roots<'a>(&'a self, linux: &'a PutDirRoots) -> &'a PutDirRoots {
        self.roots.as_ref().unwrap_or(linux)
    }
}

/// Whether `image` is a name, `@sha256:` and 64 lower-case hex digits, as the job service
/// takes an image.
fn pinned(image: &str) -> bool {
    let lower_hex = |b: u8| matches!(b, b'0'..=b'9' | b'a'..=b'f');
    image
        .rsplit_once("@sha256:")
        .is_some_and(|(name, d)| !name.is_empty() && d.len() == 64 && d.bytes().all(lower_hex))
}

/// Refuses a Mac action that asks for a newer macOS than the least the pool runs: no Mac of the
/// pool is sure to run it, and the job service cannot be asked for a version.
///
/// # Errors
/// The refusal, naming both versions.
pub(super) fn check_version(asked: Option<u32>, pool: &MacPool) -> Result<(), String> {
    match asked {
        Some(asked) if asked > pool.os_version => Err(format!(
            "the action asks for macOS {asked} or newer ({OS_VERSION}), and this executor's \
             Macs are sure to run only macOS {} or newer (--executor-mac-os-version)",
            pool.os_version
        )),
        _ => Ok(()),
    }
}

/// What a Mac job asks of its host beside resources: a Mac, its architecture, the native
/// runtime.
#[must_use]
pub(super) fn mac_constraints() -> Constraints {
    Constraints {
        os: MAC_OS.to_owned(),
        arch: MAC_ARCH.to_owned(),
        exec: ExecMode::SandboxExec.into(),
        isa_level: String::new(),
        accelerator: None,
        isolation: Isolation::Unspecified.into(),
    }
}

/// The flags of the task tool for the Mac run of `action` on `lease`: the inputs are copied out
/// of the tree at `exec/.kbf-tree` (`--materialize-work`), the object is committed into
/// `exec/.kbf-out`, and the action may run for `deadline`. Both paths are relative to the
/// task's working directory, `exec/`.
#[must_use]
pub(super) fn mac_argv(action: &Digest, lease: LeaseId, deadline: Duration) -> Vec<String> {
    let in_exec = |target: &str| target.trim_start_matches("/work/").to_owned();
    vec![
        format!("--materialize-work={}", in_exec(TREE_TARGET)),
        "--stage-and-commit".to_owned(),
        "--out-dir".to_owned(),
        in_exec(MAC_OUT_TARGET),
        "--action".to_owned(),
        format!("{}/{}", action.hash, action.size_bytes),
        "--lease".to_owned(),
        build_id(lease),
        "--deadline-ms".to_owned(),
        deadline.as_millis().to_string(),
        "--object-format".to_owned(),
        "one-pass".to_owned(),
    ]
}

/// The mount of `host_path` at `target`, `no_exec`, private; read-write or read-only.
fn mount_at(host_path: String, target: &str, mode: MountMode) -> Mount {
    Mount {
        source: Some(mount::Source::HostPath(host_path)),
        target: target.to_owned(),
        mode: mode.into(),
        sub_path: String::new(),
        no_exec: true,
        propagation: MountPropagation::Private.into(),
    }
}

/// What a Mac job is made of, beside what every job carries.
pub(super) struct MacJob<'a> {
    /// The job's id.
    pub(super) job_id: String,
    /// The pool.
    pub(super) pool: &'a MacPool,
    /// The tier store's roots on the Macs.
    pub(super) roots: &'a PutDirRoots,
    /// The run's put directory, `cas/b/<lease>/<nonce>`.
    pub(super) dir: &'a str,
    /// The run's action tree, its digest in hex.
    pub(super) tree: &'a str,
    /// The task tool's flags.
    pub(super) args: Vec<String>,
    /// What the job needs.
    pub(super) needs: Needs,
    /// The job's wall limit, in seconds.
    pub(super) wall_seconds: u64,
    /// The labels of every job.
    pub(super) labels: BTreeMap<String, String>,
}

/// The Mac job of `job`: the pool's task bundle, whose entrypoint runs, the tree and the put
/// directory mounted under `/work`, no layer, no network.
pub(super) fn mac_spec(job: MacJob<'_>) -> JobSpec {
    JobSpec {
        job_id: job.job_id,
        image: job.pool.bundle.clone(),
        // Empty: the bundle's entrypoint, `kbf-task`, runs with the arguments.
        command: String::new(),
        args: job.args,
        env: BTreeMap::new(),
        needs: Some(job.needs),
        constraints: Some(mac_constraints()),
        wall_limit_seconds: Some(job.wall_seconds),
        // The inputs are the tree and the outputs the mount: the job reaches nothing.
        network: NetworkLevel::None.into(),
        ports: Vec::new(),
        host_ports: Vec::new(),
        layers: Vec::new(),
        mounts: vec![
            mount_at(
                format!("{}/{}", job.roots.trees(), job.tree),
                TREE_TARGET,
                MountMode::ReadOnly,
            ),
            mount_at(
                format!("{}/{}", job.roots.put(), job.dir),
                MAC_OUT_TARGET,
                MountMode::ReadWrite,
            ),
        ],
        files: Vec::new(),
        priority: 0,
        labels: job.labels,
    }
}

#[cfg(test)]
#[path = "mac_tests.rs"]
mod tests;
