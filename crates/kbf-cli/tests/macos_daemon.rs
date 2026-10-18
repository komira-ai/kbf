//! `kbf daemon --driver native` as the Mac's launchd unit starts it: the `kbf` binary
//! with the words the Mac install kit writes into the unit's `ProgramArguments`
//! (`daemon_args` in the kit's script), against a fake front that stands where the
//! frontend does, and the helpers a macOS daemon re-runs its own binary as (the keeper
//! of each action; the `as-user` step of the lease-user pool), which have to work
//! through `kbf` now that the unit runs `kbf`, not `kbf-daemon`.
//!
//! The tests of `kbf-node` run these paths through the `kbf-daemon` binary, and
//! `src/tests.rs` pins that `kbf daemon` parses the flags as the old binary did. This
//! file is what runs `kbf` itself on macOS: its `main` has to hand a helper's command
//! line to the helper before clap reads it as a mode, and the unit's words have to start
//! the daemon.

#![cfg(target_os = "macos")]

// The fake front kbf-node's tests share; this file uses a part of it.
#[allow(dead_code)]
#[path = "../../kbf-node/tests/front/mod.rs"]
mod front;

use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use kbf_daemon::keeper::{self, Mode};
use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};

const BIN: &str = env!("CARGO_BIN_EXE_kbf");

/// How long the test waits for what should happen promptly.
const PROMPT: Duration = Duration::from_secs(60);

/// A child process, killed and reaped when dropped if it has not ended, so a failed
/// assertion leaves no daemon running.
struct Guarded(Child);

