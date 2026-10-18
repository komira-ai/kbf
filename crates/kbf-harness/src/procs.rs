//! A real process, its output in files so a failed run can say why.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// How long a run waits for what should happen promptly: it ends a hang only.
pub const HANG: Duration = Duration::from_secs(60);

/// What starts one process: the program, its arguments and environment, and the
/// directory it runs in. A [`Spec`] is kept so a role can be started again with the
/// identical command line.
#[derive(Clone, Debug)]
pub struct Spec {
    pub name: String,
    pub program: PathBuf,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// A running process.
pub struct Proc {
    pub name: String,
    child: Child,
    out: PathBuf,
    err: PathBuf,
}

/// The log files of generation `generation` of process `name` in `logs`: the first start
/// is `<name>.out` and `.err`, a later one `<name>.<generation>.out`, so a restart
/// keeps what the process before it printed.
#[must_use]
pub fn log_files(logs: &Path, name: &str, generation: usize) -> (PathBuf, PathBuf) {
    let stem = if generation == 0 {
        name.to_owned()
    } else {
        format!("{name}.{generation}")
    };
    (
        logs.join(format!("{stem}.out")),
        logs.join(format!("{stem}.err")),
    )
}

impl Proc {
    /// Starts `spec`, its stdout and stderr in the files [`log_files`] names.
    ///
    /// # Panics
    /// When the log files cannot be made or the program cannot be spawned: a harness
    /// that cannot start its process has nothing to report.
    #[must_use]
    pub fn start(spec: &Spec, logs: &Path, generation: usize) -> Self {
        let (out, err) = log_files(logs, &spec.name, generation);
        let child = Command::new(&spec.program)
            .args(&spec.argv)
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&spec.cwd)
            .stdout(Stdio::from(File::create(&out).expect("stdout file")))
            .stderr(Stdio::from(File::create(&err).expect("stderr file")))
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", spec.program.display()));
        Self {
            name: spec.name.clone(),
            child,
            out,
            err,
        }
    }

    /// The process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// What the process printed, for a failure message.
    #[must_use]
    pub fn log(&self) -> String {
        let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
        format!(
            "--- {} stdout ---\n{}--- {} stderr ---\n{}",
            self.name,
            read(&self.out),
            self.name,
            read(&self.err)
        )
    }

    /// Its exit status, if it has ended.
    ///
    /// # Panics
    /// When the operating system cannot say.
    pub fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().expect("wait")
    }

    /// The first stdout line that starts with `prefix`, once the process printed it.
    #[must_use]
    pub fn line(&self, prefix: &str) -> Option<String> {
        let text = std::fs::read_to_string(&self.out).ok()?;
        text.lines()
            .find(|line| line.starts_with(prefix))
            .map(str::to_owned)
    }

    /// SIGTERM, and the exit status if the process has drained within `limit`.
    ///
    /// # Panics
    /// When the signal cannot be sent.
    pub fn term_within(&mut self, limit: Duration) -> Option<ExitStatus> {
        let pid = rustix::process::Pid::from_child(&self.child);
        rustix::process::kill_process(pid, rustix::process::Signal::TERM).expect("SIGTERM");
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.exited() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// SIGKILL: the process is gone, as a host losing power would take it.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The value of `key` in a start line such as `kbf-frontend 1.0 reapi=127.0.0.1:80 ...`.
///
/// # Panics
/// When the line has no such field.
#[must_use]
pub fn field<'a>(line: &'a str, key: &str) -> &'a str {
    line.split(' ')
        .find_map(|word| word.strip_prefix(key))
        .unwrap_or_else(|| panic!("no {key} in {line:?}"))
}
