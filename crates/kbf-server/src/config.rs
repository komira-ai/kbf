//! Command-line flags. Every setting is a flag; the only environment variables read are
//! the standard S3 key pair, `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` (a secret
//! never goes on the command line).

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, ValueEnum};
use kbf_objstore::s3::{Credentials, S3Config, S3ConfigError, S3Store};
use kbf_objstore::{Capabilities, KeyError, KeyPrefix};
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

use kbf_auth::{AllowAuthorizer, Authorizer, Policy, PolicyError};
use kbf_meta::ServerId;
use kbf_tier::{TierArgs, TierOptions};

use crate::identity::{DenyList, DenyListError};
use crate::serve::{Api, Listeners, WorkerTls};
use crate::token::{ApiToken, TokenFileError};

/// The longest heartbeat interval `--heartbeat-interval-ms` accepts: half of
/// [`kbf_sched::fence::START_VALIDITY`].
pub const MAX_HEARTBEAT_INTERVAL_MS: u64 = kbf_sched::fence::START_VALIDITY.as_millis() as u64 / 2;

/// The roles a server can run. A single node runs them all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Role {
    /// Front, scheduler and metadata in one process.
    All,
}

/// Where blob bytes go.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum StoreKind {
    /// In this process; gone when it exits.
    Memory,
    /// An S3 bucket (MinIO, RustFS). Under `--meta-log=memory` the index is in
    /// memory, so a restart forgets every blob, and each start writes under a fresh
    /// key prefix; under `--meta-log=raft` a restart keeps them.
    S3,
}

/// Where the metadata (the CAS index and the action cache) is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum MetaLogKind {
    /// In this process; a restart forgets it.
    Memory,
    /// A single-voter Raft log in `--raft-dir` ([`crate::raft_log`]); a restart keeps
    /// it. Needs `--store=s3`: the bytes it indexes must outlive the process too.
    Raft,
}

