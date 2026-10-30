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
//! | 12 | pack: the outputs could not be collected, did not fit one pack (`output-too-large`), or made a manifest or pack the frontend would refuse (`invalid-manifest`, `invalid-pack`) |
//! | 13 | put: the pack could not be stored (refused, key already taken, still no room after the retries of a 503, unreachable) |
//! | 14 | setup: `--lease` is not a build id, a credential or root certificate is unusable, a credential given as a flag value without `--insecure-argv-secrets`, or the runtime did not start |

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{ArgGroup, Parser};
use kbf_proto::reapi::{self, Action, Command};
use kbf_task_pack::{
    OutputDirectory, OutputFile, OutputSymlink, TaskManifest, Timing, Usage, check_build,
};
use kbf_types::Digest;

use crate::digest;
use crate::fetch::{
    BlobSource, FetchError, HttpBlobSource, fetch_message, materialize, native_roots,
};
use crate::network;
use crate::outputs::{OutputLimits, collect_outputs};
use crate::pack::{MAX_PACK_BYTES, PackError, build};
use crate::put::{
    PutError, Sleeper, ThreadSleeper, agent, agent_with_plain_http, put, put_retrying, tls_config,
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
/// Today only the first pack slot is used, exactly as v1 uses its one pack URL; the other slots
/// are ignored, and the planning of several packs and the large-object PUT are not wired in.
///
/// # What the manifest claims
///
/// The manifest's `exit_code`, `timing` and `usage` are written by this tool, which runs as the
/// same user as the Command: they are claims, and the frontend does not trust them. The pack is
/// built in memory (the collected blobs, the writer's buffer and the finished pack), so the tool's
/// peak memory is about three times the pack (about 384 MiB at the 128 MiB limit); a job's memory
/// limit must allow for it, or the kill shows as 137, not 12.
///
/// # Storing the pack
///
/// The pack is PUT once, create-only. A 503 (no room in the store) is retried up to three times
/// after the wait its `Retry-After` names, at most 30 s; every other answer is final, and a 412
/// means the key exists (a re-attempt of the task gets a new key).
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
    /// `--pack-url-file`: read, never deleted, and a credential like the pack URL. v2 key set:
    /// today only the first pack slot is used, as the single pack URL of the v1 flags; the other
    /// pack slots and the large slots are checked and ignored.
    #[arg(long)]
    pub put_urls_file: Option<PathBuf>,
    /// The v2 key set itself. Stopgap (komira-ai/kbf#1934): it is in argv, so it needs
    /// `--insecure-argv-secrets`. v2 key set: today only the first pack slot is used.
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
            Failure::Run(_) => EXIT_RUN,
            Failure::Outputs(_) | Failure::Result(_) | Failure::Read(_) | Failure::Pack(_) => {
                EXIT_PACK
            }
            Failure::Put(_) => EXIT_PUT,
        }
    }
}

/// Where the finished pack goes.
pub trait PackSink {
    /// Stores `body`, once.
    ///
    /// # Errors
    /// The pack was not stored.
    fn store(&self, body: &[u8]) -> Result<(), PutError>;
}

/// The real sink: the create-only PUT ([`put`]) to the presigned URL it holds, repeated after a
/// 503 only ([`put_retrying`]), with `sleeper` between the attempts.
pub struct HttpSink {
    agent: ureq::Agent,
    url: String,
    sleeper: Arc<dyn Sleeper>,
}

impl PackSink for HttpSink {
    fn store(&self, body: &[u8]) -> Result<(), PutError> {
        put_retrying(|| put(&self.agent, &self.url, body), self.sleeper.as_ref())
    }
}

