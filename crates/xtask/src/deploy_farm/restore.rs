//! `cargo xtask deploy-farm restore`: the last step of a deploy job that failed after (or
//! before) it set its release as the farm's desired version. It sets the farm back to the
//! version the release replaced, which the coordinator keeps (`docs/design/gamma-cicd.md`, G1).
//!
//! The steps, over the same HTTPS calls and the same CI identity as the deploy (a fresh OIDC
//! token for every call, since the coordinator accepts a token once; a 503 is tried again as
//! on every other call):
//!
//! 1. With an empty `--url` the verb prints a notice and exits 0, as the deploy does.
//! 2. `GET /v1/convergence`. When the desired version is not this release (another writer, a
//!    newer release or the coordinator's own rollback has it now), it sets nothing, says so
//!    and exits 0. When the coordinator shows no `generation` (it is older than the restore)
//!    or no `replaced` (a first deploy), it sets nothing and exits 1 naming the forced
//!    rollback.
//! 3. `POST /v1/desired` of `replaced` (its version and its minimums), with `if_generation`
//!    the generation read and `write_id` `restore-<run>`. It prints what it set and exits 1:
//!    the run stays red.
//! 4. On a 409 (some write was accepted between the read and the restore) it reads once more.
//!    The state shows `restore-<run>`: an earlier try of this POST landed and its answer was
//!    lost, so the restore is done (exit 1, as in 3). Any other `write_id`, or none: that
//!    write stands, nothing is restored, and it exits 1 naming the state now and its write.
//!
//! Any other failure of the POST (a 503 on every try, a lost connection, a timeout) is
//! followed by the same read: a state showing `restore-<run>` is the restore done, whose
//! answer was lost. Otherwise, and on a 401 or 403 (which set nothing), it exits 1 naming the
//! version it could not set and the forced rollback.

use std::io::Write;
use std::time::Duration;

use clap::Args as ClapArgs;
use serde_json::{Value, json};

use super::{
    Client, Clock, DESIRED_PATH, Error as Deploy, KBF, Plan, Roots, TokenService, check_args,
    failed, fetch, log, pem_roots, plain,
};

/// What the calls of the restore are named in an error.
pub(super) const READ_BEFORE: &str = "read the farm before the restore";
pub(super) const SET_BACK: &str = "set the farm back";
pub(super) const READ_AFTER: &str = "read the farm after the restore was refused";

/// Where the operator goes when the restore cannot set the farm back.
pub const FORCED_ROLLBACK: &str = "the forced rollback (docs/design/gamma-cicd.md, section 4: the coordinator's `force`, C1, or `kbf-host-install activate` on each host, H1)";

/// The prefix of the restore's `write_id`.
pub const WRITE_ID_PREFIX: &str = "restore-";

/// The most bytes of a `write_id` the coordinator keeps (`MAX_WRITE_ID` of
/// `kbf_coordinator::writes`, which xtask does not depend on).
const MAX_WRITE_ID: usize = 128;

/// The flags of `deploy-farm restore`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The coordinator's control address, `https://<name>:<port>`. Empty: print a notice and
    /// set nothing.
    #[arg(long)]
    pub url: String,
    /// The version of the release the job deployed, as `deploy-farm --version` was given it.
    #[arg(long)]
    pub version: String,
    /// The service the job deployed (`kbf`, the default, or another the coordinator manages).
    #[arg(long, default_value = "kbf")]
    pub service: String,
    /// The farm CA certificate (PEM text), as for the deploy. Empty: the system store.
    #[arg(long, default_value = "", allow_hyphen_values = true)]
    pub ca_pem: String,
    /// The audience of the OIDC token (default: `--url` as given).
    #[arg(long)]
    pub audience: Option<String>,
    /// The job's run id and attempt, `<run id>-<attempt>`: the restore's write is
    /// `restore-<run>`, which tells it from any other write.
    #[arg(long)]
    pub run: String,
    /// Seconds between two tries of a call the coordinator answered 503.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub every_secs: u64,
}

