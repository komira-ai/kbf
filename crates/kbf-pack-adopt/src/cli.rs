//! The command line, and the checks made of it before anything is read or written.

use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, ValueEnum};
use kbf_objstore::footers::FooterReads;
use kbf_objstore::s3::{S3Config, S3ConfigError, S3Store};
use kbf_objstore::{Capabilities, KeyPrefix};
use kbf_pack::{AdoptConfig, AdoptMode};
use kbf_state::postgres::PostgresConfig;

use crate::credentials;
use crate::error::Error;

/// How many blobs a dry run reads back unless `--verify-sample` says another number.
pub const DRY_RUN_SAMPLE: u64 = 20;

/// The most `--footer-concurrency` takes: each footer in flight is a request to the members'
/// store.
const MAX_FOOTER_CONCURRENCY: usize = 64;

const EXIT_STATUS: &str = "Exit status: 0 done (or stopped at --limit-segments), 2 refused \
     (nothing was changed), 1 failed.";

const FILE_SOURCE: &str = "--source-store-endpoint: a file:// source is refused, as a local \
     store evicts the segments a pack would point at; give the http:// endpoint of the \
     members' S3-compatible store";

const NEEDS_PACK_BUCKET: &str = "--mode apply needs --pack-store-bucket, the bucket the \
     frontends write their packs to, and it must be --source-store-bucket: segments are \
     adopted where they lie";

const TOO_MANY_FOOTERS: &str = "--footer-concurrency: at most 64 footers can be read at once";

const PREFIX_SLASH: &str = "--source-prefix must be empty or end in /";

const BAD_INDEX_URL: &str = "--pack-index: not a postgres:// URL this client can use (it \
     needs a host and no TLS), as in postgres:///kbf?host=/run/postgresql";

const INDEX_PASSWORD: &str = "--pack-index: the URL must carry no password (a command line \
     is world-readable); use the Unix socket with peer authentication, as in \
     postgres:///kbf?host=/run/postgresql";

/// What a run does with the segments.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    /// Report what an apply would record, and record nothing.
    DryRun,
    /// Record each segment as a pack in the pack index.
    Apply,
}

impl Mode {
    fn adopt(self) -> AdoptMode {
        match self {
            Mode::DryRun => AdoptMode::DryRun,
            Mode::Apply => AdoptMode::Apply,
        }
    }

    /// The `--verify-sample` of a run that does not give one.
    fn default_sample(self) -> u64 {
        match self {
            Mode::DryRun => DRY_RUN_SAMPLE,
            Mode::Apply => 0,
        }
    }
}

/// Adopts the segments that storage members flushed to the cold store into the pack index, in
/// place, so that a read finds their blobs through the index.
///
/// The tool lists the members' bucket, reads the footer of each segment and records one pack
/// per segment under the segment's own key. It copies no byte, writes nothing to the bucket and
/// deletes nothing. Without `--mode apply` it is a dry run that writes nothing to the index
/// either (only the checkpoint and report files you name): it reports the packs and rows that an apply would add, and reads a sample of blobs
/// back to check the footers against the bytes. An apply is repeatable: a pack already recorded
/// changes nothing. Run it after the frontends write packs, and again until it adds nothing.
#[derive(Debug, Parser)]
#[command(name = "kbf-pack-adopt", version, after_help = EXIT_STATUS)]
pub struct Args {
    /// The members' S3-compatible object store, as `http://host:port`. The tool only reads
    /// from it. A `file://` store is refused.
    #[arg(long, value_name = "URL")]
    pub source_store_endpoint: String,

    /// The bucket that holds the members' segments. With `--mode apply` it must be the bucket
    /// given as `--pack-store-bucket`.
    #[arg(long, value_name = "BUCKET")]
    pub source_store_bucket: String,

    /// The prefix of the members' keys in the bucket: empty, or ending in `/`. Segments are
    /// found under `<prefix>cas/<member>/<boot>/<seq>`, `<prefix>cas/<epoch>/<seq>` and
    /// `<prefix><log>/cas/<epoch>/<seq>`; any other key is counted and left alone.
    #[arg(long, value_name = "PREFIX", default_value = "kbf/")]
    pub source_prefix: String,

