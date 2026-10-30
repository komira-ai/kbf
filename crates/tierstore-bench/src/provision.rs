//! `provision`: the test buckets, one scoped account per store whose policy reaches only those
//! buckets, and a 30 GiB quota on each bucket.
//!
//! Run by engineering with the admin key pair of each store in a file the ops file names
//! (`admin`, see [`crate::preflight::ops`]); the harness's own keys never create anything. The
//! default is a dry run that prints what it would do and reads and checks its inputs (the admin
//! key file, the endpoint, the live bucket named for the probe) but runs no command and sends no
//! request; `--apply` does it. For each store role of the run that is a shared store (`hot`,
//! `cold`) it:
//! 1. makes the test bucket, after finding that the store has none of that name (one that is
//!    there is kept only if this run made it, see [`state`]; any other stops the verb);
//! 2. sets a hard quota of 30 GiB on it, which the harness cannot lift;
//! 3. writes the policy that allows the bucket's own objects and listings and nothing else, to
//!    `policy-<role>.json` of the result directory, and creates it;
//! 4. adds the scoped account (the key pair the run file holds for the role), after finding
//!    that the store has no account of that id (one that is there is kept only if this run made
//!    it; any other stops the verb, since `mc admin user add` would reset its secret), and
//!    attaches the policy. The secret goes to `mc` on its standard input, not its arguments;
//! 5. probes: the account lists its test bucket (must be served), and reads a key of the live
//!    bucket the ops file names (must be refused with 403). A live bucket the account reaches
//!    stops the verb after it removes the account, if this run made it.
//!
//! Before the first command every bucket and account id is checked against the run's test prefix
//! ([`RunConfig::check_names`]); a name outside it stops the verb with nothing sent. What the run
//! makes is written to `provision-state.json` as it is made.
//!
//! The commands are `mc`'s, driven through [`crate::preflight::mc`]; the steps are written to
//! `provision.json` and printed, by role and never by bucket name.

pub mod probe;
pub mod state;

use serde::Serialize;

use self::probe::Access;
use self::state::State;
use crate::config::{RunConfig, StoreSpec};
use crate::error::BenchError;
use crate::preflight::host::{Host, SystemHost};
use crate::preflight::mc::{Mc, QUOTA_SIZE};
use crate::preflight::ops::{KeyPair, OpsFile};
use crate::preflight::write_json;
use crate::role::Role;

/// The roles whose stores are shared with the farm and so get a scoped account.
pub const SHARED_ROLES: [Role; 2] = [Role::Hot, Role::Cold];

/// The file of the result directory the steps are written to.
pub const STEPS_FILE: &str = "provision.json";

/// What a provision run does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Print the plan; change nothing.
    DryRun,
    /// Create the buckets, the accounts and the quotas.
    Apply,
}

/// One line per step taken or planned.
pub type Steps = Vec<String>;

/// The steps for one role, in order.
#[derive(Clone, Copy, Debug)]
enum Step {
    MakeBucket,
    SetQuota,
    MakePolicy,
    AddAccount,
    AttachPolicy,
    ProbeOwn,
    ProbeLive,
}

const STEPS: [Step; 7] = [
    Step::MakeBucket,
    Step::SetQuota,
    Step::MakePolicy,
    Step::AddAccount,
    Step::AttachPolicy,
    Step::ProbeOwn,
    Step::ProbeLive,
];

/// The step in words, by role.
fn describe(step: Step, role: Role) -> String {
    match step {
        Step::MakeBucket => format!("role {role}: make the test bucket"),
        Step::SetQuota => format!("role {role}: set the test bucket's hard quota to {QUOTA_SIZE}"),
        Step::MakePolicy => {
            format!("role {role}: create the policy that reaches the test bucket only")
        }
        Step::AddAccount => format!("role {role}: add the scoped account"),
        Step::AttachPolicy => format!("role {role}: attach the policy to the account"),
        Step::ProbeOwn => format!("role {role}: the account must list its test bucket"),
        Step::ProbeLive => {
            format!("role {role}: the account must be refused a read of the named live bucket")
        }
    }
}

