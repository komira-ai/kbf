//! `cargo xtask deploy-farm`: after a release is published, tell the farm's update
//! coordinator "desired = this release" and wait until the farm converges
//! (`docs/deploy/kbf-updater.md`, "Turn on the deploy step").
//!
//! `--service NAME` (default `kbf`) names the service the deploy is about. For `kbf` the
//! requests are exactly those below. For another service the POST body is the version and
//! `"service": NAME` (the `min_up` keys are kbf's; that service's minimums are its definition's),
//! the coordinator's commit is not compared with the version, and every convergence read is `GET /v1/convergence?service=NAME`, so
//! the wait is on that service's own hosts and desired state. `--version` is then that
//! service's own version text, as its hosts report it: the coordinator compares the text, so
//! only `kbf`'s form (`<semver>+g<commit12>`) is checked here.
//!
//! The steps, each over HTTPS (the runner's token service as the system store trusts it, the
//! coordinator as the system store does, or, with `--ca-pem`, as that CA alone does: the
//! coordinator's certificate is the farm CA's, which no system store holds):
//!
//! 1. With an empty `--url` the verb prints a notice and exits 0: a fork, or a repository
//!    whose operator has not set the coordinator's address, deploys nothing and fails nothing.
//! 2. `GET /v1/convergence` once, before anything is changed: the hosts live then (at the
//!    desired version, updating or failed; not `down`) are the baseline, per role, and are
//!    printed. Its `commit` (the build the coordinator runs) is compared with the release's;
//!    with `--require-coordinator-current` a different one ends the verb before anything is
//!    set, a different one without the flag is a `::warning` annotation and the deploy goes
//!    on, and a coordinator that reports none is noted and trusted.
//! 3. `POST /v1/desired` with the version and the minimums, authenticated by a GitHub
//!    Actions OIDC token whose audience is the coordinator (`--audience`, by default the URL).
//! 4. `GET /v1/convergence` every `--every-secs`, with a fresh token each time (a token is
//!    good for one call and for ten minutes; a deploy waits for an hour). Each answer is
//!    printed as a table.
//!
//! It ends with exit 0 when the coordinator says `converged` and every role has at least as
//! many hosts at the version as it had live at the baseline (the coordinator's `converged`
//! answers for the hosts it hears from, so a host the release crashed would not stop it); a
//! host that was `down` before the deploy is not in the baseline, and one that joins during
//! it only adds. It ends with 1 when a role has fewer, when the coordinator says `blocked`
//! (a host failed its update to this version), when the counts have not changed for
//! `--stall-min` minutes, when `--deadline-min` minutes are up, or when a call is refused
//! (401, 403), when the coordinator's desired version is no longer this release's (another
//! writer replaced it, and a convergence on that version is not this deploy's), or the job
//! has no OIDC token to ask for. The coordinator does not keep the time
//! of the last change; the stall is the client's to judge. A poll that fails for any other
//! reason is reported and tried again, and counts as no change.
//!
//! The OIDC token is asked of the runner's token service (`ACTIONS_ID_TOKEN_REQUEST_URL` and
//! `ACTIONS_ID_TOKEN_REQUEST_TOKEN`, which the platform sets for a job with `id-token:
//! write`) and never printed: it is masked in the log as soon as it is read.

use std::io::Write;
use std::time::{Duration, Instant};

use clap::Args as ClapArgs;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use serde_json::{Value, json};

use crate::fetch;

/// The longest any one call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The most bytes of any answer read.
const MAX_ANSWER: u64 = 1 << 20;

/// How many times the POST is tried when the call fails for a reason that may pass.
const POST_ATTEMPTS: u32 = 4;

/// Whether a failed call may pass: the coordinator's 503 on the POST or on the baseline read,
/// and nothing else (a certificate it is not trusted by, or a runner without a token, does
/// not get better; a poll that fails is tried again anyway).
fn retryable(what: &str, why: &str) -> bool {
    matches!(what, SET_DESIRED | READ_BASELINE) && why == "status 503"
}

/// What the calls are named in an error.
const SET_DESIRED: &str = "set the desired version";
const READ_BASELINE: &str = "read the farm before the deploy";

