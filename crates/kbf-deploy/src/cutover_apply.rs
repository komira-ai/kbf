//! `kbf-deploy job-service apply` and `back-out`: the switch of one frontend's executor onto
//! the job service, and its way back, each a dry run without `--apply`.
//!
//! `apply --apply`, in order, each step printed before it is made:
//!
//! 1. `check`: one `ListJobs` with kbf's client token ([`crate::job_service`]), so a service
//!    that cannot be reached or refuses the token stops the switch before anything changes.
//! 2. The new `kbf.toml` is staged beside the file with a byte-exact backup
//!    ([`crate::cutover_edit`]), and `kbf --mode=frontend --config <staged> --check` must
//!    pass on it. A failure here removes both files: nothing has changed.
//! 3. The staged file is renamed over the file. From here a failure says how to back out.
//! 4. The restart, through the update path a roll takes for a frontend: the frontend's own
//!    drain (`kbf cluster drain --frontend`, `POST /v1/frontend/drain`), a wait until it has
//!    exited (its operator API refuses the connection), however long its open executions
//!    run unless `--drain-deadline-secs` is given, then `restart frontend=<instance>` to the
//!    host's `kbf-host-install` over ssh with the updater's key (the forced command; the same
//!    request and argument vector `kbf-updater` sends), then a wait for `GET /readyz` 200.
//! 5. A wait for the first virtual worker, `GET /v1/nodes/js:<frontend id>-0` 200: the
//!    executor of the job service is the one running.
//! 6. The canary ([`crate::canary`]): one job through the job service, checked to succeed.
//!
//! `back-out --apply` checks the backup with `kbf --check`, renames it over the file, and
//! restarts the same way (steps 4). Neither verb has a `--force`, and no flag skips a step.
//!
//! The waits ask once per `--poll-secs`; the pause between asks is injected ([`Host`]), so
//! a test reads no clock and never sleeps. A deadline is a number of asks.

use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Output};
use std::time::Duration;

use clap::Args;
use kbf_host::exec::Exec;
use kbf_host::path::HostPath;
use kbf_host::step::{Cmd, HttpUrl};
use tokio::runtime::Runtime;

use crate::canary::{self, Canary};
use crate::cutover::{
    CutoverError, Executor, JOB_SERVICE_EXECUTOR, Readiness, Settings, Target, Value, print_plan,
    read_config,
};
use crate::cutover_edit::{self, Staged};
use crate::job_service::{self, CheckArgs};

/// What the cutover asks of the host: run a tool, ask an HTTP status, pause between asks.
pub(crate) trait Host {
    fn run(&mut self, cmd: &Cmd) -> io::Result<Output>;
    fn status(&mut self, url: &HttpUrl) -> io::Result<u16>;
    fn pause(&mut self, secs: u64);
}

/// The real host: tools and HTTP through `exec`, and a pause that sleeps.
pub(crate) struct OnHost<'a, E: Exec> {
    pub(crate) exec: &'a E,
}

impl<E: Exec> Host for OnHost<'_, E> {
    fn run(&mut self, cmd: &Cmd) -> io::Result<Output> {
        self.exec.query(cmd)
    }

    fn status(&mut self, url: &HttpUrl) -> io::Result<u16> {
        self.exec.http_status(url)
    }

    fn pause(&mut self, secs: u64) {
        // Nothing else runs on the cutover's runtime between two asks.
        std::thread::sleep(Duration::from_secs(secs));
    }
}

