//! `cargo xtask mac-farm`: runs one Cargo command of this workspace on a Mac of the build farm,
//! from a Linux runner, as one REAPI action (docs/design/mac-ci-on-the-farm.md).
//!
//! A farm Mac runs an action as an ordinary user under a sandbox with no network, and has the
//! Command Line Tools and no Rust. So everything the command needs is an input of the action,
//! gathered here:
//!
//! 1. the files git tracks in the checkout, at their paths;
//! 2. the crates of `Cargo.lock` for `aarch64-apple-darwin`, which `cargo fetch` downloads into a
//!    Cargo home of this run's own (in `--scratch`), below [`CARGO_HOME`];
//! 3. the pinned Rust toolchain for `aarch64-apple-darwin` ([`toolchain`]), below [`TOOLCHAIN`].
//!
//! The Command is the toolchain's `cargo` with the arguments after `--`, from the checkout's
//! root, offline, with the toolchain's `rustc` and `rustdoc` and the Mac's `/usr/bin` (its C
//! compiler and linker). The action's platform is `--properties`, the farm's Mac platform.
//!
//! The blobs the farm's CAS lacks are uploaded, the action is executed and followed until it is
//! done (a stage change is printed: queued while no Mac is free, then running), and the
//! command's stdout and stderr are printed. The verb ends 0 when the command exited 0, and 1
//! for every other end: the command's own non-zero exit, or the farm's refusal or failure,
//! whose status is printed (with the job's output tail when the farm attached one).
//!
//! The verb's first lines mask the address in a workflow's log (`::add-mask::`), so nothing it
//! prints after them shows it. (A workflow step's own header still shows the values of its
//! `env:`; that is the workflow's to keep out.)
//!
//! With an empty `--address` the verb prints a notice and exits 0: a fork, or a repository with
//! no farm, runs nothing and fails nothing.

pub mod client;
pub mod toolchain;
pub mod tree;

#[cfg(test)]
mod tests;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use clap::Args as ClapArgs;
use kbf_proto::google::rpc;
use kbf_proto::reapi::{
    Action, Command, Digest, ExecuteResponse, Platform, command::EnvironmentVariable,
    platform::Property,
};
use prost::Message as _;

use self::client::{Farm, Progress};
use self::tree::{Entry, Tree};
use crate::Error;
use crate::pkg_linux::{Out, spawn};
use crate::prove_farm::parse_properties;

/// Where the toolchain is in the input root.
pub const TOOLCHAIN: &str = ".kbf-mac/toolchain";

/// Where the Cargo home is in the input root.
pub const CARGO_HOME: &str = ".kbf-mac/cargo-home";

/// The directory both are in, which the checkout must not have.
const OURS: &str = ".kbf-mac";

/// The target the crates are fetched for, and the Mac's.
pub const TARGET: &str = "aarch64-apple-darwin";

/// The action's timeout when `--timeout-secs` is not given: under the farm's default limit of
/// 3000 s.
pub const DEFAULT_TIMEOUT_SECS: u64 = 2400;

/// The Mac's programs a build uses: its C compiler and linker, `git`, `xcrun`.
const MAC_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The entries of a Cargo home's top directory that are not sent: Cargo's locks and its cache
/// tracker, which change on every run, and the installed programs.
const HOME_SKIPPED: [&str; 4] = [
    ".global-cache",
    ".package-cache",
    ".package-cache-mutate",
    "bin",
];

/// The reason of the `ErrorInfo` the farm attaches to a failed job: its output tail.
const OUTPUT_TAIL: &str = "JOB_OUTPUT_TAIL";

/// The flags of the verb.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The farm's REAPI address, `grpc://<host>:<port>`. Empty: print a notice and run
    /// nothing.
    #[arg(long)]
    pub address: String,
    /// The instance name sent on every request. Empty: the empty instance.
    #[arg(long, default_value = "")]
    pub instance: String,
    /// Whether the address is served over TLS: `true` or `false`.
    #[arg(long, default_value = "")]
    pub tls: String,
    /// The farm's Mac platform properties, `key=value` separated by commas.
    #[arg(long, default_value = "")]
    pub properties: String,
    /// A directory of this run's own: the Cargo home of `cargo fetch` is made in it.
    #[arg(long)]
    pub scratch: PathBuf,
    /// The action's timeout, in seconds.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout_secs: u64,
    /// The git program that lists the checkout's files.
    #[arg(long, default_value = "git")]
    pub git: PathBuf,
    /// The Cargo program that fetches the crates on this machine.
    #[arg(long = "cargo", default_value = "cargo")]
    pub cargo_program: PathBuf,
    /// The Cargo command to run on the Mac, after `--`: `test -p kbf-outputs --locked`.
    #[arg(last = true, required = true)]
    pub cargo: Vec<String>,
}

