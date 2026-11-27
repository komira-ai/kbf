//! `farm-build --buck2 <path> --assert-remote <path> --farm-config <path> --log-dir <dir> --target
//! <label>... --patch <path> --summary <path> --repo <owner/name> --run-id <id>`: the farm lane's
//! build (`.github/workflows/farm.yml`), one command.
//!
//! Every buck2 command that talks to the farm (the build of the `buildrules` command line, and
//! the build) runs through komira-ai/komira's `assert_remote run` ([`Farm`]), the tool of its
//! `buck2-farm` action, fetched and built at a pinned commit by the workflow:
//! `assert_remote run --log <log-dir>/<name>.log --summary <summary> --farm-config <farm-config>
//! -- <buck2> <args>`. While buck2 runs, that tool's watchdog reads the runner's TCP connections
//! to the farm named by the farm config's `engine_address`; when one has heard nothing from the
//! farm for 300 s (a live farm sends something at least every 30 s), it ends buck2 and its daemon
//! and writes one line saying so to the log and one to the job summary. A run it ended, or one
//! whose only failures are a cut connection to the farm, is run again once the farm's port
//! answers (within the farm config's `farm_wait_seconds`), at most three runs in all. Its exit
//! status is buck2's, of the last run. `buck2 audit config` and `buck2 kill` reach no farm and
//! run directly.
//!
//! In this order:
//!
//! 1. `buck2 audit config buildrules_re.linux_x86_64_properties --style json` must give the key a
//!    value that is not blank: the worker property set `farm-config` wrote, which the
//!    buckconfig hands to buildrules. Without one buck2 has no execution platform, and this says
//!    so first.
//! 2. The generated build table must be fresh ([`table`]): `buildrules check`, with the
//!    `buildrules` command line built from the pinned buildrules cell on the farm. When it calls the
//!    table stale, the job runs `buildrules sync`, writes what sync changed as a patch to `--patch`
//!    (the workflow uploads it as the artifact [`table::PATCH_ARTIFACT`]) and the stale inputs, the
//!    changed files and the one command that applies the patch to `--summary` (the job summary),
//!    and fails without building.
//! 3. `buck2 build <every label, in the order given>`, one call. What it prints, buck2's
//!    standard output and error and the watchdog's lines on one pipe (its progress, the output
//!    of a failed action, its summary), is written to the log line by line as it comes, so a long
//!    build shows its progress in the job's log; a failed build is a failure.
//! 4. The build's summary line, `Commands: N (cached: N, remote: N, local: N)`, must be there and
//!    say `local: 0`: every action ran on the farm or came from its cache, none on this machine.
//!    After a run again it is the last run's line. The line is what the verb prints.
//!
//! Whatever happened, it then runs `buck2 kill`, so no daemon outlives the job; that command's own
//! outcome changes nothing.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};

pub mod table;

pub use table::Handoff;

/// The flags of the verb.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The buck2 program.
    #[arg(long)]
    pub buck2: PathBuf,
    /// komira's `assert_remote`, which runs each buck2 command that talks to the farm.
    #[arg(long)]
    pub assert_remote: PathBuf,
    /// The farm's buckconfig `farm-config` wrote, which `assert_remote` reads the farm from.
    #[arg(long)]
    pub farm_config: PathBuf,
    /// An existing directory where `assert_remote` keeps each run's log.
    #[arg(long)]
    pub log_dir: PathBuf,
    /// A label to build; given once for each, built by one `buck2 build` in the order given.
    #[arg(long, required = true)]
    pub target: Vec<String>,
    /// Where a stale table's patch (what `buildrules sync` changed) is written.
    #[arg(long)]
    pub patch: PathBuf,
    /// The job summary a stale table's fix, and `assert_remote`'s line for a run again, are
    /// appended to.
    #[arg(long)]
    pub summary: PathBuf,
    /// The repository of the run, `owner/name`, for the command that downloads the patch.
    #[arg(long)]
    pub repo: String,
    /// The run's id, for the command that downloads the patch.
    #[arg(long)]
    pub run_id: u64,
}

