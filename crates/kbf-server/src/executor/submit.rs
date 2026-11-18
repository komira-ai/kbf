//! What a run becomes: the job id that makes its submit idempotent, and the job.
//!
//! A run is one job ([`job_id`]): its id is the run's operation and attempt, with the hash of
//! the frontends' name before it, so a submit sent again with the job its run kept, after its
//! answer was lost, names the job already made, and the service answers from it; and a frontend
//! that takes up an unfinished operation from the state store computes the same id. Two labels
//! name the frontends and the process; a third, `size-class`, names the action's class
//! ([`crate::executor::size_class`]), its target and kind, except on a whole-host job, which
//! books a whole host and so is never sized.
//!
//! A Linux run commits into a put directory (`--task-out-root`, [`Config::task_out`]) and is a
//! job ([`put_dir_spec`]): no image, its root the action tree as its one layer (the host
//! directory of its manifests, or with `--executor-input-tree provider` tierstore's mount
//! provider naming its tree list, [`provider_layer`]), the task tool of the base tree as its
//! program, the put directory mounted read-write at `/out`, no network, no files and no secret.
//! It names no CPU, memory or disk, so the job service books it what the history of its size
//! class says, or, for an action whose platform asks for a whole machine
//! (`kbf-lease=whole_machine`), it needs the whole host and nothing beside it ([`Booking`]); it
//! is of a Linux host of the configured architecture in a container, for the action's timeout
//! and the run margin. A whole-host run cannot be one: the job service gives a whole-host job
//! no layer and no mount, so it is refused as the caller's. An executor given no roots refuses
//! every run as the caller's, naming `--task-out-root`.
//!
//! An action whose platform asks for a Mac is a Mac job ([`crate::executor::mac`]): the
//! release's `kbf-task` bundle, the tree and the put directory mounted under `/work`, no layer, of
//! a Mac in the native runtime, with no CPU or memory and the class's label. It is refused as
//! the caller's on an executor with no Macs, and as for Linux for a whole machine.
//!
//! A [`JobSpec`] is sent and kept, and never logged.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_proto::v1::{JobSpec, Needs, NetworkLevel};
use kbf_proto::reapi::Digest;
use ring::digest::{SHA256, digest};

use crate::executor::config::{Config, InputTree};
use crate::executor::mac::{MacJob, System, check_version, mac_argv, mac_spec};
use crate::executor::out::{PutDirRoots, TASK_IN_TREE, provider_layer};
use crate::executor::outcome::Settled;
use crate::executor::size_class::{SIZE_CLASS_LABEL, SizeClass};
use crate::executor::task::{PackKey, RunId, build_id, put_dir_argv};
use crate::executor::traits::{InputsError, MintError};
use crate::executor::{Ingest, Inputs};

/// The label every job of this executor carries, valued with the frontends' name.
pub(super) const FRONTEND_LABEL: &str = "kbf-frontend";

/// The label every job carries beside the frontends': valued with the id of the process that
/// submitted it.
pub(super) const INSTANCE_LABEL: &str = "kbf-instance";

/// The id of the job of `run`:
/// `k<the first 8 hex digits of the sha256 of the frontend id>-<operation>-<attempt>`.
///
/// It is at most 41 bytes (`k`, 8 digits, two dashes and two decimal numbers of at most 20 and
/// 10 digits) of letters, digits and dashes, which the service takes. The frontend id is
/// hashed, not written, so an id of any length fits and two farms' ids do not meet. Equal
/// inputs give equal ids: that is what makes a submit sent again name the job already made, and
/// lets a frontend that takes up the operation read the job back. It has three parts where the
/// ids of earlier releases had four, so it never names one of their jobs.
#[must_use]
pub fn job_id(frontend: &str, run: RunId) -> String {
    let hash = digest(&SHA256, frontend.as_bytes());
    let prefix = hash
        .as_ref()
        .iter()
        .take(4)
        .fold(0_u32, |acc, byte| (acc << 8) | u32::from(*byte));
    format!("k{prefix:08x}-{}", build_id(run))
}

/// A job ready to submit.
pub(super) struct Prepared {
    pub(super) spec: JobSpec,
    pub(super) key: PackKey,
}

impl From<InputsError> for Settled {
    fn from(error: InputsError) -> Self {
        match error {
            InputsError::Invalid(why) => Self::Invalid(why),
            InputsError::Unavailable(why) => Self::Infra(why),
        }
    }
}