/// Whether `version` is `<semver>+g<12 lower-case hex digits>`: what a host's heartbeat reports
/// and what its installer needs to find the commit (`<major>.<minor>.<patch>`, an optional
/// `-<pre-release>`, then `+g` and exactly 12 digits of `0-9a-f`, and nothing after).
///
/// # Errors
/// [`Error::Version`] when it is not.
pub fn check_version(version: &str) -> Result<(), Error> {
    let bad = || Error::Version(version.to_owned());
    let (semver, build) = version.rsplit_once('+').ok_or_else(bad)?;
    let digits = build.strip_prefix('g').ok_or_else(bad)?;
    let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    if digits.len() != 12 || !digits.bytes().all(hex) {
        return Err(bad());
    }
    let (core, pre) = match semver.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (semver, None),
    };
    let number = |n: &str| {
        !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && (n == "0" || !n.starts_with('0'))
    };
    let parts: Vec<&str> = core.split('.').collect();
    let pre_ok = pre.is_none_or(|p| {
        !p.is_empty()
            && p.split('.')
                .all(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    });
    if parts.len() == 3 && parts.iter().all(|n| number(n)) && pre_ok {
        Ok(())
    } else {
        Err(bad())
    }
}

/// The path of the desired state, and of the convergence read.
const DESIRED_PATH: &str = "/v1/desired";
const CONVERGENCE_PATH: &str = "/v1/convergence";

/// The service the coordinator runs of its own: the default of `--service`.
const KBF: &str = "kbf";

/// The most bytes of a service name, as the coordinator's wire says (`MAX_SERVICE_NAME` of
/// `kbf_updater::wire`, which xtask does not depend on).
const MAX_SERVICE_NAME: usize = 64;

/// Whether `name` is a service name the coordinator accepts: lowercase letters, digits and
/// `-`, 1 to [`MAX_SERVICE_NAME`] bytes (`kbf_updater::wire::is_service_name`).
fn service_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SERVICE_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// What `args` must satisfy before anything is sent: a service name the coordinator accepts,
/// and for `kbf` a version of the form its hosts report.
fn check_args(args: &Args) -> Result<(), Error> {
    if !service_name_ok(&args.service) {
        return Err(Error::Service(args.service.clone()));
    }
    if args.service == KBF {
        check_version(&args.version)?;
    }
    Ok(())
}

/// The variables the platform sets for a job that may ask for an OIDC token.
const TOKEN_URL_VAR: &str = "ACTIONS_ID_TOKEN_REQUEST_URL";
const TOKEN_KEY_VAR: &str = "ACTIONS_ID_TOKEN_REQUEST_TOKEN";

/// The flags of the verb.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The coordinator's control address, `https://<name>:<port>`. Empty: print a notice and
    /// deploy nothing.
    #[arg(long)]
    pub url: String,
    /// The release's version text, as the hosts' heartbeats report it once they run it:
    /// `<semver>+g<12 lower-case hex digits of the commit>`, e.g. `0.1.0+g0123456789ab`. The
    /// release tag (`0.0.13`) is not that text and is refused.
    #[arg(long)]
    pub version: String,
    /// The service to deploy: `kbf` (the default) or another service the coordinator manages.
    /// For another service `--version` is that service's own version text, as its hosts
    /// report it.
    #[arg(long, default_value = "kbf")]
    pub service: String,
    /// The farm CA certificate (PEM text) the coordinator's certificate must chain to, instead
    /// of the system store's roots. Empty: the system store.
    #[arg(long, default_value = "", allow_hyphen_values = true)]
    pub ca_pem: String,
    /// The audience of the OIDC token (default: `--url` as given).
    #[arg(long)]
    pub audience: Option<String>,
    /// Minutes to wait for the farm to converge before failing.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=1440))]
    pub deadline_min: u64,
    /// Minutes the counts may stay as they are before failing.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..=1440))]
    pub stall_min: u64,
    /// Seconds between two reads.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub every_secs: u64,
    /// The storage hosts that must stay up while others update. `kbf` only: another service's
    /// minimums are in its definition, and none is sent.
    #[arg(long, default_value_t = 1)]
    pub min_up_storage: u32,
    /// The frontends that must stay up while others update. `kbf` only, as above.
    #[arg(long, default_value_t = 1)]
    pub min_up_frontend: u32,
    /// Fail before anything is changed when the coordinator runs another commit than the
    /// release's. Use it for a release that changes the coordinator or the wire between it
    /// and the hosts: the coordinator must be updated first
    /// (`docs/deploy/kbf-updater.md`, "Updating the coordinator").
    ///
    /// Bare, or `true`, turns it on; `false` or empty (an unset workflow variable) leaves it off.
    #[arg(
        long,
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = parse_switch
    )]
    pub require_coordinator_current: bool,
}