/// How the frontend is restarted: the flags `apply` and `back-out` share.
#[derive(Args, Clone, Debug)]
pub struct Restart {
    /// The frontend's unit as the host's `kbf-host-install` names it: `-` for
    /// `kbf-frontend.service`, NAME for `kbf-frontend@NAME.service`.
    #[arg(long, value_name = "NAME")]
    pub(crate) instance: String,
    /// The frontend's operator API (its `--api-listen`), on loopback.
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:8982")]
    pub(crate) api: SocketAddr,
    /// The operator API's token file, for the drain.
    #[arg(long, value_name = "FILE", value_parser = HostPath::new)]
    pub(crate) api_token_file: HostPath,
    /// The address of the frontend's host in the updater's inventory.
    #[arg(long, value_name = "ADDRESS")]
    pub(crate) ssh_host: String,
    /// The ssh port of that host, when it is not 22.
    #[arg(long, value_name = "PORT")]
    pub(crate) ssh_port: Option<u16>,
    /// The user the updater's key logs in as.
    #[arg(long, value_name = "USER")]
    pub(crate) ssh_user: String,
    /// The updater's key, the one the forced command of `kbf-host-install` is set up for.
    #[arg(long, value_name = "FILE", value_parser = HostPath::new)]
    pub(crate) ssh_key_file: HostPath,
    /// A known-hosts file holding the host's key, the only key that is trusted.
    #[arg(long, value_name = "FILE", value_parser = HostPath::new)]
    pub(crate) known_hosts: HostPath,
    /// The `kbf` binary, for `cluster drain` and `--check`.
    #[arg(long, value_name = "FILE", default_value = "/usr/bin/kbf", value_parser = HostPath::new)]
    pub(crate) kbf: HostPath,
    /// The `ssh` binary.
    #[arg(long, value_name = "FILE", default_value = "/usr/bin/ssh", value_parser = HostPath::new)]
    pub(crate) ssh: HostPath,
    /// Seconds between two asks of a wait.
    #[arg(long, value_name = "SECS", default_value_t = 5,
          value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) poll_secs: u64,
    /// How long to wait for the drained frontend to exit. Not given: however long its open
    /// executions run; none is cut.
    #[arg(long, value_name = "SECS", value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) drain_deadline_secs: Option<u64>,
    /// How long to wait for the restarted frontend to be ready, and for its first worker.
    #[arg(long, value_name = "SECS", default_value_t = 300,
          value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) ready_deadline_secs: u64,
}

/// The arguments of `job-service apply`.
#[derive(Args, Clone, Debug)]
pub struct ApplyArgs {
    #[command(flatten)]
    pub(crate) target: Target,
    #[command(flatten)]
    pub(crate) restart: Restart,
    /// How long to wait for the canary job to end, after which it is cancelled.
    #[arg(long, value_name = "SECS", default_value_t = 600,
          value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) canary_deadline_secs: u64,
    /// Carry the plan out.
    #[arg(long)]
    pub(crate) apply: bool,
}

/// The arguments of `job-service back-out`.
#[derive(Args, Clone, Debug)]
pub struct BackOutArgs {
    /// The frontend's `kbf.toml`, beside which `apply` kept the backup.
    #[arg(long, value_name = "FILE")]
    pub(crate) config: PathBuf,
    #[command(flatten)]
    pub(crate) restart: Restart,
    /// Carry the plan out.
    #[arg(long)]
    pub(crate) apply: bool,
}

/// How many asks a wait of `secs` makes, one per `poll` seconds, at least one.
pub(crate) fn asks(secs: u64, poll: u64) -> u64 {
    secs.div_ceil(poll).max(1)
}

/// Whether `name` is one `kbf-host-install` takes in `restart frontend=<name>`: `-`, or
/// `[a-z][a-z0-9-]{0,31}`.
fn instance_ok(name: &str) -> bool {
    let mut bytes = name.bytes();
    name == "-"
        || (name.len() <= 32
            && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
            && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
}

/// Whether `user` is an ssh user the updater takes: `[a-z_][a-z0-9_-]{0,31}`.
fn user_ok(user: &str) -> bool {
    let mut bytes = user.bytes();
    user.len() <= 32
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Whether `host` is an address ssh takes as one argument, not as an option.
fn address_ok(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with('-')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".:-[]".contains(&b))
}

/// One step of a verb, printed before it is made.
#[derive(Clone, Debug)]
enum Step {
    /// `ListJobs` with kbf's client token.
    Check(CheckArgs),
    /// Stage the new file and its backup.
    Stage {
        file: PathBuf,
        new: String,
        old: String,
    },
    /// `kbf --mode=frontend --config <file> --check`.
    CheckConfig(Cmd),
    /// Rename the staged file over the file.
    PutInPlace,
    /// Rename the backup over the file.
    Restore { file: PathBuf, backup: PathBuf },
    /// `kbf cluster drain --frontend`.
    Drain(Cmd),
    /// Wait until the operator API refuses the connection.
    AwaitExit { url: HttpUrl, asks: Option<u64> },
    /// The forced command `restart frontend=<instance>`, over ssh.
    Restart(Cmd),
    /// Wait for `url` to answer 200.
    Await {
        url: HttpUrl,
        asks: u64,
        what: &'static str,
    },
    /// The canary job.
    Canary { canary: Canary, asks: u64 },
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Step::Check(a) => write!(
                f,
                "check: ListJobs on {} with the token in {}",
                a.url,
                a.token_file.display()
            ),
            Step::Stage { file, .. } => write!(
                f,
                "stage {} and keep the original as {}",
                cutover_edit::beside(file, cutover_edit::STAGED_SUFFIX).display(),
                cutover_edit::beside(file, cutover_edit::BACKUP_SUFFIX).display()
            ),
            Step::CheckConfig(cmd) | Step::Drain(cmd) => write!(f, "{cmd}"),
            Step::PutInPlace => f.write_str("rename the staged file over the config"),
            Step::Restore { file, backup } => {
                write!(f, "rename {} over {}", backup.display(), file.display())
            }
            Step::AwaitExit { url, asks: None } => {
                write!(
                    f,
                    "wait until {url} refuses the connection (the frontend has exited), however long"
                )
            }
            Step::AwaitExit { url, asks: Some(n) } => {
                write!(
                    f,
                    "wait until {url} refuses the connection (the frontend has exited), at most {n} asks"
                )
            }
            Step::Restart(cmd) => write!(f, "{cmd}"),
            Step::Await { url, asks, what } => {
                write!(
                    f,
                    "wait for {url} to answer 200 ({what}), at most {asks} asks"
                )
            }
            Step::Canary { canary, asks } => write!(
                f,
                "canary: submit job {} ({} --version, no network, label {}={}) and ask after it at most {asks} times",
                canary.id,
                canary.image,
                canary::LABEL,
                canary.frontend
            ),
        }
    }
}