    /// The region the requests to `--source-store-endpoint` are signed for.
    #[arg(long, value_name = "REGION", default_value = "us-east-1")]
    pub source_store_region: String,

    /// A file with the key pair for `--source-store-endpoint`, as JSON
    /// `{"access_key_id": "...", "secret_access_key": "..."}`, readable by its owner alone
    /// (mode 0600 or 0400). The secret is never a flag: a command line is world-readable.
    #[arg(long, value_name = "PATH")]
    pub source_store_credentials_file: PathBuf,

    /// The Postgres state store that holds the pack index, as a `postgres://` URL with no
    /// password, such as `postgres:///kbf?host=/run/postgresql`: the tool connects as the user
    /// that runs it. Its schema must have the pack tables.
    #[arg(long, value_name = "URL")]
    pub pack_index: String,

    /// `dry-run` reports and writes nothing to the index; `apply` records the packs.
    #[arg(long, value_enum, default_value_t = Mode::DryRun)]
    pub mode: Mode,

    /// The bucket the frontends write their packs to (their `--pack-store-bucket`). Needed
    /// with `--mode apply`, which is refused unless it is `--source-store-bucket`: a pack is
    /// recorded where the segment lies, and readers look for packs in the pack bucket.
    #[arg(long, value_name = "BUCKET")]
    pub pack_store_bucket: Option<String>,

    /// A file the tool keeps the report so far in (written beside it and renamed over it), and
    /// resumes from: a run that stopped part-way goes on after the last key it handled. A
    /// checkpoint of a run that finished is ignored, so the same command line runs again from
    /// the start. A checkpoint of another mode, prefix or source store is refused.
    #[arg(long, value_name = "PATH")]
    pub checkpoint_file: Option<PathBuf>,

    /// The most segments handled a second. Without it the tool goes as fast as the store and
    /// the index answer.
    #[arg(long, value_name = "N")]
    pub segments_per_second: Option<NonZeroU32>,

    /// How many segment footers are read from the store at once, at most 64.
    #[arg(long, value_name = "N", default_value = "4")]
    pub footer_concurrency: NonZeroUsize,

    /// Stop after this many segments; the report says where. Run again with the same
    /// `--checkpoint-file` to go on.
    #[arg(long, value_name = "N")]
    pub limit_segments: Option<u64>,

    /// How many blobs to read back from the store and hash, one from each of the first
    /// segments, to catch a footer that does not match its bytes. A segment with a blob that
    /// fails is not recorded. Defaults to 20 in a dry run and to 0 with `--mode apply`.
    #[arg(long, value_name = "N")]
    pub verify_sample: Option<u64>,

    /// Write the report, as JSON, to this file when the run ends (written beside it and
    /// renamed over it).
    #[arg(long, value_name = "PATH")]
    pub report_json: Option<PathBuf>,

    /// End the run, as a failure, at a segment whose key the index holds as a pack with other
    /// contents. Without it a conflict is counted and listed in the report, and the run goes on.
    #[arg(long)]
    pub fail_on_conflict: bool,
}

/// What the flags came to, once checked.
pub struct Plan {
    /// The source store, built (nothing is sent to it yet).
    pub store: S3Store,
    /// Where the pack index is.
    pub index: PostgresConfig,
    /// How the adoption runs.
    pub adopt: AdoptConfig,
    /// `--checkpoint-file`.
    pub checkpoint_file: Option<PathBuf>,
    /// `--report-json`.
    pub report_json: Option<PathBuf>,
}

