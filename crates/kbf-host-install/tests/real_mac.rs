//! The tools the Mac install reads, run for real: `launchctl print` of a throwaway
//! LaunchDaemon in each state the install tells apart, `lsof -F n` of a running process,
//! `codesign -d` of a binary signed ad hoc with the hardened runtime, `xattr` of a
//! quarantined file, `dscl` of `nobody`, and a privilege drop by the real runner; each output
//! is read with the crate's own parsers.
//!
//! These tests need a Mac and root, so they are `#[ignore]`d and run in the `host-install-mac`
//! workflow on a GitHub-hosted macOS runner through `cargo xtask sudo-test -p kbf-host-install
//! --test real_mac`. They write every raw output to `target/real-mac` (the workflow uploads
//! it): the files under `tests/fixtures/mac` are those outputs, and the ordinary unit tests
//! read them.
//!
//! Each test captures first and asserts after, so a parser that does not read the real
//! output still leaves the output to look at.

#![cfg(target_os = "macos")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use kbf_host_install::mac_tools::{Cmd, Exec, Identity, Runner, SystemRunner, tools};
use kbf_testkit::dir::TestDir;

const LIMIT: Duration = Duration::from_secs(30);
const WAIT: Duration = Duration::from_secs(60);

/// Where the raw outputs go.
fn out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/real-mac")
}

/// Write the raw output of a command: its stdout, its stderr (when it has any) and its
/// exit code.
fn capture(name: &str, exec: &Exec) {
    let dir = out_dir();
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{name}.stdout")), &exec.stdout).unwrap();
    if !exec.stderr.is_empty() {
        fs::write(dir.join(format!("{name}.stderr")), &exec.stderr).unwrap();
    }
    let code = exec.code.map_or("signal".to_owned(), |c| c.to_string());
    fs::write(dir.join(format!("{name}.code")), format!("{code}\n")).unwrap();
}

fn run(cmd: &Cmd) -> Exec {
    SystemRunner
        .run(cmd)
        .unwrap_or_else(|e| panic!("{} did not run: {e}", cmd.program))
}

fn assert_root() {
    assert!(
        rustix::process::geteuid().is_root(),
        "run through `cargo xtask sudo-test`"
    );
}

/// A throwaway LaunchDaemon in the system domain, booted out on drop.
struct Job {
    label: String,
    _dir: TestDir,
}

impl Job {
    /// Load `program` with `args`, started at load or not.
    fn load(scenario: &str, program: &str, args: &[&str], run_at_load: bool) -> Self {
        assert_root();
        let dir = TestDir::under("/private/var/tmp", &format!("kbf-real-mac-{scenario}"));
        let label = format!(
            "com.example.kbf-real-mac-test.{scenario}.{}",
            std::process::id()
        );
        let arguments: String = std::iter::once(program)
            .chain(args.iter().copied())
            .map(|a| format!("\t\t<string>{a}</string>\n"))
            .collect();
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n\
             \t<key>Label</key>\n\t<string>{label}</string>\n\
             \t<key>ProgramArguments</key>\n\t<array>\n{arguments}\t</array>\n\
             \t<key>RunAtLoad</key>\n\t<{run_at_load}/>\n\
             \t<key>KeepAlive</key>\n\t<false/>\n\
             </dict>\n</plist>\n"
        );
        let path = dir.join("job.plist");
        fs::write(&path, plist).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let job = Self { label, _dir: dir };
        let exec = run(&tools::bootstrap(&path, LIMIT));
        assert!(
            exec.success(),
            "bootstrap: {:?} {}",
            exec.code,
            exec.stderr_text()
        );
        job
    }

    fn print(&self) -> Exec {
        run(&tools::print(&self.label, LIMIT))
    }

    /// Poll `launchctl print` until `done` accepts its text; the text then. A timeout
    /// captures the last text as `<name>_timeout` and panics.
    fn wait(&self, name: &str, what: &str, done: impl Fn(&str) -> bool) -> Exec {
        let deadline = Instant::now() + WAIT;
        loop {
            let exec = self.print();
            if exec.success() && done(&exec.stdout_text()) {
                return exec;
            }
            if Instant::now() >= deadline {
                capture(&format!("{name}_timeout"), &exec);
                panic!("the job never {what}: {}", exec.stdout_text());
            }
            sleep(Duration::from_millis(200));
        }
    }

    fn bootout(&self) -> Exec {
        run(&tools::bootout(&self.label, LIMIT))
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        let _ = self.bootout();
    }
}

fn pid_of(text: &str) -> i32 {
    i32::try_from(tools::print_pid(text).expect("a running job has a pid")).unwrap()
}

