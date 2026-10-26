//! `cargo xtask prove-farm`: after the gamma stage has deployed a release to the farm, prove
//! the farm with a real build. A small Buck2 project (`crates/kbf-integration-test/gamma/buck2`)
//! is built against the farm's REAPI address with remote execution on and local execution
//! off, and the build's own summary line decides the verb: at least one action ran remotely
//! and none ran locally.
//!
//! 1. With an empty `--address` the verb prints a notice and exits 0, as `deploy-farm` does
//!    for an unset coordinator: a fork, or a repository that runs no farm, proves nothing and
//!    fails nothing.
//! 2. The flags are checked: the address is a `grpc://` URL (Buck2 refuses `grpcs://`; TLS is
//!    `--tls`, `true` or `false`), `--properties` is a comma-separated `key=value` list with
//!    no empty or repeated entry, and nothing carries a character that could end a line of
//!    the configuration file.
//! 3. `<project>/.buckconfig.local` is written ([`render_buckconfig`]): the farm's addresses,
//!    TLS and instance name, the execution platform (remote on, local off, the properties),
//!    and the run's nonce, which every action of the project takes as an argument, so no
//!    action of this run was ever run, or cached, before.
//! 4. `buck2 build` runs in the project. Its output is printed, and its
//!    `Commands: N (cached: C, remote: R, local: L)` line is read ([`parse_commands`]).
//!    The verb ends with 1 when the build fails, when the line is missing, when no action
//!    ran remotely or when any ran locally.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Args as ClapArgs;

use crate::Error;
use crate::pkg_linux::{Out, spawn};

/// The target the verb builds: the end of a chain of actions that depend on each other.
pub const TARGET: &str = "//:proof";

/// The file the configuration is written to, beside the project's committed `.buckconfig`.
pub const CONFIG_FILE: &str = ".buckconfig.local";

/// The flags of the verb.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The farm's REAPI address, `grpc://<host>:<port>`. Empty: print a notice and prove
    /// nothing.
    #[arg(long)]
    pub address: String,
    /// The instance name sent on every request. Empty: the empty instance.
    #[arg(long, default_value = "")]
    pub instance: String,
    /// Whether the address is served over TLS: `true` or `false`.
    #[arg(long, default_value = "")]
    pub tls: String,
    /// The execution platform's properties, `key=value` separated by commas.
    #[arg(long, default_value = "")]
    pub properties: String,
    /// The Buck2 program.
    #[arg(long)]
    pub buck2: PathBuf,
    /// The Buck2 project to build.
    #[arg(long)]
    pub project: PathBuf,
    /// A text no earlier run used (the run id and attempt).
    #[arg(long)]
    pub nonce: String,
}

/// Why the proof did not hold.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error(
        "the address {0:?} is not grpc://<host>:<port> (Buck2 refuses grpcs://; --tls says TLS)"
    )]
    Address(String),
    #[error("--tls is {0:?}: it must be `true` or `false`")]
    Tls(String),
    #[error("the properties are empty: the platform needs at least one key=value")]
    NoProperties,
    #[error("the property {0:?} is not key=value with a key and a value")]
    Property(String),
    #[error("the property {0:?} is given twice")]
    DuplicateProperty(String),
    #[error("{what} holds whitespace or a control character: {value:?}")]
    Unsafe { what: &'static str, value: String },
    #[error("the nonce is empty: every action would be an action the cache may have")]
    NoNonce,
    #[error("buck2 printed no `Commands:` line with remote and local counts")]
    NoSummary,
    #[error("no action ran remotely ({0})")]
    NothingRemote(Counts),
    #[error("{} action(s) ran locally ({}): the proof is remote only", .0.local, .0)]
    RanLocally(Counts),
}

/// The counts of one build's summary line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub total: u64,
    pub cached: u64,
    pub remote: u64,
    pub local: u64,
}

