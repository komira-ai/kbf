//! The VM driver's less common paths against the fake host: malformed actions, a
//! silent guest and a slow one, a host that cannot confirm a stop, a run the daemon drops, and kills
//! before the VM exists.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::cas::{digest_of, label};
use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_vm::FakeHost;
use kbf_proto::reapi::{Action, Command, Directory};
use kbf_types::LeaseId;
use prost::Message as _;
use support::{
    MemoryCas, Spec, config, driver, leftovers, outputs_share, scratch, until_running, work,
};

/// Catches an Action without a Command or an input root, or a Command without
/// arguments, sent to a VM (or reported as the farm's, to be run again elsewhere)
/// rather than refused as the client's error before anything is cloned.
#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_action_is_the_clients_error() {
    let dir = scratch("malformed");
    let config = config(&dir);
    let (host, cas) = (
        Arc::new(FakeHost::default()),
        Arc::new(MemoryCas::default()),
    );
    let driver = driver(config.clone(), &host, &cas);
    let root = Some(cas.insert(Directory::default().encode_to_vec()));
    let command = |arguments: &[&str]| {
        let command = Command {
            arguments: arguments.iter().map(|a| (*a).to_owned()).collect(),
            ..Command::default()
        };
        Some(cas.insert(command.encode_to_vec()))
    };
    let actions = [
        Action {
            input_root_digest: root.clone(),
            ..Action::default()
        },
        Action {
            command_digest: command(&["true"]),
            ..Action::default()
        },
        Action {
            command_digest: command(&[]),
            input_root_digest: root,
            ..Action::default()
        },
    ];

    for (seq, action) in (1..).zip(actions) {
        let digest = cas.insert(action.encode_to_vec());
        let ended = driver.run(work(seq, digest)).await;
        assert!(
            matches!(ended, Err(RuntimeError::Invalid(_))),
            "{seq}: {ended:?}"
        );
    }
    assert_eq!(host.clones_made(), 0);
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches an input blob missing from the CAS reported as the farm's (or the run going
/// on without it) rather than as the blob the client has to upload.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_input_blob_is_named_for_the_client() {
    let dir = scratch("missing-blob");
    let config = config(&dir);
    let (host, cas) = (
        Arc::new(FakeHost::default()),
        Arc::new(MemoryCas::default()),
    );
    let driver = driver(config.clone(), &host, &cas);
    let action = Spec::sh("true").input("gone.txt", b"gone").store(&cas);
    let gone = digest_of(b"gone");
    cas.remove(&gone);

    let ended = driver.run(work(1, action)).await;

    assert!(
        matches!(ended, Err(RuntimeError::MissingBlob(ref blob)) if *blob == label(&gone)),
        "{ended:?}"
    );
    assert_eq!(host.clones_made(), 0);
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches the working directory ignored (the command run at the top of the input tree)
/// and one made outside the inputs share's `root/`.
#[tokio::test(flavor = "multi_thread")]
async fn the_command_runs_in_its_working_directory() {
    let dir = scratch("cwd");
    let config = config(&dir);
    let (host, cas) = (
        Arc::new(FakeHost::default()),
        Arc::new(MemoryCas::default()),
    );
    let driver = driver(config.clone(), &host, &cas);
    let action = Spec::sh("pwd").working_directory("sub/dir").store(&cas);

    let result = driver.run(work(1, action)).await.expect("ran");

    let stdout = cas.blob(result.stdout_digest.as_ref().expect("stdout"));
    let lease = config.leases().join(support::name(1));
    let expected = lease.join("inputs/root/sub/dir").display().to_string();
    assert_eq!(
        String::from_utf8(stdout).expect("utf-8").trim_end(),
        expected
    );
}