/// `kbf-server` flags.
#[derive(Clone, Debug, Parser)]
#[command(
    name = "kbf-server",
    version = crate::SERVER_VERSION,
    about = "The kbf farm server"
)]
pub struct Args {
    /// The roles to run.
    #[arg(long, value_enum, default_value = "all")]
    pub role: Role,
    /// Where blob bytes are stored.
    #[arg(long, value_enum, default_value = "memory")]
    pub store: StoreKind,
    /// Where the metadata (the CAS index and the action cache) is kept.
    #[arg(long, value_enum, default_value = "memory")]
    pub meta_log: MetaLogKind,
    /// The directory of the Raft log (`--meta-log=raft`), on a local disk; created if
    /// missing. An empty directory gets a new log with a new id; every later start
    /// over it reads the id from the directory.
    #[arg(long, required_if_eq("meta_log", "raft"))]
    pub raft_dir: Option<PathBuf>,
    /// The REAPI listener. Without `--reapi-tls-cert` and `--reapi-tls-key` it serves
    /// plain text, on any address; off loopback the start line carries a warning.
    #[arg(long, default_value = "127.0.0.1:8980")]
    pub listen: SocketAddr,
    /// PEM certificate (chain) of the REAPI listener. With `--reapi-tls-key` it serves
    /// TLS with this server certificate only; clients present no certificate. It is meant
    /// for a certificate from the farm's own internal CA, trusted by the front's proxy
    /// (its hop to this server) and by clients that dial this server directly; TLS for
    /// the farm's client-facing name still ends at the front. `--reapi-auth-policy`
    /// applies over TLS as over plain text.
    #[arg(long, requires = "reapi_tls_key")]
    pub reapi_tls_cert: Option<PathBuf>,
    /// PEM private key of the REAPI listener; it must match `--reapi-tls-cert`, or the
    /// server refuses to start.
    #[arg(long, requires = "reapi_tls_cert")]
    pub reapi_tls_key: Option<PathBuf>,
    /// A JSON file with the REAPI listener's authentication policy and authorizers
    /// (`docs/reapi-auth.md`), read once at start; a file that is not a valid policy
    /// stops the server. Without it every REAPI call is accepted and allowed. The
    /// worker listener does not read it; on the operator API only the build pages and
    /// the blob route do, and they are off unless it names their authorizers.
    #[arg(long)]
    pub reapi_auth_policy: Option<PathBuf>,
    /// The `kbf.worker.v1` listener. Without mutual TLS (`--worker-tls-cert`,
    /// `--worker-tls-key`, `--worker-client-ca`) it serves plain text, on any address;
    /// off loopback the start line carries a warning.
    #[arg(long, default_value = "127.0.0.1:8981")]
    pub worker_listen: SocketAddr,
    /// The operator API listener (HTTP/JSON under `/v1`). Off unless given. Reads are
    /// open to whoever reaches it: bind it where only operators do.
    #[arg(long)]
    pub api_listen: Option<SocketAddr>,
    /// A file holding the token operator API writes (cordon, drain, uncordon) must
    /// present as `Authorization: Bearer <token>`. It must be owned by the server's
    /// user with mode 0600 or 0400, or the server refuses to start; run the server as
    /// a user other than the daemon's, whose builds could read it. Without it, writes
    /// are refused.
    #[arg(long, requires = "api_listen")]
    pub api_token_file: Option<PathBuf>,
    /// Serves the build pages (`GET /v1/invocations/{id}` and below) and the blob route
    /// (`GET /v1/blobs/{digest}`) on the operator API to every caller the REAPI policy
    /// accepts, with no `invocations` or `blobs` key in the policy file
    /// (`docs/design/invocations.md` section 4.3): it fills in `allow` for whichever of
    /// the two keys the file leaves out. Without it, and without the keys, those routes
    /// answer 404. Pages and blobs hold build outputs and logs: give it only for a
    /// listener that only trusted people reach.
    #[arg(long)]
    pub invocations_open: bool,
    /// How long, in milliseconds, `GET /readyz` on the operator API and the REAPI
    /// listener's `grpc.health.v1` service wait for the object store to answer their
    /// read-only probe before they answer not ready.
    #[arg(
        long,
        default_value_t = crate::health::STORE_PROBE_TIMEOUT.as_millis() as u64,
        value_parser = clap::value_parser!(u64).range(1..=60_000)
    )]
    pub readyz_store_timeout_ms: u64,
    /// PEM certificate of the worker listener. With `--worker-tls-key` and
    /// `--worker-client-ca` it serves mutual TLS; without all three, plain text.
    #[arg(long, requires_all = ["worker_tls_key", "worker_client_ca"])]
    pub worker_tls_cert: Option<PathBuf>,
    /// PEM private key of the worker listener.
    #[arg(long, requires_all = ["worker_tls_cert", "worker_client_ca"])]
    pub worker_tls_key: Option<PathBuf>,
    /// PEM CA that daemon client certificates must chain to.
    #[arg(long, requires_all = ["worker_tls_cert", "worker_tls_key"])]
    pub worker_client_ca: Option<PathBuf>,
    /// The worker listener's deny list: certificate serials, public keys and node ids
    /// it refuses (format in the `identity` module docs). Read at start, then again at
    /// every `Hello` and `Heartbeat`, so an edit takes effect without a restart. Needs
    /// mutual TLS.
    #[arg(long, requires_all = ["worker_tls_cert", "worker_tls_key", "worker_client_ca"])]
    pub worker_deny_list: Option<PathBuf>,
    /// The heartbeat interval daemons are asked for, in milliseconds, at most half the
    /// window in which a daemon may act on a `Start` (each `Start` names the newest
    /// heartbeat the server took, so a longer interval would leave it too little).
    #[arg(
        long,
        default_value_t = 5_000,
        value_parser = clap::value_parser!(u64).range(1..=MAX_HEARTBEAT_INTERVAL_MS)
    )]
    pub heartbeat_interval_ms: u64,
    /// How long, in seconds, queued work waits while no connected daemon can run it
    /// (none satisfies its platform, or none that does is large enough) before it is
    /// refused FAILED_PRECONDITION. The wait restarts whenever one can.
    #[arg(long, default_value_t = kbf_sched::UNSERVABLE_WAIT.as_secs())]
    pub unservable_wait_secs: u64,
    /// How long, in seconds, a finished operation is kept after its callers are
    /// answered. Within it, WaitExecution on the operation's name streams its result
    /// (a client whose Execute stream broke gets a result the action cache does not
    /// keep, such as a failure); after it, the name is NOT_FOUND. Every operation
    /// finished within it is held in memory.
    #[arg(long, default_value_t = kbf_sched::FINISHED_RETENTION.as_secs())]
    pub finished_retention_secs: u64,
    /// How long, in seconds, a stop (SIGINT or SIGTERM) waits for REAPI clients to
    /// disconnect after their open Execute and WaitExecution streams are ended
    /// UNAVAILABLE and their connections sent GOAWAY. A client still uploading is cut
    /// off when it runs out.
    #[arg(long, default_value_t = 10)]
    pub shutdown_timeout_secs: u64,
    /// The memory, in bytes, the invocation records may take together
    /// (`docs/design/invocations.md`, section 3.5). At the bound, records of a minted id
    /// go first, then idle ones, least recently seen first; with only open builds left,
    /// the largest stops taking rows. Records are kept in memory only and lost on
    /// restart. Their heap was measured at up to 1.08 times this at 200 kB and above,
    /// and 1.12 at 50 kB (section 3.5).
    #[arg(
        long,
        default_value_t = kbf_front::DEFAULT_INVOCATION_MEMORY as u64,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub invocation_memory: u64,
    /// `http://host[:port]` of the S3 service (`--store=s3`).
    #[arg(long, required_if_eq("store", "s3"))]
    pub s3_endpoint: Option<String>,
    /// The bucket (`--store=s3`).
    #[arg(long, required_if_eq("store", "s3"))]
    pub s3_bucket: Option<String>,
    /// The signing region.
    #[arg(long, default_value = "us-east-1")]
    pub s3_region: String,
    /// The key prefix. Under `--meta-log=memory` each start writes under
    /// `<prefix><start time>/`; under `--meta-log=raft` every start writes under
    /// `<prefix><log id>/`, fixed when the log is created (a later start with another
    /// prefix is refused).
    #[arg(long, default_value = "kbf/")]
    pub s3_prefix: String,
    /// The store refuses to overwrite a key (MinIO and RustFS do).
    #[arg(long)]
    pub s3_conditional_put: bool,
    /// The local write tier (`docs/design/write-path.md`, section 6). Off unless
    /// `--write-tier-dir` is given.
    #[command(flatten)]
    pub tier: TierArgs,
}

