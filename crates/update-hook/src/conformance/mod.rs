//! The conformance harness: runs a service's real hook against a fake service and checks that it
//! keeps the contract.
//!
//! The hook is **any executable**: the harness is given its path and `hook_args`, starts it as
//! the updater would (`<executable> <hook_args> <step> [<artifact>] --params <file>`, an empty
//! environment but what the fake service names, standard input empty, a process group of its
//! own) and judges what it does from outside. It has no part in the hook's language.
//!
//! A service crate implements [`FakeService`] (the world its hook acts on, in a directory) and
//! calls [`check`] from a test, with this crate as a dev-dependency. The checks:
//!
//! 1. **Well-formed ends.** Every run exits `0`, `1`, `75` or `76`, never by a signal, and
//!    writes the JSON last line (strictly read); `75` carries `retry_after_secs`.
//! 2. **Run twice.** Each step of a successful update, and `rollback`, run twice in a row: the
//!    second run ends the same way and leaves the world as the first left it.
//! 3. **Nothing outside the record.** No run changes the staged release or `outside/`.
//! 4. **Rollback tells the truth.** After `cutover`, a `rollback` that says it was reversible
//!    (the default) leaves the world as it was before `prepare`, with nothing of the update left
//!    behind; `cleanup` after a rollback changes nothing.
//! 5. **Crash-resume.** The hook is killed at every kill point its fake service reaches (see
//!    [`gate`]), and at every step boundary the step is run again after it finished; running
//!    on from there reaches the world an uninterrupted run reaches.
//! 6. **The sequences.** The updater's own next-step table ([`crate::next`]) is driven through
//!    success, and through a failure, a deferral and a halt at each step, with faults the fake
//!    service injects: the hook ends each step as injected, and a deferral leaves the world as
//!    it was.
//! 7. **Unknown reserved keys.** A params file with an `updater.*` key the hook does not know
//!    is no reason to refuse (a hook may refuse an unknown key of its own `[hook.params]`).
//! 8. **One at a time.** With an exclusive lock held on the record directory, every step
//!    refuses (exit `1`) and changes nothing.
//!
//! The params file is written once per sandbox and given to every step: the staged release
//! (byte for byte the argument of `prepare`), the target and the previous version, the
//! promotion ceiling and the fake's own table.
//!
//! Nothing here waits for a time: the only waiting is for a child to exit or to announce that it
//! is parked at a kill point.

pub mod gate;
mod run;
mod sandbox;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

pub use sandbox::{Entry, Sandbox, Tree, changes, snapshot};

use crate::{
    ARTIFACT_DIGEST_KEY, Context, Invocation, LineError, Next, Outcome, PREVIOUS_RELEASE_KEY,
    PREVIOUS_VERSION_KEY, Params, Report, STAGED_RELEASE_KEY, Scalar, Step, TARGET_VERSION_KEY,
    Verdict, next,
};
use run::{Run, run_step};

/// The steps of a successful update, in order.
const SUCCESS_PATH: [Step; 4] = [Step::Prepare, Step::Cutover, Step::Verify, Step::Cleanup];

/// The steps before a `rollback` of the same update.
const ROLLBACK_PATH: [Step; 2] = [Step::Prepare, Step::Cutover];

/// How many times the harness lets `verify` be deferred, and `cleanup` and `rollback` be
/// retried, before it says the deadline has passed or the retries are used up. Counts, not
/// times.
const RETRIES: u32 = 2;

/// A reserved key the harness invents, to see that a hook ignores what it does not know.
const PROBE_KEY: &str = "updater.conformance_probe";

/// The artifact digest the harness puts in the params file: a fixed identity of the right shape
/// (the harness stages no real tree to hash).
const ARTIFACT_DIGEST: &str =
    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The versions the harness puts in the params file.
const TARGET_VERSION: &str = "2.0.0";
const PREVIOUS_VERSION: &str = "1.0.0";

/// The seconds the harness puts in the reserved ceiling of the params file.
const CEILING_SECS: u64 = 3600;

