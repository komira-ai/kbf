//! One scripted fake per port, for this crate's tests and for the adapters' tests to come.
//!
//! Every fake is a cheap handle (clones share state) and records what it was asked in one
//! shared [`Log`], so a test can say not only that host B was installed but that it was
//! installed after host A was healthy, and that host C was never asked about at all. Unscripted
//! answers are the happy path: safe to stop, install succeeds, healthy at once.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::inventory::{Host, Role};
use crate::ports::{
    Apply, Clock, Drain, DrainStatus, FrontendControl, Health, HealthCheck, InstallAnswer,
    Installed, Installer, Placed, PortError, Report, SafeToStop, Stop, Verdict,
};
use crate::version::Version;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Something a port was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Check(String),
    Install(String, String),
    Query(String),
    Health(String),
    /// A node drain was asked for, with the deadline in seconds.
    Drain(String, Option<u64>),
    /// The node's leases were read.
    Leases(String),
    Undrain(String),
    /// The host's frontend member was asked what commit it was built from.
    Commit(String),
    /// The host was asked to restart the frontend unit of this name.
    Restart(String, String),
    /// The host's frontend member was told to drain itself.
    MemberDrain(String),
    /// The host's frontend member was asked how its own drain stands.
    MemberDrainStatus(String),
    /// The host's frontend member was asked whether it is ready.
    Ready(String),
}

impl Event {
    /// The call as a short word: its kind, then the detail a test tells calls apart by (the
    /// version of an install, the deadline of a drain, the name of a unit).
    pub fn word(&self) -> String {
        match self {
            Event::Check(_) => "check".to_owned(),
            Event::Install(_, version) => format!("install {version}"),
            Event::Query(_) => "query".to_owned(),
            Event::Health(_) => "health".to_owned(),
            Event::Drain(_, secs) => match secs {
                Some(secs) => format!("drain {secs}"),
                None => "drain none".to_owned(),
            },
            Event::Leases(_) => "leases".to_owned(),
            Event::Undrain(_) => "undrain".to_owned(),
            Event::Commit(_) => "commit".to_owned(),
            Event::Restart(_, unit) => format!("restart {unit}"),
            Event::MemberDrain(_) => "member drain".to_owned(),
            Event::MemberDrainStatus(_) => "member drain status".to_owned(),
            Event::Ready(_) => "ready".to_owned(),
        }
    }
}

/// The calls made to the fakes, in order.
#[derive(Clone, Default)]
pub struct Log(Arc<Mutex<Vec<Event>>>);

impl Log {
    fn push(&self, e: Event) {
        lock(&self.0).push(e);
    }

    pub fn events(&self) -> Vec<Event> {
        lock(&self.0).clone()
    }

    /// The aliases installed, in order, one entry per install call.
    pub fn installs(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Install(alias, _) => Some(alias),
                _ => None,
            })
            .collect()
    }

    /// Every call that names `alias`.
    pub fn touching(&self, alias: &str) -> Vec<Event> {
        self.events()
            .into_iter()
            .filter(|e| match e {
                Event::Check(a)
                | Event::Install(a, _)
                | Event::Query(a)
                | Event::Health(a)
                | Event::Drain(a, _)
                | Event::Leases(a)
                | Event::Undrain(a)
                | Event::Commit(a)
                | Event::MemberDrain(a)
                | Event::MemberDrainStatus(a)
                | Event::Ready(a)
                | Event::Restart(a, _) => a == alias,
            })
            .collect()
    }

    pub fn clear(&self) {
        lock(&self.0).clear();
    }
}

/// Per alias: answers to give first, in order, then one answer for ever after.
struct Script<T: Clone> {
    map: Mutex<BTreeMap<String, (VecDeque<T>, T)>>,
    default: T,
}

impl<T: Clone> Script<T> {
    fn new(default: T) -> Script<T> {
        Script {
            map: Mutex::new(BTreeMap::new()),
            default,
        }
    }

    fn set(&self, alias: &str, first: Vec<T>, then: T) {
        lock(&self.map).insert(alias.to_owned(), (first.into(), then));
    }

    fn has(&self, alias: &str) -> bool {
        lock(&self.map).contains_key(alias)
    }

