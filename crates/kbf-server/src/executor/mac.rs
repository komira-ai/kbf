//! The Macs: an action whose platform names `os=macos` becomes a job of the job service's
//! macOS hosts, run by their agent's native runtime (a user of the agent's pool, under its
//! `sandbox-exec` profile), (docs/design/actions-as-jobs.md, section 14).
//!
//! # Which actions
//!
//! An action asks for a Mac by its platform: `OSFamily` `macos` (or `darwin`, `macosx`, `osx`)
//! or `os=macos` ([`kbf_caps::Request::from_platform`]); every other action is a Linux one
//! ([`System::of`]). A Mac action becomes a Mac job when the executor has Macs ([`MacPool`],
//! given by `--executor-mac-task-bundle`, none by default), and is refused as the caller's when
//! it has none, or when it asks for a whole machine: a put-directory job cannot be one.
//!
//! `os_version` is read too, by the platform-tags rule: `os_version=26` means macOS 26 or
//! newer. The executor holds the rule itself against the least major version every Mac of the
//! pool runs (`--executor-mac-os-version`, 26 by default): an action that asks for a newer one
//! is refused as the caller's, naming both ([`check_version`]). A Linux action's `os_version` is
//! not read.
//!
//! # The Xcode
//!
//! A Mac action names the Xcode it runs with by build, `xcode=16C5032a` (the Xcode's
//! `ProductBuildVersion`), read in any case as every capability key ([`XcodeBuild`]). The job
//! asks for that build (`Constraints.xcode`): the job service places it only on a Mac whose
//! agent reports that Xcode installed, and the agent runs it with that Xcode's developer
//! directory, never the Mac's selected one. Since the property is part of the action digest, a
//! result built with one Xcode is never served for the same action naming another.
//!
//! A Mac action that names no `xcode` is refused as the caller's, `INVALID_ARGUMENT` with the
//! reason [`NO_XCODE_REASON`](crate::executor::outcome::NO_XCODE_REASON), and no job is made for
//! it: it would run with whichever Xcode the Mac has selected, which an update of the Mac can
//! change, and its cached result would then be served across that change. A value that is not a
//! build id (1 to 32 letters, digits, `.`, `_` and `-`, starting with a letter or a digit) is
//! refused as malformed. A Linux action's `xcode` is neither required nor checked as a build id
//! nor sent, but like every capability key it is read with the platform: one that is empty or
//! given twice is refused as malformed, for any system.
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
//!   tree read-only at [`TREE_TARGET`], from tierstore's mount provider ([`PROVIDER`]) given the
//!   key of the tree's tree list, as a Linux job's layer is with `--executor-input-tree
//!   provider` ([`provider_layer`](crate::executor::out::provider_layer)); and the put directory
//!   `<mp>/put/cas/b/<run>/<nonce>` read-write at [`MAC_OUT_TARGET`]. On a Mac the provider
//!   answers the mount with a directory of the Mac's own disk holding the whole tree. Both
//!   `no_exec`: the task copies the inputs out of the tree into `exec/kbf-work` (APFS clones
//!   where the volume allows), runs the Command there, and commits its one object into the put
//!   directory, as a Linux job does. `<mp>/put` is the Macs' own (`--executor-mac-task-out-root`,
//!   by default `--task-out-root`). A Mac's tree is always its tree list, whatever
//!   `--executor-input-tree` says: a Mac has no host directory of manifests. So on a host's
//!   refusal of the list the miss path's first step (the list written again, checked) helps a
//!   Mac job, and its second (manifests) does not: the next run mounts the list again.
//! - `os=macos`, `arch=arm64`, `exec=sandbox-exec`, the action's `xcode`, no CPU and no memory
//!   (the job service sizes it from its class's history), no network, no file and no secret.
//!
//! # The pack
//!
//! On a Mac the put directory is a plain directory: the commit's rename makes no object of the
//! store. The Mac's tier store stores the object, under the same key `cas/b/<run>/<nonce>/pack`,
//! once the job has ended, and the frontend reads it from the store as it reads a Linux job's.
//! So a Mac job that ended well has its pack read again every poll while the store has no object
//! under its key, for up to [`PACK_WAIT`] ([`is_mac`]); a pack still absent then fails the run as
//! the farm's, and the action runs again.

use std::collections::BTreeMap;
use std::time::Duration;

