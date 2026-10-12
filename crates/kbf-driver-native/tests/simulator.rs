//! A simulator of the lease's own (`simulator=<device type>,<runtime>`,
//! `kbf_driver_native::simulator`): made in the lease's own device set before the
//! action runs, named to it, and shut down and deleted when the lease ends whatever the
//! outcome, or at the next start when the daemon was killed.
//!
//! Everywhere, a fake `xcrun` (kbf-testkit's, through a link to this test binary)
//! stands in for `xcrun simctl` and logs every call; the tests read that log. On macOS
//! the `#[ignore]`d tests at the end make, boot and delete a real simulator; the
//! native-macos job runs them with `--ignored`.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_native::NativeConfig;
use kbf_driver_native::network::Isolation;
use kbf_driver_native::simulator::{SET_DIR, SET_VAR, UDID_VAR};
use support::{MemoryCas, PROMPT, Spec, config, no_leases, run, runtime, scratch, stderr, stdout};

/// The UDID the fake `simctl create` prints for [`DEVICE_TYPE`].
const UDID: &str = "0A1B2C3D-4E5F-4071-8293-A4B5C6D7E8F9";
/// A second device's UDID: one the action makes in its set itself.
const OTHER: &str = "1A1B2C3D-4E5F-4071-8293-A4B5C6D7E8F9";
/// The UDID the fake prints for [`STUCK_TYPE`], which its `delete` refuses.
const STUCK: &str = "2A1B2C3D-4E5F-4071-8293-A4B5C6D7E8F9";

const DEVICE_TYPE: &str = "com.apple.CoreSimulator.SimDeviceType.iPhone-16";
const RUNTIME: &str = "com.apple.CoreSimulator.SimRuntime.iOS-18-4";
/// A device type the fake `create` refuses, as `simctl` does one it does not know.
const BAD_TYPE: &str = "com.apple.CoreSimulator.SimDeviceType.Nothing";
/// A device type whose device the fake `delete` refuses.
const STUCK_TYPE: &str = "com.apple.CoreSimulator.SimDeviceType.Stuck";
/// A device type for which the fake `create` prints no UDID.
const MUTE_TYPE: &str = "com.apple.CoreSimulator.SimDeviceType.Mute";

/// A fake `xcrun` at `dir/bin/xcrun` with `extra` rules before the default ones:
/// `simctl create` makes the device's directory in the set, as `simctl` does, and
/// prints its UDID (none for [`MUTE_TYPE`]); [`BAD_TYPE`] is refused with exit 148;
/// `shutdown` exits 149, as `simctl` does for a device that is not booted (the driver
/// must not care); `delete` of [`STUCK`] fails, every other `delete` succeeds.
fn fake_xcrun(dir: &Path, extra: &str) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let xcrun = bin.join("xcrun");
    kbf_testkit::act::link_self(&xcrun).expect("link");
    let rules = format!(
        "{extra}\
         simctl --set * create * {BAD_TYPE} *\t-\techo --stderr Invalid device type: $6 ; exit 148\n\
         simctl --set * create * {MUTE_TYPE} *\t-\techo\n\
         simctl --set * create * {STUCK_TYPE} *\t-\tmkdir $3/{STUCK} ; echo {STUCK}\n\
         simctl --set * create kbf-lease-* {DEVICE_TYPE} {RUNTIME}\t-\tmkdir $3/{UDID} ; echo {UDID}\n\
         simctl --set * shutdown *\t-\techo --stderr Unable to shutdown device in current state: Shutdown ; exit 149\n\
         simctl --set * delete {STUCK}\t-\techo --stderr Unable to delete ; exit 1\n\
         simctl --set * delete *\t-\texit 0\n"
    );
    std::fs::write(format!("{}.rules", xcrun.display()), rules).expect("rules");
    xcrun
}

/// What the fake `xcrun` was asked, one call a line, `xcrun` and the set left out:
/// `create kbf-lease-1-1 <type> <runtime>`, `delete <udid>`, ...; each set checked to be
/// `set`.
fn calls(xcrun: &Path, set: &Path) -> Vec<String> {
    let log = std::fs::read_to_string(format!("{}.log", xcrun.display())).unwrap_or_default();
    let prefix = format!("{} simctl --set {} ", xcrun.display(), set.display());
    log.lines()
        .map(|line| {
            line.strip_prefix(&prefix)
                .unwrap_or_else(|| panic!("{line:?} is not a simctl call in {}", set.display()))
                .to_owned()
        })
        .collect()
}