/// The verb: the build's summary line, with buck2's log on standard error.
///
/// # Errors
/// [`Error`], as [`build`].
pub fn run(args: &Args) -> Result<String, Error> {
    let farm = Farm {
        buck2: args.buck2.clone(),
        assert_remote: args.assert_remote.clone(),
        farm_config: args.farm_config.clone(),
        log_dir: args.log_dir.clone(),
        summary: args.summary.clone(),
    };
    let handoff = Handoff {
        patch: args.patch.clone(),
        summary: args.summary.clone(),
        repo: args.repo.clone(),
        run_id: args.run_id,
    };
    build(&farm, &args.target, &handoff, &mut std::io::stderr())
}

/// How the verb runs buck2: directly for a command that reaches no farm, and through
/// `assert_remote run` for one that does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Farm {
    /// The buck2 program.
    pub buck2: PathBuf,
    /// komira's `assert_remote`.
    pub assert_remote: PathBuf,
    /// The farm's buckconfig, `assert_remote`'s `--farm-config`.
    pub farm_config: PathBuf,
    /// The directory of each run's log.
    pub log_dir: PathBuf,
    /// The job summary, `assert_remote`'s `--summary`.
    pub summary: PathBuf,
}

impl Farm {
    /// The command that runs `buck2 <args>` against the farm under the watchdog, its log
    /// `<log_dir>/<log>.log` (`assert_remote` keeps a failed run's beside it, as
    /// `<log>.first.log` and `<log>.second.log`).
    pub fn command(&self, log: &str, args: &[&str]) -> Command {
        let mut command = Command::new(&self.assert_remote);
        command
            .arg("run")
            .arg("--log")
            .arg(self.log_dir.join(format!("{log}.log")))
            .arg("--summary")
            .arg(&self.summary)
            .arg("--farm-config")
            .arg(&self.farm_config)
            .arg("--")
            .arg(&self.buck2)
            .args(args);
        command
    }
}

/// The key of the worker property set buildrules reads.
pub const PROPERTIES_KEY: &str = "buildrules_re.linux_x86_64_properties";

/// Why a farm build failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A program (buck2, `assert_remote`, the `buildrules` command line) could not be started, or
    /// waited for.
    #[error("cannot run {program:?}: {why}")]
    Start {
        /// The program that was to run.
        program: PathBuf,
        /// The system's reason.
        why: String,
    },
    /// `buck2 audit config` ended with a failure status.
    #[error("`buck2 audit config` failed ({status}): {stderr}")]
    Audit {
        /// How it ended.
        status: String,
        /// What it printed on its standard error, trimmed.
        stderr: String,
    },
    /// The audit gave the worker property set no value, or a blank one.
    #[error(
        "buildrules_re.linux_x86_64_properties is empty: `farm-config` wrote no worker \
         properties, so buck2 has no farm to build on"
    )]
    NoWorkers,
    /// The generated build table is not fresh, or its freshness could not be settled.
    #[error(transparent)]
    Table(#[from] table::Error),
    /// The build ended with a failure status: buck2's, of its last run, or `assert_remote`'s 2
    /// for a command line, log or job summary it could not use.
    #[error("`buck2 build {targets}` failed ({status})")]
    Build {
        /// The labels that were built, joined by spaces.
        targets: String,
        /// How the build ended.
        status: String,
    },
    /// The build passed and printed no readable summary line.
    #[error("the build printed no summary line (`Commands: N (cached: N, remote: N, local: N)`)")]
    NoSummary,
    /// The summary line says an action ran on this machine; the line is the payload.
    #[error("`{0}`: actions ran on this machine, not on the farm (local must be 0)")]
    Local(String),
}

/// Whether `json`, what `buck2 audit config <key> --style json` printed, gives
/// [`PROPERTIES_KEY`] a value that is not blank.
pub fn has_value(json: &str) -> bool {
    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json)
        .ok()
        .and_then(|config| config.get(PROPERTIES_KEY).cloned())
        .and_then(|value| value.as_str().map(|v| !v.trim().is_empty()))
        .unwrap_or(false)
}

/// The last summary line of a build's output, from `Commands: `, and the number after its
/// `local: `; `None` when there is none, or when the last line holding `Commands: ` is not a
/// readable summary (an earlier one is never read in its place: it may be of another build).
pub fn summary(log: &str) -> Option<(&str, u64)> {
    let line = log
        .lines()
        .rev()
        .find_map(|line| line.find("Commands: ").map(|at| &line[at..]))?;
    let (_, after) = line.split_once("local: ")?;
    let digits = after
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or_default();
    Some((line, digits.parse().ok()?))
}

