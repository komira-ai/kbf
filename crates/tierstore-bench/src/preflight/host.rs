//! The machine the ops verbs run on, as a seam: files, commands, a port and the load sample.
//!
//! The ops verbs read a node (its files, `systemctl`, a port) and drive one admin tool. Behind
//! [`Host`] they are the same code on the node and in a test, where a fake answers from a table
//! and records the commands it was asked to run. [`SystemHost`] is the node itself.

use std::io::{self, Read, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::error::BenchError;
use crate::gate::LoadSample;

/// A command to run: a program, its arguments, the environment variables added to it and the
/// text it is given on standard input.
///
/// The arguments, the variables and the input may carry a secret (an account's key pair), so
/// `Debug` prints the program and the counts only, and an error built from a command never
/// repeats them. A secret goes in the input or a variable, never in the arguments: arguments are
/// readable by every user of the machine.
#[derive(Clone, PartialEq, Eq)]
pub struct Cmd {
    /// The program, a path or a name looked up on `PATH`.
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// Environment variables set for it, over the caller's own.
    pub env: Vec<(String, String)>,
    /// What it reads from standard input, if anything; none is an empty, closed input.
    pub stdin: Option<String>,
}

impl Cmd {
    /// A command with no variables.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>, args: &[&str]) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            env: Vec::new(),
            stdin: None,
        }
    }

    /// The same command reading `text` from standard input.
    #[must_use]
    pub fn with_stdin(mut self, text: String) -> Self {
        self.stdin = Some(text);
        self
    }

    /// The same command with one more variable.
    #[must_use]
    pub fn with_env(mut self, name: &str, value: String) -> Self {
        self.env.push((name.to_owned(), value));
        self
    }
}

impl std::fmt::Debug for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Cmd({}, {} args, {} variables, {} input; not printed)",
            self.program.display(),
            self.args.len(),
            self.env.len(),
            if self.stdin.is_some() { "with" } else { "no" }
        )
    }
}

/// How a command ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CmdOutput {
    /// Whether it exited with status 0.
    pub success: bool,
    /// The exit status, if it exited (not killed by a signal).
    pub code: Option<i32>,
    /// What it wrote to standard output.
    pub stdout: String,
    /// What it wrote to standard error.
    pub stderr: String,
}

/// What the ops verbs need of the machine they run on.
pub trait Host: Send + Sync {
    /// The bytes of a file.
    ///
    /// # Errors
    /// The operating system's error.
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>>;

    /// The bytes of a status file that a writer outside this tool keeps and the run believes
    /// (the cold store's heal status file). The machine itself refuses it unless it passes
    /// [`crate::preflight::trusted::rule`] and is opened without following a link; the default
    /// reads like [`Host::read_file`], for a host that is not a real file system.
    ///
    /// # Errors
    /// The operating system's error, or a refusal.
    fn read_status_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.read_file(path)
    }

    /// Whether a file or directory is at `path`.
    fn exists(&self, path: &Path) -> bool;

    /// Runs a command to its end, with no input.
    ///
    /// # Errors
    /// The command could not be started.
    fn run(&self, cmd: &Cmd) -> io::Result<CmdOutput>;

    /// Runs a command until it ends or `stop` is set; if `stop` is set first, the command is
    /// killed and the answer is `None`. Only the child this call started is killed.
    ///
    /// The default runs the command to its end and ignores `stop`.
    ///
    /// # Errors
    /// The command could not be started.
    fn run_until(&self, cmd: &Cmd, _stop: &AtomicBool) -> io::Result<Option<CmdOutput>> {
        self.run(cmd).map(Some)
    }

    /// Whether something accepts connections on loopback `port`.
    fn port_open(&self, port: u16) -> bool;

    /// One reading of the machine's load, for the load gate.
    ///
    /// # Errors
    /// [`BenchError`] if the reading cannot be made.
    fn load_sample(&self) -> Result<LoadSample, BenchError>;
}

/// The machine itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemHost;

/// The longest a connection to a loopback port may take before the port counts as closed.
const PORT_CONNECT: Duration = Duration::from_millis(500);