    fn next(&self, alias: &str) -> T {
        match lock(&self.map).get_mut(alias) {
            Some((queue, then)) => queue.pop_front().unwrap_or_else(|| then.clone()),
            None => self.default.clone(),
        }
    }
}

/// A clock that moves only when something sleeps on it.
#[derive(Clone, Default)]
pub struct FakeClock(Arc<Mutex<Duration>>);

impl FakeClock {
    pub fn new() -> FakeClock {
        FakeClock::default()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        *lock(&self.0)
    }

    fn sleep(&self, d: Duration) {
        *lock(&self.0) += d;
    }
}

/// The verdict "safe, nothing to report".
pub fn safe() -> Stop {
    Stop::Answer(Verdict {
        safe: true,
        reasons: Vec::new(),
        planned_down: false,
    })
}

/// A verdict "not safe", with one reason.
pub fn unsafe_because(reason: &str) -> Stop {
    Stop::Answer(Verdict {
        safe: false,
        reasons: vec![reason.to_owned()],
        planned_down: false,
    })
}

#[derive(Clone)]
pub struct FakeSafeToStop {
    log: Log,
    script: Arc<Script<Result<Stop, PortError>>>,
}

impl FakeSafeToStop {
    pub fn new(log: &Log) -> FakeSafeToStop {
        FakeSafeToStop {
            log: log.clone(),
            script: Arc::new(Script::new(Ok(safe()))),
        }
    }

    pub fn script(
        &self,
        alias: &str,
        first: Vec<Result<Stop, PortError>>,
        then: Result<Stop, PortError>,
    ) {
        self.script.set(alias, first, then);
    }
}

impl SafeToStop for FakeSafeToStop {
    fn check(&self, host: &Host) -> Result<Stop, PortError> {
        self.log.push(Event::Check(host.alias.clone()));
        self.script.next(&host.alias)
    }
}

/// The node id and the running commit a host reports.
type Identity = (Option<String>, Option<String>);

type Hook = Arc<dyn Fn(&Host) + Send + Sync>;

#[derive(Clone)]
pub struct FakeInstaller {
    log: Log,
    installed: Arc<Mutex<BTreeMap<String, Installed>>>,
    /// `None` answers as the host's role does: `Installed` for a Linux host, `ApplyStarted`
    /// for a Mac.
    installs: Arc<Script<Result<Option<InstallAnswer>, PortError>>>,
    /// Per alias: the apply state a Mac reports on each query once an apply is going.
    applies: Arc<Script<Option<Apply>>>,
    /// The Macs whose apply has started: an install was answered, or a test said so.
    applying: Arc<Mutex<BTreeSet<String>>>,
    queries: Arc<Script<Option<Result<Installed, PortError>>>>,
    /// Per alias: the node id and the running commit the host reports.
    identity: Arc<Mutex<BTreeMap<String, Identity>>>,
    hook: Arc<Mutex<Option<Hook>>>,
    /// Per alias: the answers of `restart_frontend`.
    restarts: Arc<Script<Result<(), PortError>>>,
    /// Aliases that stand for the host of another alias, and so share what it has installed.
    twins: Arc<Mutex<BTreeMap<String, String>>>,
}

impl FakeInstaller {
    pub fn new(log: &Log) -> FakeInstaller {
        FakeInstaller {
            log: log.clone(),
            installed: Arc::default(),
            installs: Arc::new(Script::new(Ok(None))),
            applies: Arc::new(Script::new(None)),
            applying: Arc::default(),
            queries: Arc::new(Script::new(None)),
            identity: Arc::default(),
            hook: Arc::default(),
            restarts: Arc::new(Script::new(Ok(()))),
            twins: Arc::default(),
        }
    }

    /// The alias whose install state `alias` has: its own, unless it was made the twin of
    /// another with [`FakeInstaller::same_host`].
    fn key(&self, alias: &str) -> String {
        lock(&self.twins)
            .get(alias)
            .cloned()
            .unwrap_or_else(|| alias.to_owned())
    }

    /// Make `alias` another name of the host `of`: the two share what is installed, as two
    /// frontend members listed on one host do. Say it before anything is installed.
    pub fn same_host(&self, alias: &str, of: &str) {
        lock(&self.twins).insert(alias.to_owned(), of.to_owned());
    }

