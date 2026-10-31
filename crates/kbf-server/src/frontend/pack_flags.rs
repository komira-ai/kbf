//! The flags that keep the frontend's blobs in packs: `--pack-store-endpoint` and its
//! companions, and the [`Packs`] they open over the pack index of `--pack-index`.
//!
//! They are the frontend's own and not `kbf-server`'s. All are off unless
//! `--pack-store-endpoint` is given, and then a frontend serves exactly as it did: no object
//! store is built or contacted. The endpoint needs `--pack-index`, the bucket and the
//! credentials file, which clap enforces (so does the file of `kbf.toml`, which becomes flags).
//! The companions that have a default (`--pack-store-region`, `--pack-bytes`, `--pack-writer`,
//! `--pack-inflight-bytes`, `--pack-upload-idle-timeout`, `--pack-commit-sample`) need nothing,
//! since the config template writes every flag with a default as a live line.
//!
//! With the endpoint and `--pack-index` the frontend also takes a daemon's `CommitPack`
//! ([`crate::pack_locate`]): `--pack-commit-sample` says how many blobs of a committed pack it
//! reads back and checks ([`commits`]).
//!
//! The credentials are a file and never a flag, as a command line is world-readable: JSON with
//! `access_key_id` and `secret_access_key`, readable by its owner alone (the reader is
//! [`crate::key_file`], which `--cold-store-credentials-file` uses too).
//!
//! Opening the packs sends nothing to the store. [`super::FrontendHandle::serve`] first checks
//! that the store refuses to replace an object, and does not serve if it does not.

use std::error::Error;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Arg, ArgMatches};
use kbf_front::{PackSettings, Packs};
use kbf_objstore::Capabilities;
use kbf_objstore::s3::{Credentials, S3Config, S3ConfigError, S3Store};
use kbf_pack::{CacheError, PackError, PackKeys, ReadCache, WriterKind};
use kbf_pack_index::PackIndex;

use crate::key_file::read_key_pair;
use crate::pack_locate::PackCommits;

const ENDPOINT: &str = "pack_store_endpoint";
const BUCKET: &str = "pack_store_bucket";
const REGION: &str = "pack_store_region";
const CREDENTIALS: &str = "pack_store_credentials_file";
const PACK_BYTES: &str = "pack_bytes";
const WRITER: &str = "pack_writer";
const CACHE_DIR: &str = "pack_cache_dir";
const CACHE_BYTES: &str = "pack_cache_bytes";
const INFLIGHT_BYTES: &str = "pack_inflight_bytes";
const UPLOAD_IDLE_TIMEOUT: &str = "pack_upload_idle_timeout";
const COMMIT_SAMPLE: &str = "pack_commit_sample";

/// The id of `--pack-index`, which the endpoint needs.
const PACK_INDEX: &str = "pack_index";

/// The signing region when `--pack-store-region` is left out: the one S3-compatible stores
/// accept unless they are configured to another.
const DEFAULT_REGION: &str = "us-east-1";

/// The name of a frontend in its pack keys when `--pack-writer` is left out.
const DEFAULT_WRITER: &str = "frontend";

/// The slowest rate, in bytes a second, at which a pack may reach the store before the
/// request is given up on: 1 MiB/s.
const MIN_PUT_RATE: u64 = 1 << 20;

/// The least `--pack-inflight-bytes`: the largest blob the packs take (64 MiB), so that no
/// upload they accept is refused for its size alone.
const MIN_INFLIGHT_BYTES: i64 = 64 << 20;

/// The longest `--pack-upload-idle-timeout`, in seconds: a day, the longest the packs keep.
const MAX_IDLE_SECS: u64 = kbf_front::MAX_UPLOAD_IDLE_TIMEOUT.as_secs();

/// The most `--pack-commit-sample` takes: the most blobs a pack holds
/// ([`kbf_front::MAX_COMMIT_BLOBS`]), at which every blob of a pack is checked.
const MAX_COMMIT_SAMPLE: i64 = 16_384;

