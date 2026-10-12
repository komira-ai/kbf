//! A simulator of the lease's own (`simulator=<device type>,<runtime>`,
//! `kbf_driver_native::simulator`): made in the default device set under the lease's
//! name before the action runs, named to it, and shut down and deleted when the lease
//! ends whatever the outcome, or at the next start when the daemon was killed.
//!
//! Everywhere, a fake `xcrun` (kbf-testkit's, through a link to this test binary)
//! stands in for `xcrun simctl` and logs every call; the tests read that log. Its
//! `simctl list devices -j` prints the file `devices.json` beside it, which a test
//! writes. On macOS the `#[ignore]`d tests at the end make, boot and delete real
//! simulators; the native-macos job runs them with `--ignored`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::support::{
    MemoryCas, PROMPT, Spec, config, no_leases, run, runtime, scratch, stderr, stdout,
};
use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_native::NativeConfig;
use kbf_driver_native::network::Isolation;
use kbf_driver_native::simulator::{MARKER, UDID_VAR};

/// The UDID the fake `simctl create` prints for [`DEVICE_TYPE`].
const UDID: &str = "0A1B2C3D-4E5F-4071-8293-A4B5C6D7E8F9";
/// A second device's UDID: one named for the same lease (a create cut short made it
/// late).
const OTHER: &str = "1A1B2C3D-4E5F-4071-8293-A4B5C6D7E8F9";
/// The UDID the fake prints for [`STUCK_TYPE`], which its `delete` refuses.
const STUCK: &str = "2A1B2C3D-4E5F-4071-8293-A4B5C6D7E8F9";
/// A device of the shared set no lease made.
const SHARED: &str = "3A1B2C3D-4E5F-4071-8293-A4B5C6D7E8F9";
/// Another lease's device.
const NEIGHBOUR: &str = "4A1B2C3D-4E5F-4071-8293-A4B5C6D7E8F9";

const DEVICE_TYPE: &str = "com.apple.CoreSimulator.SimDeviceType.iPhone-16";
const RUNTIME: &str = "com.apple.CoreSimulator.SimRuntime.iOS-18-4";
/// A device type the fake `create` refuses, as `simctl` does one it does not know.
const BAD_TYPE: &str = "com.apple.CoreSimulator.SimDeviceType.Nothing";
/// A device type whose device the fake `delete` refuses.
const STUCK_TYPE: &str = "com.apple.CoreSimulator.SimDeviceType.Stuck";
/// A device type for which the fake `create` prints no UDID.
const MUTE_TYPE: &str = "com.apple.CoreSimulator.SimDeviceType.Mute";

/// A fake `xcrun` at `dir/bin/xcrun` with `extra` rules before the default ones:
/// `simctl create` prints the device's UDID (none for [`MUTE_TYPE`]); [`BAD_TYPE`] is
/// refused with exit 148; `list devices -j` prints `dir/bin/devices.json` (no devices
/// until a test writes one); `shutdown` exits 149, as `simctl` does for a device that
/// is not booted (the driver must not care); `delete` of [`STUCK`] fails, every other
/// `delete` succeeds.
fn fake_xcrun(dir: &Path, extra: &str) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let xcrun = bin.join("xcrun");
    kbf_testkit::act::link_self(&xcrun).expect("link");
    let list = bin.join("devices.json");
    std::fs::write(&list, br#"{"devices": {}}"#).expect("list");
    let list = list.display();
    let rules = format!(
        "{extra}\
         simctl create * {BAD_TYPE} *\t-\techo --stderr Invalid device type: $4 ; exit 148\n\
         simctl create * {MUTE_TYPE} *\t-\techo\n\
         simctl create * {STUCK_TYPE} *\t-\techo {STUCK}\n\
         simctl create kbf-lease-* {DEVICE_TYPE} {RUNTIME}\t-\techo {UDID}\n\
         simctl list devices -j\t-\tcat {list}\n\
         simctl shutdown *\t-\techo --stderr Unable to shutdown device in current state: Shutdown ; exit 149\n\
         simctl delete {STUCK}\t-\techo --stderr Unable to delete ; exit 1\n\
         simctl delete *\t-\texit 0\n"
    );
    std::fs::write(format!("{}.rules", xcrun.display()), rules).expect("rules");
    xcrun
}