/// Why the flags do not make a run.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("the address {0:?} is not grpc://<host>:<port>")]
    Address(String),
    #[error("--tls is {0:?}: it must be `true` or `false`")]
    Tls(String),
    #[error(transparent)]
    Properties(#[from] crate::prove_farm::Problem),
    #[error("--timeout-secs is 0")]
    NoTimeout,
    #[error(
        "the Cargo command is empty, or starts with an option ({0:?}): it starts with its subcommand"
    )]
    Cargo(String),
}

/// The checked settings of a run.
#[derive(Debug, PartialEq, Eq)]
pub struct Settings {
    /// The URL of the farm, `http://` or `https://`.
    pub endpoint: String,
    pub tls: bool,
    pub instance: String,
    /// Sorted by key.
    pub properties: Vec<(String, String)>,
    pub timeout_secs: u64,
    pub cargo: Vec<String>,
}

impl Settings {
    /// The settings `args` name.
    ///
    /// # Errors
    /// The [`Problem`] of the first flag that is wrong.
    pub fn of(args: &Args) -> Result<Settings, Problem> {
        let address = args.address.trim();
        let host = address.strip_prefix("grpc://").unwrap_or_default();
        let port = host.rsplit_once(':').map(|(_, p)| p).unwrap_or_default();
        let plain = !host.contains('/') && !host.chars().any(char::is_whitespace);
        if !plain || port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Problem::Address(address.to_owned()));
        }
        let tls = match args.tls.trim() {
            "true" => true,
            "false" => false,
            other => return Err(Problem::Tls(other.to_owned())),
        };
        let mut properties = parse_properties(&args.properties)?;
        properties.sort();
        if args.timeout_secs == 0 {
            return Err(Problem::NoTimeout);
        }
        match args.cargo.first() {
            Some(sub) if !sub.starts_with('-') && !sub.starts_with('+') => {}
            first => return Err(Problem::Cargo(first.cloned().unwrap_or_default())),
        }
        let scheme = if tls { "https" } else { "http" };
        Ok(Settings {
            endpoint: format!("{scheme}://{host}"),
            tls,
            instance: args.instance.trim().to_owned(),
            properties,
            timeout_secs: args.timeout_secs,
            cargo: args.cargo.clone(),
        })
    }
}

/// The action's platform.
fn platform(s: &Settings) -> Platform {
    Platform {
        properties: s
            .properties
            .iter()
            .map(|(name, value)| Property {
                name: name.clone(),
                value: value.clone(),
            })
            .collect(),
    }
}

/// The Command: the toolchain's `cargo` with the run's arguments, from the input root, its
/// environment sorted by name. Its platform is the Action's ([`action`]).
#[must_use]
pub fn command(s: &Settings) -> Command {
    let env = [
        ("CARGO_HOME", CARGO_HOME.to_owned()),
        ("CARGO_INCREMENTAL", "0".to_owned()),
        ("CARGO_NET_OFFLINE", "true".to_owned()),
        ("CARGO_TERM_COLOR", "always".to_owned()),
        ("PATH", MAC_PATH.to_owned()),
        ("RUSTC", format!("{TOOLCHAIN}/bin/rustc")),
        ("RUSTDOC", format!("{TOOLCHAIN}/bin/rustdoc")),
        ("RUSTFLAGS", "-D warnings".to_owned()),
    ];
    Command {
        arguments: std::iter::once(format!("{TOOLCHAIN}/bin/cargo"))
            .chain(s.cargo.iter().cloned())
            .collect(),
        environment_variables: env
            .into_iter()
            .map(|(name, value)| EnvironmentVariable {
                name: name.to_owned(),
                value,
            })
            .collect(),
        ..Command::default()
    }
}

