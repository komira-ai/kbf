//! Scenario B0, `farm_under_build_load` (docs/design/kbf-cicd.md, section 4.4): a farm of
//! real processes under Buck2 builds for a fixed time, no update and no kill; zero failed
//! builds, zero failed actions, no blob lost.
//!
//! It starts the farm (Buck2 dials the frontend's REAPI port), waits until it is ready,
//! runs the build loops while the scraper reads the frontend's `/metrics` every second
//! and the supervisor watches for a process that exits, waits a short settle time, takes
//! a last scrape, stops every process (each must exit 0), and applies the gates
//! ([`crate::gates`]) to what it measured. The result document is `RUNNING` from the
//! start and finished on every way out ([`crate::result::Writer`]); a hang is red, with
//! the process list beside the logs (`logs/processes.txt`), and a farm that does not come up has
//! the logs in its report (`logs/hang-report.txt`).
//!
//! A gate that never fails is not a gate, so the scenario carries injected faults
//! ([`Fault`]), each a way to make the farm worse that the gates must catch.
//!
//! **The lost-blob gates** (`lost_blobs_gauge`, `blob_losses`) read `kbf_cache_lost_blobs`
//! and `kbf_cache_blob_losses_total`, which a process that holds a cache writes: the
//! frontend holds none, so they are read from each storage node's `/metrics`
//! (`--metrics-listen`), summed over the nodes; a node that does not answer, or whose
//! scrapes lack a series, fails them.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::farm::{Bins, Farm, Role, Topology};
use crate::gates::{BuildResult, Evidence, Gate, all_pass};
use crate::interrupt::Interrupt;
use crate::load::{self, Daemons, Load, LoadConfig, Outcome, wait_bounded};
use crate::procs::after;
use crate::reap::{Reap, Sentinel};
use crate::result::{Builds, Document, Status, Writer, now};
use crate::scrape::{self, Scraper};

/// The scenario's name in the result document.
pub const SCENARIO: &str = "farm_under_build_load";

/// A way to make the farm worse, which the gates must catch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
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
    /// The sample project (`crates/kbf-integration-test/beta/buck2`).
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
    pub fault: Option<Fault>,
    /// Flags to set on a role after the fault's (a test's way to break one process).
    pub extra_flags: Vec<(Role, Vec<String>)>,
    /// How long the farm may take to become ready once its processes have started.
    pub farm_hang: Duration,
    /// Whether the run was asked to stop (SIGTERM, SIGINT): it then stops the farm, kills
    /// the Buck2 daemons and ends `FAILED` instead of running on.
    pub interrupt: Interrupt,
    /// The program that runs as the sentinel (`kbf-beta`, which answers `reap`): it kills the
    /// Buck2 daemons and the roles' process groups if this run is killed with no chance to
    /// ([`crate::reap`]). `None` runs without one.
    pub reaper: Option<PathBuf>,
}

/// How long `buck2 --version` is waited for.
const VERSION_BOUND: Duration = Duration::from_secs(30);

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
pub fn shrunk() -> Vec<String> {
    vec![
        format!("--cold-store-size-bytes={}", 1 << 20),
        "--cold-store-floor-bytes=0".to_owned(),
        "--cold-store-evict-grace-ms=0".to_owned(),
        format!("--hot-store-max-bytes={}", 1 << 20),
    ]
}