use js_proto::v1::{
    Constraints, ExecMode, Isolation, JobSpec, Mount, MountMode, MountPropagation, Needs,
    NetworkLevel, ProviderSource, mount,
};
use kbf_proto::reapi::{Digest, Platform};

use crate::executor::out::{PROVIDER, PutDirRoots};
use crate::executor::task::{RunId, build_id};

/// The `os` of a Mac, as the job service spells it.
pub const MAC_OS: &str = "macos";

/// The architecture of every Mac: kbf's macOS release is `macos-arm64` only.
pub(super) const MAC_ARCH: &str = "arm64";

/// Where a Mac job's action tree is mounted, read-only: `exec/.kbf-tree` on the Mac.
pub const TREE_TARGET: &str = "/work/.kbf-tree";

/// Where a Mac job's put directory is mounted, read-write: `exec/.kbf-out` on the Mac.
pub const MAC_OUT_TARGET: &str = "/work/.kbf-out";

/// The longest the frontend waits, after a Mac job ended well, for the Mac's tier store to
/// store its pack: the store stores it only after the job ended, and a large pack takes a
/// while to send.
pub(super) const PACK_WAIT: Duration = Duration::from_secs(30 * 60);

/// The least macOS a Mac of the pool runs when `--executor-mac-os-version` is left out.
pub const DEFAULT_MAC_OS_VERSION: u32 = 26;

/// The system an action asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum System {
    /// Linux: every action that does not ask for a Mac.
    #[default]
    Linux,
    /// macOS, at least `os_version` (a major version) when the action names one, with the Xcode
    /// the action names.
    Macos {
        /// The least major version the action takes.
        os_version: Option<u32>,
        /// The Xcode the action runs with; `None` when it names none, which is refused before
        /// a job is made.
        xcode: Option<XcodeBuild>,
    },
}

/// An Xcode, by its build id (`16C5032a`, its `ProductBuildVersion`), as a Mac action names it
/// in the platform property `xcode` and the job service takes it in `Constraints.xcode`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XcodeBuild(String);

impl XcodeBuild {
    /// The most bytes of a build id the job service takes.
    const MAX: usize = 32;

    /// `value` as a build id.
    ///
    /// # Errors
    /// A value that is not 1 to 32 bytes of ASCII letters, digits, `.`, `_` and `-` starting
    /// with a letter or a digit, as the job service checks a build id.
    pub fn parse(value: &str) -> Result<Self, String> {
        let allowed = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
        let first = value.bytes().next();
        match first.is_some_and(|b| b.is_ascii_alphanumeric())
            && value.len() <= Self::MAX
            && value.bytes().all(allowed)
        {
            true => Ok(Self(value.to_owned())),
            false => Err(format!(
                "the action's {XCODE} {value:?} is not an Xcode build id such as 16C5032a: 1 to \
                 {} letters, digits, '.', '_' and '-', starting with a letter or a digit",
                Self::MAX
            )),
        }
    }

