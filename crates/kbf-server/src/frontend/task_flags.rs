//! The flags that let the frontend prepare the runs of a host executor: `--task-secret-file`
//! and its companions, and the [`PutDirPrep`] they open over the frontend's [`Backend`].
//!
//! A [`PutDirPrep`] (`kbf_front::prepare_run`) hands the executor, for each run, the put
//! directory a task commits its object into and the closure its action tree is written from
//! (docs/design/jobs-on-jobscheduler-and-tierstore.md). All are off unless `--task-secret-file`
//! is given, and then a frontend serves exactly as it did: nothing is read from the files until
//! the frontend starts, and nothing is sent anywhere. The switch needs `--task-out-root`, which
//! clap enforces (so does the file of `kbf.toml`, which becomes flags). The companions that have
//! a default (`--task-url-slack-secs`, `--task-closure-max-digests`, `--task-closure-max-bytes`
//! and the limits of an object) need nothing, since the config template writes every flag with a
//! default as a live line.
//!
//! `--task-out-mode` says where a task's outputs go, and `put-dir` is the only place: a put
//! directory on the job's host under `--task-out-root` (`<mp>/put`, the action trees under
//! `<mp>/trees` beside it), where the task commits its object and from which the node's tier
//! store uploads it. The mode `presigned`, a presigned write of each pack to the tier store, was
//! removed; the flag still takes the word, so that a configuration that names it is refused with
//! a message that says what changed and not with a bare unknown value.
//!
//! Each run's closure walk reads the input Directories through a cache keyed by digest, in
//! memory (256 MiB) and, with `--task-directory-cache-dir` and `--task-directory-cache-bytes`
//! (each needs the other), on disk as well: a Directory is verified against its digest on every
//! read, so the directory holds nothing that is trusted. A directory that cannot be opened
//! fails the start, naming the flag and the path.
//!
//! The secret is a file and never a flag, as a command line is world-readable. The secret file
//! holds one secret of at least 32 bytes (surrounding white space is dropped), owned by the
//! server's user and readable by it alone: the mint of the put directories is made from it, and
//! it is the secret the job-facing listener verifies grants with.

use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::builder::ValueParser;
use clap::{Arg, ArgMatches};
use kbf_front::prepare_run::{ClosureLimits, DirectoryCache, PutDirPrep};
use kbf_front::{Backend, MintError, PackKeyMint};
use zeroize::Zeroizing;

use crate::executor::PutDirRoots;
use crate::token::{TokenFileError, read_owner_only};

const SECRET: &str = "task_secret_file";
const PUT_RETRY: &str = "task_put_retry_secs";
const MANIFEST_WRITES: &str = "task_manifest_writes";
const URL_SLACK: &str = "task_url_slack_secs";
const MAX_DIGESTS: &str = "task_closure_max_digests";
const MAX_BYTES: &str = "task_closure_max_bytes";
const OUT_MODE: &str = "task_out_mode";
const OUT_ROOT: &str = "task_out_root";
const OUT_FORMAT: &str = "task_out_format";
const MAX_OBJECT_ENTRIES: &str = "task_max_object_entries";
const MAX_OBJECT_MANIFEST: &str = "task_max_object_manifest_bytes";
const OBJECT_PER_ENTRY: &str = "task_object_bytes_per_entry";
const DIRECTORY_DIR: &str = "task_directory_cache_dir";
const DIRECTORY_BYTES: &str = "task_directory_cache_bytes";
const ONE_PASS: &str = "one-pass";
const PRESIGNED: &str = "presigned";
const PUT_DIR: &str = "put-dir";

/// The slack when `--task-url-slack-secs` is left out: 10 minutes. Clap takes the default as the
/// literal below; this is the number the default-flags test checks it against.
#[cfg(test)]
const DEFAULT_URL_SLACK_SECS: u64 = 600;

/// The total wait for a 503 when `--task-put-retry-secs` is left out: 60 seconds (test-only, as
/// above).
#[cfg(test)]
const DEFAULT_PUT_RETRY_SECS: u64 = 60;

/// The most writes `--task-manifest-writes` takes.
const MAX_MANIFEST_WRITES: u64 = 4096;

/// The longest slack a flag takes, in seconds: the 4 hour ceiling of the keys of a put
/// directory ([`kbf_front::prepare_run::MAX_EXPIRY`]).
const MAX_SECS: u64 = kbf_front::prepare_run::MAX_EXPIRY.as_secs();