/// The eleven flags.
pub(super) fn args() -> [Arg; 11] {
    [
        Arg::new(ENDPOINT)
            .long("pack-store-endpoint")
            .value_name("URL")
            .value_parser(clap::value_parser!(String))
            .requires_all([BUCKET, CREDENTIALS, PACK_INDEX])
            .help(
                "Keeps the blobs clients upload in packs (many blobs in one object) in this \
                 S3-compatible object store, as `http://host:port`: a pack is written with a \
                 create-only request and recorded in the pack index before the upload is \
                 acknowledged, and a read asks the index first and fetches only the blob's \
                 bytes from its pack. A blob the index lacks is read from the storage nodes \
                 as before, and a blob over 64 MiB and any compressed blob still go to them. \
                 At start the frontend checks that the store refuses to replace an object \
                 and does not serve if it does not. Needs `--pack-index`, \
                 `--pack-store-bucket` and `--pack-store-credentials-file`. Daemons read blobs \
                 from the storage nodes only, so a farm must not enable this before they can \
                 read a blob that is only in a pack. Left out, no pack store is built or \
                 contacted and blobs go where they always did",
            ),
        Arg::new(BUCKET)
            .long("pack-store-bucket")
            .value_name("BUCKET")
            .value_parser(clap::value_parser!(String))
            .requires(ENDPOINT)
            .help(
                "The bucket of `--pack-store-endpoint` that holds the packs, under the keys \
                 `cas/f/<--pack-writer>/<start>/<sequence>`",
            ),
        Arg::new(REGION)
            .long("pack-store-region")
            .value_name("REGION")
            .value_parser(clap::value_parser!(String))
            .default_value(DEFAULT_REGION)
            .help("The region the requests to `--pack-store-endpoint` are signed for"),
        Arg::new(CREDENTIALS)
            .long("pack-store-credentials-file")
            .value_name("PATH")
            .value_parser(clap::value_parser!(PathBuf))
            .requires(ENDPOINT)
            .help(
                "A file with the key pair for `--pack-store-endpoint`, as JSON \
                 `{\"access_key_id\": \"...\", \"secret_access_key\": \"...\"}`, readable by \
                 its owner alone (mode 0600 or 0400). The secret is never a flag: a command \
                 line is world-readable",
            ),
        Arg::new(PACK_BYTES)
            .long("pack-bytes")
            .value_name("BYTES")
            .value_parser(clap::value_parser!(u64).range(1..))
            // FRONTEND_PACK_BYTES, which clap cannot take as a computed string; the default-flags
            // test parses it back and compares it with the constant.
            .default_value("33554432")
            .help(
                "The length, in bytes, at which a pack of client uploads is closed: the blobs \
                 of one batch or upload that do not fit are written as the next pack. Used \
                 only with `--pack-store-endpoint`",
            ),
        Arg::new(INFLIGHT_BYTES)
            .long("pack-inflight-bytes")
            .value_name("BYTES")
            .value_parser(clap::value_parser!(u32).range(MIN_INFLIGHT_BYTES..))
            // DEFAULT_INFLIGHT_BYTES (1 GiB), which clap cannot take as a computed string; the
            // default-flags test parses it back and compares it with the constant.
            .default_value("1073741824")
            .help(
                "The most bytes of client uploads into packs this frontend holds in flight at \
                 once, at least 67108864 (the largest blob that is packed, so that no upload is \
                 refused for its size) and at most 4294967295. Each upload takes its blob's size \
                 as soon as its resource name has arrived, before it reads on, and gives it back \
                 when it ends; one that finds the rest taken waits its turn, so a burst of \
                 uploads is slowed down, not failed. The bound counts blob bytes, not the memory \
                 of the process: a batch upload is decoded before it waits, so the bytes it \
                 holds meanwhile are not counted, and reads are not counted. Used only with \
                 `--pack-store-endpoint`",
            ),
        Arg::new(UPLOAD_IDLE_TIMEOUT)
            .long("pack-upload-idle-timeout")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64).range(1..=MAX_IDLE_SECS))
            .default_value("30")
            .help(
                "How long, in seconds, a ByteStream upload into packs may go without a byte of \
                 its body arriving before it is ended DEADLINE_EXCEEDED and gives back its \
                 bytes, so that a client that stops sending cannot hold room for ever. The \
                 time before its resource name has arrived is not counted. An upload that keeps \
                 sending, however slowly, is never cut and keeps its bytes until it ends, so \
                 `--pack-inflight-bytes` is the only bound on that. At least 1, at most 86400. \
                 Used only with `--pack-store-endpoint`",
            ),
        Arg::new(COMMIT_SAMPLE)
            .long("pack-commit-sample")
            .value_name("BLOBS")
            .value_parser(clap::value_parser!(u32).range(0..=MAX_COMMIT_SAMPLE))
            .default_value("2")
            .help(
                "How many blobs of each pack a daemon commits this frontend reads back from the \
                 pack store and checks against their digests before it records the pack, spread \
                 over the pack from a start that its name picks (the same commit is always \
                 checked at the same blobs). A pack with fewer blobs is checked whole, and \
                 16384, the most blobs a pack holds, checks every blob. 0 checks no blob and \
                 trusts the daemon: a blob that is not its digest is then caught only when it is \
                 first read. The last byte of the pack is read in any case, so a pack that is \
                 not stored is always refused. Used only with `--pack-store-endpoint`",
            ),
        Arg::new(WRITER)
            .long("pack-writer")
            .value_name("NAME")
            .value_parser(clap::value_parser!(String))
            .default_value(DEFAULT_WRITER)
            .help(
                "The name of this frontend in the keys of its packs and in the pack index's \
                 record of who wrote each: give each frontend of a farm one of its own. Used \
                 only with `--pack-store-endpoint`",
            ),
        Arg::new(CACHE_DIR)
            .long("pack-cache-dir")
            .value_name("DIR")
            .value_parser(clap::value_parser!(PathBuf))
            .requires_all([CACHE_BYTES, ENDPOINT])
            .help(
                "A directory this process owns where the blobs read from packs are kept, one \
                 file each, checked by digest on every read; the least recently used are \
                 removed to stay within `--pack-cache-bytes`. Left out, nothing is kept",
            ),
        Arg::new(CACHE_BYTES)
            .long("pack-cache-bytes")
            .value_name("BYTES")
            .value_parser(clap::value_parser!(u64).range(1..))
            .requires(CACHE_DIR)
            .help("The most bytes of blobs `--pack-cache-dir` holds"),
    ]
}

