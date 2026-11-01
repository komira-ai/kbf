//! `kbf-updater-hook-fake`: `kbf update-hook` over a fake host, the program the conformance
//! harness runs (tests/hook_conformance.rs).
//!
//! It is the hook's own code (`kbf_updater::agent::hook::run`: the flags, the params, the
//! record, the five steps) with its ports played by a host kept in a directory instead of the
//! helper's socket and the frontend's API: the same traits the agent's fakes implement, but
//! with their state in files, so that a step can be killed and run again by another process.
//! The flags before the step come from the environment of this test program (the harness
//! gives a hook only the arguments of its declaration and an environment), and every change to
//! the host or to the record passes a kill point first.
//!
//! The host (`HOOKFAKE_WORLD`): `helper.json`, what the helper runs and has on trial, and
//! `node`, `serving` or `drained`. The leases are `leases`, if the file is there. Faults come
//! from `HOOKFAKE_FAULTS/<step>` (`fail`, `defer`) and take effect where the step makes the
//! call that can fail: the install, the switch, the health, the revert, the confirm; a
//! deferral is a lease that never goes.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use kbf_updater::agent::hook::{HookWorld, Setup, run};
use kbf_updater::agent::local::{HostState, Local, LocalError, Staged, Trial};
use kbf_updater::inventory::{Host, Role};
use kbf_updater::ports::{Clock, Drain, Health, HealthCheck, Placed, PortError};
use kbf_updater::version::Version;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use update_hook::conformance::gate;

/// The commit the helper reports for a release.
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// The deadline the helper gives a trial.
const DEADLINE: u64 = 10_600;

/// Unix seconds, always.
const NOW: u64 = 10_000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct HelperFile {
    /// What runs.
    version: String,
    /// What ran before the release on trial.
    before: Option<String>,
    /// A trial is on.
    pending: bool,
}

struct Paths {
    world: PathBuf,
    faults: PathBuf,
}