/// The restart's steps, from the drain to the frontend being ready.
fn restart_steps(r: &Restart) -> Result<Vec<Step>, CutoverError> {
    let refuse = |m: &str| CutoverError::Refused(m.to_owned());
    if !instance_ok(&r.instance) {
        return Err(refuse(
            "--instance: `-`, or 1..=32 of a-z, 0-9 and -, starting with a letter",
        ));
    }
    if !user_ok(&r.ssh_user) {
        return Err(refuse(
            "--ssh-user: 1..=32 of a-z, 0-9, _ and -, not starting with - or a digit",
        ));
    }
    if !address_ok(&r.ssh_host) {
        return Err(refuse(
            "--ssh-host: a host name or address, not starting with -",
        ));
    }
    if !r.api.ip().is_loopback() {
        return Err(refuse(
            "--api: the operator API answers a drain from loopback only",
        ));
    }
    let url = |path: &str| api_url(r.api, path);
    let drain = Cmd::new(r.kbf.clone())
        .arg("cluster")
        .arg("--api")
        .arg(r.api.to_string())
        .arg("drain")
        .arg("--frontend")
        .arg("--token-file")
        .arg(r.api_token_file.to_string());
    let mut ssh = Cmd::new(r.ssh.clone());
    for arg in [
        "-F",
        "/dev/null",
        "-i",
        r.ssh_key_file.as_str(),
        "-o",
        "BatchMode=yes",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "StrictHostKeyChecking=yes",
    ] {
        ssh = ssh.arg(arg);
    }
    ssh = ssh
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", r.known_hosts))
        .arg("-o")
        .arg("GlobalKnownHostsFile=/dev/null")
        .arg("-o")
        .arg("ConnectTimeout=10");
    if let Some(port) = r.ssh_port {
        ssh = ssh.arg("-p").arg(port.to_string());
    }
    ssh = ssh
        .arg("-T")
        .arg("--")
        .arg(format!("{}@{}", r.ssh_user, r.ssh_host))
        .arg(format!("restart frontend={}", r.instance))
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/")
        .env("LANG", "C");
    Ok(vec![
        Step::Drain(drain),
        Step::AwaitExit {
            url: url("/healthz")?,
            asks: r.drain_deadline_secs.map(|s| asks(s, r.poll_secs)),
        },
        Step::Restart(ssh),
        Step::Await {
            url: url("/readyz")?,
            asks: asks(r.ready_deadline_secs, r.poll_secs),
            what: "the frontend is ready",
        },
    ])
}

/// `http://<api><path>`, a URL of the operator API.
fn api_url(api: SocketAddr, path: &str) -> Result<HttpUrl, CutoverError> {
    HttpUrl::new(&format!("http://{api}{path}"))
        .map_err(|e| CutoverError::Refused(format!("--api: {e}")))
}

