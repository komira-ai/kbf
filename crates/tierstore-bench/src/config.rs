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

use serde::Deserialize;

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
    /// The byte caps are not `cell <= pass <= campaign`.
    #[error("byte caps must satisfy cell <= pass <= campaign")]
    CapOrder,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// The store roles in use, each complete.
    pub stores: BTreeMap<Role, StoreSpec>,
    /// Declared by the operator.
    pub window: Window,
    /// Content seed.
    pub seed: u64,
    /// The load gate's limits.
    pub gate: LoadGate,
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
        if bytes.cell > bytes.pass || bytes.pass > bytes.campaign {
            return Err(ConfigError::CapOrder);
        }
        let file = file.unwrap_or_default();
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
        Ok(Self {
            result_dir: args.result_dir.clone(),
            stores,
            window: args.window,
            seed: args.seed,
            gate: LoadGate {
                max_loadavg_1m: args.max_loadavg_1m,
                max_psi_cpu_avg10: args.max_psi_cpu_avg10,
                max_psi_io_avg10: args.max_psi_io_avg10,
                max_disk_util_pct: args.max_disk_util_pct,
            },
            rates: RateCaps {
                cold_rps: args.cold_rps,
                hot_rps: args.hot_rps,
                list_poll_rps: args.list_poll_rps,
            },
            bytes,
        })
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

    fn args(extra: &[&str]) -> ConfigArgs {
        let mut argv = vec!["t", "--result-dir", "out"];
        argv.extend_from_slice(extra);
        Wrap::try_parse_from(argv).unwrap().args
    }

    fn file(json: &str) -> RunFile {
        serde_json::from_str(json).unwrap()
    }

    const FILE: &str = r#"{"stores": {
        "hot": {"endpoint": "http://hot.example:9000", "bucket": "hot-test",
                "access_key_id": "hot-id", "secret_access_key": "hot-secret"},
        "cold": {"access_key_id": "cold-id", "secret_access_key": "cold-secret",
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

    /// Catches a flag that does not override the run file, and a missing endpoint that is
    /// filled in silently.
    #[test]
    fn a_flag_overrides_the_run_file_and_the_file_fills_the_rest() {
        let a = args(&[
            "--endpoint",
            "cold=http://cold.example:9000",
            "--bucket",
            "cold=cold-test",
            "--endpoint",
            "hot=http://other.example:9000",
        ]);
        let cfg = RunConfig::resolve(&a, Some(file(FILE))).unwrap();
        let hot = cfg.store(Role::Hot).unwrap();
        assert_eq!(hot.endpoint, "http://other.example:9000");
        assert_eq!(hot.bucket, "hot-test");
        assert_eq!(hot.region, "us-east-1");
        let cold = cfg.store(Role::Cold).unwrap();
        assert_eq!(cold.endpoint, "http://cold.example:9000");
        assert_eq!(cold.region, "eu-test-1");
        assert_eq!(cold.access_key_id, "cold-id");
    }

    /// Catches a secret reaching a log through `Debug` of the configuration.
    #[test]
    fn debug_of_a_configuration_never_shows_a_secret() {
        let a = args(&[
            "--endpoint",
            "cold=http://cold.example:9000",
            "--bucket",
            "cold=cold-test",
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
            "instance=b-test",
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
        let a = args(&["--bucket", "generator=x-test"]);
        let err = RunConfig::resolve(&a, None).unwrap_err();
        assert!(matches!(err, ConfigError::NotAStore(Role::Generator)));
    }

    /// Catches caps out of order, where a cell could write more than its pass or the campaign.
    #[test]
    fn caps_out_of_order_are_refused() {
        for extra in [["--cell-cap-gib", "9"], ["--campaign-cap-gib", "7"]] {
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
}
