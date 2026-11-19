//! The command line, and the checks made of it before anything is read or written.

use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, ValueEnum};
use kbf_objstore::Capabilities;
use kbf_objstore::s3::{S3Config, S3ConfigError, S3Store};
use kbf_state::postgres::PostgresConfig;
use serde::Serialize;

use crate::credentials;
use crate::error::Error;

const SECONDS_PER_DAY: u64 = 86_400;

const EXIT_STATUS: &str = "Exit status: 0 done (or stopped at --limit-trees), 2 refused \
     (nothing was changed), 1 failed (a call failed, or a tree failed: the report says which).";

const FILE_STORE: &str = "--store-endpoint: a file:// store is refused; give the http:// \
     endpoint of the S3-compatible store the frontends write their packs and tree lists to";

const BAD_INDEX_URL: &str = "--pack-index: not a postgres:// URL this client can use (it \
     needs a host, and an sslmode other than allow), as in postgres:///kbf?host=/run/postgresql";

const INDEX_PASSWORD: &str = "--pack-index: the URL must carry no password (a command line \
     is world-readable); use the Unix socket with peer authentication, as in \
     postgres:///kbf?host=/run/postgresql";

/// What a run does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Report what an apply would do; read the index and ask the store whether each list is
    /// there, and write nothing.
    DryRun,
    /// Read the objects of the trees, record their cells and write the sidecars.
    Apply,
}

/// Gives the tree lists recorded unlisted their L2 sidecar, and samples the live keys
/// of the pack index for `tierstore store audit --keys`.
///
/// The backfill takes the trees the pack index holds as unlisted (a list was written and its
/// sidecar could not be, as an object it names had no recorded cells) that were recorded within
/// `--within-days`, and for each reads the list, reads once every object it names that has no
/// cells, records the cells, writes the sidecar `cas/l2/<id>` create-only, and drops the tree's
/// unlisted row. Run it off-peak: it reads the packs.
///
/// Without `--mode apply` a run is a dry run: it reads the index and asks the store whether
/// each list is still there, and writes nothing to either (only the files you name).
///
/// `--audit-keys-out` writes a uniform random sample of the keys of the packs the index holds
/// live, one key per line. With `--mode dry-run` and `--audit-keys-out` the run takes the sample
/// alone and does no backfill pass; with `--mode apply` it backfills and then takes the sample.
/// `--audit-keys` and `--audit-seed` need `--audit-keys-out`.
#[derive(Debug, Parser)]
#[command(name = "kbf-l2-backfill", version, after_help = EXIT_STATUS)]
pub struct Args {
    /// The S3-compatible object store the frontends write their packs and tree lists to, as
    /// `http://host:port`. A `file://` store is refused.
    #[arg(long, value_name = "URL")]
    pub store_endpoint: String,

    /// The bucket of the frontends' `--pack-store-bucket`: the tree lists are `cas/l/<id>`
    /// in it, and the sidecars are written to `cas/l2/<id>`.
    #[arg(long, value_name = "BUCKET")]
    pub store_bucket: String,

    /// The region the requests to `--store-endpoint` are signed for.
    #[arg(long, value_name = "REGION", default_value = "us-east-1")]
    pub store_region: String,

    /// A file with the key pair for `--store-endpoint`, as JSON
    /// `{"access_key_id": "...", "secret_access_key": "..."}`, readable by its owner alone
    /// (mode 0600 or 0400). The secret is never a flag: a command line is world-readable. A
    /// dry run needs a key that may read; an apply, one that may read and create objects.
    #[arg(long, value_name = "PATH")]
    pub store_credentials_file: PathBuf,

    /// The Postgres state store that holds the pack index, as a `postgres://` URL with no
    /// password, such as `postgres:///kbf?host=/run/postgresql`: the tool connects as the user
    /// that runs it. Its schema must have the cell and unlisted-tree tables (migration 5).
    #[arg(long, value_name = "URL")]
    pub pack_index: String,

    /// `dry-run` writes nothing; `apply` reads the objects and writes the sidecars.
    #[arg(long, value_enum, default_value_t = Mode::DryRun)]
    pub mode: Mode,

    /// Take the unlisted trees last recorded within this many days.
    #[arg(long, value_name = "N", default_value = "14")]
    pub within_days: NonZeroU32,

    /// The most trees handled a second. Each is a read of the list and, in an apply, of the
    /// objects it names that have no cells: start low on a live farm. Without it the tool goes
    /// as fast as the store and the index answer.
    #[arg(long, value_name = "N")]
    pub trees_per_second: Option<NonZeroU32>,

    /// Stop after this many trees; the report says so. Run again to go on: a tree done is no
    /// longer unlisted.
    #[arg(long, value_name = "N")]
    pub limit_trees: Option<u64>,

