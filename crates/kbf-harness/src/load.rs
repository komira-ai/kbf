//! The Buck2 load: builds of the sample project (`crates/kbf-it/beta/buck2`) in a loop,
//! `concurrency` loops at once, each loop with a Buck2 daemon of its own (an isolation
//! directory), through the frontend's REAPI port.
//!
//! Three kinds of build, taken in turn by each loop: a short chain of actions that
//! stands in for a compile, an action that sleeps and so holds a lease, and actions
//! that write new bytes for the farm to store. Every build is salted, so none is answered
//! from the action cache, and bounded by a time limit of its own.
//!
//! A build is judged by its exit status and by what it says it did: Buck2's console
//! summary (`Commands: N (cached: N, remote: N, local: N)`, parsed with `kbf-it`'s
//! reader) must show actions run on the farm and none anywhere else, and its event log
//! ([`crate::eventlog`]) is counted for failed actions.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kbf_it::summary::{Spawns, Tool, parse};

use crate::eventlog;
use crate::procs::after;

/// The kinds of build, in the order a loop takes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Compile,
    Pause,
    Upload,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Compile, Kind::Pause, Kind::Upload];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Kind::Compile => "compile",
            Kind::Pause => "pause",
            Kind::Upload => "upload",
        }
    }

    /// The targets this kind builds.
    #[must_use]
    pub fn targets(self) -> &'static [&'static str] {
        match self {
            Kind::Compile => &["//:joined"],
            Kind::Pause => &["//:pause"],
            Kind::Upload => &["//:fill_0", "//:fill_1", "//:fill_2", "//:fill_3"],
        }
    }
}

/// What the load runs.
#[derive(Clone, Debug)]
pub struct LoadConfig {
    pub buck2: PathBuf,
    /// The project's copy, with `tools/kbf-m2-act` in it ([`prepare`]).
    pub project: PathBuf,
    /// Where each build's console output and event log go.
    pub logs: PathBuf,
    /// The farm CA's certificate.
    pub ca: PathBuf,
    /// The port of `localhost` the builds dial.
    pub port: u16,
    pub concurrency: usize,
    /// How long the loops start new builds for.
    pub duration: Duration,
    /// The longest one build may take.
    pub build_timeout: Duration,
    /// The longest one `buck2 kill` may take: a wedged daemon makes it wait, and nothing
    /// that waits on it may wait for ever.
    pub kill_timeout: Duration,
    /// How long the pause action sleeps, in seconds.
    pub pause_secs: u64,
    /// The size of each upload action's output, in KiB.
    pub fill_kib: u64,
    /// Distinguishes this run's salts from another's.
    pub run_id: String,
}

/// One build and how it went.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub kind: Kind,
    /// The loop and the build's number in it.
    pub lane: usize,
    pub n: usize,
    /// The exit status, if it ended in time.
    pub exit: Option<i32>,
    pub timed_out: bool,
    pub secs: f64,
    /// What the console summary says ran where, or why it could not be read.
    pub spawns: Result<Spawns, String>,
    /// Failed actions in the event log, or why it could not be read.
    pub failed_actions: Result<u64, String>,
}

impl Outcome {
    /// The build passed: it exited 0 in time, ran at least one action on the farm and
    /// none anywhere else.
    #[must_use]
    pub fn ok(&self) -> bool {
        let ran = self
            .spawns
            .as_ref()
            .is_ok_and(|s| s.remote > 0 && s.other.is_empty());
        self.exit == Some(0) && !self.timed_out && ran
    }
}

/// The settings of the farm that the project's `.buckconfig` does not hold: where the
/// remote execution client dials (`localhost`, the farm CA's certificate names it) and
/// the CA it trusts. The scheme is `grpc://` and TLS is the project's `tls = true`: Buck2
/// refuses a `grpcs://` address ("you should omit it").
#[must_use]
pub fn buckconfig_block(cfg: &LoadConfig) -> String {
    let farm = format!("grpc://localhost:{}", cfg.port);
    format!(
        "\n[buck2_re_client]\n  engine_address = {farm}\n  action_cache_address = {farm}\n  cas_address = {farm}\n  tls_ca_certs = {}\n",
        cfg.ca.display()
    )
}

/// Appends [`buckconfig_block`] to the project's `.buckconfig`.
///
/// # Errors
/// The file cannot be opened for appending (made if missing).
pub fn write_buckconfig(cfg: &LoadConfig) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cfg.project.join(".buckconfig"))?;
    file.write_all(buckconfig_block(cfg).as_bytes())
}