/// `image` as the job service takes it, if it is pinned by digest: a name, then `@sha256:`
/// and 64 lower-case hex digits, as the service checks it. The `docker://` scheme the platform
/// property carries is dropped.
pub(super) fn pinned(image: &str) -> Result<String, Settled> {
    let image = image.strip_prefix("docker://").unwrap_or(image);
    let lower_hex = |b: u8| matches!(b, b'0'..=b'9' | b'a'..=b'f');
    let pinned = image
        .rsplit_once("@sha256:")
        .is_some_and(|(name, d)| !name.is_empty() && d.len() == 64 && d.bytes().all(lower_hex));
    match pinned {
        true => Ok(image.to_owned()),
        false => Err(Settled::Invalid(format!(
            "the action's image {image:?} is not pinned by an @sha256 digest"
        ))),
    }
}

fn require(ok: bool, why: impl FnOnce() -> String) -> Result<(), Settled> {
    match ok {
        true => Ok(()),
        false => Err(Settled::Invalid(why())),
    }
}

/// Whole seconds of `wall`, rounded up: a job is never given less than it asks for.
pub(super) fn wall_seconds(wall: Duration) -> u64 {
    wall.as_secs() + u64::from(wall.subsec_nanos() > 0)
}

/// What a run asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Booking {
    /// A share of a host, which the job service sizes from the job's class.
    Share,
    /// The whole host, with no other job beside it (`kbf-lease=whole_machine`).
    WholeHost,
}

impl Booking {
    /// What an action asks for: the whole host when its platform says `whole_machine`.
    pub(super) const fn of(whole_machine: bool) -> Self {
        match whole_machine {
            true => Self::WholeHost,
            false => Self::Share,
        }
    }

    /// What a job needs. A share names no CPU, no memory and no disk: the job service books
    /// the CPU and the memory from the history of the job's size class (its label), and the
    /// pool books no disk, so the job is placed on any host whatever disk it reports and writes
    /// to its host's scratch with no quota. The whole host is asked for alone: the job service
    /// takes a whole-host job only with no other need, and the host's cores and memory are all
    /// the job's.
    fn needs(self) -> Needs {
        Needs {
            whole_host: self == Self::WholeHost,
            ..Needs::default()
        }
    }

    /// The class to label the job with: `class` for a share, none for the whole host, which
    /// is never sized and whose runs would teach the class a whole host's use.
    fn class(self, class: &SizeClass) -> Option<&SizeClass> {
        (self != Self::WholeHost).then_some(class)
    }
}

/// What preparing a run reads.
pub(super) struct Preparer<'a> {
    pub(super) cfg: &'a Config,
    pub(super) inputs: &'a Arc<dyn Inputs>,
    pub(super) ingest: &'a Arc<dyn Ingest>,
}

/// The job of `run`, which runs `action` with `booking`, with its put directory.
pub(super) async fn prepare(
    ctx: &Preparer<'_>,
    run: RunId,
    action: &Digest,
    booking: Booking,
) -> Result<Prepared, Settled> {
    let facts = ctx.inputs.action(action).await?;
    let class = &facts.size_class;
    let timeout = facts.timeout.unwrap_or(ctx.cfg.default_timeout);
    require(timeout <= ctx.cfg.max_run, || {
        format!("the action's timeout {timeout:?} is over the executor's limit")
    })?;
    pinned(&facts.image)?;
    let wall = ctx.cfg.wall(timeout);
    if let System::Macos { os_version } = facts.system {
        let times = (timeout, wall);
        return prepare_mac(ctx, run, action, booking, times, os_version, class).await;
    }
    let Some(roots) = &ctx.cfg.task_out else {
        return Err(no_roots("a Linux action"));
    };
    require(booking != Booking::WholeHost, || {
        "a whole_machine action cannot commit into a put directory: the job service gives a \
         whole-host job no mount"
            .to_owned()
    })?;
    let key = minted(ctx, run, action, wall, ctx.cfg.input_tree).await?;
    let tree = ctx.inputs.tree(run).await?;
    let job = PutDirJob {
        roots,
        dir: &key,
        tree: &tree,
    };
    let spec = put_dir_spec(ctx, &job, run, action, (timeout, wall), (booking, class));
    Ok(Prepared { spec, key })
}

/// The refusal of a run of `what` when the executor was given no roots for the put directories.
fn no_roots(what: &str) -> Settled {
    Settled::Invalid(format!(
        "{what} commits into a put directory: give --task-out-root"
    ))
}

