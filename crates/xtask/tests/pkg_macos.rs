//! kbf-installer's `.pkg` and cask on a real Mac: what Linux cannot show (contract
//! section 7).
//!
//! - `installer_places_files_and_loads_nothing` (root): the package is built from the
//!   ad hoc signed stub, opened by Apple's `pkgutil` and `lsbom` (which read the xar
//!   archive and the bill of materials the Rust writers made), installed by
//!   `installer`, and then every file and directory is asserted: owner root:wheel,
//!   mode, the bytes, the signature, no directory on the way to the tool writable by
//!   group or other, the state directory 0700, and nothing `setup` makes (users,
//!   releases, configuration). Then the gate: the LaunchDaemon is not loaded and no
//!   process runs; loaded by hand it stays not running while
//!   the `installer.toml` of kbf-installer's layout (`/usr/local/etc/kbf`) does not exist (the control that it is a real
//!   job: it runs once the file exists). Last, `check` refuses a user-owned prefix:
//!   the prefix is handed to a user, `check` must refuse and name it, and given back
//!   it must not. That step needs the real tool and is reported as skipped for the
//!   stub.
//! - `homebrew_cask_installs_and_uninstalls` (the runner's user, which is not root, as
//!   Homebrew insists): the cask from a throwaway tap installs the package, and `brew
//!   uninstall` unloads the job (loaded by hand first, so the stanza has something to
//!   do), forgets the receipt and removes every file the package placed.
//!
//! Both are `#[ignore]`: they change the machine. CI runs them on the macOS job:
//! `cargo xtask sudo-test -p xtask --test pkg_macos -- installer` and `cargo test -p
//! xtask --test pkg_macos --locked -- --ignored homebrew`.
//!
//! The binary packaged is `INSTALLER`, the stub (`xtask-fake` run as `kbf-installer`).
//! When the real kbf-installer builds on the runner, that constant is its path and
//! nothing else here changes.

#![cfg(target_os = "macos")]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use kbf_installer::layout::Platform;
use xtask::pkg::sha1::cksum;

mod support;
use kbf_testkit::dir::TestDir;
use support::output_unbusy;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
/// The binary the package ships: the stub, until the real one is built.
const INSTALLER: &str = env!("CARGO_BIN_EXE_xtask-fake");
/// The minimum macOS the CI job builds for (`MACOSX_DEPLOYMENT_TARGET` in ci.yml).
const MINOS: &str = "14.0";
const LABEL: &str = "com.komira.kbf-installer";
const VERSION: &str = "0.1.0";
/// The file the job's gate waits on, and its directory: where `kbf-installer setup` writes
/// `installer.toml` on a Mac, by kbf-installer's own layout (never a path spelled here, which
/// is how the gate and `setup` once disagreed unseen).
fn gate() -> &'static str {
    Platform::MacOs.installer_toml()
}
fn gate_dir() -> &'static str {
    Platform::MacOs.config_dir()
}
const LOG_DIR: &str = "/Library/Logs/kbf-installer";
const JOB_LOG: &str = "/Library/Logs/kbf-installer/kbf-installer.log";
const PLIST: &str = "/Library/LaunchDaemons/com.komira.kbf-installer.plist";
const PREFIX: &str = "/usr/local/kbf-installer";
const BINARY: &str = "/usr/local/kbf-installer/bin/kbf-installer";
const TRUST: &str = "/usr/local/kbf-installer/trust/trusted_root.jsonl";
/// The state directory, by the path the tool uses (`/var` is a link).
const STATE: &str = "/var/db/kbf-installer";
/// A user id no account has: the "user" that owns a prefix it should not.
const STRANGER: &str = "4242";
const PLIST_SOURCE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../packaging/macos/com.komira.kbf-installer.plist"
);
/// A system job that always runs: the control that `launchctl print` finds a job that
/// is loaded.
const LOADED_CONTROL: &str = "system/com.apple.logd";