/// What the pack flags say, when `--pack-store-endpoint` is given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackFlags {
    /// `--pack-store-endpoint`.
    pub endpoint: String,
    /// `--pack-store-bucket`.
    pub bucket: String,
    /// `--pack-store-region`.
    pub region: String,
    /// `--pack-store-credentials-file`.
    pub credentials_file: PathBuf,
    /// `--pack-bytes`.
    pub pack_bytes: u64,
    /// `--pack-inflight-bytes`.
    pub inflight_bytes: u32,
    /// `--pack-upload-idle-timeout`.
    pub upload_idle_timeout: Duration,
    /// `--pack-writer`.
    pub writer: String,
    /// `--pack-cache-dir` and `--pack-cache-bytes`, both or neither.
    pub cache: Option<(PathBuf, u64)>,
    /// `--pack-commit-sample`.
    pub commit_sample: usize,
}

/// The setting the flags in `m` make: `None` unless `--pack-store-endpoint` is given.
pub(super) fn from_matches(m: &ArgMatches) -> Option<PackFlags> {
    let endpoint = m.get_one::<String>(ENDPOINT)?;
    // clap makes the bucket and the credentials come with the endpoint, and the two flags of
    // the cache come together; the others have a default.
    let text = |id: &str| m.get_one::<String>(id).cloned().unwrap_or_default();
    let inflight = m.get_one::<u32>(INFLIGHT_BYTES).copied();
    let idle_secs = m.get_one::<u64>(UPLOAD_IDLE_TIMEOUT).copied();
    let sample = m.get_one::<u32>(COMMIT_SAMPLE).copied();
    Some(PackFlags {
        endpoint: endpoint.clone(),
        bucket: text(BUCKET),
        region: text(REGION),
        credentials_file: m
            .get_one::<PathBuf>(CREDENTIALS)
            .cloned()
            .unwrap_or_default(),
        pack_bytes: m.get_one::<u64>(PACK_BYTES).copied().unwrap_or_default(),
        inflight_bytes: inflight.unwrap_or_default(),
        upload_idle_timeout: Duration::from_secs(idle_secs.unwrap_or_default()),
        writer: text(WRITER),
        cache: m
            .get_one::<PathBuf>(CACHE_DIR)
            .cloned()
            .zip(m.get_one::<u64>(CACHE_BYTES).copied()),
        commit_sample: sample.unwrap_or_default() as usize,
    })
}

