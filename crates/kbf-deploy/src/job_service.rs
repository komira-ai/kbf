//! `kbf-deploy job-service check`: whether the job service (jobscheduler's
//! `jobscheduler.v1.JobService`, the port of `js-lease-server --job-listen`) answers kbf's
//! client token, before a frontend's executor is pointed at it
//! (`docs/deploy/job-service-cutover.md`). It only reads.
//!
//! The verb makes one `ListJobs` call with `kbf-job-client`, the client the executor will use,
//! so it checks the same origin rule, token file rule and CA pinning the executor will meet.
//! An answer, empty or not, is a pass: a farm with no job yet is a working one. Anything else
//! fails, and the message says which of three things it was: the service could not be reached
//! (the address and the connect failure), the service refused the token (`unauthorized`), or
//! the service answered some other error.
//!
//! The output names the token file by its path and never holds the token: the token is read by
//! `kbf_job_client::Token::from_file`, which keeps it in a value that prints as `Token(..)`,
//! and no message here is made from it.
//!
//! Exit 0 the service answered; 1 it was unreachable, refused the token, or answered an error
//! (`FAIL: ...` on stderr); 2 refused: a bad `--url`, token file or CA file (`REFUSED: ...` on
//! stderr).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Subcommand};
use js_proto::v1::ListJobsRequest;
use kbf_job_client::{Client, Config, ConfigError, Error, Origin, Token, Unsent};
use tokio::runtime::Runtime;
use tonic::Code;

/// The verbs of `kbf-deploy job-service`.
#[derive(Subcommand)]
pub enum JobService {
    /// Call ListJobs on the job service with kbf's client token: passes when the service
    /// answers, fails naming the address when it cannot be reached, and fails as unauthorized
    /// when it refuses the token. Only reads.
    Check(CheckArgs),
    /// Print the egress lines of a farm agent's flags file (`/etc/js-agent/flags`) from the
    /// frontend's `kbf.toml`: the allow-list ceiling and one `--egress-allow` for the tier
    /// store's proxy, the read listener and, with `executor-locate`, the store a task reads
    /// from. Only reads.
    AgentEgress(crate::agent_egress::AgentEgressArgs),
    /// Print what switching the frontend's executor onto the job service sets in the
    /// `[executor]` table of its `kbf.toml`: each key now and then, the executor flags, and
    /// the flags of the mode it runs now (the back-out). Changes nothing.
    Plan(crate::cutover::Target),
    /// Switch the frontend's executor onto the job service: check the service, rewrite the
    /// `[executor]` table (the original kept beside it), drain and restart the frontend
    /// through kbf-host-install, wait for its first virtual worker, and run one canary job.
    /// A dry run without --apply.
    Apply(Box<crate::cutover_apply::ApplyArgs>),
    /// Put back the `kbf.toml` that `apply` kept, and drain and restart the frontend the
    /// same way. A dry run without --apply.
    BackOut(crate::cutover_apply::BackOutArgs),
}

/// The arguments of `kbf-deploy job-service check`.
#[derive(Args, Clone, Debug)]
pub struct CheckArgs {
    /// The job service: `https://host:port`, or `http://` on loopback only (the port of the
    /// lease server's `--job-listen`).
    #[arg(long, value_name = "URL")]
    pub(crate) url: String,
    /// The file holding kbf's client token of the job service (mode 0600 or 0400).
    #[arg(long, value_name = "FILE")]
    pub(crate) token_file: PathBuf,
    /// The CA certificate (PEM file) the service's certificate must chain to: needed for
    /// `https`, refused for `http`.
    #[arg(long, value_name = "FILE")]
    pub(crate) ca_file: Option<PathBuf>,
}

impl CheckArgs {
    /// The check of the job service a cutover points the executor at.
    pub(crate) fn of(t: &crate::cutover::Target) -> CheckArgs {
        CheckArgs {
            url: t.url.clone(),
            token_file: t.token_file.clone(),
            ca_file: t.ca_file.clone(),
        }
    }
}

/// Why the check did not pass.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CheckError {
    /// Bad arguments, token file or CA file: exit 2.
    #[error("{0}")]
    Refused(String),
    #[error("unreachable: cannot connect to the job service at {url}: {cause}")]
    Unreachable { url: String, cause: String },
    #[error("unauthorized: the job service at {url} refused the token in {token_file} ({answer})")]
    Unauthorized {
        url: String,
        token_file: String,
        /// The status code, then `: ` and its message when it has one: the job service's
        /// bearer layer refuses a token with no message.
        answer: String,
    },
    #[error("the job service at {url} did not answer ListJobs: {cause}")]
    Failed { url: String, cause: String },
}

impl CheckError {
    fn code(&self) -> u8 {
        if matches!(self, Self::Refused(_)) {
            2
        } else {
            1
        }
    }
}