/// Copies the sample project `source` to `<work>/project`, with `act` (`kbf-m2-act`) as its
/// `tools/kbf-m2-act`, executable.
///
/// # Errors
/// A file that cannot be read or written, or a `project/` already there.
pub fn prepare(source: &Path, work: &Path, act: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let project = work.join("project");
    if project.exists() {
        return Err(std::io::Error::other(format!(
            "{} exists; give a fresh work directory",
            project.display()
        )));
    }
    copy_tree(source, &project)?;
    let tools = project.join("tools");
    std::fs::create_dir_all(&tools)?;
    let target = tools.join("kbf-m2-act");
    std::fs::copy(act, &target)?;
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
    Ok(project)
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type()?.is_dir() {
            // A build's own output is not part of the project.
            if entry.file_name() != "buck-out" {
                copy_tree(&src, &dst)?;
            }
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// The load while it runs. Dropped before [`Load::join`], it stops the loops and kills their
/// Buck2 daemons: a daemon outlives its client (it leads a session of its own), so a run
/// that panics or returns early would otherwise leave a daemon per loop behind.
pub struct Load {
    guard: Guard,
    lanes: Vec<std::thread::JoinHandle<Vec<Outcome>>>,
}

/// What a load that is dropped before it is joined must undo.
struct Guard {
    cfg: LoadConfig,
    stop: Arc<AtomicBool>,
    armed: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.armed {
            self.stop.store(true, Ordering::Relaxed);
            kill_daemons(&self.cfg);
        }
    }
}

impl Load {
    /// Starts the loops.
    #[must_use]
    pub fn start(cfg: &LoadConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let end = after(cfg.duration);
        let lanes = (0..cfg.concurrency)
            .map(|lane| {
                let (cfg, stop) = (cfg.clone(), Arc::clone(&stop));
                std::thread::spawn(move || lane_loop(&cfg, lane, end, &stop))
            })
            .collect();
        Self {
            guard: Guard {
                cfg: cfg.clone(),
                stop,
                armed: true,
            },
            lanes,
        }
    }

    /// Whether every loop has ended.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.lanes.iter().all(std::thread::JoinHandle::is_finished)
    }

    /// Asks the loops to start no more builds (a build under way runs to its end or its
    /// bound).
    pub fn stop(&self) {
        self.guard.stop.store(true, Ordering::Relaxed);
    }

    /// Waits for the loops and returns every build, in the order of the loops. Each loop
    /// kills its own daemon as it ends, so there is nothing left for a drop to undo.
    ///
    /// # Panics
    /// When a loop's thread panicked.
    #[must_use]
    pub fn join(mut self) -> Vec<Outcome> {
        self.guard.armed = false;
        self.lanes
            .into_iter()
            .flat_map(|lane| lane.join().expect("a load loop"))
            .collect()
    }
}

/// The Buck2 daemons of a load's loops, which a run kills when it gives up on its loops
/// (and which the sentinel, [`crate::reap`], kills when the run dies without a chance to).
#[derive(Clone, Debug)]
pub struct Daemons {
    pub buck2: PathBuf,
    /// The project the daemons were started in.
    pub project: PathBuf,
    /// The number of loops: the daemons are `beta-<run>-0` to `beta-<run>-<lanes - 1>`
    /// ([`isolation`]).
    pub lanes: usize,
    /// The longest one `buck2 kill` may take ([`LoadConfig::kill_timeout`]).
    pub kill_timeout: Duration,
    /// The run's id ([`LoadConfig::run_id`]), which names the run's daemons.
    pub run_id: String,
}

impl Daemons {
    /// The daemons of the loops `cfg` describes.
    #[must_use]
    pub fn of(cfg: &LoadConfig) -> Self {
        Self {
            buck2: cfg.buck2.clone(),
            project: cfg.project.clone(),
            lanes: cfg.concurrency,
            kill_timeout: cfg.kill_timeout,
            run_id: cfg.run_id.clone(),
        }
    }

    /// Stops every daemon, all at once, each for at most [`Daemons::kill_timeout`]: for a run
    /// that gives up on its loops, which is the run of a wedged daemon.
    pub fn kill_all(&self) {
        let kills: Vec<_> = (0..self.lanes)
            .map(|lane| {
                let daemons = self.clone();
                std::thread::spawn(move || daemons.kill_lane(lane))
            })
            .collect();
        for kill in kills {
            let _ = kill.join();
        }
    }

    /// `buck2 kill` for the daemon of `lane`, given up on (the client is killed) after
    /// [`Daemons::kill_timeout`].
    pub(crate) fn kill_lane(&self, lane: usize) {
        let spawned = Command::new(&self.buck2)
            .args(["--isolation-dir", &isolation(&self.run_id, lane), "kill"])
            .current_dir(&self.project)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = spawned {
            let _ = wait_bounded(&mut child, self.kill_timeout);
        }
    }
}

/// Stops the Buck2 daemons of every loop ([`Daemons::kill_all`]).
pub fn kill_daemons(cfg: &LoadConfig) {
    Daemons::of(cfg).kill_all();
}

