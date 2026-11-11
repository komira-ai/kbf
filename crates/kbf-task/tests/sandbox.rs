//! The Command of an action runs in mount and PID namespaces of its own (`src/sandbox.rs`).
//!
//! Each case runs the `socket_probe` helper as a real Command through `kbf_task::run::run` and
//! reads what it printed and left. Where the host does not let this process make the sandbox
//! (an unprivileged user namespace that AppArmor strips of its capabilities, as on many CI
//! hosts), a case that needs it says so and checks what the run reports instead;
//! `sandbox_is_made_strict` is the case that fails there. Nothing here needs a privilege, a
//! network, or a clock.

#![cfg(target_os = "linux")]

use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_proto::reapi::Command;
use kbf_task::network::Network;
use kbf_task::run::{Ran, Run, run};
use kbf_task::sandbox::Sandbox;
use kbf_testkit::dir::TestDir;

fn probe() -> &'static str {
    env!("CARGO_BIN_EXE_socket_probe")
}

/// A run's directories: the input root `in` and `private` (`private-<n>` for the n-th run after
/// the first: a run makes its own), under `scratch`, which is hidden as a task hides the parent
/// of its work directory, beside a grant file in it.
struct Rig {
    _dir: TestDir,
    scratch: PathBuf,
    runs: std::cell::Cell<usize>,
    /// The Command is started through the relay, in a PID namespace of its own.
    relayed: bool,
}

impl Rig {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), &format!("kbf-task-sandbox-{name}"));
        let scratch = std::fs::canonicalize(&*dir).expect("real path");
        std::fs::create_dir_all(scratch.join("in")).expect("root");
        std::fs::write(scratch.join("grant"), b"token").expect("grant");
        Self {
            _dir: dir,
            scratch,
            runs: std::cell::Cell::new(0),
            relayed: true,
        }
    }

    fn root(&self) -> PathBuf {
        self.scratch.join("in")
    }

    /// The private directory of the last run.
    fn private(&self) -> PathBuf {
        match self.runs.get() {
            0 | 1 => self.scratch.join("private"),
            n => self.scratch.join(format!("private-{}", n - 1)),
        }
    }

    async fn run(&self, network: Network, args: &[&str]) -> (Ran, String) {
        let command = Command {
            arguments: std::iter::once(probe())
                .chain(args.iter().copied())
                .map(str::to_owned)
                .collect(),
            ..Command::default()
        };
        self.runs.set(self.runs.get() + 1);
        let (root, stdout) = (self.root(), self.scratch.join("stdout"));
        let ran = run(
            &command,
            &Run {
                root: &root,
                private: &self.private(),
                stdout: &stdout,
                stderr: &self.scratch.join("stderr"),
                deadline: Duration::from_secs(60),
                network,
                unreadable: &[],
                hidden: std::slice::from_ref(&self.scratch),
                relay: self
                    .relayed
                    .then(|| Path::new(env!("CARGO_BIN_EXE_kbf-task"))),
            },
        )
        .await
        .expect("the probe runs");
        assert!(!ran.timed_out);
        (ran, std::fs::read_to_string(stdout).expect("stdout"))
    }
}

/// Whether the run's sandbox was made; if not, says why the case's checks are skipped and checks
/// that the report names a step the host refused.
fn made(ran: &Ran, case: &str) -> bool {
    match &ran.sandbox {
        Sandbox::Made => true,
        Sandbox::Incomplete { step, errno } => {
            eprintln!("skipped {case}: this host refuses the sandbox ({step} {errno})");
            assert_ne!(*errno, 0);
            false
        }
        other => panic!("{case}: {other:?}"),
    }
}

/// Catches: a run whose sandbox is not reported, or reported made when the host refused a
/// step. The report names the step and its errno, and a Command still runs either way.
#[tokio::test]
async fn the_run_says_how_its_sandbox_came_out() {
    for network in [Network::Denied, Network::Allowed] {
        let rig = Rig::new("report");
        let (ran, _) = rig.run(network, &["touch", "ran"]).await;
        assert_eq!(ran.exit_code, 0);
        assert!(rig.root().join("ran").exists());
        assert!(
            matches!(ran.sandbox, Sandbox::Made | Sandbox::Incomplete { .. }),
            "{:?}",
            ran.sandbox
        );
    }
}

/// Catches: the grants or the task's run directory (its stdout and stderr files, everything but
/// the input root and `private/`) visible to the Command, or the input root not visible at its
/// own path. The hidden directory shows only the two the run binds again.
#[tokio::test]
async fn the_command_cannot_see_the_hidden_directories() {
    for network in [Network::Denied, Network::Allowed] {
        let rig = Rig::new("hidden");
        let grant = rig.scratch.join("grant");
        let (ran, out) = rig
            .run(network, &["ls", rig.scratch.to_str().expect("utf-8")])
            .await;
        if !made(&ran, "the_command_cannot_see_the_hidden_directories") {
            continue;
        }
        assert_eq!(ran.exit_code, 0);
        assert_eq!(out, "in\nprivate\n", "{network:?}");
        let (ran, _) = rig
            .run(network, &["read", grant.to_str().expect("utf-8")])
            .await;
        assert_eq!(ran.exit_code, libc::ENOENT, "{network:?}");
        // The input root is still there, and the run's directories under it are its own.
        let marker = rig.root().join("seen");
        let (ran, _) = rig
            .run(network, &["touch", marker.to_str().expect("utf-8")])
            .await;
        assert_eq!(ran.exit_code, 0);
        assert!(marker.exists());
    }
}

