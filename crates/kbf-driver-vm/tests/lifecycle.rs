//! The VM driver against the fake host, which runs the real `kbf-guest` agent on a Unix
//! socket: a run end to end, and every way a lease ends leaving no VM and no clone.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_vm::{FakeHost, MACOS_VM_LIMIT, MachineId};
use support::{
    GIB, MemoryCas, Spec, config, driver, leftovers, name, outputs_share, scratch, until,
    until_running, work,
};

/// Catches a VM run that loses the command's stdout or its declared outputs, an input
/// tree the guest cannot read from its working directory, and a lease that leaves its
/// VM running or its clone or lease directory on disk after a clean exit.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_returns_stdout_and_outputs() {
    let dir = scratch("run");
    let config = config(&dir);
    let (host, cas) = (Arc::new(FakeHost::new()), Arc::new(MemoryCas::default()));
    let driver = driver(config.clone(), &host, &cas);
    let action = Spec::sh(r#"cat in.txt; mkdir -p "$OUT/d"; printf made > "$OUT/d/a.txt""#)
        .input("in.txt", b"from the inputs\n")
        .env("OUT", &outputs_share(&config, 1))
        .outputs(&["d/a.txt"])
        .store(&cas);

    let result = driver.run(work(1, action)).await.expect("ran");

    assert_eq!(result.exit_code, 0);
    let stdout = result.stdout_digest.as_ref().expect("stdout");
    assert_eq!(cas.blob(stdout), b"from the inputs\n");
    assert_eq!(result.output_files.len(), 1, "{:?}", result.output_files);
    assert_eq!(result.output_files[0].path, "d/a.txt");
    let file = result.output_files[0].digest.as_ref().expect("digest");
    assert_eq!(cas.blob(file), b"made");
    assert_eq!(host.clones_made(), 1);
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches a VM that gets a writable inputs share (a guest could change the input tree
/// or its own token), a read-only outputs share, a network device it did not ask for,
/// the booking passed through unsized (the helper's 1 GiB not taken off), a machine
/// identifier shared by two leases, and a VM booted from the golden image itself rather
/// than a clone of its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_vm_gets_its_size_a_read_only_inputs_share_and_no_network_device() {
    let dir = scratch("spec");
    let config = config(&dir);
    let (host, cas) = (Arc::new(FakeHost::new()), Arc::new(MemoryCas::default()));
    let driver = driver(config.clone(), &host, &cas);
    let off = Spec::sh("true").property("network", "off").store(&cas);
    let absent = Spec::sh("true").store(&cas);

    driver.run(work(1, off)).await.expect("ran");
    driver.run(work(2, absent)).await.expect("ran");

    let specs = host.specs();
    assert_eq!(specs.len(), 2);
    for (spec, seq) in specs.iter().zip([1, 2]) {
        assert_eq!(spec.name, name(seq));
        assert_eq!(spec.clone, config.clones().join(name(seq)));
        assert_eq!((spec.cpus, spec.memory_bytes), (6, 16 * GIB));
        assert!(spec.network.is_empty(), "{:?}", spec.network);
        let shares: Vec<(&str, bool)> = spec.shares.iter().map(|s| (s.tag, s.read_only)).collect();
        assert_eq!(shares, [("inputs", true), ("outputs", false)]);
        let lease = config.leases().join(name(seq));
        assert_eq!(spec.shares[0].host, lease.join("inputs"));
        assert_eq!(spec.shares[1].host, lease.join("outputs"));
    }
    assert_ne!(specs[0].machine_id, specs[1].machine_id);
    assert_ne!(specs[0].machine_id, MachineId([0; 16]));
}

/// Catches network-on leases let through (the guest would reach the farm's own
/// network) or refused only after a clone was made, and the refusal reported as the
/// farm's rather than the client's.
#[tokio::test(flavor = "multi_thread")]
async fn a_network_on_lease_is_refused_before_anything_is_cloned() {
    let dir = scratch("network-on");
    let config = config(&dir);
    let (host, cas) = (Arc::new(FakeHost::new()), Arc::new(MemoryCas::default()));
    let driver = driver(config.clone(), &host, &cas);
    for (seq, value) in [(1, "on"), (2, "standard")] {
        let action = Spec::sh("true").property("network", value).store(&cas);
        let refused = driver.run(work(seq, action)).await;
        assert!(
            matches!(refused, Err(RuntimeError::Invalid(ref why)) if why.contains("network")),
            "{value}: {refused:?}"
        );
    }
    assert_eq!(host.clones_made(), 0);
    assert!(host.specs().is_empty());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches the host's refusal of a third VM reported as the action's failure (an
/// exit code or INVALID_ARGUMENT, which the farm would not run again elsewhere) rather
/// than an infrastructure failure, and the refused lease's clone left on disk.
#[tokio::test(flavor = "multi_thread")]
async fn a_third_vm_is_refused_as_an_infrastructure_failure() {
    let dir = scratch("limit");
    let config = config(&dir);
    let (host, cas) = (Arc::new(FakeHost::new()), Arc::new(MemoryCas::default()));
    let driver = driver(config.clone(), &host, &cas);
    let long = Spec::sh("sleep 30").store(&cas);
    let held: Vec<_> = (1..=2)
        .map(|seq| {
            let (driver, action) = (Arc::clone(&driver), long.clone());
            tokio::spawn(async move { driver.run(work(seq, action)).await })
        })
        .collect();
    for seq in 1..=2 {
        until_running(&config, seq).await;
    }

    let third = driver.run(work(3, Spec::sh("true").store(&cas))).await;

    assert!(
        matches!(third, Err(RuntimeError::Failed(ref why)) if why.contains(&format!("limit of {MACOS_VM_LIMIT}"))),
        "{third:?}"
    );
    assert_eq!(host.clones_made(), 3);
    let mut left = leftovers(&config);
    left.sort();
    let mut expected: Vec<_> = [config.leases(), config.clones()]
        .iter()
        .flat_map(|d| [d.join(name(1)), d.join(name(2))])
        .collect();
    expected.sort();
    assert_eq!(left, expected, "only the two running leases have files");
    for seq in 1..=2 {
        driver.kill(kbf_types::LeaseId::new(1, seq)).await;
    }
    for run in held {
        let ended = run.await.expect("joined");
        assert!(matches!(ended, Err(RuntimeError::Killed)), "{ended:?}");
    }
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches a boot that waits forever, a boot timeout reported as the action's (a
/// timeout or an exit code) rather than the farm's, and a VM left running or its clone
/// left on disk after its guest never answered.
#[tokio::test(flavor = "multi_thread")]
async fn a_boot_timeout_is_an_infrastructure_failure_and_destroys_the_clone() {
    let dir = scratch("boot-timeout");
    let mut config = config(&dir);
    config.boot_timeout = Duration::from_millis(300);
    let host = Arc::new(FakeHost::new().never_ready());
    let cas = Arc::new(MemoryCas::default());
    let driver = driver(config.clone(), &host, &cas);
    let action = Spec::sh("true").store(&cas);

    let ended = driver.run(work(1, action)).await;

    assert!(
        matches!(ended, Err(RuntimeError::Failed(ref why)) if why.contains("boot timeout")),
        "{ended:?}"
    );
    assert_eq!(host.clones_made(), 1);
    assert_eq!(host.stopped(), [name(1)]);
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches a handshake read left unbounded: a guest whose VM is reachable but which
/// never sends `Hello` (an agent hung before `Ready`) would hold the lease, its VM and
/// one of the host's two VM slots for ever. It must end at the boot timeout as the
/// farm's failure, its VM stopped and its clone removed.
#[tokio::test(flavor = "multi_thread")]
async fn a_guest_silent_before_ready_hits_the_boot_timeout() {
    let dir = scratch("mute-boot");
    let mut config = config(&dir);
    config.boot_timeout = Duration::from_millis(300);
    let host = Arc::new(FakeHost::new().mute_before_ready());
    let cas = Arc::new(MemoryCas::default());
    let driver = driver(config.clone(), &host, &cas);
    let action = Spec::sh("true").store(&cas);

    let ended = tokio::time::timeout(Duration::from_secs(5), driver.run(work(1, action)))
        .await
        .expect("ended near the boot timeout");

    assert!(matches!(ended, Err(RuntimeError::Failed(_))), "{ended:?}");
    assert_eq!(host.stopped(), [name(1)]);
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches a boot that takes every host error for "not ready yet" and retries it: a VM
/// that dies while it boots would hold its lease until the boot timeout (two minutes
/// by default) and be reported as a slow boot. The lease must end at once, as the
/// farm's failure naming the host's error, with nothing left behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_vm_that_dies_while_booting_ends_the_lease_at_once() {
    let dir = scratch("dies-booting");
    let mut config = config(&dir);
    config.boot_timeout = Duration::from_secs(30);
    let host = Arc::new(FakeHost::new().ready_after(Duration::from_secs(60)));
    let cas = Arc::new(MemoryCas::default());
    let driver = driver(config.clone(), &host, &cas);
    let action = Spec::sh("true").store(&cas);
    let run = {
        let driver = Arc::clone(&driver);
        tokio::spawn(async move { driver.run(work(1, action)).await })
    };
    until("the VM started", || !host.running().is_empty()).await;

    host.crash(&name(1));

    let ended = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("ended long before the boot timeout")
        .expect("joined");
    assert!(
        matches!(ended, Err(RuntimeError::Failed(ref why)) if why.contains("not running")),
        "{ended:?}"
    );
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches the boot charged to the action's timeout: the guest takes 1.2 s to answer
/// and the action, which runs for 0.8 s of its 1.5 s, would be cut off after 0.3 s.
/// The second lease is the control: an action that runs past its own timeout is
/// still ended by it, as the action's timeout.
#[tokio::test(flavor = "multi_thread")]
async fn the_boot_is_not_charged_to_the_actions_timeout() {
    let dir = scratch("boot-time");
    let config = config(&dir);
    let host = Arc::new(FakeHost::new().ready_after(Duration::from_millis(1200)));
    let cas = Arc::new(MemoryCas::default());
    let driver = driver(config.clone(), &host, &cas);
    let fits = Spec::sh("sleep 0.8; echo done")
        .timeout(Duration::from_millis(1500))
        .store(&cas);
    let overruns = Spec::sh("sleep 5")
        .timeout(Duration::from_millis(500))
        .store(&cas);

    let result = driver
        .run(work(1, fits))
        .await
        .expect("ran within its timeout");
    let ended = driver.run(work(2, overruns)).await;

    assert_eq!(result.exit_code, 0);
    assert_eq!(
        cas.blob(result.stdout_digest.as_ref().expect("stdout")),
        b"done\n"
    );
    assert!(matches!(ended, Err(RuntimeError::TimedOut)), "{ended:?}");
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches a guest that dies mid-run taken for a result (or waited on forever), its
/// end reported as the action's, and its VM or clone left behind because the run
/// ended with an error.
#[tokio::test(flavor = "multi_thread")]
async fn a_guest_killed_mid_run_is_destroyed_with_no_clone_left() {
    let dir = scratch("crash");
    let config = config(&dir);
    let (host, cas) = (Arc::new(FakeHost::new()), Arc::new(MemoryCas::default()));
    let driver = driver(config.clone(), &host, &cas);
    let action = Spec::sh("sleep 30").store(&cas);
    let run = {
        let driver = Arc::clone(&driver);
        tokio::spawn(async move { driver.run(work(1, action)).await })
    };
    until_running(&config, 1).await;

    host.crash(&name(1));
    let ended = run.await.expect("joined");

    assert!(matches!(ended, Err(RuntimeError::Failed(_))), "{ended:?}");
    assert_eq!(host.stopped(), [name(1)]);
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches a kill that leaves the VM running or its clone on disk, and one that
/// returns before they are gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_kill_stops_the_vm_and_removes_the_clone_before_it_returns() {
    let dir = scratch("kill");
    let config = config(&dir);
    let (host, cas) = (Arc::new(FakeHost::new()), Arc::new(MemoryCas::default()));
    let driver = driver(config.clone(), &host, &cas);
    let action = Spec::sh("sleep 30").store(&cas);
    let run = {
        let driver = Arc::clone(&driver);
        tokio::spawn(async move { driver.run(work(1, action)).await })
    };
    until_running(&config, 1).await;

    driver.kill(kbf_types::LeaseId::new(1, 1)).await;

    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
    let ended = run.await.expect("joined");
    assert!(matches!(ended, Err(RuntimeError::Killed)), "{ended:?}");
}

/// Catches a kill that arrives while the guest boots ignored until the boot timeout,
/// and the VM it started left running.
#[tokio::test(flavor = "multi_thread")]
async fn a_kill_during_the_boot_ends_the_lease_at_once() {
    let dir = scratch("kill-boot");
    let config = config(&dir);
    let host = Arc::new(FakeHost::new().never_ready());
    let cas = Arc::new(MemoryCas::default());
    let driver = driver(config.clone(), &host, &cas);
    let action = Spec::sh("true").store(&cas);
    let run = {
        let driver = Arc::clone(&driver);
        tokio::spawn(async move { driver.run(work(1, action)).await })
    };
    until("the VM started", || !host.running().is_empty()).await;

    driver.kill(kbf_types::LeaseId::new(1, 1)).await;

    let ended = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("ended before the boot timeout")
        .expect("joined");
    assert!(matches!(ended, Err(RuntimeError::Killed)), "{ended:?}");
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches a restarted driver that leaves the clones and lease directories a crashed
/// daemon left (each clone holds its disk space for good), and one that empties only
/// the top level of them.
#[test]
fn a_restarted_driver_removes_orphan_clones() {
    let dir = scratch("orphans");
    let config = config(&dir);
    for orphan in [config.clones(), config.leases()].map(|d| d.join(name(9))) {
        std::fs::create_dir_all(orphan.join("data/deep")).expect("orphan");
        std::fs::write(orphan.join("disk.img"), b"written by the guest").expect("disk");
        std::fs::write(orphan.join("data/deep/aux.img"), b"aux").expect("aux");
    }
    assert_eq!(leftovers(&config).len(), 2);
    let (host, cas) = (Arc::new(FakeHost::new()), Arc::new(MemoryCas::default()));

    let _driver = driver(config.clone(), &host, &cas);

    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches the free-space check skipped, or made after the clone (the clone would fill
/// the volume first), and its failure reported as the client's.
#[tokio::test(flavor = "multi_thread")]
async fn too_little_free_space_fails_before_the_clone() {
    let dir = scratch("free-space");
    let mut config = config(&dir);
    config.min_free_bytes = u64::MAX;
    let (host, cas) = (Arc::new(FakeHost::new()), Arc::new(MemoryCas::default()));
    let driver = driver(config.clone(), &host, &cas);
    let action = Spec::sh("true").store(&cas);

    let ended = driver.run(work(1, action)).await;

    assert!(
        matches!(ended, Err(RuntimeError::Failed(ref why)) if why.contains("bytes free")),
        "{ended:?}"
    );
    assert_eq!(host.clones_made(), 0);
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches a guest whose agent is not in a logged-in GUI session accepted (UI tests
/// would fail in it as the action's), and the refusal reported as the client's.
#[tokio::test(flavor = "multi_thread")]
async fn a_guest_outside_the_gui_session_is_refused_as_the_farms() {
    let dir = scratch("session");
    let config = config(&dir);
    let host = Arc::new(FakeHost::new().session("Background"));
    let cas = Arc::new(MemoryCas::default());
    let driver = driver(config.clone(), &host, &cas);
    let action = Spec::sh("true").store(&cas);

    let ended = driver.run(work(1, action)).await;

    assert!(
        matches!(ended, Err(RuntimeError::Failed(ref why)) if why.contains("Background")),
        "{ended:?}"
    );
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches a request the guest refuses as the action's own error (an output named like
/// the guest's stdout file, a program that does not exist) reported as the farm's,
/// which would run it again elsewhere for the same refusal.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_the_guest_refuses_is_the_clients_error() {
    let dir = scratch("refused");
    let config = config(&dir);
    let (host, cas) = (Arc::new(FakeHost::new()), Arc::new(MemoryCas::default()));
    let driver = driver(config.clone(), &host, &cas);
    let reserved = Spec::sh("true").outputs(&["stdout"]).store(&cas);
    let missing = Spec::argv(&["/no/such/program"]).store(&cas);

    for (seq, action) in [(1, reserved), (2, missing)] {
        let ended = driver.run(work(seq, action)).await;
        assert!(
            matches!(ended, Err(RuntimeError::Invalid(_))),
            "{seq}: {ended:?}"
        );
    }
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches a lease the scheduler booked too little memory for started anyway (its
/// guest would get none past the helper's share), and the refusal made after the
/// clone.
#[tokio::test(flavor = "multi_thread")]
async fn a_booking_too_small_for_a_vm_fails_before_the_clone() {
    let dir = scratch("small");
    let config = config(&dir);
    let (host, cas) = (Arc::new(FakeHost::new()), Arc::new(MemoryCas::default()));
    let driver = driver(config.clone(), &host, &cas);
    let mut lease = work(1, Spec::sh("true").store(&cas));
    lease.resources = kbf_types::Resources::new(1000, GIB);

    let ended = driver.run(lease).await;

    assert!(matches!(ended, Err(RuntimeError::Failed(_))), "{ended:?}");
    assert_eq!(host.clones_made(), 0);
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches the driver claiming another lease kind (an `action` lease would boot a VM)
/// or reporting another driver name.
#[test]
fn the_driver_serves_vm_leases_only() {
    let dir = scratch("kind");
    let (host, cas) = (Arc::new(FakeHost::new()), Arc::new(MemoryCas::default()));
    let driver = driver(config(&dir), &host, &cas);
    assert_eq!(driver.driver(), "vm");
    assert!(driver.serves("vm"));
    assert!(!driver.serves("action"));
    assert!(!driver.serves("whole_machine"));
}