/// Catches a guest that hangs mid-run waited on for ever: the driver ends the lease,
/// as the farm's, once the report is later than the action's timeout and the grace,
/// and destroys the VM.
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_guest_ends_the_lease_after_the_grace() {
    let dir = scratch("silent");
    let mut config = config(&dir);
    config.report_grace = Duration::from_millis(200);
    let host = Arc::new(FakeHost::new().mute_after_start());
    let cas = Arc::new(MemoryCas::default());
    let driver = driver(config.clone(), &host, &cas);
    let action = Spec::sh("true")
        .timeout(Duration::from_millis(300))
        .store(&cas);

    let ended = tokio::time::timeout(Duration::from_secs(5), driver.run(work(1, action)))
        .await
        .expect("ended after the grace");

    assert!(
        matches!(ended, Err(RuntimeError::Failed(ref why)) if why.contains("no report")),
        "{ended:?}"
    );
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches the report watchdog armed for the grace alone (or any span shorter than the
/// action's timeout plus the grace): a command that runs 1 s of its 5 s timeout must
/// finish under a 300 ms grace. With the defect, every action longer than the grace (30
/// s by default) would be ended as the farm's failure and run again elsewhere.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_longer_than_the_grace_finishes_within_its_timeout() {
    let dir = scratch("past-grace");
    let mut config = config(&dir);
    config.report_grace = Duration::from_millis(300);
    let host = Arc::new(FakeHost::new());
    let cas = Arc::new(MemoryCas::default());
    let driver = driver(config.clone(), &host, &cas);
    let action = Spec::sh("sleep 1; echo done")
        .timeout(Duration::from_secs(5))
        .store(&cas);

    let result = driver.run(work(1, action)).await;

    let result = result.expect("ran within its timeout");
    assert_eq!(result.exit_code, 0);
    assert_eq!(
        cas.blob(result.stdout_digest.as_ref().expect("stdout")),
        b"done\n"
    );
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches a stop the host cannot confirm taken for a clean end: before the outputs
/// are read it fails the lease; at the destroy of a run that went well it fails the
/// lease too (a VM that may still run is a dirty node). The clone and lease directory
/// are removed in both cases.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_the_host_cannot_confirm_fails_the_lease() {
    for (ok, expected) in [(0, "stopping the VM"), (1, "destroy: stop the VM")] {
        let dir = scratch(&format!("stop-fails-{ok}"));
        let config = config(&dir);
        let host = Arc::new(FakeHost::new().failing_stops_after(ok));
        let cas = Arc::new(MemoryCas::default());
        let driver = driver(config.clone(), &host, &cas);
        let action = Spec::sh("true").store(&cas);

        let ended = driver.run(work(1, action)).await;

        assert!(
            matches!(ended, Err(RuntimeError::Failed(ref why)) if why.contains(expected)),
            "{ok}: {ended:?}"
        );
        assert!(host.running().is_empty(), "{:?}", host.running());
        assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
    }
}

/// Catches a run the daemon drops (its task aborted) that leaves its VM running or its
/// clone on disk: nothing else would ever remove them while the daemon runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_run_destroys_its_vm() {
    let dir = scratch("dropped");
    let config = config(&dir);
    let (host, cas) = (
        Arc::new(FakeHost::default()),
        Arc::new(MemoryCas::default()),
    );
    let driver = driver(config.clone(), &host, &cas);
    let action = Spec::sh("sleep 30").store(&cas);
    let run = {
        let driver = Arc::clone(&driver);
        tokio::spawn(async move { driver.run(work(1, action)).await })
    };
    until_running(&config, 1).await;

    run.abort();
    let aborted = run.await.expect_err("aborted");

    assert!(aborted.is_cancelled());
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches a kill during the input fetch that waits for the fetch to finish, or goes
/// on to clone and boot a VM.
#[tokio::test(flavor = "multi_thread")]
async fn a_kill_during_the_input_fetch_stops_before_the_clone() {
    let dir = scratch("kill-fetch");
    let config = config(&dir);
    let host = Arc::new(FakeHost::default());
    let cas = Arc::new(MemoryCas::delayed(Duration::from_secs(5)));
    let driver = driver(config.clone(), &host, &cas);
    let action = Spec::sh("true").store(&cas);
    let run = {
        let driver = Arc::clone(&driver);
        tokio::spawn(async move { driver.run(work(1, action)).await })
    };
    // Well inside the first fetch's delay.
    tokio::time::sleep(Duration::from_millis(200)).await;

    tokio::time::timeout(Duration::from_secs(2), driver.kill(LeaseId::new(1, 1)))
        .await
        .expect("the kill did not wait for the fetch");

    let ended = run.await.expect("joined");
    assert!(matches!(ended, Err(RuntimeError::Killed)), "{ended:?}");
    assert_eq!(host.clones_made(), 0);
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches a kill of a lease the driver is not running that blocks or fails, and an
/// image that cannot be cloned reported as anything but the farm's, its partial clone
/// left on disk.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_kill_returns_and_an_unclonable_image_is_the_farms() {
    let dir = scratch("unclonable");
    let mut config = config(&dir);
    config.image = PathBuf::from(outputs_share(&config, 99));
    let (host, cas) = (
        Arc::new(FakeHost::default()),
        Arc::new(MemoryCas::default()),
    );
    let driver = driver(config.clone(), &host, &cas);

    tokio::time::timeout(Duration::from_secs(2), driver.kill(LeaseId::new(7, 7)))
        .await
        .expect("returned at once");
    let ended = driver.run(work(1, Spec::sh("true").store(&cas))).await;

    assert!(matches!(ended, Err(RuntimeError::Failed(_))), "{ended:?}");
    assert!(host.running().is_empty(), "{:?}", host.running());
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches an output path that leaves the working directory sent to the guest (or
/// reported as the farm's) rather than refused as the client's, and a corrupt blob in
/// the CAS reported as the client's to upload rather than as the farm's.
#[tokio::test(flavor = "multi_thread")]
async fn a_bad_output_path_is_the_clients_and_a_corrupt_blob_the_farms() {
    let dir = scratch("bad-paths");
    let config = config(&dir);
    let (host, cas) = (
        Arc::new(FakeHost::default()),
        Arc::new(MemoryCas::default()),
    );
    let driver = driver(config.clone(), &host, &cas);
    let escapes = Spec::sh("true").outputs(&["../out"]).store(&cas);
    let corrupt = Spec::sh("true").input("in.txt", b"good").store(&cas);
    cas.corrupt(&digest_of(b"good"));

    let refused = driver.run(work(1, escapes)).await;
    let failed = driver.run(work(2, corrupt)).await;

    assert!(
        matches!(refused, Err(RuntimeError::Invalid(_))),
        "{refused:?}"
    );
    assert!(matches!(failed, Err(RuntimeError::Failed(_))), "{failed:?}");
    assert_eq!(host.clones_made(), 0);
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}

/// Catches outputs past the output limits stored anyway (or reported as the client's),
/// and an output directory holding a name that is not UTF-8 reported as the farm's,
/// which would run the same action again for the same refusal.
#[tokio::test(flavor = "multi_thread")]
async fn outputs_past_the_limits_are_the_farms_and_a_bad_name_the_clients() {
    let dir = scratch("output-limits");
    let mut config = config(&dir);
    config.outputs.max_bytes = 4;
    let (host, cas) = (
        Arc::new(FakeHost::default()),
        Arc::new(MemoryCas::default()),
    );
    let driver = driver(config.clone(), &host, &cas);
    let big = Spec::sh(r#"printf 0123456789 > "$OUT/big""#)
        .env("OUT", &outputs_share(&config, 1))
        .outputs(&["big"])
        .store(&cas);
    // Linux allows any bytes but `/` and NUL in a name; APFS refuses this one.
    let bad_name = Spec::sh(r#"mkdir "$OUT/d" && : > "$OUT/d/$(printf '\377')""#)
        .env("OUT", &outputs_share(&config, 2))
        .outputs(&["d"])
        .store(&cas);

    let too_big = driver.run(work(1, big)).await;
    let named = driver.run(work(2, bad_name)).await;

    assert!(
        matches!(too_big, Err(RuntimeError::Failed(_))),
        "{too_big:?}"
    );
    if cfg!(target_os = "linux") {
        assert!(matches!(named, Err(RuntimeError::Invalid(_))), "{named:?}");
    }
    assert!(leftovers(&config).is_empty(), "{:?}", leftovers(&config));
}