/// A switch given as text: `true` is on; `false` and the empty text are off.
fn parse_switch(text: &str) -> Result<bool, String> {
    match text {
        "true" => Ok(true),
        "" | "false" => Ok(false),
        other => Err(format!("expected `true` or `false`, got `{other}`")),
    }
}

/// Why the deploy did not end converged.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error(
        "{TOKEN_URL_VAR} or {TOKEN_KEY_VAR} is not set: the job needs `permissions: id-token: write`"
    )]
    NoTokenService,
    #[error("--service {0:?} is not a service name: 1 to 64 lowercase letters, digits and -")]
    Service(String),
    #[error("--ca-pem holds no certificate")]
    Ca,
    #[error(
        "--version {0:?} is not the form the hosts report: it must be <semver>+g<12 lower-case hex digits of the commit>, e.g. 0.1.0+g0123456789ab (the release tag is not that form; `release farm-version` derives it)"
    )]
    Version(String),
    #[error("the URL must be https://<host>[:port] with no path, got {0:?}")]
    Url(String),
    #[error("{what}: {why}")]
    Call { what: &'static str, why: String },
    #[error(
        "{what}: refused with status {status}: the coordinator's OIDC allow-list does not accept this job's token"
    )]
    Refused { what: &'static str, status: u16 },
    #[error("the coordinator answered something this client cannot read: {0}")]
    Answer(String),
    #[error("blocked: a host failed its update to {0}; the table above says which role")]
    Blocked(String),
    #[error(
        "halted: {host} reported a failure no version can repair ({check}) while updating to {version}; an operator must clear the halt (`clear_halt` on /v1/operator) before anything is ordered again"
    )]
    Halted {
        version: String,
        host: String,
        check: String,
    },
    #[error(
        "the coordinator's desired version is {0:?}, not this release's: another writer replaced it"
    )]
    Superseded(String),
    #[error(
        "{role}: {expected} hosts were live before the deploy, {reached} run {version}: {} went down",
        .expected - .reached
    )]
    Lost {
        role: String,
        expected: u64,
        reached: u64,
        version: String,
    },
    #[error(
        "the coordinator runs {coordinator}, the release is {release}: update the coordinator first (docs/deploy/kbf-updater.md, Updating the coordinator)"
    )]
    CoordinatorBehind {
        coordinator: String,
        release: String,
    },
    #[error("the counts have not changed for {0} seconds")]
    Stalled(u64),
    #[error("the farm has not converged after {0} seconds")]
    Deadline(u64),
    #[error("write the log: {0}")]
    Output(String),
}

/// How the deploy waits.
#[derive(Clone, Debug)]
pub struct Plan {
    pub url: String,
    /// The service the deploy is about; `kbf` for the farm's own.
    pub service: String,
    pub audience: String,
    pub version: String,
    pub min_up_storage: u32,
    pub min_up_frontend: u32,
    pub require_coordinator_current: bool,
    pub deadline: Duration,
    pub stall: Duration,
    pub every: Duration,
}

impl Plan {
    /// The plan of `args`.
    #[must_use]
    pub fn of(args: &Args) -> Self {
        Self {
            url: args.url.trim_end_matches('/').to_owned(),
            service: args.service.clone(),
            audience: args.audience.clone().unwrap_or_else(|| args.url.clone()),
            version: args.version.clone(),
            min_up_storage: args.min_up_storage,
            min_up_frontend: args.min_up_frontend,
            require_coordinator_current: args.require_coordinator_current,
            deadline: Duration::from_secs(args.deadline_min * 60),
            stall: Duration::from_secs(args.stall_min * 60),
            every: Duration::from_secs(args.every_secs),
        }
    }
}