/// `kbf --mode=frontend --config <file> --check`.
fn check_config(r: &Restart, file: &Path) -> Result<Cmd, CutoverError> {
    let text = file.to_str().ok_or_else(|| {
        CutoverError::Refused(format!("--config: {} is not UTF-8", file.display()))
    })?;
    Ok(Cmd::new(r.kbf.clone())
        .arg("--mode=frontend")
        .arg("--config")
        .arg(text)
        .arg("--check"))
}

/// The first line of `bytes`, for a message.
fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_owned()
}

/// What a tool's run must have been: exit 0.
fn ran(host: &mut dyn Host, cmd: &Cmd, what: &str) -> Result<Output, CutoverError> {
    let output = host
        .run(cmd)
        .map_err(|e| CutoverError::Failed(format!("{what}: cannot run {}: {e}", cmd.program)))?;
    if output.status.success() {
        return Ok(output);
    }
    let code = output
        .status
        .code()
        .map_or_else(|| "a signal".to_owned(), |c| c.to_string());
    let said = first_line(&output.stderr);
    let said = if said.is_empty() {
        first_line(&output.stdout)
    } else {
        said
    };
    Err(CutoverError::Failed(format!("{what}: exit {code}: {said}")))
}

/// What the forced command answered: `restarted`, or why not.
fn restarted(output: &Output) -> Result<(), CutoverError> {
    let answer: Option<serde_json::Value> = serde_json::from_str(&first_line(&output.stdout)).ok();
    let field = |k: &str| {
        answer
            .as_ref()
            .and_then(|a| a.get(k))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned()
    };
    if output.status.success() && field("result") == "restarted" {
        return Ok(());
    }
    let code = output
        .status
        .code()
        .map_or_else(|| "a signal".to_owned(), |c| c.to_string());
    let reason = match (field("result"), field("reason")) {
        (r, why) if !r.is_empty() && !why.is_empty() => format!("{r}: {why}"),
        (r, _) if !r.is_empty() => r,
        _ => "no answer of kbf-host-install (ssh's own failure is exit 255)".to_owned(),
    };
    Err(CutoverError::Failed(format!(
        "the restart through kbf-host-install: exit {code}: {reason}"
    )))
}

/// Whether an ask of the operator API says the frontend has exited: only a refused
/// connection does; another failure of the ask (a timeout) is a frontend still there.
fn exited(answer: &io::Result<u16>) -> bool {
    matches!(answer, Err(e) if e.kind() == io::ErrorKind::ConnectionRefused)
}

/// The state of a verb while it carries its steps out.
struct Carry<'a> {
    host: &'a mut dyn Host,
    poll_secs: u64,
    staged: Option<Staged>,
}

impl Carry<'_> {
    /// Ask `url` until `done` says so, at most `asks` times (`None`: until it does).
    fn wait(
        &mut self,
        url: &HttpUrl,
        asks: Option<u64>,
        done: fn(&io::Result<u16>) -> bool,
    ) -> bool {
        let mut ask = 0u64;
        loop {
            if done(&self.host.status(url)) {
                return true;
            }
            ask += 1;
            if asks.is_some_and(|n| ask >= n) {
                return false;
            }
            self.host.pause(self.poll_secs);
        }
    }

    async fn step(&mut self, step: &Step, out: &mut dyn Write) -> Result<(), CutoverError> {
        match step {
            Step::Check(a) => job_service::check(a, out)
                .await
                .map_err(|e| CutoverError::Failed(format!("check: {e}"))),
            Step::Stage { file, new, old } => {
                self.staged = Some(Staged::write(file, old, new)?);
                Ok(())
            }
            Step::CheckConfig(cmd) => ran(self.host, cmd, "the config check").map(drop),
            Step::PutInPlace => match self.staged.take() {
                Some(staged) => staged.put_in_place(),
                None => Err(CutoverError::Failed("nothing is staged".to_owned())),
            },
            Step::Restore { file, backup } => cutover_edit::restore(file, backup),
            Step::Drain(cmd) => ran(self.host, cmd, "the drain").map(drop),
            Step::AwaitExit { url, asks } => {
                if self.wait(url, *asks, exited) {
                    Ok(())
                } else {
                    Err(CutoverError::Failed(
                        "the frontend has not exited by the drain deadline; it is left \
                         draining, and nothing was restarted"
                            .to_owned(),
                    ))
                }
            }
            Step::Restart(cmd) => {
                let output = self.host.run(cmd).map_err(|e| {
                    CutoverError::Failed(format!("the restart: cannot run {}: {e}", cmd.program))
                })?;
                restarted(&output)
            }
            Step::Await { url, asks, what } => {
                if self.wait(url, Some(*asks), |r| matches!(r, Ok(200))) {
                    Ok(())
                } else {
                    Err(CutoverError::Failed(format!(
                        "{url} did not answer 200 in {asks} asks: not {what}"
                    )))
                }
            }
            Step::Canary { .. } => Err(CutoverError::Failed(
                "the canary needs the job service's client".to_owned(),
            )),
        }
    }
}

