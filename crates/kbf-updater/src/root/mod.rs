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
pub mod socket;

use std::collections::BTreeMap;
use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::{DirBuilderExt, chown};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use kbf_step_runner::record::{CrashGate, Loaded as Stored, Record, RecordStore, State};
use kbf_step_runner::register::{Registry, Server};
use kbf_step_runner::{
    Bounds, Clock, DeadMan, Finished, RunError, Staged, StepLauncher, StepRunner, SystemClock,
    UpdateOrder,
};
use update_hook::{Params, Verdict};
use update_rights::declaration::MAX_CEILING_SECS;
use update_rights::{
    Accounts, Declared, EtcAccounts, Loaded, Policy, Refused, Spawner, Systemd, boot_unit_name,
    load_dir, run_boot,
};

use self::declare::{Decision, Place};
use self::launch::{Grants, HookLauncher, SYSTEMCTL, Shared, declared};
use self::releases::Releases;
use self::socket::{Answer, Request};

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

fn because(token: &'static str) -> impl Fn(String) -> Refusal {
    move |why| (token, why)
}

fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Makes `dir` mode 0700 if it is missing, its parents 0755, so that what lies beside it stays
/// reachable.
fn make_private_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir.parent().unwrap_or(dir))?;
    match DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    }
}

/// The record directory of a hook (`state_dir`), made by the updater and owned by the hook's
/// account (3.4.1). It is made mode 0700; a hook whose service's units execute from it gives the
/// service's group what it needs itself.
fn make_record_dir(declared: &Declared) -> io::Result<()> {
    let dir = Path::new(&declared.declaration().state_dir);
    make_private_dir(dir)?;
    let hook = declared.hook();
    chown(dir, Some(hook.uid), Some(hook.gid))
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
            // A declaration with no ceiling has the longest one a declaration may give.
            promotion_ceiling_secs: declaration
                .promotion_ceiling_secs
                .unwrap_or(MAX_CEILING_SECS),
            cleanup_retries: declaration.cleanup_retries,
        },
        declared_params,
        min_updater_protocol: None,
    })
}

