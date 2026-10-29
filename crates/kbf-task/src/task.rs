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
//! | 11 | run-start: the Command could not be started (bad paths, no program) |
//! | 12 | pack: the outputs could not be collected or did not fit one pack |
//! | 13 | put: the pack could not be stored (refused, key already taken, unreachable) |
//! | 14 | setup: a credential or root certificate is unusable, a credential given as a flag value without `--insecure-argv-secrets`, or the runtime did not start |

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{ArgGroup, Parser};
use kbf_proto::reapi::{Action, Command, Digest};

use crate::fetch::{
    BlobSource, FetchError, HttpBlobSource, fetch_message, materialize, native_roots,
};
use crate::manifest::{
    BlobId, Manifest, OutputDirectory, OutputFile, OutputSymlink, Timing, Usage, VERSION,
};
use crate::outputs::{OutputLimits, collect_outputs};
use crate::pack::{Blob, MAX_PACK_BYTES, PackError, pack};
use crate::put::{PutError, agent, put, tls_config};
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
#[derive(Parser)]
#[command(
    name = "kbf-task",
    version = concat!(env!("CARGO_PKG_VERSION"), "+", env!("KBF_BUILD_COMMIT")),
    group(ArgGroup::new("pack_url_source").required(true).args(["pack_url_file", "pack_url"])),
    group(ArgGroup::new("grant_token_source").required(true).args(["grant_token_file", "grant_token"]))
)]
pub struct Cli {
    /// A file holding the presigned URL the pack is PUT to, once. The URL is a credential, so by
    /// default it is not a flag value (argv is readable by the Command): the file is read, and
    /// removed, before the Command starts.
    #[arg(long)]
    pub pack_url_file: Option<PathBuf>,
    /// The presigned URL itself. Stopgap (komira-ai/kbf#1934): it is in argv, so it needs
    /// `--insecure-argv-secrets`.
    #[arg(long)]
    pub pack_url: Option<String>,
    /// The base URL of the read grant: blobs are read from `<base>/<hex>/<size>`.
    #[arg(long)]
    pub grant_url: String,
    /// A file holding the read grant's bearer token (a trailing newline is dropped).
    #[arg(long)]
    pub grant_token_file: Option<PathBuf>,
    /// The bearer token itself. Stopgap (komira-ai/kbf#1934): it is in argv, so it needs
    /// `--insecure-argv-secrets`.
    #[arg(long)]
    pub grant_token: Option<String>,
    /// Accept `--pack-url` and `--grant-token`, whose values are visible in argv to the Command and
    /// to every process on the host. Removed when the job scheduler delivers job secrets as files
    /// (komira-ai/kbf#1934).
    #[arg(long)]
    pub insecure_argv_secrets: bool,
    /// The lease id the frontend gave; it is copied into the manifest.
    #[arg(long)]
    pub lease: String,
    /// The Action to run, as `<hex>/<size>`.
    #[arg(long)]
    pub action: BlobId,
    /// Where the inputs are laid out (`in/` under it) and the output streams are written.
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
    /// The pack could not be built.
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

/// The real sink: one create-only PUT ([`put`]) to the presigned URL it holds.
pub struct HttpSink {
    agent: ureq::Agent,
    url: String,
}

impl PackSink for HttpSink {
    fn store(&self, body: &[u8]) -> Result<(), PutError> {
        put(&self.agent, &self.url, body)
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
) -> Result<Manifest, Failure> {
    let fetching = Instant::now();
    let action_digest = Digest::from(cli.action);
    let action: Action = fetch_message(source, &action_digest)?;
    let command: Command =
        fetch_message(source, &named("command", action.command_digest.as_ref())?)?;
    let input_root = named("input root", action.input_root_digest.as_ref())?;
    let root = cli.work_dir.join("in");
    materialize(source, &input_root, &root)?;
    let fetch_ms = millis(fetching.elapsed());

    let stdout = cli.work_dir.join("stdout");
    let stderr = cli.work_dir.join("stderr");
    let ran = run(
        &command,
        &Run {
            root: &root,
            stdout: &stdout,
            stderr: &stderr,
            deadline: deadline(&action, Duration::from_millis(cli.deadline_ms)),
        },
    )
    .await?;

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
    let mut blobs = Vec::new();
    for digest in outputs.blobs.digests() {
        blobs.extend(
            outputs
                .blobs
                .read(&digest)?
                .map(|bytes| Blob::Bytes(bytes.into_owned())),
        );
    }
    // The manifest is part of the pack, so the time to build the pack ends here.
    let pack_ms = millis(packing.elapsed());
    let result = &outputs.result;
    let mut manifest = Manifest {
        version: VERSION,
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
    };
    for file in &result.output_files {
        manifest.output_files.push(OutputFile {
            path: file.path.clone(),
            digest: blob_id(file.digest.as_ref())?,
            is_executable: file.is_executable,
        });
    }
    for dir in &result.output_directories {
        manifest.output_directories.push(OutputDirectory {
            path: dir.path.clone(),
            tree: blob_id(dir.tree_digest.as_ref())?,
        });
    }
    for link in &result.output_symlinks {
        manifest.output_symlinks.push(OutputSymlink {
            path: link.path.clone(),
            target: link.target.clone(),
        });
    }
    sink.store(&pack(&manifest, blobs)?)?;
    Ok(manifest)
}

/// The digest an Action field names, or a fetch failure saying which field is missing.
fn named(what: &str, digest: Option<&Digest>) -> Result<Digest, FetchError> {
    digest
        .cloned()
        .ok_or_else(|| FetchError::Invalid(format!("the Action names no {what}")))
}

/// A result's digest as the manifest writes it.
fn blob_id(digest: Option<&Digest>) -> Result<BlobId, Failure> {
    let digest = digest.ok_or_else(|| Failure::Result("a digest is absent".to_owned()))?;
    BlobId::try_from(digest).map_err(|e| Failure::Result(e.to_string()))
}

/// A stream's digest as the manifest writes it: absent when the stream is empty.
fn stdio(digest: Option<&Digest>) -> Result<Option<BlobId>, Failure> {
    Ok(Some(blob_id(digest)?).filter(|id| !id.is_empty()))
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

/// The trusted roots and the real source and sink the flags name.
fn connect(cli: &Cli) -> Result<(HttpBlobSource, HttpSink), Failure> {
    check_argv_secrets(cli)?;
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
    let url = credential(
        "pack URL",
        cli.pack_url_file.as_deref(),
        cli.pack_url.as_deref(),
    )?;
    if let Some(path) = &cli.pack_url_file {
        // The Command runs as a child of this process: the URL must not be left where it can read it.
        std::fs::remove_file(path)
            .map_err(|e| Failure::Setup(format!("{}: {e}", path.display())))?;
    }
    let source = HttpBlobSource::new(&cli.grant_url, &token, &roots)?;
    let sink = HttpSink {
        agent: agent(Some(tls_config(&roots))),
        url,
    };
    Ok((source, sink))
}

/// Runs the task the flags describe against the real grant and the real presigned URL, and turns
/// the outcome into the process exit code (see the module documentation). A failure is written to
/// standard error; no message holds the token or the presigned URL's query.
pub async fn main_with(cli: &Cli) -> ExitCode {
    let outcome = match connect(cli) {
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::sync::Mutex;

    use kbf_proto::reapi::command::EnvironmentVariable;
    use kbf_proto::reapi::{Directory, FileNode};
    use kbf_segments::SegmentReader;
    use prost::Message;

    use super::*;
    use crate::fetch::label;
    use crate::pack::verify;

    /// Blobs held in memory by digest.
    #[derive(Default)]
    struct Mem(HashMap<String, Vec<u8>>);

    impl Mem {
        fn add(&mut self, bytes: Vec<u8>) -> Digest {
            let id = BlobId::of(&bytes);
            let digest = Digest::from(id);
            self.0.insert(label(&digest), bytes);
            digest
        }
    }

    impl BlobSource for Mem {
        fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
            self.0
                .get(&label(digest))
                .cloned()
                .ok_or_else(|| FetchError::Source {
                    digest: label(digest),
                    why: "absent".to_owned(),
                })
        }
    }

    /// A sink that keeps what it was given, or refuses.
    struct Fake {
        stored: Mutex<Vec<Vec<u8>>>,
        refuse: bool,
    }

    impl Fake {
        fn new(refuse: bool) -> Self {
            Self {
                stored: Mutex::new(Vec::new()),
                refuse,
            }
        }
    }

    impl PackSink for Fake {
        fn store(&self, body: &[u8]) -> Result<(), PutError> {
            if self.refuse {
                return Err(PutError::Exists {
                    url: "https://store.example/pack".to_owned(),
                });
            }
            self.stored.lock().unwrap().push(body.to_vec());
            Ok(())
        }
    }

    /// A fresh scratch directory for one test, beside the test binary.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kbf-task-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn cli(work_dir: &Path, action: Digest) -> Cli {
        let id = BlobId::try_from(&action).expect("digest");
        Cli::try_parse_from([
            "kbf-task",
            "--pack-url-file",
            "/nonexistent-url",
            "--grant-url",
            "https://grant.example",
            "--grant-token-file",
            "/nonexistent",
            "--lease",
            "lease-1",
            "--action",
            &id.to_string(),
            "--work-dir",
            work_dir.to_str().expect("utf-8"),
        ])
        .expect("flags")
    }

    /// An Action whose Command is `argv` with `outputs`, over an input root holding `data`.
    fn action(mem: &mut Mem, argv: &[&str], outputs: &[&str]) -> Digest {
        let data = mem.add(b"input\n".to_vec());
        let root = mem.add(
            Directory {
                files: vec![FileNode {
                    name: "data".to_owned(),
                    digest: Some(data),
                    is_executable: false,
                    ..Default::default()
                }],
                ..Default::default()
            }
            .encode_to_vec(),
        );
        let command = mem.add(
            Command {
                arguments: argv.iter().map(|a| (*a).to_owned()).collect(),
                environment_variables: vec![EnvironmentVariable {
                    name: "PATH".to_owned(),
                    value: "/usr/bin:/bin".to_owned(),
                }],
                output_paths: outputs.iter().map(|o| (*o).to_owned()).collect(),
                ..Default::default()
            }
            .encode_to_vec(),
        );
        mem.add(
            Action {
                command_digest: Some(command),
                input_root_digest: Some(root),
                ..Default::default()
            }
            .encode_to_vec(),
        )
    }

    const SCRIPT: &str = "echo hi > out; echo err >&2";

    #[tokio::test]
    async fn a_run_stores_a_pack_whose_first_record_is_the_manifest_naming_every_output() {
        let mut mem = Mem::default();
        let digest = action(&mut mem, &["sh", "-c", SCRIPT], &["out"]);
        let dir = scratch("e2e");
        let cli = cli(&dir, digest);
        let sink = Fake::new(false);
        let manifest = execute(&cli, &mem, &sink).await.expect("task");
        let stored = sink.stored.lock().unwrap();
        assert_eq!(stored.len(), 1);
        let body = &stored[0];

        // The frontend's check: the manifest is the record at offset 0 and every digest it names
        // is a record of the pack.
        assert_eq!(verify(body).expect("verify"), manifest);
        assert_eq!(manifest.lease, "lease-1");
        assert_eq!(manifest.action, cli.action);
        assert_eq!(manifest.exit_code, 0);
        assert_eq!(manifest.output_files.len(), 1);
        assert_eq!(manifest.output_files[0].path, "out");
        assert_eq!(manifest.output_files[0].digest, BlobId::of(b"hi\n"));
        assert!(!manifest.output_files[0].is_executable);
        // Stdout is empty: it is left out of the manifest and has no record.
        assert_eq!(manifest.stdout, None);
        assert_eq!(manifest.stderr, Some(BlobId::of(b"err\n")));
        // The manifest, stderr and the one output file: three records, none of them empty.
        let reader = SegmentReader::open(body).expect("segment");
        assert_eq!(reader.footer().entries().len(), 3);
        assert!(
            reader
                .footer()
                .entries()
                .iter()
                .all(|e| e.digest.size_bytes > 0)
        );
        let json = String::from_utf8_lossy(&body[..manifest.encode().len()]).into_owned();
        assert!(
            !json.contains("stdout") && !json.contains("put_ms"),
            "{json}"
        );
    }

    /// An empty output file is listed with the empty digest and the pack holds no record for it.
    #[tokio::test]
    async fn an_empty_output_file_is_listed_without_a_record() {
        let mut mem = Mem::default();
        let digest = action(&mut mem, &["sh", "-c", ": > out"], &["out"]);
        let dir = scratch("empty-out");
        let sink = Fake::new(false);
        let manifest = execute(&cli(&dir, digest), &mem, &sink)
            .await
            .expect("task");
        assert_eq!(manifest.output_files[0].digest, BlobId::of(b""));
        assert_eq!((manifest.stdout, manifest.stderr), (None, None));
        let stored = sink.stored.lock().unwrap();
        assert_eq!(&verify(&stored[0]).expect("verify"), &manifest);
        let reader = SegmentReader::open(&stored[0]).expect("segment");
        assert_eq!(reader.footer().entries().len(), 1);
    }

    #[tokio::test]
    async fn the_actions_exit_code_is_in_the_manifest_and_the_tool_still_succeeds() {
        let mut mem = Mem::default();
        let digest = action(&mut mem, &["sh", "-c", "exit 3"], &[]);
        let dir = scratch("exit3");
        let sink = Fake::new(false);
        let manifest = execute(&cli(&dir, digest), &mem, &sink)
            .await
            .expect("task");
        assert_eq!(manifest.exit_code, 3);
        assert!(manifest.output_files.is_empty());
        assert_eq!(sink.stored.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn output_directories_and_symlinks_reach_the_manifest() {
        let mut mem = Mem::default();
        let script = "mkdir d; echo x > d/f; ln -s d/f link";
        let digest = action(&mut mem, &["sh", "-c", script], &["d", "link"]);
        let dir = scratch("tree");
        let sink = Fake::new(false);
        let manifest = execute(&cli(&dir, digest), &mem, &sink)
            .await
            .expect("task");
        assert_eq!(manifest.output_directories.len(), 1);
        assert_eq!(manifest.output_directories[0].path, "d");
        assert_eq!(manifest.output_symlinks.len(), 1);
        assert_eq!(manifest.output_symlinks[0].target, "d/f");
        let stored = sink.stored.lock().unwrap();
        assert_eq!(&verify(&stored[0]).expect("verify"), &manifest);
    }

    #[tokio::test]
    async fn an_action_that_cannot_be_read_fails_as_a_fetch_and_stores_nothing() {
        let mem = Mem::default();
        let dir = scratch("nofetch");
        let sink = Fake::new(false);
        let failure = execute(&cli(&dir, Digest::from(BlobId::of(b"x"))), &mem, &sink)
            .await
            .expect_err("fetch");
        assert!(matches!(failure, Failure::Fetch(_)), "{failure}");
        assert_eq!(failure.exit_code(), EXIT_FETCH);
        assert!(sink.stored.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_action_without_a_command_or_an_input_root_fails_as_a_fetch() {
        for (command, root, what) in [(false, true, "command"), (true, false, "input root")] {
            let mut mem = Mem::default();
            let present = mem.add(Command::default().encode_to_vec());
            let digest = mem.add(
                Action {
                    command_digest: command.then(|| present.clone()),
                    input_root_digest: root.then(|| present.clone()),
                    ..Default::default()
                }
                .encode_to_vec(),
            );
            let dir = scratch("missing-field");
            let failure = execute(&cli(&dir, digest), &mem, &Fake::new(false))
                .await
                .expect_err("fetch");
            assert!(failure.to_string().contains(what), "{failure}");
            assert_eq!(failure.exit_code(), EXIT_FETCH);
        }
    }

    #[tokio::test]
    async fn a_command_with_no_program_fails_as_a_run_start() {
        let mut mem = Mem::default();
        let digest = action(&mut mem, &[], &[]);
        let dir = scratch("noprog");
        let failure = execute(&cli(&dir, digest), &mem, &Fake::new(false))
            .await
            .expect_err("run");
        assert!(matches!(failure, Failure::Run(_)), "{failure}");
        assert_eq!(failure.exit_code(), EXIT_RUN);
    }

    #[tokio::test]
    async fn outputs_past_a_limit_fail_as_a_pack_and_store_nothing() {
        let mut mem = Mem::default();
        let digest = action(&mut mem, &["sh", "-c", SCRIPT], &["out"]);
        let dir = scratch("limit");
        let mut flags = cli(&dir, digest);
        flags.limits.max_stdio_bytes = 1;
        let sink = Fake::new(false);
        let failure = execute(&flags, &mem, &sink).await.expect_err("limit");
        assert!(matches!(failure, Failure::Outputs(_)), "{failure}");
        assert_eq!(failure.exit_code(), EXIT_PACK);
        assert!(sink.stored.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_pack_the_store_refuses_fails_as_a_put() {
        let mut mem = Mem::default();
        let digest = action(&mut mem, &["sh", "-c", SCRIPT], &["out"]);
        let dir = scratch("refused");
        let failure = execute(&cli(&dir, digest), &mem, &Fake::new(true))
            .await
            .expect_err("put");
        assert!(matches!(failure, Failure::Put(_)), "{failure}");
        assert_eq!(failure.exit_code(), EXIT_PUT);
    }

    #[test]
    fn each_failure_class_has_its_own_exit_code() {
        let codes = [
            Failure::Setup(String::new()).exit_code(),
            Failure::Fetch(FetchError::Invalid(String::new())).exit_code(),
            Failure::Run(RunError::Invalid(String::new())).exit_code(),
            Failure::Result(String::new()).exit_code(),
            Failure::Read(std::io::Error::other("x")).exit_code(),
            Failure::Pack(PackError::TooLarge { limit: 1 }).exit_code(),
            Failure::Put(PutError::Exists { url: String::new() }).exit_code(),
        ];
        assert_eq!(codes, [14, 10, 11, 12, 12, 12, 13]);
    }

    /// Each cap is a pack failure (exit 12) whose message on stderr says `output-too-large`.
    #[test]
    fn every_size_cap_is_exit_12_with_output_too_large() {
        for err in [
            PackError::TooLarge { limit: 1 },
            PackError::TooManyEntries { limit: 1 },
            PackError::RecordTooLarge { limit: 1 },
            PackError::ManifestTooLarge { limit: 1 },
        ] {
            let failure = Failure::Pack(err);
            assert_eq!(failure.exit_code(), EXIT_PACK);
            assert!(
                failure.to_string().contains("output-too-large"),
                "{failure}"
            );
        }
        for err in [
            PackError::PathTooLong { len: 2, limit: 1 },
            PackError::DuplicatePath("p".to_owned()),
        ] {
            assert_eq!(Failure::Pack(err).exit_code(), EXIT_PACK);
        }
    }

    #[test]
    fn a_result_digest_that_is_absent_or_malformed_is_refused() {
        let good = Digest::from(BlobId::of(b"x"));
        assert_eq!(blob_id(Some(&good)).expect("good"), BlobId::of(b"x"));
        assert!(matches!(blob_id(None), Err(Failure::Result(_))));
        let bad = Digest {
            hash: "xyz".to_owned(),
            size_bytes: 1,
        };
        assert!(matches!(blob_id(Some(&bad)), Err(Failure::Result(_))));
    }

    #[test]
    fn the_actions_positive_timeout_replaces_the_default() {
        let default = Duration::from_secs(60);
        let with = |seconds, nanos| Action {
            timeout: Some(prost_types_duration(seconds, nanos)),
            ..Default::default()
        };
        assert_eq!(deadline(&Action::default(), default), default);
        assert_eq!(deadline(&with(5, 0), default), Duration::from_secs(5));
        assert_eq!(deadline(&with(0, 500), default), Duration::from_nanos(500));
        assert_eq!(deadline(&with(0, 0), default), default);
        assert_eq!(deadline(&with(-1, 0), default), default);
        // An Action cannot outlive the operator's limit.
        assert_eq!(deadline(&with(1_000_000_000, 0), default), default);
    }

    /// The Action's `timeout`, whatever type the generated code gives it.
    fn prost_types_duration(seconds: i64, nanos: i32) -> prost_types::Duration {
        prost_types::Duration { seconds, nanos }
    }

    #[test]
    fn the_flags_parse_with_their_defaults() {
        let digest = Digest::from(BlobId::of(b"a"));
        let flags = cli(Path::new("/w"), digest);
        assert_eq!(flags.deadline_ms, 3_600_000);
        assert!(flags.root_der.is_empty());
        let default = Cli::try_parse_from([
            "kbf-task",
            "--pack-url-file=u",
            "--grant-url=g",
            "--grant-token-file=t",
            "--lease=l",
            &format!("--action={}", BlobId::of(b"a")),
        ])
        .expect("flags");
        assert_eq!(default.work_dir, PathBuf::from("/scratch/work"));
        assert!(!default.insecure_argv_secrets);
        // The action must be a digest, and each credential needs a source.
        for bad in [
            vec!["kbf-task", "--action=nothex/1"],
            vec![
                "kbf-task",
                "--grant-url=g",
                "--lease=l",
                "--grant-token-file=t",
            ],
        ] {
            assert!(Cli::try_parse_from(bad).is_err());
        }
    }

    const SECRET_URL: &str = "https://store.example/pack?X-Sig=very-secret";
    const SECRET_TOKEN: &str = "very-secret-token";

    /// Flags with the credentials as the given options, and `insecure` as asked.
    fn with_secrets(args: &[&str]) -> Result<Cli, clap::Error> {
        let action = format!("--action={}", BlobId::of(b"a"));
        let mut all = vec![
            "kbf-task",
            "--grant-url=https://grant.example",
            "--lease=l",
            action.as_str(),
        ];
        all.extend_from_slice(args);
        Cli::try_parse_from(all)
    }

    /// Both credentials as flag values are refused at startup without the opt-in, and the
    /// refusal names the opt-in and neither value.
    #[test]
    fn credential_flags_need_the_insecure_opt_in() {
        for args in [
            vec![
                format!("--pack-url={SECRET_URL}"),
                "--grant-token-file=t".to_owned(),
            ],
            vec![
                "--pack-url-file=u".to_owned(),
                format!("--grant-token={SECRET_TOKEN}"),
            ],
            vec![
                format!("--pack-url={SECRET_URL}"),
                format!("--grant-token={SECRET_TOKEN}"),
            ],
        ] {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let flags = with_secrets(&refs).expect("clap accepts it; the startup check refuses");
            let Err(failure) = check_argv_secrets(&flags) else {
                panic!("accepted without --insecure-argv-secrets");
            };
            assert_eq!(failure.exit_code(), EXIT_SETUP);
            let text = failure.to_string();
            assert!(text.contains("--insecure-argv-secrets"), "{text}");
            assert!(!text.contains("very-secret"), "{text}");
            assert!(matches!(connect(&flags), Err(Failure::Setup(_))));
        }
    }

    #[tokio::test]
    async fn main_with_refuses_a_credential_flag_without_the_opt_in() {
        let flags = with_secrets(&[&format!("--pack-url={SECRET_URL}"), "--grant-token-file=t"])
            .expect("flags");
        assert_eq!(main_with(&flags).await, ExitCode::from(EXIT_SETUP));
    }

    /// With the opt-in both flag values are used, and no file is needed or touched.
    #[test]
    fn credential_flags_work_with_the_insecure_opt_in() {
        let flags = with_secrets(&[
            "--insecure-argv-secrets",
            &format!("--pack-url={SECRET_URL}"),
            &format!("--grant-token={SECRET_TOKEN}"),
        ])
        .expect("flags");
        assert!(flags.insecure_argv_secrets);
        check_argv_secrets(&flags).expect("allowed");
        assert!(connect(&flags).is_ok());
        // The opt-in alone, with the file forms, changes nothing.
        let files = with_secrets(&[
            "--insecure-argv-secrets",
            "--pack-url-file=u",
            "--grant-token-file=t",
        ])
        .expect("flags");
        check_argv_secrets(&files).expect("nothing to refuse");
    }

    /// An empty or absent flag value is a setup failure that names the flag, not the value.
    #[test]
    fn empty_credential_flag_values_are_refused() {
        let mut flags = with_secrets(&[
            "--insecure-argv-secrets",
            &format!("--pack-url={SECRET_URL}"),
            "--grant-token=",
        ])
        .expect("flags");
        let Err(failure) = connect(&flags) else {
            panic!("an empty token was accepted");
        };
        assert!(
            failure.to_string().contains("grant token flag is empty"),
            "{failure}"
        );
        flags.grant_token = Some(SECRET_TOKEN.to_owned());
        flags.pack_url = Some("\n".to_owned());
        assert!(matches!(connect(&flags), Err(Failure::Setup(_))));
        flags.pack_url = None;
        let Err(failure) = connect(&flags) else {
            panic!("no URL was accepted");
        };
        assert!(failure.to_string().contains("no pack URL"), "{failure}");
    }

    /// Each credential has exactly one source: both or neither is a usage error, and the
    /// error text does not echo a value.
    #[test]
    fn each_credential_has_exactly_one_source() {
        let both_url = [
            "--insecure-argv-secrets",
            "--pack-url-file=u",
            "--pack-url=https://store.example/pack?X-Sig=very-secret",
            "--grant-token-file=t",
        ];
        let both_token = [
            "--insecure-argv-secrets",
            "--pack-url-file=u",
            "--grant-token-file=t",
            "--grant-token=very-secret-token",
        ];
        let no_url = ["--grant-token-file=t"];
        let no_token = ["--pack-url-file=u"];
        for args in [&both_url[..], &both_token[..], &no_url[..], &no_token[..]] {
            let Err(err) = with_secrets(args) else {
                panic!("accepted {args:?}");
            };
            assert_eq!(err.exit_code(), 2);
            assert!(!err.to_string().contains("very-secret"), "{err}");
        }
        with_secrets(&["--pack-url-file=u", "--grant-token-file=t"]).expect("one each");
    }

    /// The values do not appear in a setup error raised after they were read.
    #[test]
    fn credential_values_are_not_in_a_setup_error() {
        let dir = scratch("secret-text");
        let flags = with_secrets(&[
            "--insecure-argv-secrets",
            &format!("--pack-url={SECRET_URL}"),
            &format!("--grant-token={SECRET_TOKEN}"),
            &format!("--root-der={}", dir.join("absent.der").display()),
        ])
        .expect("flags");
        let Err(failure) = connect(&flags) else {
            panic!("a missing root was accepted");
        };
        let text = failure.to_string();
        assert!(!text.contains("very-secret"), "{text}");
    }

    #[test]
    fn connect_reads_the_token_file_and_the_extra_roots() {
        let dir = scratch("connect");
        let digest = Digest::from(BlobId::of(b"a"));
        let mut flags = cli(&dir, digest);
        // No token file.
        assert!(matches!(connect(&flags), Err(Failure::Setup(_))));
        // An empty one.
        flags.grant_token_file = Some(dir.join("token"));
        std::fs::write(dir.join("token"), "\n").expect("write");
        assert!(matches!(connect(&flags), Err(Failure::Setup(_))));
        // A token, and a root file that is missing.
        std::fs::write(dir.join("token"), "tok\n").expect("write");
        flags.root_der = vec![dir.join("absent.der")];
        assert!(matches!(connect(&flags), Err(Failure::Setup(_))));
        // Both usable, but the URL file is missing.
        std::fs::write(dir.join("root.der"), [1, 2, 3]).expect("write");
        flags.root_der = vec![dir.join("root.der")];
        let url_file = dir.join("url");
        flags.pack_url_file = Some(url_file.clone());
        assert!(matches!(connect(&flags), Err(Failure::Setup(_))));
        // An empty one.
        std::fs::write(&url_file, "\n").expect("write");
        assert!(matches!(connect(&flags), Err(Failure::Setup(_))));
        // A URL: it is read, and the file is gone before the Command could read it.
        std::fs::write(&url_file, "https://store.example/pack?sig=s\n").expect("write");
        assert!(connect(&flags).is_ok());
        assert!(!url_file.exists());
    }

    #[test]
    fn connect_refuses_a_url_file_it_cannot_remove() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("connect-unremovable");
        let held = dir.join("held");
        std::fs::create_dir(&held).expect("dir");
        let mut flags = cli(&dir, Digest::from(BlobId::of(b"a")));
        flags.grant_token_file = Some(dir.join("token"));
        std::fs::write(dir.join("token"), "tok\n").expect("write");
        flags.pack_url_file = Some(held.join("url"));
        std::fs::write(held.join("url"), "https://store.example/pack?sig=s").expect("write");
        std::fs::set_permissions(&held, std::fs::Permissions::from_mode(0o500)).expect("mode");
        // A superuser removes it anyway; the refusal is checked where the mode is enforced.
        let enforced = std::fs::write(held.join("probe"), b"").is_err();
        let outcome = connect(&flags);
        std::fs::set_permissions(&held, std::fs::Permissions::from_mode(0o700)).expect("mode");
        if enforced {
            assert!(matches!(outcome, Err(Failure::Setup(_))));
        }
    }

    #[tokio::test]
    async fn main_with_reports_a_setup_failure_by_exit_code() {
        let dir = scratch("main-setup");
        let flags = cli(&dir, Digest::from(BlobId::of(b"a")));
        assert_eq!(main_with(&flags).await, ExitCode::from(EXIT_SETUP));
    }

    #[test]
    fn the_http_sink_puts_the_pack_to_the_url() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/pack", listener.local_addr().expect("addr"));
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut got = Vec::new();
            let mut chunk = [0_u8; 512];
            while !got.ends_with(b"ok") {
                let n = stream.read(&mut chunk).expect("read");
                got.extend_from_slice(&chunk[..n]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .expect("reply");
            String::from_utf8_lossy(&got).into_owned()
        });
        let sink = HttpSink {
            agent: agent(None),
            url,
        };
        sink.store(b"ok").expect("stored");
        let request = server.join().expect("server");
        assert!(request.starts_with("PUT /pack "), "{request}");
    }
}
