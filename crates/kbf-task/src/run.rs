//! Runs the Command of an action in its laid-out input root.
//!
//! [`run`] does what the daemon's local runtime does for one Command: the working directory and
//! the parent of every declared output are made first (REAPI makes the worker do it), then the
//! program is started with the inherited environment cleared, standard input closed, and standard
//! output and error written to files. Its environment is the run's own `HOME`, `TMPDIR`,
//! `XDG_CACHE_HOME` and `CLANG_MODULE_CACHE_PATH` ([`private_vars`]), naming directories made
//! new for the run (an existing private directory is refused), then the Command's
//! variables: a variable the Command sets itself is the Command's. A program name with
//! a slash is resolved from the working directory, a bare name from the Command's own `PATH`.
//!
//! Unless the action asked for it (`kbf-network=allow`, [`network`]), the program and everything it
//! starts can make no socket but a Unix one.
//!
//! The program leads its own process group. When the deadline passes the whole group is killed
//! with SIGKILL and reaped, and the run is reported as timed out with exit code 137. A process a
//! signal ended reports 128 plus the signal number, as the daemon does.
//!
//! CPU time and peak memory come from `getrusage(RUSAGE_CHILDREN)`: the difference in CPU time
//! over the run, and the largest resident set of any child this process has reaped. They cover
//! the program and every descendant it waited for, and are exact only while this tool runs one
//! Command at a time, which it does.

use std::ffi::OsString;
use std::io::ErrorKind;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use kbf_proto::reapi::Command;
use tokio::fs;
use tokio::process::Command as Process;

use crate::fetch;
use crate::network::{self, Layers, Network};

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
    /// The Command is to be denied the network and this system has no way to deny it (neither
    /// Linux nor macOS): it is refused, not run with the network.
    #[error(
        "this system cannot deny the Command the network (a confine is built for Linux and macOS only), so the Command is not run; only a Command allowed the network (kbf-network=allow) runs here"
    )]
    NoConfine,
    /// A file this tool holds a credential in cannot be hidden from the Command (macOS): its path
    /// cannot be written into the sandbox profile. Nothing was run.
    #[error("cannot confine the Command: {0}")]
    Confine(String),
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
    /// The layers that kept the Command off the network ([`network`]).
    pub layers: Layers,
}

/// Where a Command runs and where its output streams go.
#[derive(Clone, Debug)]
pub struct Run<'a> {
    /// The input root, already laid out; the Command's paths are relative to it.
    pub root: &'a Path,
    /// Where the run's `home`, `tmp` and `cache` directories are made, new for the run: a path
    /// that already exists is refused. It must not be inside `root`: outputs are collected relative to `root`, and nothing here is.
    pub private: &'a Path,
    /// Created (it must not be a symlink to anything unexpected: it is created, not followed).
    pub stdout: &'a Path,
    pub stderr: &'a Path,
    /// After this long the process group is killed.
    pub deadline: Duration,
    /// Whether the program may use the network; [`Network::Denied`] unless the action asked
    /// ([`network::from_platform`]).
    pub network: Network,
    /// Files holding a credential or a presigned URL this tool was given (the grant token file,
    /// the PUT URL files). On macOS the Command's sandbox profile denies reading each; on Linux
    /// the Command's own filesystem does not hold them and the list is not used.
    pub unreadable: &'a [PathBuf],
}

/// Runs `command` under `run.root`.
///
/// # Errors
/// [`RunError::Invalid`]: no arguments, a working directory or output path that is not relative
/// and plain, or one the input root made something other than a directory (a symlink or a file).
/// [`RunError::Io`]: a directory (the run's private ones included) or an output stream file could
/// not be made.
/// [`RunError::Spawn`]: the program could not be started, or the no-network filter could not be
/// installed (the network is never left open for it).
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
    let private = private_vars(run.private).await?;
    let stdout = create(run.stdout)?;
    let stderr = create(run.stderr)?;
    let program = if program.contains('/') {
        work_dir.join(program)
    } else {
        PathBuf::from(program)
    };
    let launch = network::launch(
        &program,
        &command.arguments[1..],
        run.network,
        run.unreadable,
    )?;
    let mut process = Process::new(&launch.program);
    process
        .args(&launch.args)
        .current_dir(&work_dir)
        .env_clear()
        .envs(private)
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
    let confinement = network::confine(&mut process, run.network)?;

    let cpu_before = children_usage().cpu_us;
    let started = Instant::now();
    let started_as = launch.name(&program);
    let spawn_error = |source| RunError::Spawn {
        program: started_as.clone(),
        source,
    };
    let mut child = process.spawn().map_err(spawn_error)?;
    let layers = confinement.layers();
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
    let status = status.map_err(spawn_error)?;
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
        layers,
    })
}