/// Why the restore exits 1.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The farm was set back. The run stays red: the release failed its gamma stage.
    #[error(
        "the farm is set back to {version}, the version {release} replaced: {release} failed its gamma stage"
    )]
    Restored { version: String, release: String },
    /// The coordinator keeps no version this release replaced.
    #[error(
        "the coordinator keeps no version that {0} replaced (a first deploy): nothing was set back; to go back, use {FORCED_ROLLBACK}"
    )]
    NothingReplaced(String),
    /// The coordinator is older than the restore.
    #[error(
        "the coordinator shows no generation (it is older than the restore): nothing was set back; to go back, use {FORCED_ROLLBACK}"
    )]
    NoGeneration,
    /// Another write was accepted between the read and the restore, and stands.
    #[error(
        "another write was accepted before the restore and stands: the desired version is {desired} by {write}; nothing was set back"
    )]
    OtherWriter { desired: String, write: String },
    /// The POST, or the read after its 409, failed.
    #[error("could not set the farm back to {version}: {why}; to go back, use {FORCED_ROLLBACK}")]
    Failed { version: String, why: String },
    /// `--run` makes no `write_id` the coordinator keeps.
    #[error(
        "--run {0:?} is not <run id>-<attempt>: the restore's write id must be 1 to {MAX_WRITE_ID} visible characters"
    )]
    Run(String),
    /// A failure before anything was asked of the coordinator, or of its first read.
    #[error(transparent)]
    Deploy(#[from] Deploy),
}

/// The verb.
///
/// # Errors
/// [`crate::Error`], for every end but nothing to set back.
pub fn run(args: &Args) -> Result<Vec<String>, crate::Error> {
    check(args).map_err(|e| failed_restore(&e))?;
    if args.url.trim().is_empty() {
        return Ok(vec![
            "::notice::the restore step is off: the repository variable KBF_COORDINATOR_URL is not set"
                .to_owned(),
        ]);
    }
    let service = TokenService::from_env().map_err(failed)?;
    run_with(
        args,
        &service,
        &fetch::host_roots(),
        &mut std::io::stdout().lock(),
    )
}

/// The verb with the token service and the system roots given, on the real clock.
///
/// # Errors
/// [`crate::Error`], for every end but nothing to set back.
pub fn run_with(
    args: &Args,
    service: &TokenService,
    host: &[rustls::pki_types::CertificateDer<'static>],
    out: &mut impl Write,
) -> Result<Vec<String>, crate::Error> {
    // The restore never reads the time; it only waits between two tries of a call.
    let clock = Clock {
        now: Box::new(Duration::default),
        sleep: Box::new(std::thread::sleep),
    };
    let pinned;
    let roots = Roots {
        runner: host,
        coordinator: if args.ca_pem.trim().is_empty() {
            host
        } else {
            pinned = pem_roots(&args.ca_pem).map_err(failed)?;
            &pinned
        },
    };
    restore_on(args, service, &roots, &clock, out)
        .map(|line| vec![line])
        .map_err(|e| failed_restore(&e))
}

fn failed_restore(e: &Error) -> crate::Error {
    crate::Error::Failed(e.to_string())
}

/// The `write_id` of the restore of `run`.
#[must_use]
pub fn write_id(run: &str) -> String {
    format!("{WRITE_ID_PREFIX}{run}")
}

/// What `args` must satisfy before anything is sent: the deploy's rules for the service and
/// the version, and a `write_id` the coordinator keeps (1 to 128 visible characters).
fn check(args: &Args) -> Result<(), Error> {
    let deploy = super::Args {
        url: args.url.clone(),
        version: args.version.clone(),
        service: args.service.clone(),
        ca_pem: String::new(),
        audience: None,
        deadline_min: 1,
        stall_min: 1,
        every_secs: args.every_secs,
        min_up_storage: 0,
        min_up_frontend: 0,
        require_coordinator_current: false,
    };
    check_args(&deploy)?;
    let id = write_id(&args.run);
    if args.run.is_empty() || id.len() > MAX_WRITE_ID || !id.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Error::Run(args.run.clone()));
    }
    Ok(())
}

