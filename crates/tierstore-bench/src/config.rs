//! The configuration of a run: flags plus a private run file.
//!
//! Flags carry everything that is not a secret and is not a name of this deployment's
//! machines: caps, gate thresholds, the window, the seed, and `ROLE=VALUE` pairs for endpoints
//! and buckets. The run file (`--run-file`, JSON, outside every repository) carries what a flag
//! must not: the access key pairs, and, if one prefers, the endpoints and buckets too. A flag
//! overrides the run file for the same role. Nothing here is serialised into a result: a
//! [`RunConfig`] has no `Serialize`, so a result cannot embed it by accident.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cells::coldguard::ColdGuardLimits;
use crate::gate::LoadGate;
use crate::result::Window;
use crate::role::Role;

const GIB: u64 = 1 << 30;

/// Why the flags and the run file do not make a configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A role name that is none of [`Role::ALL`].
    #[error("unknown role {0:?}")]
    UnknownRole(String),
    /// A `ROLE=VALUE` flag without the `=`.
    #[error("expected ROLE=VALUE, got {0:?}")]
    BadPair(String),
    /// The run file could not be read.
    #[error("run file {path}: {source}")]
    RunFileRead {
        /// The path given to `--run-file`.
        path: PathBuf,
        /// What the operating system said.
        #[source]
        source: std::io::Error,
    },
    /// The run file is not JSON of the shape [`RunFile`] describes.
    #[error("run file {path}: {source}")]
    RunFileParse {
        /// The path given to `--run-file`.
        path: PathBuf,
        /// What the parser said.
        #[source]
        source: serde_json::Error,
    },
    /// A store role has no value for something it needs, in the flags or the run file.
    #[error("role {role}: no {field}")]
    Missing {
        /// The role.
        role: Role,
        /// What is missing.
        field: &'static str,
    },
    /// A flag or the run file names a store for a role that is a machine.
    #[error("role {0} is not a store: it has no endpoint, bucket or key")]
    NotAStore(Role),
    /// The byte caps are not `pass <= cell <= campaign`.
    #[error("byte caps must satisfy pass <= cell <= campaign")]
    CapOrder,
    /// No `test_prefix` was given, or it is not lowercase letters, digits and dashes with a stem
    /// and a closing dash.
    #[error("test_prefix: {0}")]
    TestPrefix(&'static str),
    /// A bucket or an account id the run would create, write or delete does not start with the
    /// test prefix. Nothing was sent to any store.
    #[error("role {role}: the {what} {value:?} does not start with the test prefix {prefix:?}")]
    OutsidePrefix {
        /// The role.
        role: Role,
        /// `bucket` or `account id`.
        what: &'static str,
        /// The offending name.
        value: String,
        /// The test prefix.
        prefix: String,
    },
}

/// Whether `provision` makes a scoped account for `role`: the shared stores, `hot` and `cold`.
fn gets_account(role: Role) -> bool {
    matches!(role, Role::Hot | Role::Cold)
}

/// The test prefix as the run file or `--test-prefix` gives it: lowercase letters, digits and
/// dashes, a stem and then a closing dash (`bench7-`), so a name that starts with it can be told
/// from a farm's own at a glance.
///
/// # Errors
/// [`ConfigError::TestPrefix`] with what is wrong.
pub fn check_test_prefix(prefix: &str) -> Result<(), ConfigError> {
    if prefix.is_empty() {
        return Err(ConfigError::TestPrefix("it is empty"));
    }
    if !prefix
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(ConfigError::TestPrefix(
            "it may hold only lowercase letters, digits and dashes",
        ));
    }
    if !prefix.ends_with('-') || prefix.len() < 2 {
        return Err(ConfigError::TestPrefix(
            "it must be a stem followed by a closing dash",
        ));
    }
    Ok(())
}

/// A secret that never prints: its `Debug` is `<redacted>`.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret.
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// The secret itself, for signing a request.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// What one store role needs to be reached. Never serialised.
#[derive(Clone, Debug)]
pub struct StoreSpec {
    /// `http://host:port`.
    pub endpoint: String,
    /// The test bucket.
    pub bucket: String,
    /// The signing region.
    pub region: String,
    /// The access key id of the scoped account for this bucket.
    pub access_key_id: String,
    /// Its secret.
    pub secret_access_key: Secret,
}