/// The most bytes a secret file may hold.
const MAX_FILE_BYTES: usize = 4096;

/// What `--task-out-mode presigned` is told: the build that refuses it is the release that
/// removed it.
const PRESIGNED_REMOVED: &str = concat!(
    "--task-out-mode presigned was removed in this release (kbf-server ",
    env!("CARGO_PKG_VERSION"),
    "+",
    env!("KBF_BUILD_COMMIT"),
    "): a task's outputs go into a put directory only. Give --task-out-mode put-dir (the \
     default) with --task-out-root, and drop --task-put-endpoint, --task-put-bucket, \
     --task-put-region, --task-put-credentials-file, --task-read-url, --task-run-wall-secs, \
     --task-max-packs, --task-max-large, --task-large-credentials-file and \
     --task-large-max-bytes, which only that mode used",
);

/// The flags of the mode that was removed, by long name: they only refuse.
const REMOVED: [&str; 10] = [
    "task-put-endpoint",
    "task-put-bucket",
    "task-put-region",
    "task-put-credentials-file",
    "task-read-url",
    "task-run-wall-secs",
    "task-max-packs",
    "task-max-large",
    "task-large-credentials-file",
    "task-large-max-bytes",
];

/// What a start that gives one of the removed flags is told, after clap's `invalid value ... for
/// '--<flag> <VALUE>'`, which names the flag.
const REMOVED_WHY: &str = concat!(
    "the flag was removed in this release (kbf-server ",
    env!("CARGO_PKG_VERSION"),
    "+",
    env!("KBF_BUILD_COMMIT"),
    ") with `--task-out-mode presigned`, the only mode that read it: a task's outputs go into a \
     put directory (`--task-out-root`); delete the flag",
);

/// The removed flags, each refusing any value with [`REMOVED_WHY`], hidden from `--help`.
fn removed() -> impl Iterator<Item = Arg> {
    REMOVED.into_iter().map(|long| {
        Arg::new(long)
            .long(long)
            .hide(true)
            .value_name("VALUE")
            .help("Removed with `--task-out-mode presigned`. Any value is refused at start")
            .value_parser(ValueParser::new(refuse))
    })
}

/// The parser of a removed flag: it takes no value.
fn refuse(_: &str) -> Result<String, &'static str> {
    Err(REMOVED_WHY)
}

/// The thirteen flags of the put directory and the ten removed ones.
pub(super) fn args() -> Vec<Arg> {
    live().into_iter().chain(removed()).collect()
}