/// The Mac job of `run`, which runs `action` with `booking` for `times` (the timeout and the
/// wall limit), labelled with `class`: refused as the caller's when this executor has no Macs,
/// when the action asks for a newer macOS than they run, when it asks for a whole machine, or
/// when its outputs do not go into a put directory.
async fn prepare_mac(
    ctx: &Preparer<'_>,
    run: RunId,
    action: &Digest,
    booking: Booking,
    (timeout, wall): (Duration, Duration),
    os_version: Option<u32>,
    class: &SizeClass,
) -> Result<Prepared, Settled> {
    let Some(pool) = &ctx.cfg.mac else {
        return Err(Settled::Invalid(
            "the action asks for a Mac, and this executor has none (--executor-mac-task-bundle)"
                .to_owned(),
        ));
    };
    check_version(os_version, pool).map_err(Settled::Invalid)?;
    if booking == Booking::WholeHost {
        return Err(Settled::Invalid(
            "a whole_machine action cannot run on a Mac of the job service: it commits into a \
             put directory, and a whole-host job has no mount"
                .to_owned(),
        ));
    }
    let Some(linux) = &ctx.cfg.task_out else {
        return Err(no_roots("a Mac action"));
    };
    // The native runtime takes no mount provider: a Mac job's tree is always its manifests.
    let key = minted(ctx, run, action, wall, InputTree::Manifests).await?;
    let tree = ctx.inputs.tree(run).await?;
    let spec = mac_spec(MacJob {
        job_id: job_id(&ctx.cfg.frontend, run),
        pool,
        roots: pool.roots(linux),
        dir: key.as_str(),
        tree: &tree,
        args: mac_argv(action, run, timeout),
        needs: booking.needs(),
        wall_seconds: wall_seconds(wall),
        labels: labels(ctx, Some(class)),
    });
    Ok(Prepared { spec, key })
}

/// The put directory of `run`, from the frontend's ingest, its input tree written as `tree` says.
async fn minted(
    ctx: &Preparer<'_>,
    run: RunId,
    action: &Digest,
    wall: Duration,
    tree: InputTree,
) -> Result<PackKey, Settled> {
    let minted = ctx.ingest.mint(run, action, wall, tree).await;
    minted.map_err(|e| match e {
        MintError::Refused(why) => Settled::Invalid(format!("no put directory: {why}")),
        MintError::Unavailable(why) => Settled::Infra(format!("no put directory: {why}")),
        MintError::Missing(digests) => Settled::Missing(digests),
    })
}

/// The labels of a job: the frontends' name and this process's id, and its size class if it
/// has one.
fn labels(ctx: &Preparer<'_>, class: Option<&SizeClass>) -> BTreeMap<String, String> {
    let class = class.map(|class| (SIZE_CLASS_LABEL, class.as_str()));
    [
        (FRONTEND_LABEL, ctx.cfg.frontend.as_str()),
        (INSTANCE_LABEL, ctx.cfg.instance.as_str()),
    ]
    .into_iter()
    .chain(class)
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect()
}

/// Where a put-directory job reads and writes: the roots, the run's put directory
/// (`cas/b/<run>/<nonce>`) and its action tree (the digest in hex, or with
/// [`InputTree::Provider`] the key of its tree list).
struct PutDirJob<'a> {
    roots: &'a PutDirRoots,
    dir: &'a PackKey,
    tree: &'a str,
}

/// The job of `run` of `job`, with its booking and size class: the tree is the root,
/// the tool of the base tree runs, the directory is at `/out`, and nothing else is given.
fn put_dir_spec(
    ctx: &Preparer<'_>,
    job: &PutDirJob<'_>,
    run: RunId,
    action: &Digest,
    (timeout, wall): (Duration, Duration),
    (booking, class): (Booking, &SizeClass),
) -> JobSpec {
    JobSpec {
        job_id: job_id(&ctx.cfg.frontend, run),
        // No image: the action tree is the whole root, the base tree in it.
        image: String::new(),
        command: TASK_IN_TREE.to_owned(),
        args: put_dir_argv(action, run, timeout),
        env: BTreeMap::new(),
        needs: Some(booking.needs()),
        constraints: Some(ctx.cfg.constraints()),
        wall_limit_seconds: Some(wall_seconds(wall)),
        // The inputs are the tree and the outputs the mount: the job reaches nothing.
        network: NetworkLevel::None.into(),
        ports: Vec::new(),
        host_ports: Vec::new(),
        layers: vec![match ctx.cfg.input_tree {
            InputTree::Manifests => job.roots.layer(job.tree),
            InputTree::Provider => provider_layer(job.tree),
        }],
        mounts: vec![job.roots.mount(job.dir.as_str())],
        files: Vec::new(),
        priority: 0,
        labels: labels(ctx, booking.class(class)),
    }
}

#[cfg(test)]
#[path = "submit_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "submit_put_dir_tests.rs"]
mod put_dir_tests;

#[cfg(test)]
#[path = "submit_mac_tests.rs"]
mod mac_tests;