/// The dry run's last line.
const DRY_RUN: &str = "dry run: nothing changed. Re-run with --apply.";

/// Print `steps` as a dry run, or carry them out, each printed first. The canary step runs
/// with `client`.
async fn carry(
    steps: &[Step],
    apply: bool,
    host: &mut dyn Host,
    poll_secs: u64,
    client: Option<&kbf_job_client::Client>,
    out: &mut dyn Write,
) -> Result<(), CutoverError> {
    if !apply {
        for step in steps {
            let _ = writeln!(out, "DRY-RUN: {step}");
        }
        return Ok(());
    }
    let mut c = Carry {
        host,
        poll_secs,
        staged: None,
    };
    for step in steps {
        let _ = writeln!(out, "+ {step}");
        match (step, client) {
            (Step::Canary { canary, asks }, Some(client)) => {
                let pause_secs = c.poll_secs;
                let host = &mut *c.host;
                let line = canary::run(client, canary, *asks, &mut || host.pause(pause_secs))
                    .await
                    .map_err(CutoverError::Failed)?;
                let _ = writeln!(out, "{line}");
            }
            _ => c.step(step, out).await?,
        }
    }
    Ok(())
}

/// The back-out line printed after a switch: the command and the flags it returns to.
fn print_back_out(config: &Path, table: &Executor, out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "back-out: kbf-deploy job-service back-out --config {} with the same restart flags \
         (restores {} and restarts the same way); it returns to:",
        config.display(),
        cutover_edit::beside(config, cutover_edit::BACKUP_SUFFIX).display()
    );
    for flag in table.back_out_flags() {
        let _ = writeln!(out, "  {flag}");
    }
}

/// The canary of a frontend whose `[executor]` table is `table`, named by `now_secs`.
fn canary_of(table: &Executor, now_secs: u64) -> Canary {
    let text = |key: &str| match table.value(key) {
        Value::Text(s) => s,
        _ => String::new(),
    };
    let kbf_image = text("executor-kbf-image");
    let arch = text("executor-arch");
    Canary {
        id: format!("kbf-canary-{now_secs}"),
        image: kbf_image
            .strip_prefix("docker://")
            .unwrap_or(&kbf_image)
            .to_owned(),
        arch: if arch.is_empty() {
            "x86_64".to_owned()
        } else {
            arch
        },
        frontend: text("executor-frontend-id"),
    }
}