/// The thirteen flags that are read.
fn live() -> [Arg; 14] {
    [
        Arg::new(SECRET)
            .long("task-secret-file")
            .value_name("PATH")
            .value_parser(clap::value_parser!(PathBuf))
            .requires(OUT_ROOT)
            .help(
                "Turns on the preparation of the runs of a host executor: for each run the \
                 frontend makes the put directory the task commits its object into, and the \
                 input closure its action tree is written from. The file holds one secret of \
                 at least 32 bytes (surrounding white space is dropped), owned by this user \
                 and readable by it alone (mode 0600 or 0400): the mint of the put \
                 directories is made from it, and it is the secret the job-facing listener \
                 verifies grants with. The secret is never a flag: a command line is \
                 world-readable. Needs `--task-out-root`. Left out, no run is prepared",
            ),
        Arg::new(PUT_RETRY)
            .long("task-put-retry-secs")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64).range(0..=MAX_SECS))
            // DEFAULT_PUT_RETRY_SECS, as above.
            .default_value("60")
            .help(
                "The most seconds the waits add up to before a create-only write of an action tree to the tier store (`cas/l/<sha256>`) gives up on a 503 (\"retry later\", which the store answers while it registers: 10 to 40 s), so the lease fails only after this long. A write whose connection turns out to be dead is made once more at once, whatever this says. 0 never waits for a 503. A 503 `SlowDown` (the store taking as many uploads as it can) is back-pressure and is waited out past this, until the run's preparation limit. Used only with `--task-secret-file` and `--task-out-mode put-dir`",
            ),
        Arg::new(MANIFEST_WRITES)
            .long("task-manifest-writes")
            .value_name("WRITES")
            .value_parser(clap::value_parser!(u64).range(1..=MAX_MANIFEST_WRITES))
            // kbf_front::action_tree::MANIFEST_WRITES; the default-flags test compares them.
            .default_value("32")
            .help(
                "The most create-only writes of action-tree manifests to the tier store (`cas/l/<sha256>`) this frontend has open at once, across every run it prepares; a write past it waits at the frontend for one to end. A burst of runs then queues here and not at the store, whose every write would slow as more arrive. Used only with `--task-secret-file` and `--task-out-mode put-dir`",
            ),
        Arg::new(URL_SLACK)
            .long("task-url-slack-secs")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64).range(0..=MAX_SECS))
            // DEFAULT_URL_SLACK_SECS, which clap cannot take as a computed string; the
            // default-flags test parses it back and compares it with the constant.
            .default_value("600")
            .help(
                "The time past a run's wall, in seconds, that the keys of its put directory \
                 stay good: the time a task takes to start and to commit its object. The wall \
                 and the slack together must not pass 14400, the longest a key may be good \
                 for, or the frontend does not start. Used only with `--task-secret-file`",
            ),
        Arg::new(MAX_DIGESTS)
            .long("task-closure-max-digests")
            .value_name("DIGESTS")
            .value_parser(clap::value_parser!(u32).range(1..))
            // ClosureLimits::DEFAULT.max_digests, as above.
            .default_value("500000")
            .help(
                "The most distinct digests (directories and files) the input tree of a run may \
                 have: a run whose tree has more is refused, never run on a part of its \
                 inputs. Used only with `--task-secret-file`",
            ),
        Arg::new(MAX_BYTES)
            .long("task-closure-max-bytes")
            .value_name("BYTES")
            .value_parser(clap::value_parser!(u64).range(1..))
            // ClosureLimits::DEFAULT.max_bytes, as above.
            .default_value("34359738368")
            .help(
                "The most bytes of blobs, by their digests' sizes, the input tree of a run may \
                 have (each distinct blob once): a run whose tree has more is refused. Used only \
                 with `--task-secret-file`",
            ),
        Arg::new(OUT_MODE)
            .long("task-out-mode")
            .value_name("MODE")
            .value_parser([PRESIGNED, PUT_DIR])
            .default_value(PUT_DIR)
            // The root names the switch, so a put directory is never asked for without it.
            .requires_if(PUT_DIR, OUT_ROOT)
            .help(
                "Where a task's outputs go. `put-dir`, the only mode and the default: the task \
                 commits them as files into a put directory under `--task-out-root`, mounted \
                 at /out, which the host's tier store uploads; the inputs are the action tree \
                 under the trees directory beside it, the job's root, and the job has no \
                 network. `presigned` was removed: it is refused, with the flags to drop. Used \
                 only with `--task-secret-file` and `--executor js-agent`",
            ),
        Arg::new(OUT_ROOT)
            .long("task-out-root")
            .value_name("PATH")
            .value_parser(clap::value_parser!(String))
            .requires(SECRET)
            .help(
                "The tier store's put directory on every host (`<mp>/put`, an absolute path), \
                 which the host's agent lists as a read-write mount root; each run's directory \
                 is `<mp>/put/cas/b/<lease>/<nonce>`, and its action tree `<mp>/trees/<digest>`",
            ),
        Arg::new(MAX_OBJECT_ENTRIES)
            .long("task-max-object-entries")
            .value_name("ENTRIES")
            .value_parser(clap::value_parser!(u32).range(1..))
            // kbf_front::MAX_UNTRUSTED_OBJECT_ENTRIES, as above.
            .default_value("1048576")
            .help(
                "The most records one object of a run may index. A larger index is refused as \
                 too large before it is read, so this bounds what the frontend reads to trust \
                 an object (52 bytes a record)",
            ),
        Arg::new(MAX_OBJECT_MANIFEST)
            .long("task-max-object-manifest-bytes")
            .value_name("BYTES")
            .value_parser(clap::value_parser!(u64).range(1..))
            // kbf_front::MAX_UNTRUSTED_MANIFEST_BYTES, as above.
            .default_value("4194304")
            .help(
                "The most bytes of an object's manifest, and the base of the most its tail \
                 (manifest, file index and central directory) may be: the base plus \
                 `--task-object-bytes-per-entry` for each of `--task-max-object-entries`. A \
                 larger one is refused as too large before it is read",
            ),
        Arg::new(OBJECT_PER_ENTRY)
            .long("task-object-bytes-per-entry")
            .value_name("BYTES")
            .value_parser(clap::value_parser!(u64).range(1..))
            // kbf_front::MAX_UNTRUSTED_BYTES_PER_ENTRY, as above.
            .default_value("1024")
            .help(
                "The bytes of an object's tail each record may account for, its central \
                 directory entry (about 130 bytes) and its line of the file index, which names \
                 a path. See `--task-max-object-manifest-bytes`",
            ),
        Arg::new(DIRECTORY_DIR)
            .long("task-directory-cache-dir")
            .value_name("DIR")
            .value_parser(clap::value_parser!(PathBuf))
            .requires(DIRECTORY_BYTES)
            .help(
                "A directory this process owns where the input Directories the closure walk \
                 decodes are kept on disk, one file each by digest, checked by SHA-256 on every \
                 read, so that a restart or a Directory pushed out of memory is read from the \
                 disk and not from the store. At most `--task-directory-cache-bytes` of them \
                 are kept, the least recently used removed first. One process per directory: \
                 nothing else may write there. Needs `--task-directory-cache-bytes`. Left out, \
                 the Directories are kept in memory only (256 MiB). Used only with \
                 `--task-secret-file`",
            ),
        Arg::new(DIRECTORY_BYTES)
            .long("task-directory-cache-bytes")
            .value_name("BYTES")
            .value_parser(clap::value_parser!(u64).range(1..))
            .requires(DIRECTORY_DIR)
            .help(
                "The most bytes of Directories `--task-directory-cache-dir` holds. Needs \
                 `--task-directory-cache-dir`. Used only with `--task-secret-file`",
            ),
        Arg::new(OUT_FORMAT)
            .long("task-out-format")
            .value_name("FORMAT")
            .value_parser([ONE_PASS])
            .default_value(ONE_PASS)
            .help(
                "What the task writes. `one-pass`, the only value and the default: one object \
                 per action, written while the outputs are read, its manifest and index at the \
                 end (docs/design/one-pass-task-object.md). The flag is still accepted so that \
                 a configuration that names it starts; the `packs` value of the releases \
                 before is gone, and this frontend still reads the packs those wrote. Every \
                 task image must be this release or later",
            ),
    ]
}