/// What the frontend commits a daemon's packs with: the packs and the number of blobs of a pack
/// to check, when it has both (`--pack-store-endpoint` opened packs); none otherwise, and the
/// frontend answers `CommitPack` UNIMPLEMENTED.
pub(super) fn commits(packs: Option<&Packs>, flags: Option<&PackFlags>) -> Option<PackCommits> {
    let both = packs.zip(flags);
    both.map(|(packs, flags)| PackCommits::new(packs.clone(), flags.commit_sample))
}

/// Why the pack store cannot be opened. No message carries the secret.
#[derive(Debug, thiserror::Error)]
pub enum PackFlagError {
    /// The credentials file cannot be used.
    #[error("--pack-store-credentials-file {path}: {why}")]
    Credentials {
        /// The file.
        path: String,
        /// What is wrong with it.
        why: String,
    },
    /// The endpoint or the bucket is not one the store client takes.
    #[error("--pack-store-endpoint: {0}")]
    Store(#[from] S3ConfigError),
    /// The packs cannot be named or written: the writer's name is not one a key can hold, or
    /// the store does not claim create-only writes.
    #[error("--pack-writer or the pack store: {0}")]
    Packs(#[from] PackError),
    /// The read cache cannot be opened.
    #[error("--pack-cache-dir: {0}")]
    Cache(#[from] CacheError),
}

impl PackFlags {
    /// The packs these flags describe, over `index`: the S3 store built (nothing is sent to
    /// it yet), the cache opened, the keys named for this start of the process.
    ///
    /// # Errors
    /// [`PackFlagError`]: the credentials file, the endpoint, the writer's name or the cache
    /// directory is refused.
    pub fn open(&self, index: Arc<dyn PackIndex>) -> Result<Packs, PackFlagError> {
        let credentials = read_credentials(&self.credentials_file)?;
        let store = S3Store::new(S3Config {
            endpoint: self.endpoint.clone(),
            region: self.region.clone(),
            bucket: self.bucket.clone(),
            credentials,
            capabilities: Capabilities {
                conditional_put: true,
                object_lock: false,
            },
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(60),
            min_body_rate: NonZeroU64::new(MIN_PUT_RATE),
        })?;
        let keys = PackKeys::new(WriterKind::Frontend, &self.writer, &boot_id())?;
        let cache = self
            .cache
            .as_ref()
            .map(|(dir, bytes)| ReadCache::open(dir, *bytes))
            .transpose()?;
        let settings = PackSettings {
            pack_bytes: self.pack_bytes,
            inflight_bytes: self.inflight_bytes,
            upload_idle_timeout: self.upload_idle_timeout,
            cache: cache.map(Arc::new),
            ..PackSettings::default()
        };
        let packs = Packs::new(Arc::new(store), index, keys, settings);
        packs.map_err(PackFlagError::from)
    }
}

/// The packs the flags ask for, if they ask for any, over the pack index `--pack-index`
/// opened.
///
/// # Errors
/// The pack flags are set and there is no pack index, or as [`PackFlags::open`].
pub(super) fn open(
    flags: Option<&PackFlags>,
    index: Option<&Arc<dyn PackIndex>>,
) -> Result<Option<Packs>, Box<dyn Error>> {
    match (flags, index) {
        (None, _) => Ok(None),
        (Some(_), None) => Err("--pack-store-endpoint needs --pack-index".into()),
        (Some(flags), Some(index)) => {
            tracing::warn!(
                "packs are on: daemons read blobs from the storage nodes only, so they cannot \
                 read a blob that is only in a pack"
            );
            Ok(Some(flags.open(Arc::clone(index))?))
        }
    }
}

/// The key pair in the file at `path`, read as every key file of the server is
/// ([`crate::key_file::read_key_pair`]): a regular file, not a link, owned by the server's
/// user, mode 0600 or 0400.
fn read_credentials(path: &Path) -> Result<Credentials, PackFlagError> {
    read_key_pair(path).map_err(|error| PackFlagError::Credentials {
        path: path.display().to_string(),
        why: error.to_string(),
    })
}

/// A name for this start of the process, so that no two starts of a frontend name the same
/// pack: the time in microseconds and the process id, in hex.
fn boot_id() -> String {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    let micros = since.map_or(0, |since| since.as_micros());
    format!("{micros:x}-{:x}", std::process::id())
}

#[cfg(test)]
mod tests;
