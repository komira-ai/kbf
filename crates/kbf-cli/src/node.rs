//! The start of `storage`, `daemon` and `storage-and-daemon` when it is given no mode flags:
//! `kbf --mode=<mode> [--config FILE]`, or `kbf` alone with `mode = "..."` in the file
//! (`docs/design/install-and-enrollment.md`, section 4.4). There is no `node` word and no
//! mode that is "a node": the host's config states what it is.
//!
//! 1. Reads `kbf.toml` (`--config`) and resolves the mode: `--mode`, else the file's
//!    `mode`, else exit 2 naming both. A flag that disagrees with the file wins and the
//!    start says so.
//! 2. Enrolls the node if it holds no `node.json`, for exactly the roles the mode needs
//!    ([`Want`]), exactly as [`crate::enroll`] says; a node that is enrolled does not read
//!    its token file again, and one enrolled for roles that do not cover the mode is
//!    refused (exit 2). A failure that may pass (exit 1: the frontend is down, busy, the
//!    clock is off) is retried here with a backoff ([`retry_wait`]); one a restart cannot
//!    fix exits 2.
//! 3. Builds the mode's command line from `kbf.toml`, the configuration the frontend sent
//!    and the certificate files, parses it with the mode's own parser
//!    ([`crate::parse_from`], the same one the units use, so a flag
//!    the mode would refuse is refused here the same way) and runs the mode in this
//!    process.
//! 4. While the mode runs, renews the certificates at half their life
//!    ([`Renewer`], `kbf_enroll::renew`), retrying every 5 minutes doubling to an hour,
//!    and writes the new files in place. A change of roles or endpoints in a renewal's
//!    answer is stored and applied at the next start; the start never restarts itself.
//!
//! `--check` does steps 1 and the checks of 2 and 3 that need no network, starts nothing
//! and enrolls nothing: exit 0 for a host that is configured and not yet enrolled (valid
//! `kbf.toml`, a token file, no `node.json`) and for an enrolled one whose files cover the
//! mode; exit 2 for a config that is not right, a token file that is missing or open, or an
//! enrolled `node.json` that does not cover the mode.
//!
//! **What the certificate files reach.** A storage node's listener reads its certificate
//! again every `NodeConfig.tls_reload_interval_s` (`kbf-tls`), and a daemon's connections
//! to the frontend and to the storage nodes read theirs when they are made
//! (`kbf-daemon`), so a renewal needs no restart. The daemon's one connection that does
//! not follow the files is the transfer engine over `--cas` (`kbf-node`), which a daemon
//! uses only until the frontend sends it a map.
//!
//! **The object store.** A storage node is given no store and no key: it is started with
//! `--cold-store registered`, so the store is the one the frontend has registered for the
//! farm (`kbf cluster object-store`), whatever it was when the node enrolled
//! (`NodeConfig.cold_store` is not read). The storage half waits for a registration, fetches
//! the endpoint, bucket, prefix and keys itself after it asks, probes the keys and holds
//! them in memory only (`ColdStore.GetStoreConfig` on the worker listener,
//! `kbf_server::cold_apply`, `kbf_server::join`; `kbf_enroll::keys` is not called from
//! here). A farm with no store registered leaves the node waiting for one, and a registration
//! that changes reaches it at its next start.
//!
//! Exit codes are [`crate::enroll`]'s until the mode starts, and the mode's own after.

use std::ffi::{OsStr, OsString};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use kbf_enroll::client::{ConnectError, connect};
use kbf_enroll::config::{Config, FrontendUrl, Trust};
use kbf_enroll::renew::{Backoff, RenewError, first_attempt, near_expiry, renew};
use kbf_enroll::store::{Node as Enrolled, Store, StoreError, StoredConfig};
use kbf_enroll::token;
use kbf_proto::kbf::enroll::v1::CredentialMode;
use kbf_types::Role;
use std::path::{Path, PathBuf};
use tokio::sync::watch;

use crate::enroll::{Env, Failure, Loaded, Want, anchor, enrolled, ensure_enrolled, load, report};
use crate::{Invocation, names, parse_with, preflight};