/// Builds `targets` on the farm as `farm` says, writing what the build prints to `log` as it
/// comes, once the generated build table is fresh, and stops buck2's daemon after: the summary
/// line, or why the build is not a farm build. A stale table leaves its fix where `handoff` says.
///
/// # Errors
/// buck2 or `assert_remote` cannot be started, buck2 names no worker property set, the table is
/// stale or its check fails, the build fails, prints no summary line, or ran an action on this
/// machine.
pub fn build(
    farm: &Farm,
    targets: &[String],
    handoff: &Handoff,
    log: &mut dyn Write,
) -> Result<String, Error> {
    let result = build_and_check(farm, targets, handoff, log);
    // The daemon outlives the command: stop it however the build ended.
    let _ = Command::new(&farm.buck2)
        .arg("kill")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    result
}

/// The error for a program that could not be started or waited for.
fn start_error(program: &Path) -> impl FnOnce(std::io::Error) -> Error {
    move |e| Error::Start {
        program: program.to_owned(),
        why: e.to_string(),
    }
}

/// `program <args>` run to its end, its standard input closed.
fn output(program: &Path, args: &[&str]) -> Result<Output, Error> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(start_error(program))
}

/// Every line `from` gives, written to `log` as it comes, and all of them.
fn tee(from: impl std::io::Read, log: &mut dyn Write) -> Vec<u8> {
    let mut lines = BufReader::new(from);
    let (mut line, mut text) = (Vec::new(), Vec::new());
    // A read that fails ends the log; the status of the build is still buck2's.
    while lines.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
        // A log that cannot be written loses buck2's words, not the verdict.
        let _ = log.write_all(&line);
        text.append(&mut line);
    }
    text
}

/// `command` (of the program `program`, which an error names) run to its end, its standard input
/// closed and its standard output and error on one pipe, so its words come in the order it wrote
/// them: how it ended, and everything it printed, each line also written to `log` as it comes.
fn merged(
    mut command: Command,
    program: &Path,
    log: &mut dyn Write,
) -> Result<(ExitStatus, Vec<u8>), Error> {
    let (reader, writer) = std::io::pipe().map_err(start_error(program))?;
    command
        .stdin(Stdio::null())
        .stdout(writer.try_clone().map_err(start_error(program))?)
        .stderr(writer);
    let spawned = command.spawn();
    // The command holds this process's ends of the pipe; dropped, the read below ends when the
    // program and its children close theirs.
    drop(command);
    let mut child = spawned.map_err(start_error(program))?;
    let text = tee(reader, log);
    let status = child.wait().map_err(start_error(program))?;
    Ok((status, text))
}

/// The checks and the build, without the daemon's stop.
fn build_and_check(
    farm: &Farm,
    targets: &[String],
    handoff: &Handoff,
    log: &mut dyn Write,
) -> Result<String, Error> {
    let audit = output(
        &farm.buck2,
        &["audit", "config", PROPERTIES_KEY, "--style", "json"],
    )?;
    if !audit.status.success() {
        return Err(Error::Audit {
            status: audit.status.to_string(),
            stderr: String::from_utf8_lossy(&audit.stderr).trim().to_owned(),
        });
    }
    if !has_value(&String::from_utf8_lossy(&audit.stdout)) {
        return Err(Error::NoWorkers);
    }
    table::ensure_fresh(farm, handoff, log)?;
    let mut args = vec!["build"];
    args.extend(targets.iter().map(String::as_str));
    let (status, text) = merged(farm.command(BUILD_LOG, &args), &farm.assert_remote, log)?;
    if !status.success() {
        return Err(Error::Build {
            targets: targets.join(" "),
            status: status.to_string(),
        });
    }
    let text = String::from_utf8_lossy(&text);
    let (line, local) = summary(&text).ok_or(Error::NoSummary)?;
    if local != 0 {
        return Err(Error::Local(line.to_owned()));
    }
    Ok(line.to_owned())
}

/// The name of the build's log in [`Farm::log_dir`], without `.log`.
pub const BUILD_LOG: &str = "buck2-farm";

#[cfg(test)]
mod tests;