/// Catches: `/tmp`, `/var/tmp`, `/run` or `/root` left as the job has them (a small `noexec`
/// tmpfs, or a path the Command may not write), or bound to anything but the run's own
/// directories: a program written to each runs, and its bytes are under `private/`.
#[tokio::test]
async fn the_command_writes_and_runs_a_program_from_tmp_var_tmp_run_and_root() {
    for (target, sub) in [
        ("/tmp", "tmp"),
        ("/var/tmp", "var-tmp"),
        ("/run", "run"),
        ("/root", "home"),
    ] {
        if !Path::new(target).is_dir() {
            eprintln!("skipped {target}: this host has no such directory, so none is bound");
            continue;
        }
        let rig = Rig::new(&format!("exec-{sub}"));
        let (ran, _) = rig.run(Network::Denied, &["exec-from", target]).await;
        if !made(
            &ran,
            "the_command_writes_and_runs_a_program_from_tmp_var_tmp_run_and_root",
        ) {
            continue;
        }
        assert_eq!(ran.exit_code, 0, "{target}");
        let private = rig.private().join(sub);
        assert_eq!(
            std::fs::read(private.join("prog")).expect("prog"),
            std::fs::read(probe()).expect("probe"),
            "{target}"
        );
        assert!(private.join("ran").exists(), "{target}");
    }
}

/// Catches: a process the Command started (in a process group of its own, which the group kill
/// does not reach) still running when the run returns, or the Command being PID 1 of its
/// namespace (where a signal it sends itself is ignored). Where a PID namespace was made, the
/// Command is its second process and the process it left is gone by the time the run returns:
/// its lock is free. Where none was made, the case says so, and ends that process itself.
#[tokio::test]
async fn a_process_the_command_leaves_is_gone_when_the_run_returns() {
    let rig = Rig::new("linger");
    let (lock, pidfile) = (rig.root().join("lock"), rig.root().join("pid"));
    let (ran, _) = rig
        .run(
            Network::Denied,
            &[
                "linger",
                lock.to_str().expect("utf-8"),
                pidfile.to_str().expect("utf-8"),
            ],
        )
        .await;
    // Ended when this test ends, whichever way, unless it is known to be gone.
    let mut left = Leftover(
        std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|pid| pid.trim().parse().ok()),
    );
    assert_eq!(ran.exit_code, 0);
    assert!(
        left.0.is_some(),
        "the process the Command left wrote no pid"
    );
    let (_, pid) = rig.run(Network::Denied, &["pid"]).await;
    let file = std::fs::File::open(&lock).expect("lock");
    let free =
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok();
    if pid.trim() == "2" {
        assert!(free, "a process of the action outlived it");
        // Gone: its pid may already be another process's.
        left.0 = None;
    } else {
        eprintln!(
            "skipped: this host made no PID namespace ({:?}); the process is ended here",
            ran.sandbox
        );
    }
}

/// Catches: a run that is dropped before its Command ends (the task's future cancelled) leaving
/// the Command or a process it started running. The Command holds one lock and starts a child
/// that holds another and says so on a FIFO; once that has been read the run is dropped, and
/// each lock can then be taken, which happens only when its holder has died. Without the relay
/// (no PID namespace, the dropped child process alone is killed) only the kill of the Command's
/// process group ends the child.
#[tokio::test]
async fn a_dropped_run_ends_the_command_and_what_it_started() {
    for relayed in [true, false] {
        dropped_run(relayed).await;
    }
}

async fn dropped_run(relayed: bool) {
    let mut rig = Rig::new(&format!("dropped-{relayed}"));
    rig.relayed = relayed;
    let (own, lock, fifo) = (
        rig.root().join("own"),
        rig.root().join("lock"),
        rig.root().join("fifo"),
    );
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .expect("fifo");
    let args: Vec<String> = [
        "holds",
        &*own.to_string_lossy(),
        &*lock.to_string_lossy(),
        &*fifo.to_string_lossy(),
    ]
    .iter()
    .map(|a| (*a).to_owned())
    .collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // Boxed, so that dropping it below drops the run itself.
    let mut run = Box::pin(rig.run(Network::Denied, &args));
    // The Command and the descendant each hold the FIFO open for writing until they die; the
    // descendant writes its id on it once it holds its lock.
    let read = fifo.clone();
    let reader = tokio::task::spawn_blocking(move || {
        use std::io::BufRead as _;
        let mut fifo = std::io::BufReader::new(std::fs::File::open(read)?);
        let mut line = String::new();
        fifo.read_line(&mut line)?;
        Ok::<_, std::io::Error>((fifo, line))
    });
    let (mut fifo_end, pid) = tokio::select! {
        _ = &mut run => {
            // Let the reader's open return, so the test fails rather than waits.
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo);
            panic!("the run ended by itself");
        }
        read = reader => read.expect("reader").expect("the descendant's id"),
    };
    let mut left = Leftover(pid.trim().parse().ok());
    // The run is dropped while its Command and the descendant still run.
    drop(run);
    // The end of the FIFO: both have died.
    let rest = tokio::task::spawn_blocking(move || {
        let mut rest = String::new();
        std::io::Read::read_to_string(&mut fifo_end, &mut rest).map(|_| rest)
    })
    .await
    .expect("reader")
    .expect("the end of the FIFO");
    assert_eq!(rest, "", "relayed: {relayed}");
    for held in [own, lock] {
        let file = std::fs::File::open(&held).expect("lock");
        let free = rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive);
        assert!(
            free.is_ok(),
            "{} still held; relayed: {relayed}",
            held.display()
        );
    }
    // Gone: its pid may already be another process's.
    left.0 = None;
}

