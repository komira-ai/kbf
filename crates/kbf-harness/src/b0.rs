//! Scenario B0, `farm_under_build_load` (docs/design/kbf-cicd.md, section 4.4): a farm of
//! real processes under Buck2 builds for a fixed time, no update and no kill; zero failed
//! builds, zero failed actions, no blob lost.
//!
//! It starts the farm (behind an Envoy when one is available), waits until it is ready,
//! runs the build loops while the scraper reads the frontend's `/metrics` every second
//! and the supervisor watches for a process that exits, waits a short settle time, takes
//! a last scrape, stops every process (each must exit 0), and applies the gates
//! ([`crate::gates`]) to what it measured. The result document is `RUNNING` from the
//! start and finished on every way out ([`crate::result::Writer`]); a hang is red, with
//! the process list beside the logs (`logs/processes.txt`), and a farm that does not come up has
//! the logs in its report (`logs/hang-report.txt`).
//!
//! The gate that is never seen red is not a gate, so the scenario carries two mutants
//! ([`Mutant`]), each a way to make the farm worse that the gates must catch.
//!
//! **The lost-blob gates** (`lost_blobs_gauge`, `blob_losses`) read `kbf_cache_lost_blobs`
//! and `kbf_cache_blob_losses_total`, which a process that holds a cache writes: the
//! frontend holds none, so they are read from each storage node's `/metrics`
//! (`--metrics-listen`), summed over the nodes; a node that does not answer, or whose
//! scrapes lack a series, fails them.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::envoy::{self, Found};
use crate::farm::{Bins, Farm, Role, Topology};
use crate::gates::{BuildResult, Evidence, Gate, all_pass};
use crate::load::{self, Load, LoadConfig, Outcome};
use crate::result::{Builds, Document, Status, Writer, now};
use crate::scrape::{self, Scraper};

/// The scenario's name in the result document.
pub const SCENARIO: &str = "farm_under_build_load";

/// A way to make the farm worse, which the gates must catch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mutant {
    /// Node 1's cold store is told its disk is nearly full, so it evicts what it holds.
    ShrinkNodeCap,
    /// Every node's cold store is told so: with two copies of each blob, one node alone may
    /// evict without a build noticing.
    ShrinkEveryCap,
    /// Node 2 is killed halfway through the load.
    KillNodeMidway,
}

/// What B0 runs with.
#[derive(Clone, Debug)]
pub struct B0 {
    /// The `kbf` binary every farm role runs.
    pub kbf: PathBuf,
    /// `kbf-m2-act`, the program the sample project's actions run.
    pub act: PathBuf,
    pub buck2: PathBuf,
    /// An Envoy binary; `None` takes the one the variable `envoy_var` names, if any.
    pub envoy: Option<PathBuf>,
    /// The variable that names an Envoy binary when `envoy` is `None`
    /// ([`envoy::ENV_VAR`]; a test names one that is not set).
    pub envoy_var: String,
    /// The sample project (`crates/kbf-it/beta/buck2`).
    pub project: PathBuf,
    /// The run's directory: the farm, the project's copy, `logs/` and the result.
    pub work: PathBuf,
    /// How long the loops start builds for.
    pub duration: Duration,
    /// How long to wait after the load, for what is still in flight to be counted.
    pub settle: Duration,
    pub concurrency: usize,
    pub build_timeout: Duration,
    /// The longest one `buck2 kill` may take ([`LoadConfig::kill_timeout`]).
    pub kill_timeout: Duration,
    /// The longest the load may take in all, builds under way at its end included: past it
    /// the run is `TIMED_OUT` (the caller gives `duration` + `build_timeout` and some
    /// seconds).
    pub wall_bound: Duration,
    pub pause_secs: u64,
    pub fill_kib: u64,
    pub scrape_every: Duration,
    /// Where to append the job summary (`$GITHUB_STEP_SUMMARY`).
    pub summary: Option<PathBuf>,
    pub mutant: Option<Mutant>,
    /// Flags to set on a role after the mutant's (a test's way to break one process).
    pub extra_flags: Vec<(Role, Vec<String>)>,
    /// How long the farm may take to become ready once its processes have started.
    pub farm_hang: Duration,
    /// A missing Envoy is a failure (the CI setting), not a skip.
    pub require_envoy: bool,
}

/// How a run ended, for the process's exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    Passed,
    Failed,
}

