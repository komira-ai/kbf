//! The task tool's driver: the flags, the order of the steps, and the exit code of each way they
//! can fail.
//!
//! [`main_with`] runs a tree job ([`crate::tree`]): the Action, the Command and the input root
//! are read from the job's root, the Command runs there, its outputs are collected into the run's
//! one object as they are read ([`collected`]), and the object is committed into the put
//! directory ([`crate::out_dir`]).
//!
//! # Exit codes
//!
//! The exit code says whether the farm did its part, not whether the action succeeded: the
//! action's own exit code is in the manifest.
//!
//! | code | meaning |
//! |------|---------|
//! | 0 | the action ran (whatever its exit code, a timeout included) and its object was committed |
//! | 2 | the flags are wrong (clap) |
//! | 10 | `.kbf/action` or `.kbf/command` could not be read, is not the blob its digest names (the Action's is `--action`) or does not decode, or `work` is not a directory |
//! | 11 | run-start: the Command could not be started (bad paths, no program, a `kbf-network` property other than `allow`), or the run's `private/` home, tmp and cache directories, or its run directory `.kbf-run` (an existing one is refused), could not be made or opened |
//! | 12 | pack: the outputs could not be collected, or made a manifest or object the frontend would refuse (`invalid-manifest`, `invalid-pack`, `output-too-large`) |
//! | 13 | put: an object could not be written to the put directory or its commit `rename` failed |
//! | 14 | setup: `--lease` is not a build id, `--out-dir` is not a directory, or the runtime did not start |
//! | 15 | no confine: the Command is to be denied the network and this system cannot deny it (neither Linux nor macOS); nothing was run |

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
#[cfg(test)]
use kbf_proto::reapi::Action;
use kbf_task_pack::{
    OutputDirectory, OutputFile, OutputSymlink, TaskManifest, Timing, Usage, check_build,
};
use kbf_types::Digest;

use crate::digest;
use crate::inputs::Inputs;
use crate::message::FetchError;
use crate::outputs::OutputLimits;
use crate::pack::PackError;
use crate::run::{Job, Run, RunError, output_paths, run};
use crate::sandbox;
use crate::spool::{self, ObjectFormat, Spool};

/// Exit code: the Action, the Command or the input root could not be read, checked or used.
pub const EXIT_FETCH: u8 = 10;
/// Exit code: the Command could not be started.
pub const EXIT_RUN: u8 = 11;
/// Exit code: the outputs could not be collected or packed.
pub const EXIT_PACK: u8 = 12;
/// Exit code: the object could not be stored.
pub const EXIT_PUT: u8 = 13;
/// Exit code: a flag is unusable, or the runtime did not start.
pub const EXIT_SETUP: u8 = 14;
/// Exit code: the Command is to be denied the network and this system (neither Linux nor macOS) has no confine that can.
pub const EXIT_NO_CONFINE: u8 = 15;