/// Makes the fake `xcrun`'s `simctl list devices -j` print `devices` (name, UDID),
/// all under one runtime.
fn list(xcrun: &Path, devices: &[(&str, &str)]) {
    let devices: Vec<String> = (devices.iter())
        .map(|(name, udid)| {
            format!(r#"{{"name": "{name}", "udid": "{udid}", "state": "Shutdown"}}"#)
        })
        .collect();
    let json = format!(
        r#"{{"devices": {{"{RUNTIME}": [{}]}}}}"#,
        devices.join(", ")
    );
    std::fs::write(xcrun.with_file_name("devices.json"), json).expect("list");
}

/// What the fake `xcrun` was asked, one call a line, `xcrun simctl` left out:
/// `create kbf-lease-1-1 <type> <runtime>`, `delete <udid>`, ...
fn calls(xcrun: &Path) -> Vec<String> {
    let log = std::fs::read_to_string(format!("{}.log", xcrun.display())).unwrap_or_default();
    let prefix = format!("{} simctl ", xcrun.display());
    log.lines()
        .map(|line| {
            line.strip_prefix(&prefix)
                .unwrap_or_else(|| panic!("{line:?} is not a simctl call"))
                .to_owned()
        })
        .collect()
}

/// Forgets the calls so far.
fn new_log(xcrun: &Path) {
    let _ = std::fs::remove_file(format!("{}.log", xcrun.display()));
}

/// The calls one lease that made [`UDID`] makes, when `also` are the other devices
/// named for it at the end.
fn lease_calls(seq: u64, also: &[&str]) -> Vec<String> {
    let mut want = vec![
        format!("create kbf-lease-1-{seq} {DEVICE_TYPE} {RUNTIME}"),
        "list devices -j".to_owned(),
    ];
    for udid in std::iter::once(&UDID).chain(also) {
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

/// The real path of lease `1.seq`'s directory, once the scratch root exists.
fn lease_of(config: &NativeConfig, seq: u64) -> PathBuf {
    std::fs::canonicalize(&config.scratch)
        .expect("scratch root")
        .join(format!("lease-1-{seq}"))
}

fn asks(spec: Spec, device_type: &str) -> Spec {
    spec.property("simulator", &format!("{device_type},{RUNTIME}"))
}

/// Catches: no simulator made for an action that asks for one, or one not named for
/// the lease; its UDID not named to the action, or the Command's variable winning
/// over the driver's; the device not shut down and deleted at the end; a device named
/// for the lease that the create did not print (one a create cut short made late) left
/// behind; a device of the shared set or another lease's deleted (the end matching
/// names by prefix, or deleting every device listed), or the lease's own deleted twice;
/// and `xcrun` run at all for an action that asks for none.
#[tokio::test]
async fn a_lease_that_asks_for_a_simulator_gets_its_own() {
    let dir = scratch("sim-own");
    let (config, xcrun) = fake_config(&dir, "");
    list(
        &xcrun,
        &[
            ("iPhone 16", SHARED),
            ("kbf-lease-1-1", UDID),
            ("kbf-lease-1-1", OTHER),
            ("kbf-lease-1-12", NEIGHBOUR),
        ],
    );
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let spec =
        asks(Spec::act(&format!("env {UDID_VAR}")), DEVICE_TYPE).env(UDID_VAR, "the-commands");
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", stderr(&cas, &result));
    assert_eq!(stdout(&cas, &result), format!("{UDID}\n"));
    assert_eq!(calls(&xcrun), lease_calls(1, &[OTHER]));
    assert!(no_leases(&config), "the lease directory is removed");

    let plain = Spec::act(&format!("env {UDID_VAR} none"));
    let result = run(&rt, &cas, 2, &plain).await.expect("ran");
    assert_eq!(stdout(&cas, &result), "none\n");
    assert_eq!(calls(&xcrun).len(), 6, "no simctl call for lease 2");
}

/// Catches the end deleting what the action's lease directory says rather than what
/// the daemon kept: an action that rewrites its directory (the marker made a link to
/// another lease's, a file named for another device) changes nothing the end does.
#[tokio::test]
async fn the_end_deletes_the_device_the_daemon_made_whatever_the_lease_directory_holds() {
    let dir = scratch("sim-dir-edited");
    let (config, xcrun) = fake_config(&dir, "");
    list(
        &xcrun,
        &[("kbf-lease-1-12", NEIGHBOUR), ("iPhone 16", SHARED)],
    );
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let lease = lease_of(&config, 1);
    let steps = format!(
        "run rm {marker} ; symlink {other} {marker} ; mkdir {lease}/{SHARED} ; mkdir {lease}/sims/{NEIGHBOUR}",
        marker = lease.join(MARKER).display(),
        other = dir.join("elsewhere").display(),
        lease = lease.display(),
    );
    let result = run(&rt, &cas, 1, &asks(Spec::act(&steps), DEVICE_TYPE)).await;
    assert_eq!(result.expect("ran").exit_code, 0);
    assert_eq!(calls(&xcrun), lease_calls(1, &[]));
    assert!(no_leases(&config));
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
    assert_eq!(calls(&xcrun), lease_calls(1, &[]));

    new_log(&xcrun);
    let slow = asks(Spec::act("sleep 300"), DEVICE_TYPE).timeout(Duration::from_millis(300));
    let timed_out = run(&rt, &cas, 2, &slow).await;
    assert!(
        matches!(timed_out, Err(RuntimeError::TimedOut { .. })),
        "{timed_out:?}"
    );
    assert_eq!(calls(&xcrun), lease_calls(2, &[]));

    new_log(&xcrun);
    let started = dir.join("started");
    let steps = format!("append {} x ; sleep 300", started.display());
    let action = asks(Spec::act(&steps), DEVICE_TYPE).store(&cas);
    let running = tokio::spawn({
        let rt = Arc::clone(&rt);
        async move { rt.run(crate::support::work(3, action, 0)).await }
    });
    wait_for(&started).await;
    rt.kill(kbf_types::LeaseId::new(1, 3)).await;
    let killed = running.await.expect("run task");
    assert!(matches!(killed, Err(RuntimeError::Killed)), "{killed:?}");
    assert_eq!(calls(&xcrun), lease_calls(3, &[]));
    assert!(no_leases(&config), "every lease directory is removed");
}

/// Waits until `path` exists.
async fn wait_for(path: &Path) {
    let waited = tokio::time::timeout(PROMPT, async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    waited.await.expect("it appears");
}

/// Catches a run the daemon drops (its future dropped mid-action) leaving its
/// simulator behind until the next daemon start: the drop must delete it as the end
/// does. Mutant planted: the drop only removes the directory (red).
#[tokio::test]
async fn a_dropped_run_deletes_its_simulator() {
    let dir = scratch("sim-dropped");
    let (config, xcrun) = fake_config(&dir, "");
    let cas = Arc::new(MemoryCas::default());
    let rt = Arc::new(runtime(config.clone(), &cas));
    let started = dir.join("started");
    let steps = format!("append {} x ; sleep 300", started.display());
    let action = asks(Spec::act(&steps), DEVICE_TYPE).store(&cas);
    let running = tokio::spawn({
        let rt = Arc::clone(&rt);
        async move { rt.run(crate::support::work(1, action, 0)).await }
    });
    wait_for(&started).await;
    running.abort();
    let dropped = running.await.expect_err("the run is dropped");
    assert!(dropped.is_cancelled(), "{dropped:?}");
    assert_eq!(calls(&xcrun), lease_calls(1, &[]));
    assert!(no_leases(&config), "the lease directory is removed");
}

/// Catches: a kill while `simctl create` runs cutting the create short (the device it
/// makes after the end looked would be left), the action run after the kill, and the
/// device that create made left behind.
#[tokio::test]
async fn a_kill_during_the_create_still_deletes_the_simulator() {
    let dir = scratch("sim-kill-create");
    let creating = dir.join("creating");
    let extra = format!(
        "simctl create * slow *\t-\tappend {} x ; sleep 0.5 ; echo {UDID}\n",
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
        async move { rt.run(crate::support::work(1, action, 0)).await }
    });
    wait_for(&creating).await;
    rt.kill(kbf_types::LeaseId::new(1, 1)).await;
    let killed = running.await.expect("run task");
    assert!(matches!(killed, Err(RuntimeError::Killed)), "{killed:?}");
    assert!(!ran.exists(), "the action did not run");
    assert_eq!(calls(&xcrun)[1..], lease_calls(1, &[])[1..]);
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
/// action (it is the node's: a runtime it lacks) or not naming `simctl`'s reason; the
/// devices named for the lease not looked for after a create that failed (it may have
/// made one); and the lease directory left behind.
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
        (2, MUTE_TYPE, "simctl create printed no UDID"),
    ] {
        list(&xcrun, &[(&format!("kbf-lease-1-{seq}"), OTHER)]);
        let outcome = run(&rt, &cas, seq, &asks(action.clone(), device_type)).await;
        let shown = format!("{outcome:?}");
        assert!(
            shown.starts_with("Err(Failed(\"simulator: "),
            "{device_type}: {shown}"
        );
        assert!(shown.contains(why), "{device_type}: {shown}");
        assert_eq!(
            calls(&xcrun),
            [
                format!("create kbf-lease-1-{seq} {device_type} {RUNTIME}"),
                "list devices -j".to_owned(),
                format!("shutdown {OTHER}"),
                format!("delete {OTHER}"),
            ]
        );
        new_log(&xcrun);
    }
    assert!(!ran.exists(), "the action did not run");
    assert!(no_leases(&config), "the lease directories are removed");
}

/// Catches: a simulator that could not be deleted, or a device list that could not be
/// read, passed over in silence (the node fills with devices and nobody hears), or
/// the lease directory kept because of it.
#[tokio::test]
async fn a_simulator_that_cannot_be_deleted_fails_the_lease() {
    let dir = scratch("sim-delete-fails");
    let extra = "simctl list devices -j\tTMPDIR=*-1-2\techo not JSON\n";
    let (config, xcrun) = fake_config(&dir, extra);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let outcome = run(&rt, &cas, 1, &asks(Spec::act("exit 0"), STUCK_TYPE)).await;
    let shown = format!("{outcome:?}");
    assert!(
        shown.starts_with("Err(Failed(\"simulator: simctl delete ")
            && shown.contains("Unable to delete"),
        "{shown}"
    );
    assert_eq!(
        calls(&xcrun)[1..],
        [
            "list devices -j".to_owned(),
            format!("shutdown {STUCK}"),
            format!("delete {STUCK}")
        ]
    );
    let outcome = run(&rt, &cas, 2, &asks(Spec::act("exit 0"), DEVICE_TYPE)).await;
    let shown = format!("{outcome:?}");
    assert!(
        shown.starts_with("Err(Failed(\"simulator: simctl list devices -j: "),
        "{shown}"
    );
    assert_eq!(calls(&xcrun)[4..], lease_calls(2, &[]));
    assert!(
        no_leases(&config),
        "the lease directories are removed anyway"
    );
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
    let devices = xcrun.with_file_name("devices.json");
    let extra = format!(
        "simctl create *\tDEVELOPER_DIR={developer}\tappend {seen} create ${{KBF_LEASE}} ; echo {UDID}\n\
         simctl create *\t-\techo --stderr not the actions Xcode ; exit 9\n\
         simctl list *\tDEVELOPER_DIR=\tappend {seen} list ${{KBF_LEASE}} ; cat {}\n\
         simctl list *\t-\techo --stderr not the nodes Xcode ; exit 9\n\
         simctl shutdown *\tDEVELOPER_DIR=\tappend {seen} shutdown ${{KBF_LEASE}}\n\
         simctl delete *\tDEVELOPER_DIR=\tappend {seen} delete ${{KBF_LEASE}}\n\
         simctl delete *\t-\techo --stderr not the nodes Xcode ; exit 9\n",
        devices.display()
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
    let lease = lease_of(&config, 1);
    let lease = lease.display();
    assert_eq!(
        std::fs::read_to_string(dir.join("seen")).expect("seen"),
        format!("create {lease}\nlist {lease}\nshutdown {lease}\ndelete {lease}\n")
    );
}

/// Catches: a simulator a killed daemon's lease left never deleted (it would keep
/// running, maybe booted, and fill the node); a device of the shared set deleted (the
/// sweep must take only names that start `kbf-lease-`); the sweep run more than once,
/// or for a start whose left lease directories hold no marker (a start would ask
/// `simctl` every time); a marker followed through a link; and a device that cannot be
/// deleted, or a list that cannot be read, stopping the start (the daemon would stay
/// down over one simulator).
#[tokio::test]
async fn the_start_sweep_deletes_the_simulators_a_killed_daemon_left() {
    let dir = scratch("sim-sweep");
    let (config, xcrun) = fake_config(&dir, "");
    list(
        &xcrun,
        &[
            ("kbf-lease-7-7", UDID),
            ("iPhone 16", SHARED),
            ("kbf-lease-3-3", STUCK),
            ("kbf-lease-3-3 copy", OTHER),
        ],
    );
    let left = |name: &str, marker: bool| {
        let lease = config.scratch.join(name);
        std::fs::create_dir_all(lease.join("root")).expect("left");
        if marker {
            std::fs::write(lease.join(MARKER), b"").expect("marker");
        }
    };
    left("lease-7-7", true);
    left("lease-7-8", true);
    left("lease-7-9", false);
    let cas = Arc::new(MemoryCas::default());
    let _rt = runtime(config.clone(), &cas);
    assert_eq!(
        calls(&xcrun),
        [
            "list devices -j".to_owned(),
            format!("shutdown {UDID}"),
            format!("delete {UDID}"),
            format!("shutdown {STUCK}"),
            format!("delete {STUCK}"),
            format!("shutdown {OTHER}"),
            format!("delete {OTHER}"),
        ]
    );
    assert!(no_leases(&config), "every lease directory is removed");

    new_log(&xcrun);
    left("lease-7-9", false);
    let lease = config.scratch.join("lease-8-1");
    std::fs::create_dir_all(&lease).expect("left");
    std::os::unix::fs::symlink(dir.join("bin").join("devices.json"), lease.join(MARKER))
        .expect("link");
    let _rt = runtime(config.clone(), &cas);
    assert_eq!(calls(&xcrun), [""; 0], "no marker, no sweep");
    assert!(no_leases(&config));

    left("lease-9-9", true);
    let mut unlisted = config.clone();
    unlisted.xcrun = dir.join("no-xcrun");
    let _rt = runtime(unlisted, &cas);
    assert!(
        no_leases(&config),
        "a list that cannot be read does not stop the start"
    );
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

    /// The iOS XCUITest fixture (`tools/spike/macos/fixtures`), copied to `dir`; the
    /// `-project` path.
    fn ios_fixture(dir: &Path) -> String {
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/spike/macos/fixtures");
        let status = Command::new("/bin/cp")
            .arg("-R")
            .arg(&fixtures)
            .arg(dir)
            .status()
            .expect("cp");
        assert!(status.success(), "copy the fixtures");
        let project = dir.join("IOSFixture.xcodeproj");
        assert!(project.is_dir(), "{}", project.display());
        project.to_str().expect("UTF-8").to_owned()
    }

    /// `xcodebuild args` on the iOS fixture at `project`, unsandboxed: whether it exited
    /// 0, and its stdout and stderr together.
    fn xcodebuild(project: &str, derived: &Path, args: &[&str]) -> (bool, String) {
        let out = Command::new("/usr/bin/xcodebuild")
            .args([
                "-project",
                project,
                "-scheme",
                "IOSFixture",
                "-derivedDataPath",
            ])
            .arg(derived)
            .args(args)
            .output()
            .expect("xcodebuild");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    /// Settles, on a real Mac, where a lease's simulator must live: `xcodebuild`
    /// (`-showdestinations`, and `-destination id=<UDID>` resolved by a dry-run build)
    /// finds a device of the default set and not one made in a device set elsewhere
    /// (`simctl --set`). Catches a design that makes the device where `xcodebuild
    /// test -destination` cannot reach it; the default-set arm is the control (a
    /// listing that never names any device would pass the other arm alone).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "macOS with an iOS simulator runtime; the native-macos job runs it"]
    async fn xcodebuild_finds_a_device_of_the_default_set_and_not_one_of_another_set() {
        let _one = ONE.lock().await;
        let (device_type, runtime_id) = pick();
        let dir = scratch("sim-real-xcodebuild");
        let set = dir.join("set");
        std::fs::create_dir_all(&set).expect("set");
        let set = std::fs::canonicalize(&set).expect("real path");
        let set_arg = set.to_str().expect("UTF-8");
        let elsewhere = xcrun(&[
            "simctl",
            "--set",
            set_arg,
            "create",
            "kbf-probe-set",
            &device_type,
            &runtime_id,
        ]);
        let elsewhere = elsewhere.trim().to_owned();
        let default = xcrun(&[
            "simctl",
            "create",
            "kbf-probe-default",
            &device_type,
            &runtime_id,
        ]);
        let default = default.trim().to_owned();
        let project = ios_fixture(&dir);
        let derived = dir.join("derived");
        let (_, shown) = xcodebuild(&project, &derived, &["-showdestinations"]);
        let dry = |udid: &str| {
            let destination = format!("id={udid}");
            xcodebuild(
                &project,
                &derived,
                &["-destination", &destination, "-dry-run", "build"],
            )
        };
        let (default_ok, default_dry) = dry(&default);
        let (elsewhere_ok, elsewhere_dry) = dry(&elsewhere);
        xcrun(&["simctl", "delete", &default]);
        xcrun(&["simctl", "--set", set_arg, "delete", &elsewhere]);
        let _ = writeln!(
            std::io::stderr(),
            "xcodebuild -showdestinations:\n{shown}\n\
             dry-run build on the default set's device: {default_ok}\n\
             dry-run build on the other set's device: {elsewhere_ok}: {}",
            elsewhere_dry
                .lines()
                .filter(|l| l.contains("destination"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(
            shown.contains(&default),
            "-showdestinations lists the default set's device"
        );
        assert!(
            !shown.contains(&elsewhere),
            "-showdestinations lists the other set's device"
        );
        assert!(default_ok, "{default_dry}");
        assert!(!elsewhere_ok, "a destination in another set resolved");
    }

    /// Catches, on a real Mac: `simctl create` in the default set not working under the
    /// actions' sandbox (create, or the delete at the end, refused), a device the
    /// action cannot boot from its sandbox, and a booted device left running or
    /// registered after the lease. Also prints whether the action's own `xcodebuild
    /// -showdestinations`, under the sandbox, lists the device: a measurement, not a
    /// check (the check without the sandbox is the test above).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "macOS with an iOS simulator runtime; the native-macos job runs it"]
    async fn a_real_simulator_is_made_booted_and_deleted_for_the_lease() {
        let _one = ONE.lock().await;
        let (device_type, runtime_id) = pick();
        let dir = scratch("sim-real");
        let fixtures = dir.join("fixtures");
        std::fs::create_dir_all(&fixtures).expect("fixtures");
        let project = ios_fixture(&fixtures);
        let config = config(&dir);
        let cas = Arc::new(MemoryCas::default());
        let rt = runtime(config.clone(), &cas);
        let script = format!(
            "set -e\n\
             echo \"${UDID_VAR}\"\n\
             xcrun simctl bootstatus \"${UDID_VAR}\" -b >&2\n\
             xcrun simctl list devices booted | grep -c \"${UDID_VAR}\"\n\
             cp -R {project} \"$TMPDIR/IOSFixture.xcodeproj\" >&2 || true\n\
             echo \"xcodebuild under the sandbox lists it: $(xcodebuild -showdestinations -project \"$TMPDIR/IOSFixture.xcodeproj\" -scheme IOSFixture -derivedDataPath \"$TMPDIR/dd\" 2>&1 | grep -c \"${UDID_VAR}\" || true)\" >&2\n"
        );
        let spec = Spec::sh(&script)
            .property("simulator", &format!("{device_type},{runtime_id}"))
            .timeout(Duration::from_secs(600));
        let action = spec.store(&cas);
        let result = tokio::time::timeout(
            Duration::from_secs(660),
            rt.run(crate::support::work(1, action, 0)),
        )
        .await
        .expect("the run ends")
        .expect("ran");
        let err = stderr(&cas, &result);
        let _ = writeln!(std::io::stderr(), "action stderr:\n{err}");
        assert_eq!(result.exit_code, 0, "{err}");
        let out = stdout(&cas, &result);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        let udid = lines[0];
        assert_eq!(lines[1], "1", "the device booted");
        assert!(
            !xcrun(&["simctl", "list", "devices", "-j"]).contains(udid),
            "the device is deleted"
        );
        assert!(!running(udid), "the device is shut down");
        assert!(no_leases(&config), "the lease directory is removed");
    }

    /// Catches, on a real Mac: a booted simulator a killed daemon's lease left not shut
    /// down and deleted by the next start (it would keep running and fill the node).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "macOS with an iOS simulator runtime; the native-macos job runs it"]
    async fn the_start_deletes_a_real_booted_simulator_left_behind() {
        let _one = ONE.lock().await;
        let (device_type, runtime_id) = pick();
        let dir = scratch("sim-real-left");
        let config = config(&dir);
        let lease = config.scratch.join("lease-9-9");
        std::fs::create_dir_all(&lease).expect("lease");
        std::fs::write(lease.join(MARKER), b"").expect("marker");
        let created = xcrun(&[
            "simctl",
            "create",
            "kbf-lease-9-9",
            &device_type,
            &runtime_id,
        ]);
        let udid = created.trim().to_owned();
        // `boot` returns once the device is booted, without the minutes of data
        // migration `bootstatus` waits for: enough for a device left running.
        xcrun(&["simctl", "boot", &udid]);
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
        assert!(
            !xcrun(&["simctl", "list", "devices", "-j"]).contains(&udid),
            "the left device is deleted"
        );
        assert!(no_leases(&config), "the left lease directory is removed");
    }
}