/// `apply`, with the instant `now_secs` naming the canary job.
pub(crate) async fn apply(
    a: &ApplyArgs,
    host: &mut dyn Host,
    now_secs: u64,
    out: &mut dyn Write,
) -> Result<(), CutoverError> {
    let settings = Settings::of(&a.target)?;
    let restart = restart_steps(&a.restart)?;
    let file = cutover_edit::canonical(&a.target.config)?;
    let old = read_config(&file)?;
    let table = Executor::parse(&file, &old)?;
    let ready = Readiness::of(&table);
    if !ready.missing.is_empty() {
        return Err(CutoverError::Refused(format!(
            "[executor] in {} lacks {}; `job-service plan` says what to add",
            file.display(),
            ready.missing.join("; ")
        )));
    }
    if table.mode() == JOB_SERVICE_EXECUTOR {
        return Err(CutoverError::Refused(format!(
            "{} already runs executor = \"{JOB_SERVICE_EXECUTOR}\": nothing to switch",
            file.display()
        )));
    }
    let new = cutover_edit::rewritten(&file, &old, &settings)?;
    cutover_edit::require_no_backup(&file)?;
    let canary = canary_of(&table, now_secs);
    let frontend = canary.frontend.clone();
    let check = CheckArgs::of(&a.target);
    let staged = cutover_edit::beside(&file, cutover_edit::STAGED_SUFFIX);
    let workers = api_url(a.restart.api, &format!("/v1/nodes/js:{frontend}-0"))?;
    let mut steps = vec![
        Step::Check(check.clone()),
        Step::Stage {
            file: file.clone(),
            new,
            old,
        },
        Step::CheckConfig(check_config(&a.restart, &staged)?),
        Step::PutInPlace,
    ];
    let switched = steps.len();
    steps.extend(restart);
    steps.push(Step::Await {
        url: workers,
        asks: asks(a.restart.ready_deadline_secs, a.restart.poll_secs),
        what: "the executor of the job service registered its first worker",
    });
    steps.push(Step::Canary {
        canary,
        asks: asks(a.canary_deadline_secs, a.restart.poll_secs),
    });
    let mode = if a.apply { "apply" } else { "dry run" };
    let _ = writeln!(out, "kbf-deploy job-service apply ({mode})");
    print_plan(&file, &table, &settings, &ready, out);
    let client = if a.apply {
        Some(job_service::client(&check).map_err(|e| CutoverError::Refused(e.to_string()))?)
    } else {
        None
    };
    let (before, after) = steps.split_at(switched);
    carry(
        before,
        a.apply,
        host,
        a.restart.poll_secs,
        client.as_ref(),
        out,
    )
    .await?;
    let done = carry(
        after,
        a.apply,
        host,
        a.restart.poll_secs,
        client.as_ref(),
        out,
    )
    .await;
    if let Err(e) = done {
        let _ = writeln!(
            out,
            "the switch is in place in {}, but a step after it failed",
            file.display()
        );
        print_back_out(&file, &table, out);
        return Err(e);
    }
    if a.apply {
        let _ = writeln!(
            out,
            "done: the frontend runs executor = \"{JOB_SERVICE_EXECUTOR}\""
        );
    } else {
        let _ = writeln!(out, "{DRY_RUN}");
    }
    print_back_out(&file, &table, out);
    Ok(())
}

/// `back-out`.
pub(crate) async fn back_out(
    a: &BackOutArgs,
    host: &mut dyn Host,
    out: &mut dyn Write,
) -> Result<(), CutoverError> {
    let restart = restart_steps(&a.restart)?;
    let file = cutover_edit::canonical(&a.config)?;
    let now = Executor::parse(&file, &read_config(&file)?)?;
    let (backup, text) = cutover_edit::read_backup(&file)?;
    let then = Executor::parse(&backup, &text)?;
    let mut steps = vec![
        Step::CheckConfig(check_config(&a.restart, &backup)?),
        Step::Restore {
            file: file.clone(),
            backup: backup.clone(),
        },
    ];
    steps.extend(restart);
    let mode = if a.apply { "apply" } else { "dry run" };
    let _ = writeln!(out, "kbf-deploy job-service back-out ({mode})");
    let _ = writeln!(
        out,
        "config: {} back over {}",
        backup.display(),
        file.display()
    );
    let _ = writeln!(out, "  executor now: {}, then: {}", now.mode(), then.mode());
    let _ = writeln!(out, "the flags it returns to:");
    for flag in then.back_out_flags() {
        let _ = writeln!(out, "  {flag}");
    }
    carry(&steps, a.apply, host, a.restart.poll_secs, None, out).await?;
    if a.apply {
        let _ = writeln!(
            out,
            "done: the frontend runs executor = \"{}\" again",
            then.mode()
        );
    } else {
        let _ = writeln!(out, "{DRY_RUN}");
    }
    Ok(())
}

/// The verb that runs on the cutover's runtime.
pub(crate) enum Verb<'a> {
    Apply(&'a ApplyArgs, u64),
    BackOut(&'a BackOutArgs),
}

/// Run `verb` on `runtime`, as an exit code.
pub(crate) fn run_on(
    runtime: io::Result<Runtime>,
    verb: Verb<'_>,
    host: &mut dyn Host,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let done = match runtime {
        Ok(runtime) => runtime.block_on(async {
            match verb {
                Verb::Apply(a, now_secs) => apply(a, host, now_secs, out).await,
                Verb::BackOut(a) => back_out(a, host, out).await,
            }
        }),
        Err(e) => Err(CutoverError::Failed(format!("no runtime: {e}"))),
    };
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => e.report(err),
    }
}

#[cfg(test)]
#[path = "cutover_apply_tests.rs"]
mod tests;