fn run(program: &str, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run {program}: {e}"))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The stdout of `program args`, which must succeed.
fn success(program: &str, args: &[&str]) -> String {
    let out = run(program, args);
    assert!(
        out.status.success(),
        "{program} {args:?}: {}\n{}\n{}",
        out.status,
        text(&out.stdout),
        text(&out.stderr)
    );
    text(&out.stdout)
}

/// `program args` must exit non-zero.
fn failure(program: &str, args: &[&str]) {
    let out = run(program, args);
    assert!(
        !out.status.success(),
        "{program} {args:?} succeeded:\n{}",
        text(&out.stdout)
    );
}

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

/// An empty directory of its own for one test. One directory per test directly under
/// the target's tmp dir, not a shared parent: the `installer` test runs as root first
/// (`cargo xtask sudo-test`) and a shared parent it created would be root's, so the
/// `homebrew` test, run as the runner's user after it, could not create its own.
fn scratch(name: &str) -> TestDir {
    TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("xtask-pkg-macos-{name}"),
    )
}

/// The stub copied as `kbf-installer`, signed ad hoc as CI signs a release binary
/// (`cargo xtask sign-darwin`: hardened runtime, no entitlements, `--version` run).
fn signed_installer(dir: &Path) -> PathBuf {
    let binary = dir.join("kbf-installer");
    std::fs::copy(INSTALLER, &binary).expect("the installer binary");
    // `sign-darwin` runs the copy, and a copy this process just wrote is busy (`ETXTBSY`)
    // while a child another test thread forked still holds the write descriptor, until
    // that child execs. Run it plainly until it is not busy: once one exec succeeds the
    // descriptor is closed everywhere and no later holder can appear (the file is no
    // longer opened for writing), so `sign-darwin` cannot meet the race.
    output_unbusy(&mut Command::new(&binary)).expect("the copy runs once it is free");
    success(XTASK, &["sign-darwin", s(&binary), "--minos", MINOS]);
    binary
}

/// Builds the package into `out` and returns its path; the trust file it ships is
/// `trusted_root.jsonl` in `dir`.
fn build_pkg(dir: &Path, binary: &Path, out: &Path) -> PathBuf {
    let trust = dir.join("trusted_root.jsonl");
    std::fs::write(&trust, b"{\"trusted\":\"root\"}\n").expect("trust file");
    success(
        XTASK,
        &[
            "pkg",
            "build",
            "--binary",
            s(binary),
            "--launchd-plist",
            PLIST_SOURCE,
            "--trust",
            s(&trust),
            "--version",
            VERSION,
            "--out",
            s(out),
        ],
    );
    out.join(format!("kbf-installer-{VERSION}-macos-arm64.pkg"))
}

fn is_loaded(target: &str) -> bool {
    run("launchctl", &["print", target]).status.success()
}

/// Whether a process named `kbf-installer` runs.
fn is_running() -> bool {
    run("pgrep", &["-x", "kbf-installer"]).status.success()
}

