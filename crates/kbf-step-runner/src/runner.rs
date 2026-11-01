//! The step runner: one update of one service, from `prepare` to its verdict, through a
//! [`StepLauncher`], with every move written to the step record first.
//!
//! The runner is the root part of the updater (docs/design/host-updates.md, 3.4 and 3.5). It
//! asks [`update_hook::next`] what follows each step, it runs nothing that is not in a staged
//! release it has checked against the digest the updater verified, and a crash at any point
//! leaves a record that [`StepRunner::run`] picks up: a step found `running` has its unit
//! stopped by name and is run again, a step found `done` is not.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use update_hook::{
    ARTIFACT_DIGEST_KEY, Context, Invocation, Next, Outcome, PREVIOUS_RELEASE_KEY,
    PREVIOUS_VERSION_KEY, Params, Phase, Report, STAGED_RELEASE_KEY, Scalar, Step,
    TARGET_VERSION_KEY, Verdict, next,
};

use crate::artifact::{ArtifactError, Staged};
use crate::clock::{Clock, Stamp};
use crate::deadman::Watch;
use crate::launcher::{LaunchError, StepCall, StepExit, StepLauncher};
use crate::record::{HookSel, Loaded, Progress, Record, RecordError, RecordStore, SCHEMA, State};

/// The protocol version of this updater: a declaration's `min_updater_protocol` above it is
/// `Deferred` with the reason `updater_old` until the updater has updated itself.
pub const UPDATER_PROTOCOL: u32 = 1;
/// How often a deferred `rollback` is run again before it is read as a failure.
pub const ROLLBACK_ATTEMPTS: u32 = 3;
/// The wait before a deferred step is run again when the hook gave no `retry_after_secs`.
pub const DEFAULT_RETRY_SECS: u64 = 60;
/// The wait between two runs of `verify` by the post-success watcher when the hook gave none.
pub const WATCH_POLL_SECS: u64 = 30;
/// The reason of a `halt` for a staged release that no longer has its digest.
pub const REASON_ARTIFACT_CHANGED: &str = "artifact_changed";
/// The longest wait a hook can ask for between two runs of a step.
pub const MAX_RETRY_SECS: u64 = 3600;
/// The reason a cleared halt is recorded with.
pub const REASON_HALT_CLEARED: &str = "halt_cleared";
/// The reason of a `Deferred` for a hook that needs a newer updater.
pub const REASON_UPDATER_OLD: &str = "updater_old";

/// The bounds of the declaration the runner reads (and no other key of it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub prepare_secs: u64,
    pub cutover_secs: u64,
    pub verify_secs: u64,
    pub rollback_secs: u64,
    pub cleanup_secs: u64,
    /// After `cutover` succeeded, `verify` must succeed by then or the update rolls back.
    pub verify_deadline_secs: u64,
    /// The ceiling on a pending promotion; also written to the params file.
    pub promotion_ceiling_secs: u64,
    /// How often a failed `cleanup` is run again.
    pub cleanup_retries: u32,
}

impl Bounds {
    /// The bound of one step.
    pub fn timeout(&self, step: Step) -> u64 {
        match step {
            Step::Prepare => self.prepare_secs,
            Step::Cutover => self.cutover_secs,
            Step::Verify => self.verify_secs,
            Step::Rollback => self.rollback_secs,
            Step::Cleanup => self.cleanup_secs,
        }
    }
}

/// One update of one service.
#[derive(Clone, Debug)]
pub struct UpdateOrder {
    /// The service, a name of ASCII letters, digits, `-` and `_`.
    pub service: String,
    pub target_version: String,
    pub previous_version: Option<String>,
    /// The sha256 of the release artifact the updater verified (its attested tarball): 64
    /// lower-case hex digits, the value in the order and in the release's SHA256SUMS. It is
    /// written to every step's params file as `updater.artifact_digest` = `sha256:<hex>`, and an
    /// order with another artifact digest is another update even for the same tree.
    ///
    /// **This crate does not bind the digest to the unpacked bytes.** The step that unpacks the
    /// tarball must hash the bytes as it reads them and unpack from that same stream, so that the
    /// digest is of what was unpacked, then seal the tree with [`Staged::seal`].
    pub artifact_digest: String,
    /// The verified staged release (the unpacked tree of that artifact).
    pub staged: Staged,
    /// The release being replaced, kept staged until `cleanup` has succeeded; `None` on the first
    /// install.
    pub previous: Option<Staged>,
    /// The declaration's `binary`: the hook executable within a release.
    pub hook_binary: PathBuf,
    pub bounds: Bounds,
    /// The declaration's `[hook.params]`.
    pub declared_params: Params,
    /// The declaration's `min_updater_protocol`.
    pub min_updater_protocol: Option<u32>,
}