/// The Action of `command` over the input root `root`; its blobs go into `blobs`.
pub fn action(s: &Settings, root: &Digest, tree: &mut Tree) -> Digest {
    let command = tree::add(&mut tree.blobs, command(s).encode_to_vec());
    let action = Action {
        command_digest: Some(command),
        input_root_digest: Some(root.clone()),
        timeout: Some(prost_types::Duration {
            seconds: i64::try_from(s.timeout_secs).unwrap_or(i64::MAX),
            nanos: 0,
        }),
        platform: Some(platform(s)),
        ..Action::default()
    };
    tree::add(&mut tree.blobs, action.encode_to_vec())
}

/// The entry of the file or symlink at `path`.
fn entry_at(path: &Path) -> Result<Entry, Error> {
    let what = || format!("read {}", path.display());
    let meta = std::fs::symlink_metadata(path).map_err(Error::spawn(what()))?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path).map_err(Error::spawn(what()))?;
        return Ok(Entry::Symlink(target.to_string_lossy().into_owned()));
    }
    if !meta.is_file() {
        return Err(Error::Failed(format!(
            "{} is neither a file nor a symlink",
            path.display()
        )));
    }
    use std::os::unix::fs::PermissionsExt as _;
    let executable = meta.permissions().mode() & 0o111 != 0;
    let bytes = std::fs::read(path).map_err(Error::spawn(what()))?;
    Ok(Entry::File { bytes, executable })
}

/// The entries of the checkout `root`: each of `tracked` (paths relative to it).
///
/// # Errors
/// [`Error`] for a path that cannot be read, or one in the directory the run's own inputs use.
pub fn checkout(root: &Path, tracked: &[String]) -> Result<Vec<(String, Entry)>, Error> {
    let mut out = Vec::new();
    for path in tracked {
        if path == OURS || path.starts_with(&format!("{OURS}/")) {
            return Err(Error::Failed(format!(
                "the checkout tracks {path}, where the run puts its toolchain and crates"
            )));
        }
        out.push((path.clone(), entry_at(&root.join(path))?));
    }
    Ok(out)
}

/// The entries of the Cargo home `home`, below [`CARGO_HOME`], but the top entries of
/// [`HOME_SKIPPED`].
///
/// # Errors
/// [`Error`] for a directory or file that cannot be read.
pub fn cargo_home(home: &Path) -> Result<Vec<(String, Entry)>, Error> {
    let mut out = Vec::new();
    walk(home, CARGO_HOME, true, &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, at: &str, top: bool, out: &mut Vec<(String, Entry)>) -> Result<(), Error> {
    let what = format!("list {}", dir.display());
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .and_then(|d| d.map(|e| e.map(|e| e.file_name())).collect())
        .map_err(Error::spawn(what))?;
    names.sort();
    for name in names {
        let name = name.to_string_lossy().into_owned();
        if top && HOME_SKIPPED.contains(&name.as_str()) {
            continue;
        }
        let path = dir.join(&name);
        let inside = format!("{at}/{name}");
        let meta =
            std::fs::symlink_metadata(&path).map_err(Error::spawn(path.display().to_string()))?;
        if meta.is_dir() {
            walk(&path, &inside, false, out)?;
        } else {
            out.push((inside, entry_at(&path)?));
        }
    }
    Ok(())
}

/// What a run takes from the machine it runs on.
pub struct Host<'a> {
    /// The paths git tracks in the checkout.
    pub tracked: &'a mut dyn FnMut() -> Result<Vec<String>, Error>,
    /// Fetches the crates of `Cargo.lock` for [`TARGET`] into the Cargo home it is given.
    pub fetch: &'a mut dyn FnMut(&Path) -> Result<(), Error>,
    /// The toolchain's pins ([`toolchain::PINS`]).
    pub pins: toolchain::Pins,
    /// Downloads a toolchain tarball.
    pub download: &'a mut toolchain::Download<'a>,
}