    /// What the host has installed before any install (unscripted hosts have `0.0.0`).
    pub fn set_installed(&self, alias: &str, what: Installed) {
        lock(&self.installed).insert(self.key(alias), what);
    }

    pub fn installed(&self, alias: &str) -> Installed {
        lock(&self.installed)
            .get(&self.key(alias))
            .cloned()
            .unwrap_or_else(|| Installed::Version("0.0.0".to_owned()))
    }

    /// Answers of `restart_frontend` for a host: `first` in order, then `then`.
    pub fn script_restart(
        &self,
        alias: &str,
        first: Vec<Result<(), PortError>>,
        then: Result<(), PortError>,
    ) {
        self.restarts.set(alias, first, then);
    }

    /// Answers of `install` for a host: `first` in order, then `then`. A successful install
    /// makes the host report the version.
    pub fn script_install(
        &self,
        alias: &str,
        first: Vec<Result<(), PortError>>,
        then: Result<(), PortError>,
    ) {
        let answer = |r: Result<(), PortError>| r.map(|()| None);
        self.installs
            .set(alias, first.into_iter().map(answer).collect(), answer(then));
    }

    /// As [`FakeInstaller::script_install`], with the answer each install gives.
    pub fn script_install_answers(
        &self,
        alias: &str,
        first: Vec<Result<InstallAnswer, PortError>>,
        then: Result<InstallAnswer, PortError>,
    ) {
        let answer = |r: Result<InstallAnswer, PortError>| r.map(Some);
        self.installs
            .set(alias, first.into_iter().map(answer).collect(), answer(then));
    }

    /// The apply state a Mac host reports on each query once its apply has started: `first`
    /// in order, then `then`. A Mac reports none before that (an install answered, or
    /// [`FakeInstaller::begin_apply`]) and, unscripted, `done` after; a Linux host always
    /// reports none.
    pub fn script_apply(&self, alias: &str, first: Vec<Option<Apply>>, then: Option<Apply>) {
        self.applies.set(alias, first, then);
    }

    /// The Mac's apply is going now, as if an install had started it before the test began.
    pub fn begin_apply(&self, alias: &str) {
        lock(&self.applying).insert(alias.to_owned());
        if !self.applies.has(alias) {
            self.applies.set(alias, Vec::new(), Some(Apply::Done));
        }
    }

    /// Answers of `query` for a host: `first` in order, then what the host has installed.
    pub fn script_query(&self, alias: &str, first: Vec<Result<Installed, PortError>>) {
        self.queries
            .set(alias, first.into_iter().map(Some).collect(), None);
    }

    /// What the host reports as its node id and running commit. Unscripted hosts report the
    /// node id `<alias>-node` and no commit.
    pub fn set_identity(&self, alias: &str, node_id: Option<&str>, running_commit: Option<&str>) {
        lock(&self.identity).insert(
            alias.to_owned(),
            (
                node_id.map(str::to_owned),
                running_commit.map(str::to_owned),
            ),
        );
    }

    /// Call `f` at the start of every install (a test reads the record from it).
    pub fn on_install(&self, f: impl Fn(&Host) + Send + Sync + 'static) {
        *lock(&self.hook) = Some(Arc::new(f));
    }
}

impl Installer for FakeInstaller {
    fn install(&self, host: &Host, version: &Version) -> Result<InstallAnswer, PortError> {
        self.log
            .push(Event::Install(host.alias.clone(), version.to_string()));
        // Cloned out, so a hook that re-enters the installer does not deadlock on the slot.
        let hook = lock(&self.hook).clone();
        if let Some(hook) = hook {
            hook(host);
        }
        let answer = self.installs.next(&host.alias)?.unwrap_or(match host.role {
            Role::MacDaemon => InstallAnswer::ApplyStarted,
            _ => InstallAnswer::Installed,
        });
        self.set_installed(&host.alias, Installed::Version(version.to_string()));
        self.begin_apply(&host.alias);
        Ok(answer)
    }

