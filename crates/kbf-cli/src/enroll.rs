//! Enrollment, as the start of a node mode runs it (`docs/design/install-and-enrollment.md`,
//! sections 4.4 and 5): the library [`crate::node`] calls, not a verb.
//!
//! The start reads `kbf.toml` ([`load`]); if the node is not enrolled yet (no `node.json`
//! in its state directory) it reads the token file, dials the frontend under the pin the
//! token carries, sends one key's request per role the **mode** needs and installs what
//! comes back ([`kbf_enroll::client::enroll`]). A node that is enrolled is left alone and
//! its token file is not read again.
//!
//! **Which roles it asks for.** The mode, not the tables of `kbf.toml`, decides
//! ([`Want::of`]): `storage` asks for the storage role, `daemon` for the daemon role,
//! `storage-and-daemon` for both. The token decides what a host may be, and a request
//! for a role the token lacks is refused whole by the frontend (a typed reason, exit 2:
//! a restart cannot fix it). `[storage]` and `[daemon]` are settings only.
//!
//! **A node that is enrolled** must hold a certificate for every role of the mode. One
//! whose `node.json` does not (the mode was changed after enrollment) is refused with exit 2
//! naming both; it is never enrolled again behind the operator's back.
//!
//! **Exit codes.** 2 for what a restart cannot fix: a file that is not right (config,
//! token), a frontend that refused (a typed reason is printed: expired, used up, role not
//! granted, ...), a certificate that is not the pinned CA's, an answer that was refused, a
//! mode the enrollment does not cover. 1 for what may pass: the frontend is down,
//! unavailable or rate-limiting, or the node's clock is off; [`crate::node`] retries
//! those with a backoff.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use kbf_enroll::answer::AnswerError;
use kbf_enroll::client::{ConnectError, EnrollError, Enrollment, Frontend, connect, enroll};
use kbf_enroll::config::{Config, Host, Problem, Problems, Trust};
use kbf_enroll::pin::{Anchor, system_roots};
use kbf_enroll::store::{Node as Enrolled, Store, StoreError};
use kbf_enroll::token::{self, TokenFileError};
use kbf_proto::kbf::enroll::v1::NodeFacts;
use kbf_types::{CaPin, Role, RoleSet, RoleSetError};

use crate::names;

/// `kbf.toml` when `--config` is not given: where the packages put it.
pub const DEFAULT_CONFIG: &str = if cfg!(target_os = "macos") {
    "/usr/local/etc/kbf/kbf.toml"
} else {
    "/etc/kbf/kbf.toml"
};

/// What a start needs to know about the machine and the time, passed in so a test can
/// give them a scratch host, a clock and a wait of its own.
#[derive(Clone)]
pub struct Env {
    /// The host name and CPU count `kbf.toml`'s defaults and limits read.
    pub host: Host,
    /// The effective uid that must own the node's files.
    pub euid: u32,
    /// This node's unix time.
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// Waits for the given time before a start tries again; false gives up (the start
    /// then fails with the failure it was retrying). The system's sleeps and says true.
    pub pause: Arc<dyn Fn(Duration) -> bool + Send + Sync>,
}

impl Env {
    /// This machine and its clock.
    #[must_use]
    pub fn system() -> Self {
        Self {
            host: Host {
                hostname: rustix::system::uname()
                    .nodename()
                    .to_string_lossy()
                    .into_owned(),
                cpus: std::thread::available_parallelism().map_or(1, usize::from),
            },
            euid: rustix::process::geteuid().as_raw(),
            now: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
            }),
            pause: Arc::new(|wait| {
                std::thread::sleep(wait);
                true
            }),
        }
    }
}

