//! Runs the Command of an action in its laid-out input root.
//!
//! [`run`] does what the daemon's local runtime does for one Command: the working directory and
//! the parent of every declared output are made first (REAPI makes the worker do it), then the
//! program is started with the inherited environment cleared and only the Command's variables
//! set, standard input closed, and standard output and error written to files. A program name with
//! a slash is resolved from the working directory, a bare name from the Command's own `PATH`.
//!
//! The program leads its own process group. When the deadline passes the whole group is killed
//! with SIGKILL and reaped, and the run is reported as timed out with exit code 137. A process a
//! signal ended reports 128 plus the signal number, as the daemon does.
//!
//! CPU time and peak memory come from `getrusage(RUSAGE_CHILDREN)`: the difference in CPU time
//! over the run, and the largest resident set of any child this process has reaped. They cover
//! the program and every descendant it waited for, and are exact only while this tool runs one
//! Command at a time, which it does.

use std::io::ErrorKind;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use kbf_proto::reapi::Command;
use tokio::fs;
use tokio::process::Command as Process;

/// The exit code reported for a run the deadline ended: 128 plus SIGKILL.
pub const TIMED_OUT_EXIT_CODE: i32 = 128 + 9;

/// Why a Command could not be run (not: why it failed; a failing program is a [`Ran`]).
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// The Command is not runnable as written, or its input root makes it unsafe to run.
    #[error("invalid command: {0}")]
    Invalid(String),
    /// A file or directory the run needs could not be made.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The program could not be started.
    #[error("cannot start {program}: {source}")]
    Spawn {
        program: PathBuf,
        source: std::io::Error,
    },
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ran {
    /// The exit status; 128 plus the signal number for a program a signal ended;
    /// [`TIMED_OUT_EXIT_CODE`] when the deadline did.
    pub exit_code: i32,
    /// The deadline passed and the process group was killed.
    pub timed_out: bool,
    /// Wall-clock milliseconds from start to reaped.
    pub wall_ms: u64,
    /// User plus system CPU time of the program and its reaped descendants, in microseconds.
    pub cpu_us: u64,
    /// The largest resident set of any reaped child, in bytes.
    pub peak_rss_bytes: u64,
}

/// Where a Command runs and where its output streams go.
#[derive(Clone, Debug)]
pub struct Run<'a> {
    /// The input root, already laid out; the Command's paths are relative to it.
    pub root: &'a Path,
    /// Created (it must not be a symlink to anything unexpected: it is created, not followed).
    pub stdout: &'a Path,
    pub stderr: &'a Path,
    /// After this long the process group is killed.
    pub deadline: Duration,
}

/// Runs `command` under `run.root`.
///
/// # Errors
/// [`RunError::Invalid`]: no arguments, a working directory or output path that is not relative
/// and plain, or one the input root made something other than a directory (a symlink or a file).
/// [`RunError::Io`]: a directory or an output stream file could not be made.
/// [`RunError::Spawn`]: the program could not be started.
pub async fn run(command: &Command, run: &Run<'_>) -> Result<Ran, RunError> {
    let Some(program) = command.arguments.first() else {
        return Err(RunError::Invalid("the Command has no arguments".to_owned()));
    };
    check_relative("working directory", &command.working_directory)?;
    let outputs = output_paths(command);
    for path in &outputs {
        if path.is_empty() {
            return Err(RunError::Invalid("an output path is empty".to_owned()));
        }
        check_relative("output path", path)?;
    }
    let work_dir = real_dirs(run.root, &command.working_directory).await?;
    for output in &outputs {
        let parent = output.rsplit_once('/').map_or("", |(parent, _)| parent);
        real_dirs(&work_dir, parent).await?;
    }
    let stdout = create(run.stdout)?;
    let stderr = create(run.stderr)?;
    let program = if program.contains('/') {
        work_dir.join(program)
    } else {
        PathBuf::from(program)
    };
    let mut process = Process::new(&program);
    process
        .args(&command.arguments[1..])
        .current_dir(&work_dir)
        .env_clear()
        .envs(
            command
                .environment_variables
                .iter()
                .map(|v| (&v.name, &v.value)),
        )
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0)
        .kill_on_drop(true);

    let cpu_before = children_usage().cpu_us;
    let started = Instant::now();
    let mut child = process.spawn().map_err(|source| RunError::Spawn {
        program: program.clone(),
        source,
    })?;
    // Taken before the wait: the pid is also the id of the group the child leads.
    let group = child.id();
    let waited = tokio::time::timeout(run.deadline, child.wait()).await;
    let (status, timed_out) = if let Ok(status) = waited {
        (status, false)
    } else {
        kill_group(group);
        (child.wait().await, true)
    };
    // Whatever the program left running in its group is ended with it.
    kill_group(group);
    let status = status.map_err(|source| RunError::Spawn {
        program: program.clone(),
        source,
    })?;
    let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let usage = children_usage();
    let exit_code = if timed_out {
        TIMED_OUT_EXIT_CODE
    } else {
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
    };
    Ok(Ran {
        exit_code,
        timed_out,
        wall_ms,
        cpu_us: usage.cpu_us.saturating_sub(cpu_before),
        peak_rss_bytes: usage.peak_rss_bytes,
    })
}

