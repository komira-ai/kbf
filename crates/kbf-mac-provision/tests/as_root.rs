//! `kbf-mac-provision` run as root on a hosted macOS runner: what a virtual machine
//! that no later job reuses can prove, and what the fake Mac of the other tests cannot
//! (docs/design/mac-node-provisioning.md, section 10).
//!
//! The profile is the example profile with its check-only values (macOS, FileVault,
//! the Xcodes, the default developer directory, a probe) read from this runner, so
//! those items must pass as they are; the host name is `kbf-mac-01`, the role id the
//! first id from 480 that no user or group holds, the time server Apple's.
//!
//! - **Applied and asserted:** the names, the role account, the software update and
//!   App Store preferences, the sshd drop-in, the firewall, the LaunchDaemon and the
//!   default developer directory (changed only when `xcode-select` points through a
//!   link). The first apply changes the role account and the LaunchDaemon; a second
//!   apply prints nothing; check passes and the job is loaded; two settings drifted by
//!   hand fail check with exactly two named FAIL lines, and the next apply fixes them;
//!   restart empties the error log and the job is loaded again; the log directory is
//!   root's, mode 755, and the error log the role account's, mode 644, one link; with a
//!   link planted at the error log, check fails, restart refuses with exit 2, apply
//!   replaces the link and the file it named keeps its bytes; `sshd -T` reports the
//!   drop-in's values.
//! - **Asserted as read:** `macos_version`, `macos_build`, `filevault`, `xcode` and
//!   `probe` pass before any apply; `simulator_runtimes` fails exactly when `xcrun
//!   simctl list runtimes` lists a runtime.
//! - **Applied, printed, not asserted:** the `power_*` keys (a VM may accept and ignore
//!   `pmset`).
//! - **Read and printed only:** `admin_user`, `remote_login`, `autologin`,
//!   `time_server`, `time_max_offset_ms` (Remote Login needs Full Disk Access; the clock
//!   and the runner's own login are left alone).
//!
//! No kbf-daemon is built for these tests: they assert that the job is loaded, never
//! that the daemon runs. When nothing is installed at the daemon's path, a copy of
//! `/usr/bin/true` stands there, so `launchctl bootstrap` has a program to start.
//!
//! Every test is `#[ignore]`: each needs root and changes this machine. CI runs them
//! last in `native-macos` with `cargo xtask sudo-test -p kbf-mac-provision --test
//! as_root -- --test-threads=1 --nocapture`, which builds this file as the runner's user
//! and runs it through `sudo -n` (GitHub's hosted runners allow passwordless sudo). The
//! tests also take one lock, so they never overlap, and each sets up what it needs
//! from the converged state the first of them reaches; what the check said before the
//! first apply is kept for the tests that read it. `--nocapture` shows every run and
//! the check of every item before and after apply in the job log. The file builds on
//! every platform, so clippy reads it on Linux; run anywhere but macOS, each test stops
//! at its first line.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use sha2::{Digest as _, Sha256};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-mac-provision");
/// The example profile the test profile is made from.
const EXAMPLE: &str = include_str!("../example.profile");
/// The tests' own directory: the profile, the probe, a file a link names. Root's.
const SCRATCH: &str = "/private/tmp/kbf-mac-provision-as-root";
const LABEL: &str = "org.example.kbf-daemon";
const LOG_DIR: &str = "/Library/Logs/kbf";
const ERR_LOG: &str = "/Library/Logs/kbf/kbf-daemon.err.log";
const DAEMON_BIN: &str = "/usr/local/kbf/current/kbf";
const SOFTWARE_UPDATE: &str = "/Library/Preferences/com.apple.SoftwareUpdate";
/// The items applied and asserted: none can break the job running the tests.
const SAFE: &str = "hostname,role_user,updates_auto_download,updates_auto_install_macos,\
updates_auto_install_security,updates_config_data,appstore_auto_update,ssh_password_auth,\
firewall,launchd_label,default_developer_dir";
/// Applied and printed, not asserted: a VM may accept and ignore `pmset`.
const POWER: &str = "power_sleep,power_standby,power_powernap,power_autorestart,power_womp";
/// The check-only items that must pass as this runner reads them.
const READ: &[&str] = &[
    "macos_version",
    "macos_build",
    "filevault",
    "xcode",
    "probe",
];

