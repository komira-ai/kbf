//! `kbf-package-hook-fake`: `kbf package-hook` over a fake host kept in a directory, the program
//! the conformance harness runs (tests/conformance.rs). Not shipped: the binary
//! (`src/bin/kbf-package-hook-fake.rs`) forwards to [`main`] here, so that this code is measured
//! and tested with the library.
//!
//! ```text
//! kbf-package-hook-fake package-hook --world DIR --faults DIR <step> [<staged>] --params FILE
//! kbf-package-hook-fake is-active|stop|start|restart <world DIR>     (systemctl, over the world;
//!                                                                     faults in <world>/faults)
//! kbf-package-hook-fake --version --json                             (kbf)
//! kbf-package-hook-fake --mode=<ready|not-ready|refused|crash|other> --check   (the staged kbf)
//! ```
//!
//! The same program answers as `systemctl`, as the installed `kbf` and as the staged `kbf`, so a
//! test can run the real host ([`crate::Systemd`]) against it with no shell and no host unit.
//!
//! The world (`--world`): `unit`, the state of the unit (`inactive` when there is no file), and
//! `version`, the native version of the installed package. Starting the unit installs the update
//! (the target of the params file: the package manager ran between `prepare` and `cutover`);
//! stopping or restarting it puts back the version the world held before (the package manager's
//! downgrade, which has run before a rollback). Faults come from `--faults/<name>`: `check`
//! (`refuse`, `not-ready`), `start` (`fail`), `verify` (the state a started unit shows:
//! `failed`, `activating`). Every change to the world or to the record passes a kill point first
//! ([`update_hook::conformance::gate::point`]).

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use update_hook::conformance::gate;
use update_hook::{Outcome, Params};

use crate::host::{Check, Host, HostError, UnitState};
use crate::run::{Settings, run};

/// What the fake installed `kbf` reports (`--version --json`).
pub const FAKE_VERSION_LINE: &str = r#"{"seq":7,"version":"0.1.0"}"#;

/// The host kept in a directory.
pub struct FakeHost {
    world: PathBuf,
    faults: PathBuf,
    target: Option<String>,
    before: Option<String>,
}

fn io_error(what: &str, e: io::Error) -> HostError {
    HostError(format!("{what}: {e}"))
}

impl FakeHost {
    /// A host over `world`, with `faults`, the version a start installs (`target`) and the
    /// version the world held before the update (`before`), which a stop or a restart puts back:
    /// the package manager has downgraded or removed the package before a rollback.
    pub fn new(
        world: PathBuf,
        faults: PathBuf,
        target: Option<String>,
        before: Option<String>,
    ) -> FakeHost {
        FakeHost {
            world,
            faults,
            target,
            before,
        }
    }

    fn put_before(&self) -> Result<(), HostError> {
        match &self.before {
            Some(before) => self.put("version", before),
            None => Ok(()),
        }
    }

    fn fault(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.faults.join(name))
            .ok()
            .map(|text| text.trim().to_owned())
    }

    /// A kill point, then the file replaced whole.
    fn put(&self, name: &str, text: &str) -> Result<(), HostError> {
        gate::point();
        let tmp = self.world.join(format!("{name}.tmp"));
        fs::write(&tmp, text)
            .and_then(|()| fs::rename(&tmp, self.world.join(name)))
            .map_err(|e| io_error(&format!("cannot write {name} in the world"), e))
    }

    fn read(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.world.join(name))
            .ok()
            .map(|text| text.trim().to_owned())
    }
}

impl Host for FakeHost {
    fn check(&self, _kbf: &Path, _mode: &str) -> Result<Check, HostError> {
        Ok(match self.fault("check").as_deref() {
            Some("refuse") => Check::Refused,
            Some("not-ready") => Check::NotReady,
            _ => Check::Ready,
        })
    }

    fn state(&self, _unit: &str) -> Result<UnitState, HostError> {
        let state = self.read("unit").unwrap_or_else(|| "inactive".to_owned());
        // The verify fault is what the unit shows once the update has been started.
        let updated = self.target.is_some() && self.read("version") == self.target;
        Ok(match self.fault("verify") {
            Some(shown) if state == "active" && updated => UnitState::parse(&shown),
            _ => UnitState::parse(&state),
        })
    }

