//! The command line: `run` (start or resume a bake), `sample` (add a reading to the latency
//! ledger), `baseline` (take the 7-day baseline at deploy start) and `tag-message` (print the
//! body of the release tag a green verdict makes).
//!
//! Exit codes: 0 green, 1 red, 2 the command or its inputs are wrong (nothing was decided),
//! 4 `baseline` could not take a value (the file is still written, with the reason), 3 extended (no decision: thin usage, the farm could not be read, or the evaluator could not
//! write its state or verdict). A red or extended
//! end also prints `PAGE: <why>` on standard error, the documented hook for whatever pages a
//! person.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use kbf_gate::{
    CommitId, Header, Metrics, ResultDoc, Run, SetHash, Stage, Status, Waiver, system_clock,
};

use crate::WAIVE_MIN_LOAD;
use crate::baseline::{self, BaselineDoc};
use crate::drive::{Fetched, drive};
use crate::eval::Evaluator;
use crate::handoff::Handoff;
use crate::ledger::Ledger;
use crate::rollback::{self, Plan};
use crate::scrape::http_get;
use crate::state::State;
use crate::table::BAKE_SECS;
use crate::table::metric;
use crate::updater::UpdaterLog;

/// Green.
pub const EXIT_GREEN: u8 = 0;
/// Red.
pub const EXIT_RED: u8 = 1;
/// Wrong command or inputs.
pub const EXIT_USAGE: u8 = 2;
/// Extended: no decision.
pub const EXIT_EXTENDED: u8 = 3;
/// `baseline` could not take every value.
pub const EXIT_NO_BASELINE: u8 = 4;

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
    /// Add one reading of the queue-to-start histogram to the latency ledger.
    Sample(SampleArgs),
    /// Take the 7-day baseline for a deploy that is starting, and write it to a file.
    Baseline(BaselineArgs),
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
    /// The deploy's hand-off file (commit, set hash, run id, generation, deploy span,
    /// reversibility, previous commit); it replaces the flags that give those.
    #[arg(
        long,
        conflicts_with_all = [
            "commit", "sha256sums", "run_id", "desired_generation", "deploy_start", "deploy_end"
        ]
    )]
    handoff: Option<PathBuf>,
    /// The baseline file `kbf-bake baseline` wrote; replaces the two baseline flags.
    #[arg(long, conflicts_with_all = ["baseline_error_ratio", "baseline_queue_p99"])]
    baseline: Option<PathBuf>,
    /// Roll back by itself when the bake ends red on a measured hard gate and the hand-off
    /// says the release is reversible. Off unless given. Needs `--handoff`, `--rollback-url`
    /// and `--rollback-token-file`.
    #[arg(long, requires_all = ["handoff", "rollback_url", "rollback_token_file"])]
    auto_rollback: bool,
    /// The API that takes `POST /v1/update/desired` (its write gate is loopback only), e.g.
    /// `http://127.0.0.1:PORT`.
    #[arg(long, requires = "auto_rollback")]
    rollback_url: Option<String>,
    /// A file holding the API's bearer token.
    #[arg(long, requires = "auto_rollback")]
    rollback_token_file: Option<PathBuf>,
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
struct SampleArgs {
    /// The farm's metrics endpoint.
    #[arg(long)]
    metrics_url: String,
    /// The ledger file, created if absent.
    #[arg(long)]
    ledger: PathBuf,
}

#[derive(Args)]
struct BaselineArgs {
    /// The ledger `sample` keeps.
    #[arg(long)]
    ledger: PathBuf,
    /// The farm's API origin, e.g. `http://host:port`; the invocation list is read from it.
    #[arg(long)]
    invocations_url: String,
    /// Where the baseline file is written.
    #[arg(long)]
    out: PathBuf,
    /// The deploy's start (unix seconds); the hand-off's, or now, when neither is given.
    #[arg(long, conflicts_with = "handoff")]
    at: Option<u64>,
    /// The deploy's hand-off file, for its start.
    #[arg(long)]
    handoff: Option<PathBuf>,
}

#[derive(Args)]
struct TagArgs {
    /// The verdict file of a green bake.
    #[arg(long)]
    verdict: PathBuf,
}

/// A message the command line prints before it exits 2; any error converts into one.
struct Failure(String);

impl<E: std::error::Error> From<E> for Failure {
    fn from(e: E) -> Self {
        Self(e.to_string())
    }
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
        Cmd::Sample(a) => sample(&a, out),
        Cmd::Baseline(a) => take_baseline(&a, out, err),
        Cmd::TagMessage(a) => tag_message(&a, out),
    };
    result.unwrap_or_else(|Failure(why)| {
        let _ = writeln!(err, "kbf-bake: {why}");
        EXIT_USAGE
    })
}

