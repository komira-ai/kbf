//! `kbf update-hook` against the conformance harness of the hook contract
//! (`update_hook::conformance::check`): the real flags, params, record and steps of the hook,
//! run as a process the way the updater runs a hook, over a fake host kept in a directory.
//!
//! The harness runs every step twice, kills the hook at every write it makes and runs it
//! again, drives the updater's table through a failure, a deferral and a halt at each step,
//! and runs the update as a first install; it refuses a hook that breaks the contract by name.
//!
//! The fake host is configured by flags only, given as the declaration's `hook_args` (see
//! [`hook`]); the hook's environment is empty but for the harness's own kill-point variables.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use update_hook::conformance::{FakeService, Fault, HookCommand, Sandbox, check};
use update_hook::{Params, Step};

/// A directory under the target directory, removed when it is dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("hook-conformance-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create the scratch directory");
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The fake host of the hook: the release `1.0.0` runs, `2.0.0` is on offer, the node serves
/// (or an operator has drained it already), and a fault is a file.
#[derive(Default)]
struct FakeHost {
    operator_drained: bool,
}

impl FakeService for FakeHost {
    fn init(&mut self, sandbox: &Sandbox) -> io::Result<()> {
        fs::write(
            sandbox.world().join("helper.json"),
            r#"{"version":"1.0.0","before":null,"pending":false}"#,
        )?;
        let node = if self.operator_drained {
            "drained"
        } else {
            "serving"
        };
        fs::write(sandbox.world().join("node"), node)?;
        fs::create_dir_all(sandbox.root().join("faults"))
    }

    /// Nothing: every setting of the fake host is a flag ([`hook`]).
    fn env(&self, _sandbox: &Sandbox) -> Vec<(OsString, OsString)> {
        Vec::new()
    }

    fn params(&self) -> Params {
        Params::default()
    }

    /// A failure of any step, and a deferral of `prepare`, which is a lease that never goes. The hook has no other deferral and no halt: a drain that does
    /// not finish is its only busy host, and no check of it is one no version can repair.
    fn inject(&mut self, sandbox: &Sandbox, step: Step, fault: Fault) -> io::Result<bool> {
        let word = match (step, fault) {
            (_, Fault::Fail) => "fail",
            (Step::Prepare, Fault::Defer) => "defer",
            _ => return Ok(false),
        };
        fs::write(sandbox.root().join("faults").join(step.name()), word)?;
        Ok(true)
    }

    fn residue(&self, sandbox: &Sandbox) -> io::Result<Vec<String>> {
        let mut left = Vec::new();
        if fs::read_to_string(sandbox.world().join("helper.json"))?.contains("\"pending\":true") {
            left.push("trial".to_owned());
        }
        let drained = fs::read_to_string(sandbox.world().join("node"))? == "drained";
        if drained && !self.operator_drained {
            left.push("cordon".to_owned());
        }
        if sandbox.world().join("mark").exists() {
            left.push("mark".to_owned());
        }
        Ok(left)
    }
}

/// The fake host's program and its `hook_args`: the fake's own flags (where the host and the
/// faults are) and the flags of `kbf update-hook` (where the record is, the channel, the
/// certificate files, which the fake world never reads, and the waits). The harness starts
/// every step in the root of its sandbox, so the paths are relative to it: `world/` and
/// `record/` are the sandbox's own, `faults/` is the one `init` makes.
fn hook() -> HookCommand {
    HookCommand::new(Path::new(env!("CARGO_BIN_EXE_kbf-updater-hook-fake"))).with_hook_args(&[
        "update-hook",
        "--world",
        "world",
        "--faults",
        "faults",
        "--record-dir",
        "record",
        "--channel",
        "stable",
        "--client-cert",
        "absent/cert.pem",
        "--client-key",
        "absent/key.pem",
        "--ca",
        "absent/ca.pem",
        "--drain-deadline-secs",
        "30",
        "--retry-secs",
        "60",
    ])
}

/// The environment of the hook is empty: the fake host is found by the flags of [`hook`]
/// alone, and they name the sandbox's own directories.
///
/// Catches: configuration slipped back into the environment, which a declaration's
/// `hook_args` would not show.
#[test]
fn the_fake_host_is_configured_by_flags_alone() {
    let scratch = Scratch::new("flags");
    let sandbox = Sandbox::create(scratch.0.join("run")).expect("a sandbox");
    assert!(FakeHost::default().env(&sandbox).is_empty());
    let args: Vec<String> = hook()
        .hook_args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let value = |flag: &str| {
        let at = args.iter().position(|a| a == flag).expect(flag);
        sandbox.root().join(&args[at + 1])
    };
    assert_eq!(value("--world"), sandbox.world());
    assert_eq!(value("--record-dir"), sandbox.record());
    assert_eq!(value("--faults"), sandbox.root().join("faults"));
}

/// `kbf update-hook` keeps the contract on a host whose node serves: every step is safe to run
/// twice, a kill at any write of the record or of the host is repaired by running on, the
/// updater's table ends as injected at each step, a deferral leaves the host as it was, and
/// the same holds as a first install.
///
/// Catches: a hook step that is not idempotent, that decides from the order it was called in
/// and not from its record, or that leaves a cordon or a trial behind.
#[test]
fn the_kbf_hook_keeps_the_contract() {
    let scratch = Scratch::new("serving");
    let mut fake = FakeHost::default();
    let summary = check(&hook(), &mut fake, &scratch.0).unwrap_or_else(|e| panic!("{e}"));
    assert!(summary.kill_points > 10, "{summary:?}");
    assert_eq!(
        summary.scenarios,
        [
            "none",
            "prepare=Fail",
            "prepare=Defer",
            "cutover=Fail",
            "verify=Fail",
            "cleanup=Fail",
            "cutover=Fail,rollback=Fail",
            "verify=Fail,rollback=Fail",
        ]
    );
    assert_eq!(
        summary.skipped,
        [
            "prepare=Halt",
            "cutover=Defer",
            "cutover=Halt",
            "verify=Defer",
            "verify=Halt",
            "cleanup=Defer",
            "cleanup=Halt",
            "cutover=Fail,rollback=Defer",
        ]
    );
}

/// The same on a node an operator had drained before the update: the hook drains it and waits
/// as for any node, never gives it back, and the host is as it was after a rollback.
///
/// Catches: a hook that uncordons a node it did not cordon, which the harness sees as a world
/// that is not the world it started from.
#[test]
fn the_kbf_hook_keeps_the_contract_on_a_node_an_operator_drained() {
    let scratch = Scratch::new("operator-drained");
    let mut fake = FakeHost {
        operator_drained: true,
    };
    check(&hook(), &mut fake, &scratch.0).unwrap_or_else(|e| panic!("{e}"));
}