/// The output paths a Command declares: `output_paths` (REAPI 2.1), else the older
/// `output_files` and `output_directories`.
#[allow(deprecated)]
fn output_paths(command: &Command) -> Vec<&String> {
    if command.output_paths.is_empty() {
        command
            .output_files
            .iter()
            .chain(&command.output_directories)
            .collect()
    } else {
        command.output_paths.iter().collect()
    }
}

/// A path the Command names must be empty (the directory itself) or relative, of non-empty
/// components other than `.` and `..`, joined by single slashes.
fn check_relative(what: &str, path: &str) -> Result<(), RunError> {
    let plain = path.is_empty()
        || path
            .split('/')
            .all(|part| !matches!(part, "" | "." | "..") && !part.contains('\0'));
    if plain {
        Ok(())
    } else {
        Err(RunError::Invalid(format!(
            "{what} {path:?} must be relative and must not contain . or .."
        )))
    }
}

/// Creates `base/relative` and each directory on the way that does not exist yet. A component
/// that exists as anything but a directory (a symlink the input tree planted, a file) refuses
/// the command: following it would put the program's work outside the input root.
async fn real_dirs(base: &Path, relative: &str) -> Result<PathBuf, RunError> {
    let mut at = base.to_owned();
    for part in relative.split('/').filter(|p| !p.is_empty()) {
        at.push(part);
        match fs::create_dir(&at).await {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                let meta = fs::symlink_metadata(&at).await.map_err(io(&at))?;
                if !meta.is_dir() {
                    return Err(RunError::Invalid(format!(
                        "{relative:?} passes through {part:?}, which the input tree makes \
                         something other than a directory"
                    )));
                }
            }
            Err(e) => return Err(io(&at)(e)),
        }
    }
    Ok(at)
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> RunError + '_ {
    move |source| RunError::Io {
        path: path.to_owned(),
        source,
    }
}

fn create(path: &Path) -> Result<Stdio, RunError> {
    std::fs::File::create(path)
        .map(Stdio::from)
        .map_err(io(path))
}

/// SIGKILL to the process group `group` leads; nothing if it is already gone. `group` is the pid
/// of a child this process has not reaped yet or has just reaped; a group with members left
/// after its leader was reaped is still named by that id, because the id is not reused while
/// the group has a member.
fn kill_group(group: Option<u32>) {
    if let Some(pid) = group.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
        // SAFETY: `kill` takes plain integers. A group that no longer exists makes it fail
        // with ESRCH, which is what a kill of finished work should do: nothing.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
}

struct ChildrenUsage {
    cpu_us: u64,
    peak_rss_bytes: u64,
}

fn children_usage() -> ChildrenUsage {
    // SAFETY: `rusage` is a C struct of integers; all zeroes is a valid value.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: the pointer is to a live local of the right type, which `getrusage` writes and
    // does not keep. On failure the zeroes stay, which reads as "no usage".
    unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &raw mut usage) };
    let micros = |t: libc::timeval| u64::try_from(t.tv_sec * 1_000_000 + t.tv_usec).unwrap_or(0);
    ChildrenUsage {
        cpu_us: micros(usage.ru_utime) + micros(usage.ru_stime),
        // Linux reports the peak resident set in KiB.
        peak_rss_bytes: u64::try_from(usage.ru_maxrss).unwrap_or(0) * 1024,
    }
}

#[cfg(test)]
mod tests {
    use kbf_proto::reapi::command::EnvironmentVariable;

    use super::*;

    struct Rig {
        dir: PathBuf,
        stdout: PathBuf,
        stderr: PathBuf,
    }