impl Plan {
    /// The path of the convergence read: the coordinator's own for `kbf`, else asking for
    /// the service by name (a name needs no escaping: [`service_name_ok`]).
    fn convergence_path(&self) -> String {
        if self.service == KBF {
            CONVERGENCE_PATH.to_owned()
        } else {
            format!("{CONVERGENCE_PATH}?service={}", self.service)
        }
    }
}

/// The time the wait reads and spends: `now` is the time since some fixed start, `sleep`
/// waits for the duration it is given. A test passes a virtual clock whose `sleep` moves `now`.
pub struct Clock<'a> {
    pub now: Box<dyn Fn() -> Duration + 'a>,
    pub sleep: Box<dyn Fn(Duration) + 'a>,
}

/// The runner's token service.
#[derive(Clone, Debug)]
pub struct TokenService {
    pub url: String,
    pub key: String,
}

impl TokenService {
    /// The service the platform set for this job.
    ///
    /// # Errors
    /// [`Error::NoTokenService`] when either variable is missing or empty.
    pub fn from_env() -> Result<Self, Error> {
        let var = |name| std::env::var(name).ok().filter(|v| !v.is_empty());
        match (var(TOKEN_URL_VAR), var(TOKEN_KEY_VAR)) {
            (Some(url), Some(key)) => Ok(Self { url, key }),
            _ => Err(Error::NoTokenService),
        }
    }
}