fn mode_owner(path: &str) -> (u32, u32, u32) {
    let m = std::fs::metadata(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    (m.mode() & 0o7777, m.uid(), m.gid())
}

/// Every path the package installs, for the cleanup and the checks.
const INSTALLED: [&str; 3] = [BINARY, PLIST, TRUST];

/// Removes what the package installed and forgets its receipt, whatever a test did; at
/// the start of a test it also proves the machine is clean.
struct Installed;

impl Installed {
    fn clean() {
        let _ = run("launchctl", &["bootout", &format!("system/{LABEL}")]);
        let _ = run("pkgutil", &["--forget", LABEL]);
        for path in INSTALLED.iter().chain(&[gate(), JOB_LOG]) {
            let _ = std::fs::remove_file(path);
        }
        // Directories last, each only when empty: a test that left something in one
        // fails its own assertion rather than losing it here.
        for dir in [
            "/usr/local/kbf-installer/bin",
            "/usr/local/kbf-installer/trust",
            PREFIX,
            STATE,
            LOG_DIR,
            gate_dir(),
        ] {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        Installed::clean();
    }
}

fn assert_root() {
    assert!(
        rustix::process::geteuid().is_root(),
        "not root: run `cargo xtask sudo-test -p xtask --test pkg_macos -- installer`"
    );
}

/// Polls `f` until it holds. `sudo-test` runs the binary itself, not nextest, so no per-test
/// hang guard applies: a condition that never comes is ended by the workflow job's own
/// `timeout-minutes`. `waiting_for` is said on stderr every twenty polls, so that job's log
/// names what it was waiting for.
fn until(waiting_for: &str, f: impl Fn() -> bool) {
    let mut polls = 0u32;
    while !f() {
        polls += 1;
        if polls.is_multiple_of(20) {
            eprintln!("still waiting: {waiting_for}");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
#[ignore = "installs a package: needs root; CI runs it through sudo-test"]
fn installer_places_files_and_loads_nothing() {
    assert_root();
    Installed::clean();
    let _clean = Installed;
    let dir = scratch("installer");
    let binary = signed_installer(&dir);
    let pkg = build_pkg(&dir, &binary, &dir.join("out"));
    // The same inputs, built again elsewhere, are the same bytes on this host too.
    let again = scratch("installer-again");
    let pkg2 = build_pkg(&dir, &binary, &again.join("out"));
    assert_eq!(
        std::fs::read(&pkg).unwrap(),
        std::fs::read(&pkg2).unwrap(),
        "the package is not deterministic"
    );

    // Apple's tools open what the Rust writers made.
    let expanded = dir.join("expanded");
    success("pkgutil", &["--expand", s(&pkg), s(&expanded)]);
    for member in ["Bom", "PackageInfo", "Payload"] {
        assert!(expanded.join(member).exists(), "{member} is in the package");
    }
    assert!(!expanded.join("Scripts").exists());
    let bill = success("lsbom", &[s(&expanded.join("Bom"))]);
    let size = std::fs::metadata(&binary).unwrap().len();
    let sum = cksum(&std::fs::read(&binary).unwrap());
    let plist_bytes = std::fs::read(PLIST_SOURCE).unwrap();
    let lines: Vec<&str> = bill.lines().collect();
    for want in [
        ".\t40755\t0/0".to_owned(),
        "./usr/local/kbf-installer\t40755\t0/0".to_owned(),
        "./usr/local/kbf-installer/bin\t40755\t0/0".to_owned(),
        "./usr/local/kbf-installer/trust\t40755\t0/0".to_owned(),
        format!("./usr/local/kbf-installer/bin/kbf-installer\t100755\t0/0\t{size}\t{sum}"),
        "./private/var/db/kbf-installer\t40700\t0/0".to_owned(),
        format!(
            "./Library/LaunchDaemons/{LABEL}.plist\t100644\t0/0\t{}\t{}",
            plist_bytes.len(),
            cksum(&plist_bytes)
        ),
        "./Library/Logs/kbf-installer\t40755\t0/0".to_owned(),
    ] {
        assert!(lines.contains(&want.as_str()), "{want}\nlsbom:\n{bill}");
    }
    let listed = success("pkgutil", &["--payload-files", s(&pkg)]);
    assert!(
        listed.contains("usr/local/kbf-installer/bin/kbf-installer"),
        "{listed}"
    );

    // Installed.
    let out = success("installer", &["-pkg", s(&pkg), "-target", "/"]);
    assert!(out.contains("The install was successful"), "{out}");
    assert_eq!(mode_owner(BINARY), (0o755, 0, 0));
    assert_eq!(mode_owner(PLIST), (0o644, 0, 0));
    assert_eq!(mode_owner(TRUST), (0o644, 0, 0));
    assert_eq!(mode_owner(LOG_DIR), (0o755, 0, 0));
    // The state directory is the installer's alone.
    assert_eq!(mode_owner(STATE), (0o700, 0, 0));
    // Contract section 3: the tree is root-owned, no link, nothing writable by group or
    // other, from the prefix to the tool and the trust files.
    for path in [
        PREFIX,
        "/usr/local/kbf-installer/bin",
        "/usr/local/kbf-installer/trust",
        BINARY,
        TRUST,
    ] {
        let m = std::fs::symlink_metadata(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert!(!m.file_type().is_symlink(), "{path} is a link");
        assert_eq!((m.uid(), m.gid()), (0, 0), "{path} is not root:wheel");
        assert_eq!(m.mode() & 0o022, 0, "{path} is writable by group or other");
    }
    for dir in [
        "/usr/local/kbf-installer/bin",
        "/usr/local/kbf-installer/trust",
    ] {
        assert_eq!(mode_owner(dir), (0o755, 0, 0), "{dir}");
    }
    assert_eq!(std::fs::read(PLIST).unwrap(), plist_bytes);
    assert_eq!(
        std::fs::read(BINARY).unwrap(),
        std::fs::read(&binary).unwrap()
    );
    assert_eq!(
        std::fs::read(TRUST).unwrap(),
        std::fs::read(dir.join("trusted_root.jsonl")).unwrap()
    );
    success("plutil", &["-lint", PLIST]);
    success("codesign", &["--verify", "--strict", BINARY]);
    let version = success(BINARY, &["--version"]);
    assert_eq!(version, "kbf-installer-stub --version\n");
    let info = success("pkgutil", &["--pkg-info", LABEL]);
    assert!(info.contains(&format!("version: {VERSION}")), "{info}");
    assert!(
        success("pkgutil", &["--files", LABEL])
            .contains("usr/local/kbf-installer/bin/kbf-installer")
    );
    // What `setup` makes does not exist: no configuration, no release, no user, and
    // the job has not written.
    assert!(!Path::new(gate()).exists());
    assert!(!Path::new(JOB_LOG).exists());
    assert!(!Path::new("/usr/local/kbf-installer/releases").exists());
    assert!(!Path::new("/usr/local/kbf-installer/current").exists());
    for user in ["kbf", "_kbf-fetch"] {
        failure("id", &["-u", user]);
    }

    // Nothing is loaded and nothing runs. The control: a job that is loaded is found.
    assert!(is_loaded(LOADED_CONTROL), "the control job is not loaded");
    assert!(!is_loaded(&format!("system/{LABEL}")), "the job is loaded");
    assert!(!is_running());

    // Loaded by hand without the configuration: still not running.
    success("launchctl", &["bootstrap", "system", PLIST]);
    let state = success("launchctl", &["print", &format!("system/{LABEL}")]);
    assert!(state.contains("state = not running"), "{state}");
    // No wait stands between bootstrap and these checks: the old three-second sleep raced
    // a clock, and `launchctl` offers no event for "launchd evaluated the gate and left the
    // job down" to wait on instead. What this step proves is therefore the state at that
    // instant (a job that ignored the gate and started late is not seen here). That defect
    // is closed from the other side: `cargo xtask pkg build` refuses a property list not
    // gated on the file and kbf-setup's `setup::tests` pins the gate path, and the step
    // below shows the gate is what starts the job.
    assert!(!is_running());
    assert!(
        !Path::new(JOB_LOG).exists(),
        "the job ran without {}",
        gate()
    );
    // With it, it runs: the job's output is the stub's arguments. The file is made by hand
    // because the package holds the stub, which has no `setup`; its path is the one real
    // `setup` writes (kbf-setup's `setup::tests` pins that the plist's gate is it).
    std::fs::create_dir_all(gate_dir()).unwrap();
    std::fs::write(gate(), b"# configured\n").unwrap();
    until(&format!("the job to run once {} exists", gate()), || {
        std::fs::read_to_string(JOB_LOG).is_ok_and(|log| log.contains("kbf-installer-stub run"))
    });
    success("launchctl", &["bootout", &format!("system/{LABEL}")]);
    std::fs::remove_file(gate()).unwrap();

    check_refuses_a_user_owned_prefix();
}

/// Contract section 7: "a user-owned prefix is refused by `check`". The prefix is
/// handed to a user: `check` must fail and name the prefix; given back to root it must
/// not name it (the same binary, the same path, only the owner differs, so what the
/// refusal is about cannot be anything else). `check` lists every problem, and on this
/// node it has others (no `installer.toml`), which is why the assertion is on the
/// prefix being named, not on the exit code alone.
///
/// Needs the real `check`: the stub only prints its arguments, so for it the step is
/// reported as skipped and not passed.
fn check_refuses_a_user_owned_prefix() {
    if success(BINARY, &["--version"]).starts_with("kbf-installer-stub") {
        eprintln!(
            "SKIPPED check_refuses_a_user_owned_prefix: BINARY is the stub, the step \
             waits on the real kbf-installer (contract section 2, `check`)"
        );
        return;
    }
    let named =
        |out: &Output| text(&out.stdout).contains(PREFIX) | text(&out.stderr).contains(PREFIX);
    success("chown", &["-R", STRANGER, PREFIX]);
    let refused = run(BINARY, &["check"]);
    success("chown", &["-R", "0:0", PREFIX]);
    assert!(
        !refused.status.success() & named(&refused),
        "check accepted a prefix owned by uid {STRANGER}: {}\n{}",
        text(&refused.stdout),
        text(&refused.stderr)
    );
    let fine = run(BINARY, &["check"]);
    assert!(
        !named(&fine),
        "check names {PREFIX} though root owns it: {}\n{}",
        text(&fine.stdout),
        text(&fine.stderr)
    );
}

/// A throwaway tap named `name` under `komira`, removed when dropped.
struct Tap(String);

impl Tap {
    fn new(name: &str) -> Tap {
        let full = format!("komira/{name}");
        let _ = run("brew", &["untap", &full]);
        success("brew", &["tap-new", "--no-git", &full]);
        Tap(full)
    }

    fn dir(&self, kind: &str) -> PathBuf {
        let repo = success("brew", &["--repository", &self.0]);
        let dir = PathBuf::from(repo.trim()).join(kind);
        std::fs::create_dir_all(&dir).expect("tap directory");
        dir
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        let _ = run("brew", &["untap", &self.0]);
    }
}

fn file_url(p: &Path) -> String {
    format!("file://{}", s(p))
}

#[test]
#[ignore = "installs through Homebrew: not root; CI runs it on the macOS job"]
fn homebrew_cask_installs_and_uninstalls() {
    assert!(
        !rustix::process::geteuid().is_root(),
        "Homebrew refuses to run as root"
    );
    // The machine starts clean: the root test removed what it installed.
    failure("pkgutil", &["--pkg-info", LABEL]);
    for path in INSTALLED {
        assert!(!Path::new(path).exists(), "{path} is already there");
    }
    let dir = scratch("homebrew");
    let binary = signed_installer(&dir);
    let pkg = build_pkg(&dir, &binary, &dir.join("out"));

    // The cask.
    let tap = Tap::new("kbf-cask-test");
    let cask = tap.dir("Casks").join("kbf-installer.rb");
    success(
        XTASK,
        &[
            "pkg",
            "cask",
            "--pkg",
            s(&pkg),
            "--version",
            VERSION,
            "--url",
            &file_url(&pkg),
            "--out",
            s(&cask),
        ],
    );
    let cask_name = format!("{}/kbf-installer", tap.0);
    success("brew", &["install", "--cask", &cask_name]);
    for path in INSTALLED {
        assert!(Path::new(path).exists(), "{path} was not installed");
    }
    success("pkgutil", &["--pkg-info", LABEL]);
    assert!(
        !is_loaded(&format!("system/{LABEL}")),
        "the cask loaded the job"
    );
    // Loaded by hand, so the `launchctl:` stanza has a job to unload.
    success("sudo", &["-n", "launchctl", "bootstrap", "system", PLIST]);
    assert!(is_loaded(&format!("system/{LABEL}")));
    success("brew", &["uninstall", "--cask", &cask_name]);
    assert!(
        !is_loaded(&format!("system/{LABEL}")),
        "uninstall left the job loaded"
    );
    failure("pkgutil", &["--pkg-info", LABEL]);
    for path in INSTALLED {
        assert!(!Path::new(path).exists(), "uninstall left {path}");
    }
}