/// A job that is loaded and has not started: not running, not finished, no process; after
/// `bootout` the label is gone and `print` fails.
#[test]
#[ignore = "needs a Mac and root: cargo xtask sudo-test"]
fn print_of_a_job_loaded_and_not_started() {
    let job = Job::load("idle", "/bin/sleep", &["600"], false);
    let exec = job.print();
    capture("print_idle", &exec);
    let text = exec.stdout_text();
    assert!(exec.success(), "{}", exec.stderr_text());
    assert!(!tools::print_running(&text), "{text}");
    assert!(!tools::print_finished(&text), "{text}");
    assert_eq!(tools::print_pid(&text), None, "{text}");

    let out = job.bootout();
    capture("bootout_idle", &out);
    // `bootout` may return before the label is gone: wait for the listing to fail.
    let deadline = Instant::now() + WAIT;
    let gone = loop {
        let exec = job.print();
        if !exec.success() {
            break exec;
        }
        assert!(Instant::now() < deadline, "the label stayed loaded");
        sleep(Duration::from_millis(200));
    };
    capture("print_missing", &gone);
    assert!(!gone.success());
}

/// A running job: `state = running`, a pid; `lsof -F n` of that pid names the program
/// file, and `kill -9` of it leaves a job that is finished, not running, with no pid.
#[test]
#[ignore = "needs a Mac and root: cargo xtask sudo-test"]
fn print_and_lsof_of_a_running_job_and_after_kill_nine() {
    let job = Job::load("running", "/bin/sleep", &["600"], true);
    let exec = job.wait("print_running", "ran", tools::print_running);
    capture("print_running", &exec);
    let text = exec.stdout_text();
    assert!(tools::print_running(&text), "{text}");
    assert!(!tools::print_finished(&text), "{text}");
    let pid = pid_of(&text);

    let open = run(&tools::lsof_text(u32::try_from(pid).unwrap(), LIMIT));
    capture("lsof_running", &open);
    let listing = open.stdout_text();
    let paths = tools::lsof_paths(&listing);
    assert!(
        paths.iter().any(|path| path.ends_with("/sleep")),
        "{listing}"
    );
    assert!(listing.starts_with(&format!("p{pid}\n")), "{listing}");

    rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid).unwrap(),
        rustix::process::Signal::KILL,
    )
    .unwrap();
    let exec = job.wait("print_killed", "ended", tools::print_finished);
    capture("print_killed", &exec);
    let text = exec.stdout_text();
    assert!(!tools::print_running(&text), "{text}");
    assert!(tools::print_finished(&text), "{text}");
    assert_eq!(tools::print_pid(&text), None, "{text}");
}

/// A job that exits 0 and one that exits 1 are both finished, not running.
#[test]
#[ignore = "needs a Mac and root: cargo xtask sudo-test"]
fn print_of_a_job_after_a_clean_exit_and_after_a_failure() {
    for (name, program, code) in [
        ("clean_exit", "/usr/bin/true", "0"),
        ("nonzero_exit", "/usr/bin/false", "1"),
    ] {
        let job = Job::load(name, program, &[], true);
        let exec = job.wait(&format!("print_{name}"), "ended", tools::print_finished);
        capture(&format!("print_{name}"), &exec);
        let text = exec.stdout_text();
        assert!(!tools::print_running(&text), "{name}: {text}");
        assert!(tools::print_finished(&text), "{name}: {text}");
        assert_eq!(tools::print_pid(&text), None, "{name}: {text}");
        assert!(
            text.contains(&format!("last exit code = {code}")),
            "{name}: {text}"
        );
    }
}

/// Copy this test binary to `dir` and sign it ad hoc, with the hardened runtime or without.
fn signed_copy(dir: &Path, name: &str, runtime: bool) -> PathBuf {
    let path = dir.join(name);
    fs::copy(std::env::current_exe().unwrap(), &path).unwrap();
    let mut sign = Command::new("/usr/bin/codesign");
    sign.args(["--force", "--sign", "-"]);
    if runtime {
        sign.args(["--options", "runtime"]);
    }
    let out = sign.arg(&path).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    path
}