/// This server in the tier registry. A farm runs one server today
/// (`docs/design/deployment-topology.md`); each gets its own id with HA.
pub const SERVER: ServerId = ServerId::new(1);

/// Why the flags do not make a working server.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A TLS file could not be read.
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The deny list cannot be read or parsed.
    #[error("--worker-deny-list: {0}")]
    DenyList(#[from] DenyListError),
    /// An S3 setting is missing from the environment.
    #[error("{0} must be set for --store=s3")]
    MissingEnv(&'static str),
    /// The S3 configuration is refused.
    #[error(transparent)]
    S3(#[from] S3ConfigError),
    /// The key prefix is not a valid prefix.
    #[error("--s3-prefix: {0}")]
    Prefix(#[from] KeyError),
    /// The operator API token file is refused.
    #[error("--api-token-file: {0}")]
    ApiToken(#[from] TokenFileError),
    /// The REAPI policy file cannot be read.
    #[error("--reapi-auth-policy: read {path}: {source}")]
    PolicyRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// `--meta-log=raft` with `--store=memory`.
    #[error(
        "--meta-log=raft needs --store=s3: a durable index over blobs that die with the process"
    )]
    RaftWithoutS3,
    /// `--raft-dir` without `--meta-log=raft`.
    #[error("--raft-dir is read only with --meta-log=raft")]
    RaftDirUnused,
    /// `--write-tier-dir` with `--meta-log=raft`: the tier's commands need log format
    /// 2, and this binary does not commit the format raise yet.
    #[error(
        "--write-tier-dir needs --meta-log=memory: the Raft log is at format 1, which has no write-tier commands"
    )]
    TierWithRaft,
    /// The REAPI policy file is not a policy.
    #[error("--reapi-auth-policy: {path}: {source}")]
    Policy {
        path: PathBuf,
        #[source]
        source: PolicyError,
    },
}

impl Args {
    /// The write tier's directory and options, or `None` when the tier is off.
    ///
    /// The tier runs over the memory log only, until the Raft log's format raise: the
    /// server then clears the directory at every start
    /// ([`crate::write_tier::attach`]), which a durable log must not do.
    ///
    /// # Errors
    /// [`ConfigError::TierWithRaft`].
    pub fn write_tier(&self) -> Result<Option<(PathBuf, TierOptions)>, ConfigError> {
        match (self.tier.config(), self.meta_log) {
            (Some(_), MetaLogKind::Raft) => Err(ConfigError::TierWithRaft),
            (config, _) => Ok(config),
        }
    }