fn sample(a: &SampleArgs, out: &mut dyn Write) -> Result<u8, Failure> {
    let text = http_get(&a.metrics_url, Duration::from_secs(10)).map_err(Failure)?;
    let metrics = Metrics::parse(&text)?;
    let buckets = metrics.buckets(metric::QUEUED_SECONDS).ok_or_else(|| {
        Failure(format!(
            "{} is absent from the scrape",
            metric::QUEUED_SECONDS
        ))
    })?;
    let mut ledger = Ledger::load(&a.ledger).map_err(Failure)?;
    ledger.push(system_clock(), &buckets).map_err(Failure)?;
    ledger.save(&a.ledger)?;
    let said = format!(
        "kbf-bake: {} readings in {}",
        ledger.snapshots.len(),
        a.ledger.display()
    );
    writeln!(out, "{said}")?;
    Ok(EXIT_GREEN)
}

fn take_baseline(
    a: &BaselineArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<u8, Failure> {
    let at = match (&a.handoff, a.at) {
        (Some(path), _) => Handoff::load(path)?.deploy_start,
        (None, Some(at)) => at,
        (None, None) => system_clock(),
    };
    let ledger = Ledger::load(&a.ledger).map_err(Failure)?;
    let doc = baseline::take(
        &mut |url| http_get(url, Duration::from_secs(30)),
        &a.invocations_url,
        &ledger,
        at,
    );
    doc.save(&a.out)?;
    for why in &doc.missing {
        writeln!(err, "kbf-bake: no baseline: {why}")?;
    }
    let said = format!("kbf-bake: baseline for {at} written to {}", a.out.display());
    writeln!(out, "{said}")?;
    Ok(if doc.complete() {
        EXIT_GREEN
    } else {
        EXIT_NO_BASELINE
    })
}

fn tag_message(a: &TagArgs, out: &mut dyn Write) -> Result<u8, Failure> {
    let bytes = std::fs::read(&a.verdict)
        .map_err(|e| Failure(format!("read {}: {e}", a.verdict.display())))?;
    let doc = ResultDoc::from_bytes(&bytes)
        .map_err(|e| Failure(format!("{}: {e}", a.verdict.display())))?;
    if doc.stage != Stage::Gamma || !doc.passed() {
        return Err(Failure(format!(
            "{} is not a finished green gamma verdict (stage {:?}, status {:?}): a tag carries only that",
            a.verdict.display(),
            doc.stage,
            doc.status
        )));
    }
    out.write_all(&bytes)?;
    Ok(EXIT_GREEN)
}

fn run(a: &RunArgs, out: &mut dyn Write, err: &mut dyn Write) -> Result<u8, Failure> {
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
            return Err(Failure(format!("{flag} {v} is not a usable baseline")));
        }
    }
    if a.bake_seconds == 0 || a.interval_seconds == 0 {
        return Err(Failure(
            "--bake-seconds and --interval-seconds must be above zero".to_owned(),
        ));
    }
    let handoff = a.handoff.as_deref().map(Handoff::load).transpose()?;
    let base = a
        .baseline
        .as_deref()
        .map(|p| BaselineDoc::load(p).map_err(Failure))
        .transpose()?;
    let mut state = open_state(a, handoff.as_ref())?;
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
    if let Some(h) = &handoff {
        state.desired_generation = h.desired_generation.or(state.desired_generation);
        state.deploy = Some(kbf_gate::Window {
            start: h.deploy_start,
            end: h.deploy_end,
        });
        state.reversible = Some(h.reversible);
        state.previous_commit = h.previous_commit.clone();
    }
    if let Some(b) = &base {
        state.baseline.error_ratio = b.error_ratio;
        state.baseline.queue_p99 = b.queue_p99;
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
    eval.state().save(&a.state)?;
    let run = Run::start(&a.verdict, header, system_clock)?;
    let url = a.metrics_url.clone();
    let log_path = a.updater_log.clone();
    let driven = drive(
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
    );
    // An error of the evaluator's own (the state or the verdict could not be written) was sealed
    // as extended by `drive`; it is reported as extended and paged, not as a usage error.
    let (status, note, measured) = match driven {
        Ok(outcome) => (outcome.status, outcome.note, outcome.measured),
        Err(e) => (
            Status::Extended,
            format!("the evaluator failed: {e}"),
            false,
        ),
    };
    roll_back(a, &mut eval, status, measured, out, err);
    Ok(report(status, &note, out, err))
}