/// How an update ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    pub verdict: Verdict,
    /// The hook's `reason` token, or the updater's own ([`REASON_ARTIFACT_CHANGED`],
    /// [`REASON_UPDATER_OLD`]).
    pub reason: Option<String>,
}

/// Why the runner stopped without a verdict. The record says where it was; calling
/// [`StepRunner::run`] again resumes.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Record(#[from] RecordError),
    #[error(transparent)]
    Launch(#[from] LaunchError),
    #[error("the order is not valid: {0}")]
    BadOrder(&'static str),
    #[error("an update of {service} to {target} is not finished")]
    Busy { service: String, target: String },
    #[error("the step record has schema {0}, newer than this updater writes")]
    RecordNewer(u64),
    /// The staged release could not be read (out of descriptors, an I/O error): nothing ran and
    /// the step is still `pending` in the record. Calling [`StepRunner::run`] again checks the
    /// release again.
    #[error(
        "the staged release cannot be read now; nothing ran and the step is still pending: {0}"
    )]
    Unreadable(ArtifactError),
    /// The dead-man could not stop the step at its bound, and the step ended later: it is left
    /// `running` in the record, so that [`StepRunner::run`] again stops its unit by name before
    /// anything else and runs the step again.
    #[error("the dead-man could not stop {unit} at its bound ({error}); the step is left running")]
    Unstopped { unit: String, error: String },
}

/// A test's way to make the check of the staged release fail with an I/O error before a step:
/// `Some` for the step whose check fails. It can only make a check fail, never pass.
pub type CheckFault = Arc<dyn Fn(Step) -> Option<io::Error> + Send + Sync>;

enum Ran {
    Exit(StepExit),
    NoStart,
}

/// Runs the updates of one service.
pub struct StepRunner {
    launcher: Arc<dyn StepLauncher>,
    clock: Arc<dyn Clock>,
    store: RecordStore,
    watch: Arc<Watch>,
    fault: Option<CheckFault>,
}

impl StepRunner {
    pub fn new(
        launcher: Arc<dyn StepLauncher>,
        clock: Arc<dyn Clock>,
        store: RecordStore,
    ) -> StepRunner {
        StepRunner {
            launcher,
            clock,
            store,
            watch: Watch::new(),
            fault: None,
        }
    }

    /// The same runner, its check of the staged release failed wherever `fault` says: the seam of
    /// the tests of a release that cannot be read for a moment.
    pub fn with_check_fault(mut self, fault: CheckFault) -> StepRunner {
        self.fault = Some(fault);
        self
    }

    /// What the dead-man watches: give it to [`crate::DeadMan::new`].
    pub fn watch(&self) -> Arc<Watch> {
        Arc::clone(&self.watch)
    }

    /// Clears a `halt`: the next order runs. `false` when the record holds no halt.
    pub fn clear_halt(&self) -> Result<bool, RunError> {
        let mut rec = match self.store.load()? {
            Loaded::Newer { schema } => return Err(RunError::RecordNewer(schema)),
            Loaded::Absent => return Ok(false),
            Loaded::Current(rec) => rec,
        };
        let halted = matches!(
            rec.state,
            State::Finished {
                verdict: Verdict::Halt,
                ..
            }
        );
        if !halted {
            return Ok(false);
        }
        rec.state = State::Finished {
            verdict: Verdict::Deferred {
                retry_after_secs: None,
            },
            reason: Some(REASON_HALT_CLEARED.to_owned()),
        };
        self.store.save(&rec)?;
        Ok(true)
    }

    /// How the last update of this service ended, `None` when none has or one is under way.
    pub fn last_result(&self) -> Result<Option<Finished>, RunError> {
        Ok(match self.store.load()? {
            Loaded::Current(rec) => match rec.state {
                State::Finished { verdict, reason } => Some(Finished { verdict, reason }),
                _ => None,
            },
            Loaded::Absent => None,
            Loaded::Newer { schema } => return Err(RunError::RecordNewer(schema)),
        })
    }

