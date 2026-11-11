//! `kbf package-hook` against the conformance harness of the hook contract
//! (`update_hook::conformance`): the real params, record and steps of the hook, run as a process
//! the way hostkeeper runs a hook, over a fake host kept in a directory
//! (`src/bin/kbf-package-hook-fake.rs`).
//!
//! The harness runs every step twice, kills the hook at every write it makes and runs it again,
//! drives the updater's table through a failure and a deferral at each step it can, and runs the
//! update as a first install; it refuses a hook that breaks the contract by name.
//!
//! The fake host is found by flags alone, given as the declaration's `hook_args`; its
//! `FakeService::env` adds nothing.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use kbf_testkit::dir::TestDir;
use update_hook::conformance::{Conformance, FakeService, Fault, HookCommand, Sandbox, check};
use update_hook::{Params, Scalar, Step};

/// The fake host of the hook: the unit is stopped (or running, for an update of a running node)
/// and runs `1.0.0`; `2.0.0` is on offer; a fault is a file.
struct FakeHost {
    running: bool,
}

impl FakeService for FakeHost {
    fn init(&mut self, sandbox: &Sandbox) -> io::Result<()> {
        let unit = if self.running { "active" } else { "inactive" };
        fs::write(sandbox.world().join("unit"), unit)?;
        fs::write(sandbox.world().join("version"), "1.0.0")?;
        fs::create_dir_all(sandbox.root().join("faults"))
    }

    /// Nothing: every setting of the fake host is a flag.
    fn env(&self, _sandbox: &Sandbox) -> Vec<(OsString, OsString)> {
        Vec::new()
    }

    /// The manifest's `[params]`; the paths are relative to the sandbox's root, where the
    /// harness starts every step.
    fn params(&self) -> Params {
        let mut params = Params::default();
        for (key, value) in [
            ("unit", "kbf-fake.service"),
            ("mode", "storage-and-daemon"),
            ("state_dir", "record"),
        ] {
            params.insert(key, Scalar::Text(value.to_owned()));
        }
        params
    }

    /// A failed check at `prepare`, a unit that does not start at `cutover`, a unit that fails
    /// or stays activating at `verify`. The hook has no halt, no deferral of its other steps,
    /// and nothing at `cleanup` or `rollback` the host can be made to fail.
    fn inject(&mut self, sandbox: &Sandbox, step: Step, fault: Fault) -> io::Result<bool> {
        let (name, word) = match (step, fault) {
            (Step::Prepare, Fault::Fail) => ("check", "refuse"),
            (Step::Cutover, Fault::Fail) => ("start", "fail"),
            (Step::Verify, Fault::Fail) => ("verify", "failed"),
            (Step::Verify, Fault::Defer) => ("verify", "activating"),
            _ => return Ok(false),
        };
        fs::write(sandbox.root().join("faults").join(name), word)?;
        Ok(true)
    }

    fn residue(&self, _sandbox: &Sandbox) -> io::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

fn hook() -> HookCommand {
    HookCommand::new(Path::new(env!("CARGO_BIN_EXE_kbf-package-hook-fake"))).with_hook_args(&[
        "package-hook",
        "--world",
        "world",
        "--faults",
        "faults",
    ])
}

fn base(name: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let base = dir.join("runs");
    (dir, base)
}

/// The fake service adds nothing to the hook's environment: the fake host is found by the flags
/// of the hook command alone, and they name the sandbox's own directories.
#[test]
fn the_fake_host_is_configured_by_flags_alone() {
    let (dir, _) = base("package-hook-conformance-flags");
    let sandbox = Sandbox::create(dir.join("run")).expect("a sandbox");
    assert!(FakeHost { running: false }.env(&sandbox).is_empty());
    let args: Vec<String> = hook()
        .hook_args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args[0], "package-hook");
    let value = |flag: &str| {
        let at = args.iter().position(|a| a == flag).expect(flag);
        sandbox.root().join(&args[at + 1])
    };
    assert_eq!(value("--world"), sandbox.world());
    assert_eq!(value("--faults"), sandbox.root().join("faults"));
}

/// `kbf package-hook` keeps the contract on a host whose unit is stopped: every step is safe to
/// run twice, a kill at any write of the record or of the host is repaired by running on, the
/// updater's table ends as injected at each step, and the same holds as a first install.
#[test]
fn the_package_hook_keeps_the_contract() {
    let (_dir, base) = base("package-hook-conformance-stopped");
    let summary =
        check(&hook(), &mut FakeHost { running: false }, &base).unwrap_or_else(|e| panic!("{e}"));
    assert!(summary.kill_points > 5, "{summary:?}");
    assert_eq!(
        summary.scenarios,
        [
            "none",
            "prepare=Fail",
            "cutover=Fail",
            "verify=Fail",
            "verify=Defer"
        ]
    );
    assert!(
        summary.skipped.contains(&"cleanup=Fail".to_owned()),
        "{summary:?}"
    );
}

/// The update of a running unit: the hook stops it in `prepare`, starts the new binary in
/// `cutover`, and a rollback restarts it, so the world after a rollback is the world before.
/// Run as an update only: a first install has no running unit to give back.
#[test]
fn the_package_hook_keeps_the_contract_on_a_running_unit() {
    let (_dir, base) = base("package-hook-conformance-running");
    let mut fake = FakeHost { running: true };
    let command = hook();
    let mut harness = Conformance::new(&command, &mut fake, &base);
    harness.idempotence().unwrap_or_else(|e| panic!("{e}"));
    let points = harness.crash_resume().unwrap_or_else(|e| panic!("{e}"));
    assert!(points > 5, "{points}");
    harness.scenarios().unwrap_or_else(|e| panic!("{e}"));
    harness
        .tolerates_unknown_updater_keys()
        .unwrap_or_else(|e| panic!("{e}"));
    harness.exclusive_record().unwrap_or_else(|e| panic!("{e}"));
}
