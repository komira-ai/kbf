//! The task tool's driver: the flags, the order of the steps, and the exit code of each way they
//! can fail.
//!
//! [`execute`] reads the Action, its Command and its input tree through a [`BlobSource`], runs the
//! Command, collects its outputs, builds the manifest and the pack, and stores the pack through a
//! [`PackSink`]. The source and the sink are parameters so the whole order runs in tests without a
//! network; [`main_with`] supplies the real ones, built from the flags.
//!
//! # Exit codes
//!
//! The exit code says whether the farm did its part, not whether the action succeeded: the
//! action's own exit code is in the manifest.
//!
//! | code | meaning |
//! |------|---------|
//! | 0 | the action ran (whatever its exit code, a timeout included) and its pack was stored |
//! | 2 | the flags are wrong (clap) |
//! | 10 | fetch: the Action, the Command or an input could not be read or checked, or laid out |
//! | 11 | run-start: the Command could not be started (bad paths, no program, a `kbf-network` property other than `allow`), or the run's `private/` home, tmp and cache directories could not be made |
//! | 12 | pack: the outputs could not be collected, did not fit the run's packs and large objects (`output-too-large`), changed after they were hashed (`output-changed`), or made a manifest or pack the frontend would refuse (`invalid-manifest`, `invalid-pack`) |
//! | 13 | put: a pack or large object could not be stored (refused, key already taken, still no room after the retries of a 503, unreachable), or a large object's bytes changed while it was sent (the stored object is an orphan) |
//! | 14 | setup: `--lease` is not a build id, a credential or root certificate is unusable, a credential given as a flag value without `--insecure-argv-secrets`, or the runtime did not start |
//! | 15 | no confine: the Command is to be denied the network and this system cannot deny it (neither Linux nor macOS); nothing was run |

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{ArgGroup, Parser};
use kbf_proto::reapi::{self, Action, Command};
use kbf_segments::sha256;
use kbf_task_pack::{
    LargeEntry, MAX_LARGE, MAX_PACKS, OutputDirectory, OutputFile, OutputSymlink, TaskManifest,
    Timing, Usage, check_build,
};
use kbf_types::Digest;

use crate::digest;
use crate::fetch::{
    BlobSource, FetchError, HttpBlobSource, LocatedSource, fetch_message, materialize, native_roots,
};
use crate::network;
use crate::outputs::{Blobs, OutputLimits, collect_outputs};
use crate::pack::{MAX_LARGE_BYTES, PackError, build, build_extra};
use crate::plan::{Limits, plan};
use crate::put::{
    Jitter, PutError, RandomJitter, Sleeper, ThreadSleeper, agent, agent_with_plain_http, put,
    put_reader, put_retrying, tls_config,
};
use crate::put_urls::PutUrls;
use crate::run::{Run, RunError, output_paths, run};

/// Exit code: an input could not be read, checked or laid out.
pub const EXIT_FETCH: u8 = 10;
/// Exit code: the Command could not be started.
pub const EXIT_RUN: u8 = 11;
/// Exit code: the outputs could not be collected or packed.
pub const EXIT_PACK: u8 = 12;
/// Exit code: the pack could not be stored.
pub const EXIT_PUT: u8 = 13;
/// Exit code: a file a flag names is unusable, or the runtime did not start.
pub const EXIT_SETUP: u8 = 14;
/// Exit code: the Command is to be denied the network and this system (neither Linux nor macOS) has no confine that can.
pub const EXIT_NO_CONFINE: u8 = 15;