/// What the task flags say for a frontend whose runs commit into put directories: given by
/// `--task-secret-file` with `--task-out-root`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutDirFlags {
    /// `--task-secret-file`.
    pub secret_file: PathBuf,
    /// `--task-out-root`.
    pub roots: PutDirRoots,
    /// `--task-url-slack-secs`: the time past a run's wall that its directory's keys stay good.
    pub url_slack: Duration,
    /// `--task-put-retry-secs`.
    pub put_retry: Duration,
    /// `--task-manifest-writes`.
    pub manifest_writes: usize,
    /// `--task-closure-max-digests`.
    pub max_digests: usize,
    /// `--task-max-object-entries`.
    pub max_object_entries: usize,
    /// `--task-max-object-manifest-bytes`.
    pub max_object_manifest_bytes: u64,
    /// `--task-object-bytes-per-entry`.
    pub object_bytes_per_entry: u64,
    /// `--task-closure-max-bytes`.
    pub max_bytes: u64,
    /// `--task-directory-cache-dir` and `--task-directory-cache-bytes`, which come together:
    /// the directory and the most bytes of Directories it holds. `None` keeps the Directories in
    /// memory only.
    pub directory_cache: Option<(PathBuf, u64)>,
}

/// The setting the flags in `m` make: `None` unless `--task-secret-file` is given (clap makes it
/// come with `--task-out-root`).
///
/// # Errors
/// `--task-out-mode presigned`, which was removed, or `--task-out-root` is not an absolute path
/// in normal form.
pub(super) fn from_matches(m: &ArgMatches) -> Result<Option<PutDirFlags>, String> {
    if m.get_one::<String>(OUT_MODE).map(String::as_str) == Some(PRESIGNED) {
        return Err(PRESIGNED_REMOVED.to_owned());
    }
    let (Some(secret_file), Some(root)) =
        (m.get_one::<PathBuf>(SECRET), m.get_one::<String>(OUT_ROOT))
    else {
        return Ok(None);
    };
    let digests = m.get_one::<u32>(MAX_DIGESTS).copied().unwrap_or_default();
    Ok(Some(PutDirFlags {
        secret_file: secret_file.clone(),
        roots: PutDirRoots::new(root).map_err(|error| format!("--task-out-root: {error}"))?,
        url_slack: Duration::from_secs(m.get_one::<u64>(URL_SLACK).copied().unwrap_or_default()),
        put_retry: Duration::from_secs(m.get_one::<u64>(PUT_RETRY).copied().unwrap_or_default()),
        manifest_writes: usize::try_from(m.get_one::<u64>(MANIFEST_WRITES).copied().unwrap_or(1))
            .unwrap_or(usize::MAX),
        max_digests: usize::try_from(digests).unwrap_or(usize::MAX),
        max_object_entries: entries_of(m),
        max_object_manifest_bytes: m
            .get_one::<u64>(MAX_OBJECT_MANIFEST)
            .copied()
            .unwrap_or_default(),
        object_bytes_per_entry: m
            .get_one::<u64>(OBJECT_PER_ENTRY)
            .copied()
            .unwrap_or_default(),
        max_bytes: m.get_one::<u64>(MAX_BYTES).copied().unwrap_or_default(),
        directory_cache: m
            .get_one::<PathBuf>(DIRECTORY_DIR)
            .cloned()
            .zip(m.get_one::<u64>(DIRECTORY_BYTES).copied()),
    }))
}

