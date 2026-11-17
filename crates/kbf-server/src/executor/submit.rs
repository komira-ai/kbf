//! What a `Start` becomes: the job id that makes its submit idempotent, and the job.
//!
//! A farm lease is one job ([`job_id`]): its id is the farm lease's build id, with the hash of
//! the frontend's id before it and an attempt after it, so a submit sent again with the job its
//! task kept, after its answer was lost, names the job already made, and the service answers
//! from it (design section 5.3). The job is the kbf image, whose entrypoint is the
//! task tool, run with the tool's flags; it names no CPU, memory or disk, so the job service
//! books it what the history of its size class says, or, for a `whole_machine` lease, it needs
//! the whole host and nothing beside it ([`Booking`]), of a Linux host of the configured
//! architecture in a container, for the action's timeout and the run margin. The read grant and
//! the presigned `PUT` travel as the job's files, or as arguments where the operator allowed it
//! (`insecure_argv_secrets`). Two labels name the frontend and the process, by which a
//! restarted frontend finds the jobs of an earlier one; a third, `size-class`, names the
//! action's class ([`crate::executor::size_class`]), its target and kind, except on a
//! `whole_machine` job, which books a whole host and so is never sized.
//!
//! A run that commits into a put directory (`--task-out-mode put-dir`, [`TaskOut::PutDir`]) is
//! another job ([`put_dir_spec`]): no image, its root the action tree as its one layer, the
//! task tool of the base tree as its program, the put directory mounted read-write at `/out`,
//! no network, no files and no secret, and needs, and is labelled, as the other. A
//! `whole_machine` lease cannot be one: the job service gives a whole-host job no layer and no
//! mount, so it is refused as the caller's.
//!
//! An action whose platform asks for a Mac is a Mac job ([`crate::executor::mac`]): the
//! release's `kbf-task` bundle, the tree and the put directory mounted under `/work`, no layer, of
//! a Mac in the native runtime. It needs what the farm booked ([`named_needs`]) and carries the
//! class's label: the job service takes the size of a class with no history from the largest
//! host it knows, a Linux host, which a Mac may not hold. It is refused as the caller's on an
//! executor with no Macs, and as for Linux for a `whole_machine` lease.
//!
//! An action whose platform asks for another system than the worker it was placed on stands
//! for is refused as the caller's ([`same_system`]).
//!
//! A [`JobSpec`] holds the secrets: it is sent and kept, never logged or put in an error.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_proto::v1::{JobSpec, Needs, NetworkLevel};
use kbf_proto::reapi::Digest;
use kbf_proto::worker::Start;
use kbf_types::LeaseId;
use ring::digest::{SHA256, digest};

use crate::executor::config::Config;
use crate::executor::mac::{MacJob, System, WorkerOs, check_version, mac_argv, mac_spec};
use crate::executor::out::{PutDirRoots, TASK_IN_TREE, TaskOut};
use crate::executor::outcome::Settled;
use crate::executor::output::Grants;
use crate::executor::size_class::{SIZE_CLASS_LABEL, SizeClass};
use crate::executor::task::{PackKey, TaskArgs, build_id, put_dir_argv};
use crate::executor::traits::{InputsError, MintError, PresignError};
use crate::executor::{Ctx, Ingest, Inputs, Presign};

/// The label every job of this executor carries, valued with the frontend's id: how a
/// restarted frontend finds the jobs it made.
pub(super) const FRONTEND_LABEL: &str = "kbf-frontend";

/// The label every job carries beside the frontend's: valued with an id this process made up
/// when it started. Two processes started with the same frontend id (a copied config) are
/// told apart by it, and a job made by a process that is not this one is never taken for this
/// one's.
pub(super) const INSTANCE_LABEL: &str = "kbf-instance";