/// `kbf-task`: runs one build action inside a job and stores its pack with one PUT.
///
/// # The credentials
///
/// The presigned PUT URL and the read grant's bearer token each come from exactly one of a file
/// (`--pack-url-file`, `--grant-token-file`) or, as a stopgap, a flag value (`--pack-url`,
/// `--grant-token`). A flag value is in the tool's argv, which the Command and every process on
/// the host can read, so the flag forms are accepted only together with
/// `--insecure-argv-secrets`; without it the tool refuses to start (exit 14). Their exposure is
/// bounded: the URL is create-only, for one key, and expires at the action's timeout plus a margin;
/// the grant is scoped to one build's inputs. The flag forms exist because the job scheduler does
/// not yet deliver job secrets as files (komira-ai/kbf#1934), and are removed when it does. Neither
/// value is ever written to a log or an error message.
///
/// # The v2 key set
///
/// `--put-urls-file` (or, with `--insecure-argv-secrets`, `--put-urls`) takes the place of
/// `--pack-url-file`/`--pack-url`: the JSON key set of the pack and large-object URLs the
/// frontend issued for the run (see [`crate::put_urls`]). It is read and validated at startup.
/// The run plans its outputs into those slots ([`crate::plan`]): a blob over the record cap is
/// one large object, PUT as raw bytes streamed from the file it was read from; the other blobs
/// are packed into packs 1.. and then pack 0, which holds the manifest. The PUTs go in this order:
/// the large objects, packs `N-1` down to 1, then pack 0, the commit point. The v1 flags are a key
/// set of one pack and no large object, and give the pack the v1 tool wrote, byte for byte; so
/// does a key set of that shape.
///
/// # What the manifest claims
///
/// The manifest's `exit_code`, `timing` and `usage` are written by this tool, which runs as the
/// same user as the Command: they are claims, and the frontend does not trust them. Packs are
/// built in memory: pack 0 is kept built, and each other pack is built twice, one at a time, from
/// the blobs read for it, so the tool's peak memory is about pack 0 plus two packs (about 384 MiB
/// at the 128 MiB limit); a job's memory limit must allow for it, or the kill shows as 137, not
/// 12. A large object is streamed from its file and never held.
///
/// # Storing the pack
///
/// The pack is PUT once, create-only. A 503 (no room in the store, or the store is taking as
/// many puts as it may) is retried up to seven times, each after a wait of between half and one
/// and a half times the `Retry-After` it names (taken as at most 30 s; 2 s when it names none),
/// the task's own waits adding up to at most 60 s (the store may also hold each refused attempt
/// for its put-permit wait before answering, so the last attempt can begin about 165 s after the
/// first; the URL's lifetime, the run's wall limit plus the frontend's slack, bounds it). A
/// connection that broke after it was made is retried the same way. Every other answer is final
/// (429, 500, 502 and 504 deliberately not retried), and a 412 means the key exists (a
/// re-attempt of the task gets a new key).
#[derive(Parser)]
#[command(
    name = "kbf-task",
    version = concat!(env!("CARGO_PKG_VERSION"), "+", env!("KBF_BUILD_COMMIT")),
    group(ArgGroup::new("pack_url_source").required(true).args(["pack_url_file", "pack_url", "put_urls_file", "put_urls"])),
    group(ArgGroup::new("grant_token_source").required(true).args(["grant_token_file", "grant_token"]))
)]
pub struct Cli {
    /// A file holding the presigned URL the pack is PUT to, once. The URL is a credential, so by
    /// default it is not a flag value (argv is readable by the Command): the file is read and left
    /// in place. The executor that delivered it removes it when the run ends (on js-agent it is a
    /// read-only run file). The URL allows one create-only PUT of this task's own pack key, so a
    /// Command that reads it, and copies it into a declared output, stores nothing it could not
    /// already store as an output.
    #[arg(long)]
    pub pack_url_file: Option<PathBuf>,
    /// The presigned URL itself. Stopgap (komira-ai/kbf#1934): it is in argv, so it needs
    /// `--insecure-argv-secrets`.
    #[arg(long)]
    pub pack_url: Option<String>,
    /// A file holding the v2 key set, JSON `{"packs":[url,...],"large":[url,...]}`, instead of
    /// `--pack-url-file`: read, never deleted, and a credential like the pack URL. The pack URLs
    /// are, in order, the slots of packs 0..; the large URLs, the slots of large objects 0.. . A
    /// run that needs more packs or large objects than the set has fails with `output-too-large`
    /// before anything is stored. More than 8 of either is refused at startup (exit 14). The file's protection is the
    /// executor's per-run directory, not its mode: the executor delivers it read-only for the
    /// run, so the tool does not check its permissions.
    #[arg(long)]
    pub put_urls_file: Option<PathBuf>,
    /// The v2 key set itself. Stopgap (komira-ai/kbf#1934): it is in argv, so it needs
    /// `--insecure-argv-secrets`.
    #[arg(long)]
    pub put_urls: Option<String>,
    /// The base URL of the read grant: blobs are read from `<base>/<hex>/<size>`.
    #[arg(long)]
    pub grant_url: String,
    /// The most requests to the read grant in flight at once while the input tree is fetched, 1 to
    /// 256. The tree is read breadth-first: each level's Directory messages together, then every
    /// distinct file blob, each streamed to disk.
    #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u16).range(1..=256))]
    pub fetch_concurrency: u16,
    /// Read the input blobs under 64 KiB in `POST <grant-url>/batch` requests (up to 256 digests
    /// and 4 MiB each) instead of one GET each; larger blobs still go by GET. Off by default: a
    /// grant without the route answers 404, after which every blob is read by GET.
    #[arg(long)]
    pub fetch_batch: bool,
    /// A file holding the read grant's bearer token (a trailing newline is dropped).
    #[arg(long)]
    pub grant_token_file: Option<PathBuf>,
    /// The bearer token itself. Stopgap (komira-ai/kbf#1934): it is in argv, so it needs
    /// `--insecure-argv-secrets`.
    #[arg(long)]
    pub grant_token: Option<String>,
    /// Accept `--pack-url`, `--put-urls` and `--grant-token`, whose values are visible in argv to the Command and
    /// to every process on the host. Removed when the job scheduler delivers job secrets as files
    /// (komira-ai/kbf#1934).
    #[arg(long)]
    pub insecure_argv_secrets: bool,
    /// PUT the pack to an `http` URL as well as an `https` one: for a store reached over a private
    /// network without TLS, where the presigned signature crosses that network in the clear. Without
    /// it, only `https` URLs are used. Blobs are read over `https` either way.
    #[arg(long)]
    pub allow_plain_http_put: bool,
    /// Read the grant over `http` as well as `https`: for a farm on its own network, where the
    /// bearer token crosses that network in the clear. The token is scoped to one action's inputs
    /// and expires. Without it, an `http` grant URL is refused at startup (exit 14), before any
    /// request. Applies to `--fetch-batch` too.
    #[arg(long)]
    pub allow_plain_http_grant: bool,
    /// Ask the listener's `GET /locate` once, before the inputs are fetched, and read each input
    /// where it points: a byte range of a pack, or a whole large object, directly from the store
    /// the answer names, with the read grants the answer carries. Every byte is checked against
    /// its digest like any other, and a blob whose direct read fails (a refused or expired grant,
    /// a short or wrong read, any error) is read from the listener. Without this flag `/locate` is
    /// never called. An `http` store needs `--allow-plain-http-grant`, since the grants cross the
    /// network in the clear; the grants are never written to a log.
    #[arg(long)]
    pub locate: bool,
    /// The lease id the frontend gave; it is copied into the manifest. It must be a build id, 1 to
    /// 128 bytes of `0-9A-Za-z_-`, the only lease the frontend accepts in a manifest; any other is
    /// refused at startup (exit 14), before anything is fetched.
    #[arg(long)]
    pub lease: String,
    /// The Action to run, as `<64 lowercase hex digits>/<size>`.
    #[arg(long, value_parser = digest::parse)]
    pub action: Digest,
    /// Where the inputs are laid out (`in/` under it) and the output streams are written. The
    /// Command's `HOME`, `TMPDIR`, `XDG_CACHE_HOME` and `CLANG_MODULE_CACHE_PATH` name directories
    /// under `private/` here, made new for the run (an existing `private` is refused) with mode 0700, beside `in/` and so never collected as outputs: a job's
    /// image root is read-only and its `/tmp` is small and `noexec`. A variable the Command sets
    /// itself keeps its value.
    #[arg(long, default_value = "/scratch/work")]
    pub work_dir: PathBuf,
    /// The longest the Command may run, in milliseconds: the Action's timeout when it sets a
    /// shorter positive one, else this.
    #[arg(long, default_value_t = 3_600_000)]
    pub deadline_ms: u64,
    /// A DER certificate to trust besides the host's roots, for a job image with no CA bundle.
    #[arg(long)]
    pub root_der: Vec<PathBuf>,
    /// The limits on what the action may leave.
    #[command(flatten)]
    pub limits: OutputLimits,
}