    fn stop(&self, _unit: &str) -> Result<(), HostError> {
        self.put_before()?;
        self.put("unit", "inactive")
    }

    fn start(&self, _unit: &str) -> Result<(), HostError> {
        if self.fault("start").is_some() {
            return Err(HostError("the unit failed to start".to_owned()));
        }
        if let Some(target) = &self.target {
            self.put("version", target)?;
        }
        self.put("unit", "active")
    }

    fn restart(&self, _unit: &str) -> Result<(), HostError> {
        self.put_before()?;
        self.put("unit", "active")
    }

    fn installed_version(&self) -> Result<String, HostError> {
        self.read("version")
            .ok_or_else(|| HostError("no version in the world".to_owned()))
    }
}

/// The value of `flag` in `args`, and `args` without the flag and its value.
fn take_flag(args: &[OsString], flag: &str) -> Option<(PathBuf, Vec<OsString>)> {
    let at = args.iter().position(|arg| arg == flag)?;
    let value = args.get(at + 1)?;
    let mut rest = args.to_vec();
    rest.drain(at..=at + 1);
    Some((PathBuf::from(value), rest))
}

fn exit(code: u8) -> ExitCode {
    ExitCode::from(code)
}

/// `systemctl <verb> <world>`.
fn systemctl(verb: &str, world: &Path) -> ExitCode {
    let host = FakeHost::new(world.to_path_buf(), world.join("faults"), None, None);
    let done = match verb {
        "is-active" => {
            let state = host.state("").unwrap_or(UnitState::Inactive);
            println!("{}", state.word());
            return exit(if state == UnitState::Active { 0 } else { 3 });
        }
        "stop" => host.stop(""),
        "start" => host.start(""),
        _ => host.restart(""),
    };
    exit(u8::from(done.is_err()))
}

/// The fake's entry: everything after the program's name.
#[must_use]
pub fn main(args: &[OsString]) -> ExitCode {
    let first = args.first().and_then(|arg| arg.to_str()).unwrap_or("");
    match first {
        "is-active" | "stop" | "start" | "restart" => {
            // The unit argument names the world: without one there is nothing to act on.
            let Some(world) = args.get(1).filter(|world| !world.is_empty()) else {
                return exit(1);
            };
            return systemctl(first, Path::new(world));
        }
        "--version" => {
            println!("{FAKE_VERSION_LINE}");
            return exit(0);
        }
        _ => {}
    }
    if let Some(mode) = first.strip_prefix("--mode=") {
        return match mode {
            "ready" => exit(0),
            "not-ready" => exit(75),
            "refused" => exit(2),
            // A staged binary that dies on a signal.
            "crash" => std::process::abort(),
            _ => exit(3),
        };
    }
    hook(args)
}

/// `package-hook --world DIR --faults DIR <step> ...`.
fn hook(args: &[OsString]) -> ExitCode {
    let rest = args.get(1..).unwrap_or_default();
    let Some((world, rest)) = take_flag(rest, "--world") else {
        return refuse("--world needs a directory");
    };
    let Some((faults, rest)) = take_flag(&rest, "--faults") else {
        return refuse("--faults needs a directory");
    };
    if world.as_os_str().is_empty() || !world.is_dir() {
        return refuse("--world must name a directory");
    }
    // The versions the update moves between are in the params file, which the hook reads too.
    let params = take_flag(&rest, "--params")
        .and_then(|(file, _)| Params::read(&file).ok())
        .unwrap_or_default();
    let host = FakeHost::new(
        world,
        faults,
        params.target_version().map(str::to_owned),
        // A first install has no previous version: the world held the version it started with.
        Some(params.previous_version().unwrap_or("1.0.0").to_owned()),
    );
    let settings = Settings {
        host: &host,
        before_write: Some(gate::point),
    };
    let outcome = run(&rest, &settings, &mut io::stdout().lock());
    ExitCode::from(u8::try_from(outcome.code()).unwrap_or(1))
}

fn refuse(why: &str) -> ExitCode {
    let done = crate::Done::failed("bad_arguments", update_hook::Class::Release, why);
    let _ = done.report.write_to(&mut io::stdout().lock());
    ExitCode::from(u8::try_from(Outcome::Failure.code()).unwrap_or(1))
}