/// The client config of `a`: the origin, the token from its file and the CA from its file.
/// Every message names the flag; the token file's names the path, never its content.
fn config(a: &CheckArgs) -> Result<Config, CheckError> {
    let refuse = CheckError::Refused;
    let origin: Origin = a
        .url
        .parse()
        .map_err(|e: ConfigError| refuse(format!("--url: {e}")))?;
    let token = Token::from_file(&a.token_file).map_err(|e| {
        refuse(match e {
            ConfigError::TokenFile { .. } | ConfigError::TokenFileMode { .. } => {
                format!("--token-file: {e}")
            }
            other => format!(
                "--token-file: token file {}: {other}",
                a.token_file.display()
            ),
        })
    })?;
    let config = Config::new(origin, token);
    match &a.ca_file {
        None => Ok(config),
        Some(path) => {
            let pem = std::fs::read(path)
                .map_err(|e| refuse(format!("--ca-file {}: {e}", path.display())))?;
            Ok(config.with_ca_pem(pem))
        }
    }
}

/// What the check makes of the answer to `ListJobs`.
fn judge(answer: Result<usize, Error>, url: &str, token_file: &Path) -> Result<usize, CheckError> {
    let url = url.to_owned();
    match answer {
        Ok(jobs) => Ok(jobs),
        Err(Error::Unreachable(Unsent::Connect(status))) => Err(CheckError::Unreachable {
            url,
            cause: status.message().to_owned(),
        }),
        Err(Error::Status {
            code: code @ (Code::Unauthenticated | Code::PermissionDenied),
            message,
        }) => Err(CheckError::Unauthorized {
            url,
            token_file: token_file.display().to_string(),
            answer: if message.is_empty() {
                format!("{code:?}")
            } else {
                format!("{code:?}: {message}")
            },
        }),
        Err(other) => Err(CheckError::Failed {
            url,
            cause: other.to_string(),
        }),
    }
}

/// The client of `a`'s job service, inside a Tokio runtime (the client's channel starts its
/// task on it).
pub(crate) fn client(a: &CheckArgs) -> Result<Client, CheckError> {
    Client::new(config(a)?).map_err(|e| {
        CheckError::Refused(match e {
            ConfigError::MissingCa => "--url is https: give --ca-file".to_owned(),
            ConfigError::UnusedCa => "--ca-file is for an https --url".to_owned(),
            other => format!("--ca-file: {other}"),
        })
    })
}

/// The check, inside a Tokio runtime.
pub(crate) async fn check(a: &CheckArgs, out: &mut dyn Write) -> Result<(), CheckError> {
    let client = client(a)?;
    // A reader that has gone (a closed pipe) is not the service's failure.
    let _ = writeln!(out, "job service: {}", a.url);
    let _ = writeln!(out, "token file: {}", a.token_file.display());
    // One job is enough to know the service answers; the page size keeps the answer small on
    // a busy farm.
    let filter = ListJobsRequest {
        page_size: 1,
        ..ListJobsRequest::default()
    };
    let answer = client.list_jobs(filter).await.map(|page| page.jobs.len());
    let jobs = judge(answer, &a.url, &a.token_file)?;
    let seen = if jobs == 0 {
        "no job of this client yet"
    } else {
        "jobs of this client listed"
    };
    let _ = writeln!(out, "pass: the job service answered ListJobs ({seen})");
    Ok(())
}

/// The check on `runtime`, as an exit code: `FAIL: ...` or `REFUSED: ...` on `err`.
pub(crate) fn run_on(
    runtime: io::Result<Runtime>,
    a: &CheckArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let done = match runtime {
        Ok(runtime) => runtime.block_on(check(a, out)),
        Err(e) => Err(CheckError::Failed {
            url: a.url.clone(),
            cause: format!("no runtime to call it from: {e}"),
        }),
    };
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let word = if e.code() == 2 { "REFUSED" } else { "FAIL" };
            let _ = writeln!(err, "{word}: {e}");
            ExitCode::from(e.code())
        }
    }
}

/// A current-thread Tokio runtime with its I/O and timers.
fn runtime() -> io::Result<Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

/// Run a `job-service` verb. `host` runs the tools and HTTP asks of `apply` and `back-out`;
/// `now_secs` names the canary job.
pub(crate) fn run(
    verb: &JobService,
    host: &mut dyn crate::cutover_apply::Host,
    now_secs: u64,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    use crate::cutover_apply::{Verb, run_on as run_cutover};
    match verb {
        JobService::Check(a) => run_on(runtime(), a, out, err),
        JobService::AgentEgress(a) => crate::agent_egress::run(a, out, err),
        JobService::Plan(t) => crate::cutover::run_plan(t, out, err),
        JobService::Apply(a) => run_cutover(runtime(), Verb::Apply(a, now_secs), host, out, err),
        JobService::BackOut(a) => run_cutover(runtime(), Verb::BackOut(a), host, out, err),
    }
}

#[cfg(test)]
#[path = "job_service_tests.rs"]
mod tests;