/// Why a task did not complete; each class has its own exit code.
#[derive(Debug, thiserror::Error)]
pub enum Failure {
    /// A flag's file is unusable.
    #[error("setup: {0}")]
    Setup(String),
    /// An input could not be read, checked or laid out.
    #[error("fetch: {0}")]
    Fetch(#[from] FetchError),
    /// The Command could not be started.
    #[error("run: {0}")]
    Run(#[from] RunError),
    /// The outputs could not be collected.
    #[error("outputs: {0}")]
    Outputs(#[from] kbf_outputs::OutputsError),
    /// The collected result names a digest the manifest cannot carry.
    #[error("outputs: {0}")]
    Result(String),
    /// A collected blob could not be read back.
    #[error("outputs: {0}")]
    Read(#[from] std::io::Error),
    /// The manifest or the pack is one the frontend would refuse: a size cap (detail
    /// `output-too-large`), or a path, a target, a number or a digest the shared rules refuse.
    #[error("pack: {0}")]
    Pack(#[from] PackError),
    /// The pack could not be stored.
    #[error("put: {0}")]
    Put(#[from] PutError),
}

impl Failure {
    /// The process exit code of this class of failure.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Failure::Setup(_) => EXIT_SETUP,
            Failure::Fetch(_) => EXIT_FETCH,
            Failure::Run(RunError::NoConfine) => EXIT_NO_CONFINE,
            Failure::Run(_) => EXIT_RUN,
            Failure::Outputs(_) | Failure::Result(_) | Failure::Read(_) | Failure::Pack(_) => {
                EXIT_PACK
            }
            Failure::Put(_) => EXIT_PUT,
        }
    }
}

/// The files this tool was given a credential or presigned URL in (`--grant-token-file`,
/// `--pack-url-file`, `--put-urls-file`): the ones the Command must not read, as on macOS its
/// sandbox profile denies. A credential given as a flag value is in no file.
pub(crate) fn credential_files(cli: &Cli) -> Vec<PathBuf> {
    [
        &cli.grant_token_file,
        &cli.pack_url_file,
        &cli.put_urls_file,
    ]
    .into_iter()
    .flatten()
    .cloned()
    .collect()
}

/// Where the finished packs and large objects go: one slot for each URL of the run's key set.
pub trait PackSink {
    /// How many pack slots the run was issued (`N`).
    fn pack_slots(&self) -> usize;

    /// How many large-object slots the run was issued (`M`).
    fn large_slots(&self) -> usize;

    /// Stores `body`, once, as pack `slot` (0 is the one with the manifest).
    ///
    /// # Errors
    /// The pack was not stored.
    fn store_pack(&self, slot: usize, body: &[u8]) -> Result<(), PutError>;

    /// Stores the blob `digest` of `blobs`, once, as large object `slot`: its raw bytes, streamed,
    /// never read whole.
    ///
    /// # Errors
    /// The object was not stored.
    fn store_large(&self, slot: usize, blobs: &Blobs, digest: &Digest) -> Result<(), PutError>;
}

/// The real sink: the create-only PUT ([`put`], [`put_reader`]) to the presigned URL of each
/// slot, repeated after a 503 only ([`put_retrying`]), with `sleeper` between the attempts and
/// `jitter` moving each wait.
pub struct HttpSink {
    agent: ureq::Agent,
    urls: PutUrls,
    sleeper: Arc<dyn Sleeper>,
    jitter: Arc<dyn Jitter>,
}

impl PackSink for HttpSink {
    fn pack_slots(&self) -> usize {
        self.urls.packs.len()
    }

    fn large_slots(&self) -> usize {
        self.urls.large.len()
    }

    fn store_pack(&self, slot: usize, body: &[u8]) -> Result<(), PutError> {
        put_retrying(
            || put(&self.agent, &self.urls.packs[slot], body),
            self.sleeper.as_ref(),
            self.jitter.as_ref(),
        )
    }

    fn store_large(&self, slot: usize, blobs: &Blobs, digest: &Digest) -> Result<(), PutError> {
        // Each attempt reads the blob again from its first byte.
        put_retrying(
            || {
                let blob = digest::to_reapi(digest);
                let mut reader = blobs.reader(&blob).map_err(|e| PutError::File {
                    path: format!("output blob {digest}"),
                    reason: e.to_string(),
                })?;
                put_reader(
                    &self.agent,
                    &self.urls.large[slot],
                    digest.size_bytes,
                    &mut reader,
                )?;
                // The bytes streamed are hashed on the way. If they are not the blob's, the
                // object is stored but wrong: the run fails (exit 13) and pack 0 is never PUT,
                // so the object is an orphan that the janitor removes.
                reader.matches(&blob).then_some(()).ok_or(PutError::File {
                    path: format!("output blob {digest}"),
                    reason:
                        "its bytes changed after they were hashed; the object stored is an orphan"
                            .to_owned(),
                })
            },
            self.sleeper.as_ref(),
            self.jitter.as_ref(),
        )
    }
}

/// What the manifest of a run with several large objects can take beyond a manifest without:
/// the `packs` field and up to [`MAX_LARGE`](kbf_task_pack::MAX_LARGE) entries of about 110 bytes each.
const MANIFEST_SLACK: u64 = 2048;

/// The bytes pack 0 keeps for the manifest. A run of one pack and no large object writes the
/// manifest it already has, so the reserve is exactly its JSON length and a v1 run that fills
/// the pack to the byte still fits. With more packs or a large object the manifest gains the
/// `packs` field and entries, so [`MANIFEST_SLACK`] is added.
fn manifest_reserve(manifest: &TaskManifest, limits: &Limits) -> Result<u64, PackError> {
    let json = manifest.to_json_with(MAX_LARGE_BYTES)?.len() as u64;
    Ok(if limits.packs == 1 && limits.large == 0 {
        json
    } else {
        json + MANIFEST_SLACK
    })
}

/// The bytes of outputs the recorder may hold: what the run's packs and large objects can.
fn output_cap(limits: &Limits) -> u64 {
    let packs = (limits.packs as u64).saturating_mul(limits.pack_bytes);
    let large = (limits.large as u64).saturating_mul(limits.large_bytes);
    packs.saturating_add(large)
}

/// One of packs 1.., built from the recorded blobs `digests`.
fn extra_pack(blobs: &Blobs, digests: &[Digest]) -> Result<Vec<u8>, Failure> {
    let held = held(blobs, digests)?;
    let views: Vec<&[u8]> = held.iter().map(|bytes| bytes.as_ref()).collect();
    Ok(build_extra(&views)?)
}

/// The recorded blobs `digests` in memory, for one pack, each checked against its digest: a file
/// that changed in place after it was hashed is not packed (detail `output-changed`), and one
/// that shrank cannot be read in full.
fn held<'a>(blobs: &'a Blobs, digests: &[Digest]) -> Result<Vec<Cow<'a, [u8]>>, Failure> {
    let mut held = Vec::new();
    for digest in digests {
        for bytes in blobs.read(&digest::to_reapi(digest))?.into_iter() {
            if sha256(&bytes) != *digest {
                return Err(PackError::changed(digest).into());
            }
            held.push(bytes);
        }
    }
    Ok(held)
}

/// Runs the task `cli` names, against `source` and `sink`, and returns the manifest that went into
/// the pack.
///
/// The steps run in this order: fetch the Action, its Command and the input tree; run; collect the
/// outputs; plan where each blob goes; build the manifest and pack 0; store the large objects,
/// then packs `N-1` down to 1, then pack 0. The Command's own exit code, or a
/// deadline kill, is in the manifest and is not a failure. The manifest has no time for the PUT: the
/// pack holds the manifest, so it cannot hold the time the pack took to store. An empty stdout or
/// stderr is left out of the manifest, and no empty blob is a record of the pack.
///
/// # Errors
/// A [`Failure`], whose [`Failure::exit_code`] names the step that failed.
pub async fn execute(
    cli: &Cli,
    source: &dyn BlobSource,
    sink: &dyn PackSink,
) -> Result<TaskManifest, Failure> {
    let limits = Limits {
        packs: sink.pack_slots(),
        large: sink.large_slots(),
        ..Limits::real()
    };
    execute_with(cli, source, sink, &limits).await
}

/// [`execute`] within `limits`: the placement limits are a parameter so a test can use small packs.
/// A sink or limits with more pack or large slots than a manifest can name (8 of each) is a setup
/// failure (exit 14) before anything is fetched.
pub(crate) async fn execute_with(
    cli: &Cli,
    source: &dyn BlobSource,
    sink: &dyn PackSink,
    limits: &Limits,
) -> Result<TaskManifest, Failure> {
    if limits.packs > usize::from(MAX_PACKS) || limits.large > usize::from(MAX_LARGE) {
        return Err(Failure::Setup(format!(
            "{} pack slots and {} large slots; a manifest names at most {MAX_PACKS} and {MAX_LARGE}",
            limits.packs, limits.large
        )));
    }
    let fetching = Instant::now();
    let action_digest = digest::to_reapi(&cli.action);
    let action: Action = fetch_message(source, &action_digest)?;
    let command: Command =
        fetch_message(source, &named("command", action.command_digest.as_ref())?)?;
    // Read before the inputs are fetched: a refused property costs no download.
    let network = network::from_platform(&action, &command)?;
    if network == network::Network::Allowed {
        eprintln!("{}", network::allowed_notice(&cli.action));
    }
    let input_root = named("input root", action.input_root_digest.as_ref())?;
    let root = cli.work_dir.join("in");
    materialize(
        source,
        &input_root,
        &root,
        usize::from(cli.fetch_concurrency),
    )?;
    let fetch_ms = millis(fetching.elapsed());
    if let Some(report) = source.report() {
        eprintln!("kbf-task: {report}");
    }

    let stdout = cli.work_dir.join("stdout");
    let stderr = cli.work_dir.join("stderr");
    let private = cli.work_dir.join("private");
    let unreadable = credential_files(cli);
    let ran = run(
        &command,
        &Run {
            root: &root,
            private: &private,
            stdout: &stdout,
            stderr: &stderr,
            deadline: deadline(&action, Duration::from_millis(cli.deadline_ms)),
            network,
            unreadable: &unreadable,
        },
    )
    .await?;
    if network != network::Network::Allowed {
        eprintln!("{}", network::layers_notice(&cli.action, &ran.layers));
    }

    let packing = Instant::now();
    let paths: Vec<String> = output_paths(&command).into_iter().cloned().collect();
    let outputs = collect_outputs(
        &root,
        &command.working_directory,
        &paths,
        &stdout,
        &stderr,
        cli.limits,
        output_cap(limits),
    )
    .await?;
    // The manifest is part of pack 0, so the time to build the packs ends here.
    let pack_ms = millis(packing.elapsed());
    let result = &outputs.result;
    let mut manifest = TaskManifest {
        lease: cli.lease.clone(),
        action: cli.action,
        exit_code: ran.exit_code,
        stdout: stdio(result.stdout_digest.as_ref())?,
        stderr: stdio(result.stderr_digest.as_ref())?,
        output_files: Vec::new(),
        output_directories: Vec::new(),
        output_symlinks: Vec::new(),
        timing: Timing {
            fetch_ms,
            run_ms: ran.wall_ms,
            pack_ms,
        },
        usage: Usage {
            cpu_usage_us: ran.cpu_us,
            memory_peak_bytes: ran.peak_rss_bytes,
        },
        packs: 1,
        large: Vec::new(),
        inputs: None,
    };
    for file in &result.output_files {
        manifest.output_files.push(OutputFile {
            path: file.path.clone(),
            digest: digest_of(file.digest.as_ref())?,
            is_executable: file.is_executable,
        });
    }
    for dir in &result.output_directories {
        manifest.output_directories.push(OutputDirectory {
            path: dir.path.clone(),
            tree: digest_of(dir.tree_digest.as_ref())?,
        });
    }
    for link in &result.output_symlinks {
        manifest.output_symlinks.push(OutputSymlink {
            path: link.path.clone(),
            target: link.target.clone(),
        });
    }
    store_outputs(&mut manifest, &outputs.blobs, limits, sink)?;
    Ok(manifest)
}

/// Plans `blobs` into the slots `limits` allows, sets `manifest.packs` and `manifest.large`, and
/// stores the large objects and the packs, in this order: large objects, packs `N-1` down to 1,
/// then pack 0, the commit point.
///
/// Nothing is stored until pack 0 has been built, validated and read back, and every other pack
/// has been built once; a blob that no longer has the bytes it was hashed with fails there (exit
/// 12, detail `output-changed`). Memory: pack 0 is kept built; each other pack is built once to
/// check it and dropped, and built again when its turn comes, so besides pack 0 there are, at
/// one time, the blobs read for one pack and the pack built from them: about three packs of
/// 128 MiB, and the blobs of every pack but pack 0 are read twice.
///
/// # Errors
/// A [`Failure`]: a pack refusal (exit 12) before the first PUT, or the first refused PUT (exit
/// 13), after which nothing more is stored.
fn store_outputs(
    manifest: &mut TaskManifest,
    blobs: &Blobs,
    limits: &Limits,
    sink: &dyn PackSink,
) -> Result<(), Failure> {
    // Plan before anything is built or stored: pack 0 keeps room for the manifest as it will be
    // written, with its `packs` and `large` fields.
    let reserve = manifest_reserve(manifest, limits)?;
    let sizes = blobs
        .digests()
        .map(|d| digest_of(Some(&d)).map(|d| (d, d.size_bytes)))
        .collect::<Result<Vec<_>, _>>()?;
    let planned = plan(&sizes, reserve, limits).map_err(PackError::from)?;
    // At most `limits.packs`, which `execute_with` keeps within MAX_PACKS; a count that did not
    // fit a byte would be refused by the manifest's own check.
    manifest.packs = u8::try_from(planned.packs.len()).unwrap_or(u8::MAX);
    manifest.large = planned
        .large
        .iter()
        .zip(0..)
        .map(|(digest, slot)| LargeEntry {
            slot,
            digest: *digest,
        })
        .collect();

    let pack_0 = {
        let held = held(blobs, &planned.packs[0])?;
        let views: Vec<&[u8]> = held.iter().map(|bytes| bytes.as_ref()).collect();
        build(manifest, &views)?
    };
    for slot in 1..planned.packs.len() {
        extra_pack(blobs, &planned.packs[slot])?;
    }
    for (slot, digest) in planned.large.iter().enumerate() {
        sink.store_large(slot, blobs, digest)?;
    }
    for slot in (1..planned.packs.len()).rev() {
        sink.store_pack(slot, &extra_pack(blobs, &planned.packs[slot])?)?;
    }
    sink.store_pack(0, &pack_0)?;
    Ok(())
}

/// The digest an Action field names, or a fetch failure saying which field is missing.
fn named(what: &str, digest: Option<&reapi::Digest>) -> Result<reapi::Digest, FetchError> {
    digest
        .cloned()
        .ok_or_else(|| FetchError::Invalid(format!("the Action names no {what}")))
}

/// A result's digest as the manifest writes it.
fn digest_of(digest: Option<&reapi::Digest>) -> Result<Digest, Failure> {
    let digest = digest.ok_or_else(|| Failure::Result("a digest is absent".to_owned()))?;
    digest::from_reapi(digest).map_err(|e| Failure::Result(e.to_string()))
}

/// A stream's digest as the manifest writes it: absent when the stream is empty.
fn stdio(digest: Option<&reapi::Digest>) -> Result<Option<Digest>, Failure> {
    Ok(Some(digest_of(digest)?).filter(|d| d.size_bytes != 0))
}

/// How long the Command may run: the Action's timeout when it sets a positive one, but never more
/// than `cap`, the operator's limit; else `cap`.
fn deadline(action: &Action, cap: Duration) -> Duration {
    match &action.timeout {
        Some(t) if t.seconds > 0 || (t.seconds == 0 && t.nanos > 0) => {
            Duration::new(t.seconds.unsigned_abs(), t.nanos.unsigned_abs()).min(cap)
        }
        _ => cap,
    }
}

/// A duration in whole milliseconds, saturating.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Refuses a credential given as a flag value unless `--insecure-argv-secrets` allows it. The
/// message names the flags, never their values.
fn check_argv_secrets(cli: &Cli) -> Result<(), Failure> {
    let used = [
        ("--pack-url", cli.pack_url.is_some()),
        ("--put-urls", cli.put_urls.is_some()),
        ("--grant-token", cli.grant_token.is_some()),
    ];
    match used.iter().find(|(_, set)| *set) {
        Some((flag, _)) if !cli.insecure_argv_secrets => Err(Failure::Setup(format!(
            "{flag} puts a credential in argv, where the Command can read it; \
             pass --insecure-argv-secrets to accept that, or use the file form"
        ))),
        _ => Ok(()),
    }
}

/// Refuses a lease the frontend would refuse in a manifest ([`check_build`]), before anything is
/// fetched: the Command would run for nothing, and its pack would be refused after the PUT.
fn check_lease(cli: &Cli) -> Result<(), Failure> {
    check_build(&cli.lease).map_err(|e| Failure::Setup(format!("--lease: {e}")))
}

/// Refuses an `http` grant URL unless `--allow-plain-http-grant` was given, before anything is
/// requested. The message names the flag, not the URL, whose query may be a credential.
fn check_grant_scheme(cli: &Cli) -> Result<(), Failure> {
    let plain = cli
        .grant_url
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http://"));
    if plain && !cli.allow_plain_http_grant {
        return Err(Failure::Setup(
            "--grant-url is an http URL, which would send the bearer token in the clear; \
             pass --allow-plain-http-grant to read the grant without TLS"
                .to_owned(),
        ));
    }
    Ok(())
}

/// A credential from the file or the flag value the command line named, a trailing newline
/// dropped, and the file (when it was one) left in place. `what` names the credential. The
/// file's protection is the executor's per-run directory, not its mode (the executor delivers it
/// read-only), so its permissions are not checked.
fn credential(what: &str, file: Option<&Path>, flag: Option<&str>) -> Result<String, Failure> {
    let (raw, from) = match (file, flag) {
        (Some(path), _) => (
            std::fs::read_to_string(path)
                .map_err(|e| Failure::Setup(format!("{}: {e}", path.display())))?,
            path.display().to_string(),
        ),
        (None, Some(value)) => (value.to_owned(), format!("the {what} flag")),
        (None, None) => return Err(Failure::Setup(format!("no {what} was given"))),
    };
    let value = raw.trim_end_matches(['\r', '\n']);
    if value.is_empty() {
        return Err(Failure::Setup(format!("{from} is empty")));
    }
    Ok(value.to_owned())
}

/// The URLs the run PUTs to: the key set of `--put-urls-file`/`--put-urls`, else the single pack
/// URL of the v1 flags as a key set of one pack and no large object.
fn put_urls(cli: &Cli) -> Result<PutUrls, Failure> {
    let set = PutUrls::from_sources(cli.put_urls_file.as_deref(), cli.put_urls.as_deref())
        .map_err(|e| Failure::Setup(format!("the v2 key set: {e}")))?;
    match set {
        Some(set) => Ok(set),
        None => Ok(PutUrls {
            packs: vec![credential(
                "pack URL",
                cli.pack_url_file.as_deref(),
                cli.pack_url.as_deref(),
            )?],
            large: Vec::new(),
        }),
    }
}

/// The host's roots and the DER certificates `--root-der` names.
fn trusted_roots(cli: &Cli) -> Result<Vec<Vec<u8>>, Failure> {
    let mut roots = native_roots();
    for path in &cli.root_der {
        roots.push(
            std::fs::read(path).map_err(|e| Failure::Setup(format!("{}: {e}", path.display())))?,
        );
    }
    Ok(roots)
}

/// The trusted roots and the real source and sink the flags name.
#[cfg(test)]
fn connect(cli: &Cli) -> Result<(HttpBlobSource, HttpSink), Failure> {
    connect_with_roots(cli).map(|(source, sink, _)| (source, sink))
}

/// [`connect`], and the roots it read, for a caller that needs them too.
fn connect_with_roots(cli: &Cli) -> Result<(HttpBlobSource, HttpSink, Vec<Vec<u8>>), Failure> {
    check_lease(cli)?;
    check_argv_secrets(cli)?;
    check_grant_scheme(cli)?;
    let token = credential(
        "grant token",
        cli.grant_token_file.as_deref(),
        cli.grant_token.as_deref(),
    )?;
    let roots = trusted_roots(cli)?;
    let urls = put_urls(cli)?;
    let source =
        HttpBlobSource::new_with(&cli.grant_url, &token, &roots, cli.allow_plain_http_grant)?
            .with_batch(cli.fetch_batch);
    let sink = HttpSink {
        agent: if cli.allow_plain_http_put {
            agent_with_plain_http(tls_config(&roots))
        } else {
            agent(Some(tls_config(&roots)))
        },
        urls,
        sleeper: Arc::new(ThreadSleeper),
        jitter: Arc::new(RandomJitter),
    };
    Ok((source, sink, roots))
}

/// [`connect`], and with `--locate` the source wrapped to read where the listener's `/locate`
/// points. Without `--locate` the source is the listener's alone and `/locate` is never asked.
fn connect_located(cli: &Cli) -> Result<(Box<dyn BlobSource>, HttpSink), Failure> {
    let (source, sink, roots) = connect_with_roots(cli)?;
    if !cli.locate {
        return Ok((Box::new(source), sink));
    }
    let located = LocatedSource::open(
        source,
        &roots,
        cli.allow_plain_http_grant,
        cli.work_dir.clone(),
    );
    Ok((Box::new(located), sink))
}

/// Runs the task the flags describe against the real grant and the real presigned URL, and turns
/// the outcome into the process exit code (see the module documentation). A failure is written to
/// standard error; no message holds the token or the presigned URL's query.
pub async fn main_with(cli: &Cli) -> ExitCode {
    let connected = connect_located(cli).map(|(source, sink)| {
        let sink: Box<dyn PackSink> = Box::new(sink);
        (source, sink)
    });
    finish(cli, connected).await
}

/// The source and the sink a run uses.
type Connected = (Box<dyn BlobSource>, Box<dyn PackSink>);

/// Runs the task over `connected` (what [`connect`] made, or why it could not) and turns the
/// outcome into the exit code.
async fn finish(cli: &Cli, connected: Result<Connected, Failure>) -> ExitCode {
    let outcome = match connected {
        Ok((source, sink)) => execute(cli, source.as_ref(), sink.as_ref()).await,
        Err(failure) => Err(failure),
    };
    match outcome {
        Ok(_) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("kbf-task: {failure}");
            ExitCode::from(failure.exit_code())
        }
    }
}

/// [`run_cli`] once `protection` (what [`network::protect_this_process`] returned, called before
/// anything else) is known to have worked: if it did not, the tool stops with the setup exit code
/// rather than run with the Command able to take its sockets.
pub fn run_cli_with(protection: std::io::Result<()>, cli: &Cli) -> ExitCode {
    match protection {
        Ok(()) => run_cli(cli),
        Err(e) => {
            eprintln!("kbf-task: cannot make this process non-dumpable: {e}");
            ExitCode::from(EXIT_SETUP)
        }
    }
}

/// Runs the task the flags describe on a fresh single-threaded runtime and returns the exit code;
/// a runtime that cannot start is a setup failure.
pub fn run_cli(cli: &Cli) -> ExitCode {
    start(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build(),
        cli,
    )
}

fn start(runtime: std::io::Result<tokio::runtime::Runtime>, cli: &Cli) -> ExitCode {
    match runtime {
        Ok(runtime) => runtime.block_on(main_with(cli)),
        Err(e) => {
            eprintln!("kbf-task: cannot start the async runtime: {e}");
            ExitCode::from(EXIT_SETUP)
        }
    }
}

#[cfg(test)]
#[path = "task_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "credential_files_tests.rs"]
mod credential_files_tests;

#[cfg(test)]
#[path = "plain_grant_tests.rs"]
mod plain_grant_tests;

#[cfg(test)]
#[path = "locate_flags_tests.rs"]
mod locate_flags_tests;

#[cfg(test)]
#[path = "put_urls_flag_tests.rs"]
mod put_urls_flag_tests;

#[cfg(test)]
#[path = "recording_sink.rs"]
pub(crate) mod recording_sink;