/// Why a verb stopped.
#[derive(Debug, thiserror::Error)]
pub enum Failure {
    /// `kbf.toml` could not be read.
    #[error("cannot read {path}: {source}")]
    ReadConfig {
        /// The file.
        path: String,
        /// What the system said.
        source: std::io::Error,
    },
    /// `kbf.toml` is not valid; every problem is listed.
    #[error("{0}")]
    Config(#[from] Problems),
    /// A directory the file names is not the service user's, private.
    #[error("{}", list(.0))]
    Paths(Vec<Problem>),
    /// The state directory or a file in it.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The token file cannot be used.
    #[error(transparent)]
    Token(#[from] TokenFileError),
    /// The frontend could not be reached, or its certificate was refused.
    #[error(transparent)]
    Connect(#[from] ConnectError),
    /// The enrollment failed.
    #[error(transparent)]
    Enroll(#[from] EnrollError),
    /// This node enrolled under another id than `kbf.toml` now says.
    #[error(
        "this node enrolled as {enrolled}, but kbf.toml says node.id = {configured}; the \
         certificates name {enrolled}"
    )]
    RenamedNode {
        /// The id in `node.json`.
        enrolled: String,
        /// The id in `kbf.toml`.
        configured: String,
    },
    /// This node enrolled for roles that do not cover the mode it is now configured to run.
    #[error(
        "this node is enrolled for {enrolled}, but mode {mode} needs {needs}; the mode \
         changed after enrollment, and a node is not enrolled again by itself: set the mode \
         back, or enroll this host anew (erase its state directory and give it a token \
         that grants {needs})"
    )]
    NotCovered {
        /// The mode.
        mode: String,
        /// The roles the mode needs.
        needs: String,
        /// The roles `node.json` holds certificates for.
        enrolled: String,
    },
    /// No mode: neither the command line nor `kbf.toml` names one.
    #[error("no mode: give --mode=<{modes}> or set mode = \"...\" in {path} ({detail})")]
    NoMode {
        /// The names of the modes.
        modes: String,
        /// The config file.
        path: String,
        /// Why the file names none.
        detail: String,
    },
    /// `mode` in `kbf.toml` is not a mode.
    #[error("{path}: mode = {value:?} is not a mode (one of {modes})")]
    BadMode {
        /// The config file.
        path: String,
        /// What it says.
        value: String,
        /// The names of the modes.
        modes: String,
    },
    /// The mode takes its configuration from flags, not from `kbf.toml`.
    #[error(
        "mode {0} takes its configuration from flags, not from kbf.toml; start it as \
         `kbf --mode={0} <flags>`"
    )]
    FlagsOnly(String),
    /// A set of roles that cannot be (none, or one twice).
    #[error("roles: {0}")]
    Roles(#[from] RoleSetError),
    /// What the frontend granted cannot be run here, or is not complete.
    #[error("{0}")]
    Plan(String),
    /// The flags a mode was built from were refused by the mode's own parser.
    #[error("{0}")]
    Mode(clap::Error),
    /// A runtime or a thread could not be made.
    #[error("cannot start: {0}")]
    Runtime(#[from] std::io::Error),
}

fn list(problems: &[Problem]) -> String {
    problems
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

impl Failure {
    /// The exit status: 1 when waiting may help (the frontend is down or busy, the
    /// clock is off), 2 when a restart cannot fix it (the module docs).
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        let passing = match self {
            Self::Connect(ConnectError::Unreachable(_)) => true,
            Self::Enroll(EnrollError::Call(call)) => call.retryable(),
            Self::Enroll(EnrollError::Answer(AnswerError::ClockSkew { .. })) => true,
            _ => false,
        };
        if passing { 1 } else { 2 }
    }
}

/// A loaded `kbf.toml` and the node's state directory, opened.
pub struct Loaded {
    /// The validated file.
    pub config: Config,
    /// The state directory.
    pub store: Store,
}

/// The mode a start runs and the roles it needs ([`Want::of`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Want {
    /// The mode's name.
    pub mode: &'static str,
    /// The roles whose certificates the mode needs.
    pub roles: RoleSet,
}

impl Want {
    /// What `mode` needs: `storage` the storage role, `daemon` the daemon role,
    /// `storage-and-daemon` both; `None` for a mode that holds no certificate from
    /// enrollment (the frontend and all-in-one take their files as flags).
    #[must_use]
    pub fn of(mode: &str) -> Option<Self> {
        let (mode, roles): (&'static str, &[Role]) = match mode {
            names::STORAGE => (names::STORAGE, &[Role::Storage]),
            names::DAEMON => (names::DAEMON, &[Role::Daemon]),
            names::STORAGE_AND_DAEMON => (names::STORAGE_AND_DAEMON, &Role::ALL),
            _ => return None,
        };
        Some(Self {
            mode,
            roles: RoleSet::new(roles.iter().copied()).ok()?,
        })
    }
}