/// The directories made under the run's private directory, parents first.
const PRIVATE_DIRS: [&str; 5] = [
    "home",
    "tmp",
    "cache",
    "cache/clang",
    "cache/clang/ModuleCache",
];

/// The variable naming each of those directories. The image root of a job is read-only and its
/// `/tmp` is small and `noexec`, so a tool that writes under `~`, the temporary directory or a
/// per-user cache (`clang` keeps its module cache there) needs these.
const PRIVATE_VARS: [(&str, &str); 4] = [
    ("HOME", "home"),
    ("TMPDIR", "tmp"),
    ("XDG_CACHE_HOME", "cache"),
    ("CLANG_MODULE_CACHE_PATH", "cache/clang/ModuleCache"),
];

/// Makes `private` and the run's home, temporary and cache directories under it, new for this
/// run and each readable by this user alone (mode 0700), and returns the variables that name
/// them. Nothing is created recursively, so a `private` that already exists (a directory with
/// an earlier run's files, a file, a symlink) is refused rather than reused or followed.
async fn private_vars(private: &Path) -> Result<Vec<(&'static str, OsString)>, RunError> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(private).await.map_err(|e| {
        io(private)(std::io::Error::new(
            e.kind(),
            format!("cannot make the run's private directory (an existing one is refused): {e}"),
        ))
    })?;
    for sub in PRIVATE_DIRS {
        let path = private.join(sub);
        builder.create(&path).await.map_err(io(&path))?;
    }
    Ok(PRIVATE_VARS
        .iter()
        .map(|(name, sub)| (*name, private.join(sub).into_os_string()))
        .collect())
}