/// `codesign --verify --strict` accepts a binary signed ad hoc; `codesign -d --verbose=4`
/// describes it on one of the two streams, and the code directory's flags list `runtime`
/// only when it was signed with the hardened runtime.
#[test]
#[ignore = "needs a Mac: cargo xtask sudo-test"]
fn codesign_of_a_hardened_and_a_plain_binary() {
    let dir = TestDir::under("/private/var/tmp", "kbf-real-mac-codesign");
    for (name, runtime) in [("hardened", true), ("plain", false)] {
        let path = signed_copy(&dir, name, runtime);
        let verify = run(&tools::codesign_verify(&path, LIMIT));
        capture(&format!("codesign_verify_{name}"), &verify);
        let describe = run(&tools::codesign_describe(&path, LIMIT));
        capture(&format!("codesign_describe_{name}"), &describe);
        assert!(verify.success(), "{name}: {}", verify.stderr_text());
        assert!(describe.success(), "{name}: {}", describe.stderr_text());
        // The stage reads both streams.
        let both = format!("{}\n{}", describe.stdout_text(), describe.stderr_text());
        assert_eq!(
            tools::has_hardened_runtime(&both),
            runtime,
            "{name}: {both}"
        );
    }
}

/// A file carrying the quarantine attribute is listed as such, and the attribute is gone
/// after the removal command.
#[test]
#[ignore = "needs a Mac: cargo xtask sudo-test"]
fn xattr_lists_and_removes_the_quarantine_attribute() {
    let dir = TestDir::under("/private/var/tmp", "kbf-real-mac-xattr");
    let path = dir.join("file");
    fs::write(&path, b"x").unwrap();
    let clean = run(&tools::xattr_list(&path, LIMIT));
    capture("xattr_clean", &clean);
    assert!(!tools::has_quarantine(&clean.stdout_text()));
    let set = Command::new("/usr/bin/xattr")
        .args(["-w", tools::QUARANTINE, "0081;00000000;Safari;"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(set.status.success());
    let listed = run(&tools::xattr_list(&path, LIMIT));
    capture("xattr_quarantined", &listed);
    assert!(tools::has_quarantine(&listed.stdout_text()));
    let removed = run(&tools::xattr_remove_quarantine(&path, LIMIT));
    capture("xattr_remove", &removed);
    assert!(removed.success(), "{}", removed.stderr_text());
    let after = run(&tools::xattr_list(&path, LIMIT));
    assert!(!tools::has_quarantine(&after.stdout_text()));
}

/// Not a test of its own: the process the privilege-drop test starts, which prints the
/// groups it belongs to (`getgroups`, not the account's memberships that `id` prints).
#[test]
#[ignore = "run by dscl_ids_of_nobody_and_a_drop_to_them, as another user"]
fn print_my_groups() {
    let groups: Vec<String> = rustix::process::getgroups()
        .unwrap()
        .iter()
        .map(|g| g.as_raw().to_string())
        .collect();
    println!("groups: {}", groups.join(" "));
}

/// The groups the test binary's copy belongs to when `run_as` starts it.
fn groups_of_child(copy: &Path, run_as: Option<Identity>) -> Vec<u32> {
    let mut probe = Cmd {
        program: copy.display().to_string(),
        args: ["--ignored", "--exact", "print_my_groups", "--nocapture"]
            .map(str::to_owned)
            .to_vec(),
        timeout: Some(LIMIT),
        ..Cmd::default()
    };
    probe.run_as = run_as;
    let out = run(&probe);
    capture(
        &format!(
            "groups_{}",
            if run_as.is_some() { "dropped" } else { "root" }
        ),
        &out,
    );
    assert!(out.success(), "{}", out.stderr_text());
    let text = out.stdout_text();
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix("groups:"))
        .unwrap_or_else(|| panic!("no groups line: {text}"));
    line.split_whitespace()
        .map(|g| g.parse().unwrap())
        .collect()
}

/// `dscl` gives the ids of `nobody`, the account the attestation check runs as, and the
/// runner's privilege drop to it leaves no supplementary group.
#[test]
#[ignore = "needs a Mac and root: cargo xtask sudo-test"]
fn dscl_ids_of_nobody_and_a_drop_to_them() {
    assert_root();
    let exec = run(&tools::dscl_ids("nobody", LIMIT));
    capture("dscl_nobody", &exec);
    assert!(exec.success(), "{}", exec.stderr_text());
    let text = exec.stdout_text();
    let ids = tools::parse_ids(&text);
    assert!(ids.is_some(), "the ids are not read from: {text}");
    let ids: Identity = ids.unwrap();
    assert_ne!(ids.uid, 0);

    // The test binary again, in a directory `nobody` can reach.
    let dir = TestDir::under("/private/var/tmp", "kbf-real-mac-groups");
    let copy = dir.join("probe");
    fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
    fs::set_permissions(&copy, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&*dir, fs::Permissions::from_mode(0o755)).unwrap();
    let as_root = groups_of_child(&copy, None);
    assert!(
        as_root.len() > 1,
        "root belongs to several groups: {as_root:?}"
    );
    let dropped = groups_of_child(&copy, Some(ids));
    assert!(
        dropped.iter().all(|g| *g == ids.gid),
        "only the primary group is left: {dropped:?}"
    );
}