/// One store's entry in the run file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreFile {
    /// Overridden by `--endpoint ROLE=URL`.
    pub endpoint: Option<String>,
    /// Overridden by `--bucket ROLE=NAME`.
    pub bucket: Option<String>,
    /// Overridden by `--region` only when this is absent.
    pub region: Option<String>,
    /// The scoped account's access key id.
    pub access_key_id: String,
    /// The scoped account's secret.
    pub secret_access_key: Secret,
}

/// The private run file: JSON, `{"stores": {"hot": {...}, "cold": {...}, "instance": {...}}}`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunFile {
    /// What every bucket and every account id the tool creates, writes or deletes must start
    /// with. Required, here or as `--test-prefix` (the flag wins).
    #[serde(default)]
    pub test_prefix: Option<String>,
    /// One entry per store role.
    #[serde(default)]
    pub stores: BTreeMap<Role, StoreFile>,
}

/// Caps on the bytes the campaign may write to the cold store, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteCaps {
    /// The whole campaign.
    pub campaign: u64,
    /// One pass of one cell.
    pub pass: u64,
    /// All passes of one cell.
    pub cell: u64,
}

/// Request-rate caps, per second, held by the harness whatever a cell asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateCaps {
    /// Requests to the cold store (token bucket, global).
    pub cold_rps: u32,
    /// Requests to the hot store, applied inside a drained window only.
    pub hot_rps: u32,
    /// Prefix LISTs a copy-lag poll may issue.
    pub list_poll_rps: u32,
}

/// The flags every verb that touches a store takes.
#[derive(Clone, Debug, clap::Args)]
pub struct ConfigArgs {
    /// Private run file (JSON, never committed): access keys, and endpoints and buckets by role.
    #[arg(long, value_name = "PATH")]
    pub run_file: Option<PathBuf>,
    /// Directory the result JSON, the CSV of samples and the request log are written to.
    #[arg(long, value_name = "DIR")]
    pub result_dir: PathBuf,
    /// What every test bucket name and every scoped account id must start with (lowercase
    /// letters, digits and dashes, ending in `-`); a verb refuses, before any store call, a
    /// name that does not. Required, here or as `test_prefix` of the run file.
    #[arg(long, value_name = "PREFIX")]
    pub test_prefix: Option<String>,
    /// Endpoint of a store role, `ROLE=http://host:port`; repeatable.
    #[arg(long = "endpoint", value_name = "ROLE=URL", value_parser = parse_pair)]
    pub endpoints: Vec<(Role, String)>,
    /// Bucket of a store role, `ROLE=NAME`; repeatable.
    #[arg(long = "bucket", value_name = "ROLE=NAME", value_parser = parse_pair)]
    pub buckets: Vec<(Role, String)>,
    /// Signing region for a store whose run-file entry names none.
    #[arg(long, default_value = "us-east-1")]
    pub region: String,
    /// Whether this run is in a drained window (`quiet`) or in normal hours (`busy`); the
    /// operator declares it, and a passing load gate does not turn a busy run quiet.
    #[arg(long, value_enum, default_value_t = Window::Busy)]
    pub window: Window,
    /// Seed of the content generator, so a tree or a body is the same bytes on every run.
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
    /// Load gate: highest one-minute load average.
    #[arg(long, default_value_t = 8.0)]
    pub max_loadavg_1m: f64,
    /// Load gate: highest CPU pressure (some, ten-second average, percent).
    #[arg(long, default_value_t = 10.0)]
    pub max_psi_cpu_avg10: f64,
    /// Load gate: highest IO pressure (some, ten-second average, percent).
    #[arg(long, default_value_t = 10.0)]
    pub max_psi_io_avg10: f64,
    /// Load gate: highest utilisation of the busiest disk, percent.
    #[arg(long, default_value_t = 60.0)]
    pub max_disk_util_pct: f64,
    /// Cold store guard, hung-store backstop only: the run stops when the p99 latency of the
    /// harness's own requests to the cold store over the window is above this, milliseconds. The
    /// bench's own latency is what a cell measures, so this is not a health limit: 30 s is a
    /// store that has stopped answering. What the guard judges is the load from elsewhere
    /// (`--cold-abort-foreign-requests`) and the drives.
    #[arg(long, default_value_t = 30_000, value_parser = clap::value_parser!(u64).range(1..))]
    pub cold_abort_p99_ms: u64,
    /// Cold store guard: the run stops when a pass saw more than this many requests at the cold
    /// store that this tool did not send (the store's S3 request counter less the tool's own
    /// count over the pass). A quiet store showed none in 30 s; a few strays (a probe, an
    /// operator's listing) are allowed, a job or a person at work is not. Needs the cold admin key.
    #[arg(long, default_value_t = 10)]
    pub cold_abort_foreign_requests: u64,
    /// How long to wait, with the tool silent, before each reading of the cold store's request
    /// counter, seconds: MinIO serves that counter from a 10 s cache. 0 reads at once and may
    /// count a pass's requests in its neighbour's window.
    #[arg(long, default_value_t = 11)]
    pub cold_counter_settle_secs: u64,
    /// Cold store guard: the run stops when the share of the harness's own requests to the cold
    /// store over the window that failed or timed out is above this (0 to 1).
    #[arg(long, default_value_t = 0.05, value_parser = parse_share)]
    pub cold_abort_error_share: f64,
    /// Cold store guard: the length of the sliding window, seconds.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
    pub cold_abort_window_secs: u64,
    /// The longest a reading of the cold store's state (`mc admin info`) may take, seconds; the
    /// run stops, the reading being unreadable, if it takes longer.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    pub cold_state_timeout_secs: u64,
    /// File mode only (the ops file has no cold admin key and names a heal status file): the
    /// oldest a status file's own `read_unix_ms` may be, seconds, when a reading is taken. An
    /// older file stops the run and fails the preflight; so does one stamped more than 5 s in
    /// the future.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    pub cold_state_max_age_secs: u64,
    /// Cap on requests per second to the cold store.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..))]
    pub cold_rps: u32,
    /// Cap on requests per second to the hot store.
    #[arg(long, default_value_t = 2000, value_parser = clap::value_parser!(u32).range(1..))]
    pub hot_rps: u32,
    /// Cap on prefix LISTs per second of a copy-lag poll.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(1..))]
    pub list_poll_rps: u32,
    /// Campaign cap on bytes written to the cold store, GiB.
    #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u64).range(1..))]
    pub campaign_cap_gib: u64,
    /// Cap on one pass of a cell, GiB.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u64).range(1..))]
    pub pass_cap_gib: u64,
    /// Cap on all passes of a cell, GiB.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..))]
    pub cell_cap_gib: u64,
}