/// The longest the renew loop sleeps at once, in seconds: a wall clock that jumped (a
/// machine that slept) is noticed within this.
const NAP_MAX_S: u64 = 3600;

/// A start that reads `kbf.toml`: the words `kbf --mode=<mode> --config FILE` and `--check`
/// mean ([`crate::parse_from`] makes it from a command line that carries no other flag of
/// a mode).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Start {
    /// The mode `--mode` (or the alias word) named; `None` reads it from the file.
    pub mode: Option<&'static str>,
    /// The node's config file.
    pub config: PathBuf,
    /// Check the host and exit, starting and enrolling nothing.
    pub check: bool,
}

/// How the daemon half runs its leases on this platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    /// Containers: Linux.
    Container,
    /// Processes in pool users: macOS.
    Native,
}

impl Driver {
    /// The driver of the machine this is built for.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub const fn host() -> Self {
        Self::Native
    }

    /// The driver of the machine this is built for.
    #[cfg(not(target_os = "macos"))]
    #[must_use]
    pub const fn host() -> Self {
        Self::Container
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::Native => "native",
        }
    }
}

/// `--name=value` as one argument.
fn flag(name: &str, value: impl AsRef<OsStr>) -> OsString {
    let mut out = OsString::from(format!("--{name}="));
    out.push(value.as_ref());
    out
}

fn missing(what: &str) -> Failure {
    Failure::Plan(format!(
        "the frontend's configuration for this node has no {what}; the node cannot start \
         until it is sent one"
    ))
}

/// The storage half's `--cold-store`: no store of its own, the one the frontend registers.
const COLD_STORE: &str = "registered";

/// The storage half's flags.
fn storage_flags(
    config: &Config,
    stored: &StoredConfig,
    store: &Store,
) -> Result<Vec<OsString>, Failure> {
    let port = u16::try_from(stored.storage_port)
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| missing("storage port"))?;
    let advertise = config.advertise.as_ref().map_or_else(
        || format!("{}:{port}", config.node_id),
        |a| format!("{}:{}", a.host, a.port),
    );
    let mut out = vec![
        flag("cold-store", COLD_STORE),
        flag("hot-store", format!("file://{}", config.hot_dir.display())),
        flag("meta-dir", &config.meta_dir),
        flag("storage-listen", format!("0.0.0.0:{port}")),
        flag("storage-tls-cert", store.cert_path(Role::Storage)),
        flag("storage-tls-key", store.key_path(Role::Storage)),
        flag("storage-client-ca", store.ca_path()),
        flag("frontend", &stored.worker_endpoint),
        flag("advertise-addr", advertise),
    ];
    if !config.zone.is_empty() {
        out.push(flag("storage-zone", &config.zone));
    }
    if stored.tls_reload_interval_s > 0 {
        out.push(flag(
            "tls-reload-interval-secs",
            stored.tls_reload_interval_s.to_string(),
        ));
    }
    Ok(out)
}

/// The daemon half's flags; `with_cas` for a daemon with no storage in its process.
fn daemon_flags(
    config: &Config,
    stored: &StoredConfig,
    store: &Store,
    driver: Driver,
    with_cas: bool,
) -> Vec<OsString> {
    let mut out = vec![
        flag("driver", driver.name()),
        flag("server", &stored.worker_endpoint),
    ];
    if with_cas {
        let cas = if stored.cas_endpoint.is_empty() {
            &stored.worker_endpoint
        } else {
            &stored.cas_endpoint
        };
        out.push(flag("cas", cas));
    }
    out.extend([
        flag("ca-cert", store.ca_path()),
        flag("cert", store.cert_path(Role::Daemon)),
        flag("key", store.key_path(Role::Daemon)),
        flag("node-id", config.node_id.as_str()),
        flag("data-dir", &config.data_dir),
    ]);
    if let Some(cpus) = config.reserve_cpus {
        out.push(flag("reserve-cpus", cpus.to_string()));
    }
    out
}