/// What one run of the binary printed, and its exit status.
struct Run {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Runs `kbf-mac-provision args` and prints what it printed.
fn prov(args: &[&str]) -> Run {
    let o = Command::new(BIN)
        .args(args)
        .output()
        .expect("ran kbf-mac-provision");
    let run = Run {
        status: o.status.code(),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    };
    println!("$ kbf-mac-provision {}", args.join(" "));
    for line in run.stdout.lines().chain(run.stderr.lines()) {
        println!("  | {line}");
    }
    println!("  exit {:?}", run.status);
    run
}

/// The trimmed stdout of a macOS program that must succeed.
fn output(program: &str, args: &[&str]) -> String {
    let o = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{program}: {e}"));
    assert!(
        o.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_owned()
}

/// Whether launchd has the job loaded in the system domain.
fn loaded() -> bool {
    let target = format!("system/{LABEL}");
    Command::new("/bin/launchctl")
        .args(["print", target.as_str()])
        .output()
        .expect("ran launchctl")
        .status
        .success()
}

/// Stops a run as the runner's user, where every check below would fail for the wrong
/// reason. Asks the kernel, not the code under test.
fn assert_root() {
    if cfg!(not(target_os = "macos")) {
        panic!("these tests run on macOS only");
    }
    assert!(
        rustix::process::geteuid().is_root(),
        "not root: run `cargo xtask sudo-test -p kbf-mac-provision --test as_root`"
    );
}

/// The first id from `from` that no user and no group holds.
fn free_id(from: u32) -> u32 {
    let taken: BTreeSet<u32> = [("/Users", "UniqueID"), ("/Groups", "PrimaryGroupID")]
        .into_iter()
        .flat_map(|(dir, attr)| {
            output("/usr/bin/dscl", &[".", "-list", dir, attr])
                .lines()
                .filter_map(|line| line.split_whitespace().nth(1)?.parse().ok())
                .collect::<Vec<u32>>()
        })
        .collect();
    (from..).find(|id| !taken.contains(id)).expect("a free id")
}

/// The pinned Xcodes: one `xcode=` line for each `Xcode*.app` directory in
/// `/Applications` (links skipped, as the item skips them), by name. A second app of a
/// build already pinned is left out, and shows as unpinned.
fn xcode_lines() -> String {
    let mut names: Vec<String> = fs::read_dir("/Applications")
        .expect("read /Applications")
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("Xcode") && name.ends_with(".app"))
        .collect();
    names.sort();
    let mut builds = BTreeSet::new();
    let mut lines = String::new();
    for name in names {
        let app = format!("/Applications/{name}");
        if !fs::symlink_metadata(&app).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let plist = format!("{app}/Contents/version.plist");
        let build = output(
            "/usr/libexec/PlistBuddy",
            &["-c", "Print :ProductBuildVersion", plist.as_str()],
        );
        if builds.insert(build.clone()) {
            lines.push_str(&format!(
                "xcode={build}:{app}/Contents/Developer:{}\n",
                "0".repeat(64)
            ));
        } else {
            println!("note: {app} repeats build {build}; it shows as unpinned");
        }
    }
    lines
}

/// `xcode-select -p`, through a link at the app resolved: then apply has the default
/// developer directory to change.
fn default_developer_dir() -> String {
    let dir = output("/usr/bin/xcode-select", &["-p"]);
    match dir.strip_suffix("/Contents/Developer") {
        Some(app) if fs::symlink_metadata(app).is_ok_and(|m| m.file_type().is_symlink()) => {
            let real = fs::canonicalize(app).expect("resolve the default Xcode");
            format!("{}/Contents/Developer", real.display())
        }
        _ => dir,
    }
}

/// The test profile: the example with this runner's check-only values.
fn profile_text(role_id: u32, probe: &Path) -> String {
    let filevault = output("/usr/bin/fdesetup", &["status"]);
    let filevault = if filevault.lines().next() == Some("FileVault is On.") {
        "on"
    } else {
        "off"
    };
    let admin = std::env::var("SUDO_USER").unwrap_or_else(|_| "runner".into());
    let mut text = String::new();
    for line in EXAMPLE.lines() {
        let dropped = [
            "#",
            "xcode",
            "probe=",
            "expect_probe=",
            "default_developer_dir=",
        ];
        if dropped.iter().any(|prefix| line.starts_with(prefix)) {
            continue;
        }
        let (key, _) = line.split_once('=').expect("KEY=VALUE");
        let value = match key {
            "macos_version" => output("/usr/bin/sw_vers", &["-productVersion"]),
            "macos_build" => output("/usr/bin/sw_vers", &["-buildVersion"]),
            "hostname" => "kbf-mac-01".into(),
            "admin_user" => admin.clone(),
            "role_id" => role_id.to_string(),
            "filevault" => filevault.into(),
            "time_server" => "time.apple.com".into(),
            _ => {
                text.push_str(line);
                text.push('\n');
                continue;
            }
        };
        text.push_str(&format!("{key}={value}\n"));
    }
    text.push_str(&xcode_lines());
    text.push_str(&format!(
        "default_developer_dir={}\n",
        default_developer_dir()
    ));
    let sha = hex::encode(Sha256::digest(fs::read(probe).expect("read the probe")));
    text.push_str(&format!("probe=host_identity:{}:{sha}\n", probe.display()));
    text
}

/// Puts a copy of `/usr/bin/true` at the daemon's path when nothing is there.
fn stand_in_daemon() {
    let daemon = Path::new(DAEMON_BIN);
    if daemon.exists() {
        return;
    }
    fs::create_dir_all(daemon.parent().expect("a parent")).expect("make the daemon's directory");
    fs::copy("/usr/bin/true", daemon).expect("copy /usr/bin/true");
    fs::set_permissions(daemon, fs::Permissions::from_mode(0o755)).expect("chmod the daemon");
}

/// The profile, and what check and the first apply printed, set up once.
struct Setup {
    profile: String,
    role_id: u32,
    /// `check` of every item before the first apply.
    before: Run,
    /// The first `apply --keys SAFE`.
    first: Run,
}

impl Setup {
    fn prepare() -> Setup {
        let _ = fs::remove_dir_all(SCRATCH);
        fs::create_dir_all(SCRATCH).expect("make the scratch directory");
        fs::set_permissions(SCRATCH, fs::Permissions::from_mode(0o755)).expect("chmod");
        let probe = Path::new(SCRATCH).join("probe.sh");
        fs::write(&probe, "echo identity\n").expect("write the probe");
        let role_id = free_id(480);
        let profile = Path::new(SCRATCH).join("profile");
        let text = profile_text(role_id, &probe);
        fs::write(&profile, &text).expect("write the profile");
        println!("== the profile");
        for line in text.lines() {
            println!("  | {line}");
        }
        stand_in_daemon();
        let profile = profile.to_str().expect("a UTF-8 path").to_owned();
        println!("== check, before apply (every item)");
        let before = prov(&["check", "--profile", profile.as_str()]);
        println!("== apply power (not asserted: a VM may ignore pmset)");
        prov(&["apply", "--profile", profile.as_str(), "--keys", POWER]);
        println!("== the first apply");
        let first = prov(&["apply", "--profile", profile.as_str(), "--keys", SAFE]);
        Setup {
            profile,
            role_id,
            before,
            first,
        }
    }