/// A share: a number from 0 to 1.
fn parse_share(s: &str) -> Result<f64, String> {
    let share: f64 = s.parse().map_err(|_| format!("{s:?} is not a number"))?;
    if (0.0..=1.0).contains(&share) {
        Ok(share)
    } else {
        Err(format!("{s} is not between 0 and 1"))
    }
}

/// `ROLE=VALUE`, split at the first `=`.
fn parse_pair(s: &str) -> Result<(Role, String), ConfigError> {
    let (role, value) = s
        .split_once('=')
        .ok_or_else(|| ConfigError::BadPair(s.to_owned()))?;
    Ok((role.parse()?, value.to_owned()))
}

/// The resolved configuration of a run.
#[derive(Clone, Debug)]
pub struct RunConfig {
    /// Where results, samples and the request log go.
    pub result_dir: PathBuf,
    /// What every bucket and every account id this tool creates, writes or deletes starts with.
    pub test_prefix: String,
    /// The store roles in use, each complete.
    pub stores: BTreeMap<Role, StoreSpec>,
    /// Declared by the operator.
    pub window: Window,
    /// Content seed.
    pub seed: u64,
    /// The load gate's limits.
    pub gate: LoadGate,
    /// The cold store guard's limits.
    pub cold_guard: ColdGuardLimits,
    /// The wait before each reading of the cold store's request counter.
    pub cold_counter_settle: Duration,
    /// The longest one reading of the cold store's state may take.
    pub cold_state_timeout: Duration,
    /// See [`ConfigArgs::cold_state_max_age_secs`].
    pub cold_state_max_age: Duration,
    /// Request-rate caps.
    pub rates: RateCaps,
    /// Byte caps.
    pub bytes: ByteCaps,
}

impl RunConfig {
    /// Reads the run file named by the flags, if any, and resolves.
    ///
    /// # Errors
    /// [`ConfigError`]: the file cannot be read or parsed, or the result is incomplete.
    pub fn load(args: &ConfigArgs) -> Result<Self, ConfigError> {
        let file = args.run_file.as_deref().map(read_run_file).transpose()?;
        Self::resolve(args, file)
    }