impl std::fmt::Display for Counts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Commands: {} (cached: {}, remote: {}, local: {})",
            self.total, self.cached, self.remote, self.local
        )
    }
}

/// The checked settings of a run.
#[derive(Debug, PartialEq, Eq)]
pub struct Settings {
    pub address: String,
    pub instance: String,
    pub tls: bool,
    pub properties: Vec<(String, String)>,
    pub nonce: String,
}

/// A value that goes into one line of the configuration file.
fn one_token(what: &'static str, value: &str) -> Result<(), Problem> {
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Problem::Unsafe {
            what,
            value: value.to_owned(),
        });
    }
    Ok(())
}

/// The properties in `text`, in order: `key=value` entries separated by commas, each
/// trimmed. No entry may be empty, lack the `=`, have an empty key or value, or repeat a
/// key; the list may not be empty.
///
/// # Errors
/// The [`Problem`] naming the first entry that is wrong.
pub fn parse_properties(text: &str) -> Result<Vec<(String, String)>, Problem> {
    if text.trim().is_empty() {
        return Err(Problem::NoProperties);
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for entry in text.split(',') {
        let entry = entry.trim();
        let (key, value) = match entry.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() && !v.trim().is_empty() => (k.trim(), v.trim()),
            _ => return Err(Problem::Property(entry.to_owned())),
        };
        one_token("a property", key)?;
        one_token("a property", value)?;
        if out.iter().any(|(k, _)| k == key) {
            return Err(Problem::DuplicateProperty(key.to_owned()));
        }
        out.push((key.to_owned(), value.to_owned()));
    }
    Ok(out)
}

impl Settings {
    /// The settings `args` name.
    ///
    /// # Errors
    /// The [`Problem`] of the first flag that is wrong.
    pub fn of(args: &Args) -> Result<Self, Problem> {
        let address = args.address.trim();
        one_token("the address", address)?;
        let host = address.strip_prefix("grpc://").unwrap_or_default();
        if host.is_empty() || host.contains('/') {
            return Err(Problem::Address(address.to_owned()));
        }
        let tls = match args.tls.trim() {
            "true" => true,
            "false" => false,
            other => return Err(Problem::Tls(other.to_owned())),
        };
        let instance = args.instance.trim();
        one_token("the instance name", instance)?;
        let nonce = args.nonce.trim();
        if nonce.is_empty() {
            return Err(Problem::NoNonce);
        }
        one_token("the nonce", nonce)?;
        Ok(Self {
            address: address.to_owned(),
            instance: instance.to_owned(),
            tls,
            properties: parse_properties(&args.properties)?,
            nonce: nonce.to_owned(),
        })
    }
}

/// The text of `<project>/.buckconfig.local`: the farm, the one execution platform (remote
/// on, local off, the properties the project's platform rule reads), and the nonce.
#[must_use]
pub fn render_buckconfig(s: &Settings) -> String {
    let mut out = String::from("[buck2_re_client]\n");
    for key in ["engine_address", "action_cache_address", "cas_address"] {
        out.push_str(&format!("  {key} = {}\n", s.address));
    }
    out.push_str(&format!("  tls = {}\n", s.tls));
    if !s.instance.is_empty() {
        out.push_str(&format!("  instance_name = {}\n", s.instance));
    }
    let properties: Vec<String> = s
        .properties
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    out.push_str("\n[build]\n  execution_platforms = root//platforms:remote\n");
    out.push_str("\n[kbf_gamma]\n  remote_enabled = true\n  local_enabled = false\n");
    out.push_str(&format!("  properties = {}\n", properties.join(",")));
    out.push_str(&format!("  nonce = {}\n", s.nonce));
    out
}

