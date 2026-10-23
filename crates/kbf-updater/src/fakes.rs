//! One scripted fake per port, for this crate's tests and for the adapters' tests to come.
//!
//! Every fake is a cheap handle (clones share state) and records what it was asked in one
//! shared [`Log`], so a test can say not only that host B was installed but that it was
//! installed after host A was healthy, and that host C was never asked about at all. Unscripted
//! answers are the happy path: safe to stop, install succeeds, healthy at once.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::inventory::Host;
use crate::ports::{
    Clock, Health, HealthCheck, Installed, Installer, PortError, SafeToStop, Stop, Verdict,
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
                Event::Check(a) | Event::Install(a, _) | Event::Query(a) | Event::Health(a) => {
                    a == alias
                }
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

type Hook = Arc<dyn Fn(&Host) + Send + Sync>;

#[derive(Clone)]
pub struct FakeInstaller {
    log: Log,
    installed: Arc<Mutex<BTreeMap<String, Installed>>>,
    installs: Arc<Script<Result<(), PortError>>>,
    queries: Arc<Script<Option<Result<Installed, PortError>>>>,
    hook: Arc<Mutex<Option<Hook>>>,
}

impl FakeInstaller {
    pub fn new(log: &Log) -> FakeInstaller {
        FakeInstaller {
            log: log.clone(),
            installed: Arc::default(),
            installs: Arc::new(Script::new(Ok(()))),
            queries: Arc::new(Script::new(None)),
            hook: Arc::default(),
        }
    }

    /// What the host has installed before any install (unscripted hosts have `0.0.0`).
    pub fn set_installed(&self, alias: &str, what: Installed) {
        lock(&self.installed).insert(alias.to_owned(), what);
    }

    pub fn installed(&self, alias: &str) -> Installed {
        lock(&self.installed)
            .get(alias)
            .cloned()
            .unwrap_or_else(|| Installed::Version("0.0.0".to_owned()))
    }

    /// Answers of `install` for a host: `first` in order, then `then`. A successful install
    /// makes the host report the version.
    pub fn script_install(
        &self,
        alias: &str,
        first: Vec<Result<(), PortError>>,
        then: Result<(), PortError>,
    ) {
        self.installs.set(alias, first, then);
    }

    /// Answers of `query` for a host: `first` in order, then what the host has installed.
    pub fn script_query(&self, alias: &str, first: Vec<Result<Installed, PortError>>) {
        self.queries
            .set(alias, first.into_iter().map(Some).collect(), None);
    }

    /// Call `f` at the start of every install (a test reads the record from it).
    pub fn on_install(&self, f: impl Fn(&Host) + Send + Sync + 'static) {
        *lock(&self.hook) = Some(Arc::new(f));
    }
}

impl Installer for FakeInstaller {
    fn install(&self, host: &Host, version: &Version) -> Result<(), PortError> {
        self.log
            .push(Event::Install(host.alias.clone(), version.to_string()));
        // Cloned out, so a hook that re-enters the installer does not deadlock on the slot.
        let hook = lock(&self.hook).clone();
        if let Some(hook) = hook {
            hook(host);
        }
        let result = self.installs.next(&host.alias);
        if result.is_ok() {
            self.set_installed(&host.alias, Installed::Version(version.to_string()));
        }
        result
    }

    fn query(&self, host: &Host) -> Result<Installed, PortError> {
        self.log.push(Event::Query(host.alias.clone()));
        self.queries
            .next(&host.alias)
            .unwrap_or_else(|| Ok(self.installed(&host.alias)))
    }
}

#[derive(Clone)]
pub struct FakeHealth {
    log: Log,
    script: Arc<Script<Result<Health, PortError>>>,
    /// The aliases the roll said it was about to install, in order (not in the [`Log`]: the
    /// call sequences the roll tests pin do not change).
    installing: Arc<Mutex<Vec<String>>>,
}

impl FakeHealth {
    pub fn new(log: &Log) -> FakeHealth {
        FakeHealth {
            log: log.clone(),
            script: Arc::new(Script::new(Ok(Health::Healthy))),
            installing: Arc::default(),
        }
    }

    /// The aliases the roll announced an install for, in order.
    pub fn installing_calls(&self) -> Vec<String> {
        lock(&self.installing).clone()
    }

    pub fn script(
        &self,
        alias: &str,
        first: Vec<Result<Health, PortError>>,
        then: Result<Health, PortError>,
    ) {
        self.script.set(alias, first, then);
    }
}

impl HealthCheck for FakeHealth {
    fn installing(&self, host: &Host, _target: &Version) {
        lock(&self.installing).push(host.alias.clone());
    }

    fn check(&self, host: &Host, _target: &Version) -> Result<Health, PortError> {
        self.log.push(Event::Health(host.alias.clone()));
        self.script.next(&host.alias)
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
        assert_eq!(inst.install(&b, &v), Ok(()));
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
}