/// `kbf-task`: runs one build action inside a job and commits its one object into the put
/// directory.
///
/// A tree job is the only kind: the job's root file system is the action's tree, which the
/// frontend wrote, and the job has no network, so the tool reads no grant and sends nothing. A
/// frontend of an earlier release that sends the flags of the presigned mode (`--grant-url`,
/// `--pack-url-file` and the others) is refused by the flag parser (exit 2).
///
/// # What the manifest claims
///
/// The manifest's `exit_code`, `timing` and `usage` are written by this tool, which runs as the
/// same user as the Command: they are claims, and the frontend does not trust them.
#[derive(Clone, Parser)]
#[command(
    name = "kbf-task",
    version = concat!(env!("CARGO_PKG_VERSION"), "+", env!("KBF_BUILD_COMMIT")),
)]
pub struct Cli {
    /// Run the tree job whose root file system is the action's tree, rooted at `ROOT`
    /// (`--tree` alone: `/`): the Action and Command are read from `<ROOT>/.kbf`, the inputs are
    /// `<ROOT>/work` and the run directory is `<ROOT>/.kbf-run` (see the `tree` module). The
    /// root must follow `=`.
    #[arg(
        long,
        value_name = "ROOT",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "/",
        required = true,
    )]
    pub tree: Option<PathBuf>,
    /// Write the run's object into the put directory `--out-dir`: it is written as
    /// `pack.part`, renamed to `pack.sha256-<hex>.part`, then committed by renaming it to `pack`
    /// (see `crate::out_dir`). It is the only way to store an object, so the flag is required.
    #[arg(long, required = true)]
    pub stage_and_commit: bool,
    /// The put directory of `--stage-and-commit` (default `/out`); it must be a directory when
    /// the task starts.
    #[arg(long, value_name = "DIR")]
    pub out_dir: Option<PathBuf>,
    /// The shape of what is written: `one-pass`, the single object `pack` written while the
    /// outputs are read, its manifest and index at the end (`crate::spool`,
    /// docs/design/one-pass-task-object.md). It is the only shape, with or without the flag,
    /// which is accepted so that a frontend may send it.
    #[arg(long, value_enum)]
    pub object_format: Option<ObjectFormat>,
    /// The lease id the frontend gave; it is copied into the manifest. It must be a build id, 1 to
    /// 128 bytes of `0-9A-Za-z_-`, the only lease the frontend accepts in a manifest; any other is
    /// refused at startup (exit 14), before anything runs.
    #[arg(long)]
    pub lease: String,
    /// The Action to run, as `<64 lowercase hex digits>/<size>`.
    #[arg(long, value_parser = digest::parse)]
    pub action: Digest,
    /// The longest the Command may run, in milliseconds: the Action's timeout when it sets a
    /// shorter positive one, else this.
    #[arg(long, default_value_t = 3_600_000)]
    pub deadline_ms: u64,
    /// The limits on what the action may leave.
    #[command(flatten)]
    pub limits: OutputLimits,
    /// Not a flag: the binary whose relay starts the Command in a PID namespace of its own
    /// ([`crate::run::Run::relay`]). The binary's own start sets it to
    /// its own path ([`run_cli_with`]); a task run in another process (a test)
    /// has none, and its Command gets no PID namespace.
    #[arg(skip)]
    pub relay: Option<PathBuf>,
    /// Not a flag: make this process the subreaper of what the Command leaves, and end and reap
    /// it all ([`crate::run::Job`]). The binary's own start sets it ([`run_cli_with`]); a task
    /// run in another process (a test) has none, and only the Command's process group is ended.
    #[arg(skip)]
    pub subreaper: bool,
}

impl Cli {
    /// The put directory of `--stage-and-commit`: `--out-dir`, or `/out`.
    #[must_use]
    pub fn out_dir(&self) -> &Path {
        self.out_dir.as_deref().unwrap_or(Path::new("/out"))
    }

    /// The root of the tree job, `--tree`'s value (clap requires the flag).
    #[must_use]
    pub fn root(&self) -> &Path {
        self.tree.as_deref().unwrap_or(Path::new("/"))
    }
}

/// Why a task did not complete; each class has its own exit code.
#[derive(Debug, thiserror::Error)]
pub enum Failure {
    /// A flag is unusable.
    #[error("setup: {0}")]
    Setup(String),
    /// The Action, the Command or the input root could not be read, checked or used.
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
    /// The manifest or the object is one the frontend would refuse: a size cap (detail
    /// `output-too-large`), or a path, a target, a number or a digest the shared rules refuse.
    #[error("pack: {0}")]
    Pack(#[from] PackError),
    /// An object could not be written to or committed in the put directory.
    #[error("put: {0}")]
    OutDir(#[from] crate::out_dir::OutDirError),
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
            Failure::OutDir(_) => EXIT_PUT,
        }
    }
}

/// Refuses a lease the frontend would refuse in a manifest ([`check_build`]), before anything
/// runs: the Command would run for nothing, and its object would be refused after the commit.
pub(crate) fn check_lease(cli: &Cli) -> Result<(), Failure> {
    check_build(&cli.lease).map_err(|e| Failure::Setup(format!("--lease: {e}")))
}