    /// The listeners these flags describe.
    ///
    /// # Errors
    /// A TLS file cannot be read.
    pub fn listeners(&self) -> Result<Listeners, ConfigError> {
        let worker_tls = match (
            &self.worker_tls_cert,
            &self.worker_tls_key,
            &self.worker_client_ca,
        ) {
            (Some(cert), Some(key), Some(ca)) => Some(WorkerTls {
                server: ServerTlsConfig::new()
                    .identity(Identity::from_pem(read(cert)?, read(key)?))
                    .client_ca_root(Certificate::from_pem(read(ca)?)),
                deny_list: self
                    .worker_deny_list
                    .as_deref()
                    .map(DenyList::open)
                    .transpose()?,
            }),
            _ => None,
        };
        let reapi_tls = match (&self.reapi_tls_cert, &self.reapi_tls_key) {
            (Some(cert), Some(key)) => {
                Some(ServerTlsConfig::new().identity(Identity::from_pem(read(cert)?, read(key)?)))
            }
            _ => None,
        };
        Ok(Listeners {
            reapi: self.listen,
            reapi_tls,
            worker: self.worker_listen,
            worker_tls,
            heartbeat_interval: Duration::from_millis(self.heartbeat_interval_ms),
            hello_wait: Duration::from_secs(10),
            tick: Duration::from_secs(1),
            unservable_wait: Duration::from_secs(self.unservable_wait_secs),
            finished_retention: Duration::from_secs(self.finished_retention_secs),
            shutdown_timeout: Duration::from_secs(self.shutdown_timeout_secs),
            store_probe_timeout: Duration::from_millis(self.readyz_store_timeout_ms),
            invocation_memory: usize::try_from(self.invocation_memory).unwrap_or(usize::MAX),
        })
    }

    /// The listeners that serve plain text on an address that is not loopback, as the
    /// binary's start-line warning names them: `reapi` (no `--reapi-tls-cert`), then
    /// `worker` (no `--worker-tls-cert`). Empty when every plain-text listener is on
    /// loopback. Nothing refuses such a bind; whether plain text is acceptable on that
    /// network is the operator's call.
    #[must_use]
    pub fn plain_text_off_loopback(&self) -> Vec<&'static str> {
        let reapi = self.reapi_tls_cert.is_none() && !loopback_only(self.listen.ip());
        let worker = self.worker_tls_cert.is_none() && !loopback_only(self.worker_listen.ip());
        [("reapi", reapi), ("worker", worker)]
            .into_iter()
            .filter_map(|(name, plain)| plain.then_some(name))
            .collect()
    }

    /// The REAPI listener's policy: the file `--reapi-auth-policy` names, or
    /// [`Policy::allow_all`] without it; with `--invocations-open`, its
    /// `invocations_get` and `blobs_get` are `allow` where the file names neither.
    ///
    /// # Errors
    /// The file cannot be read, or is not a policy ([`Policy::from_json`]).
    pub fn reapi_auth_policy(&self) -> Result<Policy, ConfigError> {
        let mut policy = match &self.reapi_auth_policy {
            None => Policy::allow_all(),
            Some(path) => {
                let text =
                    std::fs::read_to_string(path).map_err(|source| ConfigError::PolicyRead {
                        path: path.clone(),
                        source,
                    })?;
                Policy::from_json(&text).map_err(|source| ConfigError::Policy {
                    path: path.clone(),
                    source,
                })?
            }
        };
        if self.invocations_open {
            let allow = || -> Arc<dyn Authorizer> { Arc::new(AllowAuthorizer) };
            policy.invocations_get.get_or_insert_with(allow);
            policy.blobs_get.get_or_insert_with(allow);
        }
        Ok(policy)
    }

    /// The operator API these flags describe, if `--api-listen` is given, with the
    /// token read from `--api-token-file`.
    ///
    /// # Errors
    /// The token file is refused (see [`ApiToken::from_file`]).
    pub fn api(&self) -> Result<Option<Api>, ConfigError> {
        let Some(listen) = self.api_listen else {
            return Ok(None);
        };
        let token = self
            .api_token_file
            .as_deref()
            .map(ApiToken::from_file)
            .transpose()?;
        Ok(Some(Api { listen, token }))
    }

    /// The raft directory under `--meta-log=raft`; `None` under `--meta-log=memory`.
    ///
    /// # Errors
    /// [`ConfigError::RaftWithoutS3`] for `--meta-log=raft --store=memory`, and
    /// [`ConfigError::RaftDirUnused`] for a `--raft-dir` under `--meta-log=memory`.
    pub fn raft_dir(&self) -> Result<Option<&Path>, ConfigError> {
        match (self.meta_log, self.store, self.raft_dir.as_deref()) {
            (MetaLogKind::Memory, _, None) => Ok(None),
            (MetaLogKind::Memory, _, Some(_)) => Err(ConfigError::RaftDirUnused),
            (MetaLogKind::Raft, StoreKind::Memory, _) => Err(ConfigError::RaftWithoutS3),
            // clap requires `--raft-dir` with `--meta-log=raft`.
            (MetaLogKind::Raft, StoreKind::S3, dir) => Ok(dir),
        }
    }

    /// The S3 store and the key prefix of this start, `<prefix><start time>/` (the
    /// one `--meta-log=memory` writes under), for `--store=s3`. `env` reads an
    /// environment variable (the S3 key pair).
    ///
    /// # Errors
    /// A flag or the key pair is missing, or the configuration is refused.
    pub fn s3_store(
        &self,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<(S3Store, KeyPrefix), ConfigError> {
        let need = |name: &'static str| env(name).ok_or(ConfigError::MissingEnv(name));
        let credentials =
            Credentials::new(need("AWS_ACCESS_KEY_ID")?, need("AWS_SECRET_ACCESS_KEY")?);
        // clap requires both flags with `--store=s3`; an empty value is S3Store's to refuse.
        let store = S3Store::new(S3Config {
            endpoint: self.s3_endpoint.clone().unwrap_or_default(),
            region: self.s3_region.clone(),
            bucket: self.s3_bucket.clone().unwrap_or_default(),
            credentials,
            capabilities: Capabilities {
                conditional_put: self.s3_conditional_put,
                object_lock: false,
            },
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(60),
        })?;
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let prefix = KeyPrefix::new(format!("{}{started}/", self.s3_prefix))?;
        Ok((store, prefix))
    }
}