/// Runs the task `cli` names, against `source` and `sink`, and returns the manifest that went into
/// the pack.
///
/// The steps run in this order: fetch the Action, its Command and the input tree; run; collect the
/// outputs; build the manifest and the pack; store the pack. The Command's own exit code, or a
/// deadline kill, is in the manifest and is not a failure. The manifest has no time for the PUT: the
/// pack holds the manifest, so it cannot hold the time the pack took to store. An empty stdout or
/// stderr is left out of the manifest, and no empty blob is a record of the pack.
///
/// # Errors
/// A [`Failure`], whose [`Failure::exit_code`] names the step that failed.
pub async fn execute(
    cli: &Cli,
    source: &impl BlobSource,
    sink: &impl PackSink,
) -> Result<TaskManifest, Failure> {
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

    let stdout = cli.work_dir.join("stdout");
    let stderr = cli.work_dir.join("stderr");
    let private = cli.work_dir.join("private");
    let ran = run(
        &command,
        &Run {
            root: &root,
            private: &private,
            stdout: &stdout,
            stderr: &stderr,
            deadline: deadline(&action, Duration::from_millis(cli.deadline_ms)),
            network,
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
        MAX_PACK_BYTES,
    )
    .await?;
    let mut held: Vec<Cow<'_, [u8]>> = Vec::new();
    for digest in outputs.blobs.digests() {
        held.extend(outputs.blobs.read(&digest)?);
    }
    let blobs: Vec<&[u8]> = held.iter().map(|bytes| bytes.as_ref()).collect();
    // The manifest is part of the pack, so the time to build the pack ends here.
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
    sink.store(&build(&manifest, &blobs)?)?;
    Ok(manifest)
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
/// dropped, and the file (when it was one) left in place. `what` names the credential.
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

/// The URL the pack is PUT to: the single pack URL of the v1 flags, or, with a v2 key set, its
/// first pack slot. The other slots are validated with the set and not used yet.
fn pack_url(cli: &Cli) -> Result<String, Failure> {
    let set = PutUrls::from_sources(cli.put_urls_file.as_deref(), cli.put_urls.as_deref())
        .map_err(|e| Failure::Setup(format!("the v2 key set: {e}")))?;
    match set {
        Some(mut set) => Ok(set.packs.swap_remove(0)),
        None => credential(
            "pack URL",
            cli.pack_url_file.as_deref(),
            cli.pack_url.as_deref(),
        ),
    }
}

/// The trusted roots and the real source and sink the flags name.
fn connect(cli: &Cli) -> Result<(HttpBlobSource, HttpSink), Failure> {
    check_lease(cli)?;
    check_argv_secrets(cli)?;
    check_grant_scheme(cli)?;
    let token = credential(
        "grant token",
        cli.grant_token_file.as_deref(),
        cli.grant_token.as_deref(),
    )?;
    let mut roots = native_roots();
    for path in &cli.root_der {
        roots.push(
            std::fs::read(path).map_err(|e| Failure::Setup(format!("{}: {e}", path.display())))?,
        );
    }
    let url = pack_url(cli)?;
    let source =
        HttpBlobSource::new_with(&cli.grant_url, &token, &roots, cli.allow_plain_http_grant)?
            .with_batch(cli.fetch_batch);
    let sink = HttpSink {
        agent: if cli.allow_plain_http_put {
            agent_with_plain_http(tls_config(&roots))
        } else {
            agent(Some(tls_config(&roots)))
        },
        url,
        sleeper: Arc::new(ThreadSleeper),
    };
    Ok((source, sink))
}

/// Runs the task the flags describe against the real grant and the real presigned URL, and turns
/// the outcome into the process exit code (see the module documentation). A failure is written to
/// standard error; no message holds the token or the presigned URL's query.
pub async fn main_with(cli: &Cli) -> ExitCode {
    finish(cli, connect(cli)).await
}

/// Runs the task over `connected` (what [`connect`] made, or why it could not) and turns the
/// outcome into the exit code.
async fn finish(
    cli: &Cli,
    connected: Result<(impl BlobSource, impl PackSink), Failure>,
) -> ExitCode {
    let outcome = match connected {
        Ok((source, sink)) => execute(cli, &source, &sink).await,
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
#[path = "plain_grant_tests.rs"]
mod plain_grant_tests;

#[cfg(test)]
#[path = "put_urls_flag_tests.rs"]
mod put_urls_flag_tests;