    fn report(&self, host: &Host) -> Result<Report, PortError> {
        self.log.push(Event::Query(host.alias.clone()));
        let installed = self
            .queries
            .next(&host.alias)
            .unwrap_or_else(|| Ok(self.installed(&host.alias)))?;
        let (node_id, running_commit) = lock(&self.identity)
            .get(&host.alias)
            .cloned()
            .unwrap_or_else(|| (Some(format!("{}-node", host.alias)), None));
        let apply = match host.role {
            Role::MacDaemon if lock(&self.applying).contains(&host.alias) => {
                self.applies.next(&host.alias)
            }
            _ => None,
        };
        Ok(Report {
            installed,
            running_commit,
            node_id,
            apply,
        })
    }

    fn restart_frontend(&self, host: &Host, name: &str) -> Result<(), PortError> {
        self.log
            .push(Event::Restart(host.alias.clone(), name.to_owned()));
        self.restarts.next(&host.alias)
    }
}

/// A scripted drain of nodes: every call is in the [`Log`] by the host's alias.
#[derive(Clone)]
pub struct FakeDrain {
    log: Log,
    drains: Arc<Script<Result<(), PortError>>>,
    leases: Arc<Script<Result<usize, PortError>>>,
    undrains: Arc<Script<Result<(), PortError>>>,
}

impl FakeDrain {
    pub fn new(log: &Log) -> FakeDrain {
        FakeDrain {
            log: log.clone(),
            drains: Arc::new(Script::new(Ok(()))),
            leases: Arc::new(Script::new(Ok(0))),
            undrains: Arc::new(Script::new(Ok(()))),
        }
    }

    pub fn script_drain(
        &self,
        alias: &str,
        first: Vec<Result<(), PortError>>,
        then: Result<(), PortError>,
    ) {
        self.drains.set(alias, first, then);
    }

    /// The lease counts a node reports, in order, then `then` for ever after.
    pub fn script_leases(
        &self,
        alias: &str,
        first: Vec<Result<usize, PortError>>,
        then: Result<usize, PortError>,
    ) {
        self.leases.set(alias, first, then);
    }

    pub fn script_undrain(
        &self,
        alias: &str,
        first: Vec<Result<(), PortError>>,
        then: Result<(), PortError>,
    ) {
        self.undrains.set(alias, first, then);
    }
}

impl Drain for FakeDrain {
    fn placed(&self, _host: &Host) -> Result<Placed, PortError> {
        Ok(Placed::In)
    }

    fn drain(&self, host: &Host, deadline: Option<Duration>) -> Result<(), PortError> {
        self.log.push(Event::Drain(
            host.alias.clone(),
            deadline.map(|d| d.as_secs()),
        ));
        self.drains.next(&host.alias)
    }

    fn leases(&self, host: &Host) -> Result<usize, PortError> {
        self.log.push(Event::Leases(host.alias.clone()));
        self.leases.next(&host.alias)
    }

    fn undrain(&self, host: &Host) -> Result<(), PortError> {
        self.log.push(Event::Undrain(host.alias.clone()));
        self.undrains.next(&host.alias)
    }
}

/// The commit an unscripted member reports (40 digits, of which a version names 12).
pub const MEMBER_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// The drain status of a member that is draining with `open` executions still open.
pub fn draining(open: u64) -> DrainStatus {
    DrainStatus {
        draining: true,
        open_executions: open,
        closing: false,
    }
}

/// What a member that has exited answers: the connection is refused.
pub fn gone() -> PortError {
    PortError::refused("the operator API: connection refused")
}

/// A fake of a frontend member's own API; every call is in the [`Log`] by the host's alias.
/// Unscripted, a member reports [`MEMBER_COMMIT`], is ready, accepts a drain
/// and has exited ([`gone`]) when its drain is read.
#[derive(Clone)]
pub struct FakeFrontend {
    log: Log,
    commits: Arc<Script<Result<String, PortError>>>,
    drains: Arc<Script<Result<(), PortError>>>,
    drain_statuses: Arc<Script<Result<DrainStatus, PortError>>>,
    readies: Arc<Script<Result<bool, PortError>>>,
}