    /// Joins flags and a parsed run file. Every role named by either must be a store with an
    /// endpoint, a bucket and a key pair.
    ///
    /// # Errors
    /// [`ConfigError`]: a role that is a machine, a field with no value, or byte caps out of
    /// order.
    pub fn resolve(args: &ConfigArgs, file: Option<RunFile>) -> Result<Self, ConfigError> {
        let bytes = ByteCaps {
            campaign: args.campaign_cap_gib.saturating_mul(GIB),
            pass: args.pass_cap_gib.saturating_mul(GIB),
            cell: args.cell_cap_gib.saturating_mul(GIB),
        };
        if bytes.pass > bytes.cell || bytes.cell > bytes.campaign {
            return Err(ConfigError::CapOrder);
        }
        let file = file.unwrap_or_default();
        let test_prefix = args
            .test_prefix
            .clone()
            .or_else(|| file.test_prefix.clone())
            .ok_or(ConfigError::TestPrefix(
                "none given (--test-prefix, or test_prefix in the run file)",
            ))?;
        check_test_prefix(&test_prefix)?;
        let roles: BTreeSet<Role> = file
            .stores
            .keys()
            .copied()
            .chain(args.endpoints.iter().map(|(r, _)| *r))
            .chain(args.buckets.iter().map(|(r, _)| *r))
            .collect();
        let mut stores = BTreeMap::new();
        for role in roles {
            if !role.is_store() {
                return Err(ConfigError::NotAStore(role));
            }
            let entry = file.stores.get(&role).ok_or(ConfigError::Missing {
                role,
                field: "access key (run file)",
            })?;
            let endpoint = flag_or(&args.endpoints, role, entry.endpoint.as_ref()).ok_or(
                ConfigError::Missing {
                    role,
                    field: "endpoint",
                },
            )?;
            let bucket = flag_or(&args.buckets, role, entry.bucket.as_ref()).ok_or(
                ConfigError::Missing {
                    role,
                    field: "bucket",
                },
            )?;
            let region = entry.region.clone().unwrap_or_else(|| args.region.clone());
            stores.insert(
                role,
                StoreSpec {
                    endpoint,
                    bucket,
                    region,
                    access_key_id: entry.access_key_id.clone(),
                    secret_access_key: entry.secret_access_key.clone(),
                },
            );
        }
        let config = Self {
            result_dir: args.result_dir.clone(),
            test_prefix,
            stores,
            window: args.window,
            seed: args.seed,
            gate: LoadGate {
                max_loadavg_1m: args.max_loadavg_1m,
                max_psi_cpu_avg10: args.max_psi_cpu_avg10,
                max_psi_io_avg10: args.max_psi_io_avg10,
                max_disk_util_pct: args.max_disk_util_pct,
            },
            cold_guard: ColdGuardLimits {
                window: Duration::from_secs(args.cold_abort_window_secs),
                max_p99_ms: args.cold_abort_p99_ms,
                max_error_share: args.cold_abort_error_share,
                max_foreign_requests: args.cold_abort_foreign_requests,
            },
            cold_counter_settle: Duration::from_secs(args.cold_counter_settle_secs),
            cold_state_timeout: Duration::from_secs(args.cold_state_timeout_secs),
            cold_state_max_age: Duration::from_secs(args.cold_state_max_age_secs),
            rates: RateCaps {
                cold_rps: args.cold_rps,
                hot_rps: args.hot_rps,
                list_poll_rps: args.list_poll_rps,
            },
            bytes,
        };
        config.check_names()?;
        Ok(config)
    }

    /// Refuses a bucket, or an account id of the roles `provision` makes accounts for (`hot`,
    /// `cold`), that does not start with the test prefix. [`Self::resolve`] runs it, so every
    /// verb that holds a configuration has passed it; `provision` runs it again before its first
    /// command, for a configuration built by hand.
    ///
    /// # Errors
    /// [`ConfigError::OutsidePrefix`] naming the role and the offending value.
    pub fn check_names(&self) -> Result<(), ConfigError> {
        let outside = |role, what, value: &str| ConfigError::OutsidePrefix {
            role,
            what,
            value: value.to_owned(),
            prefix: self.test_prefix.clone(),
        };
        for (&role, spec) in &self.stores {
            if !spec.bucket.starts_with(&self.test_prefix) {
                return Err(outside(role, "bucket", &spec.bucket));
            }
            if gets_account(role) && !spec.access_key_id.starts_with(&self.test_prefix) {
                return Err(outside(role, "account id", &spec.access_key_id));
            }
        }
        Ok(())
    }