/// The automatic rollback of a bake that ended `status`: decided by [`rollback::plan`], called
/// once (no retry), and recorded in the state file. Whatever it does is said on `out` or
/// `err` as a `ROLLBACK:` line.
fn roll_back(
    a: &RunArgs,
    eval: &mut Evaluator,
    status: Status,
    measured: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) {
    let st = eval.state();
    let plan = rollback::plan(&rollback::Inputs {
        enabled: a.auto_rollback,
        status,
        measured,
        reversible: st.reversible,
        previous: st.previous_commit.as_ref(),
        generation: st.desired_generation,
    });
    let said = match plan {
        Plan::Off => return,
        Plan::Page(why) => {
            let _ = writeln!(err, "ROLLBACK: not offered: {why}");
            format!("not offered: {why}")
        }
        Plan::Offer { commit, expect } => {
            let called = call_offer(a, &commit, expect);
            match called {
                Ok(()) => {
                    let said = format!("offered {} over generation {expect}", commit.as_str());
                    let _ = writeln!(out, "ROLLBACK: {said}");
                    said
                }
                Err(why) => {
                    let _ = writeln!(err, "ROLLBACK: failed, not retried: {why}");
                    format!("failed: {why}")
                }
            }
        }
    };
    eval.record_rollback(said);
    if let Err(e) = eval.state().save(&a.state) {
        let _ = writeln!(
            err,
            "kbf-bake: the rollback was not recorded in the state: {e}"
        );
    }
}

/// The offer, with the URL and token file clap requires together with `--auto-rollback`.
fn call_offer(a: &RunArgs, commit: &CommitId, expect: u64) -> Result<(), String> {
    let url = a.rollback_url.as_deref().unwrap_or_default();
    let token_path = a.rollback_token_file.clone().unwrap_or_default();
    let raw = std::fs::read_to_string(&token_path)
        .map_err(|e| format!("read {}: {e}", token_path.display()))?;
    let token = raw.trim();
    if token.is_empty() {
        return Err(format!("{} holds no token", token_path.display()));
    }
    rollback::offer(url, token, commit, expect, Duration::from_secs(30))
}

/// The waiver the flags ask for: refused without an operator and a reason.
fn waiver_of(a: &RunArgs) -> Result<Option<Waiver>, Failure> {
    if !a.waive_min_load {
        return Ok(None);
    }
    let (by, why) = (
        a.waive_by.as_deref().unwrap_or(""),
        a.waive_reason.as_deref().unwrap_or(""),
    );
    Waiver::new(WAIVE_MIN_LOAD, by, why).map(Some).map_err(|_| {
        Failure("a waiver needs --waive-by and --waive-reason, and both recorded".to_owned())
    })
}

/// The state to run on: the file's, checked against the identity (the hand-off's, or the
/// flags'), or a new one.
fn open_state(a: &RunArgs, handoff: Option<&Handoff>) -> Result<State, Failure> {
    let (commit, set_hash, run_id) = match handoff {
        Some(h) => (
            Some(h.commit.clone()),
            Some(h.set_hash.clone()),
            Some(h.run_id.clone()),
        ),
        None => (
            a.commit.as_deref().map(CommitId::parse).transpose()?,
            a.sha256sums
                .as_ref()
                .map(|p| {
                    std::fs::read(p)
                        .map(|b| SetHash::of_sums(&b))
                        .map_err(|e| Failure(format!("read {}: {e}", p.display())))
                })
                .transpose()?,
            a.run_id.clone(),
        ),
    };
    if let Some(state) = State::load(&a.state)? {
        let same = commit.as_ref().is_none_or(|c| *c == state.commit)
            && set_hash.as_ref().is_none_or(|h| *h == state.set_hash)
            && run_id.as_ref().is_none_or(|r| *r == state.run_id);
        if !same {
            return Err(Failure(format!(
                "{} holds the bake of commit {}, run {}: the {} names another",
                a.state.display(),
                state.commit.as_str(),
                state.run_id,
                if handoff.is_some() {
                    "hand-off"
                } else {
                    "flags"
                }
            )));
        }
        return Ok(state);
    }
    let (Some(commit), Some(set_hash), Some(run_id)) = (commit, set_hash, run_id) else {
        return Err(Failure(
            "a new bake needs --handoff, or --commit, --sha256sums and --run-id (no state file yet)"
                .to_owned(),
        ));
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
        _ => ("extended", EXIT_EXTENDED),
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