/// The kind of build `n` of loop `lane`: each loop takes the kinds in turn, and the loops
/// start on different ones, so at any time the farm holds a mix.
#[must_use]
pub fn kind_for(lane: usize, n: usize) -> Kind {
    Kind::ALL[(lane + n) % Kind::ALL.len()]
}

fn lane_loop(cfg: &LoadConfig, lane: usize, end: Instant, stop: &AtomicBool) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    let mut n = 0;
    while Instant::now() < end && !stop.load(Ordering::Relaxed) {
        let kind = kind_for(lane, n);
        let (outcome, started) = build(cfg, lane, n, kind);
        outcomes.push(outcome);
        // A build that cannot be started (no such program) cannot be started the next time
        // either: a loop that went on would spin, keeping an outcome per turn in memory, for
        // the whole load. The failed build is the loop's result.
        if !started {
            break;
        }
        n += 1;
    }
    // This loop's daemon goes with it.
    Daemons::of(cfg).kill_lane(lane);
    outcomes
}

/// The Buck2 isolation directory of loop `lane` of run `run_id`. It names the run as well as
/// the loop: two runs in one project (two jobs on one runner) have daemons of their own, and
/// one's cleanup does not kill the other's. Characters a directory name should not have
/// become `-`.
#[must_use]
pub(crate) fn isolation(run_id: &str, lane: usize) -> String {
    let run: String = run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("beta-{run}-{lane}")
}

/// The command line of one build (without the program): the loop's own daemon, the salt,
/// and what the kind needs. The farm's address and CA are not here: Buck2 reads the
/// remote execution client's settings when its daemon starts, and ignores them on a
/// command line, so they are in the project's `.buckconfig` ([`write_buckconfig`]).
#[must_use]
pub fn build_args(
    cfg: &LoadConfig,
    lane: usize,
    n: usize,
    kind: Kind,
    event_log: &Path,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--isolation-dir".into(),
        isolation(&cfg.run_id, lane),
        "build".into(),
        "--console".into(),
        "simple".into(),
        "--event-log".into(),
        event_log.display().to_string(),
    ];
    let mut config = |key: &str, value: String| {
        args.push("-c".into());
        args.push(format!("{key}={value}"));
    };
    config("kbf_beta.salt", format!("{}-{lane}-{n}", cfg.run_id));
    match kind {
        Kind::Compile => {}
        Kind::Pause => config("kbf_beta.sleep_secs", cfg.pause_secs.to_string()),
        Kind::Upload => config("kbf_beta.fill_kib", cfg.fill_kib.to_string()),
    }
    args.extend(kind.targets().iter().map(|t| (*t).to_owned()));
    args
}

/// One build, and whether Buck2 could be started for it.
fn build(cfg: &LoadConfig, lane: usize, n: usize, kind: Kind) -> (Outcome, bool) {
    let event_log = cfg.logs.join(format!("buck2-{lane}-{n}.json-lines.gz"));
    let console = cfg.logs.join(format!("buck2-{lane}-{n}.log"));
    let started = Instant::now();
    let spawned = std::fs::File::create(&console).and_then(|file| {
        Command::new(&cfg.buck2)
            .args(build_args(cfg, lane, n, kind, &event_log))
            .current_dir(&cfg.project)
            .stdin(Stdio::null())
            .stdout(Stdio::from(file.try_clone()?))
            .stderr(Stdio::from(file))
            .spawn()
    });
    let (exit, timed_out) = match spawned {
        Err(e) => {
            let why = format!("spawn {}: {e}", cfg.buck2.display());
            let outcome = Outcome {
                kind,
                lane,
                n,
                exit: None,
                timed_out: false,
                secs: 0.0,
                spawns: Err(why.clone()),
                failed_actions: Err(why),
            };
            return (outcome, false);
        }
        Ok(mut child) => wait_bounded(&mut child, cfg.build_timeout),
    };
    let text = std::fs::read_to_string(&console).unwrap_or_default();
    let outcome = Outcome {
        kind,
        lane,
        n,
        exit,
        timed_out,
        secs: started.elapsed().as_secs_f64(),
        spawns: parse(Tool::Buck2, &text).map_err(|e| e.to_string()),
        failed_actions: eventlog::failed_actions(&event_log),
    };
    (outcome, true)
}

/// Waits for `child` for at most `limit`: its exit code, or killed and `timed_out`.
pub(crate) fn wait_bounded(
    child: &mut std::process::Child,
    limit: Duration,
) -> (Option<i32>, bool) {
    let deadline = after(limit);
    loop {
        if let Some(status) = child.try_wait().ok().flatten() {
            return (status.code(), false);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return (None, true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
#[path = "load_tests.rs"]
mod tests;