    /// The names a verb would create, write or delete, one `key=value` per line: `test_prefix=`,
    /// then for each configured store role, in the order of [`Role::ALL`], its `bucket=`, and for `hot` and
    /// `cold` its `account=` and the `policy=` that `provision` creates for it. Reads nothing and sends nothing; the held scripts compare it with
    /// their site file.
    #[must_use]
    pub fn plan_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("test_prefix={}", self.test_prefix)];
        for (&role, spec) in &self.stores {
            lines.push(format!("bucket={}", spec.bucket));
            if gets_account(role) {
                lines.push(format!("account={}", spec.access_key_id));
                lines.push(format!("policy={}", policy_name(&self.test_prefix, role)));
            }
        }
        lines
    }

    /// The spec of a store role.
    ///
    /// # Errors
    /// [`ConfigError::Missing`] if the run does not configure `role`.
    pub fn store(&self, role: Role) -> Result<&StoreSpec, ConfigError> {
        self.stores.get(&role).ok_or(ConfigError::Missing {
            role,
            field: "store configuration",
        })
    }
}

/// The name of the policy `provision` creates for `role`'s scoped account: the test prefix, then
/// `tierstore-bench-<role>`, so that two campaigns on one store never share (or overwrite) one.
#[must_use]
pub fn policy_name(test_prefix: &str, role: Role) -> String {
    format!("{test_prefix}tierstore-bench-{role}")
}

/// The last flag for `role`, else the run file's value.
fn flag_or(flags: &[(Role, String)], role: Role, file: Option<&String>) -> Option<String> {
    flags
        .iter()
        .rev()
        .find(|(r, _)| *r == role)
        .map(|(_, v)| v.clone())
        .or_else(|| file.cloned())
}