/// The calls one lease that made [`UDID`] makes, when its set holds `devices` at the
/// end.
fn lease_calls(seq: u64, devices: &[&str]) -> Vec<String> {
    let mut want = vec![format!("create kbf-lease-1-{seq} {DEVICE_TYPE} {RUNTIME}")];
    for udid in devices {
        want.push(format!("shutdown {udid}"));
        want.push(format!("delete {udid}"));
    }
    want
}

/// A configuration with the fake `xcrun` and no sandbox (on macOS too: the fake writes
/// its log outside the lease directory, which the real sandbox refuses).
fn fake_config(dir: &Path, extra: &str) -> (NativeConfig, PathBuf) {
    let mut config = config(dir);
    config.isolation = Isolation::None;
    config.xcrun = fake_xcrun(dir, extra);
    let xcrun = config.xcrun.clone();
    (config, xcrun)
}

/// The real path of lease `1.seq`'s device set, once the scratch root exists.
fn set_of(config: &NativeConfig, seq: u64) -> PathBuf {
    std::fs::canonicalize(&config.scratch)
        .expect("scratch root")
        .join(format!("lease-1-{seq}"))
        .join(SET_DIR)
}

fn asks(spec: Spec, device_type: &str) -> Spec {
    spec.property("simulator", &format!("{device_type},{RUNTIME}"))
}

/// Catches: no simulator made for an action that asks for one, or one made outside the
/// lease's own set (`--set` missing or another path: the shared set every lease
/// sees); its UDID or set not named to the action, or the Command's variable winning
/// over the driver's; the device not shut down and deleted at the end, or a device the
/// action made in its set itself left there; and `xcrun` run at all for an action that
/// asks for none.
#[tokio::test]
async fn a_lease_that_asks_for_a_simulator_gets_its_own() {
    let dir = scratch("sim-own");
    let (config, xcrun) = fake_config(&dir, "");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let set = set_of(&config, 1);
    let made_by_action = set.join(OTHER);
    let steps = format!(
        "env {UDID_VAR} ; env {SET_VAR} ; mkdir {}",
        made_by_action.display()
    );
    let spec = asks(Spec::act(&steps), DEVICE_TYPE).env(UDID_VAR, "the-commands");
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", stderr(&cas, &result));
    assert_eq!(
        stdout(&cas, &result),
        format!("{UDID}\n{}\n", set.display())
    );
    assert_eq!(calls(&xcrun, &set), lease_calls(1, &[UDID, OTHER]));
    assert!(no_leases(&config), "the lease directory is removed");

    let plain = Spec::act(&format!("env {UDID_VAR} none ; env {SET_VAR} none"));
    let result = run(&rt, &cas, 2, &plain).await.expect("ran");
    assert_eq!(stdout(&cas, &result), "none\nnone\n");
    assert_eq!(calls(&xcrun, &set).len(), 5, "no simctl call for lease 2");
}