/// The counts of the last `Commands:` line of Buck2's output, which reads
/// `Commands: 3 (cached: 0, remote: 3, local: 0)`. Fields it does not know are ignored; the
/// remote and local counts must be there (a missing `cached` is 0).
///
/// # Errors
/// [`Problem::NoSummary`] when there is no such line, or it lacks either count.
pub fn parse_commands(output: &str) -> Result<Counts, Problem> {
    let line = output
        .lines()
        .rev()
        .find_map(|l| l.find("Commands:").map(|at| &l[at + "Commands:".len()..]))
        .ok_or(Problem::NoSummary)?;
    let (total, fields) = line.split_once('(').unwrap_or((line, ""));
    let number = |text: &str| text.trim().parse::<u64>().ok();
    let field = |name: &str| {
        fields
            .trim_end_matches(|c: char| c == ')' || c.is_whitespace())
            .split(',')
            .find_map(|f| number(f.trim().strip_prefix(name)?.strip_prefix(':')?))
    };
    match (number(total), field("remote"), field("local")) {
        (Some(total), Some(remote), Some(local)) => Ok(Counts {
            total,
            cached: field("cached").unwrap_or(0),
            remote,
            local,
        }),
        _ => Err(Problem::NoSummary),
    }
}

/// Whether `counts` prove the farm: some action ran remotely and none ran locally.
///
/// # Errors
/// [`Problem::NothingRemote`] or [`Problem::RanLocally`].
pub fn judge(counts: Counts) -> Result<(), Problem> {
    if counts.local > 0 {
        return Err(Problem::RanLocally(counts));
    }
    if counts.remote == 0 {
        return Err(Problem::NothingRemote(counts));
    }
    Ok(())
}

/// The arguments of the build (without the program).
#[must_use]
pub fn build_args() -> Vec<String> {
    ["build", "--console", "simple", TARGET]
        .map(str::to_owned)
        .to_vec()
}

/// Runs the Buck2 program with `args`.
pub type Buck2<'a> = dyn FnMut(&[String]) -> Result<Out, Error> + 'a;

/// The closure that runs `buck2` in `project`.
pub fn process(buck2: &Path, project: &Path) -> impl FnMut(&[String]) -> Result<Out, Error> {
    let (buck2, project) = (buck2.to_owned(), project.to_owned());
    move |args| spawn(Command::new(&buck2).args(args).current_dir(&project))
}

fn failed(problem: &Problem) -> Error {
    Error::Failed(problem.to_string())
}

/// The verb: the lines to print.
///
/// # Errors
/// [`Error`], for every end but a proven farm.
pub fn run(args: &Args) -> Result<Vec<String>, Error> {
    if args.address.trim().is_empty() {
        return Ok(vec![
            "::notice::the gamma proof is off: the repository variable FARM_ADDRESS is not set"
                .to_owned(),
        ]);
    }
    run_with(
        args,
        &mut process(&args.buck2, &args.project),
        &mut std::io::stdout().lock(),
    )
}

/// The verb with the Buck2 program given; Buck2's output is written to `out`.
///
/// # Errors
/// [`Error`], for every end but a proven farm.
pub fn run_with(
    args: &Args,
    buck2: &mut Buck2<'_>,
    out: &mut impl Write,
) -> Result<Vec<String>, Error> {
    let settings = Settings::of(args).map_err(|p| failed(&p))?;
    let config = args.project.join(CONFIG_FILE);
    std::fs::write(&config, render_buckconfig(&settings)).map_err(|e| Error::Spawn {
        what: format!("write {}", config.display()),
        why: e.to_string(),
    })?;
    let built = buck2(&build_args())?;
    let text = format!("{}{}", built.stdout, built.stderr);
    // A log line that cannot be written must not hide the verdict.
    let _ = out.write_all(text.as_bytes());
    if !built.ok {
        return Err(Error::Failed(
            "buck2 build failed; its output is above".to_owned(),
        ));
    }
    let counts = parse_commands(&text).map_err(|p| failed(&p))?;
    judge(counts).map_err(|p| failed(&p))?;
    Ok(vec![format!(
        "ok: {} remote action(s) on {}, none local ({counts})",
        counts.remote, settings.address
    )])
}