/// How the Command is run: in a job, which is its sandbox, directly ([`Job`]: its group is ended,
/// and with `Cli::subreaper` every process it left).
pub(crate) fn job_of(cli: &Cli) -> Job {
    if cli.subreaper {
        Job::Subreaper
    } else {
        Job::Group
    }
}

/// Runs the Command and collects its outputs into `spool`: the manifest (`pack_ms` the time the
/// outputs took to collect, one pack and no large object) for the object to end with.
pub(crate) async fn collected(
    cli: &Cli,
    inputs: Inputs,
    spool: &Arc<Spool>,
) -> Result<TaskManifest, Failure> {
    let Inputs {
        action,
        command,
        network,
        root,
        run_dir,
        hidden,
        fetch_ms,
        stats,
    } = inputs;
    let stdout = run_dir.join("stdout");
    let stderr = run_dir.join("stderr");
    let private = run_dir.join("private");
    let ran = run(
        &command,
        &Run {
            root: &root,
            private: &private,
            stdout: &stdout,
            stderr: &stderr,
            deadline: deadline(&action, Duration::from_millis(cli.deadline_ms)),
            network,
            unreadable: &[],
            hidden: &hidden,
            relay: cli.relay.as_deref(),
            job: Some(job_of(cli)),
        },
    )
    .await?;
    // None only where no sandbox is built (not Linux).
    sandbox::notice(&cli.action, &ran.sandbox)
        .into_iter()
        .for_each(|line| eprintln!("{line}"));
    // Nothing is said of a job's layers: the job is the sandbox, and no layer is made.

    let packing = Instant::now();
    let paths: Vec<String> = output_paths(&command).into_iter().cloned().collect();
    let result = spool::collect(
        spool,
        (
            root.as_path(),
            command.working_directory.as_str(),
            paths.as_slice(),
        ),
        (&stdout, &stderr),
        cli.limits,
    )
    .await?;
    // The manifest is part of the object, so the time to build it ends here.
    let pack_ms = millis(packing.elapsed());
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
            ..Timing::default()
        },
        usage: Usage {
            cpu_usage_us: ran.cpu_us,
            memory_peak_bytes: ran.peak_rss_bytes,
        },
        packs: 1,
        large: Vec::new(),
        inputs: stats,
        objects: None,
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
    Ok(manifest)
}

pub(crate) use crate::task_util::{deadline, digest_of, millis, named, stdio};

/// Runs the task the flags describe and turns the outcome into the process exit code (see the
/// module documentation). A failure is written to standard error.
pub async fn main_with(cli: &Cli) -> ExitCode {
    crate::tree::finish_out_dir(cli, cli.root()).await
}

/// The exit code of a run's outcome; a failure is written to standard error first.
pub(crate) fn exit_code(outcome: Result<TaskManifest, Failure>) -> ExitCode {
    match outcome {
        Ok(_) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("kbf-task: {failure}");
            ExitCode::from(failure.exit_code())
        }
    }
}

/// [`run_cli`] once `protection` (what [`crate::network::protect_this_process`] returned, called before
/// anything else) is known to have worked: if it did not, the tool stops with the setup exit code
/// rather than run with the Command able to take its sockets.
pub fn run_cli_with(protection: std::io::Result<()>, cli: &Cli) -> ExitCode {
    match protection {
        Ok(()) => {
            let mut cli = relayed(cli);
            cli.subreaper = true;
            run_cli(&cli)
        }
        Err(e) => {
            eprintln!("kbf-task: cannot make this process non-dumpable: {e}");
            ExitCode::from(EXIT_SETUP)
        }
    }
}

/// `cli` with this binary as the relay of its Command ([`Cli::relay`]), named by the path it
/// was started by: the child that executes the relay, once it has made its user namespace, was
/// refused `/proc/self/exe` itself (`EACCES`) on the hosts this was tried on.
#[cfg(target_os = "linux")]
fn relayed(cli: &Cli) -> Cli {
    let mut cli = cli.clone();
    cli.relay = std::env::current_exe().ok();
    cli
}

/// No relay on this system: `cli` itself.
#[cfg(not(target_os = "linux"))]
fn relayed(cli: &Cli) -> Cli {
    cli.clone()
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