impl FakeFrontend {
    pub fn new(log: &Log) -> FakeFrontend {
        FakeFrontend {
            log: log.clone(),
            commits: Arc::new(Script::new(Ok(MEMBER_COMMIT.to_owned()))),
            drains: Arc::new(Script::new(Ok(()))),
            drain_statuses: Arc::new(Script::new(Err(gone()))),
            readies: Arc::new(Script::new(Ok(true))),
        }
    }

    /// The answers of a member's own drain (`POST`): `first` in order, then `then`.
    pub fn script_member_drain(
        &self,
        alias: &str,
        first: Vec<Result<(), PortError>>,
        then: Result<(), PortError>,
    ) {
        self.drains.set(alias, first, then);
    }

    /// The answers of a member's drain status: `first` in order, then `then`.
    pub fn script_drain_status(
        &self,
        alias: &str,
        first: Vec<Result<DrainStatus, PortError>>,
        then: Result<DrainStatus, PortError>,
    ) {
        self.drain_statuses.set(alias, first, then);
    }

    /// The answers of a member's `/readyz`: `first` in order, then `then`.
    pub fn script_ready(
        &self,
        alias: &str,
        first: Vec<Result<bool, PortError>>,
        then: Result<bool, PortError>,
    ) {
        self.readies.set(alias, first, then);
    }

    pub fn script_commit(
        &self,
        alias: &str,
        first: Vec<Result<String, PortError>>,
        then: Result<String, PortError>,
    ) {
        self.commits.set(alias, first, then);
    }
}

impl FrontendControl for FakeFrontend {
    fn commit(&self, host: &Host) -> Result<String, PortError> {
        self.log.push(Event::Commit(host.alias.clone()));
        self.commits.next(&host.alias)
    }

    fn drain(&self, host: &Host) -> Result<(), PortError> {
        self.log.push(Event::MemberDrain(host.alias.clone()));
        self.drains.next(&host.alias)
    }

    fn drain_status(&self, host: &Host) -> Result<DrainStatus, PortError> {
        self.log.push(Event::MemberDrainStatus(host.alias.clone()));
        self.drain_statuses.next(&host.alias)
    }

    fn ready(&self, host: &Host) -> Result<bool, PortError> {
        self.log.push(Event::Ready(host.alias.clone()));
        self.readies.next(&host.alias)
    }
}

#[derive(Clone)]
pub struct FakeHealth {
    log: Log,
    script: Arc<Script<Result<Health, PortError>>>,
    /// Answers for a host the roll holds drained; unscripted hosts answer as `script` does.
    held: Arc<Script<Option<Result<Health, PortError>>>>,
}

impl FakeHealth {
    pub fn new(log: &Log) -> FakeHealth {
        FakeHealth {
            log: log.clone(),
            script: Arc::new(Script::new(Ok(Health::Healthy))),
            held: Arc::new(Script::new(None)),
        }
    }

    pub fn script(
        &self,
        alias: &str,
        first: Vec<Result<Health, PortError>>,
        then: Result<Health, PortError>,
    ) {
        self.script.set(alias, first, then);
    }

    /// The answers of `check_held` for a host: `first` in order, then `then`.
    pub fn script_held(
        &self,
        alias: &str,
        first: Vec<Result<Health, PortError>>,
        then: Result<Health, PortError>,
    ) {
        self.held
            .set(alias, first.into_iter().map(Some).collect(), Some(then));
    }
}

impl HealthCheck for FakeHealth {
    fn refusal(&self, _host: &Host) -> Option<String> {
        None
    }

    fn check(&self, host: &Host, _target: &Version) -> Result<Health, PortError> {
        self.log.push(Event::Health(host.alias.clone()));
        self.script.next(&host.alias)
    }

