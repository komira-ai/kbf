//! `kbf-updater root`: the root part of the generic host updater (docs/design/host-updates.md,
//! 3.3 to 3.5). It knows no service: everything it does for one comes from that service's
//! root-owned declaration in `/etc/updater.d` and from a staged release it verified.
//!
//! - It loads the declarations (`update_rights::load_dir`); one that fails a rule refuses that
//!   service alone, and the others are served.
//! - It takes "update service N to version V" from the unprivileged agent over a 0600 socket
//!   ([`socket`]) and runs that update through the step runner (`kbf_step_runner::StepRunner`),
//!   which writes the step record before and after every step and decides each next step from
//!   the table of the contract. Each step runs as a transient systemd unit with the rights of the
//!   declaration ([`launch::HookLauncher`] over `update_rights`), and a dead-man per service
//!   enforces its bound on a thread of its own.
//! - A declaration the release carries is installed only through `update_rights::gate`
//!   ([`declare`]): a widening, or a first install, is `Deferred` with the reason
//!   `rights_change` and runs nothing until `approve` records its digest.
//! - At its start it reads the record of every service ([`boot`]): after a boot a step cut off
//!   by it is a killed step and the table decides (a pending `cutover` is rolled back), and an
//!   update left unfinished is run to its verdict. Then it runs each declaration's boot entry,
//!   from the release of the last update that succeeded, through the same rights path with the
//!   hook-wide capabilities. Only then does it serve the agent.
//! - It serves the registration socket (`kbf_step_runner::register`) for the declared services.
//!
//! What the agent does not yet do: send its orders here. The step that fetches, verifies and
//! stages a release under `--releases-dir` ([`releases`]) is not built, so the agent keeps
//! updating kbf through the helper's verbs, and this part runs updates of releases staged by
//! another hand (docs/design/host-updates.md, 4.3, "As built").

pub mod boot;
pub mod declare;
pub mod flags;
pub mod launch;
pub mod releases;
mod serve;
pub mod socket;
mod start;
pub mod test_hook;

#[cfg(test)]
mod mod_tests;
#[cfg(test)]
mod test_hook_tests;

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, chown};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use kbf_step_runner::record::{CrashGate, Loaded as Stored, Record, RecordStore, State};
use kbf_step_runner::register::{Registry, RegistryError, Server};
use kbf_step_runner::{
    Bounds, Clock, DeadMan, Finished, RunError, Staged, StepLauncher, StepRunner, SystemClock,
    UpdateOrder,
};
use update_hook::{Params, Phase, Verdict};
use update_rights::{
    Accounts, Declared, EtcAccounts, Loaded, Policy, Refused, Spawner, Systemd, load_dir,
};

use self::declare::{Decision, Place};
use self::launch::{Grants, HookLauncher, SYSTEMCTL, Shared, declared};
use self::releases::{Releases, SealError};
use self::socket::{Answer, Request};
use self::start::Unread;

pub use self::serve::{Stop, announce_ready, notify_ready, run_config};

/// Where the root part keeps and finds things.
#[derive(Clone, Debug)]
pub struct Config {
    pub declarations: PathBuf,
    pub approvals: PathBuf,
    /// Its own directory: a directory per service with the step record and the params file, the
    /// registrations, the boot id. Made mode 0700.
    pub state: PathBuf,
    pub releases: PathBuf,
    /// The account that owns the declarations, the approvals and the seals.
    pub trusted_uid: u32,
}

impl Config {
    /// The settings of the flags; the trusted account is root.
    pub fn of(flags: &flags::Flags) -> Config {
        Config {
            declarations: flags.declarations.clone(),
            approvals: flags.approvals.clone(),
            state: flags.state.clone(),
            releases: flags.releases.clone(),
            trusted_uid: 0,
        }
    }
}

/// What the root part runs on: the accounts, the clock and the two programs of systemd.
pub struct Parts {
    pub accounts: Arc<dyn Accounts + Send + Sync>,
    pub clock: Arc<dyn Clock>,
    /// Starts a step's unit (`systemd-run`).
    pub systemd_run: Arc<dyn Spawner + Send + Sync>,
    /// Stops a unit (`systemctl`).
    pub systemctl: Arc<dyn Spawner + Send + Sync>,
    /// Where each service's step record stops the updater, for the tests that kill it at every
    /// write; `None` on a host.
    pub crash_gate: Option<CrashGate>,
    /// How often each service's dead-man looks at its running step, in seconds; `None` starts
    /// none.
    pub deadman_poll_secs: Option<u64>,
}