/// The input root of the run and its action: every blob of both, and the action's digest.
///
/// # Errors
/// [`Error`] for an input that cannot be gathered.
pub fn inputs(
    s: &Settings,
    root: &Path,
    scratch: &Path,
    host: &mut Host<'_>,
) -> Result<(Tree, Digest), Error> {
    let mut entries = checkout(root, &(host.tracked)()?)?;
    let home = scratch.join("cargo-home");
    std::fs::create_dir_all(&home).map_err(Error::spawn(format!("create {}", home.display())))?;
    (host.fetch)(&home)?;
    entries.extend(cargo_home(&home)?);
    entries.extend(
        toolchain::entries(&host.pins, TOOLCHAIN, host.download)
            .map_err(|e| Error::Failed(e.to_string()))?,
    );
    let mut tree = tree::build(entries).map_err(|e| Error::Failed(e.to_string()))?;
    let root = tree.root.clone();
    let action = action(s, &root, &mut tree);
    Ok((tree, action))
}

/// What the farm answered, printed into `out` and `err`; the verb's lines, or its failure.
///
/// # Errors
/// [`Error::Failed`] for a status that is not OK, no result, or a non-zero exit.
pub async fn report(
    farm: &Farm,
    s: &Settings,
    response: ExecuteResponse,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<Vec<String>, Error> {
    let shown = s.cargo.join(" ");
    if let Some(result) = &response.result {
        // A farm may send a small output inline (`stdout_raw`), a copy of the blob its digest
        // names: the inline bytes when there are any, else the blob.
        for (raw, digest, to) in [
            (
                &result.stdout_raw,
                &result.stdout_digest,
                &mut *out as &mut dyn Write,
            ),
            (
                &result.stderr_raw,
                &result.stderr_digest,
                &mut *err as &mut dyn Write,
            ),
        ] {
            if !raw.is_empty() {
                let _ = to.write_all(raw);
            } else if let Some(digest) = digest {
                let bytes = farm
                    .read(digest)
                    .await
                    .map_err(|e| Error::Failed(e.to_string()))?;
                // A log line that cannot be written must not hide the verdict.
                let _ = to.write_all(&bytes);
            }
        }
    }
    let status = response.status.unwrap_or_default();
    if status.code != 0 {
        let _ = err.write_all(tail(&status).as_bytes());
        return Err(Error::Failed(format!(
            "the farm did not run `cargo {shown}` to its end: {:?}: {}",
            tonic::Code::from_i32(status.code),
            status.message
        )));
    }
    let Some(result) = response.result else {
        return Err(Error::Failed(format!(
            "the farm answered `cargo {shown}` with no result"
        )));
    };
    let cached = if response.cached_result {
        ", a cached result"
    } else {
        ""
    };
    if result.exit_code != 0 {
        return Err(Error::Failed(format!(
            "`cargo {shown}` exited {} on a farm Mac{cached}",
            result.exit_code
        )));
    }
    Ok(vec![format!(
        "ok: `cargo {shown}` passed on a farm Mac{cached}"
    )])
}

/// The stdout and stderr tails of a failed job the farm attached to `status`, as text.
fn tail(status: &rpc::Status) -> String {
    let mut text = String::new();
    for detail in &status.details {
        let Ok(info) = rpc::ErrorInfo::decode(detail.value.as_slice()) else {
            continue;
        };
        if info.reason != OUTPUT_TAIL {
            continue;
        }
        for key in ["stdout", "stderr"] {
            if let Some(value) = info.metadata.get(key) {
                text.push_str(&format!("--- the job's {key} (its end) ---\n{value}\n"));
            }
        }
    }
    text
}

/// The line a stage change prints.
#[must_use]
pub fn progress_line(progress: Progress) -> &'static str {
    match progress {
        Progress::Queued => "mac-farm: queued: waiting for a free Mac",
        Progress::Executing => "mac-farm: running on a farm Mac",
    }
}

