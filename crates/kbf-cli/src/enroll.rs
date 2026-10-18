//! `kbf enroll`: one enrollment, then exit (`docs/design/install-and-enrollment.md`,
//! sections 4.4 and 5).
//!
//! Reads `kbf.toml` (`--config`), and if the node is not enrolled yet (no `node.json` in
//! its state directory) reads the token file, dials the frontend under the pin the token
//! carries, sends one key's request per role and installs what comes back
//! ([`kbf_enroll::client::enroll`]). A node that is enrolled is left alone and the verb
//! exits 0 at once, so a unit can run it before every start (`ExecStartPre=`).
//!
//! **Which roles it asks for.** The token, not the node, decides what a node may be, and
//! a request for a role the token lacks is refused whole. The node asks for the roles
//! `kbf.toml` names: a `[storage]` table asks for `storage`, a `[daemon]` table for
//! `daemon` (an empty table is enough). A file that names neither, or both, asks for both
//! non-frontend roles, so the common token (`storage` and `daemon`) needs no table, and a
//! token that grants one role needs the matching table, which the refusal says.
//!
//! **Exit codes.** 0 enrolled (now, or before). 2 for what a restart cannot fix: a file
//! that is not right (config, token), a frontend that was refused (a typed reason is
//! printed: expired, used up, role not granted, ...), a certificate that is not the
//! pinned CA's, an answer that was refused. 1 for what may pass: the frontend is down,
//! unavailable or rate-limiting, or the node's clock is off.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::Args;
use kbf_enroll::answer::AnswerError;
use kbf_enroll::client::{ConnectError, EnrollError, Enrollment, Frontend, connect, enroll};
use kbf_enroll::config::{Config, Host, Problem, Problems, Trust};
use kbf_enroll::pin::{Anchor, system_roots};
use kbf_enroll::store::{Node as Enrolled, Store, StoreError};
use kbf_enroll::token::{self, TokenFileError};
use kbf_proto::kbf::enroll::v1::NodeFacts;
use kbf_types::{CaPin, Role, RoleSet, RoleSetError};

/// `kbf.toml` when `--config` is not given: where the packages put it.
pub const DEFAULT_CONFIG: &str = if cfg!(target_os = "macos") {
    "/usr/local/etc/kbf/kbf.toml"
} else {
    "/etc/kbf/kbf.toml"
};

/// `kbf enroll`'s command line.
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct Enroll {
    /// The node's config file.
    #[arg(long, value_name = "FILE", default_value = DEFAULT_CONFIG)]
    pub config: PathBuf,
}

/// What the verbs need to know about the machine and the time, passed in so a test can
/// give them a scratch host and a clock of its own.
#[derive(Clone)]
pub struct Env {
    /// The host name and CPU count `kbf.toml`'s defaults and limits read.
    pub host: Host,
    /// The effective uid that must own the node's files.
    pub euid: u32,
    /// This node's unix time.
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
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
    /// The roles the node asks for (the module docs).
    pub roles: RoleSet,
}

/// The roles `text` (a `kbf.toml` that has been validated) asks for.
fn requested_roles(text: &str) -> Result<RoleSet, RoleSetError> {
    let tables = text.parse::<toml::Table>().unwrap_or_default();
    let named: Vec<Role> = Role::ALL
        .into_iter()
        .filter(|role| tables.contains_key(role.name()))
        .collect();
    RoleSet::new(named).or_else(|_| RoleSet::new(Role::ALL))
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
    Ok(Loaded {
        roles: requested_roles(&text)?,
        config,
        store,
    })
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

/// The node as it is enrolled, enrolling it first if it is not.
///
/// # Errors
/// [`Failure`]; for an enrollment that failed nothing was installed and the token file
/// is untouched.
pub fn ensure_enrolled(loaded: &Loaded, env: &Env) -> Result<Outcome, Failure> {
    let config = &loaded.config;
    if let Some(node) = loaded.store.recover()? {
        if node.node_id != config.node_id.as_str() {
            return Err(Failure::RenamedNode {
                enrolled: node.node_id,
                configured: config.node_id.to_string(),
            });
        }
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
            roles: loaded.roles,
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

/// Runs `kbf enroll`: [`ensure_enrolled`], printing what happened.
///
/// # Errors
/// [`Failure`].
pub fn enroll_with(args: &Enroll, env: &Env) -> Result<Outcome, Failure> {
    let loaded = load(&args.config, env)?;
    ensure_enrolled(&loaded, env)
}

/// Prints a failure as `kbf <verb>: ...` and returns its exit status.
pub(crate) fn report(verb: &str, failure: &Failure) -> ExitCode {
    match failure {
        Failure::Mode(error) => {
            let _ = error.print();
        }
        other => eprintln!("kbf {verb}: {other}"),
    }
    ExitCode::from(failure.exit_code())
}

/// Runs `kbf enroll`.
#[must_use]
pub fn run(args: &Enroll) -> ExitCode {
    match enroll_with(args, &Env::system()) {
        Ok(done) => {
            let roles: Vec<&str> = done.node.certs.iter().map(|c| c.role.as_str()).collect();
            let how = if done.fresh {
                "enrolled"
            } else {
                "already enrolled"
            };
            println!(
                "kbf enroll: {how} as {} ({})",
                done.node.node_id,
                roles.join(", ")
            );
            ExitCode::SUCCESS
        }
        Err(failure) => report("enroll", &failure),
    }
}