/// The verb: lines for stdout when it ends converged.
///
/// # Errors
/// [`crate::Error`], for every end but convergence.
pub fn run(args: &Args) -> Result<Vec<String>, crate::Error> {
    // Before the notice for an unset URL: a version the hosts could never report is a defect
    // of the caller whether or not a farm is there to be told.
    check_args(args).map_err(failed)?;
    if args.url.trim().is_empty() {
        return Ok(vec![
            "::notice::the deploy step is off: the repository variable KBF_COORDINATOR_URL is not set"
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

/// The verb for `args` with the token service and the system roots given: the coordinator is
/// trusted by `--ca-pem` when it is not empty, else by `host`.
///
/// # Errors
/// [`crate::Error`], for every end but convergence.
pub fn run_with(
    args: &Args,
    service: &TokenService,
    host: &[CertificateDer<'static>],
    out: &mut impl Write,
) -> Result<Vec<String>, crate::Error> {
    check_args(args).map_err(failed)?;
    let plan = Plan::of(args);
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
    deploy(&plan, service, &roots, out).map_err(failed)?;
    Ok(vec![format!(
        "ok: every live host runs {} ({})",
        plan.version, plan.url
    )])
}

/// The certificates in `pem`.
///
/// # Errors
/// [`Error::Ca`] when there is none, or the text is not PEM.
pub fn pem_roots(pem: &str) -> Result<Vec<CertificateDer<'static>>, Error> {
    let certs: Result<Vec<_>, _> = CertificateDer::pem_slice_iter(pem.as_bytes()).collect();
    certs.ok().filter(|c| !c.is_empty()).ok_or(Error::Ca)
}

/// Whom each side of the deploy is trusted by.
#[derive(Clone, Copy, Debug)]
pub struct Roots<'a> {
    /// The runner's token service.
    pub runner: &'a [CertificateDer<'static>],
    /// The coordinator.
    pub coordinator: &'a [CertificateDer<'static>],
}

fn failed(e: Error) -> crate::Error {
    crate::Error::Failed(e.to_string())
}

/// Sets `plan.version` as desired and waits, writing each table to `out`.
///
/// # Errors
/// [`Error`]: anything but `converged`.
pub fn deploy(
    plan: &Plan,
    service: &TokenService,
    roots: &Roots<'_>,
    out: &mut impl Write,
) -> Result<(), Error> {
    let start = Instant::now();
    let clock = Clock {
        now: Box::new(|| start.elapsed()),
        sleep: Box::new(std::thread::sleep),
    };
    deploy_on(plan, service, roots, &clock, out)
}

/// [`deploy`] on a clock it is given.
///
/// # Errors
/// [`Error`]: anything but `converged`.
pub fn deploy_on(
    plan: &Plan,
    service: &TokenService,
    roots: &Roots<'_>,
    clock: &Clock<'_>,
    out: &mut impl Write,
) -> Result<(), Error> {
    if !plan.url.starts_with("https://") || plan.url[8..].contains('/') {
        return Err(Error::Url(plan.url.clone()));
    }
    let agent = |roots: &[CertificateDer<'static>]| -> ureq::Agent {
        ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(CALL_TIMEOUT))
            .tls_config(fetch::tls_config(roots))
            .build()
            .into()
    };
    let client = Client {
        runner: agent(roots.runner),
        coordinator: agent(roots.coordinator),
        plan,
        service,
        clock,
    };
    let read_path = plan.convergence_path();
    let first = Read::of(&client.send_retrying(out, READ_BASELINE, &read_path, None, plan)?)?;
    if plan.service == KBF {
        check_coordinator(plan, first.commit.as_deref(), out)?;
    } else {
        log(
            out,
            &format!(
                "not checking the coordinator's commit against {}: its version is not a kbf commit",
                plan.service
            ),
        )?;
    }
    let baseline = first.live();
    log(out, &baseline_line(&baseline))?;
    // The `min_up` keys are kbf's contract with CI; any other service's minimums are its
    // definition's, so it is sent none (the coordinator would drop the keys its roles lack).
    let body = if plan.service == KBF {
        json!({
            "version": plan.version,
            "min_up": {"storage": plan.min_up_storage, "frontend": plan.min_up_frontend},
        })
    } else {
        json!({"version": plan.version, "service": plan.service})
    };
    client.send_retrying(out, SET_DESIRED, DESIRED_PATH, Some(&body), plan)?;
    log(out, &format!("desired version {} set", plan.version))?;

    let started = (clock.now)();
    let mut changed = started;
    let mut last: Option<(String, Value)> = None;
    loop {
        match client.send(out, "read the convergence", &read_path, None) {
            Ok(answer) => {
                let read = Read::of(&answer)?;
                log(out, &read.table())?;
                if read.desired != plan.version {
                    return Err(Error::Superseded(read.desired));
                }
                match read.state.as_str() {
                    "converged" => return read.holds(&baseline, &plan.version),
                    "blocked" => return Err(Error::Blocked(plan.version.clone())),
                    "halted" => return Err(read.halted(&plan.version)),
                    _ => {}
                }
                let now = (read.state, answer["roles"].clone());
                if last.as_ref() != Some(&now) {
                    last = Some(now);
                    changed = (clock.now)();
                }
            }
            Err(e @ Error::Refused { .. }) => return Err(e),
            Err(e) => log(out, &format!("the read failed, trying again: {e}"))?,
        }
        let elapsed = (clock.now)().saturating_sub(started);
        if elapsed >= plan.deadline {
            return Err(Error::Deadline(plan.deadline.as_secs()));
        }
        if (clock.now)().saturating_sub(changed) >= plan.stall {
            return Err(Error::Stalled(plan.stall.as_secs()));
        }
        (clock.sleep)(plan.every.min(plan.deadline - elapsed));
    }
}