impl Args {
    /// Checks the flags and the credentials file, and builds what a run needs from them.
    ///
    /// # Errors
    /// [`Error::Refused`]: the source is a `file://` store, `--mode apply` lacks the pack
    /// bucket or names another one, `--footer-concurrency` is over 64, the prefix, the index
    /// URL, the credentials file or the store is not acceptable.
    pub fn plan(&self) -> Result<Plan, Error> {
        if self.source_store_endpoint.starts_with("file://") {
            return Err(Error::refused(FILE_SOURCE));
        }
        if self.mode == Mode::Apply {
            self.check_pack_bucket()?;
        }
        self.check_footer_concurrency()?;
        let prefix = self.prefix()?;
        let index = self.index_config()?;
        let store = self.source_store()?;
        let mut adopt = AdoptConfig::new(self.mode.adopt(), prefix);
        adopt.source = self.source_name();
        adopt.reads = FooterReads {
            concurrency: self.footer_concurrency,
            ..FooterReads::default()
        };
        adopt.limit_segments = self.limit_segments;
        adopt.segments_per_second = self.segments_per_second;
        adopt.verify_sample = self.verify_sample.unwrap_or(self.mode.default_sample());
        adopt.fail_on_conflict = self.fail_on_conflict;
        Ok(Plan {
            store,
            index,
            adopt,
            checkpoint_file: self.checkpoint_file.clone(),
            report_json: self.report_json.clone(),
        })
    }

    /// `--mode apply` records packs where the segments lie, so the pack bucket the frontends
    /// use must be that bucket.
    fn check_pack_bucket(&self) -> Result<(), Error> {
        match self.pack_store_bucket.as_deref() {
            None => Err(Error::refused(NEEDS_PACK_BUCKET)),
            Some(bucket) if bucket != self.source_store_bucket => Err(differs(bucket, self)),
            Some(_) => Ok(()),
        }
    }

    /// The store the run lists, as a checkpoint names it: the endpoint and the bucket.
    fn source_name(&self) -> String {
        let endpoint = &self.source_store_endpoint;
        let bucket = &self.source_store_bucket;
        format!("{endpoint}/{bucket}")
    }

    fn check_footer_concurrency(&self) -> Result<(), Error> {
        if self.footer_concurrency.get() > MAX_FOOTER_CONCURRENCY {
            return Err(Error::refused(TOO_MANY_FOOTERS));
        }
        Ok(())
    }

    fn prefix(&self) -> Result<KeyPrefix, Error> {
        let text = self.source_prefix.as_str();
        if !matches!(text.chars().next_back(), None | Some('/')) {
            return Err(Error::refused(PREFIX_SLASH));
        }
        KeyPrefix::new(text).map_err(|error| Error::refused(format!("--source-prefix: {error}")))
    }

    fn index_config(&self) -> Result<PostgresConfig, Error> {
        let parsed = PostgresConfig::parse(&self.pack_index);
        // The parser's own message may quote the URL: none is kept.
        let config = parsed.map_err(|_| Error::refused(BAD_INDEX_URL))?;
        if config.has_password() {
            return Err(Error::refused(INDEX_PASSWORD));
        }
        let config = config.with_application_name("kbf-pack-adopt");
        Ok(config.with_max_connections(2))
    }

    fn source_store(&self) -> Result<S3Store, Error> {
        let file = &self.source_store_credentials_file;
        let credentials = credentials::read(file).map_err(|why| bad_credentials(file, &why))?;
        let config = S3Config {
            endpoint: self.source_store_endpoint.clone(),
            region: self.source_store_region.clone(),
            bucket: self.source_store_bucket.clone(),
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
    Error::refused(format!(
        "--source-store-endpoint or --source-store-bucket: {error}"
    ))
}

/// The refusal of `--mode apply` with a pack bucket that is not the source bucket.
fn differs(bucket: &str, args: &Args) -> Error {
    let source = &args.source_store_bucket;
    Error::refused(format!(
        "--mode apply: --pack-store-bucket {bucket} is not --source-store-bucket {source}; \
         segments are adopted where they lie, so the packs' bucket must be the segments' bucket"
    ))
}

fn bad_credentials(path: &Path, why: &str) -> Error {
    let shown = path.display();
    Error::refused(format!("--source-store-credentials-file {shown}: {why}"))
}