impl Drop for Guarded {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        // The process may have ended between the two calls; a destructor that panics
        // during a panic aborts the test binary.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A scratch directory for one test, with a CA, a node certificate and the fake
/// front's certificate (for `localhost`) in it.
fn tls(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-macos-daemon")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
    std::fs::write(dir.join("ca.pem"), ca.pem()).expect("write");
    for (name, dns) in [("node", "node-1"), ("server", "localhost")] {
        let key = KeyPair::generate().expect("key");
        let cert = CertificateParams::new(vec![dns.to_owned()])
            .expect("params")
            .signed_by(&key, &ca)
            .expect("sign");
        std::fs::write(dir.join(format!("{name}.pem")), cert.pem()).expect("write");
        // Private to this user: `kbf`'s startup self-checks refuse a key the group or
        // others can read, and the default umask leaves it 0644.
        let key_path = dir.join(format!("{name}.key"));
        std::fs::write(&key_path, key.serialize_pem()).expect("write");
        std::fs::set_permissions(
            &key_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .expect("mode");
    }
    dir
}

/// The words after the program in the unit's `ProgramArguments`, as the kit's
/// `daemon_args` makes them (one word per argument, flag and value apart; no
/// `--tls-server-name`: the server is named by DNS name, which the certificate carries),
/// for a front on `port` of `localhost`, the node's files in `dir`, and `data` as the
/// data directory.
fn unit_args(dir: &Path, data: &Path, port: u16) -> Vec<String> {
    let at = |name: &str| dir.join(name).display().to_string();
    let url = format!("https://localhost:{port}");
    [
        "daemon",
        "--driver",
        "native",
        "--server",
        &url,
        "--cas",
        &url,
        "--ca-cert",
        &at("ca.pem"),
        "--cert",
        &at("node.pem"),
        "--key",
        &at("node.key"),
        "--node-id",
        "node-1",
        "--data-dir",
        &data.display().to_string(),
        "--data-max-bytes",
        "4294967296",
        "--label",
        "pool=darwin-sized",
        "--label",
        "os=macos",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// Whether process `pid` has ended: gone, or a zombie its new parent has yet to reap.
fn ended(pid: i32) -> bool {
    let out = Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    stat.is_empty() || stat.starts_with(['Z', 'X', 'x'])
}

/// The pid the action wrote to `file`, waiting for it.
fn pid_in(file: &Path) -> i32 {
    let deadline = Instant::now() + PROMPT;
    loop {
        let pid = std::fs::read_to_string(file)
            .ok()
            .and_then(|text| text.strip_suffix('\n')?.parse().ok());
        if let Some(pid) = pid {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            file.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `kbf check-args` on `args`: the status the unit's words get before an installer
/// starts the unit.
fn check_args(args: &[String]) -> (Option<i32>, String) {
    let out = Command::new(BIN)
        .arg("check-args")
        .args(args)
        .output()
        .expect("run");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The unit's words are accepted by `kbf check-args daemon ...`, which is what an
/// installer asks before it loads a unit, and a flag the daemon does not take is not:
/// the same words with `--data-dir` spelled as another flag exit 2.
///
/// Catches a `kbf daemon` that no longer takes a flag the unit passes (a flag renamed or
/// dropped in the node's flags while the unit still writes it: launchd would restart the
/// daemon every ten seconds, each start exiting 2), and a `check-args` that accepts
/// everything.
#[test]
fn check_args_accepts_the_units_words_and_refuses_a_renamed_flag() {
    let dir = tls("check-args");
    let args = unit_args(&dir, &dir.join("data"), 8981);
    let (code, err) = check_args(&args);
    assert_eq!(code, Some(0), "{err}");

    let renamed: Vec<String> = args
        .iter()
        .map(|a| if a == "--data-dir" { "--datadir" } else { a }.to_owned())
        .collect();
    let (code, err) = check_args(&renamed);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--datadir"), "{err}");
}

/// A keeper started from the `kbf` binary answers as a keeper (its READY line) and ends
/// the process group it is the leader of when its stdin closes: `kbf __keeper group`
/// reaches the keeper before clap reads `__keeper` as a mode.
///
/// Catches a `kbf` `main` that does not call `kbf_node::dispatch_helpers()` first: clap
/// refuses `__keeper` (exit 2, nothing on stdout), and every action a `kbf daemon` runs
/// is refused at the keeper's start.
#[test]
fn kbf_answers_as_a_keeper() {
    let mut child = keeper::command(Path::new(BIN), &Mode::Group)
        .process_group(0)
        .spawn()
        .map(Guarded)
        .expect("spawn");
    let started = keeper::started(&mut child.0);
    drop(child.0.stdin.take());
    let status = child.0.wait().expect("wait");
    started.expect("the binary answered as a keeper");
    // The group's SIGKILL ends the keeper itself.
    assert_eq!(status.signal(), Some(9), "{status}");
}

/// The lease-user pool's `as-user` step, `kbf __as-user ...`, reaches the code that drops
/// to a user: given no words it refuses with its own usage line (exit 126), not a usage
/// error of `kbf`'s (exit 2). The drop to a user itself needs root and is not run here.
///
/// Catches a `kbf` `main` whose helper dispatch leaves out the pool's step: an action
/// of a lease user would never start.
#[test]
fn kbf_answers_as_the_pools_as_user_step() {
    let out = Command::new(BIN).arg("__as-user").output().expect("run");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(126), "{err}");
    assert!(
        err.contains("kbf-mac-session as-user: usage: as-user <uid> <gid> -- <program> [args]"),
        "{err}"
    );
}

/// `kbf daemon` started with the unit's words, in the unit's working directory and with
/// its `HOME`, connects to the front, says `Hello` as node `node-1` offering the native
/// driver, runs the action the front starts, and, killed by SIGKILL, has its action ended
/// by the keeper it started: the `kbf` binary re-run as `__keeper`.
///
/// Catches a unit whose words the daemon refuses at start (no `Hello`), a daemon that
/// reports another node or driver than it was given, a `kbf` that cannot run its own
/// helpers (the action is refused at the keeper's start, so its files never appear), and
/// a keeper that does not end the action when the daemon is killed.
#[test]
fn kbf_daemon_starts_as_the_units_words_start_it() {
    let dir = tls("daemon");
    let data = dir.join("data");
    let front = front::Front::start(&dir);
    let action = front.user_shell("echo $$ > leader; sleep 300 & echo $! > child; wait");
    let log = dir.join("daemon.log");
    let mut daemon = Command::new(BIN)
        .args(unit_args(&dir, &data, front.worker.port()))
        .current_dir(&dir)
        .env("HOME", &dir)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).expect("log file"))
        .spawn()
        .map(Guarded)
        .expect("spawn");
    let logged = || std::fs::read_to_string(&log).unwrap_or_default();

    // A daemon that refuses to start never connects: say why, from its own log, rather
    // than only that the fake front waited out its connect.
    let session = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| front.session(PROMPT)))
        .unwrap_or_else(|_| panic!("the daemon did not connect; its log:\n{}", logged()));
    let hello = session.hello();
    assert_eq!(hello.node_id, "node-1", "{}", logged());
    let drivers: Vec<&str> = hello
        .capabilities
        .iter()
        .filter(|c| c.key == "drivers")
        .map(|c| c.value.as_str())
        .collect();
    assert_eq!(drivers, ["native"], "{}", logged());
    session.welcome();
    session.start(1, 1, action);

    let root = data.join("leases/lease-1-1/root");
    let (leader, child) = (pid_in(&root.join("leader")), pid_in(&root.join("child")));
    assert!(!ended(leader) && !ended(child), "the action runs");

    let pid = rustix::process::Pid::from_child(&daemon.0);
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).expect("SIGKILL");
    let status = daemon.0.wait().expect("wait");
    assert_eq!(status.signal(), Some(9), "{status}");
    let deadline = Instant::now() + PROMPT;
    while !(ended(leader) && ended(child)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let survivors: Vec<i32> = [leader, child].into_iter().filter(|&p| !ended(p)).collect();
    if let Some(group) = rustix::process::Pid::from_raw(leader).filter(|_| !survivors.is_empty()) {
        // A red run must not leave the action behind: the leader's group is the action's.
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    assert!(
        survivors.is_empty(),
        "processes of the action {survivors:?} outlived the killed daemon: {}",
        logged()
    );
}