/// How often a command run with `run_until` is looked at.
const POLL: Duration = Duration::from_millis(20);

/// Starts `cmd` with its pipes and writes its input.
fn start(cmd: &Cmd) -> io::Result<Child> {
    let mut child = Command::new(&cmd.program)
        .args(&cmd.args)
        .envs(cmd.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(if cmd.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(text) = &cmd.stdin {
        // A write that fails (the child exited without reading) is not an error of its own:
        // the exit status says what happened. Dropping the pipe closes the child's input.
        child.stdin.take().into_iter().for_each(|mut pipe| {
            pipe.write_all(text.as_bytes()).ok();
        });
    }
    Ok(child)
}

/// The text a pipe gives until it closes, read on a thread of its own.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<String> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.into_iter().for_each(|mut pipe| {
            pipe.read_to_end(&mut bytes).ok();
        });
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

impl Host for SystemHost {
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn read_status_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        super::trusted::read_trusted(path)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn run(&self, cmd: &Cmd) -> io::Result<CmdOutput> {
        let out = start(cmd)?.wait_with_output()?;
        Ok(CmdOutput {
            success: out.status.success(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn run_until(&self, cmd: &Cmd, stop: &AtomicBool) -> io::Result<Option<CmdOutput>> {
        let mut child = start(cmd)?;
        // Read on threads, so a command that fills a pipe is not a command that hangs.
        let (out, err) = (drain(child.stdout.take()), drain(child.stderr.take()));
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if stop.load(Ordering::SeqCst) {
                // This child only, by the handle that started it. The reader threads end with
                // the pipes; they are not waited for, as a grandchild may hold them open.
                child.kill().ok();
                child.wait().ok();
                return Ok(None);
            }
            std::thread::sleep(POLL);
        };
        Ok(Some(CmdOutput {
            success: status.success(),
            code: status.code(),
            stdout: out.join().unwrap_or_default(),
            stderr: err.join().unwrap_or_default(),
        }))
    }

    fn port_open(&self, port: u16) -> bool {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        TcpStream::connect_timeout(&addr, PORT_CONNECT).is_ok()
    }

    fn load_sample(&self) -> Result<LoadSample, BenchError> {
        LoadSample::read()
    }
}

/// The text of a file.
///
/// # Errors
/// [`BenchError::Io`] if it cannot be read. Bytes that are not UTF-8 are replaced.
pub fn read_text(host: &dyn Host, path: &Path) -> Result<String, BenchError> {
    host.read_file(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|source| BenchError::Io {
            path: path.to_owned(),
            source,
        })
}

/// The text of a status file (see [`Host::read_status_file`]).
///
/// # Errors
/// [`BenchError::Io`] if it cannot be read or is refused. Bytes that are not UTF-8 are replaced.
pub fn read_status_text(host: &dyn Host, path: &Path) -> Result<String, BenchError> {
    host.read_status_file(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|source| BenchError::Io {
            path: path.to_owned(),
            source,
        })
}

/// The longest stretch of a command's standard error an error message keeps.
const STDERR_KEPT: usize = 200;

/// Runs `cmd` and returns its standard output.
///
/// A failed command's standard error (its first 200 characters) goes to this process's own
/// standard error, for the operator at the terminal. It is not in the error: a tool's complaint
/// may name a host, a path or a key, and the error ends up in a check's detail and so in a file.
///
/// # Errors
/// [`BenchError::Ops`] naming `what` if the command cannot be started or exits non-zero; the
/// message carries the exit status, never the arguments or the output.
pub fn run_ok(host: &dyn Host, what: &str, cmd: &Cmd) -> Result<String, BenchError> {
    let out = host
        .run(cmd)
        .map_err(|e| BenchError::Ops(format!("{what}: cannot start: {:?}", e.kind())))?;
    if out.success {
        return Ok(out.stdout);
    }
    let first: String = out.stderr.trim().chars().take(STDERR_KEPT).collect();
    eprintln!("{what}: {first}");
    Err(BenchError::Ops(format!("{what}: exited {:?}", out.code)))
}
