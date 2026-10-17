//! `kbf node`: the one unit for every non-frontend role
//! (`docs/design/install-and-enrollment.md`, section 4.4).
//!
//! 1. Reads `kbf.toml` (`--config`) and, if the node holds no `node.json`, enrolls it
//!    exactly as `kbf enroll` does ([`crate::enroll`]); a node that is enrolled does not
//!    read its token file again.
//! 2. Picks its mode from the roles the frontend granted (`NodeConfig.roles`, kept in
//!    `node.json`): `{daemon}` is `kbf daemon`, `{storage}` is `kbf storage`, both is
//!    `kbf storage-and-daemon`. It builds that mode's command line from `kbf.toml`, the
//!    configuration the frontend sent and the certificate files, parses it with the
//!    mode's own parser ([`crate::parse_from`], the same one `kbf check-args` and the
//!    units use, so a flag the mode would refuse is refused here the same way) and runs
//!    the mode in this process.
//! 3. While the mode runs, renews the certificates at half their life
//!    ([`Renewer`], `kbf_enroll::renew`), retrying every 5 minutes doubling to an hour,
//!    and writes the new files in place. A change of roles or endpoints in a renewal's
//!    answer is stored and applied at the next start; `kbf node` never restarts itself.
//!
//! **What the certificate files reach.** A storage node's listener reads its certificate
//! again every `NodeConfig.tls_reload_interval_s` (`kbf-tls`), and a daemon's connections
//! to the frontend and to the storage nodes read theirs when they are made
//! (`kbf-daemon`), so a renewal needs no restart. The daemon's one connection that does
//! not follow the files is the transfer engine over `--cas` (`kbf-node`), which a daemon
//! uses only until the frontend sends it a map.
//!
//! **The object-store credentials.** A storage node is given the cold store
//! (`NodeConfig.cold_store`), never a key. This build does not fetch keys from the
//! frontend (`kbf_enroll::keys` is not called): the object-store client uses the
//! credentials of the process, as `kbf storage` does, and a frontend that asks for
//! `KEYS` mode is told so on stderr.
//!
//! Exit codes are `kbf enroll`'s ([`crate::enroll`]) until the mode starts, and the
//! mode's own after.

use std::ffi::{OsStr, OsString};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use kbf_enroll::client::{ConnectError, connect};
use kbf_enroll::config::{Config, FrontendUrl, Trust};
use kbf_enroll::renew::{Backoff, RenewError, first_attempt, near_expiry, renew};
use kbf_enroll::store::{Node as Enrolled, Store, StoreError, StoredConfig};
use kbf_proto::kbf::enroll::v1::CredentialMode;
use kbf_types::{Role, RoleSet};
use std::path::PathBuf;
use tokio::sync::watch;

use crate::enroll::{DEFAULT_CONFIG, Env, Failure, Loaded, anchor, ensure_enrolled, load, report};
use crate::{Invocation, names, parse_from};

/// The longest the renew loop sleeps at once, in seconds: a wall clock that jumped (a
/// machine that slept) is noticed within this.
const NAP_MAX_S: u64 = 3600;

/// `kbf node`'s command line.
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// The node's config file.
    #[arg(long, value_name = "FILE", default_value = DEFAULT_CONFIG)]
    pub config: PathBuf,
    /// A label the daemon half reports to the frontend (repeatable), `key=value`.
    #[arg(long = "label", value_name = "LABEL")]
    pub labels: Vec<String>,
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

/// The roles the node runs: those the frontend granted, each of which the node must
/// hold a certificate for.
fn roles_to_run(node: &Enrolled) -> Result<RoleSet, Failure> {
    let granted =
        RoleSet::from_names(node.config.roles.iter().map(String::as_str)).map_err(|e| {
            Failure::Plan(format!(
                "the frontend granted this node no usable roles: {e}"
            ))
        })?;
    let held = node.roles();
    for role in granted.iter() {
        if !held.contains(&role) {
            return Err(Failure::Plan(format!(
                "the frontend granted the {role} role, but this node holds no certificate \
                 for it; name it in kbf.toml (a [{role}] table) and enroll again with a \
                 token that grants it"
            )));
        }
    }
    Ok(granted)
}

/// The mode that runs `roles`.
fn mode_of(roles: RoleSet) -> &'static str {
    match (roles.contains(Role::Storage), roles.contains(Role::Daemon)) {
        (true, true) => names::STORAGE_AND_DAEMON,
        (true, false) => names::STORAGE,
        _ => names::DAEMON,
    }
}

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
    if stored.cold_store.is_empty() {
        return Err(missing("cold store"));
    }
    let advertise = config.advertise.as_ref().map_or_else(
        || format!("{}:{port}", config.node_id),
        |a| format!("{}:{}", a.host, a.port),
    );
    let mut out = vec![
        flag("cold-store", &stored.cold_store),
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
    labels: &[String],
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
    out.extend(labels.iter().map(|label| flag("label", label)));
    out
}

/// The command line of the mode the node runs, `kbf <mode> <flags>`.
///
/// # Errors
/// [`Failure::Plan`]: the frontend granted no usable role, or a role the node holds no
/// certificate for, or sent no worker endpoint, or (for storage) no storage port or cold
/// store.
pub fn plan(
    config: &Config,
    node: &Enrolled,
    store: &Store,
    driver: Driver,
    labels: &[String],
) -> Result<Vec<OsString>, Failure> {
    let roles = roles_to_run(node)?;
    let stored = &node.config;
    if stored.worker_endpoint.is_empty() {
        return Err(missing("worker endpoint"));
    }
    let mode = mode_of(roles);
    let mut argv = vec![OsString::from("kbf"), OsString::from(mode)];
    if roles.contains(Role::Storage) {
        argv.extend(storage_flags(config, stored, store)?);
    }
    if roles.contains(Role::Daemon) {
        let with_cas = !roles.contains(Role::Storage);
        argv.extend(daemon_flags(
            config, stored, store, driver, labels, with_cas,
        ));
    }
    Ok(argv)
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

/// Runs `kbf node`: enrolls if needed, runs the mode the roles name through `launch`
/// while a renewer runs beside it, and returns what the mode returned.
///
/// # Errors
/// [`Failure`] before the mode starts.
pub fn run_with(
    args: &Node,
    env: &Env,
    driver: Driver,
    launch: &dyn Fn(&Invocation) -> ExitCode,
) -> Result<ExitCode, Failure> {
    let loaded = load(&args.config, env)?;
    let outcome = ensure_enrolled(&loaded, env)?;
    let node = outcome.node;
    if wants_keys(&node.config) {
        eprintln!(
            "kbf: warning: the frontend asks for object-store keys to be fetched; this \
             build does not, and the object-store client uses the credentials of this process"
        );
    }
    let argv = plan(&loaded.config, &node, &loaded.store, driver, &args.labels)?;
    let invocation = parse_from(argv).map_err(Failure::Mode)?;

    let renewer = Renewer::new(&loaded, &node, env, jitter());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (stop, stopped) = watch::channel(false);
    let thread = std::thread::Builder::new()
        .name("kbf-renew".to_owned())
        .spawn(move || runtime.block_on(renewer.run(stopped)))?;
    let code = launch(&invocation);
    let _ = stop.send(true);
    let _ = thread.join();
    Ok(code)
}

/// Runs `kbf node`.
#[must_use]
pub fn run(args: &Node) -> ExitCode {
    run_with(args, &Env::system(), Driver::host(), &crate::run)
        .unwrap_or_else(|failure| report("node", &failure))
}

#[cfg(test)]
#[path = "node_tests.rs"]
mod tests;