/// The id of the job of `lease`, the `attempt`th this frontend submits for it:
/// `k<the first 8 hex digits of the sha256 of the frontend id>-<term>-<seq>-<attempt>`.
///
/// It is at most 62 bytes (`k`, 8 digits, three dashes and three decimal numbers of at most
/// 20, 20 and 10 digits) of letters, digits and dashes, which the service takes. The frontend
/// id is hashed, not written, so an id of any length fits and two frontends' ids do not meet.
/// Equal inputs give equal ids: that is what makes a `Start` sent again, and a submit sent
/// again, name the job already made. The executor submits attempt 0 for every farm lease: the
/// farm runs an action again only as a new farm lease, so no lease is submitted twice on
/// purpose; another attempt is a new job under the same lease.
#[must_use]
pub fn job_id(frontend: &str, lease: LeaseId, attempt: u32) -> String {
    let hash = digest(&SHA256, frontend.as_bytes());
    let prefix = hash
        .as_ref()
        .iter()
        .take(4)
        .fold(0_u32, |acc, byte| (acc << 8) | u32::from(*byte));
    format!("k{prefix:08x}-{}-{attempt}", build_id(lease))
}

/// A job ready to submit.
pub(super) struct Prepared {
    pub(super) spec: JobSpec,
    pub(super) key: PackKey,
    /// What the job was handed that its attached output must never show.
    pub(super) grants: Grants,
}

impl From<InputsError> for Settled {
    fn from(error: InputsError) -> Self {
        match error {
            InputsError::Invalid(why) => Self::Invalid(why),
            InputsError::Unavailable(why) => Self::Infra(why),
        }
    }
}

impl From<PresignError> for Settled {
    fn from(error: PresignError) -> Self {
        let (PresignError::Refused(why) | PresignError::Unavailable(why)) = error;
        Self::Infra(format!("no url for the pack: {why}"))
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

/// The kind of a `Start` that asks for a whole machine.
const WHOLE_MACHINE: &str = "whole_machine";

/// What a `Start` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Booking {
    /// A share of a host: the CPU and memory the farm booked. A Linux job names neither and is
    /// sized by the job service; a Mac job names both ([`named_needs`]).
    Share { millicpus: u64, memory_bytes: u64 },
    /// A `whole_machine` lease: the whole host, with no other job beside it. The CPU and
    /// memory the `Start` carries are the virtual worker's whole capacity, not a host's, and
    /// are not asked for.
    WholeHost,
}

impl Booking {
    /// What `start` asks for: the whole host for a `whole_machine` lease, else the share it
    /// booked.
    pub(super) fn of(start: &Start) -> Self {
        match start.kind == WHOLE_MACHINE {
            true => Self::WholeHost,
            false => Self::Share {
                millicpus: start.millicpus,
                memory_bytes: start.memory_bytes,
            },
        }
    }