    /// The build id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The platform property of the least system version.
const OS_VERSION: &str = "os_version";

/// The platform property that names a Mac action's Xcode, by build: the capability key a Mac's
/// agent reports its installed Xcodes under.
pub const XCODE: &str = kbf_caps::XCODE;

impl System {
    /// The system `platform` (the Action's, or else the Command's) asks for: macOS when it names
    /// `os=macos`, with its `os_version` and `xcode`, else Linux.
    ///
    /// # Errors
    /// A platform that does not parse (`xcode` given twice, in any two spellings, or empty
    /// among them), an `os_version` of a Mac action that is not a version
    /// (`<major>[.<minor>[.<patch>]]`, digits), or an `xcode` of a Mac action that is not a
    /// build id ([`XcodeBuild::parse`]).
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
        let properties = request.properties();
        let xcode = properties.iter().find(|(key, _)| *key == XCODE);
        let xcode = xcode
            .map(|(_, build)| XcodeBuild::parse(build))
            .transpose()?;
        Ok(Self::Macos { os_version, xcode })
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

/// The Macs of the executor: the task tool's bundle, the least macOS each runs and the tier
/// store's roots there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacPool {
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
/// pool is sure to run it. The executor holds this rule itself: the job is sent with no least
/// version (`Constraints.os_version` is left empty).
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
/// runtime, and the Xcode `xcode` installed.
#[must_use]
pub(super) fn mac_constraints(xcode: &XcodeBuild) -> Constraints {
    Constraints {
        os: MAC_OS.to_owned(),
        arch: MAC_ARCH.to_owned(),
        exec: ExecMode::SandboxExec.into(),
        isa_level: String::new(),
        accelerator: None,
        isolation: Isolation::Unspecified.into(),
        // The least version is held by the executor (`check_version`), not asked for.
        os_version: String::new(),
        xcode: xcode.as_str().to_owned(),
    }
}

/// The flags of the task tool for the Mac run of `action` on `run`: the inputs are copied out
/// of the tree at `exec/.kbf-tree` (`--materialize-work`), the object is committed into
/// `exec/.kbf-out`, and the action may run for `deadline`. Both paths are relative to the
/// task's working directory, `exec/`.
#[must_use]
pub(super) fn mac_argv(action: &Digest, run: RunId, deadline: Duration) -> Vec<String> {
    let in_exec = |target: &str| target.trim_start_matches("/work/").to_owned();
    vec![
        format!("--materialize-work={}", in_exec(TREE_TARGET)),
        "--stage-and-commit".to_owned(),
        "--out-dir".to_owned(),
        in_exec(MAC_OUT_TARGET),
        "--action".to_owned(),
        format!("{}/{}", action.hash, action.size_bytes),
        "--lease".to_owned(),
        build_id(run),
        "--deadline-ms".to_owned(),
        deadline.as_millis().to_string(),
        "--object-format".to_owned(),
        "one-pass".to_owned(),
    ]
}

/// The mount of `source` at `target`, `no_exec`, private; read-write or read-only.
fn mount_at(source: mount::Source, target: &str, mode: MountMode) -> Mount {
    Mount {
        source: Some(source),
        target: target.to_owned(),
        mode: mode.into(),
        sub_path: String::new(),
        no_exec: true,
        propagation: MountPropagation::Private.into(),
    }
}

/// The source of a Mac job's tree: tierstore's mount provider, given `key`, the key of the
/// tree's tree list.
fn tree_source(key: &str) -> mount::Source {
    mount::Source::Provider(ProviderSource {
        provider: PROVIDER.to_owned(),
        r#ref: key.to_owned(),
    })
}

/// Whether `spec` is a Mac job's: one that asks for `os=macos`. Read from the job the service
/// answers, so a job a gone process submitted is known as one too.
#[must_use]
pub(super) fn is_mac(spec: Option<&JobSpec>) -> bool {
    let constraints = spec.and_then(|spec| spec.constraints.as_ref());
    constraints.is_some_and(|c| c.os == MAC_OS)
}

/// What a Mac job is made of, beside what every job carries.
pub(super) struct MacJob<'a> {
    /// The job's id.
    pub(super) job_id: String,
    /// The pool.
    pub(super) pool: &'a MacPool,
    /// The Xcode the action names.
    pub(super) xcode: &'a XcodeBuild,
    /// The tier store's roots on the Macs.
    pub(super) roots: &'a PutDirRoots,
    /// The run's put directory, `cas/b/<run>/<nonce>`.
    pub(super) dir: &'a str,
    /// The key of the run's tree list, which tierstore's provider is asked for.
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

/// The Mac job of `job`: the pool's task bundle, whose entrypoint runs, the tree (from
/// tierstore's provider) and the put directory mounted under `/work`, no layer, no network, of
/// a Mac with the action's Xcode.
pub(super) fn mac_spec(job: MacJob<'_>) -> JobSpec {
    JobSpec {
        job_id: job.job_id,
        image: job.pool.bundle.clone(),
        // Empty: the bundle's entrypoint, `kbf-task`, runs with the arguments.
        command: String::new(),
        args: job.args,
        env: BTreeMap::new(),
        needs: Some(job.needs),
        constraints: Some(mac_constraints(job.xcode)),
        wall_limit_seconds: Some(job.wall_seconds),
        // The inputs are the tree and the outputs the mount: the job reaches nothing.
        network: NetworkLevel::None.into(),
        ports: Vec::new(),
        host_ports: Vec::new(),
        layers: Vec::new(),
        mounts: vec![
            mount_at(tree_source(job.tree), TREE_TARGET, MountMode::ReadOnly),
            mount_at(
                mount::Source::HostPath(format!("{}/{}", job.roots.put(), job.dir)),
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