/// The hook executable and its leading arguments.
#[derive(Clone, Debug)]
pub struct HookCommand {
    pub executable: PathBuf,
    /// Arguments before the step. The default is `["update-hook"]`, the convention of our own
    /// services; `[]` for an executable that takes the step first.
    pub hook_args: Vec<OsString>,
}

impl HookCommand {
    /// A hook with the conventional `hook_args`.
    pub fn new(executable: &Path) -> HookCommand {
        HookCommand {
            executable: executable.to_path_buf(),
            hook_args: vec![OsString::from("update-hook")],
        }
    }

    /// Replaces `hook_args`.
    pub fn with_hook_args(mut self, args: &[&str]) -> HookCommand {
        self.hook_args = args.iter().map(OsString::from).collect();
        self
    }
}

/// A way for a fake service to make a step end other than well.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The step fails (exit `1`).
    Fail,
    /// The host is busy (exit `75`).
    Defer,
    /// A check no version can repair fails (exit `76`).
    Halt,
}

impl Fault {
    /// The outcome the step must end with.
    pub fn outcome(self) -> Outcome {
        match self {
            Fault::Fail => Outcome::Failure,
            Fault::Defer => Outcome::Deferred,
            Fault::Halt => Outcome::Halt,
        }
    }
}

/// The world a hook acts on, as a directory a test can read, and the knobs the harness turns.
///
/// The harness makes a new [`Sandbox`] for every scenario and asks the fake to [`init`]
/// it. Everything the hook changes in the service must land in `sandbox.world()`, so that the
/// harness can compare worlds; the fake's changes should pass a [`gate::point`] each, so that
/// the harness can stop the hook before them.
///
/// [`init`]: FakeService::init
pub trait FakeService {
    /// Stages the release in `sandbox.artifact()` and sets the world to its starting state.
    fn init(&mut self, sandbox: &Sandbox) -> io::Result<()>;

    /// The environment of the hook process: how it finds the sandbox's directories. Nothing
    /// else of the harness's environment is inherited.
    fn env(&self, sandbox: &Sandbox) -> Vec<(OsString, OsString)>;

    /// The hook's `[hook.params]` table (the harness adds the reserved ceiling).
    fn params(&self) -> Params;

    /// Arranges for `step` to end as `fault`, from now on. `Ok(false)` if this fake cannot
    /// make that happen at that step (not every service has a `halt`); the scenario is then
    /// skipped and said to be.
    fn inject(&mut self, sandbox: &Sandbox, step: Step, fault: Fault) -> io::Result<bool>;

    /// What only the update needed and is still there (a cordon, a staged copy): empty after a
    /// `cleanup` that succeeded and after a `rollback` that was reversible.
    fn residue(&self, sandbox: &Sandbox) -> io::Result<Vec<String>>;
}