/// The update an unfinished record holds, as it was begun: the releases are those its params
/// file names and the digests those the record holds, so a tree changed since is found by the
/// runner's check and halts.
fn resumed(declared: &Declared, rec: &Record) -> Result<UpdateOrder, Refusal> {
    let params = Params::read(&rec.params).map_err(|e| ("record", e.to_string()))?;
    let staged = Staged {
        dir: params.staged_release().unwrap_or_default(),
        digest: rec.artifact_digest.clone(),
    };
    let previous = params
        .previous_version()
        .map(str::to_owned)
        .zip(params.previous_release())
        .zip(rec.previous_digest.clone())
        .map(|((version, dir), digest)| (version, Staged { dir, digest }));
    order(
        declared,
        &rec.target_version,
        rec.tarball_digest.clone(),
        staged,
        previous,
    )
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
    /// Ends the dead-men and the registration server.
    stop: Arc<AtomicBool>,
}

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
    pub fn new(config: Config, parts: Parts) -> Result<RootPart, String> {
        make_private_dir(&config.state).map_err(text)?;
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
            stop: Arc::new(AtomicBool::new(false)),
        };
        root.reload()?;
        Ok(root)
    }

    /// Loads the declarations again.
    ///
    /// # Errors
    /// The directory cannot be listed.
    pub fn reload(&self) -> Result<(), String> {
        let policy = Policy {
            trusted_uid: self.config.trusted_uid,
            accounts: &*self.parts.accounts,
        };
        let fresh = load_dir(&self.config.declarations, &policy)
            .map_err(|e| format!("{}: {e}", self.config.declarations.display()))?;
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
        make_private_dir(&dir).map_err(|e| ("internal", e.to_string()))?;
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
        let decision = match &carried {
            Some(bytes) => {
                let loaded = self.loaded.read().unwrap_or_else(PoisonError::into_inner);
                declare::decide(&self.place(&unit.dir), &loaded, service, bytes)
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
        if ran.is_err() & installed {
            // Nothing ran to a verdict under the declaration this order installed (it fails a rule
            // of the load, or the runner refused the order): the one that was active goes back.
            let _ = declare::restore(&self.place(&unit.dir), service);
            let _ = self.reload();
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
        make_record_dir(&declared).map_err(|e| ("internal", text(e)))?;
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

    /// What an update's verdict leaves: on success the release is `current` and the declaration
    /// it installed stays; after a failure or a deferral the declaration it installed is taken
    /// back; after a halt nothing is touched.
    fn after(
        &self,
        service: &str,
        version: &str,
        dir: &Path,
        finished: &Finished,
    ) -> Result<(), Refusal> {
        let place = self.place(dir);
        let internal = |e: io::Error| ("internal", e.to_string());
        match finished.verdict {
            Verdict::Succeeded { .. } => {
                self.releases()
                    .set_current(service, version)
                    .map_err(internal)?;
                declare::settle(&place).map_err(internal)
            }
            Verdict::Halt => Ok(()),
            Verdict::Failed { .. } | Verdict::Deferred { .. } => {
                if declare::restore(&place, service).map_err(internal)? {
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

    /// The start pass of one service: after a boot (`first_since_boot`) a step the boot cut off
    /// is read as killed ([`boot::read_as_killed`]); an update the record holds unfinished is run
    /// to its verdict; then the declaration's boot entry runs, from the release of the last
    /// update that succeeded. The answer carries the verdict of a resumed update and the outcome
    /// of the boot entry (`boot`).
    pub fn start(&self, service: &str, first_since_boot: bool) -> Answer {
        self.try_start(service, first_since_boot)
            .unwrap_or_else(|(token, why)| Answer::refused(token, why))
    }

    fn try_start(&self, service: &str, first_since_boot: bool) -> Result<Answer, Refusal> {
        let declared = declared(&self.loaded, service).ok_or_else(|| self.unknown(service))?;
        let unit = self.service(service)?;
        let _busy = unit.busy.lock().unwrap_or_else(PoisonError::into_inner);
        let store = RecordStore::new(&unit.dir);
        let record = |e: kbf_step_runner::RecordError| ("record", e.to_string());
        if first_since_boot {
            boot::read_as_killed(&store).map_err(record)?;
        }
        let rec = match store.load().map_err(record)? {
            Stored::Current(rec) => Some(rec),
            _ => None,
        };
        let generation = rec.as_ref().map_or(0, |rec| rec.generation);
        let mut answer = Answer {
            ok: true,
            ..Answer::default()
        };
        if let Some(rec) = rec {
            let finished = match rec.state.clone() {
                State::Finished { verdict, reason } => Finished { verdict, reason },
                _ => {
                    make_record_dir(&declared).map_err(|e| ("internal", text(e)))?;
                    let finished = unit
                        .runner
                        .run(&resumed(&declared, &rec)?)
                        .map_err(run_refusal)?;
                    answer = Answer::finished(finished.clone());
                    finished
                }
            };
            // What the verdict leaves (the `current` link, the declaration kept or put back) is
            // done again: a root part killed between the verdict and those has not done them.
            self.after(service, &rec.target_version, &unit.dir, &finished)?;
        }
        answer.boot = self.boot_entry(service, generation)?;
        Ok(answer)
    }

    /// Runs the boot entry of `service`, if its declaration names one and a release of it was
    /// installed by an update: the tree is checked against its seal first, and a unit of the
    /// same name left by an earlier start is stopped. The outcome, `None` when nothing ran.
    fn boot_entry(&self, service: &str, generation: u64) -> Result<Option<String>, Refusal> {
        let declared = declared(&self.loaded, service).ok_or_else(|| self.unknown(service))?;
        let binary = &declared.declaration().binary;
        let current = match declared.declaration().boot {
            Some(_) => self
                .releases()
                .current(service)
                .map_err(because("not_staged"))?,
            None => None,
        };
        let Some(current) = current else {
            return Ok(None);
        };
        current
            .staged
            .check(Path::new(binary))
            .map_err(|e| ("artifact", e.to_string()))?;
        make_record_dir(&declared).map_err(|e| ("internal", text(e)))?;
        self.launcher
            .stop(&boot_unit_name(service, generation))
            .map_err(|e| ("boot", e.to_string()))?;
        let ran = run_boot(
            &*self.parts.systemd_run,
            &declared,
            generation,
            &current.staged.dir,
        )
        .map_err(|e| ("boot", e.to_string()))?;
        Ok(ran.map(|run| format!("{:?}", run.outcome).to_lowercase()))
    }

    /// Serves the registration socket at `socket` for the members of the group `gid`, on a
    /// thread of its own, until the root part is dropped or stopped.
    ///
    /// # Errors
    /// The socket's directory is not the trusted account's and closed to others, or the
    /// registrations kept on disk cannot be read.
    pub fn serve_registrations(&self, socket: &Path, gid: u32) -> Result<(), String> {
        let registry =
            Registry::load(&self.config.state, Arc::clone(&self.parts.clock)).map_err(text)?;
        let server = Server::bind(
            socket,
            self.config.trusted_uid,
            Some(gid),
            Arc::new(registry),
            Arc::new(Grants(Arc::clone(&self.loaded))),
        )
        .map_err(text)?;
        let server = Arc::new(server);
        let stop = Arc::clone(&self.stop);
        let _ = std::thread::spawn(move || server.serve(&stop));
        Ok(())
    }
}

/// Stops a running root part.
#[derive(Clone, Debug)]
pub struct Stop {
    flag: Arc<AtomicBool>,
    socket: PathBuf,
}

impl Stop {
    /// Ends the root part: it answers no more requests and returns.
    pub fn stop(&self) {
        self.flag.store(true, Ordering::Release);
        // Wakes the accept.
        let _ = UnixStream::connect(&self.socket);
    }
}

/// `kbf-updater root` with its flags and `config` ([`Config::of`] the flags, on a host): loads the
/// declarations, serves the registrations, runs the start pass of every service, then binds the
/// agent's socket, calls `ready` and answers the agent until stopped. Exit 1 with the reason when
/// it cannot start.
pub fn run_config(
    flags: &flags::Flags,
    config: Config,
    parts: Parts,
    ready: impl FnOnce(Stop),
) -> (u8, String) {
    match serve(flags, config, parts, ready) {
        Ok(()) => (0, String::new()),
        Err(why) => (1, format!("kbf-updater root: {why}\n")),
    }
}

fn serve(
    flags: &flags::Flags,
    config: Config,
    parts: Parts,
    ready: impl FnOnce(Stop),
) -> Result<(), String> {
    let accounts = Arc::clone(&parts.accounts);
    let agent = accounts.user(&flags.socket_user).ok_or_else(|| {
        format!(
            "--socket-user {:?} is not an account of this host",
            flags.socket_user
        )
    })?;
    let clock = Arc::clone(&parts.clock);
    let state = config.state.clone();
    let root = Arc::new(RootPart::new(config, parts)?);
    for refused in root.refused() {
        eprintln!(
            "kbf-updater root: refused {} ({}): {}",
            refused.service,
            refused.file.display(),
            refused.reason
        );
    }
    if let Some(register) = &flags.register {
        let gid = accounts.group(&register.group).ok_or_else(|| {
            format!(
                "--register-group {:?} is not a group of this host",
                register.group
            )
        })?;
        root.serve_registrations(&register.socket, gid)?;
    }
    let first = boot::boot_changed(&state, &*clock);
    for service in root.services() {
        let started = root.start(&service, first);
        eprintln!(
            "kbf-updater root: {service}: {}",
            serde_json::to_string(&started).unwrap_or_default()
        );
    }
    boot::commit_boot(&state, &*clock).map_err(text)?;
    let listener = socket::bind(&flags.socket, agent.uid, agent.gid).map_err(text)?;
    let stop = Stop {
        flag: Arc::new(AtomicBool::new(false)),
        socket: flags.socket.clone(),
    };
    ready(stop.clone());
    for stream in listener.incoming() {
        if stop.flag.load(Ordering::Acquire) {
            break;
        }
        let root = Arc::clone(&root);
        let _ = stream.map(|stream| {
            std::thread::spawn(move || socket::serve_connection(&stream, &|r| root.answer(r)))
        });
    }
    Ok(())
}