    /// Runs the update to its verdict, or resumes the unfinished one the record holds.
    ///
    /// An order for a different update while one is unfinished is [`RunError::Busy`], except
    /// that a post-success watcher is stopped by a new order (3.2). A record that is finished
    /// starts the next generation: the verdict of a finished update is read with
    /// [`StepRunner::last_result`].
    pub fn run(&self, order: &UpdateOrder) -> Result<Finished, RunError> {
        let staged = path_str(order.staged.dir())?;
        let previous = order
            .previous
            .as_ref()
            .map(|p| path_str(p.dir()))
            .transpose()?;
        if order.service.is_empty()
            || !order
                .service
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(RunError::BadOrder("the service name"));
        }
        let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
        if order.artifact_digest.len() != 64 || !order.artifact_digest.bytes().all(hex) {
            return Err(RunError::BadOrder("the artifact digest"));
        }
        if order
            .min_updater_protocol
            .is_some_and(|needed| needed > UPDATER_PROTOCOL)
        {
            return Ok(Finished {
                verdict: Verdict::Deferred {
                    retry_after_secs: None,
                },
                reason: Some(REASON_UPDATER_OLD.to_owned()),
            });
        }
        self.store.check_private()?;
        let generation = match self.store.load()? {
            Loaded::Newer { schema } => return Err(RunError::RecordNewer(schema)),
            Loaded::Absent => 1,
            Loaded::Current(rec) => {
                // A verdict stands: the same order again is answered with it and runs nothing,
                // and a halt holds the service for every order until `clear_halt`. A `Deferred`
                // is run again, because the order comes back after its wait.
                if let State::Finished { verdict, reason } = &rec.state {
                    let ended =
                        matches!(verdict, Verdict::Succeeded { .. } | Verdict::Failed { .. });
                    if matches!(verdict, Verdict::Halt) || (ended && same_update(&rec, order)) {
                        // The updater may have died between the verdict and the removal.
                        self.store.remove_params()?;
                        return Ok(Finished {
                            verdict: verdict.clone(),
                            reason: reason.clone(),
                        });
                    }
                }
                let unfinished = !matches!(rec.state, State::Finished { .. });
                if unfinished && same_update(&rec, order) {
                    return self.advance(order, *rec);
                }
                if unfinished && rec.progress.watching.is_none() {
                    return Err(RunError::Busy {
                        service: rec.service,
                        target: rec.target_version,
                    });
                }
                // A watcher that is stopped by a new order may have a `verify` still alive.
                if matches!(rec.state, State::Running { .. }) {
                    self.launcher.stop(&rec.unit(rec.step))?;
                }
                rec.generation + 1
            }
        };
        let rec = self.begin(order, generation, staged, previous)?;
        self.advance(order, rec)
    }

    /// Writes the params file and the first record of an update.
    fn begin(
        &self,
        order: &UpdateOrder,
        generation: u64,
        staged: &str,
        previous: Option<&str>,
    ) -> Result<Record, RunError> {
        let mut params = order
            .declared_params
            .clone()
            .with_promotion_ceiling_secs(order.bounds.promotion_ceiling_secs);
        let text = |s: &str| Scalar::Text(s.to_owned());
        params.insert(STAGED_RELEASE_KEY, text(staged));
        params.insert(TARGET_VERSION_KEY, text(&order.target_version));
        params.insert(
            ARTIFACT_DIGEST_KEY,
            text(&format!("sha256:{}", order.artifact_digest)),
        );
        if let Some(previous) = previous {
            params.insert(PREVIOUS_RELEASE_KEY, text(previous));
        }
        if let Some(version) = &order.previous_version {
            params.insert(PREVIOUS_VERSION_KEY, text(version));
        }
        self.store.save_params(&params.to_toml())?;
        let rec = Record {
            schema: SCHEMA,
            service: order.service.clone(),
            target_version: order.target_version.clone(),
            artifact_digest: order.staged.digest().to_owned(),
            tarball_digest: order.artifact_digest.clone(),
            previous_digest: order.previous.as_ref().map(|p| p.digest().to_owned()),
            generation,
            params: self.store.params_path(),
            step: Step::Prepare,
            attempt: 1,
            state: State::Pending,
            hook: HookSel::New,
            progress: Progress::default(),
        };
        self.store.save(&rec)?;
        Ok(rec)
    }

    fn advance(&self, order: &UpdateOrder, mut rec: Record) -> Result<Finished, RunError> {
        loop {
            match rec.state.clone() {
                State::Finished { verdict, reason } => {
                    self.store.remove_params()?;
                    return Ok(Finished { verdict, reason });
                }
                State::Running { .. } => {
                    // The updater died with the step's unit possibly alive: stop it by name and
                    // wait for it to end before the step is started again (3.5).
                    self.launcher.stop(&rec.unit(rec.step))?;
                    rec.attempt += 1;
                    rec.state = State::Pending;
                    self.store.save(&rec)?;
                }
                State::Pending => self.start(order, &mut rec)?,
                State::Done { code, last_line } => self.after(order, &mut rec, code, last_line)?,
            }
        }
    }

    /// Checks the release, records the step as running, runs it, records how it ended.
    fn start(&self, order: &UpdateOrder, rec: &mut Record) -> Result<(), RunError> {
        let release = match rec.hook {
            HookSel::New => &order.staged,
            HookSel::Previous => order.previous.as_ref().unwrap_or(&order.staged),
        };
        let checked = match self.fault.as_ref().and_then(|fault| fault(rec.step)) {
            Some(error) => Err(ArtifactError::Io(error)),
            None => release.check(&order.hook_binary),
        };
        let ran = match checked {
            Ok(executable) => self.launch(order, rec, &executable)?,
            // The hook cannot be started: a failed step, and for `rollback` the one case to go
            // to the previous release's hook (3.3).
            Err(ArtifactError::BadHookPath(_) | ArtifactError::HookMissing(_)) => Ran::NoStart,
            // The tree is not what was verified: nothing from it runs.
            Err(_) => {
                rec.state = State::Finished {
                    verdict: Verdict::Halt,
                    reason: Some(REASON_ARTIFACT_CHANGED.to_owned()),
                };
                return Ok(self.store.save(rec)?);
            }
        };
        rec.state = match ran {
            Ran::Exit(exit) => State::Done {
                code: exit.code,
                last_line: exit.last_line,
            },
            Ran::NoStart => {
                if rec.step == Step::Rollback
                    && rec.hook == HookSel::New
                    && order.previous.is_some()
                {
                    rec.hook = HookSel::Previous;
                    State::Pending
                } else {
                    State::Done {
                        code: None,
                        last_line: None,
                    }
                }
            }
        };
        Ok(self.store.save(rec)?)
    }

    fn launch(
        &self,
        order: &UpdateOrder,
        rec: &mut Record,
        executable: &Path,
    ) -> Result<Ran, RunError> {
        let started = Stamp::now(&*self.clock);
        let timeout = self.timeout(order, rec);
        // Nothing is left of the bound (the verify window has passed): the step is not started,
        // and never with a bound of 0, which systemd reads as no limit. It ends as a failure.
        if timeout == 0 {
            return Ok(Ran::Exit(StepExit {
                code: None,
                last_line: None,
            }));
        }
        let deadline_secs = started.mono_secs.saturating_add(timeout);
        rec.state = State::Running {
            started,
            deadline_secs,
        };
        self.store.save(rec)?;
        let unit = rec.unit(rec.step);
        let artifact = (rec.step == Step::Prepare).then(|| order.staged.dir().to_path_buf());
        self.watch.arm(&unit, deadline_secs);
        let result = self.launcher.run(&StepCall {
            service: &order.service,
            invocation: Invocation {
                step: rec.step,
                artifact,
                params: Some(rec.params.clone()),
            },
            executable,
            unit: &unit,
            timeout_secs: timeout,
        });
        self.watch.disarm();
        match result {
            Ok(exit) => Ok(Ran::Exit(exit)),
            Err(LaunchError::NoStart(_)) => Ok(Ran::NoStart),
            Err(e) => Err(e.into()),
        }
    }

    /// The bound of the step about to run: its own, and for `verify` before the update has
    /// succeeded no more than the time left of the verify window.
    fn timeout(&self, order: &UpdateOrder, rec: &Record) -> u64 {
        let own = order.bounds.timeout(rec.step);
        match (&rec.progress.verify_started, rec.step) {
            (Some(began), Step::Verify) => {
                let left = order
                    .bounds
                    .verify_deadline_secs
                    .saturating_sub(began.elapsed_secs(&*self.clock));
                own.min(left)
            }
            _ => own,
        }
    }

    /// Decides from a step that ended what comes next.
    fn after(
        &self,
        order: &UpdateOrder,
        rec: &mut Record,
        code: Option<i32>,
        last_line: Option<String>,
    ) -> Result<(), RunError> {
        let step = rec.step;
        let report = Report::read(last_line.as_deref().unwrap_or(""));
        let outcome = Outcome::from_exit(code);
        if rec.progress.watching.is_some() {
            self.watching(order, rec, outcome, &report);
            return Ok(self.store.save(rec)?);
        }
        let mut ctx = Context {
            committed: rec.progress.committed,
            promotion_pending: rec.progress.promotion_pending,
            then: rec.progress.then.clone(),
            verify_deadline_passed: self.verify_window_passed(order, rec, &report),
            retries_left: match step {
                Step::Rollback => rec.progress.retry < ROLLBACK_ATTEMPTS,
                Step::Cleanup => rec.progress.retry < order.bounds.cleanup_retries,
                _ => false,
            },
        };
        ctx.observe(step, &report);
        rec.progress.committed = ctx.committed;
        rec.progress.promotion_pending = ctx.promotion_pending;
        match next(step, outcome, &report, &ctx) {
            Next::Run(then) => {
                if step == Step::Cutover {
                    rec.progress.verify_started = Some(Stamp::now(&*self.clock));
                }
                rec.progress.retry = 0;
                self.move_to(rec, then);
            }
            Next::Again { step, after_secs } => {
                self.clock
                    .sleep_secs(wait_secs(after_secs, DEFAULT_RETRY_SECS));
                if step != Step::Verify {
                    rec.progress.retry += 1;
                }
                rec.attempt += 1;
                rec.state = State::Pending;
            }
            Next::Rollback { then } => {
                rec.progress.then = Some(then);
                rec.progress.reason = report.reason;
                rec.progress.retry = 0;
                self.move_to(rec, Step::Rollback);
            }
            Next::Report(verdict) => {
                let reason = report.reason.or(rec.progress.reason.clone());
                self.report(rec, verdict, reason);
            }
        }
        Ok(self.store.save(rec)?)
    }

    fn move_to(&self, rec: &mut Record, step: Step) {
        rec.step = step;
        rec.attempt = 1;
        rec.state = State::Pending;
    }

    /// Whether another run of `verify` after the wait would fall outside the verify window.
    fn verify_window_passed(&self, order: &UpdateOrder, rec: &Record, report: &Report) -> bool {
        let Some(began) = &rec.progress.verify_started else {
            return false;
        };
        let wait = wait_secs(report.retry_after_secs, DEFAULT_RETRY_SECS);
        began.elapsed_secs(&*self.clock).saturating_add(wait) >= order.bounds.verify_deadline_secs
    }

    /// The update is over, or (a pending promotion) the watcher begins.
    fn report(&self, rec: &mut Record, verdict: Verdict, reason: Option<String>) {
        match verdict {
            Verdict::Succeeded {
                promotion_pending: true,
            } => {
                rec.progress.watching = Some(Stamp::now(&*self.clock));
                // The verify window is over: the watcher is bounded by the promotion ceiling.
                rec.progress.verify_started = None;
                rec.progress.retry = 0;
                self.move_to(rec, Step::Verify);
            }
            verdict => rec.state = State::Finished { verdict, reason },
        }
    }

    /// The post-success watcher: `verify` again until it exits `0` without the flag. It is
    /// outside the table of the contract and never runs `rollback`: the commit has happened.
    fn watching(&self, order: &UpdateOrder, rec: &mut Record, outcome: Outcome, report: &Report) {
        let after_commit = Verdict::Failed {
            phase: Phase::AfterCommit,
            class: report.class,
        };
        let over = rec.progress.watching.as_ref().is_some_and(|began| {
            began.elapsed_secs(&*self.clock) >= order.bounds.promotion_ceiling_secs
        });
        let verdict = match outcome {
            Outcome::Success if !report.promotion_pending => Verdict::Succeeded {
                promotion_pending: false,
            },
            Outcome::Success if !over => {
                self.clock
                    .sleep_secs(wait_secs(report.retry_after_secs, WATCH_POLL_SECS));
                rec.attempt += 1;
                rec.state = State::Pending;
                return;
            }
            Outcome::Halt => Verdict::Halt,
            _ => after_commit,
        };
        rec.state = State::Finished {
            verdict,
            reason: report.reason.clone(),
        };
    }
}

/// How long to wait: what the hook asked for, or `default`, never more than [`MAX_RETRY_SECS`].
fn wait_secs(asked: Option<u64>, default: u64) -> u64 {
    asked.unwrap_or(default).min(MAX_RETRY_SECS)
}

/// Whether `rec` is the update `order` asks for.
fn same_update(rec: &Record, order: &UpdateOrder) -> bool {
    rec.service == order.service
        && rec.target_version == order.target_version
        && rec.artifact_digest == order.staged.digest()
        && rec.tarball_digest == order.artifact_digest
        && rec.previous_digest.as_deref() == order.previous.as_ref().map(Staged::digest)
}

fn path_str(path: &Path) -> Result<&str, RunError> {
    path.to_str()
        .ok_or(RunError::BadOrder("a release path that is not UTF-8"))
}