    /// Write the report, as JSON, to this file when the run ends (written beside it and
    /// renamed over it).
    #[arg(long, value_name = "PATH")]
    pub report_json: Option<PathBuf>,

    /// Write a uniform random sample of the live pack keys of the pack index to this file: one
    /// key per line, sorted, each ended by a newline, for `tierstore store audit --keys`.
    /// Alone with `--mode dry-run`; after the backfill with `--mode apply`.
    #[arg(long, value_name = "PATH")]
    pub audit_keys_out: Option<PathBuf>,

    /// How many keys the sample holds (all of them when fewer are live).
    #[arg(
        long,
        value_name = "N",
        default_value = "1000",
        requires = "audit_keys_out"
    )]
    pub audit_keys: NonZeroUsize,

    /// The seed of the sample: the same seed over the same live keys gives the same sample.
    /// Without it the seed is taken from the clock; the report says which was used.
    #[arg(long, value_name = "N", requires = "audit_keys_out")]
    pub audit_seed: Option<u64>,
}

/// The sample to take.
#[derive(Debug, PartialEq, Eq)]
pub struct AuditPlan {
    /// The file to write.
    pub out: PathBuf,
    /// How many keys.
    pub keys: NonZeroUsize,
    /// The seed, if given.
    pub seed: Option<u64>,
}

/// What the flags came to, once checked.
pub struct Plan {
    /// The store, built (nothing is sent to it yet).
    pub store: S3Store,
    /// Where the pack index is.
    pub index: PostgresConfig,
    /// What the run does.
    pub mode: Mode,
    /// How far back the unlisted trees are taken from.
    pub within: Duration,
    /// `--within-days`.
    pub within_days: u32,
    /// `--trees-per-second`.
    pub rate: Option<NonZeroU32>,
    /// `--limit-trees`.
    pub limit_trees: Option<u64>,
    /// `--report-json`.
    pub report_json: Option<PathBuf>,
    /// The sample, if one is asked for.
    pub audit: Option<AuditPlan>,
}

impl Plan {
    /// Whether the run has a backfill pass: always with `--mode apply`, and in a dry run unless
    /// the run is only the sample.
    #[must_use]
    pub fn backfills(&self) -> bool {
        self.mode == Mode::Apply || self.audit.is_none()
    }
}

impl Args {
    /// Checks the flags and the credentials file, and builds what a run needs from them.
    ///
    /// # Errors
    /// [`Error::Refused`]: the store is a `file://` store, `--limit-trees` is 0, the index URL,
    /// the credentials file or the store is not acceptable.
    pub fn plan(&self) -> Result<Plan, Error> {
        if self.store_endpoint.starts_with("file://") {
            return Err(Error::refused(FILE_STORE));
        }
        if self.limit_trees == Some(0) {
            return Err(Error::refused("--limit-trees: 0 would handle no tree"));
        }
        let index = self.index_config()?;
        let store = self.store()?;
        let days = self.within_days.get();
        Ok(Plan {
            store,
            index,
            mode: self.mode,
            within: Duration::from_secs(u64::from(days) * SECONDS_PER_DAY),
            within_days: days,
            rate: self.trees_per_second,
            limit_trees: self.limit_trees,
            report_json: self.report_json.clone(),
            audit: self.audit_keys_out.as_ref().map(|out| AuditPlan {
                out: out.clone(),
                keys: self.audit_keys,
                seed: self.audit_seed,
            }),
        })
    }

    fn index_config(&self) -> Result<PostgresConfig, Error> {
        let parsed = PostgresConfig::parse(&self.pack_index);
        // The parser's own message may quote the URL: none is kept.
        let config = parsed.map_err(|_| Error::refused(BAD_INDEX_URL))?;
        if config.has_password() {
            return Err(Error::refused(INDEX_PASSWORD));
        }
        let config = config.with_application_name("kbf-l2-backfill");
        Ok(config.with_max_connections(2))
    }

    fn store(&self) -> Result<S3Store, Error> {
        let file = &self.store_credentials_file;
        let credentials = credentials::read(file).map_err(|why| bad_credentials(file, &why))?;
        let config = S3Config {
            endpoint: self.store_endpoint.clone(),
            region: self.store_region.clone(),
            bucket: self.store_bucket.clone(),
            credentials,
            capabilities: Capabilities::default(),
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(60),
            min_body_rate: None,
        };
        S3Store::new(config).map_err(bad_store)
    }
}

fn bad_store(error: S3ConfigError) -> Error {
    Error::refused(format!("--store-endpoint or --store-bucket: {error}"))
}

fn bad_credentials(path: &Path, why: &str) -> Error {
    let shown = path.display();
    Error::refused(format!("--store-credentials-file {shown}: {why}"))
}