/// The output paths a Command declares: `output_paths` (REAPI 2.1), else the older
/// `output_files` and `output_directories`.
#[allow(deprecated)]
pub(crate) fn output_paths(command: &Command) -> Vec<&String> {
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
/// components other than `.` and `..`: the rule of [`fetch::check_relative`].
fn check_relative(what: &str, path: &str) -> Result<(), RunError> {
    fetch::check_relative(what, path).map_err(|e| RunError::Invalid(e.to_string()))
}

/// Creates `base/relative` and each directory on the way that does not exist yet. A component
/// that exists as anything but a directory (a symlink the input tree contains, a file) refuses
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
    // `tv_usec` is an `i32` on macOS and an `i64` on Linux.
    #[allow(clippy::useless_conversion)]
    let micros = |t: libc::timeval| {
        u64::try_from(i64::from(t.tv_sec) * 1_000_000 + i64::from(t.tv_usec)).unwrap_or(0)
    };
    ChildrenUsage {
        cpu_us: micros(usage.ru_utime) + micros(usage.ru_stime),
        // Linux reports the peak resident set in KiB, macOS in bytes.
        peak_rss_bytes: u64::try_from(usage.ru_maxrss).unwrap_or(0)
            * if cfg!(target_os = "macos") { 1 } else { 1024 },
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use kbf_proto::reapi::command::EnvironmentVariable;
    use kbf_testkit::dir::TestDir;

    use super::*;

    const NETWORK: Network = Network::Denied;

    struct Rig {
        dir: TestDir,
        stdout: PathBuf,
        stderr: PathBuf,
    }

    impl Rig {
        fn new(name: &str) -> Self {
            let dir = TestDir::under(std::env::temp_dir(), &format!("kbf-task-run-{name}"));
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

        fn private(&self) -> PathBuf {
            self.dir.join("private")
        }

        async fn run(&self, command: &Command, deadline: Duration) -> Result<Ran, RunError> {
            let root = self.root();
            run(
                command,
                &Run {
                    root: &root,
                    private: &self.private(),
                    stdout: &self.stdout,
                    stderr: &self.stderr,
                    deadline,
                    network: NETWORK,
                    unreadable: &[],
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

    const LONG: Duration = Duration::from_secs(600);

    /// A Command that runs `argv` directly, without a shell.
    fn program(argv: &[&str]) -> Command {
        Command {
            arguments: argv.iter().map(|a| (*a).to_owned()).collect(),
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
    async fn stdout_goes_to_its_file() {
        let rig = Rig::new("stdout");
        let ran = rig
            .run(&program(&["/bin/echo", "out"]), LONG)
            .await
            .expect("ran");
        assert_eq!(ran.exit_code, 0);
        assert!(!ran.timed_out);
        assert_eq!(rig.out(), "out\n");
        assert_eq!(rig.err(), "");
    }

    /// The text of the message is the program's and its locale's, so only that it reached the
    /// stderr file, and not the stdout file, is asserted.
    #[tokio::test]
    async fn stderr_goes_to_its_file() {
        let rig = Rig::new("stderr");
        let ran = rig
            .run(&program(&["/bin/cat", "no-such-file"]), LONG)
            .await
            .expect("ran");
        assert_ne!(ran.exit_code, 0);
        assert!(!ran.timed_out);
        assert_eq!(rig.out(), "");
        assert!(!rig.err().is_empty());
    }

    #[tokio::test]
    async fn the_exit_code_is_reported() {
        // `env` exits 127 when it cannot find the program it was asked to run. Each run gets its
        // own work directory: a run refuses a `private` directory an earlier one left.
        for (argv, code) in [
            (vec!["/usr/bin/true"], 0),
            (vec!["/usr/bin/false"], 1),
            (vec!["/usr/bin/env", "./no-such-program"], 127),
        ] {
            let rig = Rig::new(&format!("exit-{code}"));
            let ran = rig.run(&program(&argv), LONG).await.expect("ran");
            assert_eq!(ran.exit_code, code, "{argv:?}");
            assert!(!ran.timed_out);
        }
    }

    #[tokio::test]
    async fn a_program_a_signal_ended_reports_128_plus_the_signal() {
        let rig = Rig::new("signal");
        // Process 0 is the sender's own group: the program leads its group, so only it ends.
        let ran = rig
            .run(&program(&["/bin/kill", "-TERM", "0"]), LONG)
            .await
            .expect("ran");
        assert_eq!(ran.exit_code, 128 + 15);
        assert!(!ran.timed_out);
    }

    #[tokio::test]
    async fn the_environment_is_the_commands_alone() {
        let rig = Rig::new("env");
        // Set in the parent of the program; the Command does not name it.
        let mut command = program(&["/usr/bin/env"]);
        command.environment_variables = vec![var("KBF_TASK_FROM_COMMAND", "yes")];
        // SAFETY: nothing else in this test binary reads or writes this variable.
        unsafe { std::env::set_var("KBF_TASK_ONLY_MINE", "leaked") };
        rig.run(&command, LONG).await.expect("ran");
        let out = rig.out();
        let names: Vec<&str> = out
            .lines()
            .map(|l| l.split_once('=').map_or(l, |(name, _)| name))
            .collect();
        assert!(
            out.lines().any(|l| l == "KBF_TASK_FROM_COMMAND=yes"),
            "{out}"
        );
        assert!(!names.contains(&"KBF_TASK_ONLY_MINE"), "{out}");
        // Only the run's own four are added to the Command's.
        for name in [
            "HOME",
            "TMPDIR",
            "XDG_CACHE_HOME",
            "CLANG_MODULE_CACHE_PATH",
        ] {
            assert!(names.contains(&name), "{name}: {out}");
        }
        assert_eq!(names.len(), 5, "{out}");
    }

    /// Prints the run's environment as a map.
    async fn env_of(rig: &Rig, command: &Command) -> std::collections::HashMap<String, String> {
        rig.run(command, LONG).await.expect("ran");
        rig.out()
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    /// Catches: a Command that gets no `HOME`, `TMPDIR`, `XDG_CACHE_HOME` or
    /// `CLANG_MODULE_CACHE_PATH`, or one that names a directory that does not exist or sits
    /// inside the input root (where a compiler's scratch would be packed as an output).
    #[tokio::test]
    async fn the_command_gets_its_own_home_tmp_and_cache_directories_outside_the_root() {
        let rig = Rig::new("homes");
        let env = env_of(&rig, &program(&["/usr/bin/env"])).await;
        let private = rig.private();
        for (name, sub) in [
            ("HOME", "home"),
            ("TMPDIR", "tmp"),
            ("XDG_CACHE_HOME", "cache"),
            ("CLANG_MODULE_CACHE_PATH", "cache/clang/ModuleCache"),
        ] {
            assert!(env.contains_key(name), "{name} is unset");
            let value = &env[name];
            let path = Path::new(value);
            assert_eq!(path, private.join(sub), "{name}");
            assert!(path.is_dir(), "{name}={value}");
            assert!(!path.starts_with(rig.root()), "{name}={value}");
            assert_eq!(
                std::fs::metadata(path).expect("meta").permissions().mode() & 0o777,
                0o700,
                "{name}"
            );
        }
    }

    /// Catches: the run's directories overriding a variable the Command sets itself (a build
    /// that points `TMPDIR` at its own scratch would write somewhere else).
    #[tokio::test]
    async fn a_variable_the_command_sets_keeps_the_commands_value() {
        let rig = Rig::new("override");
        let mut command = program(&["/usr/bin/env"]);
        command.environment_variables = vec![var("TMPDIR", "/own/tmp"), var("HOME", "/own/home")];
        let env = env_of(&rig, &command).await;
        assert_eq!(env["TMPDIR"], "/own/tmp");
        assert_eq!(env["HOME"], "/own/home");
        assert_eq!(
            Path::new(&env["XDG_CACHE_HOME"]),
            rig.private().join("cache")
        );
    }

    /// Catches: a `private` directory left by an earlier run being reused (its stale files
    /// seen by this Command) or written to, instead of the run failing as a run start.
    #[tokio::test]
    async fn an_existing_private_directory_is_refused_and_left_alone() {
        let rig = Rig::new("stale");
        std::fs::create_dir(rig.private()).expect("private");
        std::fs::write(rig.private().join("stale"), b"old").expect("stale");
        let error = rig
            .run(&program(&["/usr/bin/true"]), LONG)
            .await
            .expect_err("refused");
        assert!(matches!(error, RunError::Io { .. }), "{error}");
        assert!(error.to_string().contains("private"), "{error}");
        assert_eq!(
            std::fs::read(rig.private().join("stale")).expect("kept"),
            b"old"
        );
        assert_eq!(std::fs::read_dir(rig.private()).expect("dir").count(), 1);
        assert!(!rig.stdout.exists());
    }

    /// Catches: a `private` that is a symlink being followed, so the run's directories (and
    /// whatever the Command writes there) land in the symlink's target.
    #[tokio::test]
    async fn a_private_symlink_is_refused_and_its_target_untouched() {
        let rig = Rig::new("symlink");
        let target = TestDir::under(std::env::temp_dir(), "kbf-task-run-symlink-target");
        std::os::unix::fs::symlink(&*target, rig.private()).expect("link");
        let error = rig
            .run(&program(&["/usr/bin/true"]), LONG)
            .await
            .expect_err("refused");
        assert!(matches!(error, RunError::Io { .. }), "{error}");
        assert_eq!(std::fs::read_dir(&*target).expect("dir").count(), 0);
        assert!(!rig.stdout.exists());
    }

    /// Catches: an output path into the run's private directory being accepted, which would
    /// have the pack collect files from outside the input root.
    #[tokio::test]
    async fn an_output_path_out_of_the_root_is_refused_before_the_command_runs() {
        let rig = Rig::new("escape");
        let mut command = program(&["/usr/bin/true"]);
        command.output_paths = vec!["../private/tmp/x".into()];
        let error = rig.run(&command, LONG).await.expect_err("refused");
        assert!(matches!(error, RunError::Invalid(_)), "{error}");
        assert!(!rig.private().exists());
    }

    /// Catches: the run's directories made for a run that cannot have them (a file where the
    /// private directory must be) and the Command started anyway with variables naming nothing.
    #[tokio::test]
    async fn private_directories_that_cannot_be_made_are_an_io_error() {
        let rig = Rig::new("blocked");
        std::fs::write(rig.private(), b"in the way").expect("file");
        let error = rig
            .run(&program(&["/usr/bin/true"]), LONG)
            .await
            .expect_err("refused");
        assert!(matches!(error, RunError::Io { .. }), "{error}");
        assert!(!rig.stdout.exists());
    }

    #[tokio::test]
    async fn a_bare_program_name_is_found_on_the_commands_path() {
        let rig = Rig::new("path");
        let mut command = program(&["echo", "found"]);
        command.environment_variables = vec![var("PATH", "/bin:/usr/bin")];
        let ran = rig.run(&command, LONG).await.expect("ran");
        assert_eq!(ran.exit_code, 0);
        assert_eq!(rig.out(), "found\n");
    }

    #[tokio::test]
    async fn a_program_path_with_a_slash_is_relative_to_the_working_directory() {
        let rig = Rig::new("slash");
        let work = rig.root().join("sub");
        std::fs::create_dir_all(&work).expect("work");
        // Named `echo`: a multi-call build of the tools picks its behaviour from the name it is
        // run as. Nothing on the run's PATH (it sets none) could find it: only `./` does.
        std::os::unix::fs::symlink("/bin/echo", work.join("echo")).expect("link");
        let mut command = program(&["./echo", "ok"]);
        command.working_directory = "sub".into();
        rig.run(&command, LONG).await.expect("ran");
        assert_eq!(rig.out(), "ok\n");
    }

    #[tokio::test]
    async fn the_working_directory_is_made_and_the_program_runs_in_it() {
        let rig = Rig::new("cwd");
        let mut command = program(&["/bin/pwd", "-P"]);
        command.working_directory = "a/b".into();
        rig.run(&command, LONG).await.expect("ran");
        let expected = std::fs::canonicalize(rig.root().join("a/b")).expect("made");
        assert_eq!(rig.out().trim_end(), expected.to_str().expect("utf8"));
    }

    #[tokio::test]
    async fn the_parent_directories_of_outputs_are_made_under_the_working_directory() {
        let rig = Rig::new("parents");
        let mut command = program(&["/usr/bin/true"]);
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
        let mut command = program(&["/usr/bin/true"]);
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
        // `find` stays alive and runs `sleep` as its child; both must go.
        let command = program(&[
            "/usr/bin/find",
            ".",
            "-maxdepth",
            "0",
            "-exec",
            "/bin/sleep",
            "600",
            ";",
        ]);
        let ran = rig.run(&command, Duration::ZERO).await.expect("ran");
        assert!(ran.timed_out);
        assert_eq!(ran.exit_code, TIMED_OUT_EXIT_CODE);
    }

    #[tokio::test]
    async fn usage_is_reported() {
        let rig = Rig::new("usage");
        let ran = rig
            .run(&program(&["/bin/echo", "hi"]), LONG)
            .await
            .expect("ran");
        // A program that ran and was reaped has a resident set; the other figures are
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
            let mut command = program(&["/usr/bin/true"]);
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
            let mut command = program(&["/usr/bin/true"]);
            command.working_directory = work.into();
            let error = rig.run(&command, LONG).await.expect_err("refused");
            assert!(matches!(error, RunError::Invalid(_)), "{work}: {error}");
        }
        // The same for a directory an output needs.
        let mut command = program(&["/usr/bin/true"]);
        command.output_paths = vec!["link/out".into()];
        let error = rig.run(&command, LONG).await.expect_err("refused");
        assert!(matches!(error, RunError::Invalid(_)), "{error}");
    }

    /// Catches a kill of a child that never had a pid, or whose pid does not fit a `pid_t`,
    /// signalling anything or failing: both are nothing to kill.
    #[test]
    fn kill_group_does_nothing_without_a_usable_group() {
        kill_group(None);
        kill_group(Some(u32::MAX));
    }

    #[tokio::test]
    async fn a_directory_that_cannot_be_made_is_an_io_error() {
        let rig = Rig::new("mkdir");
        std::fs::set_permissions(
            rig.root(),
            std::os::unix::fs::PermissionsExt::from_mode(0o500),
        )
        .expect("chmod");
        let mut command = program(&["/usr/bin/true"]);
        command.working_directory = "new".into();
        let result = rig.run(&command, LONG).await;
        std::fs::set_permissions(
            rig.root(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("chmod back");
        // A privileged user ignores the mode; then the directory is made and nothing fails.
        assert!(
            result
                .err()
                .is_none_or(|error| matches!(error, RunError::Io { .. }))
        );
    }

    #[tokio::test]
    async fn a_stream_file_that_cannot_be_created_is_an_io_error() {
        let rig = Rig::new("streamfile");
        let root = rig.root();
        let missing = rig.dir.join("none/stdout");
        let error = run(
            &program(&["/usr/bin/true"]),
            &Run {
                root: &root,
                private: &rig.private(),
                stdout: &missing,
                stderr: &rig.stderr,
                deadline: LONG,
                network: NETWORK,
                unreadable: &[],
            },
        )
        .await
        .expect_err("refused");
        assert!(matches!(error, RunError::Io { .. }), "{error}");
    }

    /// Catches a Command whose program is missing being reported as a run. Where the program is
    /// started directly, that is a spawn error. On macOS `sandbox-exec` starts it and is what
    /// fails, so the run ends with a nonzero exit and the message in the Command's stderr.
    #[tokio::test]
    async fn a_program_that_does_not_exist_is_not_run() {
        let rig = Rig::new("spawn");
        let command = Command {
            arguments: vec!["./no-such-program".into()],
            ..Command::default()
        };
        let result = rig.run(&command, LONG).await;
        if cfg!(target_os = "macos") {
            let ran = result.expect("sandbox-exec ran");
            assert_ne!(ran.exit_code, 0);
            assert!(rig.err().contains("no-such-program"), "{}", rig.err());
        } else {
            let error = result.expect_err("refused");
            assert!(matches!(error, RunError::Spawn { .. }), "{error}");
        }
    }
}