/// Catches: a program the first process cannot start being taken for a Command that ran: the
/// start report makes it the start error it is without a relay, `ENOENT` for a missing one.
#[tokio::test]
async fn a_program_the_relay_cannot_start_is_a_start_error() {
    let rig = Rig::new("missing");
    let root = rig.root();
    let error = run(
        &Command {
            arguments: vec!["./no-such-program".into()],
            ..Command::default()
        },
        &Run {
            root: &root,
            private: &rig.private(),
            stdout: &rig.scratch.join("stdout"),
            stderr: &rig.scratch.join("stderr"),
            deadline: Duration::from_secs(60),
            network: Network::Denied,
            unreadable: &[],
            hidden: &[],
            relay: Some(Path::new(env!("CARGO_BIN_EXE_kbf-task"))),
        },
    )
    .await
    .expect_err("a missing program does not run");
    match error {
        kbf_task::run::RunError::Spawn { program, source } => {
            assert_eq!(source.raw_os_error(), Some(libc::ENOENT), "{source}");
            assert_eq!(program, root.join("no-such-program"));
        }
        other => panic!("{other}"),
    }
}

/// A process a Command left, killed when dropped.
struct Leftover(Option<libc::pid_t>);

impl Drop for Leftover {
    fn drop(&mut self) {
        if let Some(pid) = self.0.and_then(rustix::process::Pid::from_raw) {
            // The process is the one the Command left; one already gone is nothing to kill.
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }
}

/// Catches: a Command in the sandbox's user namespace without `no_new_privs` or with a
/// capability, where it, or a file capability, would hold one there (enough to take a cover off). A Command allowed
/// the network has no seccomp filter, so this is the only place it gets it.
#[tokio::test]
async fn a_command_in_a_user_namespace_has_no_new_privs_even_with_the_network() {
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: root gets no user namespace, and keeps its privileges");
        return;
    }
    let rig = Rig::new("nnp");
    let (ran, nnp) = rig.run(Network::Allowed, &["nnp"]).await;
    let (_, pid) = rig.run(Network::Allowed, &["pid"]).await;
    if pid.trim() == "2" {
        assert_eq!(nnp, "1\n", "{:?}", ran.sandbox);
        // And no capability in any set, though it is the user namespace's own user.
        let (_, caps) = rig.run(Network::Allowed, &["caps"]).await;
        assert_eq!(caps, "0 0 0\n");
    } else {
        eprintln!("skipped: this host made no namespace ({:?})", ran.sandbox);
        assert_eq!(nnp, "0\n");
    }
}

/// The checks above (the hidden directory and grant, a program run from `/tmp`, the PID
/// namespace, `no_new_privs`), failing where the sandbox is not made. Run it where it is expected (root,
/// or a host that lets an unprivileged user namespace keep its capabilities):
///
/// `cargo test -p kbf-task --test sandbox -- --ignored sandbox_is_made_strict`
#[tokio::test]
#[ignore = "needs a host that lets this process make the sandbox; run with --ignored"]
async fn sandbox_is_made_strict() {
    let rig = Rig::new("strict");
    let (ran, out) = rig
        .run(
            Network::Denied,
            &["ls", rig.scratch.to_str().expect("utf-8")],
        )
        .await;
    assert_eq!(ran.sandbox, Sandbox::Made);
    assert_eq!(out, "in\nprivate\n");
    let grant = rig.scratch.join("grant");
    let (ran, _) = rig
        .run(Network::Denied, &["read", grant.to_str().expect("utf-8")])
        .await;
    assert_eq!(ran.exit_code, libc::ENOENT);
    let (ran, _) = rig.run(Network::Denied, &["exec-from", "/tmp"]).await;
    assert_eq!(ran.exit_code, 0);
    assert!(rig.private().join("tmp/ran").exists());
    let (_, pid) = rig.run(Network::Denied, &["pid"]).await;
    assert_eq!(pid, "2\n");
    if !rustix::process::geteuid().is_root() {
        let (_, nnp) = rig.run(Network::Allowed, &["nnp"]).await;
        assert_eq!(nnp, "1\n");
    }
}