/// The run against the farm: upload what it lacks, execute, report.
///
/// # Errors
/// [`Error::Failed`], for every end but a command that exited 0.
pub async fn remote(
    s: &Settings,
    tree: &Tree,
    action: &Digest,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<Vec<String>, Error> {
    let failed = |e: client::FarmError| Error::Failed(e.to_string());
    let farm = Farm::connect(&s.endpoint, s.tls, &s.instance)
        .await
        .map_err(failed)?;
    let digests: Vec<Digest> = tree
        .blobs
        .iter()
        .map(|(hash, bytes)| Digest {
            hash: hash.clone(),
            size_bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
        })
        .collect();
    let missing = farm.missing(&digests).await.map_err(failed)?;
    let bytes: i64 = missing.iter().map(|d| d.size_bytes).sum();
    let _ = writeln!(
        err,
        "mac-farm: {} blobs, {} missing from the farm ({bytes} bytes to upload)",
        digests.len(),
        missing.len()
    );
    farm.upload(&tree.blobs, &missing).await.map_err(failed)?;
    let mut lines = Vec::new();
    let response = farm
        .execute(action, &mut |p| {
            eprintln!("{}", progress_line(p));
        })
        .await
        .map_err(failed)?;
    lines.extend(report(&farm, s, response, out, err).await?);
    Ok(lines)
}

/// The verb with the machine's inputs given; the command's output is written to `out` and
/// `err`.
///
/// # Errors
/// [`Error`], for every end but a command that exited 0 on a farm Mac.
pub fn run_with(
    args: &Args,
    root: &Path,
    host: &mut Host<'_>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<Vec<String>, Error> {
    // The logs of a public repository's runs are public: keep the farm's address out of what
    // follows, the refusal of a wrong address included.
    let address = args.address.trim();
    let _ = writeln!(out, "::add-mask::{address}");
    let _ = writeln!(out, "::add-mask::{}", address.trim_start_matches("grpc://"));
    let settings = Settings::of(args).map_err(|p| Error::Failed(p.to_string()))?;
    let scratch = std::path::absolute(&args.scratch)
        .map_err(Error::spawn(format!("resolve {}", args.scratch.display())))?;
    let (tree, action) = inputs(&settings, root, &scratch, host)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Error::spawn("start the runtime".to_owned()))?;
    runtime.block_on(remote(&settings, &tree, &action, out, err))
}

/// The verb: the lines to print.
///
/// # Errors
/// [`Error`], for every end but a command that exited 0 on a farm Mac.
pub fn run(args: &Args) -> Result<Vec<String>, Error> {
    if args.address.trim().is_empty() {
        return Ok(vec![
            "::notice::mac-farm runs nothing: the repository variable FARM_ADDRESS is not set"
                .to_owned(),
        ]);
    }
    let root =
        std::env::current_dir().map_err(Error::spawn("read the working directory".to_owned()))?;
    let roots = crate::fetch::host_roots();
    let mut tracked = || git_files(&args.git, &root);
    let mut fetch = |home: &Path| fetch_crates(&args.cargo_program, &root, home);
    let mut download = |url: &str| crate::fetch::download(url, &roots).map_err(|e| e.to_string());
    let pins = toolchain::parse(toolchain::PINS).map_err(|e| Error::Failed(e.to_string()))?;
    let mut host = Host {
        tracked: &mut tracked,
        fetch: &mut fetch,
        pins,
        download: &mut download,
    };
    run_with(
        args,
        &root,
        &mut host,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
}

/// The output of a program `what` ran, when it succeeded.
///
/// # Errors
/// [`Error::Failed`] naming `what` and the program's stderr.
pub fn succeeded(what: &str, ran: Out) -> Result<String, Error> {
    if ran.ok {
        Ok(ran.stdout)
    } else {
        Err(Error::Failed(format!(
            "{what} failed: {}",
            ran.stderr.trim_end()
        )))
    }
}

/// The paths `<git> ls-files -z` lists in `root`.
///
/// # Errors
/// [`Error`] when git cannot be started or fails.
pub fn git_files(git: &Path, root: &Path) -> Result<Vec<String>, Error> {
    let listed = spawn(Process::new(git).args(["ls-files", "-z"]).current_dir(root))?;
    Ok(split_nul(&succeeded("git ls-files", listed)?))
}

/// The entries of a NUL-separated list.
#[must_use]
pub fn split_nul(text: &str) -> Vec<String> {
    text.split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `<cargo> fetch` of the crates of `root`'s lock file for [`TARGET`] into the Cargo home
/// `home`.
///
/// # Errors
/// [`Error`] when Cargo cannot be started or fails.
pub fn fetch_crates(cargo: &Path, root: &Path, home: &Path) -> Result<(), Error> {
    let fetched = spawn(
        Process::new(cargo)
            .args(["fetch", "--locked", "--target", TARGET])
            .env("CARGO_HOME", home)
            .current_dir(root),
    )?;
    succeeded("cargo fetch", fetched).map(drop)
}