    /// What a Linux job needs. A share names no CPU, no memory and no disk: the job service
    /// books the CPU and the memory from the history of the job's size class (its label), and
    /// the pool books no disk, so the job is placed on any host whatever disk it reports and
    /// writes to its host's scratch with no quota. The whole host is asked for alone: the job
    /// service takes a whole-host job only with no other need, and the host's cores and memory
    /// are all the job's.
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

/// What a Mac job needs: at least one thousandth of a CPU and the hard memory cap of the
/// `memory_bytes` the farm booked, and no disk.
fn named_needs(millicpus: u64, memory_bytes: u64) -> Needs {
    Needs {
        cpu_millis: millicpus.max(1),
        memory_bytes: Config::hard_memory(memory_bytes),
        ..Needs::default()
    }
}

/// What preparing a run reads, owned: a preparation runs on a task of its own, beyond the pass
/// that began it and the window of its `Start`, so it holds no borrow of the pass.
#[derive(Clone)]
pub(super) struct Preparer {
    pub(super) cfg: Config,
    inputs: Arc<dyn Inputs>,
    ingest: Arc<dyn Ingest>,
    presign: Arc<dyn Presign>,
}

impl Ctx<'_> {
    /// The owned parts a preparation reads.
    pub(super) fn preparer(&self) -> Preparer {
        Preparer {
            cfg: self.cfg.clone(),
            inputs: Arc::clone(&self.deps.inputs),
            ingest: Arc::clone(&self.deps.ingest),
            presign: Arc::clone(&self.deps.presign),
        }
    }
}

/// The job of `lease`, which runs `action` with `booking` on a worker of `on`, with its pack key.
pub(super) async fn prepare(
    ctx: &Preparer,
    on: WorkerOs,
    lease: LeaseId,
    action: &Digest,
    booking: Booking,
) -> Result<Arc<Prepared>, Settled> {
    let facts = ctx.inputs.action(action).await?;
    same_system(on, facts.system)?;
    let class = &facts.size_class;
    let timeout = facts.timeout.unwrap_or(ctx.cfg.default_timeout);
    require(timeout <= ctx.cfg.max_run, || {
        format!("the action's timeout {timeout:?} is over the executor's limit")
    })?;
    let image = pinned(&facts.image)?;
    let wall = ctx.cfg.wall(timeout);
    if let System::Macos { os_version } = facts.system {
        let times = (timeout, wall);
        return prepare_mac(ctx, lease, action, booking, times, os_version, class).await;
    }
    if let TaskOut::PutDir(roots) = &ctx.cfg.task_out {
        require(booking != Booking::WholeHost, || {
            "a whole_machine action cannot commit into a put directory: the job service gives \
             a whole-host job no mount"
                .to_owned()
        })?;
        let key = minted(ctx, lease, action, wall).await?;
        let tree = ctx.inputs.tree(lease).await?;
        let job = PutDirJob {
            roots,
            dir: &key,
            tree: &tree,
        };
        let spec = put_dir_spec(ctx, &job, lease, action, (timeout, wall), (booking, class));
        let grants = Grants::of(&spec, []);
        return Ok(Arc::new(Prepared { spec, key, grants }));
    }
    let key = minted(ctx, lease, action, wall).await?;
    let pack = ctx
        .presign
        .put_url(&key, wall, ctx.cfg.max_pack_bytes)
        .await?;
    // The signed url is the task's write permission: sent in clear only if the operator
    // said the store does not serve TLS yet.
    if !ctx.cfg.allow_plain_http_put && !pack.expose().starts_with("https://") {
        return Err(Settled::Infra("the pack url is not https".to_owned()));
    }
    let grant = ctx.inputs.read_grant(lease, action, wall).await?;
    // The bearer token goes to the task: not over a scheme that sends it in clear, unless the
    // operator said the job network is trusted for it.
    if !ctx.cfg.allow_plain_http_grant && !grant.url.starts_with("https://") {
        return Err(Settled::Infra(
            "the read grant is not served over https".to_owned(),
        ));
    }
    let args = TaskArgs {
        action,
        lease,
        grant_url: &grant.url,
        grant_token: &grant.token,
        pack_url: &pack,
        deadline: timeout,
        work_dir: ctx.cfg.work_dir.as_deref(),
        allow_plain_http_put: ctx.cfg.allow_plain_http_put,
        allow_plain_http_grant: ctx.cfg.allow_plain_http_grant,
        locate: ctx.cfg.locate,
    };
    let spec = JobSpec {
        job_id: job_id(&ctx.cfg.frontend, lease, 0),
        image,
        // Empty: the image's own entrypoint, the task tool, runs with the arguments.
        command: String::new(),
        args: args.argv(ctx.cfg.delivery),
        env: BTreeMap::new(),
        needs: Some(booking.needs()),
        constraints: Some(ctx.cfg.constraints()),
        wall_limit_seconds: Some(wall_seconds(wall)),
        // The operator's level, by default the allow-list: the task tool reaches the read
        // listener and the store, the endpoints the host's agent allows, and nothing else.
        // Never left unspecified, which the service reads as none.
        network: ctx.cfg.network.level().into(),
        ports: Vec::new(),
        host_ports: Vec::new(),
        layers: Vec::new(),
        mounts: Vec::new(),
        files: args.files(ctx.cfg.delivery),
        priority: 0,
        labels: labels(ctx, booking.class(class)),
    };
    let grants = Grants::of(&spec, [pack.expose(), grant.token.expose()]);
    Ok(Arc::new(Prepared { spec, key, grants }))
}

/// Refuses an action placed on a worker of another system than its platform asks for: an action
/// that names no system matches a Mac worker too when the frontend completes no platform
/// (`--default-platform none`), and it would hold a Mac's slot to run on Linux.
fn same_system(on: WorkerOs, system: System) -> Result<(), Settled> {
    match (on, system) {
        (WorkerOs::Mac, System::Linux) => Err(Settled::Invalid(
            "the farm placed the action on a Mac worker, and its platform asks for no macOS: \
             name the system (OSFamily)"
                .to_owned(),
        )),
        (WorkerOs::Linux, System::Macos { .. }) => Err(Settled::Invalid(
            "the farm placed the action on a Linux worker, and its platform asks for macOS"
                .to_owned(),
        )),
        _ => Ok(()),
    }
}

/// The Mac job of `lease`, which runs `action` with `booking` for `times` (the timeout and the
/// wall limit), labelled with `class`: refused as the caller's when this executor has no Macs,
/// when the action asks for a newer macOS than they run, when it is a `whole_machine` lease, or
/// when its outputs do not go into a put directory.
async fn prepare_mac(
    ctx: &Preparer,
    lease: LeaseId,
    action: &Digest,
    booking: Booking,
    (timeout, wall): (Duration, Duration),
    os_version: Option<u32>,
    class: &SizeClass,
) -> Result<Arc<Prepared>, Settled> {
    let Some(pool) = &ctx.cfg.mac else {
        return Err(Settled::Invalid(
            "the action asks for a Mac, and this executor has none (--executor-mac-hosts)"
                .to_owned(),
        ));
    };
    check_version(os_version, pool).map_err(Settled::Invalid)?;
    let Booking::Share {
        millicpus,
        memory_bytes,
    } = booking
    else {
        return Err(Settled::Invalid(
            "a whole_machine action cannot run on a Mac of the job service: it commits into a \
             put directory, and a whole-host job has no mount"
                .to_owned(),
        ));
    };
    let TaskOut::PutDir(linux) = &ctx.cfg.task_out else {
        return Err(Settled::Invalid(
            "a Mac action commits into a put directory: give --task-out-root".to_owned(),
        ));
    };
    let key = minted(ctx, lease, action, wall).await?;
    let tree = ctx.inputs.tree(lease).await?;
    let spec = mac_spec(MacJob {
        job_id: job_id(&ctx.cfg.frontend, lease, 0),
        pool,
        roots: pool.roots(linux),
        dir: key.as_str(),
        tree: &tree,
        args: mac_argv(action, lease, timeout),
        needs: named_needs(millicpus, memory_bytes),
        wall_seconds: wall_seconds(wall),
        labels: labels(ctx, Some(class)),
    });
    let grants = Grants::of(&spec, []);
    Ok(Arc::new(Prepared { spec, key, grants }))
}

/// The key of `lease`'s run, from the frontend's ingest.
async fn minted(
    ctx: &Preparer,
    lease: LeaseId,
    action: &Digest,
    wall: Duration,
) -> Result<PackKey, Settled> {
    let minted = ctx.ingest.mint(lease, action, wall).await;
    minted.map_err(|e| match e {
        MintError::Refused(why) => Settled::Invalid(format!("no key for the pack: {why}")),
        MintError::Unavailable(why) => Settled::Infra(format!("no key for the pack: {why}")),
    })
}

/// The labels of a job: the frontend's id and this process's, and its size class if it has
/// one.
fn labels(ctx: &Preparer, class: Option<&SizeClass>) -> BTreeMap<String, String> {
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
/// (`cas/b/<lease>/<nonce>`) and its action tree (the digest, in hex).
struct PutDirJob<'a> {
    roots: &'a PutDirRoots,
    dir: &'a PackKey,
    tree: &'a str,
}

/// The job of `lease`'s run of `job`, with its booking and size class: the tree is the root,
/// the tool of the base tree runs, the directory is at `/out`, and nothing else is given.
fn put_dir_spec(
    ctx: &Preparer,
    job: &PutDirJob<'_>,
    lease: LeaseId,
    action: &Digest,
    (timeout, wall): (Duration, Duration),
    (booking, class): (Booking, &SizeClass),
) -> JobSpec {
    JobSpec {
        job_id: job_id(&ctx.cfg.frontend, lease, 0),
        // No image: the action tree is the whole root, the base tree in it.
        image: String::new(),
        command: TASK_IN_TREE.to_owned(),
        args: put_dir_argv(action, lease, timeout),
        env: BTreeMap::new(),
        needs: Some(booking.needs()),
        constraints: Some(ctx.cfg.constraints()),
        wall_limit_seconds: Some(wall_seconds(wall)),
        // The inputs are the tree and the outputs the mount: the job reaches nothing.
        network: NetworkLevel::None.into(),
        ports: Vec::new(),
        host_ports: Vec::new(),
        layers: vec![job.roots.layer(job.tree)],
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