    impl Rig {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("kbf-task-run-{}-{name}", std::process::id()));
            std::fs::create_dir_all(dir.join("root")).expect("root");
            Self {
                stdout: dir.join("stdout"),
                stderr: dir.join("stderr"),
                dir,
            }
        }

        fn root(&self) -> PathBuf {
            self.dir.join("root")
        }

        async fn run(&self, command: &Command, deadline: Duration) -> Result<Ran, RunError> {
            let root = self.root();
            run(
                command,
                &Run {
                    root: &root,
                    stdout: &self.stdout,
                    stderr: &self.stderr,
                    deadline,
                },
            )
            .await
        }

        fn out(&self) -> String {
            std::fs::read_to_string(&self.stdout).expect("stdout")
        }

        fn err(&self) -> String {
            std::fs::read_to_string(&self.stderr).expect("stderr")
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const LONG: Duration = Duration::from_secs(600);

    fn sh(script: &str) -> Command {
        Command {
            arguments: vec!["/bin/sh".into(), "-c".into(), script.into()],
            ..Command::default()
        }
    }

    fn var(name: &str, value: &str) -> EnvironmentVariable {
        EnvironmentVariable {
            name: name.into(),
            value: value.into(),
        }
    }

    #[tokio::test]
    async fn stdout_and_stderr_go_to_their_files() {
        let rig = Rig::new("streams");
        let ran = rig
            .run(&sh("echo out; echo err >&2"), LONG)
            .await
            .expect("ran");
        assert_eq!(ran.exit_code, 0);
        assert!(!ran.timed_out);
        assert_eq!(rig.out(), "out\n");
        assert_eq!(rig.err(), "err\n");
    }

    #[tokio::test]
    async fn the_exit_code_is_reported() {
        let rig = Rig::new("exit");
        let ran = rig.run(&sh("exit 7"), LONG).await.expect("ran");
        assert_eq!(ran.exit_code, 7);
        assert!(!ran.timed_out);
    }

    #[tokio::test]
    async fn a_program_a_signal_ended_reports_128_plus_the_signal() {
        let rig = Rig::new("signal");
        let ran = rig.run(&sh("kill -TERM $$"), LONG).await.expect("ran");
        assert_eq!(ran.exit_code, 128 + 15);
        assert!(!ran.timed_out);
    }

    #[tokio::test]
    async fn the_environment_is_the_commands_alone() {
        let rig = Rig::new("env");
        // Set in the parent of the program; the Command does not name it.
        let mut command =
            sh("echo \"${KBF_TASK_ONLY_MINE-unset}:${KBF_TASK_FROM_COMMAND-unset}:${HOME-unset}\"");
        command.environment_variables = vec![var("KBF_TASK_FROM_COMMAND", "yes")];
        // SAFETY: nothing else in this test binary reads or writes this variable.
        unsafe { std::env::set_var("KBF_TASK_ONLY_MINE", "leaked") };
        rig.run(&command, LONG).await.expect("ran");
        assert_eq!(rig.out(), "unset:yes:unset\n");
    }

    #[tokio::test]
    async fn a_bare_program_name_is_found_on_the_commands_path() {
        let rig = Rig::new("path");
        let command = Command {
            arguments: vec!["sh".into(), "-c".into(), "echo found".into()],
            environment_variables: vec![var("PATH", "/bin:/usr/bin")],
            ..Command::default()
        };
        let ran = rig.run(&command, LONG).await.expect("ran");
        assert_eq!(ran.exit_code, 0);
        assert_eq!(rig.out(), "found\n");
    }

    #[tokio::test]
    async fn a_program_path_with_a_slash_is_relative_to_the_working_directory() {
        let rig = Rig::new("slash");
        let work = rig.root().join("sub");
        std::fs::create_dir_all(&work).expect("work");
        std::os::unix::fs::symlink("/bin/sh", work.join("mysh")).expect("link");
        let command = Command {
            arguments: vec!["./mysh".into(), "-c".into(), "echo ok".into()],
            working_directory: "sub".into(),
            ..Command::default()
        };
        rig.run(&command, LONG).await.expect("ran");
        assert_eq!(rig.out(), "ok\n");
    }

    #[tokio::test]
    async fn the_working_directory_is_made_and_the_program_runs_in_it() {
        let rig = Rig::new("cwd");
        let mut command = sh("pwd -P");
        command.working_directory = "a/b".into();
        rig.run(&command, LONG).await.expect("ran");
        let expected = std::fs::canonicalize(rig.root().join("a/b")).expect("made");
        assert_eq!(rig.out().trim_end(), expected.to_str().expect("utf8"));
    }

    #[tokio::test]
    async fn the_parent_directories_of_outputs_are_made_under_the_working_directory() {
        let rig = Rig::new("parents");
        let mut command = sh("true");
        command.working_directory = "w".into();
        command.output_paths = vec!["out/deep/file".into(), "top".into()];
        rig.run(&command, LONG).await.expect("ran");
        assert!(rig.root().join("w/out/deep").is_dir());
        assert!(!rig.root().join("w/out/deep/file").exists());
        assert!(!rig.root().join("w/top").exists());
    }

    #[tokio::test]
    #[allow(deprecated)]
    async fn the_older_output_lists_make_their_parents_too() {
        let rig = Rig::new("legacy");
        let mut command = sh("true");
        command.output_files = vec!["f/g/file".into()];
        command.output_directories = vec!["d/e".into()];
        rig.run(&command, LONG).await.expect("ran");
        assert!(rig.root().join("f/g").is_dir());
        assert!(rig.root().join("d").is_dir());
        assert!(!rig.root().join("d/e").exists());
    }

    #[tokio::test]
    async fn a_deadline_kills_the_process_group() {
        let rig = Rig::new("deadline");
        // The shell stays alive and a child of it sleeps; both must go.
        let command = sh("sleep 600 & echo $! > pid; wait");
        let ran = rig.run(&command, Duration::ZERO).await.expect("ran");
        assert!(ran.timed_out);
        assert_eq!(ran.exit_code, TIMED_OUT_EXIT_CODE);
    }

    #[tokio::test]
    async fn a_process_left_in_the_group_is_ended_with_the_program() {
        let rig = Rig::new("left");
        let command = sh("sleep 600 & echo $! > pid");
        let ran = rig.run(&command, LONG).await.expect("ran");
        assert_eq!(ran.exit_code, 0);
        let pid = std::fs::read_to_string(rig.root().join("pid")).expect("pid");
        let pid: libc::pid_t = pid.trim().parse().expect("number");
        // Gone (or a zombie adopted by init, which no longer answers a signal-0 probe once
        // reaped); poll the group kill's effect through /proc without timing assertions.
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            match stat {
                Err(_) => break,
                Ok(s) if s.contains(") Z ") => break,
                Ok(_) => tokio::task::yield_now().await,
            }
        }
    }

    #[tokio::test]
    async fn usage_is_reported() {
        let rig = Rig::new("usage");
        let ran = rig.run(&sh("echo hi"), LONG).await.expect("ran");
        // A shell that ran and was reaped has a resident set; the other figures are
        // measured, not asserted.
        assert!(ran.peak_rss_bytes > 0, "{ran:?}");
    }

    #[tokio::test]
    async fn a_command_without_arguments_is_refused() {
        let rig = Rig::new("noargs");
        let error = rig
            .run(&Command::default(), LONG)
            .await
            .expect_err("refused");
        assert!(matches!(error, RunError::Invalid(_)), "{error}");
    }

    #[tokio::test]
    async fn paths_that_leave_the_root_are_refused() {
        let rig = Rig::new("paths");
        for (work, output) in [
            ("..", ""),
            ("/abs", ""),
            ("a//b", ""),
            ("", "../x"),
            ("", ""),
        ] {
            let mut command = sh("true");
            command.working_directory = work.into();
            command.output_paths = vec![output.into()];
            let error = rig.run(&command, LONG).await.expect_err("refused");
            assert!(
                matches!(error, RunError::Invalid(_)),
                "{work} {output}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn a_working_directory_through_a_symlink_or_file_is_refused() {
        let rig = Rig::new("links");
        std::os::unix::fs::symlink("/", rig.root().join("link")).expect("link");
        std::fs::write(rig.root().join("file"), b"x").expect("file");
        for work in ["link", "link/x", "file/x"] {
            let mut command = sh("true");
            command.working_directory = work.into();
            let error = rig.run(&command, LONG).await.expect_err("refused");
            assert!(matches!(error, RunError::Invalid(_)), "{work}: {error}");
        }
        // The same for a directory an output needs.
        let mut command = sh("true");
        command.output_paths = vec!["link/out".into()];
        let error = rig.run(&command, LONG).await.expect_err("refused");
        assert!(matches!(error, RunError::Invalid(_)), "{error}");
    }

    #[tokio::test]
    async fn a_directory_that_cannot_be_made_is_an_io_error() {
        let rig = Rig::new("mkdir");
        std::fs::set_permissions(
            rig.root(),
            std::os::unix::fs::PermissionsExt::from_mode(0o500),
        )
        .expect("chmod");
        let mut command = sh("true");
        command.working_directory = "new".into();
        let result = rig.run(&command, LONG).await;
        std::fs::set_permissions(
            rig.root(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("chmod back");
        // A privileged user ignores the mode; then the directory is made and nothing fails.
        if let Err(error) = result {
            assert!(matches!(error, RunError::Io { .. }), "{error}");
        }
    }

    #[tokio::test]
    async fn a_stream_file_that_cannot_be_created_is_an_io_error() {
        let rig = Rig::new("streamfile");
        let root = rig.root();
        let missing = rig.dir.join("none/stdout");
        let error = run(
            &sh("true"),
            &Run {
                root: &root,
                stdout: &missing,
                stderr: &rig.stderr,
                deadline: LONG,
            },
        )
        .await
        .expect_err("refused");
        assert!(matches!(error, RunError::Io { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_program_that_does_not_exist_is_a_spawn_error() {
        let rig = Rig::new("spawn");
        let command = Command {
            arguments: vec!["./no-such-program".into()],
            ..Command::default()
        };
        let error = rig.run(&command, LONG).await.expect_err("refused");
        assert!(matches!(error, RunError::Spawn { .. }), "{error}");
    }
}
