//! `kbf-guest` started as root, and under launchd: what the other tests, run as the
//! runner's user, cannot check.
//!
//! - Everywhere: started as root, every subcommand refuses and exits 2, before it
//!   creates anything.
//! - On macOS: started by launchd as a LaunchAgent in the sudo user's GUI session,
//!   `kbf-guest session` reports `Aqua`; as a LaunchDaemon in the system domain (as that
//!   user, since it refuses root) it reports `System`. The daemon is the control: an
//!   agent that always said `Aqua` would pass the first check alone.
//!
//! Every test is `#[ignore]`: each needs root. CI runs them with `cargo xtask sudo-test
//! -p kbf-guest --test as_root`, which builds this file as the runner's user and runs
//! the binary through `sudo -n` with `--ignored` (GitHub's hosted runners allow
//! passwordless sudo).

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-guest");

/// Stops a test run as the runner's user, where every check below would fail for the
/// wrong reason. Asks the kernel, not the code under test.
fn assert_root() {
    // SAFETY: geteuid takes no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    assert_eq!(
        euid, 0,
        "not root: run `cargo xtask sudo-test -p kbf-guest --test as_root`"
    );
}

/// A directory of one test's own (mode 0755), removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("kbf-guest-as-root-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        set_mode(&dir, 0o755);
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Root made it: leave no root-owned files in the runner's target directory.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// Runs `kbf-guest args`. A run still going after 10 seconds (a `serve` that skipped
/// the refusal and is accepting) is killed, so the checks after it still run.
fn run_bounded(args: &[std::ffi::OsString]) -> Output {
    let mut child = Command::new(BIN)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("ran kbf-guest");
    let give_up = Instant::now() + Duration::from_secs(10);
    while child.try_wait().expect("waited on kbf-guest").is_none() && Instant::now() < give_up {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    child.wait_with_output().expect("collected kbf-guest")
}

/// Catches the root refusal skipped, or reported with a status a supervisor reads as
/// success or as a crash: as root, `session` and `serve` each exit 2 with the refusal,
/// and `serve` does not create its socket first. The token file holds a valid token,
/// so a `serve` that skipped the refusal would bind its socket: the socket check
/// catches that on its own, before the exit status is read.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-guest --test as_root`"]
fn as_root_every_subcommand_refuses_and_exits_2() {
    assert_root();
    let s = Scratch::new("refuse");
    let socket = s.0.join("s");
    let token = s.0.join("t");
    std::fs::write(&token, "ab".repeat(kbf_guest::wire::TOKEN_LEN)).expect("token file");
    let serve = vec![
        "serve".into(),
        "--unix-socket".into(),
        socket.clone().into_os_string(),
        "--token-file".into(),
        token.into_os_string(),
        "--inputs".into(),
        s.0.clone().into_os_string(),
        "--outputs".into(),
        s.0.clone().into_os_string(),
    ];
    for args in [vec!["session".into()], serve] {
        let out = run_bounded(&args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !socket.exists(),
            "kbf-guest {args:?} as root created its socket: {stderr}"
        );
        assert_eq!(
            out.status.code(),
            Some(2),
            "kbf-guest {args:?} as root: {stderr}"
        );
        assert!(
            stderr.contains("kbf-guest refuses to run as root"),
            "refused, but not as root: {stderr}"
        );
        assert!(out.stdout.is_empty(), "{args:?} printed {:?}", out.stdout);
    }
}

#[cfg(target_os = "macos")]
mod launchd {
    use std::os::unix::fs::chown;
    use std::path::Path;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::{BIN, Scratch, assert_root, set_mode};

    /// The user sudo was run by, from the variables sudo sets: uid, gid, name.
    fn sudo_user() -> (u32, u32, String) {
        let var = |name: &str| {
            std::env::var(name).unwrap_or_else(|_| panic!("{name} unset: run through sudo"))
        };
        let id = |name: &str| var(name).parse::<u32>().expect(name);
        (id("SUDO_UID"), id("SUDO_GID"), var("SUDO_USER"))
    }

    fn launchctl(args: &[&str]) -> std::process::Output {
        Command::new("/bin/launchctl")
            .args(args)
            .output()
            .expect("ran launchctl")
    }

    /// A launchd job that runs `kbf-guest session` once at load, its stdout to `out`
    /// and its stderr to `out` with `.err` appended; as `user` when given.
    fn plist(label: &str, out: &Path, user: Option<&str>) -> String {
        let user = user.map_or(String::new(), |u| {
            format!("  <key>UserName</key><string>{u}</string>\n")
        });
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array><string>{BIN}</string><string>session</string></array>
  <key>StandardOutPath</key><string>{out}</string>
  <key>StandardErrorPath</key><string>{out}.err</string>
  <key>RunAtLoad</key><true/>
{user}</dict>
</plist>
"#,
            out = out.display()
        )
    }

    /// A job bootstrapped into a domain, booted out when the test ends.
    struct Job(String);

    impl Job {
        fn bootstrap(domain: &str, label: &str, plist: &Path) -> Job {
            let path = plist.display().to_string();
            let out = launchctl(&["bootstrap", domain, &path]);
            assert!(
                out.status.success(),
                "launchctl bootstrap {domain} {path}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            Job(format!("{domain}/{label}"))
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            let _ = launchctl(&["bootout", &self.0]);
        }
    }

    /// The job's output, once it has written it (up to 30 seconds), without the newline.
    fn wait_for(out: &Path) -> String {
        let give_up = Instant::now() + Duration::from_secs(30);
        loop {
            let text = std::fs::read_to_string(out).unwrap_or_default();
            if !text.is_empty() {
                return text.trim_end_matches('\n').to_owned();
            }
            assert!(
                Instant::now() < give_up,
                "{} was never written (stderr: {:?})",
                out.display(),
                std::fs::read_to_string(out.with_extension("out.err"))
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// The job `name` in a scratch directory the sudo user owns, so the job (which runs
    /// as that user) can write its output there; its plist written by root, mode 0644.
    fn job_files(name: &str, uid: u32, gid: u32, user: Option<&str>) -> (Scratch, String) {
        let s = Scratch::new(name);
        chown(&s.0, Some(uid), Some(gid)).expect("chown scratch");
        let label = format!("kbf.ci.guest-session.{name}");
        let out = s.0.join(format!("{name}.out"));
        let file = s.0.join(format!("{name}.plist"));
        std::fs::write(&file, plist(&label, &out, user)).expect("plist");
        set_mode(&file, 0o644);
        (s, label)
    }

    /// Catches an agent that does not ask launchd for its session, or asks the wrong
    /// way: started in the user's GUI session it must say `Aqua`, which the driver
    /// requires of a guest that runs UI tests.
    #[test]
    #[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-guest --test as_root`"]
    fn a_launch_agent_in_the_gui_session_reports_aqua() {
        assert_root();
        let (uid, gid, _) = sudo_user();
        let domain = format!("gui/{uid}");
        assert!(
            launchctl(&["print", &domain]).status.success(),
            "no {domain} domain: this runner has no GUI session"
        );
        let (s, label) = job_files("agent", uid, gid, None);
        let _job = Job::bootstrap(&domain, &label, &s.0.join("agent.plist"));
        assert_eq!(wait_for(&s.0.join("agent.out")), "Aqua");
    }

    /// The control for the agent: catches a session that always reads `Aqua` (a
    /// constant, or the wrong process asked). A LaunchDaemon in the system domain, run
    /// as the sudo user, must say `System`.
    #[test]
    #[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-guest --test as_root`"]
    fn a_launch_daemon_reports_system() {
        assert_root();
        let (uid, gid, user) = sudo_user();
        let (s, label) = job_files("daemon", uid, gid, Some(&user));
        // launchd loads a system job only from a file root owns and no one else may
        // write: root wrote it, mode 0644.
        let _job = Job::bootstrap("system", &label, &s.0.join("daemon.plist"));
        assert_eq!(wait_for(&s.0.join("daemon.out")), "System");
    }
}