/// Why a hook was refused.
#[derive(Debug, thiserror::Error)]
pub enum Violation {
    #[error("harness I/O: {0}")]
    Io(#[from] io::Error),
    #[error("{step} died of a signal")]
    DiedBySignal { step: Step },
    #[error("{step} exited {code}, which is none of 0, 1, 75, 76; it said: {stderr}")]
    UnknownExit {
        step: Step,
        code: i32,
        stderr: String,
    },
    #[error("{step} left no usable last line ({error}); it said: {stderr}")]
    Line {
        step: Step,
        error: LineError,
        stderr: String,
    },
    #[error("{step} exited 75 without retry_after_secs")]
    DeferredWithoutRetryAfter { step: Step },
    #[error("{step} wrote outside its record: {changes:?}")]
    WroteOutside { step: Step, changes: Vec<String> },
    #[error("{step} ended {got:?}, expected {wanted:?}")]
    UnexpectedOutcome {
        step: Step,
        wanted: Outcome,
        got: Outcome,
    },
    #[error("{step} ended {first:?} the first time and {second:?} the second")]
    RerunOutcomeChanged {
        step: Step,
        first: Outcome,
        second: Outcome,
    },
    #[error("{step} run twice is not a no-op the second time: {changes:?}")]
    NotIdempotent { step: Step, changes: Vec<String> },
    #[error("rollback says it was reversible but the world is not as it was: {changes:?}")]
    RollbackClaimsReversible { changes: Vec<String> },
    #[error("{step} left what only the update needed: {left:?}")]
    ResidueLeft { step: Step, left: Vec<String> },
    #[error("cleanup after a rollback changed the world: {changes:?}")]
    CleanupCommitted { changes: Vec<String> },
    #[error("the fake service reached no kill point: crash-resume cannot be tested")]
    NoKillPoints,
    #[error("{step} reached fewer kill points than an uninterrupted run did (point {kill_at})")]
    KillPointVanished { step: Step, kill_at: u64 },
    #[error(
        "stopped in {step} at kill point {kill_at} (0: after the step) and run again, the world \
         differs from an uninterrupted run: {changes:?}"
    )]
    CrashResumeDiverges {
        step: Step,
        kill_at: u64,
        changes: Vec<String>,
    },
    #[error("the update was deferred but the world is not as it was: {changes:?}")]
    DeferredLeftState { changes: Vec<String> },
    #[error("{step} changed the world or its record but reached no kill point")]
    StepWithoutKillPoints { step: Step },
    #[error("{step} said reversible={first} the first time and {second} the second")]
    RerunReversibleChanged {
        step: Step,
        first: bool,
        second: bool,
    },
    #[error("{step} ended {outcome:?} when the params held an updater.* key it does not know")]
    RefusedUpdaterKey { step: Step, outcome: Outcome },
    #[error("{step} ran while another held the record lock: {outcome:?}, {changes:?}")]
    RanWhileLocked {
        step: Step,
        outcome: Outcome,
        changes: Vec<String>,
    },
}

/// The steps the updater ran for one scenario and what it reported.
#[derive(Debug, PartialEq, Eq)]
pub struct Trace {
    pub steps: Vec<(Step, Outcome)>,
    pub verdict: Verdict,
}

/// What [`check`] ran.
#[derive(Debug, PartialEq, Eq)]
pub struct Summary {
    /// Kill points over a whole successful update: how many times a hook was stopped.
    pub kill_points: u64,
    /// The scenarios run, as `step=fault` lists.
    pub scenarios: Vec<String>,
    /// The scenarios the fake service said it cannot produce.
    pub skipped: Vec<String>,
}

/// One step that ended: how, and what it said.
struct Done {
    outcome: Outcome,
    report: Report,
    run: Run,
}

/// The harness: a hook, a fake service and a directory to make sandboxes in.
pub struct Conformance<'a> {
    hook: &'a HookCommand,
    fake: &'a mut dyn FakeService,
    base: &'a Path,
    sandboxes: u32,
    first_install: bool,
}

/// Runs every check on `hook` against `fake`, in sandboxes made under `base` (a directory of
/// the caller's, which the harness fills and does not remove).
pub fn check(
    hook: &HookCommand,
    fake: &mut dyn FakeService,
    base: &Path,
) -> Result<Summary, Violation> {
    Conformance::new(hook, fake, base).check_all()
}