/// The plan the client of the restore is made from.
fn plan_of(args: &Args) -> Plan {
    let every = Duration::from_secs(args.every_secs);
    Plan {
        url: args.url.trim_end_matches('/').to_owned(),
        service: args.service.clone(),
        audience: args.audience.clone().unwrap_or_else(|| args.url.clone()),
        version: args.version.clone(),
        min_up_storage: 0,
        min_up_frontend: 0,
        require_coordinator_current: false,
        deadline: every,
        stall: every,
        every,
    }
}

/// What the restore reads of the convergence answer.
struct Read {
    desired: String,
    generation: Option<u64>,
    /// The version and minimums the last change of version replaced.
    replaced: Option<(String, Value)>,
    write_id: String,
}

impl Read {
    fn of(answer: &Value) -> Result<Self, Deploy> {
        let replaced = match &answer["replaced"] {
            Value::Null => None,
            r => Some((
                r["version"]
                    .as_str()
                    .map(plain)
                    .ok_or_else(|| Deploy::Answer("no version in replaced".to_owned()))?,
                r["min_up"].clone(),
            )),
        };
        Ok(Self {
            desired: answer["desired"]
                .as_str()
                .map_or_else(|| "(none)".to_owned(), plain),
            generation: answer["generation"].as_u64(),
            replaced,
            write_id: answer["write_id"].as_str().map(plain).unwrap_or_default(),
        })
    }

    /// The write that made the state, as the error names it.
    fn write(&self) -> String {
        if self.write_id.is_empty() {
            "a write without an id".to_owned()
        } else {
            format!("the write {}", self.write_id)
        }
    }
}

/// The restore on a clock it is given: the line to print when there was nothing to set back.
///
/// # Errors
/// [`Error`]: the farm was set back, or could not be.
pub fn restore_on(
    args: &Args,
    service: &TokenService,
    roots: &Roots<'_>,
    clock: &Clock<'_>,
    out: &mut impl Write,
) -> Result<String, Error> {
    check(args)?;
    let plan = plan_of(args);
    let client = Client::new(&plan, service, roots, clock)?;
    let path = plan.convergence_path();
    let read = Read::of(&client.send_retrying(out, READ_BEFORE, &path, None, &plan)?)?;
    if read.desired != plan.version {
        return Ok(format!(
            "nothing to set back: the desired version is {}, not {} (another writer, a newer release or the coordinator's own rollback has it now)",
            read.desired, plan.version
        ));
    }
    let Some(generation) = read.generation else {
        return Err(Error::NoGeneration);
    };
    let Some((version, min_up)) = read.replaced else {
        return Err(Error::NothingReplaced(plan.version));
    };
    let id = write_id(&args.run);
    let mut body = json!({
        "version": version,
        "min_up": min_up,
        "if_generation": generation,
        "write_id": id,
    });
    if plan.service != KBF {
        body["service"] = json!(plan.service);
    }
    let restored = || Error::Restored {
        version: version.clone(),
        release: plan.version.clone(),
    };
    let not_set = |e: Deploy| Error::Failed {
        version: version.clone(),
        why: e.to_string(),
    };
    match client.send_retrying(out, SET_BACK, DESIRED_PATH, Some(&body), &plan) {
        Ok(_) => {
            log(
                out,
                &format!(
                    "set back: desired version {version}, min_up {min_up}, on generation {generation}, as {id}"
                ),
            )?;
            Err(restored())
        }
        // A refusal of the token set nothing: no read can change that.
        Err(e @ Deploy::Refused { .. }) => Err(not_set(e)),
        // Some write was accepted, or the POST may have landed with its answer lost (a 503 on
        // every try, a dropped connection, a timeout): the state's `write_id` tells.
        Err(e) => {
            let conflict = matches!(e, Deploy::Conflict { .. });
            let now = client
                .send_retrying(out, READ_AFTER, &path, None, &plan)
                .and_then(|answer| Read::of(&answer))
                .map_err(not_set)?;
            if now.write_id == id {
                log(
                    out,
                    &format!(
                        "set back: desired version {}, by this restore ({id}), whose answer was lost before",
                        now.desired
                    ),
                )?;
                Err(restored())
            } else if conflict {
                Err(Error::OtherWriter {
                    write: now.write(),
                    desired: now.desired,
                })
            } else {
                Err(not_set(e))
            }
        }
    }
}