impl Paths {
    fn helper(&self) -> HelperFile {
        fs::read_to_string(self.world.join("helper.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// A kill point, then the file replaced whole.
    fn put(&self, name: &str, text: &str) -> io::Result<()> {
        gate::point();
        let tmp = self.world.join(format!("{name}.tmp"));
        fs::write(&tmp, text)?;
        fs::rename(&tmp, self.world.join(name))
    }

    fn put_helper(&self, file: &HelperFile) -> io::Result<()> {
        let text = serde_json::to_string(file).map_err(io::Error::other)?;
        self.put("helper.json", &text)
    }

    fn fault(&self, step: &str) -> Option<String> {
        fs::read_to_string(self.faults.join(step)).ok()
    }

    /// A kill point, then the file removed if it is there.
    fn remove(&self, name: &str) -> io::Result<()> {
        gate::point();
        match fs::remove_file(self.world.join(name)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// The mark of a cordon this update's drain made, as the agent's cordon marks are.
    fn marked(&self) -> bool {
        self.world.join("mark").exists()
    }

    fn drained(&self) -> bool {
        fs::read_to_string(self.world.join("node")).is_ok_and(|text| text.trim() == "drained")
    }
}

fn failed(what: &str, e: io::Error) -> LocalError {
    LocalError::Failed(format!("{what}: {e}"))
}

struct FakeLocal(Arc<Paths>);

impl Local for FakeLocal {
    fn query(&self) -> Result<HostState, LocalError> {
        let file = self.0.helper();
        let version = Version::parse(&file.version).ok();
        Ok(HostState {
            commit: None,
            version: version.clone(),
            node_id: None,
            pending: file.pending.then(|| Trial {
                version,
                commit: COMMIT.to_owned(),
                deadline_unix: Some(DEADLINE),
            }),
            last: None,
            busy: false,
        })
    }

    fn install(&self, version: &Version, _channel: &str) -> Result<Staged, LocalError> {
        if self.0.fault("prepare").as_deref() == Some("fail") {
            return Err(LocalError::Refused("verify_failed".to_owned()));
        }
        Ok(Staged {
            commit: COMMIT.to_owned(),
            current: self.0.helper().version == version.as_str(),
        })
    }

    fn switch(&self, version: &Version) -> Result<u64, LocalError> {
        if self.0.fault("cutover").as_deref() == Some("fail") {
            return Err(LocalError::Failed("switch_failed".to_owned()));
        }
        let was = self.0.helper();
        self.0
            .put_helper(&HelperFile {
                version: version.to_string(),
                before: Some(was.version),
                pending: true,
            })
            .map_err(|e| failed("switch", e))?;
        Ok(DEADLINE)
    }

    fn confirm(&self, _version: &Version) -> Result<(), LocalError> {
        if self.0.fault("cleanup").as_deref() == Some("fail") {
            return Err(LocalError::Failed("state_unavailable".to_owned()));
        }
        let mut file = self.0.helper();
        file.pending = false;
        self.0.put_helper(&file).map_err(|e| failed("confirm", e))
    }

    fn revert(&self) -> Result<(), LocalError> {
        if self.0.fault("rollback").as_deref() == Some("fail") {
            return Err(LocalError::Failed("revert_failed".to_owned()));
        }
        let file = self.0.helper();
        let Some(before) = file.before.filter(|_| file.pending) else {
            return Err(LocalError::Refused("nothing_pending".to_owned()));
        };
        self.0
            .put_helper(&HelperFile {
                version: before,
                before: None,
                pending: false,
            })
            .map_err(|e| failed("revert", e))
    }
}

struct FakeDrain(Arc<Paths>);

impl Drain for FakeDrain {
    fn placed(&self, _host: &Host) -> Result<Placed, PortError> {
        // A node with our mark reads as in placement (the cordon is ours); an unmarked node
        // that is drained was drained by an operator.
        Ok(if self.0.drained() && !self.0.marked() {
            Placed::Out
        } else {
            Placed::In
        })
    }

    /// The mark goes first, and only on a node that serves: an operator's drain gets none.
    fn drain(&self, _host: &Host, _deadline: Option<Duration>) -> Result<(), PortError> {
        let other = |e: io::Error| PortError::other(e.to_string());
        if !self.0.drained() {
            self.0.put("mark", "ours").map_err(other)?;
        }
        self.0.put("node", "drained").map_err(other)
    }

    fn leases(&self, _host: &Host) -> Result<usize, PortError> {
        Ok(usize::from(
            self.0.fault("prepare").as_deref() == Some("defer"),
        ))
    }

    fn undrain(&self, _host: &Host) -> Result<(), PortError> {
        let other = |e: io::Error| PortError::other(e.to_string());
        self.0.put("node", "serving").map_err(other)?;
        self.0.remove("mark").map_err(other)
    }
}

struct FakeHealth(Arc<Paths>);

impl HealthCheck for FakeHealth {
    fn refusal(&self, _host: &Host) -> Option<String> {
        None
    }

    fn check(&self, host: &Host, target: &Version) -> Result<Health, PortError> {
        self.check_held(host, target)
    }

    fn check_held(&self, _host: &Host, target: &Version) -> Result<Health, PortError> {
        let file = self.0.helper();
        let running = file.version;
        // The fault is the release on trial's: the old release is healthy all the same.
        Ok(if running != target.as_str() {
            Health::Waiting(format!("{running} runs, not {target}"))
        } else if file.pending && self.0.fault("verify").as_deref() == Some("fail") {
            Health::Abandoned("the fault says so".to_owned())
        } else {
            Health::Healthy
        })
    }
}

/// A clock that only moves when it sleeps.
#[derive(Default)]
struct FakeClock(Mutex<Duration>);

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn sleep(&self, d: Duration) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += d;
    }
}

fn var(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_default())
}

fn main() -> ExitCode {
    let paths = Arc::new(Paths {
        world: var("HOOKFAKE_WORLD"),
        faults: var("HOOKFAKE_FAULTS"),
    });
    // The harness runs `<this program> update-hook <step> ...`; the flags go between.
    let mut rest: Vec<OsString> = std::env::args_os().skip(1).collect();
    if rest.first().is_some_and(|word| word == "update-hook") {
        rest.remove(0);
    }
    let mut args: Vec<String> = [
        "--record-dir",
        &var("HOOKFAKE_RECORD").to_string_lossy(),
        "--channel",
        "stable",
        "--client-cert",
        "/unused/cert.pem",
        "--client-key",
        "/unused/key.pem",
        "--ca",
        "/unused/ca.pem",
        "--drain-deadline-secs",
        "30",
        "--retry-secs",
        "60",
    ]
    .map(str::to_owned)
    .to_vec();
    args.extend(rest.into_iter().map(|a| a.to_string_lossy().into_owned()));
    let world = |_: &kbf_updater::agent::hook::flags::Flags| {
        Ok(HookWorld {
            host: Host {
                alias: "n1".to_owned(),
                address: "localhost".to_owned(),
                role: Role::Daemon,
                host_key: String::new(),
                node_id: Some("n1".to_owned()),
                member: None,
            },
            local: Arc::new(FakeLocal(paths.clone())),
            drain: Arc::new(FakeDrain(paths.clone())),
            health: Arc::new(FakeHealth(paths.clone())),
            clock: Arc::new(FakeClock::default()),
            unix_now: Arc::new(|| NOW),
        })
    };
    let setup = Setup {
        world: &world,
        before_write: Some(Arc::new(gate::point)),
    };
    let outcome = run(
        &args,
        &setup,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    );
    let _ = io::stdout().flush();
    outcome.into()
}