/// The flags that make a node's cold store hold nothing for long: its filesystem is taken to
/// be 1 MiB, nothing is kept free, an object is evictable at once, and the hot tier in front
/// of it is 1 MiB too (so a write soon takes the direct path to the cold store, which
/// refuses it: with the hot tier at its default size, the flush to the cold store fails and
/// is retried, with the blobs still readable from the hot tier, and no build sees it).
fn shrunk() -> Vec<String> {
    vec![
        format!("--cold-store-size-bytes={}", 1 << 20),
        "--cold-store-floor-bytes=0".to_owned(),
        "--cold-store-evict-grace-ms=0".to_owned(),
        format!("--hot-store-max-bytes={}", 1 << 20),
    ]
}

/// The mutant's flags for a topology.
fn overrides(mutant: Option<Mutant>) -> Vec<(Role, Vec<String>)> {
    match mutant {
        Some(Mutant::ShrinkNodeCap) => vec![(Role::Node(1), shrunk())],
        Some(Mutant::ShrinkEveryCap) => (1..=3).map(|n| (Role::Node(n), shrunk())).collect(),
        _ => Vec::new(),
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// `buck2 --version`'s first line.
fn buck2_version(buck2: &Path) -> String {
    let out = std::process::Command::new(buck2).arg("--version").output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned(),
        Ok(o) => format!("failed: {}", o.status),
        Err(e) => format!("failed: {e}"),
    }
}

impl B0 {
    /// The same with every path made absolute (the units' paths must be).
    fn absolute(&self) -> std::io::Result<B0> {
        let abs = |p: &Path| std::path::absolute(p);
        Ok(B0 {
            kbf: abs(&self.kbf)?,
            act: abs(&self.act)?,
            buck2: abs(&self.buck2)?,
            envoy: self.envoy.as_deref().map(abs).transpose()?,
            project: abs(&self.project)?,
            work: abs(&self.work)?,
            summary: self.summary.as_deref().map(abs).transpose()?,
            ..self.clone()
        })
    }
}

/// Runs B0 and writes `<work>/beta-result.json`.
///
/// # Errors
/// The run's directory cannot be made; anything that goes wrong after that is in the
/// document.
pub fn run(args: &B0) -> std::io::Result<Ended> {
    let args = &args.absolute()?;
    let logs = args.work.join("logs");
    std::fs::create_dir_all(&logs)?;
    let doc = Document::running(SCENARIO, env);
    let mut result = Writer::start(&args.work.join("beta-result.json"), doc);
    let begun = Instant::now();
    let ended = scenario(args, &logs, &mut result);
    result.doc.duration_secs = Some(begun.elapsed().as_secs_f64());
    let status = result.doc.status;
    // `scenario` sets the status it ended with; finishing stamps it.
    result.finish(status);
    if let Some(path) = &args.summary {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(result.doc.summary().as_bytes())?;
    }
    print!("{}", result.doc.summary());
    Ok(ended)
}

/// Sets the failure on the document and ends.
fn fail(result: &mut Writer, status: Status, why: String) -> Ended {
    eprintln!("kbf-beta: {why}");
    result.doc.error = Some(why);
    result.doc.status = status;
    Ended::Failed
}

/// What [`choose_envoy`] decided.
struct Chosen {
    binary: Option<PathBuf>,
    /// Why there is none.
    absent: Option<String>,
}

impl Chosen {
    /// For the document's `runtime.envoy`.
    fn describe(&self) -> String {
        match (&self.binary, &self.absent) {
            (Some(path), _) => path.display().to_string(),
            (None, why) => format!("absent: {}", why.as_deref().unwrap_or("unknown")),
        }
    }
}

/// Envoy: the binary given, else the one the environment names. A missing one is a skip,
/// with its reason on the document, or, where the CI says Envoy is required, an error.
fn choose_envoy(args: &B0) -> Result<Chosen, String> {
    let found = match &args.envoy {
        Some(path) => Found::Binary(path.clone()),
        None => envoy::find(&args.envoy_var),
    };
    match found {
        Found::Binary(path) => Ok(Chosen {
            binary: Some(path),
            absent: None,
        }),
        Found::Absent(why) if args.require_envoy => Err(format!(
            "Envoy is required ({}=1) and absent: {why}",
            envoy::REQUIRE_VAR
        )),
        Found::Absent(why) => Ok(Chosen {
            binary: None,
            absent: Some(why),
        }),
    }
}

fn scenario(args: &B0, logs: &Path, result: &mut Writer) -> Ended {
    let envoy_bin = match choose_envoy(args) {
        Ok(chosen) => {
            result.doc.runtime.envoy = chosen.describe();
            chosen.binary
        }
        Err(why) => {
            result.doc.runtime.envoy = format!("absent: {why}");
            return fail(result, Status::Failed, why);
        }
    };
    result.doc.runtime.driver = "native".to_owned();
    result.doc.runtime.container = "none (native driver)".to_owned();
    result.doc.runtime.buck2_version = buck2_version(&args.buck2);
    result.doc.builds.concurrency = args.concurrency;
    result.write();

    // The project's copy first, so a project that cannot be copied fails before a farm starts.
    let project = match load::prepare(&args.project, &args.work, &args.act) {
        Ok(project) => project,
        Err(e) => return fail(result, Status::Failed, format!("the sample project: {e}")),
    };
    let mut flags = overrides(args.mutant);
    flags.extend(args.extra_flags.iter().cloned());
    let topology = Topology {
        overrides: flags,
        ..Topology::beta(envoy_bin)
    };
    let bins = Bins {
        kbf: args.kbf.clone(),
        fake_podman: None,
    };
    let farm_dir = args.work.join("farm");
    let farm = match Farm::start(logs, &farm_dir, &bins, &topology) {
        Ok(farm) => farm,
        Err(e) => return hang_or_fail(result, logs, &e.what, &e.report),
    };
    // The start lines have had their wait; readiness has the run's own.
    let mut farm = farm.with_hang(args.farm_hang);
    if let Err(e) = farm.wait_ready() {
        return hang_or_fail(result, logs, &e.what, &e.report);
    }

    let cfg = LoadConfig {
        buck2: args.buck2.clone(),
        project,
        logs: logs.to_owned(),
        // The farm CA, which the frontend's directory holds.
        ca: farm_dir.join("frontend/etc/ca.pem"),
        port: farm.client_port(),
        concurrency: args.concurrency,
        duration: args.duration,
        build_timeout: args.build_timeout,
        kill_timeout: args.kill_timeout,
        pause_secs: args.pause_secs,
        fill_kib: args.fill_kib,
        run_id: format!("{}-{}", result.doc.run_id, std::process::id()),
    };

    if let Err(e) = load::write_buckconfig(&cfg) {
        return fail(
            result,
            Status::Failed,
            format!("the project's .buckconfig: {e}"),
        );
    }
    let scraper = Scraper::start(
        farm.api_port(),
        args.scrape_every,
        &scrape::series_file(logs),
    );
    let node_scrapers: Vec<(String, Scraper)> = farm
        .metrics_ports()
        .iter()
        .enumerate()
        .map(|(i, port)| {
            let name = crate::farm::node_id(i + 1);
            let file = scrape::node_series_file(logs, &name);
            (name, Scraper::start(*port, args.scrape_every, &file))
        })
        .collect();
    let load_started = now();
    let load = Load::start(&cfg);
    let mut exited: Vec<(String, String)> = Vec::new();
    let mut hung = false;
    let bound = Instant::now() + args.wall_bound;
    let midway = Instant::now() + args.duration / 2;
    let mut killed = false;
    while !load.finished() {
        if args.mutant == Some(Mutant::KillNodeMidway) && !killed && Instant::now() >= midway {
            farm.kill(Role::Node(2));
            exited.push((
                Role::Node(2).to_string(),
                "killed by the kill-node-midway mutant".to_owned(),
            ));
            killed = true;
        }
        if note_exits(&mut farm, &mut exited) {
            // A farm that lost a process stops loading itself: the run is red already.
            load.stop();
        }
        if Instant::now() >= bound {
            hung = true;
            load.stop();
            load::kill_daemons(&cfg);
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let outcomes: Vec<Outcome> = if hung { Vec::new() } else { load.join() };
    let load_finished = now();
    let load_secs = args.duration.as_secs_f64();

    // What is still in flight is counted, and the processes are watched meanwhile.
    let settle_until = Instant::now() + args.settle;
    while Instant::now() < settle_until {
        note_exits(&mut farm, &mut exited);
        std::thread::sleep(Duration::from_millis(250));
    }
    // Alive is not whole: a node that lost its place in the farm keeps running.
    let farm_whole = farm.whole();
    let scrapes = scraper.stop();
    let node_scrapes: Vec<(String, scrape::Scrapes)> = node_scrapers
        .into_iter()
        .map(|(name, scraper)| (name, scraper.stop()))
        .collect();
    let stop_started = now();
    let stopped = stop_farm(&mut farm, &mut exited);
    let stop_finished = now();
    // Which process was which, for the logs beside it.
    let _ = std::fs::write(logs.join("processes.txt"), farm.process_list());
    result.doc.processes = farm.process_table();

    let builds: Vec<BuildResult> = outcomes
        .iter()
        .map(|o| BuildResult {
            kind: o.kind.name().to_owned(),
            ok: o.ok(),
            failed_actions: o.failed_actions.clone(),
        })
        .collect();
    let mut by_kind = std::collections::BTreeMap::new();
    for o in &outcomes {
        *by_kind.entry(o.kind.name().to_owned()).or_insert(0_u64) += 1;
    }
    result.doc.builds = Builds {
        run: outcomes.len() as u64,
        failed: outcomes.iter().filter(|o| !o.ok()).count() as u64,
        concurrency: args.concurrency,
        by_kind,
    };
    for o in outcomes.iter().filter(|o| !o.ok()) {
        eprintln!(
            "build {}-{} ({}): exit {:?}, timed out {}, {:?}, {:?}",
            o.lane,
            o.n,
            o.kind.name(),
            o.exit,
            o.timed_out,
            o.spawns,
            o.failed_actions
        );
    }
    let evidence = Evidence {
        builds: &builds,
        scrapes: &scrapes,
        nodes: &node_scrapes,
        exited_during_load: &exited,
        farm_whole: &farm_whole,
        stopped: &stopped,
        hung,
        load: (load_started, load_finished),
        load_secs,
        stop: (stop_started, stop_finished),
        scrape_every_secs: args.scrape_every.as_secs_f64(),
    };
    result.doc.gates = evidence.gates();
    let (status, ended) = ending(&result.doc.gates, hung);
    result.doc.status = status;
    if hung {
        result.doc.error = Some(format!(
            "the load did not end within its bound; see {}",
            logs.join("processes.txt").display()
        ));
    }
    ended
}

/// How the run ends given its gates: `TIMED_OUT` if the load hung, else `PASSED` when every
/// gate passed and `FAILED` when one did not (and when there is none).
fn ending(gates: &[Gate], hung: bool) -> (Status, Ended) {
    let ended = [Ended::Failed, Ended::Passed][usize::from(all_pass(gates) && !hung)];
    let status = match (hung, ended) {
        (true, _) => Status::TimedOut,
        (false, Ended::Passed) => Status::Passed,
        (false, Ended::Failed) => Status::Failed,
    };
    (status, ended)
}

/// Stops the farm. A role that left since the last poll (the final scrape can take seconds)
/// is noted in `exited` first, so that `processes_alive` sees it; and the stop reports every
/// role, one that had already ended included, so that `clean_stop` sees how it ended.
fn stop_farm(farm: &mut Farm, exited: &mut Vec<(String, String)>) -> Vec<(String, String)> {
    note_exits(farm, exited);
    farm.stop_all()
        .into_iter()
        .map(|(role, how)| (role.to_string(), how))
        .collect()
}

/// Adds to `exited` the roles that have ended without being ended, once each; whether there
/// was one that was not there before.
fn note_exits(farm: &mut Farm, exited: &mut Vec<(String, String)>) -> bool {
    let before = exited.len();
    for (role, how) in farm.unexpected_exits() {
        let name = role.to_string();
        if !exited.iter().any(|(r, _)| *r == name) {
            exited.push((name, how));
        }
    }
    exited.len() > before
}

/// A farm that did not start or become ready: a timeout is `TIMED_OUT`, anything else
/// `FAILED`; either way the report (process list and every log) is kept.
fn hang_or_fail(result: &mut Writer, logs: &Path, what: &str, report: &str) -> Ended {
    let _ = std::fs::write(logs.join("hang-report.txt"), format!("{what}\n{report}"));
    let status = if what.starts_with("timed out") {
        Status::TimedOut
    } else {
        Status::Failed
    };
    result.doc.gates = vec![crate::gates::Gate {
        name: "farm_ready".to_owned(),
        measured: json!(what),
        threshold: "the farm starts and becomes ready".to_owned(),
        verdict: crate::gates::Verdict::Fail,
        started: result.doc.started.clone(),
        finished: now(),
        detail: Some(format!("see {}", logs.join("hang-report.txt").display())),
    }];
    fail(result, status, what.to_owned())
}

#[cfg(test)]
#[path = "b0_tests.rs"]
mod tests;
