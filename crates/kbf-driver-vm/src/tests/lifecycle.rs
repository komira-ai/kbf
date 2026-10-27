//! The VM driver against the fake host, which runs the real `kbf-guest` agent on a Unix
//! socket: a run end to end, and every way a lease ends leaving no VM and no clone.

use std::sync::Arc;
use std::time::Duration;

use super::support::{
    GIB, MemoryCas, Spec, TestClock, config, driver, driver_on, leftovers, name, open_gate,
    outputs_share, scratch, until, until_running, work,
};
use crate::{Clock as _, FakeHost, MACOS_VM_LIMIT, MachineId};
use kbf_daemon::{Runtime, RuntimeError};

/// Catches a VM run that loses the command's stdout or its declared outputs, the
/// action's environment, an input
/// tree the guest cannot read from its working directory, and a lease that leaves its
/// VM running or its clone or lease directory on disk after a clean exit.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_returns_stdout_and_outputs() {
    let dir = scratch("run");
    let config = config(&dir);
    let (host, cas) = (Arc::new(FakeHost::new()), Arc::new(MemoryCas::default()));
    let driver = driver(config.clone(), &host, &cas);
    let out = outputs_share(&config, 1);
    let action = Spec::act(&[
        "cat",
        "in.txt",
        ";",
        "env",
        "GREETING",
        ";",
        "mkdir",
        &format!("{out}/d"),
        ";",
        "write-lines",
        &format!("{out}/d/a.txt"),
        "made",
    ])
    .input("in.txt", b"from the inputs\n")
    .env("GREETING", "hello")
    .outputs(&["d/a.txt"])
    .store(&cas);

    let result = driver.run(work(1, action)).await.expect("ran");

    assert_eq!(result.exit_code, 0);
    let stdout = result.stdout_digest.as_ref().expect("stdout");
    assert_eq!(cas.blob(stdout), b"from the inputs\nhello\n");
    assert_eq!(result.output_files.len(), 1, "{:?}", result.output_files);
    assert_eq!(result.output_files[0].path, "d/a.txt");
    let file = result.output_files[0].digest.as_ref().expect("digest");
    assert_eq!(cas.blob(file), b"made\n");
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
    let off = Spec::ok().property("network", "off").store(&cas);
    let absent = Spec::ok().store(&cas);

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
    // The network is on unless the action turns it off: asking for nothing is asking
    // for it.
    let unsaid = Spec {
        properties: Vec::new(),
        ..Spec::ok()
    };
    for (seq, value) in [(1, "on"), (2, "standard"), (3, "")] {
        let spec = if value.is_empty() {
            unsaid.clone()
        } else {
            Spec::ok().property("network", value)
        };
        let action = spec.store(&cas);
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
    let held: Vec<_> = (1..=2)
        .map(|seq| {
            let driver = Arc::clone(&driver);
            let action = Spec::held(&config, seq).store(&cas);
            tokio::spawn(async move { driver.run(work(seq, action)).await })
        })
        .collect();
    for seq in 1..=2 {
        until_running(&config, seq).await;
    }

    let third = driver.run(work(3, Spec::ok().store(&cas))).await;

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
/// left on disk after its guest never answered. The boot polls on a clock the test
/// owns: it must try every poll up to the boot timeout and no further (30 pauses of
/// 10 ms for 300 ms), which a poll that stops early or runs on would not.
#[tokio::test(flavor = "multi_thread")]
async fn a_boot_timeout_is_an_infrastructure_failure_and_destroys_the_clone() {
    let dir = scratch("boot-timeout");
    let mut config = config(&dir);
    config.boot_timeout = Duration::from_millis(300);
    let host = Arc::new(FakeHost::new().never_ready());
    let cas = Arc::new(MemoryCas::default());
    let clock = TestClock::new();
    let driver = driver_on(config.clone(), &host, &cas, &clock);
    let action = Spec::ok().store(&cas);

    let ended = driver.run(work(1, action)).await;

    assert!(
        matches!(ended, Err(RuntimeError::Failed(ref why)) if why.contains("boot timeout")),
        "{ended:?}"
    );
    assert_eq!(clock.pauses(), 30);
    assert_eq!(clock.now(), config.boot_timeout);
    assert_eq!(host.clones_made(), 1);
    assert_eq!(host.stopped(), [name(1)]);
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches a handshake read left unbounded: a guest whose VM is reachable but which
/// never sends `Hello` (an agent hung before `Ready`) would hold the lease, its VM and
/// one of the host's two VM slots for ever. The read must carry the boot time left (the
/// whole boot timeout, none of it spent); when that bound runs out (the fake host runs it
/// out on the test's say) the lease must end as the farm's failure naming the boot
/// timeout, its VM stopped and its clone removed. The message catches a read timeout
/// reported as a bare OS error ("Resource temporarily unavailable") rather than as a guest
/// not ready in time, and a fake guest that drops its end (an end at once, with another
/// error) does not pass for a silent one.
#[tokio::test(flavor = "multi_thread")]
async fn a_guest_silent_before_ready_hits_the_boot_timeout() {
    let dir = scratch("mute-boot");
    let mut config = config(&dir);
    config.boot_timeout = Duration::from_millis(300);
    let host = Arc::new(FakeHost::new().mute_before_ready());
    let cas = Arc::new(MemoryCas::default());
    let clock = TestClock::new();
    let driver = driver_on(config.clone(), &host, &cas, &clock);
    let action = Spec::ok().store(&cas);
    let run = {
        let driver = Arc::clone(&driver);
        tokio::spawn(async move { driver.run(work(1, action)).await })
    };
    until("the driver waits for Hello", || {
        host.read_bounds().iter().any(Option::is_some)
    })
    .await;
    assert_eq!(host.read_bounds(), [Some(config.boot_timeout)]);

    host.expire_read_bounds();

    let ended = run.await.expect("joined");
    assert!(
        matches!(ended, Err(RuntimeError::Failed(ref why)) if why.contains("not ready within the boot timeout")),
        "{ended:?}"
    );
    assert_eq!(host.stopped(), [name(1)]);
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches a handshake error taken for a slow boot: a guest that refuses the driver's
/// token (one booted with another lease's share, or a token written wrong) must end the
/// lease at once as the farm's failure naming the refusal, not wait out the boot
/// timeout nor be reported as a guest not ready in time. "At once" is read off the
/// boot's clock: the driver paused for no poll, so no time passed.
#[tokio::test(flavor = "multi_thread")]
async fn a_guest_that_refuses_the_token_ends_the_lease_at_once() {
    let dir = scratch("wrong-token");
    let mut config = config(&dir);
    config.boot_timeout = Duration::from_secs(30);
    let host = Arc::new(FakeHost::new().wrong_token());
    let cas = Arc::new(MemoryCas::default());
    let clock = TestClock::new();
    let driver = driver_on(config.clone(), &host, &cas, &clock);
    let action = Spec::ok().store(&cas);

    let ended = driver.run(work(1, action)).await;

    assert!(
        matches!(ended, Err(RuntimeError::Failed(ref why)) if why.contains("wrong token") && !why.contains("not ready")),
        "{ended:?}"
    );
    assert_eq!((clock.pauses(), clock.now()), (0, Duration::ZERO));
    assert_eq!(host.stopped(), [name(1)]);
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches the handshake's read bound left on the socket after `Ready`: the command's
/// report would then be awaited with a read timeout of the boot time left, so every
/// action that runs longer than that (minutes, against a two-minute boot timeout)
/// would fail as the farm's error and be rerun elsewhere, failing again. While the
/// command runs the stream must have no bound; the fake host runs out any bound there
/// is, and a command held until then must still finish.
#[tokio::test(flavor = "multi_thread")]
async fn the_handshakes_read_bound_does_not_outlive_the_handshake() {
    let dir = scratch("past-boot");
    let mut config = config(&dir);
    config.boot_timeout = Duration::from_millis(300);
    let host = Arc::new(FakeHost::new());
    let cas = Arc::new(MemoryCas::default());
    let driver = driver(config.clone(), &host, &cas);
    let action = Spec::held(&config, 1).store(&cas);
    let run = {
        let driver = Arc::clone(&driver);
        tokio::spawn(async move { driver.run(work(1, action)).await })
    };
    until_running(&config, 1).await;
    assert_eq!(host.read_bounds(), [None]);

    host.expire_read_bounds();
    open_gate(&config, 1);

    let result = run
        .await
        .expect("joined")
        .expect("ran past the boot timeout");
    assert_eq!(result.exit_code, 0);
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches a boot that takes every host error for "not ready yet" and retries it: a VM
/// that dies while it boots would hold its lease until the boot timeout (two minutes
/// by default) and be reported as a slow boot. The VM crashes at the third poll; the
/// lease must end there, as the farm's failure naming the host's error, with nothing
/// left behind. The boot's clock shows it: three pauses, 30 ms of its 30 s.
#[tokio::test(flavor = "multi_thread")]
async fn a_vm_that_dies_while_booting_ends_the_lease_at_once() {
    let dir = scratch("dies-booting");
    let mut config = config(&dir);
    config.boot_timeout = Duration::from_secs(30);
    let host = Arc::new(FakeHost::new().never_ready());
    let cas = Arc::new(MemoryCas::default());
    let clock = TestClock::new();
    let hook_host = Arc::clone(&host);
    clock.on_pause(move |pause| {
        if pause == 3 {
            hook_host.crash(&name(1));
        }
    });
    let driver = driver_on(config.clone(), &host, &cas, &clock);
    let action = Spec::ok().store(&cas);

    let ended = driver.run(work(1, action)).await;

    assert!(
        matches!(ended, Err(RuntimeError::Failed(ref why)) if why.contains("not running")),
        "{ended:?}"
    );
    assert_eq!(clock.pauses(), 3);
    assert_eq!(clock.now(), config.boot_poll * 3);
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches the boot charged to the action's timeout. The guest takes 1200 polls of one
/// second to answer, 20 minutes of the boot's clock; the guest must still be given the
/// action's whole 90 s with the `Run`, and the driver's watchdog on its report must be
/// armed for the timeout and the grace from the moment the command starts, not from the
/// lease's start (a boot charged would send 90 s less the boot, which is none). The second
/// lease is the control: a command held past its own 1 ms timeout is still ended by the
/// guest, as the action's timeout (it waits for a gate no one opens, so the timeout
/// ends it however slow the machine is).
#[tokio::test(flavor = "multi_thread")]
async fn the_boot_is_not_charged_to_the_actions_timeout() {
    let dir = scratch("boot-time");
    let mut config = config(&dir);
    config.boot_poll = Duration::from_secs(1);
    config.boot_timeout = Duration::from_secs(7200);
    config.report_grace = Duration::from_secs(30);
    let host = Arc::new(FakeHost::new().ready_after_connects(1200));
    let cas = Arc::new(MemoryCas::default());
    let clock = TestClock::new();
    let driver = driver_on(config.clone(), &host, &cas, &clock);
    let timeout = Duration::from_secs(90);
    let fits = Spec::ok().timeout(timeout).store(&cas);
    let overruns = Spec::held(&config, 2)
        .timeout(Duration::from_millis(1))
        .store(&cas);

    let result = driver
        .run(work(1, fits))
        .await
        .expect("ran within its timeout");
    let (polls, booted) = (clock.pauses(), clock.now());
    let ended = driver.run(work(2, overruns)).await;

    assert_eq!(result.exit_code, 0);
    assert_eq!((polls, booted), (1200, Duration::from_secs(1200)));
    let sent: Vec<u64> = host.requests().iter().map(|r| r.timeout_ms).collect();
    assert_eq!(sent, [90_000, 1]);
    let watched = [timeout, Duration::from_millis(1)].map(|t| t + config.report_grace);
    assert_eq!(clock.sleeps(), watched);
    assert!(
        matches!(ended, Err(RuntimeError::TimedOut { .. })),
        "{ended:?}"
    );
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
    let action = Spec::held(&config, 1).store(&cas);
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
    let action = Spec::held(&config, 1).store(&cas);
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
/// and the VM it started left running. The boot's clock is held: no poll passes and no
/// time with it, so only the kill can end the lease, and it does so before any time has
/// passed.
#[tokio::test(flavor = "multi_thread")]
async fn a_kill_during_the_boot_ends_the_lease_at_once() {
    let dir = scratch("kill-boot");
    let config = config(&dir);
    let host = Arc::new(FakeHost::new().never_ready());
    let cas = Arc::new(MemoryCas::default());
    let clock = TestClock::held();
    let driver = driver_on(config.clone(), &host, &cas, &clock);
    let action = Spec::ok().store(&cas);
    let run = {
        let driver = Arc::clone(&driver);
        tokio::spawn(async move { driver.run(work(1, action)).await })
    };
    until("the boot waits for a poll", || clock.pauses() >= 1).await;

    driver.kill(kbf_types::LeaseId::new(1, 1)).await;

    let ended = run.await.expect("joined");
    assert!(matches!(ended, Err(RuntimeError::Killed)), "{ended:?}");
    assert_eq!(clock.now(), Duration::ZERO);
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
    // The poll thread cannot be cancelled: let it see the VM gone and end.
    clock.release();
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
    let action = Spec::ok().store(&cas);

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
    let action = Spec::ok().store(&cas);

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
    let reserved = Spec::ok().outputs(&["stdout"]).store(&cas);
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
    let mut lease = work(1, Spec::ok().store(&cas));
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