fn role_list(roles: impl IntoIterator<Item = Role>) -> String {
    let names: Vec<&str> = roles.into_iter().map(Role::name).collect();
    format!("[{}]", names.join(", "))
}

/// Reads and validates `kbf.toml`, checks the directories it names and opens the state
/// directory.
///
/// # Errors
/// [`Failure`]: the file cannot be read or is not valid, a directory is not private to
/// the service user, or the state directory cannot be opened.
pub fn load(path: &Path, env: &Env) -> Result<Loaded, Failure> {
    let text = std::fs::read_to_string(path).map_err(|source| Failure::ReadConfig {
        path: path.display().to_string(),
        source,
    })?;
    let config = Config::parse(&text, &env.host)?;
    let problems = config.check_paths(env.euid);
    if !problems.is_empty() {
        return Err(Failure::Paths(problems));
    }
    let store = Store::open(&config.state_dir)?;
    Ok(Loaded { config, store })
}

/// How the enroll listener is verified: by the pin (`tls.trust = "pin"`) or by the
/// system's roots.
pub(crate) fn anchor(trust: Trust, pin: CaPin) -> Anchor {
    match trust {
        Trust::Pin => Anchor::Pin(pin),
        Trust::Public => Anchor::Roots(Arc::new(system_roots())),
    }
}

/// The node's facts the frontend shows its operator.
fn facts() -> NodeFacts {
    NodeFacts {
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        commit: kbf_server::BUILD_COMMIT.to_owned(),
        protocol: kbf_server::worker::PROTOCOL_VERSION,
        package_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// What [`ensure_enrolled`] found.
pub struct Outcome {
    /// The node's record.
    pub node: Enrolled,
    /// Whether this call enrolled it (false: it already was).
    pub fresh: bool,
}

/// The enrolled node, if there is one, checked against this start: it is the node
/// `kbf.toml` names, and it holds a certificate for every role of the mode.
///
/// # Errors
/// [`Failure::Store`] for a `node.json` that cannot be read, [`Failure::RenamedNode`],
/// [`Failure::NotCovered`].
pub fn enrolled(loaded: &Loaded, want: Want) -> Result<Option<Enrolled>, Failure> {
    let Some(node) = loaded.store.recover()? else {
        return Ok(None);
    };
    if node.node_id != loaded.config.node_id.as_str() {
        return Err(Failure::RenamedNode {
            enrolled: node.node_id,
            configured: loaded.config.node_id.to_string(),
        });
    }
    let held = node.roles();
    if want.roles.iter().any(|role| !held.contains(&role)) {
        return Err(Failure::NotCovered {
            mode: want.mode.to_owned(),
            needs: role_list(want.roles.iter()),
            enrolled: role_list(held),
        });
    }
    Ok(Some(node))
}

/// The node as it is enrolled, enrolling it first if it is not, for the roles of `want`.
///
/// # Errors
/// [`Failure`]; for an enrollment that failed nothing was installed and the token file
/// is untouched.
pub fn ensure_enrolled(loaded: &Loaded, want: Want, env: &Env) -> Result<Outcome, Failure> {
    let config = &loaded.config;
    if let Some(node) = enrolled(loaded, want)? {
        return Ok(Outcome { node, fresh: false });
    }
    let token = token::read(&config.token_file, env.euid)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let node = runtime.block_on(async {
        let frontend: Frontend =
            connect(&config.frontend, anchor(config.trust, *token.pin())).await?;
        let attempt = Enrollment {
            config,
            token: &token,
            roles: want.roles,
            facts: facts(),
            now: (env.now)(),
        };
        Ok::<_, Failure>(enroll(&frontend, &loaded.store, &attempt).await?)
    })?;
    for warning in config.role_warnings(RoleSet::new(node.roles())?) {
        eprintln!("kbf: warning: {warning}");
    }
    Ok(Outcome { node, fresh: true })
}

/// Prints a failure as `kbf: ...` and returns its exit status.
pub(crate) fn report(failure: &Failure) -> ExitCode {
    match failure {
        Failure::Mode(error) => {
            let _ = error.print();
        }
        other => eprintln!("kbf: {other}"),
    }
    ExitCode::from(failure.exit_code())
}
