//! `kbf-updater-hook-fake`: `kbf update-hook` over a fake host, the program the conformance
//! harness runs (tests/hook_conformance.rs). Not shipped, and no part of `kbf update-hook`:
//! the binary (`src/bin/kbf-updater-hook-fake.rs`) forwards to [`main`] here, so that this code
//! is measured and tested with the library.
//!
//! ```text
//! kbf-updater-hook-fake [update-hook] --world DIR --faults DIR <the flags of kbf update-hook>
//!                       <step> [<artifact>] [--params FILE]
//! ```
//!
//! It is the hook's own code (`kbf_updater::agent::hook::run`: the flags, the params, the
//! record, the five steps) with its ports played by a host kept in a directory instead of the
//! helper's socket and the frontend's API: the same traits the agent's fakes implement, but
//! with their state in files, so that a step can be killed and run again by another process.
//! Every setting is a flag, given by the conformance test as the declaration's `hook_args`;
//! none is read from the environment (the harness's own kill-point variables aside, which
//! [`gate::point`] reads). `--world` and `--faults` are this program's and come first; the
//! rest is handed to the hook as it is. Every change to the host or to the record passes a
//! kill point first.
//!
//! The host (`--world`): `helper.json`, what the helper runs and has on trial, `node`,
//! `serving` or `drained`, and `mark`, the cordon mark of this update's drain. Faults come from
//! `--faults/<step>` (`fail`, `defer`) and take effect where the step makes the call that can
//! fail: the install, the switch, the health, the revert, the confirm; a deferral is a lease
//! that never goes.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use update_hook::conformance::gate;
use update_hook::{Class, Outcome};

use super::flags::Flags;
use super::steps::Done;
use super::{HookWorld, Setup, run};
use crate::agent::local::{HostState, Local, LocalError, Staged, Trial};
use crate::inventory::{Host, Role};
use crate::ports::{Clock, Drain, Health, HealthCheck, Placed, PortError};
use crate::version::Version;

/// The commit the helper reports for a release.
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// The deadline the helper gives a trial.
pub(super) const DEADLINE: u64 = 10_600;

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

pub(super) struct Paths {
    pub(super) world: PathBuf,
    pub(super) faults: PathBuf,
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

pub(super) struct FakeLocal(pub(super) Arc<Paths>);

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

pub(super) struct FakeDrain(pub(super) Arc<Paths>);

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

pub(super) struct FakeHealth(pub(super) Arc<Paths>);

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
pub(super) struct FakeClock(Mutex<Duration>);

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

/// Why this program's own flags were refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(super) enum OwnError {
    #[error("{flag} is required")]
    Required { flag: &'static str },
    /// No value, or another flag where the value should be.
    #[error("{flag} needs a value")]
    NoValue { flag: &'static str },
    /// An empty value, which would name the working directory.
    #[error("{flag} must name a directory, not be empty")]
    Empty { flag: &'static str },
    #[error("{flag} is given twice")]
    Twice { flag: &'static str },
}

/// This program's own flags, the first words of its command line.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Own {
    /// The fake host's directory.
    pub(super) world: PathBuf,
    /// The directory of the faults, one file per step.
    pub(super) faults: PathBuf,
}

/// `--world` and `--faults`, each once, from the front of `args`; the rest is the hook's.
pub(super) fn own_flags(args: &[String]) -> Result<(Own, &[String]), OwnError> {
    let (mut world, mut faults) = (None, None);
    let mut at = 0;
    while let Some(word) = args.get(at) {
        let (flag, slot) = match word.as_str() {
            "--world" => ("--world", &mut world),
            "--faults" => ("--faults", &mut faults),
            _ => break,
        };
        let value = args
            .get(at + 1)
            .filter(|value| !value.starts_with("--"))
            .ok_or(OwnError::NoValue { flag })?;
        if value.is_empty() {
            return Err(OwnError::Empty { flag });
        }
        if slot.replace(PathBuf::from(value)).is_some() {
            return Err(OwnError::Twice { flag });
        }
        at += 2;
    }
    let world = world.ok_or(OwnError::Required { flag: "--world" })?;
    let faults = faults.ok_or(OwnError::Required { flag: "--faults" })?;
    Ok((Own { world, faults }, &args[at..]))
}

/// The host this program plays: node `n1`, a daemon's.
pub(super) fn host() -> Host {
    Host {
        alias: "n1".to_owned(),
        address: "localhost".to_owned(),
        role: Role::Daemon,
        host_key: String::new(),
        node_id: Some("n1".to_owned()),
        member: None,
    }
}

/// One run: `args` are the words after the program's name.
pub(super) fn fake_run(
    args: &[String],
    out: &mut dyn io::Write,
    err: &mut dyn io::Write,
) -> Outcome {
    // The harness runs `<this program> update-hook <flags> <step> ...`.
    let args = args
        .strip_prefix(&["update-hook".to_owned()])
        .unwrap_or(args);
    let (own, rest) = match own_flags(args) {
        Ok(split) => split,
        Err(why) => {
            let done = Done::failed("bad_arguments", Class::Release, why);
            let _ = done.report.write_to(out);
            return done.outcome;
        }
    };
    let paths = Arc::new(Paths {
        world: own.world,
        faults: own.faults,
    });
    let world = |_: &Flags| {
        Ok(HookWorld {
            host: host(),
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
    run(rest, &setup, out, err)
}

/// The program: `args` are the words after its name. The last line goes to standard output,
/// the log to standard error, and what comes back is the exit code.
#[must_use]
pub fn main(args: &[OsString]) -> ExitCode {
    let args: Vec<String> = args
        .iter()
        .map(|word| word.to_string_lossy().into_owned())
        .collect();
    let mut out = io::stdout().lock();
    let outcome = fake_run(&args, &mut out, &mut io::stderr().lock());
    let _ = out.flush();
    outcome.into()
}