/// The command line of the mode the node runs, `kbf <mode> <flags>`.
///
/// # Errors
/// [`Failure::Plan`]: the frontend sent no worker endpoint, or (for storage) no storage
/// port.
pub fn plan(
    config: &Config,
    node: &Enrolled,
    store: &Store,
    driver: Driver,
    want: Want,
) -> Result<Vec<OsString>, Failure> {
    let (mode, roles) = (want.mode, want.roles);
    let stored = &node.config;
    if stored.worker_endpoint.is_empty() {
        return Err(missing("worker endpoint"));
    }
    let mut argv = vec![OsString::from("kbf"), flag("mode", mode)];
    if roles.contains(Role::Storage) {
        argv.extend(storage_flags(config, stored, store)?);
    }
    if roles.contains(Role::Daemon) {
        let with_cas = !roles.contains(Role::Storage);
        argv.extend(daemon_flags(config, stored, store, driver, with_cas));
    }
    Ok(argv)
}

/// Parses the mode's command line `argv` together with the settings of `kbf.toml`: the
/// flags of the tables, for those `argv` does not give (the enrollment's win).
fn parse_planned(argv: Vec<OsString>, path: &Path) -> Result<Invocation, clap::Error> {
    let argv = argv.into_iter().chain([flag("config", path)]);
    parse_with(argv, None).map(|parsed| parsed.invocation)
}

