//! The command line: `run` (start or resume a bake) and `tag-message` (print the body of the
//! release tag a green verdict makes).
//!
//! Exit codes: 0 green, 1 red, 2 the command or its inputs are wrong (nothing was decided),
//! 3 extended (no decision: thin usage, or the farm could not be read). A red or extended
//! end also prints `PAGE: <why>` on standard error, the documented hook for whatever pages a
//! person.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use kbf_gate::{CommitId, Header, ResultDoc, Run, SetHash, Stage, Status, Waiver, system_clock};

use crate::WAIVE_MIN_LOAD;
use crate::drive::{Fetched, drive};
use crate::eval::Evaluator;
use crate::scrape::http_get;
use crate::state::State;
use crate::table::BAKE_SECS;
use crate::updater::UpdaterLog;

/// Green.
pub const EXIT_GREEN: u8 = 0;
/// Red.
pub const EXIT_RED: u8 = 1;
/// Wrong command or inputs.
pub const EXIT_USAGE: u8 = 2;
/// Extended: no decision.
pub const EXIT_EXTENDED: u8 = 3;

/// `kbf-bake`: the gamma bake evaluator. Runs on the operator's side, never in Actions.
#[derive(Parser)]
#[command(name = "kbf-bake", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a bake, or resume the one the state file holds (its clock is kept).
    Run(Box<RunArgs>),
    /// Print the body of the annotated release tag: the green verdict, verbatim.
    TagMessage(TagArgs),
}

#[derive(Args)]
struct RunArgs {
    /// The state file: created by the first run, read by every later one.
    #[arg(long)]
    state: PathBuf,
    /// Where `gamma-verdict.json` is written.
    #[arg(long)]
    verdict: PathBuf,
    /// The farm's metrics endpoint, e.g. `http://host:port/metrics`.
    #[arg(long)]
    metrics_url: String,
    /// The updater's phase log (JSON: node steps and restarts). Without it the restarts gate
    /// has no evidence and is red.
    #[arg(long)]
    updater_log: Option<PathBuf>,
    /// The commit under test (first run; later runs must agree).
    #[arg(long)]
    commit: Option<String>,
    /// The release set's `SHA256SUMS` file; its hash is the set hash (first run).
    #[arg(long)]
    sha256sums: Option<PathBuf>,
    /// The evaluator's id for this bake (first run).
    #[arg(long)]
    run_id: Option<String>,
    /// The generation the deploy set as desired.
    #[arg(long)]
    desired_generation: Option<u64>,
    /// When the deploy began (unix seconds).
    #[arg(long, requires = "deploy_end")]
    deploy_start: Option<u64>,
    /// When the deploy ended (unix seconds).
    #[arg(long, requires = "deploy_start")]
    deploy_end: Option<u64>,
    /// How long the bake lasts. A release needs the default; a shorter bake is a rehearsal.
    #[arg(long, default_value_t = BAKE_SECS)]
    bake_seconds: u64,
    /// Seconds between scrapes.
    #[arg(long, default_value_t = 60)]
    interval_seconds: u64,
    /// The error ratio of the seven days before the deploy. Without it the error gate is red.
    #[arg(long)]
    baseline_error_ratio: Option<f64>,
    /// The queue-to-start p99 (seconds) before the deploy. Without it the latency gate is red.
    #[arg(long)]
    baseline_queue_p99: Option<f64>,
    /// Waive the minimum-load condition. The verdict records the waiver; it needs
    /// `--waive-by` and `--waive-reason`.
    #[arg(long)]
    waive_min_load: bool,
    /// Who waives.
    #[arg(long, requires = "waive_min_load")]
    waive_by: Option<String>,
    /// Why.
    #[arg(long, requires = "waive_min_load")]
    waive_reason: Option<String>,
}

#[derive(Args)]
struct TagArgs {
    /// The verdict file of a green bake.
    #[arg(long)]
    verdict: PathBuf,
}

/// Run the command line `args` (the program name first), writing to `out` and `err`; the exit code.
pub fn execute(args: Vec<OsString>, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let code = if e.use_stderr() {
                EXIT_USAGE
            } else {
                EXIT_GREEN
            };
            let _ = if e.use_stderr() {
                write!(err, "{e}")
            } else {
                write!(out, "{e}")
            };
            return code;
        }
    };
    let result = match cli.cmd {
        Cmd::Run(a) => run(&a, out, err),
        Cmd::TagMessage(a) => tag_message(&a, out),
    };
    result.unwrap_or_else(|why| {
        let _ = writeln!(err, "kbf-bake: {why}");
        EXIT_USAGE
    })
}

fn tag_message(a: &TagArgs, out: &mut dyn Write) -> Result<u8, String> {
    let bytes =
        std::fs::read(&a.verdict).map_err(|e| format!("read {}: {e}", a.verdict.display()))?;
    let doc = ResultDoc::from_bytes(&bytes).map_err(|e| format!("{}: {e}", a.verdict.display()))?;
    if doc.stage != Stage::Gamma || !doc.passed() {
        return Err(format!(
            "{} is not a finished green gamma verdict (stage {:?}, status {:?}): a tag carries only that",
            a.verdict.display(),
            doc.stage,
            doc.status
        ));
    }
    out.write_all(&bytes).map_err(|e| e.to_string())?;
    Ok(EXIT_GREEN)
}