impl<'a> Conformance<'a> {
    pub fn new(
        hook: &'a HookCommand,
        fake: &'a mut dyn FakeService,
        base: &'a Path,
    ) -> Conformance<'a> {
        Conformance {
            hook,
            fake,
            base,
            sandboxes: 0,
            first_install: false,
        }
    }

    /// Runs the checks as a first install: the params file has neither previous release nor
    /// previous version. The default is an update, which has both.
    pub fn with_first_install(mut self) -> Conformance<'a> {
        self.first_install = true;
        self
    }

    /// All the checks of the module documentation.
    pub fn check_all(&mut self) -> Result<Summary, Violation> {
        self.idempotence()?;
        let kill_points = self.crash_resume()?;
        let (scenarios, skipped) = self.scenarios()?;
        self.tolerates_unknown_updater_keys()?;
        self.exclusive_record()?;
        // The same update as a first install: no previous release or version in the params.
        self.first_install = true;
        let first = self.idempotence();
        self.first_install = false;
        first?;
        Ok(Summary {
            kill_points,
            scenarios,
            skipped,
        })
    }

    /// A new sandbox, initialised by the fake, with the params file written.
    fn fresh(&mut self) -> Result<Sandbox, Violation> {
        self.sandboxes += 1;
        let sandbox = Sandbox::create(self.base.join(format!("run-{}", self.sandboxes)))?;
        self.fake.init(&sandbox)?;
        self.write_params(&sandbox, false)?;
        Ok(sandbox)
    }

    /// One file for the whole update, the same for every step, as the updater writes it. With
    /// `probe`, one `updater.*` key no hook knows.
    fn write_params(&self, sandbox: &Sandbox, probe: bool) -> Result<(), Violation> {
        let mut params = self.fake.params().with_promotion_ceiling_secs(CEILING_SECS);
        params.insert(
            STAGED_RELEASE_KEY,
            Scalar::Text(sandbox.artifact().to_string_lossy().into_owned()),
        );
        params.insert(TARGET_VERSION_KEY, Scalar::Text(TARGET_VERSION.to_owned()));
        params.insert(
            ARTIFACT_DIGEST_KEY,
            Scalar::Text(ARTIFACT_DIGEST.to_owned()),
        );
        if !self.first_install {
            // An update has both, and the previous release is a real tree.
            fs::write(sandbox.previous().join("VERSION"), PREVIOUS_VERSION)?;
            params.insert(
                PREVIOUS_RELEASE_KEY,
                Scalar::Text(sandbox.previous().to_string_lossy().into_owned()),
            );
            params.insert(
                PREVIOUS_VERSION_KEY,
                Scalar::Text(PREVIOUS_VERSION.to_owned()),
            );
        }
        if probe {
            params.insert(PROBE_KEY, Scalar::Text("x".to_owned()));
        }
        fs::write(sandbox.control().join("params.toml"), params.to_toml())?;
        Ok(())
    }

    fn world(sandbox: &Sandbox) -> Result<Tree, Violation> {
        Ok(snapshot(&sandbox.world())?)
    }

    /// The world, and the hook's record under `record/`: what a rerun must leave as it was and
    /// a resume must reach. The whole record is compared, so a record that grows on every run
    /// is a violation; a hook keeps its record idempotent like everything else.
    fn state(sandbox: &Sandbox) -> Result<Tree, Violation> {
        let mut tree = Self::world(sandbox)?;
        for (path, entry) in snapshot(&sandbox.record())? {
            tree.insert(Path::new("record").join(path), entry);
        }
        Ok(tree)
    }

    /// Runs `step` once and checks only that it left the artifact and `outside/` alone;
    /// `kill_at` above zero stops it at that kill point.
    fn exec_at(&self, sandbox: &Sandbox, step: Step, kill_at: u64) -> Result<Run, Violation> {
        let inv = Invocation {
            step,
            artifact: (step == Step::Prepare).then(|| sandbox.artifact()),
            params: Some(sandbox.control().join("params.toml")),
        };
        let before = sandbox.guarded()?;
        let run = run_step(self.hook, self.fake.env(sandbox), sandbox, &inv, kill_at)?;
        let moved = changes(&before, &sandbox.guarded()?);
        if !moved.is_empty() {
            return Err(Violation::WroteOutside {
                step,
                changes: moved,
            });
        }
        Ok(run)
    }

    /// Runs `step` and reads how it ended, refusing an end the contract does not allow.
    fn exec(&self, sandbox: &Sandbox, step: Step) -> Result<Done, Violation> {
        let run = self.exec_at(sandbox, step, 0)?;
        let code = run.code.ok_or(Violation::DiedBySignal { step })?;
        let outcome = Outcome::from_code(code).ok_or_else(|| Violation::UnknownExit {
            step,
            code,
            stderr: run.stderr.clone(),
        })?;
        let report = Report::from_stdout(&run.stdout).map_err(|error| Violation::Line {
            step,
            error,
            stderr: run.stderr.clone(),
        })?;
        if outcome == Outcome::Deferred && report.retry_after_secs.is_none() {
            return Err(Violation::DeferredWithoutRetryAfter { step });
        }
        Ok(Done {
            outcome,
            report,
            run,
        })
    }

    /// Runs `step` and requires success.
    fn exec_ok(&self, sandbox: &Sandbox, step: Step) -> Result<Done, Violation> {
        let done = self.exec(sandbox, step)?;
        require(step, Outcome::Success, done.outcome)?;
        Ok(done)
    }

    /// Runs `step` twice in a row and requires the second to be a no-op. Returns the first.
    fn twice(&self, sandbox: &Sandbox, step: Step) -> Result<Done, Violation> {
        let first = self.exec_ok(sandbox, step)?;
        let before = Self::state(sandbox)?;
        let second = self.exec(sandbox, step)?;
        if second.outcome != first.outcome {
            return Err(Violation::RerunOutcomeChanged {
                step,
                first: first.outcome,
                second: second.outcome,
            });
        }
        if second.report.reversible != first.report.reversible {
            return Err(Violation::RerunReversibleChanged {
                step,
                first: first.report.reversible,
                second: second.report.reversible,
            });
        }
        let moved = changes(&before, &Self::state(sandbox)?);
        if !moved.is_empty() {
            return Err(Violation::NotIdempotent {
                step,
                changes: moved,
            });
        }
        Ok(first)
    }

    /// After a successful `cleanup`: nothing of the update left.
    fn finished(&self, sandbox: &Sandbox) -> Result<(), Violation> {
        let left = self.fake.residue(sandbox)?;
        if !left.is_empty() {
            return Err(Violation::ResidueLeft {
                step: Step::Cleanup,
                left,
            });
        }
        Ok(())
    }

    /// After a successful `rollback`: if it said it was reversible, the world is as it was at
    /// `start` and nothing of the update is left.
    fn rolled_back(
        &self,
        sandbox: &Sandbox,
        start: &Tree,
        report: &Report,
    ) -> Result<(), Violation> {
        if report.reversible {
            let left = self.fake.residue(sandbox)?;
            if !left.is_empty() {
                return Err(Violation::ResidueLeft {
                    step: Step::Rollback,
                    left,
                });
            }
            let moved = changes(start, &Self::world(sandbox)?);
            if !moved.is_empty() {
                return Err(Violation::RollbackClaimsReversible { changes: moved });
            }
        }
        Ok(())
    }

    /// Each step of a successful update run twice, then the same for `rollback` after
    /// `cutover`, then `cleanup` after the rollback.
    pub fn idempotence(&mut self) -> Result<(), Violation> {
        let sandbox = self.fresh()?;
        for step in SUCCESS_PATH {
            self.twice(&sandbox, step)?;
        }
        self.finished(&sandbox)?;

        let sandbox = self.fresh()?;
        let start = Self::world(&sandbox)?;
        self.exec_ok(&sandbox, Step::Prepare)?;
        self.exec_ok(&sandbox, Step::Cutover)?;
        let rolled = self.twice(&sandbox, Step::Rollback)?;
        self.rolled_back(&sandbox, &start, &rolled.report)?;
        // The hook must not finish an update that was rolled back.
        let before = Self::world(&sandbox)?;
        self.exec(&sandbox, Step::Cleanup)?;
        let moved = changes(&before, &Self::world(&sandbox)?);
        if !moved.is_empty() {
            return Err(Violation::CleanupCommitted { changes: moved });
        }
        Ok(())
    }

    /// Stops each step at each of its kill points, and runs each step again after it finished;
    /// in both cases runs on to the end and compares the world with an uninterrupted run's.
    /// Returns the number of kill points.
    pub fn crash_resume(&mut self) -> Result<u64, Violation> {
        let sandbox = self.fresh()?;
        let mut points = Vec::new();
        for step in SUCCESS_PATH {
            points.push((step, self.measured(&sandbox, step)?));
        }
        self.finished(&sandbox)?;
        let want = Self::state(&sandbox)?;
        // The rollback path of the same update.
        let rolled = self.fresh()?;
        for step in ROLLBACK_PATH {
            self.exec_ok(&rolled, step)?;
        }
        points.push((Step::Rollback, self.measured(&rolled, Step::Rollback)?));
        let rollback_want = Self::state(&rolled)?;

        let total: u64 = points.iter().map(|(_, (gates, _))| gates).sum();
        if total == 0 {
            return Err(Violation::NoKillPoints);
        }
        // A step that changed things and reached no kill point, even when others did.
        if let Some((step, _)) = points
            .iter()
            .find(|(_, (gates, changed))| *gates == 0 && *changed)
        {
            return Err(Violation::StepWithoutKillPoints { step: *step });
        }
        let (_, (rollback_gates, _)) = points.remove(SUCCESS_PATH.len());
        for (index, (_, (gates, _))) in points.into_iter().enumerate() {
            for kill_at in 0..=gates {
                let (before, rest) = SUCCESS_PATH.split_at(index);
                self.resume(before, rest, kill_at, &want)?;
            }
        }
        for kill_at in 0..=rollback_gates {
            self.resume(&ROLLBACK_PATH, &[Step::Rollback], kill_at, &rollback_want)?;
        }
        Ok(total)
    }

    /// Runs `step`, requires success, and returns its kill points and whether it changed the
    /// world or the record.
    fn measured(&self, sandbox: &Sandbox, step: Step) -> Result<(u64, bool), Violation> {
        let before = Self::state(sandbox)?;
        let gates = self.exec_ok(sandbox, step)?.run.gates;
        Ok((gates, before != Self::state(sandbox)?))
    }

    /// Runs `before`, then the first of `rest` stopped at kill point `kill_at` (or, for `0`, to
    /// its end), then all of `rest`; the state must be `want`.
    fn resume(
        &mut self,
        before: &[Step],
        rest: &[Step],
        kill_at: u64,
        want: &Tree,
    ) -> Result<(), Violation> {
        let sandbox = self.fresh()?;
        for step in before {
            self.exec_ok(&sandbox, *step)?;
        }
        let step = rest[0];
        if kill_at == 0 {
            self.exec_ok(&sandbox, step)?;
        } else if !self.exec_at(&sandbox, step, kill_at)?.parked {
            return Err(Violation::KillPointVanished { step, kill_at });
        }
        for step in rest {
            self.exec_ok(&sandbox, *step)?;
        }
        let moved = changes(want, &Self::state(&sandbox)?);
        if !moved.is_empty() {
            return Err(Violation::CrashResumeDiverges {
                step,
                kill_at,
                changes: moved,
            });
        }
        Ok(())
    }

    /// Success, then a failure, a deferral and a halt at each of the four steps, and a few
    /// failures of `rollback` itself. Returns the scenarios run and those the fake cannot make.
    pub fn scenarios(&mut self) -> Result<(Vec<String>, Vec<String>), Violation> {
        let mut lists: Vec<Vec<(Step, Fault)>> = vec![Vec::new()];
        for step in SUCCESS_PATH {
            for fault in [Fault::Fail, Fault::Defer, Fault::Halt] {
                lists.push(vec![(step, fault)]);
            }
        }
        lists.push(vec![
            (Step::Cutover, Fault::Fail),
            (Step::Rollback, Fault::Fail),
        ]);
        lists.push(vec![
            (Step::Cutover, Fault::Fail),
            (Step::Rollback, Fault::Defer),
        ]);
        lists.push(vec![
            (Step::Verify, Fault::Fail),
            (Step::Rollback, Fault::Fail),
        ]);
        let (mut ran, mut skipped) = (Vec::new(), Vec::new());
        for faults in lists {
            let name = scenario_name(&faults);
            match self.scenario(&faults)? {
                Some(_) => ran.push(name),
                None => skipped.push(name),
            }
        }
        Ok((ran, skipped))
    }

    /// Drives the updater's table with these faults injected: `None` if the fake cannot make
    /// one of them.
    pub fn scenario(&mut self, faults: &[(Step, Fault)]) -> Result<Option<Trace>, Violation> {
        let sandbox = self.fresh()?;
        for (step, fault) in faults {
            if !self.fake.inject(&sandbox, *step, *fault)? {
                return Ok(None);
            }
        }
        let start = Self::world(&sandbox)?;
        let mut ctx = Context::default();
        let mut step = Step::Prepare;
        let mut steps = Vec::new();
        let mut tries: BTreeMap<Step, u32> = BTreeMap::new();
        loop {
            let done = self.exec(&sandbox, step)?;
            let wanted = faults
                .iter()
                .find(|(faulted, _)| *faulted == step)
                .map_or(Outcome::Success, |(_, fault)| fault.outcome());
            require(step, wanted, done.outcome)?;
            steps.push((step, done.outcome));
            if step == Step::Rollback && done.outcome == Outcome::Success {
                self.rolled_back(&sandbox, &start, &done.report)?;
            }
            ctx.observe(step, &done.report);
            let tried = tries.entry(step).or_default();
            *tried += 1;
            ctx.retries_left = *tried <= RETRIES;
            ctx.verify_deadline_passed = *tried > RETRIES;
            match next(step, done.outcome, &done.report, &ctx) {
                Next::Run(to) | Next::Again { step: to, .. } => step = to,
                Next::Rollback { then } => {
                    ctx.then = Some(then);
                    step = Step::Rollback;
                }
                Next::Report(verdict) => {
                    self.concluded(&sandbox, &start, &verdict)?;
                    return Ok(Some(Trace { steps, verdict }));
                }
            }
        }
    }

    /// A deferral leaves the world as it was.
    fn concluded(
        &self,
        sandbox: &Sandbox,
        start: &Tree,
        verdict: &Verdict,
    ) -> Result<(), Violation> {
        if matches!(verdict, Verdict::Deferred { .. }) {
            let moved = changes(start, &Self::world(sandbox)?);
            if !moved.is_empty() {
                return Err(Violation::DeferredLeftState { changes: moved });
            }
        }
        Ok(())
    }

    /// A params file with one more `updater.*` key than the hook knows is no reason to refuse:
    /// the updater may add reserved keys without breaking an older hook.
    pub fn tolerates_unknown_updater_keys(&mut self) -> Result<(), Violation> {
        let sandbox = self.fresh()?;
        self.write_params(&sandbox, true)?;
        let done = self.exec(&sandbox, Step::Prepare)?;
        if done.outcome != Outcome::Success {
            return Err(Violation::RefusedUpdaterKey {
                step: Step::Prepare,
                outcome: done.outcome,
            });
        }
        Ok(())
    }

    /// With an exclusive lock held on the record directory, each step refuses and changes
    /// nothing; once the lock is released, a step runs.
    pub fn exclusive_record(&mut self) -> Result<(), Violation> {
        let sandbox = self.fresh()?;
        let held = File::open(sandbox.record())?;
        held.lock()?;
        let world = Self::world(&sandbox)?;
        let record = snapshot(&sandbox.record())?;
        for step in Step::ALL {
            let done = self.exec(&sandbox, step)?;
            let mut moved = changes(&world, &Self::world(&sandbox)?);
            moved.extend(changes(&record, &snapshot(&sandbox.record())?));
            if done.outcome != Outcome::Failure || !moved.is_empty() {
                return Err(Violation::RanWhileLocked {
                    step,
                    outcome: done.outcome,
                    changes: moved,
                });
            }
        }
        held.unlock()?;
        self.exec_ok(&sandbox, Step::Prepare)?;
        Ok(())
    }
}

/// `step` ended `got`; it had to end `wanted`.
fn require(step: Step, wanted: Outcome, got: Outcome) -> Result<(), Violation> {
    if wanted == got {
        Ok(())
    } else {
        Err(Violation::UnexpectedOutcome { step, wanted, got })
    }
}

/// `cutover=Fail,rollback=Fail`; `none` for no fault.
fn scenario_name(faults: &[(Step, Fault)]) -> String {
    let named: Vec<String> = faults
        .iter()
        .map(|(step, fault)| format!("{step}={fault:?}"))
        .collect();
    if named.is_empty() {
        "none".to_owned()
    } else {
        named.join(",")
    }
}