/// Whether a listener bound to `ip` is reachable only from this host: a loopback
/// address, an IPv4-mapped one (`::ffff:127.0.0.1`) included. A wildcard (`0.0.0.0`,
/// `::`) and every other address can be reached from other machines.
#[must_use]
pub fn loopback_only(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

fn read(path: &PathBuf) -> Result<Vec<u8>, ConfigError> {
    std::fs::read(path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Args, ConfigError, loopback_only};

    /// Catches: the write tier accepted over `--meta-log=raft` (the log is at format 1,
    /// so every tier command would fail to commit), refused over the memory log, or on
    /// when `--write-tier-dir` is not given.
    #[test]
    fn the_write_tier_needs_the_memory_log() {
        let parse = |extra: &[&str]| {
            let base = ["kbf-server", "--store", "s3", "--s3-endpoint", "http://e:9"];
            let rest = ["--s3-bucket", "b"];
            Args::parse_from(base.iter().chain(&rest).chain(extra))
        };
        let tier = ["--write-tier-dir", "/t", "--write-tier-bytes", "9"];
        assert_eq!(parse(&[]).write_tier().expect("off"), None);
        let on = parse(&tier).write_tier().expect("memory");
        assert_eq!(on.map(|(_, o)| o.budget), Some(9));
        let raft = ["--meta-log", "raft", "--raft-dir", "/r"];
        let both: Vec<&str> = tier.iter().chain(&raft).copied().collect();
        assert!(matches!(
            parse(&both).write_tier(),
            Err(ConfigError::TierWithRaft)
        ));
    }

    /// Catches: a `loopback_only` that knows only wildcards (`is_unspecified`), so one
    /// interface other machines reach (`192.0.2.10`, a documentation address standing
    /// in for a LAN or mesh one) counts as loopback; and one that drops `to_canonical`,
    /// so the IPv4-mapped loopback address does not.
    #[test]
    fn only_loopback_addresses_are_loopback_only() {
        for (ip, want) in [
            ("127.0.0.1", true),
            ("::1", true),
            ("::ffff:127.0.0.1", true),
            ("0.0.0.0", false),
            ("::", false),
            ("192.0.2.10", false),
        ] {
            assert_eq!(loopback_only(ip.parse().expect("an address")), want, "{ip}");
        }
    }

    /// Catches: a call site in [`Args::plain_text_off_loopback`] that does not go
    /// through `loopback_only`. One listener is on a single non-loopback interface
    /// (`192.0.2.10`, which a wildcard-only check such as `is_unspecified` misses) and
    /// the other on the IPv4-mapped loopback address (`::ffff:127.0.0.1`, which
    /// `is_loopback` without `to_canonical` misses), each way round, so either
    /// mistake at either listener changes the list.
    #[test]
    fn plain_text_off_loopback_checks_each_listener_through_loopback_only() {
        for (listen, worker_listen, want) in [
            ("192.0.2.10:0", "[::ffff:127.0.0.1]:0", vec!["reapi"]),
            ("[::ffff:127.0.0.1]:0", "192.0.2.10:0", vec!["worker"]),
        ] {
            let args = Args::parse_from([
                "kbf-server",
                "--listen",
                listen,
                "--worker-listen",
                worker_listen,
            ]);
            assert_eq!(
                args.plain_text_off_loopback(),
                want,
                "--listen {listen} --worker-listen {worker_listen}"
            );
        }
    }
}