/// The policy document of a scoped account: listing and multipart bookkeeping on `bucket`, and
/// reads, writes and deletes of the objects in `bucket`. No other resource appears in it, so the
/// account cannot reach another bucket.
#[must_use]
pub fn policy_document(bucket: &str) -> serde_json::Value {
    serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [
            {
                "Effect": "Allow",
                "Action": [
                    "s3:GetBucketLocation",
                    "s3:ListBucket",
                    "s3:ListBucketMultipartUploads"
                ],
                "Resource": [format!("arn:aws:s3:::{bucket}")]
            },
            {
                "Effect": "Allow",
                "Action": [
                    "s3:GetObject",
                    "s3:PutObject",
                    "s3:DeleteObject",
                    "s3:AbortMultipartUpload",
                    "s3:ListMultipartUploadParts"
                ],
                "Resource": [format!("arn:aws:s3:::{bucket}/*")]
            }
        ]
    })
}

/// What one role's steps need.
struct Target<'a> {
    role: Role,
    spec: &'a StoreSpec,
    live_bucket: &'a str,
    mc: Mc<'a>,
    policy_name: String,
    policy_file: std::path::PathBuf,
}

/// The text of a probe that did not give the answer the step needs.
fn unexpected(what: &str, access: &Access) -> BenchError {
    BenchError::Ops(format!("{what}: got {access:?}"))
}

/// Asks the store about the test bucket: `Ok(true)` if it is there and this run made it (kept),
/// `Ok(false)` if the store has none, an error if it is there and is someone else's.
fn bucket_kept(target: &Target<'_>, state: &State) -> Result<bool, BenchError> {
    let name = &target.spec.bucket;
    if !target.mc.bucket_exists(name)? {
        return Ok(false);
    }
    if state.made_bucket(name) {
        return Ok(true);
    }
    Err(BenchError::Ops(format!(
        "role {}: the test bucket is already on the store and this run did not make it",
        target.role
    )))
}

/// [`bucket_kept`] for the scoped account: an account that is there and not this run's is never
/// touched, since `mc admin user add` would reset its secret.
fn account_kept(target: &Target<'_>, state: &State) -> Result<bool, BenchError> {
    let id = &target.spec.access_key_id;
    if !target.mc.account_exists(id)? {
        return Ok(false);
    }
    if state.made_account(id) {
        return Ok(true);
    }
    Err(BenchError::Ops(format!(
        "role {}: the scoped account is already on the store and this run did not make it",
        target.role
    )))
}

/// Does one step.
async fn execute(step: Step, target: &Target<'_>, state: &mut State) -> Result<(), BenchError> {
    let spec = target.spec;
    match step {
        Step::MakeBucket => {
            if bucket_kept(target, state)? {
                return Ok(());
            }
            target.mc.make_bucket(&spec.bucket)?;
            state.record_bucket(&spec.bucket)
        }
        Step::SetQuota => target.mc.set_quota(&spec.bucket),
        Step::MakePolicy => {
            write_json(&target.policy_file, &policy_document(&spec.bucket))?;
            target
                .mc
                .create_policy(&target.policy_name, &target.policy_file)
        }
        Step::AddAccount => {
            if account_kept(target, state)? {
                return Ok(());
            }
            target.mc.add_account(&KeyPair {
                access_key_id: spec.access_key_id.clone(),
                secret_access_key: spec.secret_access_key.clone(),
            })?;
            state.record_account(&spec.access_key_id)
        }
        Step::AttachPolicy => target
            .mc
            .attach_policy(&target.policy_name, &spec.access_key_id),
        Step::ProbeOwn => match probe::can_list(spec).await? {
            Access::Reached => Ok(()),
            other => Err(unexpected(
                &format!(
                    "role {}: the scoped account cannot list its test bucket",
                    target.role
                ),
                &other,
            )),
        },
        Step::ProbeLive => match probe::can_read(spec, target.live_bucket).await? {
            Access::Denied(_) => Ok(()),
            other => {
                let removed = match state.remove_account(&target.mc, &spec.access_key_id) {
                    Ok(()) => "the account was removed",
                    Err(_) => "REMOVE THE ACCOUNT BY HAND",
                };
                Err(unexpected(
                    &format!(
                        "role {}: the scoped account was not refused the live bucket ({removed})",
                        target.role
                    ),
                    &other,
                ))
            }
        },
    }
}