/// The fault's flags for a topology.
fn overrides(fault: Option<Fault>) -> Vec<(Role, Vec<String>)> {
    match fault {
        Some(Fault::ShrinkNodeCap) => vec![(Role::Node(1), shrunk())],
        Some(Fault::ShrinkEveryCap) => (1..=3).map(|n| (Role::Node(n), shrunk())).collect(),
        _ => Vec::new(),
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// `buck2 --version`'s first line (its output kept in `out`), or why there is none: it did not
/// start, did not answer within `limit`, or failed. Run in `cwd`, the project.
fn buck2_version(buck2: &Path, cwd: &Path, out: &Path, limit: Duration) -> String {
    let spawned = std::fs::File::create(out).and_then(|file| {
        std::process::Command::new(buck2)
            .arg("--version")
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(file)
            .stderr(std::process::Stdio::null())
            .spawn()
    });
    match spawned.map(|mut child| wait_bounded(&mut child, limit)) {
        Err(e) => format!("failed: {e}"),
        Ok((_, true)) => format!("failed: no answer in {} s", limit.as_secs()),
        Ok((Some(0), false)) => std::fs::read_to_string(out)
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned(),
        Ok((code, false)) => format!("failed: exit {code:?}"),
    }
}

/// The sentinel of a run, if it has a program for one.
fn start_sentinel(
    args: &B0,
    project: &Path,
    logs: &Path,
    run_id: &str,
) -> std::io::Result<Option<Sentinel>> {
    let Some(program) = &args.reaper else {
        return Ok(None);
    };
    let daemons = Daemons {
        buck2: args.buck2.clone(),
        project: project.to_owned(),
        lanes: args.concurrency,
        kill_timeout: args.kill_timeout,
        run_id: run_id.to_owned(),
    };
    let reap = Reap::new(&daemons, Some(logs.to_owned()));
    Sentinel::start(program, &reap, &logs.join("reaper.err")).map(Some)
}

impl B0 {
    /// The same with every path made absolute (the units' paths must be).
    fn absolute(&self) -> std::io::Result<B0> {
        let abs = |p: &Path| std::path::absolute(p);
        Ok(B0 {
            kbf: abs(&self.kbf)?,
            act: abs(&self.act)?,
            buck2: abs(&self.buck2)?,
            project: abs(&self.project)?,
            work: abs(&self.work)?,
            summary: self.summary.as_deref().map(abs).transpose()?,
            reaper: self.reaper.as_deref().map(abs).transpose()?,
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

fn scenario(args: &B0, logs: &Path, result: &mut Writer) -> Ended {
    result.doc.runtime.driver = "native".to_owned();
    result.doc.runtime.container = "none (native driver)".to_owned();
    result.doc.builds.concurrency = args.concurrency;
    result.write();

    // The project's copy first, so a project that cannot be copied fails before a farm starts.
    let project = match load::prepare(&args.project, &args.work, &args.act) {
        Ok(project) => project,
        Err(e) => return fail(result, Status::Failed, format!("the sample project: {e}")),
    };
    result.doc.runtime.buck2_version = buck2_version(
        &args.buck2,
        &project,
        &logs.join("buck2-version.txt"),
        VERSION_BOUND,
    );
    result.write();
    let mut flags = overrides(args.fault);
    flags.extend(args.extra_flags.iter().cloned());
    let topology = Topology {
        overrides: flags,
        interrupt: args.interrupt.clone(),
        ..Topology::beta()
    };
    let bins = Bins {
        kbf: args.kbf.clone(),
        fake_crun: None,
    };
    let farm_dir = args.work.join("farm");
    // Before the farm, so a run killed while it starts leaves nothing either. Dropped after
    // the farm and the load (locals drop in reverse), which have then cleaned up themselves.
    let run_id = format!("{}-{}", result.doc.run_id, std::process::id());
    let _sentinel = match start_sentinel(args, &project, logs, &run_id) {
        Ok(sentinel) => sentinel,
        Err(e) => return fail(result, Status::Failed, format!("the sentinel: {e}")),
    };
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
        run_id,
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
    let bound = after(args.wall_bound);
    let midway = after(args.duration / 2);
    let (mut killed, mut interrupted) = (false, false);
    while !load.finished() {
        if args.fault == Some(Fault::KillNodeMidway) && !killed && Instant::now() >= midway {
            farm.kill(Role::Node(2));
            exited.push((
                Role::Node(2).to_string(),
                "killed by the kill-node-midway fault".to_owned(),
            ));
            killed = true;
        }
        if note_exits(&mut farm, &mut exited) {
            // A farm that lost a process stops loading itself: the run is red already.
            load.stop();
        }
        if args.interrupt.is_set() {
            interrupted = true;
            break;
        }
        if Instant::now() >= bound {
            hung = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // A load that is dropped, not joined, stops its loops and kills their daemons.
    let outcomes: Vec<Outcome> = if hung || interrupted {
        drop(load);
        Vec::new()
    } else {
        load.join()
    };
    if interrupted {
        return interrupted_end(result, &mut farm, logs, &mut exited, args.wall_bound);
    }
    let load_finished = now();
    let load_secs = args.duration.as_secs_f64();

    // What is still in flight is counted, and the processes are watched meanwhile.
    let settle_until = after(args.settle);
    while Instant::now() < settle_until {
        if args.interrupt.is_set() {
            return interrupted_end(result, &mut farm, logs, &mut exited, args.wall_bound);
        }
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
    let stopped = stop_farm(&mut farm, &mut exited, args.wall_bound);
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
        restarts_allowed: 0,
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

/// The end of a run that was asked to stop: the farm is stopped (within `within`), the
/// process list and table are kept, and the run is `FAILED` with the reason; the buck2
/// daemons were killed with the load, and the scrapers stop as they go out of scope.
fn interrupted_end(
    result: &mut Writer,
    farm: &mut Farm,
    logs: &Path,
    exited: &mut Vec<(String, String)>,
    within: Duration,
) -> Ended {
    let stopped = stop_farm(farm, exited, within);
    let _ = std::fs::write(logs.join("processes.txt"), farm.process_list());
    result.doc.processes = farm.process_table();
    fail(
        result,
        Status::Failed,
        format!(
            "interrupted by a signal: the farm was stopped ({})",
            stopped
                .iter()
                .map(|(role, how)| format!("{role}: {how}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )
}

/// Stops the farm. A role that left since the last poll (the final scrape can take seconds)
/// is noted in `exited` first, so that `processes_alive` sees it; and the stop reports every
/// role, one that had already ended included, so that `clean_stop` sees how it ended.
fn stop_farm(
    farm: &mut Farm,
    exited: &mut Vec<(String, String)>,
    within: Duration,
) -> Vec<(String, String)> {
    note_exits(farm, exited);
    farm.stop_all_within(within)
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
