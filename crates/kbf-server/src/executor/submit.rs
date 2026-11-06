//! What a `Start` becomes: the job id that makes its submit idempotent, and the job.
//!
//! A farm lease is one job ([`job_id`]): its id is the farm lease's build id, with the hash of
//! the frontend's id before it and an attempt after it, so a submit sent again with the job its
//! task kept, after its answer was lost, names the job already made, and the service answers
//! from it (design section 5.3). The job is the kbf image, whose entrypoint is the
//! task tool, run with the tool's flags; it needs what the farm booked, with the hard memory cap
//! a daemon would set and the scratch of its inputs and outputs, of a Linux host of the
//! configured architecture in a container, for the action's timeout and the run margin. The
//! read grant and the presigned `PUT` travel as the job's files, or as arguments where the
//! operator allowed it (`insecure_argv_secrets`). Two labels name the frontend and the
//! process, by which a restarted frontend finds the jobs of an earlier one.
//!
//! A [`JobSpec`] holds the secrets: it is sent and kept, never logged or put in an error.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use js_proto::v1::{JobSpec, Needs, NetworkLevel};
use kbf_proto::reapi::Digest;
use kbf_types::LeaseId;
use ring::digest::{SHA256, digest};

use crate::executor::Ctx;
use crate::executor::config::Config;
use crate::executor::outcome::Settled;
use crate::executor::task::{PackKey, TaskArgs, build_id};
use crate::executor::traits::{InputsError, MintError, PresignError};

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

/// What a `Start` asks for: the CPU and memory the farm booked.
#[derive(Clone, Copy, Debug)]
pub(super) struct Booking {
    pub(super) millicpus: u64,
    pub(super) memory_bytes: u64,
}

/// The job of `lease`, which runs `action` with `booking`, with its pack key.
pub(super) async fn prepare(
    ctx: &Ctx<'_>,
    worker: &str,
    lease: LeaseId,
    action: &Digest,
    booking: Booking,
) -> Result<Arc<Prepared>, Settled> {
    let facts = ctx.deps.inputs.action(action).await?;
    let timeout = facts.timeout.unwrap_or(ctx.cfg.default_timeout);
    require(timeout <= ctx.cfg.max_run, || {
        format!("the action's timeout {timeout:?} is over the executor's limit")
    })?;
    let image = pinned(&facts.image)?;
    let wall = ctx.cfg.wall(timeout);
    let key = ctx
        .deps
        .ingest
        .mint(lease, action, wall)
        .await
        .map_err(|e| match e {
            MintError::Refused(why) => Settled::Invalid(format!("no key for the pack: {why}")),
            MintError::Unavailable(why) => Settled::Infra(format!("no key for the pack: {why}")),
        })?;
    let pack = ctx
        .deps
        .presign
        .put_url(&key, wall, ctx.cfg.max_pack_bytes)
        .await?;
    // The signed url is the task's write permission: sent in clear only if the operator
    // said the store does not serve TLS yet.
    if !ctx.cfg.allow_plain_http_put && !pack.expose().starts_with("https://") {
        return Err(Settled::Infra("the pack url is not https".to_owned()));
    }
    let grant = ctx.deps.inputs.read_grant(lease, action, wall).await?;
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
    if grant.input_bytes.is_none() {
        tracing::warn!(
            %worker,
            %lease,
            "the size of the inputs is not known; the job needs the allowance and the margin only"
        );
    }
    let spec = JobSpec {
        job_id: job_id(&ctx.cfg.frontend, lease, 0),
        image,
        // Empty: the image's own entrypoint, the task tool, runs with the arguments.
        command: String::new(),
        args: args.argv(ctx.cfg.delivery),
        env: BTreeMap::new(),
        needs: Some(Needs {
            cpu_millis: booking.millicpus.max(1),
            memory_bytes: Config::hard_memory(booking.memory_bytes),
            disk_bytes: Some(ctx.cfg.disk_bytes(grant.input_bytes)),
            gpus: 0,
            gpu_class: String::new(),
            whole_host: false,
        }),
        constraints: Some(ctx.cfg.constraints()),
        wall_limit_seconds: Some(wall_seconds(wall)),
        // The task tool reaches the read listener and the store, and nothing else: the
        // endpoints the host's agent allows.
        network: NetworkLevel::AllowList.into(),
        ports: Vec::new(),
        files: args.files(ctx.cfg.delivery),
        priority: 0,
        labels: [
            (FRONTEND_LABEL, ctx.cfg.frontend.as_str()),
            (INSTANCE_LABEL, ctx.cfg.instance.as_str()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect(),
    };
    Ok(Arc::new(Prepared { spec, key }))
}

#[cfg(test)]
#[path = "submit_tests.rs"]
mod tests;