impl Parts {
    /// This host's.
    ///
    /// # Errors
    /// The account files cannot be read.
    pub fn host() -> io::Result<Parts> {
        Ok(Parts {
            accounts: Arc::new(EtcAccounts::system()?),
            clock: Arc::new(SystemClock),
            systemd_run: Arc::new(Systemd::system()),
            systemctl: Arc::new(Systemd::new(Path::new(SYSTEMCTL))),
            crash_gate: None,
            deadman_poll_secs: Some(1),
        })
    }
}

/// One service's runner, and the lock that keeps one update of it at a time.
struct Service {
    runner: StepRunner,
    busy: Mutex<()>,
    dir: PathBuf,
}

/// Why a request was not done: a token for the agent, and a text for the log.
type Refusal = (&'static str, String);

fn because<E: std::fmt::Display>(token: &'static str) -> impl Fn(E) -> Refusal {
    move |why| (token, why.to_string())
}

fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Why the root part cannot start or serve.
#[derive(Debug, thiserror::Error)]
pub enum RootError {
    #[error("{}: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{} is not a directory of the updater's own: {why}", .path.display())]
    Untrusted { path: PathBuf, why: &'static str },
    #[error("another root part holds {}, or its lock cannot be taken: {source}", .path.display())]
    Locked {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the registrations: {0}")]
    Registry(#[from] RegistryError),
}

fn io_at(path: &Path) -> impl Fn(io::Error) -> RootError + '_ {
    move |source| RootError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Makes `dir` mode 0700 if it is missing, its missing parents 0755, so that what lies beside it
/// stays reachable. `true` when it made it.
fn create_dir_0700(dir: &Path) -> io::Result<bool> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir.parent().unwrap_or(dir))?;
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// Makes `dir`, a directory of the root part's own bookkeeping, if it is missing, then checks it
/// and every directory above it before anything is read or written there: each a real directory
/// (no link), `dir` owned by the account the root part runs as (root on a host) and closed to
/// group and others, the ones above owned by root or that account and writable by nobody else.
fn make_private_dir(dir: &Path) -> Result<(), RootError> {
    let owner = crate::secret::euid();
    create_dir_0700(dir).map_err(io_at(dir))?;
    let mut at = PathBuf::new();
    let last = dir.components().count();
    for (n, part) in dir.components().enumerate() {
        at.push(part);
        let meta = fs::symlink_metadata(&at).map_err(io_at(&at))?;
        let why = match (meta.is_dir(), n + 1 == last) {
            (false, _) => Some("not a real directory"),
            (true, true) => ((meta.uid() != owner) | (meta.mode() & 0o077 != 0))
                .then_some("not the updater's, mode 0700"),
            (true, false) => (![0, owner].contains(&meta.uid()) | (meta.mode() & 0o022 != 0))
                .then_some("a directory above it others can change"),
        };
        if let Some(why) = why {
            return Err(RootError::Untrusted { path: at, why });
        }
    }
    Ok(())
}

/// The record directory of a hook (`state_dir`), made by the updater and owned by the hook's
/// account (3.4.1) when it is missing. An existing one is left as it is (a hook whose service's
/// units execute from it gave the service's group what it needs); it is checked before every
/// launch (`update_rights::check_state_dir`).
fn make_record_dir(declared: &Declared) -> io::Result<()> {
    let dir = Path::new(&declared.declaration().state_dir);
    if create_dir_0700(dir)? {
        let hook = declared.hook();
        chown(dir, Some(hook.uid), Some(hook.gid))?;
    }
    Ok(())
}

/// The step record of the service whose state directory is `dir`.
fn stored(dir: &Path) -> Result<Option<Box<Record>>, Refusal> {
    match RecordStore::new(dir).load() {
        Ok(Stored::Current(rec)) => Ok(Some(rec)),
        Ok(Stored::Absent) => Ok(None),
        Ok(Stored::Newer { schema }) => Err((
            "record_newer",
            format!("the step record has schema {schema}, newer than this updater reads"),
        )),
        Err(e) => Err(("record", e.to_string())),
    }
}

/// The verdict of `rec` that stands for an order of `version` from `release`: a halt holds every
/// order, and the same update that ended is answered with how it ended, as the step runner answers
/// it (a `Deferred` is run again).
fn standing(rec: &Record, version: &str, release: &releases::Release) -> Option<Finished> {
    let State::Finished { verdict, reason } = &rec.state else {
        return None;
    };
    let ended = matches!(verdict, Verdict::Succeeded { .. } | Verdict::Failed { .. });
    let stands = matches!(verdict, Verdict::Halt) | (ended & same_release(rec, version, release));
    stands.then(|| Finished {
        verdict: verdict.clone(),
        reason: reason.clone(),
    })
}

/// The refusal of an order whose run failed and whose installed declaration could not be put
/// back: the run's token and reason, and why the declaration stays.
fn kept_after(token: &str, why: &str, e: &str) -> Refusal {
    (
        "internal",
        format!("{token}: {why}; the declaration it installed is kept: {e}"),
    )
}

/// Whether `rec` is the update of `version` from `release`.
fn same_release(rec: &Record, version: &str, release: &releases::Release) -> bool {
    (rec.target_version == version)
        & (rec.tarball_digest == release.artifact_digest)
        & (rec.artifact_digest == release.staged.digest())
}

/// The update `UpdateOrder` the runner takes, from the declaration and the releases.
fn order(
    declared: &Declared,
    target: &str,
    artifact_digest: String,
    staged: Staged,
    previous: Option<(String, Staged)>,
) -> Result<UpdateOrder, Refusal> {
    let declaration = declared.declaration();
    let timeouts = &declaration.hook.timeout_secs;
    let declared_params = Params::parse_declared(&declaration.hook.params.to_string())
        .map_err(|e| ("declaration_refused", e.to_string()))?;
    let (previous_version, previous) = previous.unzip();
    Ok(UpdateOrder {
        service: declaration.name.clone(),
        target_version: target.to_owned(),
        previous_version,
        artifact_digest,
        staged,
        previous,
        hook_binary: PathBuf::from(&declaration.binary),
        bounds: Bounds {
            prepare_secs: timeouts.prepare,
            cutover_secs: timeouts.cutover,
            verify_secs: timeouts.verify,
            rollback_secs: timeouts.rollback,
            cleanup_secs: timeouts.cleanup,
            verify_deadline_secs: declaration.verify_deadline_secs,
            promotion_ceiling_secs: declaration.promotion_ceiling_secs,
            cleanup_retries: declaration.cleanup_retries,
        },
        declared_params,
        min_updater_protocol: None,
    })
}

/// What an unfinished record resumes as.
enum Resumed {
    /// The update as it was begun.
    Order(Box<UpdateOrder>),
    /// A release it runs from is not the one that was verified, and why: nothing from it runs.
    Changed(String),
}

/// The update an unfinished record holds, as it was begun: the releases its params file names,
/// each sealed again and compared with the digest the record holds. A tree that changed since is
/// [`Resumed::Changed`]; one that cannot be read now is refused, and the next start looks again.
fn resumed(declared: &Declared, rec: &Record) -> Result<Resumed, Refusal> {
    let params = Params::read(&rec.params).map_err(|e| ("record", e.to_string()))?;
    let seal = |tree: &Path, digest: &str| match releases::sealed_as(tree, digest) {
        Ok(staged) => Ok(Ok(staged)),
        Err(SealError::Changed(why)) => Ok(Err(why)),
        Err(e @ SealError::Unreadable(_)) => Err(("artifact", e.to_string())),
    };
    let staged_dir = params.staged_release().unwrap_or_default();
    let staged = match seal(staged_dir.as_path(), rec.artifact_digest.as_str())? {
        Ok(staged) => staged,
        Err(why) => return Ok(Resumed::Changed(why)),
    };
    let previous = match (
        params.previous_version(),
        params.previous_release(),
        rec.previous_digest.as_deref(),
    ) {
        (Some(version), Some(dir), Some(digest)) => match seal(dir.as_path(), digest)? {
            Ok(staged) => Some((version.to_owned(), staged)),
            Err(why) => return Ok(Resumed::Changed(why)),
        },
        _ => None,
    };
    order(
        declared,
        &rec.target_version,
        rec.tarball_digest.clone(),
        staged,
        previous,
    )
    .map(|order| Resumed::Order(Box::new(order)))
}

/// The generation of the step record in `dir`, `None` when there is none or it cannot be read:
/// a run that changed it has begun an update.
fn record_generation(dir: &Path) -> Option<u64> {
    match RecordStore::new(dir).load() {
        Ok(Stored::Current(rec)) => Some(rec.generation),
        _ => None,
    }
}

/// The token of a run that ended without a verdict.
fn run_refusal(e: RunError) -> Refusal {
    let busy = matches!(e, RunError::Busy { .. });
    (["runner", "busy"][usize::from(busy)], e.to_string())
}

impl Answer {
    /// The verdict of an update.
    fn finished(done: Finished) -> Answer {
        Answer {
            ok: true,
            verdict: Some(done.verdict),
            reason: done.reason,
            ..Answer::default()
        }
    }
}

/// The root part.
pub struct RootPart {
    config: Config,
    parts: Parts,
    loaded: Shared,
    launcher: Arc<HookLauncher>,
    services: Mutex<BTreeMap<String, Arc<Service>>>,
    /// The records the read after a boot could not read, until each is read.
    unread: Mutex<Unread>,
    /// Ends the dead-men and the registration server.
    stop: Arc<AtomicBool>,
    /// Held while the root part lives: one root part per state directory.
    _lock: File,
}

/// The lock file in the state directory.
pub const LOCK_FILE: &str = "lock";

impl Drop for RootPart {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl RootPart {
    /// Loads the declarations and makes the state directory.
    ///
    /// # Errors
    /// The state directory cannot be made, or the directory of declarations cannot be listed.
    pub fn new(config: Config, parts: Parts) -> Result<RootPart, RootError> {
        make_private_dir(&config.state)?;
        let lock_path = config.state.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&lock_path)
            .map_err(io_at(&lock_path))?;
        lock.try_lock().map_err(|e| RootError::Locked {
            path: lock_path.clone(),
            source: io::Error::from(e),
        })?;
        let loaded: Shared = Arc::new(RwLock::new(Loaded::default()));
        let launcher = Arc::new(HookLauncher::new(
            Arc::clone(&loaded),
            Arc::clone(&parts.systemd_run),
            Arc::clone(&parts.systemctl),
        ));
        let root = RootPart {
            config,
            parts,
            loaded,
            launcher,
            services: Mutex::default(),
            unread: Mutex::default(),
            stop: Arc::new(AtomicBool::new(false)),
            _lock: lock,
        };
        root.reload()?;
        Ok(root)
    }

    /// Loads the declarations again.
    ///
    /// # Errors
    /// The directory cannot be listed.
    pub fn reload(&self) -> Result<(), RootError> {
        let policy = Policy {
            trusted_uid: self.config.trusted_uid,
            accounts: &*self.parts.accounts,
        };
        let fresh = load_dir(&self.config.declarations, &policy)
            .map_err(io_at(&self.config.declarations))?;
        *self.loaded.write().unwrap_or_else(PoisonError::into_inner) = fresh;
        Ok(())
    }

    /// The services whose declarations loaded.
    pub fn services(&self) -> Vec<String> {
        let loaded = self.loaded.read().unwrap_or_else(PoisonError::into_inner);
        loaded.services.keys().cloned().collect()
    }

    /// The declarations refused at the last load, and why.
    pub fn refused(&self) -> Vec<Refused> {
        let loaded = self.loaded.read().unwrap_or_else(PoisonError::into_inner);
        loaded.refused.clone()
    }

    fn place<'a>(&'a self, dir: &'a Path) -> Place<'a> {
        Place {
            declarations: &self.config.declarations,
            approvals: &self.config.approvals,
            trusted_uid: self.config.trusted_uid,
            state: dir,
        }
    }

    fn releases(&self) -> Releases {
        Releases::new(&self.config.releases, self.config.trusted_uid)
    }

    /// The runner of `name`, made the first time, with its dead-man.
    fn service(&self, name: &str) -> Result<Arc<Service>, Refusal> {
        let mut services = self.services.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(service) = services.get(name) {
            return Ok(Arc::clone(service));
        }
        let dir = self.config.state.join(name);
        make_private_dir(&dir).map_err(because("internal"))?;
        let store = self
            .parts
            .crash_gate
            .iter()
            .fold(RecordStore::new(&dir), |store, gate| {
                store.with_crash_gate(Arc::clone(gate))
            });
        let launcher: Arc<dyn StepLauncher> = self.launcher.clone();
        let runner = StepRunner::new(Arc::clone(&launcher), Arc::clone(&self.parts.clock), store);
        if let Some(poll_secs) = self.parts.deadman_poll_secs {
            let deadman = DeadMan::new(
                runner.watch(),
                Arc::clone(&launcher),
                Arc::clone(&self.parts.clock),
            );
            let _ = deadman.spawn(Arc::clone(&self.stop), poll_secs);
        }
        let service = Arc::new(Service {
            runner,
            busy: Mutex::new(()),
            dir,
        });
        services.insert(name.to_owned(), Arc::clone(&service));
        Ok(service)
    }

    /// The answer to one request of the agent.
    pub fn answer(&self, request: &Request) -> Answer {
        match request {
            Request::Update { service, version } => self.update(service, version),
            Request::Status { service } => self.status(service),
        }
    }

    /// Runs the update of `service` to its staged release `version` to its verdict, or resumes
    /// it: the declaration the release carries goes through the gate first, and nothing runs
    /// while it is deferred.
    pub fn update(&self, service: &str, version: &str) -> Answer {
        self.try_update(service, version)
            .unwrap_or_else(|(token, why)| Answer::refused(token, why))
    }

    /// Why `service` cannot be served: refused at the load, or not declared at all.
    fn unknown(&self, service: &str) -> Refusal {
        let loaded = self.loaded.read().unwrap_or_else(PoisonError::into_inner);
        let refused = loaded.refused.iter().find(|r| r.service == service);
        refused.map_or_else(
            || {
                (
                    "unknown_service",
                    format!("no declaration names {service:?}"),
                )
            },
            |r| ("unmanaged", format!("{}: {}", r.file.display(), r.reason)),
        )
    }

    fn try_update(&self, service: &str, version: &str) -> Result<Answer, Refusal> {
        if !update_rights::names::is_service_name(service) {
            return Err(self.unknown(service));
        }
        self.read_if_unread(service)?;
        let releases = self.releases();
        let release = releases
            .staged(service, version)
            .map_err(because("not_staged"))?;
        let carried = declare::carried(&release.staged, service).map_err(because("artifact"))?;
        if declared(&self.loaded, service).is_none() & carried.is_none() {
            return Err(self.unknown(service));
        }
        let unit = self.service(service)?;
        let _busy = unit
            .busy
            .try_lock()
            .map_err(|_| ("busy", format!("an update of {service} is running")))?;
        // Before anything changes: a verdict that stands is answered (the same update that
        // ended, or a halt, which holds every order), and an unfinished update of another
        // release refuses this one.
        let rec = stored(&unit.dir)?;
        if let Some(rec) = &rec {
            if let Some(finished) = standing(rec, version, &release) {
                self.after(service, version, &unit.dir, &finished)?;
                return Ok(Answer::finished(finished));
            }
            // A post-success watcher is stopped by a new order (3.2): the runner does that.
            let finished = matches!(rec.state, State::Finished { .. });
            let watching = rec.progress.watching.is_some();
            if !finished & !watching & !same_release(rec, version, &release) {
                return Err((
                    "busy",
                    format!(
                        "an update of {service} to {} is not finished",
                        rec.target_version
                    ),
                ));
            }
        }
        let generation = rec.as_ref().map(|rec| rec.generation);
        let decision = match &carried {
            Some(bytes) => {
                // The active declaration as it is now, not as it was at the last load.
                self.reload().map_err(because("internal"))?;
                let active = declared(&self.loaded, service);
                declare::decide(
                    &self.place(&unit.dir),
                    active.as_ref(),
                    service,
                    bytes,
                    generation,
                )
                .map_err(because("declaration_refused"))?
            }
            None => Decision::Unchanged,
        };
        let installed = matches!(decision, Decision::Installed);
        if let Decision::Deferred(deferral) = decision {
            return Ok(Answer {
                ok: true,
                verdict: Some(Verdict::Deferred {
                    retry_after_secs: None,
                }),
                reason: Some(deferral.reason.to_owned()),
                digest: Some(deferral.digest.to_string()),
                widened: deferral.widened,
                ..Answer::default()
            });
        }
        let ran = self.run_order(service, version, &releases, release, &unit);
        // The runner began nothing under the declaration this order installed (it fails a rule of
        // the load, or the runner refused the order): the one that was active goes back. Once the
        // runner has begun the update's record, the declaration stays: the start pass runs that
        // update to its verdict under it and does what it leaves.
        if let Err((token, why)) = &ran
            && installed & (record_generation(&unit.dir) == generation)
        {
            declare::restore(&self.place(&unit.dir), service)
                .map_err(text)
                .and_then(|_| self.reload().map_err(text))
                .map_err(|e| kept_after(token, why, &e))?;
        }
        ran
    }

    /// Runs one order under the declaration now loaded, to its verdict.
    fn run_order(
        &self,
        service: &str,
        version: &str,
        releases: &Releases,
        release: releases::Release,
        unit: &Service,
    ) -> Result<Answer, Refusal> {
        self.reload().map_err(because("internal"))?;
        let declared = declared(&self.loaded, service)
            .ok_or_else(|| ("declaration_refused", self.unknown(service).1))?;
        make_record_dir(&declared).map_err(because("internal"))?;
        let previous = releases
            .current(service)
            .map_err(because("not_staged"))?
            .filter(|current| current.version != version)
            .map(|current| (current.version, current.staged));
        let order = order(
            &declared,
            version,
            release.artifact_digest,
            release.staged,
            previous,
        )?;
        let finished = unit.runner.run(&order).map_err(run_refusal)?;
        self.after(service, version, &unit.dir, &finished)?;
        Ok(Answer::finished(finished))
    }

    /// What an update's verdict leaves. When the release was committed (a success, or a failure
    /// after a step reported `reversible: false`, from which nothing rolls back) it is `current`
    /// and the declaration it installed stays; after any other failure or a deferral the
    /// declaration it installed is taken back; after a halt nothing is touched.
    fn after(
        &self,
        service: &str,
        version: &str,
        dir: &Path,
        finished: &Finished,
    ) -> Result<(), Refusal> {
        let place = self.place(dir);
        match finished.verdict {
            Verdict::Succeeded { .. }
            | Verdict::Failed {
                phase: Phase::AfterCommit,
                ..
            } => {
                self.releases()
                    .set_current(service, version)
                    .map_err(because("internal"))?;
                declare::settle(&place).map_err(because("internal"))
            }
            Verdict::Halt => Ok(()),
            Verdict::Failed { .. } | Verdict::Deferred { .. } => {
                if declare::restore(&place, service).map_err(because("internal"))? {
                    self.reload().map_err(because("internal"))?;
                }
                Ok(())
            }
        }
    }

    /// Where the last update of `service` is: the step and its state, and the verdict once it is
    /// over.
    pub fn status(&self, service: &str) -> Answer {
        if declared(&self.loaded, service).is_none() {
            let (token, why) = self.unknown(service);
            return Answer::refused(token, why);
        }
        let store = RecordStore::new(&self.config.state.join(service));
        let rec = match store.load() {
            Ok(Stored::Current(rec)) => rec,
            Ok(Stored::Absent) => {
                return Answer {
                    ok: true,
                    ..Answer::default()
                };
            }
            other => return Answer::refused("record", format!("{other:?}")),
        };
        let step = rec.step;
        let state = serde_json::to_value(&rec.state).unwrap_or_default();
        let (verdict, reason) = match rec.state {
            State::Finished { verdict, reason } => (Some(verdict), reason),
            _ => (None, None),
        };
        Answer {
            ok: true,
            step: Some(step),
            state: state["state"].as_str().map(str::to_owned),
            verdict,
            reason,
            ..Answer::default()
        }
    }

    /// Serves the registration socket at `socket` for the members of the group `gid`, on a
    /// thread of its own, until the root part is dropped or stopped.
    ///
    /// # Errors
    /// The socket's directory is not the trusted account's and closed to others, or the
    /// registrations kept on disk cannot be read.
    pub fn serve_registrations(&self, socket: &Path, gid: u32) -> Result<(), RootError> {
        let grants = Arc::new(Grants(Arc::clone(&self.loaded)));
        let registry = Registry::load(&self.config.state, Arc::clone(&self.parts.clock), &*grants)?;
        let server = Server::bind(
            socket,
            self.config.trusted_uid,
            gid,
            Arc::new(registry),
            grants,
        )?;
        let server = Arc::new(server);
        let stop = Arc::clone(&self.stop);
        let _ = std::thread::spawn(move || server.serve(&stop));
        Ok(())
    }
}