    fn run(&self, verb: &str, keys: Option<&str>) -> Run {
        let mut args = vec![verb, "--profile", self.profile.as_str()];
        if let Some(keys) = keys {
            args.extend(["--keys", keys]);
        }
        prov(&args)
    }
}

static SERIAL: Mutex<()> = Mutex::new(());
static HOST: OnceLock<Setup> = OnceLock::new();

/// The lock every test holds, and the host set up (by the first test to get here).
fn host() -> (MutexGuard<'static, ()>, &'static Setup) {
    let serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    assert_root();
    (serial, HOST.get_or_init(Setup::prepare))
}

/// Applies the profile again when a test that changed this machine by hand fails, so
/// its failure does not fail the tests after it.
struct Reconverge<'a>(&'a Setup);

impl Drop for Reconverge<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            println!("== reconverge after a failure");
            self.0.run("apply", Some(SAFE));
        }
    }
}

/// `(uid, gid, permission bits, links)` of `path` itself.
fn owner_mode_links(path: &str) -> (u32, u32, u32, u64) {
    let m = fs::symlink_metadata(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    (m.uid(), m.gid(), m.mode() & 0o7777, m.nlink())
}

/// Catches: `print` failing on a real profile, or a node label not made from the
/// profile's name.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-provision --test as_root`"]
fn print_names_the_node_label() {
    let (_serial, host) = host();
    let r = prov(&["print", "--profile", host.profile.as_str()]);
    assert_eq!(r.status, Some(0), "{}", r.stderr);
    assert!(
        r.stdout.contains("node_label=profile=mac-arm64-pool@"),
        "{}",
        r.stdout
    );
}

/// Catches: a check-only item that reads macOS otherwise than macOS answers (a program
/// path, an output format the fake gets wrong): each of [`READ`] passes before any
/// apply, with the values read from this runner.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-provision --test as_root`"]
fn read_only_items_pass_as_this_host_reads_them() {
    let (_serial, host) = host();
    let missing: Vec<&str> = READ
        .iter()
        .copied()
        .filter(|key| !host.before.stdout.contains(&format!("PASS {key}:")))
        .collect();
    assert!(
        missing.is_empty(),
        "not PASS: {missing:?}\n{}",
        host.before.stdout
    );
}

/// Catches: `simulator_runtimes` looking where macOS keeps no runtime, or missing one
/// it keeps: it fails exactly when `xcrun simctl list runtimes` lists a runtime.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-provision --test as_root`"]
fn simulator_runtimes_fails_exactly_when_simctl_lists_one() {
    let (_serial, host) = host();
    let o = Command::new("/usr/bin/xcrun")
        .args(["simctl", "list", "runtimes"])
        .output()
        .expect("ran xcrun simctl");
    let listed = String::from_utf8_lossy(&o.stdout);
    println!("== simctl list runtimes\n{listed}");
    let runtimes = listed
        .lines()
        .filter(|l| l.starts_with(|c: char| c.is_ascii_alphabetic()) && l[1..].contains(" ("))
        .count();
    let want = if runtimes > 0 {
        "FAIL simulator_runtimes:"
    } else {
        "PASS simulator_runtimes:"
    };
    assert!(
        host.before.stdout.contains(want),
        "want {want}\n{}",
        host.before.stdout
    );
}

/// Catches: a first apply that fails on a real Mac or skips the role account or the
/// LaunchDaemon (a `dscl` or `launchctl` call macOS refuses shows as a FAIL and exit 1).
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-provision --test as_root`"]
fn the_first_apply_makes_the_role_account_and_the_launch_daemon() {
    let (_serial, host) = host();
    let first = &host.first;
    assert_eq!(first.status, Some(0), "{}{}", first.stdout, first.stderr);
    for want in ["CHANGE launchd_label:", "CHANGE role_user:"] {
        assert!(
            first.stdout.contains(want),
            "lacks {want}\n{}",
            first.stdout
        );
    }
}

/// Catches: an apply that is not idempotent on a real Mac: one that rewrites a setting
/// or file each run, or reads back a value in another form than it wrote.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-provision --test as_root`"]
fn a_second_apply_prints_nothing() {
    let (_serial, host) = host();
    let r = host.run("apply", Some(SAFE));
    assert_eq!(r.status, Some(0), "{}{}", r.stdout, r.stderr);
    assert_eq!((r.stdout.as_str(), r.stderr.as_str()), ("", ""));
}

/// Catches: a check that fails what apply set (they read and write different
/// places), on every applied item. Also prints the check of every item after apply.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-provision --test as_root`"]
fn check_passes_after_apply() {
    let (_serial, host) = host();
    let r = host.run("check", Some(SAFE));
    assert_eq!(r.status, Some(0), "{}{}", r.stdout, r.stderr);
    println!("== check, after apply (every item; printed, not asserted)");
    host.run("check", None);
}

/// Catches: a LaunchDaemon written but not loaded, as launchd itself reports it.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-provision --test as_root`"]
fn the_launch_daemon_is_loaded_after_apply() {
    let (_serial, _host) = host();
    assert!(loaded(), "system/{LABEL} is not loaded");
}

/// Catches: a check that misses a setting changed by hand on real macOS, names the
/// wrong item, or fails more than the two that drifted; and an apply that does not put
/// them back.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-provision --test as_root`"]
fn two_settings_drifted_by_hand_fail_check_by_name_and_apply_fixes_them() {
    let (_serial, host) = host();
    let _reconverge = Reconverge(host);
    output(
        "/usr/bin/defaults",
        &[
            "write",
            SOFTWARE_UPDATE,
            "AutomaticDownload",
            "-bool",
            "true",
        ],
    );
    output(
        "/usr/sbin/scutil",
        &["--set", "ComputerName", "kbf-drifted"],
    );
    let r = host.run("check", Some(SAFE));
    assert_eq!(r.status, Some(1), "{}", r.stdout);
    let fails: Vec<&str> = r.stdout.lines().filter(|l| l.starts_with("FAIL")).collect();
    assert_eq!(fails.len(), 2, "want exactly two FAIL lines: {fails:?}");
    for want in [
        "FAIL updates_auto_download: AutomaticDownload is 1, want 0",
        "FAIL hostname: want kbf-mac-01, found ComputerName=kbf-drifted",
    ] {
        assert!(
            fails.iter().any(|l| l.contains(want)),
            "lacks {want}: {fails:?}"
        );
    }
    let r = host.run("apply", Some(SAFE));
    assert_eq!(r.status, Some(0), "{}", r.stdout);
    for want in [
        "CHANGE updates_auto_download:",
        "CHANGE hostname: ComputerName = kbf-mac-01",
    ] {
        assert!(r.stdout.contains(want), "lacks {want}\n{}", r.stdout);
    }
}

/// Catches: a restart that leaves the error log's bytes (launchd keeps it open while
/// the job runs, so it is emptied only while the job is stopped), or that leaves the
/// job unloaded.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-provision --test as_root`"]
fn restart_empties_the_error_log_and_the_job_is_loaded_again() {
    let (_serial, host) = host();
    fs::OpenOptions::new()
        .append(true)
        .open(ERR_LOG)
        .and_then(|mut log| log.write_all(b"kbf-as-root-marker\n"))
        .expect("append a marker to the error log");
    let r = host.run("restart", None);
    assert_eq!(r.status, Some(0), "{}", r.stderr);
    let after = fs::read_to_string(ERR_LOG).expect("read the error log");
    assert!(
        !after.contains("kbf-as-root-marker"),
        "the error log was not emptied: {after}"
    );
    assert!(loaded(), "system/{LABEL} is not loaded after restart");
}

/// Catches: the log directory left writable by the role account (it could plant a
/// link for root to empty), or an error log of another owner or mode, or with a
/// second link, as the real file system reports them.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-provision --test as_root`"]
fn the_error_log_is_the_role_accounts_in_a_directory_of_roots() {
    let (_serial, host) = host();
    let (uid, gid, mode, _) = owner_mode_links(LOG_DIR);
    assert_eq!((uid, gid, mode), (0, 0, 0o755), "{LOG_DIR}");
    let id = host.role_id;
    assert_eq!(owner_mode_links(ERR_LOG), (id, id, 0o644, 1), "{ERR_LOG}");
}

/// Catches: root emptying a file through a link planted at the error log (a restart
/// that does not refuse, or opens the log through the link), or an apply that leaves
/// the link or empties the file it names.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-provision --test as_root`"]
fn a_link_at_the_error_log_is_refused_by_restart_and_replaced_by_apply() {
    let (_serial, host) = host();
    let _reconverge = Reconverge(host);
    let precious = Path::new(SCRATCH).join("precious");
    fs::write(&precious, "kbf-precious\n").expect("write the file the link names");
    fs::remove_file(ERR_LOG).expect("remove the error log");
    std::os::unix::fs::symlink(&precious, ERR_LOG).expect("plant the link");
    let r = host.run("check", Some("launchd_label"));
    assert_eq!(
        r.status,
        Some(1),
        "check with a link at the error log\n{}",
        r.stdout
    );
    let r = host.run("restart", None);
    assert_eq!(
        r.status,
        Some(2),
        "restart with a link at the error log\n{}",
        r.stdout
    );
    assert!(
        r.stderr.contains("is not a regular file of _kbf"),
        "{}",
        r.stderr
    );
    let r = host.run("apply", Some("launchd_label"));
    assert_eq!(
        r.status,
        Some(0),
        "apply with a link at the error log\n{}",
        r.stdout
    );
    assert_eq!(
        fs::read_to_string(&precious).expect("read the file the link named"),
        "kbf-precious\n",
        "the link's target was emptied"
    );
    let log = fs::symlink_metadata(ERR_LOG).expect("the error log");
    assert!(!log.file_type().is_symlink(), "apply left the link");
}

/// Catches: an sshd item that passes while sshd uses other values than the drop-in's:
/// the item runs `sshd -T` as root and says it agrees, and `sshd -T`, asked here,
/// reports each of the drop-in's three values.
#[test]
#[ignore = "needs root: run by `cargo xtask sudo-test -p kbf-mac-provision --test as_root`"]
fn sshd_reports_the_drop_ins_values() {
    let (_serial, host) = host();
    let r = host.run("check", Some("ssh_password_auth"));
    assert_eq!(r.status, Some(0), "{}", r.stdout);
    assert!(r.stdout.contains("(sshd -T agrees)"), "{}", r.stdout);
    let effective = output("/usr/sbin/sshd", &["-T"]);
    for want in [
        "passwordauthentication no",
        "kbdinteractiveauthentication no",
        "permitrootlogin no",
    ] {
        assert!(effective.lines().any(|l| l == want), "sshd -T lacks {want}");
    }
}