/// Why a renewal attempt failed; logged, never fatal.
#[derive(Debug, thiserror::Error)]
enum Attempt {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Connect(#[from] ConnectError),
    #[error(transparent)]
    Renew(#[from] RenewError),
}

/// Keeps a node's certificates fresh: when the next renewal is due, what it did last, and
/// how long to wait after a failure.
pub struct Renewer {
    url: FrontendUrl,
    trust: Trust,
    store: Store,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// Unix seconds: when the next attempt is due.
    due: i64,
    backoff: Backoff,
    /// The spread of the schedule, in thousandths (the first attempt's, kept).
    jitter: i32,
    /// The longest one sleep lasts.
    pub nap_max: Duration,
}

impl Renewer {
    /// A renewer for `node`, whose first attempt is due at the node's `renew_after`
    /// spread by `jitter_permille` (clamped to plus or minus 10%).
    #[must_use]
    pub fn new(loaded: &Loaded, node: &Enrolled, env: &Env, jitter_permille: i32) -> Self {
        Self {
            url: loaded.config.frontend.clone(),
            trust: loaded.config.trust,
            store: loaded.store.clone(),
            now: Arc::clone(&env.now),
            due: first_attempt((env.now)(), node.renew_after, jitter_permille),
            backoff: Backoff::new(),
            jitter: jitter_permille,
            nap_max: Duration::from_secs(NAP_MAX_S),
        }
    }

    /// One renewal: dial the frontend under the pin the node holds and send the request.
    async fn attempt(&self, now: i64) -> Result<Enrolled, Attempt> {
        let node = self.store.recover()?.ok_or(RenewError::NotEnrolled)?;
        let frontend = connect(&self.url, anchor(self.trust, node.pin()?)).await?;
        Ok(renew(&frontend, &self.store, now).await?)
    }

    /// Does what is due and returns the seconds until the next thing is: nothing before
    /// the renewal is due; a renewal when it is, after which the wait runs to the new
    /// `renew_after`; the next step of the backoff when it fails.
    pub async fn round(&mut self) -> i64 {
        let now = (self.now)();
        if now < self.due {
            return self.due - now;
        }
        match self.attempt(now).await {
            Ok(node) => {
                self.backoff.succeeded();
                self.due = first_attempt(now, node.renew_after, self.jitter);
                let (generation, not_after) = (node.config.generation, node.earliest_not_after());
                tracing::info!(generation, not_after, "certificates renewed");
            }
            Err(error) => {
                let wait = self.backoff.failed();
                self.due = now + wait;
                self.warn(&error, now);
            }
        }
        self.due - now
    }

    /// Logs a failed attempt; below a day to expiry it is an error.
    fn warn(&self, error: &Attempt, now: i64) {
        let expiring = self
            .store
            .load()
            .ok()
            .flatten()
            .filter(|node| near_expiry(node, now));
        match expiring {
            Some(node) => {
                let seconds_left = node.earliest_not_after() - now;
                tracing::error!(%error, seconds_left, "cannot renew; the certificates expire soon");
            }
            None => tracing::warn!(%error, "cannot renew the certificates; will retry"),
        }
    }

    /// Renews until `stop` changes (or its sender is dropped).
    pub async fn run(mut self, mut stop: watch::Receiver<bool>) {
        loop {
            let wait = tokio::select! {
                biased;
                _ = stop.changed() => return,
                wait = self.round() => wait,
            };
            let nap = nap(wait, self.nap_max);
            tokio::select! {
                biased;
                _ = stop.changed() => return,
                () = tokio::time::sleep(nap) => {}
            }
        }
    }
}

/// How long to sleep for a wait of `wait` seconds: at least one (a frontend that says "renew
/// now" must not make a loop that calls it as fast as it can answer), at most `max`.
fn nap(wait: i64, max: Duration) -> Duration {
    Duration::from_secs(u64::try_from(wait).unwrap_or(0).max(1)).min(max)
}

/// A random permille in -100..=100 for the renewal's jitter.
fn jitter() -> i32 {
    use std::hash::BuildHasher as _;
    let draw = std::collections::hash_map::RandomState::new().hash_one(0u8) % 201;
    i32::try_from(draw).map_or(0, |d| d - 100)
}

/// Whether the frontend wants the storage half's object-store keys fetched.
fn wants_keys(stored: &StoredConfig) -> bool {
    stored.credential_mode() == Some(CredentialMode::Keys)
        && stored.roles.iter().any(|r| r == Role::Storage.name())
}

/// How long a start waits before its `attempt`th retry (0 is the first): 5 seconds,
/// doubling, at most 5 minutes. A frontend that is down for a minute is found again at
/// once; one down for an hour is asked every 5 minutes, not in a loop.
#[must_use]
pub fn retry_wait(attempt: u32) -> Duration {
    Duration::from_secs(
        RETRY_FIRST_S
            .saturating_mul(2u64.saturating_pow(attempt))
            .min(RETRY_MAX_S),
    )
}

/// The first wait of [`retry_wait`], in seconds.
const RETRY_FIRST_S: u64 = 5;
/// The longest wait of [`retry_wait`], in seconds.
const RETRY_MAX_S: u64 = 300;

/// [`ensure_enrolled`], tried again after a failure that may pass (exit status 1) until it
/// succeeds, fails for good (exit status 2) or `env.pause` says to give up.
fn enroll_with_retry(
    loaded: &Loaded,
    want: Want,
    env: &Env,
) -> Result<crate::enroll::Outcome, Failure> {
    let mut attempt = 0;
    loop {
        let failure = match ensure_enrolled(loaded, want, env) {
            Err(failure) if failure.exit_code() == 1 => failure,
            done => return done,
        };
        let wait = retry_wait(attempt);
        attempt = attempt.saturating_add(1);
        eprintln!("kbf: {failure}; trying again in {} s", wait.as_secs());
        if !(env.pause)(wait) {
            return Err(failure);
        }
    }
}

/// The mode a start runs: `flag` (`--mode`), else the file's. When both are set and
/// differ the flag wins and the second value says so.
///
/// # Errors
/// [`Failure::NoMode`] when neither names one, [`Failure::BadMode`] for a file that
/// names something that is not a mode, [`Failure::FlagsOnly`] for a mode that does not
/// read `kbf.toml`.
pub fn resolve_mode(
    flag: Option<&'static str>,
    file: Option<&str>,
    path: &Path,
) -> Result<(Want, Option<String>), Failure> {
    let modes = || names::MODES.map(|m| m.name).join("|");
    let from_file = match file {
        Some(name) => Some(
            names::MODES
                .iter()
                .find(|m| m.name == name)
                .map(|m| m.name)
                .ok_or_else(|| Failure::BadMode {
                    path: path.display().to_string(),
                    value: name.to_owned(),
                    modes: modes(),
                })?,
        ),
        None => None,
    };
    let Some(mode) = flag.or(from_file) else {
        return Err(Failure::NoMode {
            modes: modes(),
            path: path.display().to_string(),
            detail: "it sets none".to_owned(),
        });
    };
    let overridden = match (flag, from_file) {
        (Some(flag), Some(file)) if flag != file => Some(format!(
            "--mode={flag} overrides mode = {file:?} in {}",
            path.display()
        )),
        _ => None,
    };
    let want = Want::of(mode).ok_or_else(|| Failure::FlagsOnly(mode.to_owned()))?;
    Ok((want, overridden))
}

/// `--check`: what this start would find, without starting, enrolling or dialing anything.
/// A node that is not enrolled yet is checked for what it will enroll with (`kbf.toml`
/// and its token file); an enrolled one gets the checks of any start
/// ([`preflight::check_only`]) on the flags its enrollment makes.
fn check(
    loaded: &Loaded,
    path: &Path,
    want: Want,
    env: &Env,
    driver: Driver,
) -> Result<ExitCode, Failure> {
    crate::check_settings(want.mode, path).map_err(Failure::Settings)?;
    let Some(node) = enrolled(loaded, want)? else {
        // Not enrolled yet: the token must be there to enroll with (and be a token).
        token::read(&loaded.config.token_file, env.euid)?;
        println!(
            "kbf: --check: mode {} is configured; this node is not enrolled yet and will \
             enroll for {} on its first start",
            want.mode,
            want.roles
                .iter()
                .map(Role::name)
                .collect::<Vec<_>>()
                .join(", ")
        );
        return Ok(ExitCode::SUCCESS);
    };
    let argv = plan(&loaded.config, &node, &loaded.store, driver, want)?;
    let invocation = parse_planned(argv, path).map_err(Failure::Mode)?;
    println!(
        "kbf: --check: mode {} is configured and this node is enrolled as {}",
        want.mode, node.node_id
    );
    Ok(preflight::check_only(&invocation, path))
}

/// Runs a start: resolves the mode, enrolls if needed, runs the mode through `launch`
/// while a renewer runs beside it, and returns what the mode returned (or, for `--check`,
/// success).
///
/// # Errors
/// [`Failure`] before the mode starts.
pub fn run_with(
    args: &Start,
    env: &Env,
    driver: Driver,
    launch: &dyn Fn(&Invocation) -> ExitCode,
) -> Result<ExitCode, Failure> {
    let loaded = match load(&args.config, env) {
        Err(Failure::ReadConfig { path, source }) if args.mode.is_none() => {
            return Err(Failure::NoMode {
                modes: names::MODES.map(|m| m.name).join("|"),
                path,
                detail: format!("cannot read it: {source}"),
            });
        }
        loaded => loaded?,
    };
    let (want, overridden) = resolve_mode(args.mode, loaded.config.mode.as_deref(), &args.config)?;
    if let Some(note) = overridden {
        eprintln!("kbf: {note}");
    }
    if args.check {
        return check(&loaded, &args.config, want, env, driver);
    }
    // Before the token is spent: a table that is no setting of the mode would be found
    // only after the node had enrolled.
    crate::check_settings(want.mode, &args.config).map_err(Failure::Settings)?;
    let outcome = enroll_with_retry(&loaded, want, env)?;
    let node = outcome.node;
    if wants_keys(&node.config) {
        eprintln!(
            "kbf: warning: the frontend asks for object-store keys to be fetched; this \
             build does not, and the object-store client uses the credentials of this process"
        );
    }
    let argv = plan(&loaded.config, &node, &loaded.store, driver, want)?;
    let invocation = parse_planned(argv, &args.config).map_err(Failure::Mode)?;

    let renewer = Renewer::new(&loaded, &node, env, jitter());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (stop, stopped) = watch::channel(false);
    let work = move || runtime.block_on(renewer.run(stopped));
    let thread = std::thread::Builder::new()
        .name("kbf-renew".to_owned())
        .spawn(work)?;
    let code = launch(&invocation);
    let _ = stop.send(true);
    let _ = thread.join();
    Ok(code)
}

/// Runs a start that reads `kbf.toml`.
#[must_use]
pub fn run(args: &Start) -> ExitCode {
    // The checks of any start, on the flags the enrollment made.
    run_with(args, &Env::system(), Driver::host(), &|invocation| {
        crate::start(invocation, &args.config)
    })
    .unwrap_or_else(|failure| report(&failure))
}

#[cfg(test)]
#[path = "node_tests.rs"]
mod tests;