/// Whether the 12 characters are a commit as `--version` shows it.
fn is_commit(text: &str) -> bool {
    text.len() == 12
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The coordinator's own commit against the release's, before anything is set. A coordinator
/// that reports none (an older build, or one built without a git checkout) is noted and
/// trusted. With `plan.require_coordinator_current`, a different commit is an error.
fn check_coordinator(
    plan: &Plan,
    reported: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), Error> {
    let release = plan.version.rsplit("+g").next().unwrap_or_default();
    let Some(coordinator) = reported.filter(|c| is_commit(c)) else {
        let line = "::notice::the coordinator reports no build commit (an older build): not checking that it is current";
        return if plan.require_coordinator_current {
            log(out, line)
        } else {
            Ok(())
        };
    };
    if coordinator == release {
        return log(
            out,
            &format!("the coordinator runs the release's commit {release}"),
        );
    }
    if plan.require_coordinator_current {
        return Err(Error::CoordinatorBehind {
            coordinator: coordinator.to_owned(),
            release: release.to_owned(),
        });
    }
    log(
        out,
        &format!(
            "::warning title=coordinator not current::the coordinator runs {coordinator}, the release is {release}: update the coordinator first (docs/deploy/kbf-updater.md, Updating the coordinator)"
        ),
    )
}

/// One line of the log, flushed.
fn log(out: &mut dyn Write, text: &str) -> Result<(), Error> {
    writeln!(out, "{text}")
        .and_then(|()| out.flush())
        .map_err(|e| Error::Output(e.to_string()))
}

struct Client<'a> {
    runner: ureq::Agent,
    coordinator: ureq::Agent,
    plan: &'a Plan,
    service: &'a TokenService,
    clock: &'a Clock<'a>,
}

impl Client<'_> {
    /// [`Client::send`], tried again (up to `POST_ATTEMPTS` calls) when the failure may pass:
    /// a 503 means the coordinator could not reach the token issuer, which is no verdict.
    fn send_retrying(
        &self,
        out: &mut dyn Write,
        what: &'static str,
        path: &str,
        body: Option<&Value>,
        plan: &Plan,
    ) -> Result<Value, Error> {
        let mut attempt = 1;
        loop {
            match self.send(out, what, path, body) {
                Err(Error::Call { what, why })
                    if attempt < POST_ATTEMPTS && retryable(what, &why) =>
                {
                    log(
                        out,
                        &format!("the call failed, trying again: {what}: {why}"),
                    )?;
                    attempt += 1;
                    (self.clock.sleep)(plan.every);
                }
                other => return other,
            }
        }
    }

    /// A fresh OIDC token for the plan's audience; masked in the log.
    fn token(&self, out: &mut dyn Write) -> Result<String, Error> {
        let call = |why: String| Error::Call {
            what: "ask the runner for an OIDC token",
            why,
        };
        let mut url = self.service.url.clone();
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str("audience=");
        url.push_str(&percent_encode(&self.plan.audience));
        let mut response = self
            .runner
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.service.key))
            .call()
            .map_err(|e| call(e.to_string()))?;
        let status = response.status().as_u16();
        let text = answer_text(&mut response).map_err(call)?;
        if status != 200 {
            return Err(call(format!("status {status}")));
        }
        let value: Value = serde_json::from_str(&text).map_err(|e| call(e.to_string()))?;
        let token = value["value"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| call("the answer has no token".to_owned()))?;
        // Masked in the log before anything else can print it.
        writeln!(out, "::add-mask::{token}").map_err(|e| Error::Output(e.to_string()))?;
        Ok(token.to_owned())
    }

    /// One call to the coordinator with a fresh token: a POST of `body`, or a GET.
    fn send(
        &self,
        out: &mut dyn Write,
        what: &'static str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, Error> {
        let call = |why: String| Error::Call { what, why };
        let bearer = format!("Bearer {}", self.token(out)?);
        let url = format!("{}{path}", self.plan.url);
        let mut response = match body {
            Some(body) => self
                .coordinator
                .post(&url)
                .header("Authorization", bearer)
                .content_type("application/json")
                .send(body.to_string().as_str()),
            None => self
                .coordinator
                .get(&url)
                .header("Authorization", bearer)
                .call(),
        }
        .map_err(|e| call(e.to_string()))?;
        let status = response.status().as_u16();
        if matches!(status, 401 | 403) {
            return Err(Error::Refused { what, status });
        }
        let text = answer_text(&mut response).map_err(call)?;
        if status != 200 {
            return Err(call(format!("status {status}")));
        }
        serde_json::from_str(&text).map_err(|e| call(e.to_string()))
    }
}

fn answer_text(response: &mut ureq::http::Response<ureq::Body>) -> Result<String, String> {
    response
        .body_mut()
        .with_config()
        .limit(MAX_ANSWER)
        .read_to_string()
        .map_err(|e| e.to_string())
}