    fn check_held(&self, host: &Host, _target: &Version) -> Result<Health, PortError> {
        self.log.push(Event::Health(host.alias.clone()));
        self.held
            .next(&host.alias)
            .unwrap_or_else(|| self.script.next(&host.alias))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::Role;

    fn host(alias: &str) -> Host {
        Host {
            alias: alias.to_owned(),
            address: "h.example".to_owned(),
            role: Role::Storage,
            host_key: "ssh-ed25519 AAAA".to_owned(),
            node_id: None,
            member: None,
        }
    }

    /// Catches fakes that do not behave as their scripts say, which would make every roll test
    /// below prove nothing: answers come in order and then repeat the last, unscripted hosts
    /// get the happy path, a successful install shows in the next query, a failed one does
    /// not, a scripted query overrides the installed state once, and the log keeps order.
    #[test]
    fn the_fakes_follow_their_scripts_and_log_in_order() {
        let log = Log::default();
        let (a, b) = (host("a"), host("b"));
        let v = Version::parse("2.0").expect("v");

        let stop = FakeSafeToStop::new(&log);
        stop.script(
            "a",
            vec![Ok(unsafe_because("m2 behind"))],
            Err(PortError("down".into())),
        );
        assert_eq!(stop.check(&a), Ok(unsafe_because("m2 behind")));
        assert_eq!(stop.check(&a), Err(PortError("down".into())));
        assert_eq!(stop.check(&a), Err(PortError("down".into())));
        assert_eq!(stop.check(&b), Ok(safe()));

        let inst = FakeInstaller::new(&log);
        assert_eq!(inst.query(&a), Ok(Installed::Version("0.0.0".into())));
        inst.script_install("b", vec![Err(PortError("lock".into()))], Ok(()));
        assert_eq!(inst.install(&b, &v), Err(PortError("lock".into())));
        assert_eq!(inst.installed("b"), Installed::Version("0.0.0".into()));
        assert_eq!(inst.install(&b, &v), Ok(InstallAnswer::Installed));
        assert_eq!(inst.query(&b), Ok(Installed::Version("2.0".into())));
        inst.script_query("b", vec![Ok(Installed::InProgress)]);
        assert_eq!(inst.query(&b), Ok(Installed::InProgress));
        assert_eq!(inst.query(&b), Ok(Installed::Version("2.0".into())));
        inst.set_installed("a", Installed::InProgress);
        assert_eq!(inst.installed("a"), Installed::InProgress);

        let health = FakeHealth::new(&log);
        health.script(
            "a",
            vec![Ok(Health::Waiting("swap".into()))],
            Ok(Health::Healthy),
        );
        assert_eq!(health.check(&a, &v), Ok(Health::Waiting("swap".into())));
        assert_eq!(health.check(&a, &v), Ok(Health::Healthy));
        assert_eq!(health.check(&b, &v), Ok(Health::Healthy));

        assert_eq!(log.installs(), ["b", "b"]);
        assert_eq!(log.touching("b").len(), 7);
        assert_eq!(log.events()[0], Event::Check("a".into()));
        log.clear();
        assert!(log.events().is_empty());
    }

    /// Catches a hook that is not called before the install result (the point where a test
    /// looks at the record on disk).
    #[test]
    fn the_install_hook_runs_before_the_install_is_recorded_as_done() {
        let log = Log::default();
        let inst = FakeInstaller::new(&log);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let probe = inst.clone();
        inst.on_install(move |h| {
            lock(&sink).push(probe.installed(&h.alias));
        });
        let v = Version::parse("2.0").expect("v");
        inst.install(&host("a"), &v).expect("ok");
        assert_eq!(*lock(&seen), [Installed::Version("0.0.0".into())]);
    }

    /// Catches a clock that does not advance on sleep: every timeout test would spin.
    #[test]
    fn the_fake_clock_moves_by_sleeping() {
        let clock = FakeClock::new();
        clock.sleep(Duration::from_secs(5));
        clock.sleep(Duration::from_secs(5));
        assert_eq!(clock.now(), Duration::from_secs(10));
    }

    /// Asserts that the fake frontend reports the commit of a member unless it is scripted
    /// otherwise, per host, and that each call is logged against the host.
    #[test]
    fn the_fake_frontend_reports_the_commit_as_scripted_and_logs_by_host() {
        let log = Log::default();
        let front = FakeFrontend::new(&log);
        let (a, b) = (host("a"), host("b"));
        assert_eq!(front.commit(&a), Ok(MEMBER_COMMIT.to_owned()));
        let no = PortError("no".into());
        front.script_commit("b", vec![Err(no.clone())], Ok("abc".to_owned()));
        assert_eq!(front.commit(&b), Err(no));
        assert_eq!(front.commit(&b), Ok("abc".to_owned()));
        assert_eq!(log.touching("a"), [Event::Commit("a".into())]);
        assert_eq!(log.touching("b").len(), 2);
    }

    /// Asserts that the fake installer restarts a frontend unit and logs the host and the unit,
    /// answers as scripted, and that two aliases of one host share what is installed on it.
    #[test]
    fn the_fake_installer_restarts_frontends_and_shares_a_host_between_aliases() {
        let log = Log::default();
        let inst = FakeInstaller::new(&log);
        let (a, b) = (host("a"), host("b"));
        assert_eq!(inst.restart_frontend(&a, "x"), Ok(()));
        let no = PortError("no".into());
        inst.script_restart("b", vec![Err(no.clone())], Ok(()));
        assert_eq!(inst.restart_frontend(&b, "y"), Err(no));
        assert_eq!(inst.restart_frontend(&b, "y"), Ok(()));
        let restarts = [
            Event::Restart("a".into(), "x".into()),
            Event::Restart("b".into(), "y".into()),
            Event::Restart("b".into(), "y".into()),
        ];
        assert_eq!(log.events(), restarts);
        inst.same_host("b", "a");
        let v = Version::parse("2.0").expect("v");
        inst.install(&a, &v).expect("install");
        assert_eq!(inst.installed("b"), Installed::Version("2.0".into()));
        assert_eq!(inst.query(&b), Ok(Installed::Version("2.0".into())));
    }

    /// Asserts that every kind of call has its own word, with the detail the tests tell calls
    /// apart by: the version of an install, the deadline of a drain or the unit.
    #[test]
    fn every_call_has_its_own_word() {
        let a = || "a".to_owned();
        let words: Vec<String> = [
            Event::Check(a()),
            Event::Install(a(), "2.0".into()),
            Event::Query(a()),
            Event::Health(a()),
            Event::Drain(a(), Some(600)),
            Event::Leases(a()),
            Event::Undrain(a()),
            Event::Commit(a()),
            Event::Restart(a(), "u".into()),
            Event::MemberDrain(a()),
            Event::MemberDrainStatus(a()),
            Event::Ready(a()),
        ]
        .iter()
        .map(Event::word)
        .collect();
        assert_eq!(
            words,
            [
                "check",
                "install 2.0",
                "query",
                "health",
                "drain 600",
                "leases",
                "undrain",
                "commit",
                "restart u",
                "member drain",
                "member drain status",
                "ready",
            ]
        );
    }

    /// Asserts that the fake frontend accepts a drain, reports that the member has exited when
    /// its drain is read, and reports the member ready, unless it is scripted otherwise per host,
    /// and that each of the three calls is logged against the host.
    #[test]
    fn the_fake_frontend_drains_and_reports_readiness_as_scripted() {
        let log = Log::default();
        let front = FakeFrontend::new(&log);
        let (a, b) = (host("a"), host("b"));
        assert_eq!(front.drain(&a), Ok(()));
        assert_eq!(front.drain_status(&a), Err(gone()));
        assert!(gone().is_refused());
        assert_eq!(front.ready(&a), Ok(true));
        let no = PortError("no".into());
        front.script_member_drain("b", vec![Err(no.clone())], Ok(()));
        front.script_drain_status("b", vec![Ok(draining(2))], Ok(draining(0)));
        front.script_ready("b", vec![Ok(false), Err(no.clone())], Ok(true));
        assert_eq!(front.drain(&b), Err(no.clone()));
        assert_eq!(front.drain(&b), Ok(()));
        assert_eq!(front.drain_status(&b), Ok(draining(2)));
        assert_eq!(front.drain_status(&b), Ok(draining(0)));
        assert_eq!(front.ready(&b), Ok(false));
        assert_eq!(front.ready(&b), Err(no));
        assert_eq!(front.ready(&b), Ok(true));
        let kinds = [
            Event::MemberDrain("a".into()),
            Event::MemberDrainStatus("a".into()),
            Event::Ready("a".into()),
        ];
        assert_eq!(log.touching("a"), kinds);
        assert_eq!(log.touching("b").len(), 7);
    }
}
