//! One run of a hook step as a child process.

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use super::HookCommand;
use super::gate::{COUNT_FILE, GATE_DIR_VAR, KILL_AT_VAR, PARKED};
use super::sandbox::Sandbox;
use crate::Invocation;

/// The only variable of the harness's own environment a hook inherits: the profile file of a
/// coverage run, so that a hook built with coverage reports its lines.
const PASS_THROUGH: &[&str] = &["LLVM_PROFILE_FILE"];

/// How a run ended.
#[derive(Debug)]
pub(crate) struct Run {
    /// The exit code; `None` for a run killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// The run stopped at its kill point and was killed there.
    pub parked: bool,
    /// Kill points reached.
    pub gates: u64,
}

/// Runs the step of `inv` in `sandbox`: the hook's command line, the environment `env` and the
/// kill-point variables and nothing else, standard input empty, a process group of its own.
/// With `kill_at` above zero, stops the run at that kill point.
pub(crate) fn run_step(
    hook: &HookCommand,
    env: Vec<(OsString, OsString)>,
    sandbox: &Sandbox,
    inv: &Invocation,
    kill_at: u64,
) -> io::Result<Run> {
    let count = sandbox.control().join(COUNT_FILE);
    fs::write(&count, b"")?;
    let (out_read, out_write) = io::pipe()?;
    let (err_read, err_write) = io::pipe()?;
    let mut command = Command::new(&hook.executable);
    command
        .args(&hook.hook_args)
        .args(inv.to_args())
        .env_clear()
        .envs(
            PASS_THROUGH.iter().filter_map(|name| {
                std::env::var_os(name).map(|value| (OsString::from(*name), value))
            }),
        )
        .envs(env)
        .env(GATE_DIR_VAR, sandbox.control())
        .env(KILL_AT_VAR, kill_at.to_string())
        .current_dir(sandbox.root())
        .stdin(Stdio::null())
        .stdout(out_write)
        .stderr(err_write)
        .process_group(0);
    let mut child = command.spawn()?;
    // The command holds the write ends; the readers see end-of-file only when it lets go.
    drop(command);

    let stdout = std::thread::spawn(move || {
        let mut out = out_read;
        let mut bytes = Vec::new();
        let _ = out.read_to_end(&mut bytes);
        bytes
    });
    let mut errors = BufReader::new(err_read);
    let mut stderr = Vec::new();
    let mut line = Vec::new();
    let parked = loop {
        line.clear();
        if errors.read_until(b'\n', &mut line)? == 0 {
            break false;
        }
        if line.trim_ascii_end() == PARKED.as_bytes() {
            break true;
        }
        stderr.extend_from_slice(&line);
    };
    if parked {
        // SAFETY: kill(2) on the process group the child leads (`process_group(0)` above));
        // no memory is touched.
        unsafe {
            libc::kill(
                -i32::try_from(child.id()).unwrap_or(i32::MAX),
                libc::SIGKILL,
            );
        }
    }
    let status = child.wait()?;
    Ok(Run {
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout.join().unwrap_or_default()).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        parked,
        gates: fs::metadata(&count).map_or(0, |meta| meta.len()),
    })
}