/// Checks the inputs of `role` and builds what its steps need.
fn prepare<'a>(
    host: &'a dyn Host,
    config: &RunConfig,
    ops: &'a OpsFile,
    role: Role,
    spec: &'a StoreSpec,
) -> Result<Target<'a>, BenchError> {
    let live_bucket = ops
        .live_buckets
        .get(&role)
        .ok_or_else(|| BenchError::Ops(format!("the ops file names no live bucket for {role}")))?;
    if *live_bucket == spec.bucket {
        return Err(BenchError::Ops(format!(
            "role {role}: the test bucket is the live bucket"
        )));
    }
    Ok(Target {
        role,
        spec,
        live_bucket,
        mc: Mc::for_role(host, ops, role, spec, &config.result_dir)?,
        policy_name: format!("tierstore-bench-{role}"),
        policy_file: config.result_dir.join(format!("policy-{role}.json")),
    })
}

/// What `provision.json` holds.
#[derive(Serialize)]
struct StepsFile<'a> {
    mode: &'a str,
    steps: &'a [String],
}

/// Creates (or, in a dry run, lists) the test buckets, scoped accounts and quotas. Reads the
/// ops file (`ops.json` of the result directory), writes `provision.json` and prints the steps.
///
/// # Errors
/// [`BenchError`] if the ops file or an admin key file cannot be read, a role has no live bucket
/// named or its test bucket is the live one, a command fails, or a probe does not give the
/// answer isolation needs; a bucket or account id outside the test prefix, a bucket or account
/// already on the store that this run did not make.
pub async fn run(config: &RunConfig, mode: Mode) -> Result<Steps, BenchError> {
    let host = SystemHost;
    let ops = OpsFile::load(&host, &config.result_dir)?;
    run_with(&host, config, &ops, mode).await
}

/// [`run`] on a given host and ops file.
///
/// # Errors
/// As [`run`], but for the ops file.
pub async fn run_with(
    host: &dyn Host,
    config: &RunConfig,
    ops: &OpsFile,
    mode: Mode,
) -> Result<Steps, BenchError> {
    config.check_names()?;
    let roles: Vec<(Role, &StoreSpec)> = SHARED_ROLES
        .into_iter()
        .filter_map(|role| config.stores.get(&role).map(|spec| (role, spec)))
        .collect();
    if roles.is_empty() {
        return Err(BenchError::Ops(
            "the run configures no hot or cold store to provision".to_owned(),
        ));
    }
    let targets = roles
        .into_iter()
        .map(|(role, spec)| prepare(host, config, ops, role, spec))
        .collect::<Result<Vec<_>, _>>()?;
    let mut state = State::load(&config.result_dir, &config.test_prefix)?;
    if mode == Mode::Apply {
        // Everything that could be someone else's is looked for before anything is changed, so a
        // refusal leaves the store as it was.
        for target in &targets {
            bucket_kept(target, &state)?;
            account_kept(target, &state)?;
        }
    }
    let mut steps = Steps::new();
    for target in &targets {
        for step in STEPS {
            let line = describe(step, target.role);
            if mode == Mode::Apply {
                execute(step, target, &mut state).await?;
                steps.push(format!("done: {line}"));
            } else {
                steps.push(format!("plan: {line}"));
            }
        }
    }
    let label = if mode == Mode::Apply {
        "apply"
    } else {
        "dry-run"
    };
    write_json(
        &config.result_dir.join(STEPS_FILE),
        &StepsFile {
            mode: label,
            steps: &steps,
        },
    )?;
    for line in &steps {
        println!("{line}");
    }
    Ok(steps)
}