/// `text` with everything but unreserved characters as `%XX`.
fn percent_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `text` as one short plain line: control characters dropped, no leading `::` (the runner
/// reads a line that starts with it as a workflow command), at most 64 characters.
fn plain(text: &str) -> String {
    let line: String = text.chars().filter(|c| !c.is_control()).take(64).collect();
    line.trim_start_matches(':').to_owned()
}

/// The baseline as one log line.
fn baseline_line(baseline: &[(String, u64)]) -> String {
    let roles: Vec<String> = baseline.iter().map(|(r, n)| format!("{r} {n}")).collect();
    format!(
        "baseline: hosts live before the deploy: {}",
        if roles.is_empty() {
            "none".to_owned()
        } else {
            roles.join(", ")
        }
    )
}

/// What the coordinator said, as far as the table needs it.
struct Read {
    state: String,
    desired: String,
    quiet: bool,
    roles: Vec<(String, [u64; 4])>,
    /// The commit the coordinator says it was built from; none from an older build.
    commit: Option<String>,
    /// The host and the check of the halt the coordinator names, when the state is `halted`.
    halt: Option<(String, String)>,
}

impl Read {
    fn of(answer: &Value) -> Result<Self, Error> {
        let bad = |what: &str| Error::Answer(format!("no {what}"));
        let text = |key: &str| answer[key].as_str().map(str::to_owned);
        let mut roles = Vec::new();
        for row in answer["roles"].as_array().ok_or_else(|| bad("roles"))? {
            let count = |key: &str| row[key].as_u64().ok_or_else(|| bad(key));
            roles.push((
                plain(row["role"].as_str().ok_or_else(|| bad("role"))?),
                [
                    count("at_desired")?,
                    count("updating")?,
                    count("failed")?,
                    count("down")?,
                ],
            ));
        }
        Ok(Self {
            state: text("state")
                .map(|t| plain(&t))
                .ok_or_else(|| bad("state"))?,
            desired: text("desired").map_or_else(|| "(none)".to_owned(), |t| plain(&t)),
            quiet: answer["quiet"].as_bool().unwrap_or(false),
            roles,
            commit: text("commit").map(|t| plain(&t)),
            halt: answer["halt"]["node_id"]
                .as_str()
                .zip(answer["halt"]["check"].as_str())
                .map(|(host, check)| (plain(host), plain(check))),
        })
    }

    /// The failure of a deploy whose coordinator says `halted`, naming the host and the check
    /// when it names them.
    fn halted(&self, version: &str) -> Error {
        let (host, check) = self
            .halt
            .clone()
            .unwrap_or_else(|| ("unknown".to_owned(), "unknown".to_owned()));
        Error::Halted {
            version: version.to_owned(),
            host,
            check,
        }
    }

    /// The hosts live now (not `down`) per role.
    fn live(&self) -> Vec<(String, u64)> {
        let live = |[at, updating, failed, _]: &[u64; 4]| at + updating + failed;
        self.roles
            .iter()
            .map(|(role, counts)| (role.clone(), live(counts)))
            .collect()
    }

    /// Whether every role of the baseline has as many hosts at the version as it had live.
    fn holds(&self, baseline: &[(String, u64)], version: &str) -> Result<(), Error> {
        for (role, expected) in baseline {
            let reached = self
                .roles
                .iter()
                .find(|(r, _)| r == role)
                .map_or(0, |(_, [at, ..])| *at);
            if reached < *expected {
                return Err(Error::Lost {
                    role: role.clone(),
                    expected: *expected,
                    reached,
                    version: version.to_owned(),
                });
            }
        }
        Ok(())
    }

    fn table(&self) -> String {
        let mut out = format!(
            "farm: {}, desired {}{}\n{:<12}{:>11}{:>10}{:>8}{:>6}",
            self.state,
            self.desired,
            if self.quiet {
                " (coordinator just started)"
            } else {
                ""
            },
            "role",
            "at_desired",
            "updating",
            "failed",
            "down"
        );
        for (role, [at, updating, failed, down]) in &self.roles {
            out.push_str(&format!(
                "\n{role:<12}{at:>11}{updating:>10}{failed:>8}{down:>6}"
            ));
        }
        out
    }
}