fn read_run_file(path: &Path) -> Result<RunFile, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::RunFileRead {
        path: path.to_owned(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| ConfigError::RunFileParse {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Wrap {
        #[command(flatten)]
        args: ConfigArgs,
    }

    /// The flags with no test prefix among them.
    fn bare(extra: &[&str]) -> ConfigArgs {
        let mut argv = vec!["t", "--result-dir", "out"];
        argv.extend_from_slice(extra);
        Wrap::try_parse_from(argv).unwrap().args
    }

    /// The flags with the test prefix `t-`.
    fn args(extra: &[&str]) -> ConfigArgs {
        let mut argv = vec!["--test-prefix", "t-"];
        argv.extend_from_slice(extra);
        bare(&argv)
    }

    fn file(json: &str) -> RunFile {
        serde_json::from_str(json).unwrap()
    }

    const FILE: &str = r#"{"stores": {
        "hot": {"endpoint": "http://hot.example:9000", "bucket": "t-hot-test",
                "access_key_id": "t-hot-id", "secret_access_key": "hot-secret"},
        "cold": {"access_key_id": "t-cold-id", "secret_access_key": "cold-secret",
                 "region": "eu-test-1"}}}"#;

    /// Catches a default that drifted from the approved caps and gate (200 GiB campaign, 8 GiB
    /// pass, 20 GiB cell, 100 and 2000 requests per second).
    #[test]
    fn the_defaults_are_the_approved_caps() {
        let cfg = RunConfig::resolve(&args(&[]), None).unwrap();
        assert_eq!(cfg.bytes.campaign, 200 * GIB);
        assert_eq!(cfg.bytes.pass, 8 * GIB);
        assert_eq!(cfg.bytes.cell, 20 * GIB);
        assert_eq!(cfg.rates.cold_rps, 100);
        assert_eq!(cfg.rates.hot_rps, 2000);
        assert_eq!(cfg.rates.list_poll_rps, 2);
        assert_eq!(cfg.window, Window::Busy);
        assert!(cfg.stores.is_empty());
    }

    /// Catches cold guard defaults other than 2,000 ms, 0.05 and 60 s, flags that do not reach the
    /// configuration, and a share outside 0 to 1 or a limit of zero that the parser takes.
    #[test]
    fn the_cold_guard_flags_have_their_defaults_and_reach_the_configuration() {
        let cfg = RunConfig::resolve(&args(&[]), None).unwrap();
        assert_eq!(cfg.cold_guard, ColdGuardLimits::default());
        assert_eq!(cfg.cold_guard.window, Duration::from_secs(60));
        assert_eq!(cfg.cold_guard.max_p99_ms, 30_000);
        assert_eq!(cfg.cold_guard.max_foreign_requests, 10);
        assert_eq!(cfg.cold_counter_settle, Duration::from_secs(11));
        let set = args(&[
            "--cold-abort-p99-ms",
            "750",
            "--cold-abort-error-share",
            "0.2",
            "--cold-abort-window-secs",
            "15",
            "--cold-abort-foreign-requests",
            "0",
            "--cold-counter-settle-secs",
            "0",
        ]);
        let cfg = RunConfig::resolve(&set, None).unwrap();
        assert_eq!(cfg.cold_guard.max_foreign_requests, 0);
        assert_eq!(cfg.cold_counter_settle, Duration::ZERO);
        assert_eq!(cfg.cold_guard.max_p99_ms, 750);
        assert!((cfg.cold_guard.max_error_share - 0.2).abs() < f64::EPSILON);
        assert_eq!(cfg.cold_guard.window, Duration::from_secs(15));
        for bad in [
            ["--cold-abort-error-share", "1.5"],
            ["--cold-abort-error-share", "-0.1"],
            ["--cold-abort-error-share", "many"],
            ["--cold-abort-p99-ms", "0"],
            ["--cold-abort-window-secs", "0"],
        ] {
            let mut argv = vec!["t", "--result-dir", "out"];
            argv.extend_from_slice(&bad);
            assert!(Wrap::try_parse_from(argv).is_err(), "{bad:?}");
        }
    }

    /// Catches a limit on the cold state reading other than 30 s, a flag that does not reach the
    /// configuration, and a limit of zero that the parser takes.
    #[test]
    fn the_cold_state_timeout_has_its_default_and_reaches_the_configuration() {
        let cfg = RunConfig::resolve(&args(&[]), None).unwrap();
        assert_eq!(cfg.cold_state_timeout, Duration::from_secs(30));
        let cfg = RunConfig::resolve(&args(&["--cold-state-timeout-secs", "5"]), None).unwrap();
        assert_eq!(cfg.cold_state_timeout, Duration::from_secs(5));
        let argv = ["t", "--result-dir", "out", "--cold-state-timeout-secs", "0"];
        assert!(Wrap::try_parse_from(argv).is_err());
    }

    /// Catches a freshness limit on the cold status file other than 30 s, a flag that does not
    /// reach the configuration, and a limit of zero that the parser takes.
    #[test]
    fn the_cold_state_max_age_has_its_default_and_reaches_the_configuration() {
        let cfg = RunConfig::resolve(&args(&[]), None).unwrap();
        assert_eq!(cfg.cold_state_max_age, Duration::from_secs(30));
        let cfg = RunConfig::resolve(&args(&["--cold-state-max-age-secs", "5"]), None).unwrap();
        assert_eq!(cfg.cold_state_max_age, Duration::from_secs(5));
        let argv = ["t", "--result-dir", "out", "--cold-state-max-age-secs", "0"];
        assert!(Wrap::try_parse_from(argv).is_err());
    }

    /// Catches a flag that does not override the run file, and a missing endpoint that is
    /// filled in silently.
    #[test]
    fn a_flag_overrides_the_run_file_and_the_file_fills_the_rest() {
        let a = args(&[
            "--endpoint",
            "cold=http://cold.example:9000",
            "--bucket",
            "cold=t-cold-test",
            "--endpoint",
            "hot=http://other.example:9000",
        ]);
        let cfg = RunConfig::resolve(&a, Some(file(FILE))).unwrap();
        let hot = cfg.store(Role::Hot).unwrap();
        assert_eq!(hot.endpoint, "http://other.example:9000");
        assert_eq!(hot.bucket, "t-hot-test");
        assert_eq!(hot.region, "us-east-1");
        let cold = cfg.store(Role::Cold).unwrap();
        assert_eq!(cold.endpoint, "http://cold.example:9000");
        assert_eq!(cold.region, "eu-test-1");
        assert_eq!(cold.access_key_id, "t-cold-id");
    }

    /// Catches a secret reaching a log through `Debug` of the configuration.
    #[test]
    fn debug_of_a_configuration_never_shows_a_secret() {
        let a = args(&[
            "--endpoint",
            "cold=http://cold.example:9000",
            "--bucket",
            "cold=t-cold-test",
        ]);
        let cfg = RunConfig::resolve(&a, Some(file(FILE))).unwrap();
        let shown = format!("{cfg:?}");
        assert!(!shown.contains("cold-secret"));
        assert!(!shown.contains("hot-secret"));
        assert!(shown.contains("<redacted>"));
        assert_eq!(
            cfg.store(Role::Cold).unwrap().secret_access_key.expose(),
            "cold-secret"
        );
    }

    /// Catches a role with no endpoint or no bucket being accepted: the run would start
    /// against nothing.
    #[test]
    fn a_store_without_an_endpoint_or_a_bucket_is_refused_by_role() {
        let err = RunConfig::resolve(&args(&[]), Some(file(FILE))).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::Missing {
                    role: Role::Cold,
                    field: "endpoint"
                }
            ),
            "{err}"
        );
        let a = args(&["--endpoint", "cold=http://cold.example:9000"]);
        let err = RunConfig::resolve(&a, Some(file(FILE))).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::Missing {
                    role: Role::Cold,
                    field: "bucket"
                }
            ),
            "{err}"
        );
    }

    /// Catches a key pair coming from anywhere but the run file: a role named by a flag alone
    /// has none.
    #[test]
    fn a_role_named_only_by_a_flag_has_no_key_pair() {
        let a = args(&[
            "--endpoint",
            "instance=http://i.example:1",
            "--bucket",
            "instance=t-b-test",
        ]);
        let err = RunConfig::resolve(&a, None).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::Missing {
                    role: Role::Instance,
                    ..
                }
            ),
            "{err}"
        );
        assert!(err.to_string().contains("instance"));
    }

    /// Catches a machine role being given a bucket.
    #[test]
    fn a_machine_role_is_not_a_store() {
        let a = args(&["--bucket", "generator=t-x-test"]);
        let err = RunConfig::resolve(&a, None).unwrap_err();
        assert!(matches!(err, ConfigError::NotAStore(Role::Generator)));
    }

    /// Catches caps out of order, where one pass could write more than its cell, or a cell more than the campaign.
    #[test]
    fn caps_out_of_order_are_refused() {
        for extra in [["--pass-cap-gib", "21"], ["--campaign-cap-gib", "19"]] {
            let err = RunConfig::resolve(&args(&extra), None).unwrap_err();
            assert!(matches!(err, ConfigError::CapOrder));
        }
    }

    /// Catches a zero cap or rate, which would stop a run or never limit it.
    #[test]
    fn a_zero_cap_or_rate_is_a_usage_error() {
        for flag in [
            "--cold-rps",
            "--hot-rps",
            "--list-poll-rps",
            "--pass-cap-gib",
        ] {
            let argv = ["t", "--result-dir", "out", flag, "0"];
            assert!(Wrap::try_parse_from(argv).is_err(), "{flag}");
        }
    }

    /// Catches a `ROLE=VALUE` flag without `=` or with a role that does not exist.
    #[test]
    fn a_malformed_pair_flag_is_a_usage_error() {
        assert!(matches!(parse_pair("hot"), Err(ConfigError::BadPair(_))));
        assert!(matches!(
            parse_pair("warm=x"),
            Err(ConfigError::UnknownRole(_))
        ));
        assert_eq!(
            parse_pair("hot=a=b").unwrap(),
            (Role::Hot, "a=b".to_owned())
        );
        let argv = ["t", "--result-dir", "out", "--endpoint", "hot"];
        assert!(Wrap::try_parse_from(argv).is_err());
    }

    /// Catches a misspelt key in the run file being ignored.
    #[test]
    fn an_unknown_field_in_the_run_file_is_refused() {
        let bad = r#"{"stores": {"hot": {"access_key_id": "a", "secret_access_key": "s", "secret": "x"}}}"#;
        assert!(serde_json::from_str::<RunFile>(bad).is_err());
        assert!(serde_json::from_str::<RunFile>(r#"{"store": {}}"#).is_err());
    }

    /// Catches a run with no test prefix, or a prefix that would not tell a test name from a
    /// farm's: none, an empty one, upper case, a dot, no closing dash and a bare dash are all
    /// refused before any store is looked at; the flag and the run file each give one.
    #[test]
    fn the_test_prefix_is_required_and_has_a_shape() {
        let err = RunConfig::resolve(&bare(&[]), None).unwrap_err();
        assert!(matches!(err, ConfigError::TestPrefix(_)), "{err}");
        for bad in ["", "Bench-", "bench.", "bench", "-", "ben ch-"] {
            let err = RunConfig::resolve(&bare(&["--test-prefix", bad]), None).unwrap_err();
            assert!(matches!(err, ConfigError::TestPrefix(_)), "{bad:?}: {err}");
        }
        let from_file = file(r#"{"test_prefix": "run7-"}"#);
        let cfg = RunConfig::resolve(&bare(&[]), Some(from_file)).unwrap();
        assert_eq!(cfg.test_prefix, "run7-");
        let from_file = file(r#"{"test_prefix": "run7-"}"#);
        let cfg = RunConfig::resolve(&args(&[]), Some(from_file)).unwrap();
        assert_eq!(cfg.test_prefix, "t-", "the flag wins over the run file");
    }

    /// Catches a bucket or a scoped account id outside the prefix being accepted: the error names
    /// the role and the value, for a bucket given by flag or file and for an id of `hot` or
    /// `cold`; an `instance` key is not an account the tool makes, so only its bucket counts.
    #[test]
    fn a_name_outside_the_prefix_is_refused_naming_it() {
        let a = args(&[
            "--endpoint",
            "cold=http://c.example:1",
            "--bucket",
            "cold=t-c",
            "--bucket",
            "hot=farm-hot",
        ]);
        let err = RunConfig::resolve(&a, Some(file(FILE))).unwrap_err();
        assert_eq!(
            err.to_string(),
            "role hot: the bucket \"farm-hot\" does not start with the test prefix \"t-\""
        );

        let live = FILE.replace("t-cold-id", "live-cold-id");
        let a = args(&[
            "--endpoint",
            "cold=http://c.example:1",
            "--bucket",
            "cold=t-c",
        ]);
        let err = RunConfig::resolve(&a, Some(file(&live))).unwrap_err();
        assert_eq!(
            err.to_string(),
            "role cold: the account id \"live-cold-id\" does not start with the test prefix \"t-\""
        );

        let instance = r#"{"stores": {"instance": {"endpoint": "http://i.example:1",
            "bucket": "t-i", "access_key_id": "tierstore-key", "secret_access_key": "s"}}}"#;
        RunConfig::resolve(&args(&[]), Some(file(instance))).unwrap();
        let outside = instance.replace("t-i", "i");
        let err = RunConfig::resolve(&args(&[]), Some(file(&outside))).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::OutsidePrefix {
                    role: Role::Instance,
                    ..
                }
            ),
            "{err}"
        );
    }

    /// Catches a hand-built configuration skipping the check that `resolve` makes: `check_names`
    /// on its own refuses a bucket that was changed after the fact.
    #[test]
    fn check_names_refuses_a_configuration_changed_after_resolving() {
        let a = args(&[
            "--endpoint",
            "cold=http://c.example:1",
            "--bucket",
            "cold=t-c",
        ]);
        let mut cfg = RunConfig::resolve(&a, Some(file(FILE))).unwrap();
        cfg.check_names().unwrap();
        cfg.stores.get_mut(&Role::Cold).unwrap().bucket = "live".to_owned();
        assert!(cfg.check_names().is_err());
    }

    /// Catches the plan listing something other than what a verb would touch, or a store call's
    /// worth of detail: the prefix, then each configured store's bucket in role order, and the
    /// account id of `hot` and `cold` only.
    #[test]
    fn the_plan_lists_the_prefix_the_buckets_and_the_accounts() {
        let instance = r#", "instance": {"endpoint": "http://i.example:1",
            "bucket": "t-inst", "access_key_id": "tierstore-key", "secret_access_key": "s"}}}"#;
        let with_instance = FILE[..FILE.len() - 2].to_owned() + instance;
        let a = args(&[
            "--endpoint",
            "cold=http://c.example:1",
            "--bucket",
            "cold=t-cold-test",
        ]);
        let cfg = RunConfig::resolve(&a, Some(file(&with_instance))).unwrap();
        assert_eq!(
            cfg.plan_lines(),
            [
                "test_prefix=t-",
                "bucket=t-hot-test",
                "account=t-hot-id",
                "policy=t-tierstore-bench-hot",
                "bucket=t-cold-test",
                "account=t-cold-id",
                "policy=t-tierstore-bench-cold",
                "bucket=t-inst",
            ]
        );
    }
}