impl PutDirFlags {
    /// The preparer these flags describe, reading the store through `backend`: the secret file
    /// is read, the mint made and the cache of Directories opened. Nothing is sent.
    ///
    /// # Errors
    /// [`TaskFlagError`]: the file is missing, not owned by this user or readable by others, the
    /// secret is under 32 bytes, or the directory of `--task-directory-cache-dir` cannot be
    /// opened.
    pub fn open(&self, backend: Backend) -> Result<PutDirPrep, TaskFlagError> {
        let secret = Zeroizing::new(read_owner_only(&self.secret_file, MAX_FILE_BYTES)?);
        let mint = PackKeyMint::new(secret.trim_ascii())?;
        let limits = ClosureLimits {
            max_digests: self.max_digests,
            max_bytes: self.max_bytes,
        };
        let directories = match &self.directory_cache {
            Some((dir, bytes)) => DirectoryCache::open(
                dir.clone(),
                kbf_front::prepare_run::DIRECTORY_MEMORY_BYTES,
                *bytes,
            )
            .map_err(|source| TaskFlagError::Directories {
                path: dir.clone(),
                source,
            })?,
            None => DirectoryCache::in_memory(kbf_front::prepare_run::DIRECTORY_MEMORY_BYTES),
        };
        Ok(PutDirPrep::new(backend, mint, self.url_slack, limits)
            .with_directories(Arc::new(directories)))
    }
}

/// The put-directory preparer the flags ask for, if they ask for one, over `backend`.
///
/// # Errors
/// As [`PutDirFlags::open`].
pub(super) fn open_put_dir(
    flags: Option<&PutDirFlags>,
    backend: &Backend,
) -> Result<Option<Arc<PutDirPrep>>, Box<dyn Error>> {
    let prepared = flags.map(|flags| flags.open(backend.clone()));
    Ok(prepared.transpose()?.map(Arc::new))
}

/// What `--task-max-object-entries` says.
fn entries_of(m: &ArgMatches) -> usize {
    let entries = m
        .get_one::<u32>(MAX_OBJECT_ENTRIES)
        .copied()
        .unwrap_or_default();
    usize::try_from(entries).unwrap_or(usize::MAX)
}

/// Why the runs cannot be prepared. No message carries a secret.
#[derive(Debug, thiserror::Error)]
pub enum TaskFlagError {
    /// The secret file cannot be used.
    #[error("--task-secret-file: {0}")]
    Secret(#[from] TokenFileError),
    /// The secret is too short for the mint of the put directories.
    #[error("--task-secret-file: {0}")]
    Mint(#[from] MintError),
    /// The directory of the cache of Directories cannot be opened.
    #[error("--task-directory-cache-dir {}: {source}", .path.display())]
    Directories {
        /// The directory the flag names.
        path: PathBuf,
        /// What the cache said.
        source: kbf_pack::CacheError,
    },
}

#[cfg(test)]
mod tests;