fn run(a: &RunArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<u8, String> {
    let waiver = waiver_of(a)?;
    for (flag, v, least) in [
        ("--baseline-error-ratio", a.baseline_error_ratio, 0.0),
        (
            "--baseline-queue-p99",
            a.baseline_queue_p99,
            f64::MIN_POSITIVE,
        ),
    ] {
        if let Some(v) = v.filter(|v| !v.is_finite() || *v < least) {
            return Err(format!("{flag} {v} is not a usable baseline"));
        }
    }
    if a.bake_seconds == 0 || a.interval_seconds == 0 {
        return Err("--bake-seconds and --interval-seconds must be above zero".to_owned());
    }
    let mut state = open_state(a)?;
    if let Some(done) = state.ended {
        return Ok(report(
            done,
            "the bake already ended; its verdict stands",
            out,
            err,
        ));
    }
    if let Some(g) = a.desired_generation {
        state.desired_generation = Some(g);
    }
    if let (Some(start), Some(end)) = (a.deploy_start, a.deploy_end) {
        state.deploy = Some(kbf_gate::Window {
            start,
            end: Some(end),
        });
    }
    if a.baseline_error_ratio.is_some() {
        state.baseline.error_ratio = a.baseline_error_ratio;
    }
    if a.baseline_queue_p99.is_some() {
        state.baseline.queue_p99 = a.baseline_queue_p99;
    }
    let header = Header {
        stage: Stage::Gamma,
        commit: state.commit.clone(),
        set_hash: state.set_hash.clone(),
        run_id: state.run_id.clone(),
        attempt: 1,
    };
    let mut eval = Evaluator::new(state, a.interval_seconds.saturating_mul(3));
    if let Some(w) = waiver {
        eval.waive(w);
    }
    eval.state().save(&a.state).map_err(|e| e.to_string())?;
    let run = Run::start(&a.verdict, header, system_clock).map_err(|e| e.to_string())?;
    let url = a.metrics_url.clone();
    let log_path = a.updater_log.clone();
    let outcome = drive(
        &mut eval,
        run,
        &a.state,
        a.interval_seconds,
        &mut || http_get(&url, Duration::from_secs(10)).map(|text| Fetched { text, builds: None }),
        &mut || {
            let bytes = std::fs::read(log_path.as_ref()?).ok()?;
            UpdaterLog::parse(&bytes).ok()
        },
        &mut |secs| std::thread::sleep(Duration::from_secs(secs)),
    )
    .map_err(|e| e.to_string())?;
    Ok(report(outcome.status, &outcome.note, out, err))
}

/// The waiver the flags ask for: refused without an operator and a reason.
fn waiver_of(a: &RunArgs) -> Result<Option<Waiver>, String> {
    if !a.waive_min_load {
        return Ok(None);
    }
    let (by, why) = (
        a.waive_by.as_deref().unwrap_or(""),
        a.waive_reason.as_deref().unwrap_or(""),
    );
    Waiver::new(WAIVE_MIN_LOAD, by, why)
        .map(Some)
        .map_err(|_| "a waiver needs --waive-by and --waive-reason, and both recorded".to_owned())
}

/// The state to run on: the file's, checked against the identity flags, or a new one.
fn open_state(a: &RunArgs) -> Result<State, String> {
    let commit = a
        .commit
        .as_deref()
        .map(CommitId::parse)
        .transpose()
        .map_err(|e| e.to_string())?;
    let set_hash = a
        .sha256sums
        .as_ref()
        .map(|p| {
            std::fs::read(p)
                .map(|b| SetHash::of_sums(&b))
                .map_err(|e| format!("read {}: {e}", p.display()))
        })
        .transpose()?;
    if let Some(state) = State::load(&a.state).map_err(|e| e.to_string())? {
        let same = commit.as_ref().is_none_or(|c| *c == state.commit)
            && set_hash.as_ref().is_none_or(|h| *h == state.set_hash)
            && a.run_id.as_ref().is_none_or(|r| *r == state.run_id);
        if !same {
            return Err(format!(
                "{} holds the bake of commit {}, run {}: the flags name another",
                a.state.display(),
                state.commit.as_str(),
                state.run_id
            ));
        }
        return Ok(state);
    }
    let (Some(commit), Some(set_hash), Some(run_id)) = (commit, set_hash, a.run_id.clone()) else {
        return Err(
            "a new bake needs --commit, --sha256sums and --run-id (no state file yet)".to_owned(),
        );
    };
    Ok(State::new(
        commit,
        set_hash,
        run_id,
        a.bake_seconds,
        system_clock(),
    ))
}

fn report(status: Status, note: &str, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let (word, code) = match status {
        Status::Green => ("green", EXIT_GREEN),
        Status::Red => ("red", EXIT_RED),
        Status::Extended | Status::Running => ("extended", EXIT_EXTENDED),
    };
    let _ = writeln!(
        out,
        "kbf-bake: {word}{}{note}",
        if note.is_empty() { "" } else { ": " }
    );
    if code != EXIT_GREEN {
        let _ = writeln!(err, "PAGE: the gamma bake is {word}: {note}");
    }
    code
}
