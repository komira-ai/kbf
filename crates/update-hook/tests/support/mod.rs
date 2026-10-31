//! Shared by the tests: a scratch directory and the fake service of the example hook.
#![allow(dead_code)]

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use update_hook::conformance::{FakeService, Fault, HookCommand, Sandbox};
use update_hook::{Params, Scalar, Step};

/// A directory under the target directory, unique to the test, removed when it is dropped.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Scratch {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("update-hook-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create the scratch directory");
        Scratch(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The example hook, with these arguments before the step.
pub fn example(args: &[&str]) -> HookCommand {
    HookCommand::new(Path::new(env!("CARGO_BIN_EXE_update-hook-example"))).with_hook_args(args)
}

/// The example hook with the conventional `hook_args` and one defect.
pub fn example_with(defect: &str) -> HookCommand {
    example(&["update-hook", &format!("--defect={defect}")])
}

/// The fake service of the example hook: a blue/green pair in `world/`, version `v1` live and
/// `v2` staged in the artifact.
#[derive(Default)]
pub struct ExampleFake {
    /// The fake cannot make a step halt (a service with no check that cannot be repaired).
    pub no_halt: bool,
}

impl FakeService for ExampleFake {
    fn init(&mut self, sandbox: &Sandbox) -> io::Result<()> {
        fs::write(sandbox.artifact().join("VERSION"), "v2\n")?;
        std::os::unix::fs::symlink("v1", sandbox.world().join("current"))?;
        fs::create_dir_all(sandbox.root().join("faults"))
    }

    fn env(&self, sandbox: &Sandbox) -> Vec<(OsString, OsString)> {
        vec![
            ("EXAMPLE_WORLD".into(), sandbox.world().into()),
            ("EXAMPLE_RECORD".into(), sandbox.record().into()),
            ("EXAMPLE_OUTSIDE".into(), sandbox.outside().into()),
            (
                "EXAMPLE_FAULTS".into(),
                sandbox.root().join("faults").into(),
            ),
        ]
    }

    fn params(&self) -> Params {
        let mut params = Params::default();
        params.insert("promotion_bound_secs", Scalar::Integer(900));
        params
    }

    fn inject(&mut self, sandbox: &Sandbox, step: Step, fault: Fault) -> io::Result<bool> {
        if fault == Fault::Halt && self.no_halt {
            return Ok(false);
        }
        let word = match fault {
            Fault::Fail => "fail",
            Fault::Defer => "defer",
            Fault::Halt => "halt",
        };
        fs::write(sandbox.root().join("faults").join(step.name()), word)?;
        Ok(true)
    }

    fn residue(&self, sandbox: &Sandbox) -> io::Result<Vec<String>> {
        Ok(["next", "cordon"]
            .into_iter()
            .filter(|name| sandbox.world().join(name).exists())
            .map(str::to_owned)
            .collect())
    }
}