/// Catches: the simulator left behind when the action fails (exits non-zero), times
/// out or is killed: the next lease's tests would find a booted, dirty device, and
/// the node would fill with them. Mutant planted: the end deletes simulators only
/// after an action that exited 0 (red on all three).
#[tokio::test]
async fn the_simulator_is_deleted_whatever_the_outcome() {
    let dir = scratch("sim-outcome");
    let (config, xcrun) = fake_config(&dir, "");
    let cas = Arc::new(MemoryCas::default());
    let rt = Arc::new(runtime(config.clone(), &cas));

    let failed = run(&rt, &cas, 1, &asks(Spec::act("exit 3"), DEVICE_TYPE)).await;
    assert_eq!(failed.expect("ran").exit_code, 3);
    assert_eq!(calls(&xcrun, &set_of(&config, 1)), lease_calls(1, &[UDID]));

    std::fs::remove_file(format!("{}.log", xcrun.display())).expect("new log");
    let slow = asks(Spec::act("sleep 300"), DEVICE_TYPE).timeout(Duration::from_millis(300));
    let timed_out = run(&rt, &cas, 2, &slow).await;
    assert!(
        matches!(timed_out, Err(RuntimeError::TimedOut { .. })),
        "{timed_out:?}"
    );
    assert_eq!(calls(&xcrun, &set_of(&config, 2)), lease_calls(2, &[UDID]));

    std::fs::remove_file(format!("{}.log", xcrun.display())).expect("new log");
    let started = dir.join("started");
    let steps = format!("append {} x ; sleep 300", started.display());
    let action = asks(Spec::act(&steps), DEVICE_TYPE).store(&cas);
    let running = tokio::spawn({
        let rt = Arc::clone(&rt);
        async move { rt.run(support::work(3, action, 0)).await }
    });
    let waited = tokio::time::timeout(PROMPT, async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    waited.await.expect("the action starts");
    rt.kill(kbf_types::LeaseId::new(1, 3)).await;
    let killed = running.await.expect("run task");
    assert!(matches!(killed, Err(RuntimeError::Killed)), "{killed:?}");
    assert_eq!(calls(&xcrun, &set_of(&config, 3)), lease_calls(3, &[UDID]));
    assert!(no_leases(&config), "every lease directory is removed");
}

/// Catches: a kill while `simctl create` runs cutting the create short (the device it
/// makes after the end looked would be left), the action run after the kill, and the
/// device that create made left behind.
#[tokio::test]
async fn a_kill_during_the_create_still_deletes_the_simulator() {
    let dir = scratch("sim-kill-create");
    let creating = dir.join("creating");
    let extra = format!(
        "simctl --set * create * slow *\t-\tappend {} x ; sleep 0.5 ; mkdir $3/{UDID} ; echo {UDID}\n",
        creating.display()
    );
    let (config, xcrun) = fake_config(&dir, &extra);
    let cas = Arc::new(MemoryCas::default());
    let rt = Arc::new(runtime(config.clone(), &cas));
    let ran = dir.join("ran");
    let action = Spec::act(&format!("append {} x", ran.display()))
        .property("simulator", &format!("slow,{RUNTIME}"))
        .store(&cas);
    let running = tokio::spawn({
        let rt = Arc::clone(&rt);
        async move { rt.run(support::work(1, action, 0)).await }
    });
    let waited = tokio::time::timeout(PROMPT, async {
        while !creating.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    waited.await.expect("the create starts");
    rt.kill(kbf_types::LeaseId::new(1, 1)).await;
    let killed = running.await.expect("run task");
    assert!(matches!(killed, Err(RuntimeError::Killed)), "{killed:?}");
    assert!(!ran.exists(), "the action did not run");
    assert_eq!(
        calls(&xcrun, &set_of(&config, 1))[1..],
        [format!("shutdown {UDID}"), format!("delete {UDID}")]
    );
    assert!(no_leases(&config));
}

/// Catches: a node where `xcrun` cannot be run (a Linux worker the action was placed
/// on without `OSFamily=darwin`, a Mac without developer tools) running the action
/// without the simulator it asked for, or failing it without saying why.
#[tokio::test]
async fn a_node_without_xcrun_fails_a_lease_that_asks_for_a_simulator() {
    let dir = scratch("sim-no-xcrun");
    let (mut config, _) = fake_config(&dir, "");
    config.xcrun = dir.join("no-xcrun");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let outcome = run(&rt, &cas, 1, &asks(Spec::act("exit 0"), DEVICE_TYPE)).await;
    let shown = format!("{outcome:?}");
    assert!(
        shown.starts_with("Err(Failed(\"simulator: simctl create kbf-lease-1-1 "),
        "{shown}"
    );
    assert!(no_leases(&config));
}

/// Catches: an action run without the simulator it asked for (a `simctl create` that
/// failed, or printed no UDID, taken for a made one), the failure blamed on the
/// action (it is the node's: a runtime it lacks) or not naming `simctl`'s reason; and
/// the lease directory left behind.
#[tokio::test]
async fn a_simulator_that_cannot_be_made_fails_the_lease_before_the_action_runs() {
    let dir = scratch("sim-create-fails");
    let (config, xcrun) = fake_config(&dir, "");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let ran = dir.join("ran");
    let action = Spec::act(&format!("append {} x", ran.display()));
    for (seq, device_type, why) in [
        (
            1,
            BAD_TYPE,
            "Invalid device type: com.apple.CoreSimulator.SimDeviceType.Nothing",
        ),
        (2, MUTE_TYPE, "simctl create printed no UDID: \"\""),
    ] {
        let outcome = run(&rt, &cas, seq, &asks(action.clone(), device_type)).await;
        assert!(
            matches!(&outcome, Err(RuntimeError::Failed(w)) if w.starts_with("simulator: ") && w.contains(why)),
            "{device_type}: {outcome:?}"
        );
        let calls = calls(&xcrun, &set_of(&config, seq));
        assert!(
            calls
                .last()
                .is_some_and(|c| c.starts_with(&format!("create kbf-lease-1-{seq} {device_type}"))),
            "{calls:?}"
        );
        std::fs::remove_file(format!("{}.log", xcrun.display())).expect("new log");
    }
    assert!(!ran.exists(), "the action did not run");
    assert!(no_leases(&config), "the lease directories are removed");
}

/// Catches: a simulator that could not be deleted passed over in silence (the node
/// fills with devices and nobody hears), or the lease directory kept because of it.
#[tokio::test]
async fn a_simulator_that_cannot_be_deleted_fails_the_lease() {
    let dir = scratch("sim-delete-fails");
    let (config, xcrun) = fake_config(&dir, "");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let outcome = run(&rt, &cas, 1, &asks(Spec::act("exit 0"), STUCK_TYPE)).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.starts_with("simulator: simctl delete ") && why.contains("Unable to delete")),
        "{outcome:?}"
    );
    let set = set_of(&config, 1);
    assert_eq!(
        calls(&xcrun, &set)[1..],
        [format!("shutdown {STUCK}"), format!("delete {STUCK}")]
    );
    assert!(no_leases(&config), "the lease directory is removed anyway");
}

/// Catches: `simctl` run outside the actions' sandbox (the daemon would run a
/// developer tool, which reads `xcrun`'s cache that leases can write, unsandboxed) or
/// with another lease directory; `create` not given the Xcode the action names (its
/// runtimes are that Xcode's); and the end run with that Xcode or the daemon's own
/// `DEVELOPER_DIR` rather than the node's Xcode (so the start sweep, which knows no
/// Xcode, ends devices the same way).
#[tokio::test]
async fn simctl_runs_under_the_sandbox_with_the_actions_xcode() {
    let dir = scratch("sim-sandbox");
    let (mut config, xcrun) = fake_config(&dir, "");
    let sandbox_exec = dir.join("bin").join("sandbox-exec");
    kbf_testkit::act::link_self(&sandbox_exec).expect("link");
    std::fs::write(format!("{}.rules", sandbox_exec.display()), "").expect("rules");
    config.isolation = Isolation::Sandbox(sandbox_exec);
    let developer = "/Apps/Xcode_16.2.app/Contents/Developer";
    config.xcodes = [("16C5032a".to_owned(), PathBuf::from(developer))].into();
    let seen = dir.join("seen");
    let seen = seen.display();
    let extra = format!(
        "simctl --set * create *\tDEVELOPER_DIR={developer}\tappend {seen} create ${{KBF_LEASE}} ; mkdir $3/{UDID} ; echo {UDID}\n\
         simctl --set * create *\t-\techo --stderr not the actions Xcode ; exit 9\n\
         simctl --set * shutdown *\tDEVELOPER_DIR=\tappend {seen} shutdown ${{KBF_LEASE}}\n\
         simctl --set * delete *\tDEVELOPER_DIR=\tappend {seen} delete ${{KBF_LEASE}}\n\
         simctl --set * delete *\t-\techo --stderr not the nodes Xcode ; exit 9\n"
    );
    std::fs::write(format!("{}.rules", xcrun.display()), {
        let rules = std::fs::read_to_string(format!("{}.rules", xcrun.display())).expect("rules");
        format!("{extra}{rules}")
    })
    .expect("rules");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let spec = asks(Spec::act("exit 0"), DEVICE_TYPE).property("xcode", "16C5032a");
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    let lease = set_of(&config, 1);
    let lease = lease.parent().expect("lease").display();
    assert_eq!(
        std::fs::read_to_string(dir.join("seen")).expect("seen"),
        format!("create {lease}\nshutdown {lease}\ndelete {lease}\n")
    );
}

/// Catches: a simulator a killed daemon's lease left never deleted (the start removes
/// the lease directory under a device `CoreSimulatorService` still holds, maybe
/// booted), a name in the set that is not a UDID taken for a device, a lease
/// directory without a set asked about, and a device that cannot be deleted stopping
/// the start (the daemon would stay down over one simulator).
#[tokio::test]
async fn the_start_sweep_deletes_the_simulators_a_killed_daemon_left() {
    let dir = scratch("sim-sweep");
    let (config, xcrun) = fake_config(&dir, "");
    let left = config.scratch.join("lease-7-7");
    let set = left.join(SET_DIR);
    for device in [UDID, STUCK, "not-a-udid"] {
        std::fs::create_dir_all(set.join(device)).expect("device");
    }
    std::fs::write(set.join("device_set.plist"), b"plist").expect("plist");
    std::fs::create_dir_all(config.scratch.join("lease-7-8/root")).expect("no set");
    let cas = Arc::new(MemoryCas::default());
    let _rt = runtime(config.clone(), &cas);
    let set = std::fs::canonicalize(&config.scratch)
        .expect("scratch")
        .join("lease-7-7")
        .join(SET_DIR);
    assert_eq!(
        calls(&xcrun, &set),
        [
            format!("shutdown {UDID}"),
            format!("delete {UDID}"),
            format!("shutdown {STUCK}"),
            format!("delete {STUCK}"),
        ]
    );
    assert!(no_leases(&config), "both lease directories are removed");
}

/// The tests that make real simulators.
#[cfg(target_os = "macos")]
mod real {
    use std::io::Write as _;
    use std::process::Command;

    use kbf_driver_native::NativeRuntime;

    use super::*;

    /// One real simulator at a time: two booting at once on a small runner is slow
    /// for no gain.
    static ONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// `xcrun args`, unsandboxed (the test's own view), its stdout; panics on failure.
    fn xcrun(args: &[&str]) -> String {
        let out = Command::new("/usr/bin/xcrun")
            .args(args)
            .output()
            .expect("xcrun");
        assert!(
            out.status.success(),
            "xcrun {args:?}: {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("UTF-8")
    }

    /// The newest available iOS runtime and the first iPhone it supports, by
    /// identifier.
    fn pick() -> (String, String) {
        let list: serde_json::Value =
            serde_json::from_str(&xcrun(&["simctl", "list", "runtimes", "available", "-j"]))
                .expect("JSON");
        let runtime = list["runtimes"]
            .as_array()
            .expect("runtimes")
            .iter()
            .filter(|r| r["platform"] == "iOS")
            .max_by_key(|r| {
                let version = r["version"].as_str().unwrap_or("0");
                version
                    .split('.')
                    .map(|p| p.parse::<u32>().unwrap_or(0))
                    .collect::<Vec<_>>()
            })
            .expect("an iOS simulator runtime");
        let device_type = runtime["supportedDeviceTypes"]
            .as_array()
            .expect("device types")
            .iter()
            .find(|t| t["productFamily"] == "iPhone")
            .expect("an iPhone");
        let pair = (
            device_type["identifier"].as_str().expect("id").to_owned(),
            runtime["identifier"].as_str().expect("id").to_owned(),
        );
        let _ = writeln!(std::io::stderr(), "simulator: {} on {}", pair.0, pair.1);
        pair
    }

    /// Whether any process's arguments name `udid` (a booted simulator's
    /// `launchd_sim` does: it runs from the device's directory), looking for up to
    /// ten seconds for none.
    fn running(udid: &str) -> bool {
        for _ in 0..100 {
            if !running_now(udid) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        true
    }

    /// Whether a process's arguments name `udid` now.
    fn running_now(udid: &str) -> bool {
        Command::new("/usr/bin/pgrep")
            .args(["-f", udid])
            .output()
            .expect("pgrep")
            .status
            .success()
    }

    /// Catches, on a real Mac: `simctl --set` in a lease's set not working under the
    /// actions' sandbox (create, or the delete at the end, refused), a device the
    /// action cannot boot from its sandbox, the device landing in the shared set, and
    /// a booted device left running or registered after the lease. Also prints whether
    /// `xcdevice` (behind `xcodebuild -destination`) lists the lease's device: a
    /// measurement, not a check.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "macOS with an iOS simulator runtime; the native-macos job runs it"]
    async fn a_real_simulator_is_made_booted_and_deleted_in_the_leases_set() {
        let _one = ONE.lock().await;
        let (device_type, runtime_id) = pick();
        let shared_before = xcrun(&["simctl", "list", "devices", "-j"]);
        let dir = scratch("sim-real");
        let config = config(&dir);
        let cas = Arc::new(MemoryCas::default());
        let rt = runtime(config.clone(), &cas);
        let script = format!(
            "set -e\n\
             echo \"${UDID_VAR}\"; echo \"${SET_VAR}\"\n\
             xcrun simctl --set \"${SET_VAR}\" bootstatus \"${UDID_VAR}\" -b >&2\n\
             xcrun simctl --set \"${SET_VAR}\" list devices booted | grep -c \"${UDID_VAR}\"\n\
             echo \"xcdevice lists it: $(xcrun xcdevice list --timeout 20 2>&1 | grep -c \"${UDID_VAR}\" || true)\" >&2\n"
        );
        let spec = Spec::sh(&script)
            .property("simulator", &format!("{device_type},{runtime_id}"))
            .timeout(Duration::from_secs(600));
        let action = spec.store(&cas);
        let result = tokio::time::timeout(
            Duration::from_secs(660),
            rt.run(support::work(1, action, 0)),
        )
        .await
        .expect("the run ends")
        .expect("ran");
        let err = stderr(&cas, &result);
        let _ = writeln!(std::io::stderr(), "action stderr:\n{err}");
        assert_eq!(result.exit_code, 0, "{err}");
        let out = stdout(&cas, &result);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        let udid = lines[0];
        assert_eq!(lines[1], set_of(&config, 1).display().to_string());
        assert_eq!(lines[2], "1", "the device booted in the lease's set");
        assert!(!shared_before.contains(udid));
        assert!(
            !xcrun(&["simctl", "list", "devices", "-j"]).contains(udid),
            "the device is not in the shared set"
        );
        assert!(!running(udid), "the device is shut down");
        assert!(no_leases(&config), "the lease directory is removed");
    }

    /// Catches, on a real Mac: a booted simulator in a lease directory a killed daemon
    /// left not shut down and deleted by the next start (it would keep running with
    /// its files removed under it).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "macOS with an iOS simulator runtime; the native-macos job runs it"]
    async fn the_start_deletes_a_real_booted_simulator_left_behind() {
        let _one = ONE.lock().await;
        let (device_type, runtime_id) = pick();
        let dir = scratch("sim-real-left");
        let config = config(&dir);
        let set = config.scratch.join("lease-9-9").join(SET_DIR);
        std::fs::create_dir_all(&set).expect("set");
        let set = std::fs::canonicalize(&set).expect("real path");
        let set_arg = set.to_str().expect("UTF-8");
        let created = xcrun(&[
            "simctl",
            "--set",
            set_arg,
            "create",
            "kbf-left",
            &device_type,
            &runtime_id,
        ]);
        let udid = created.trim().to_owned();
        // `boot` returns once the device is booted, without the minutes of data
        // migration `bootstatus` waits for: enough for a device left running.
        xcrun(&["simctl", "--set", set_arg, "boot", &udid]);
        assert!(
            running_now(&udid),
            "pgrep finds a booted simulator by its UDID"
        );
        let cas = Arc::new(MemoryCas::default());
        let _rt = tokio::task::spawn_blocking({
            let config = config.clone();
            move || NativeRuntime::new(config, cas).expect("runtime")
        })
        .await
        .expect("start");
        assert!(!running(&udid), "the left device is shut down");
        assert!(no_leases(&config), "the left lease directory is removed");
    }
}
