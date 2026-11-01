//! The whole-machine runtime over a fake `kbf-mac-session` (`fake.rs`): the order of the
//! session switch around the action, its two restarts (a run dropped mid-call, then a
//! new runtime over the same directory, as a restarted daemon builds it), the
//! quarantine after a failed cleanup, and the kill. The real helper's verbs are
//! `kbf-mac-session`'s tests; the GUI session itself is its root tests'
//! (`crates/kbf-mac-session/tests/as_root.rs`).

#[path = "whole_machine/fake.rs"]
mod fake;
mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fake::FakeSession;
use kbf_daemon::{Runtime, RuntimeError, Work};
use kbf_driver_native::whole_machine::state::{Files, LEASE_FILE, QUARANTINE_FILE, Record};
use kbf_driver_native::whole_machine::{DRIVER, KIND, WholeMachineConfig, WholeMachineRuntime};
use kbf_proto::reapi::Digest;
use kbf_types::{LeaseId, LostWhy, NodePhase, Resources};
use support::{MemoryCas, PROMPT, Spec, scratch, stdout};

type Rt = WholeMachineRuntime<FakeSession, MemoryCas>;

fn config(dir: &Path) -> WholeMachineConfig {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let mut config = WholeMachineConfig::new(dir.join("whole"));
    config.session_poll = Duration::from_millis(5);
    config.session_wait = PROMPT;
    config
}

/// A runtime (as a daemon starts it) whose verbs the fake logs with its phase.
fn start(config: &WholeMachineConfig, fake: &FakeSession, cas: &Arc<MemoryCas>) -> Arc<Rt> {
    fake.state().phase = None;
    let runtime = WholeMachineRuntime::new(config.clone(), fake.clone(), Arc::clone(cas))
        .expect("the runtime starts");
    fake.state().phase = Some(runtime.phase());
    Arc::new(runtime)
}

fn lease() -> LeaseId {
    LeaseId::new(1, 1)
}

fn work(action: Digest) -> Work {
    Work {
        lease_id: lease(),
        kind: KIND.to_owned(),
        action_digest: action,
        resources: Resources::new(12_000, 32 << 30),
    }
}

async fn run(
    runtime: &Rt,
    action: &Digest,
) -> Result<kbf_proto::reapi::ActionResult, RuntimeError> {
    tokio::time::timeout(PROMPT * 2, runtime.run(work(action.clone())))
        .await
        .expect("the run ends")
}

fn phase(runtime: &Rt) -> NodePhase {
    runtime.phase().borrow().clone()
}

/// Waits until the fake has logged a line starting with `verb`.
async fn until_logged(fake: &FakeSession, verb: &str) {
    let deadline = tokio::time::Instant::now() + PROMPT;
    while !fake.state().log.iter().any(|l| l.starts_with(verb)) {
        assert!(tokio::time::Instant::now() < deadline, "no {verb} in time");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn the_action(cas: &MemoryCas) -> Digest {
    let mut spec = Spec::act("cat in ; env FROM ; append out/made.txt made")
        .env("FROM", "the Command")
        .outputs(&["out/made.txt"]);
    spec.inputs
        .push(("in".to_owned(), b"from the input\n".to_vec()));
    spec.store(cas)
}

/// Catches the session switch out of order around the action: an action run before the
/// helper says the user's GUI session is up (the defect "run before gui/<uid> exists"),
/// a cleanup that skips `session-idle` (the catches: the Mac would stay logged in as a
/// deleted user) or deletes the user before its processes are killed, a phase that does
/// not say `preparing` before the switch and `restoring` after the action, and a node
/// not back to `serving`, its record and lease directory gone, when the lease reports.
#[tokio::test]
async fn a_lease_runs_in_its_users_session_and_the_node_is_cleaned_after() {
    let dir = scratch("whole-order");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    fake.state().ready_after = 2;
    let config = config(&dir);
    let runtime = start(&config, &fake, &cas);
    assert_eq!(runtime.driver(), DRIVER);
    assert!(runtime.serves("whole_machine") && !runtime.serves("action"));
    assert_eq!(phase(&runtime), NodePhase::Serving);

    let result = run(&runtime, &the_action(&cas)).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    assert_eq!(stdout(&cas, &result), "from the input\nthe Command\n");
    assert_eq!(result.output_files.len(), 1);
    assert!(format!("{runtime:?}").contains("phase: Serving"));
    assert_eq!(
        fake.verbs(),
        [
            "boot-reset",
            "user-create 1.1 (preparing)",
            "session-login 1.1 (preparing)",
            "session-ready 1.1 (preparing)",
            "share 1.1 (preparing)",
            "run 1.1 (preparing)",
            "wait (preparing)",
            "kill-uid 1.1 (restoring)",
            "session-idle (restoring)",
            "user-delete 1.1 (restoring)",
        ]
    );
    // Two answers said not yet; the action ran only after the third said up.
    assert_eq!(fake.state().asked_at_run, Some(3));
    assert_eq!(phase(&runtime), NodePhase::Serving);
    assert!(!config.dir.join(LEASE_FILE).exists());
    assert!(!config.dir.join("lease-1-1").exists());
}

/// Catches a lease that does not survive the userspace restart `session-login` makes:
/// the restarted daemon's runtime must say it is still `preparing` that lease (so the
/// server keeps it and sends its `Start` again), must not make the user or switch the
/// session a second time (the switch would restart the Mac again, for ever), and must
/// run the action once the session is up.
#[tokio::test]
async fn a_lease_survives_the_restart_into_its_session() {
    let dir = scratch("whole-restart-in");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    fake.state().ready_after = usize::MAX;
    let config = config(&dir);
    let action = the_action(&cas);
    let before = start(&config, &fake, &cas);
    let running = {
        let before = Arc::clone(&before);
        let action = action.clone();
        tokio::spawn(async move { before.run(work(action)).await })
    };
    until_logged(&fake, "session-ready").await;
    // The restart ends the daemon: its run is gone mid-wait.
    running.abort();
    assert!(running.await.is_err());
    drop(before);
    fake.verbs();

    fake.state().ready_after = 0;
    let after = start(&config, &fake, &cas);
    assert_eq!(phase(&after), NodePhase::Preparing(lease()));
    let result = run(&after, &action).await.expect("ran after the restart");
    assert_eq!(stdout(&cas, &result), "from the input\nthe Command\n");
    assert_eq!(
        fake.verbs(),
        [
            "boot-reset 1.1",
            "session-ready 1.1 (preparing)",
            "share 1.1 (preparing)",
            "run 1.1 (preparing)",
            "wait (preparing)",
            "kill-uid 1.1 (restoring)",
            "session-idle (restoring)",
            "user-delete 1.1 (restoring)",
        ]
    );
    assert_eq!(phase(&after), NodePhase::Serving);
}

/// Catches an outcome lost in the restart `session-idle` makes (the lease would report
/// nothing, or run its action again), or a restore that repeats the steps done before
/// it: the restarted runtime says `restoring`, deletes the user, and reports the
/// result the action produced before the restart.
#[tokio::test]
async fn the_outcome_survives_the_restart_back() {
    let dir = scratch("whole-restart-back");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    fake.state().hold.insert("session-idle");
    let config = config(&dir);
    let action = the_action(&cas);
    let before = start(&config, &fake, &cas);
    let running = {
        let before = Arc::clone(&before);
        let action = action.clone();
        tokio::spawn(async move { before.run(work(action)).await })
    };
    until_logged(&fake, "session-idle").await;
    running.abort();
    assert!(running.await.is_err());
    fake.release();
    drop(before);
    fake.verbs();

    fake.state().hold.clear();
    let after = start(&config, &fake, &cas);
    assert_eq!(phase(&after), NodePhase::Restoring(lease()));
    let result = run(&after, &action).await.expect("the kept result");
    assert_eq!(stdout(&cas, &result), "from the input\nthe Command\n");
    assert_eq!(
        fake.verbs(),
        ["boot-reset 1.1", "user-delete 1.1 (restoring)"]
    );
    assert_eq!(phase(&after), NodePhase::Serving);
}

/// Catches a failed cleanup that releases the node (the defect "release the node after
/// a failed cleanup": the next lease would start beside a user that could not be
/// deleted), a quarantine that does not say why, does not refuse the next lease, or
/// does not outlive the daemon. The lease itself still reports its action's result. A
/// boot reset that fails again on a quarantined node keeps the first reason.
#[tokio::test]
async fn a_failed_cleanup_quarantines_the_node_until_an_operator_repairs_it() {
    let dir = scratch("whole-quarantine");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    fake.state().fail.insert("user-delete");
    let config = config(&dir);
    let action = the_action(&cas);
    let runtime = start(&config, &fake, &cas);
    let result = run(&runtime, &action).await.expect("the action's result");
    assert_eq!(result.exit_code, 0);
    let NodePhase::Quarantined(why) = phase(&runtime) else {
        panic!("not quarantined: {:?}", phase(&runtime));
    };
    assert!(why.contains("user-delete: user-delete failed"), "{why}");
    let file = std::fs::read_to_string(config.dir.join(QUARANTINE_FILE)).expect("the file");
    assert_eq!(file.trim(), why);
    fake.verbs();

    let refused = run(&runtime, &action)
        .await
        .expect_err("ran on a quarantined node");
    assert!(refused.to_string().contains("quarantined"), "{refused}");
    assert_eq!(fake.verbs(), Vec::<String>::new());
    runtime.kill(lease()).await;
    assert_eq!(
        fake.verbs(),
        Vec::<String>::new(),
        "a quarantined node is not touched"
    );

    drop(runtime);
    fake.state().fail.insert("boot-reset");
    let again = start(&config, &fake, &cas);
    assert_eq!(phase(&again), NodePhase::Quarantined(why));
}

/// Catches a failed boot reset that leaves the node serving: whatever the reset could
/// not delete is still there.
#[tokio::test]
async fn a_failed_boot_reset_quarantines_the_node() {
    let dir = scratch("whole-reset-failed");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    fake.state().fail.insert("boot-reset");
    let runtime = start(&config(&dir), &fake, &cas);
    let NodePhase::Quarantined(why) = phase(&runtime) else {
        panic!("not quarantined: {:?}", phase(&runtime));
    };
    assert!(
        why.contains("the boot reset failed: boot-reset failed"),
        "{why}"
    );
}

/// Catches a wait for the session with no bound (the lease would hold the Mac until the
/// server gave it up, and could start its action after that), and a lease given up
/// without the cleanup: the user and the session switch are undone.
#[tokio::test]
async fn a_session_that_never_comes_up_fails_the_lease_and_is_undone() {
    let dir = scratch("whole-never-up");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    fake.state().ready_after = usize::MAX;
    let mut config = config(&dir);
    config.session_wait = Duration::from_millis(150);
    let runtime = start(&config, &fake, &cas);
    let error = run(&runtime, &the_action(&cas)).await.expect_err("ran");
    assert!(error.to_string().contains("was not up within"), "{error}");
    let verbs = fake.verbs();
    assert!(!verbs.iter().any(|v| v.starts_with("run")), "{verbs:?}");
    assert_eq!(
        verbs[verbs.len() - 3..],
        [
            "kill-uid 1.1 (restoring)",
            "session-idle (restoring)",
            "user-delete 1.1 (restoring)"
        ]
    );
    assert_eq!(phase(&runtime), NodePhase::Serving);
}

/// Catches a kill that does not reach a lease waiting for its session, or one that
/// leaves the switch in place: the run ends `Killed` once the node is restored.
#[tokio::test]
async fn a_kill_while_waiting_for_the_session_restores_the_node() {
    let dir = scratch("whole-kill-waiting");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    fake.state().ready_after = usize::MAX;
    let runtime = start(&config(&dir), &fake, &cas);
    let running = {
        let runtime = Arc::clone(&runtime);
        let action = the_action(&cas);
        tokio::spawn(async move { runtime.run(work(action)).await })
    };
    until_logged(&fake, "session-ready").await;
    tokio::time::timeout(PROMPT, runtime.kill(lease()))
        .await
        .expect("the kill returns");
    let outcome = running.await.expect("the run ended");
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
    assert!(fake.verbs().ends_with(&[
        "kill-uid 1.1 (restoring)".to_owned(),
        "session-idle (restoring)".to_owned(),
        "user-delete 1.1 (restoring)".to_owned(),
    ]));
    assert_eq!(phase(&runtime), NodePhase::Serving);
}

/// Catches a kill or a timeout that leaves the action running: every process of the
/// user is killed (`kill-uid`) before the lease ends, and a timeout still reports what
/// the action wrote.
#[tokio::test]
async fn a_kill_or_a_timeout_ends_the_users_processes() {
    let dir = scratch("whole-kill-running");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    let runtime = start(&config(&dir), &fake, &cas);
    let slow = Spec::act("echo started ; sleep 60").store(&cas);
    let running = {
        let runtime = Arc::clone(&runtime);
        let slow = slow.clone();
        tokio::spawn(async move { runtime.run(work(slow)).await })
    };
    until_logged(&fake, "run").await;
    tokio::time::timeout(PROMPT, runtime.kill(lease()))
        .await
        .expect("the kill returns");
    let outcome = running.await.expect("the run ended");
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
    let verbs = fake.verbs();
    let at = |verb: &str| verbs.iter().position(|v| v.starts_with(verb)).expect(verb);
    assert!(at("kill-uid") < at("wait"), "{verbs:?}");

    let timed = Spec::act("echo partial ; sleep 60")
        .timeout(Duration::from_millis(300))
        .store(&cas);
    let error = run(&runtime, &timed)
        .await
        .expect_err("ran past its timeout");
    let RuntimeError::TimedOut {
        stdout: Some(digest),
        ..
    } = error
    else {
        panic!("{error:?}");
    };
    assert_eq!(cas.blob(&digest), b"partial\n");
}

/// Catches a whole-machine lease that reports nothing of what it used: the server writes
/// an `action usage:` line only for a result that carries a `ResourceUsage`, so a Mac
/// lease of this runtime had no line. A finished action's result carries one (its wall
/// time, nothing else measured: CPU, memory and both instrument blocks absent, the basis
/// unspecified so no reader takes it for a memory sample), and so does the lease that
/// ends without a result, a timeout or a kill, through `failed_usage`, once.
#[tokio::test]
async fn every_lease_reports_its_usage_even_when_it_has_no_result() {
    use kbf_proto::worker::{ResourceUsage, UsageBasis};

    let dir = scratch("whole-usage");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    let runtime = start(&config(&dir), &fake, &cas);

    let result = run(&runtime, &the_action(&cas)).await.expect("ran");
    let usage = ResourceUsage::in_result(&result).expect("the result carries its usage");
    assert_eq!(
        ResourceUsage {
            wall_micros: 0,
            ..usage.clone()
        },
        ResourceUsage::default(),
        "only the wall time is measured: {usage:?}"
    );
    assert_eq!(usage.basis(), UsageBasis::Unspecified);
    assert_eq!(runtime.failed_usage(lease()), None, "a result has its own");

    let timed = Spec::act("echo partial ; sleep 60")
        .timeout(Duration::from_millis(300))
        .store(&cas);
    run(&runtime, &timed)
        .await
        .expect_err("ran past its timeout");
    let told = runtime
        .failed_usage(lease())
        .expect("a timed out lease's usage");
    assert!(
        told.wall_micros >= 300_000,
        "ran until the timeout: {told:?}"
    );
    assert_eq!(runtime.failed_usage(lease()), None, "told once");

    let slow = Spec::act("sleep 60").store(&cas);
    fake.verbs();
    let running = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move { runtime.run(work(slow)).await })
    };
    until_logged(&fake, "run").await;
    runtime.kill(lease()).await;
    let outcome = running.await.expect("the run ended");
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
    assert_eq!(runtime.failed_usage(LeaseId::new(9, 9)), None, "another's");
    assert!(runtime.failed_usage(lease()).is_some(), "a killed lease's");

    fake.state().fail.insert("wait");
    run(&runtime, &the_action(&cas)).await.expect_err("no exit");
    assert_eq!(
        runtime.failed_usage(lease()),
        None,
        "a run whose end was not seen measured nothing"
    );

    // An action that ran, whose outputs could not be stored: no result, but it ran.
    let dir = scratch("whole-usage-down");
    let down = Arc::new(MemoryCas {
        refuse_puts: true,
        ..MemoryCas::default()
    });
    let runtime = start(&config(&dir), &FakeSession::default(), &down);
    run(&runtime, &the_action(&down)).await.expect_err("stored");
    assert!(runtime.failed_usage(lease()).is_some(), "it ran");
}

/// Catches a lease the server gave up while the daemon restarted that is never
/// cleaned (no `Start` comes for it again, so only the server's `Cancel`, a kill of a
/// lease not running, can end it), one cleaned for a kill naming another lease, and a
/// user the boot reset already deleted that is killed and deleted again (the helper
/// would refuse, and the node be quarantined for nothing).
#[tokio::test]
async fn a_recorded_lease_the_server_gave_up_is_restored_on_its_kill() {
    let dir = scratch("whole-orphan");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    fake.state().ready_after = usize::MAX;
    let config = config(&dir);
    let before = start(&config, &fake, &cas);
    let running = {
        let before = Arc::clone(&before);
        let action = the_action(&cas);
        tokio::spawn(async move { before.run(work(action)).await })
    };
    until_logged(&fake, "session-ready").await;
    running.abort();
    let _ = running.await;
    drop(before);

    fake.state().deleted_by_reset = vec![lease()];
    let after = start(&config, &fake, &cas);
    fake.verbs();
    after.kill(LeaseId::new(1, 9)).await;
    assert_eq!(fake.verbs(), Vec::<String>::new());
    assert_eq!(phase(&after), NodePhase::Preparing(lease()));
    after.kill(lease()).await;
    assert_eq!(fake.verbs(), ["session-idle (restoring)"]);
    assert_eq!(phase(&after), NodePhase::Serving);
    assert!(!config.dir.join(LEASE_FILE).exists());
    after.kill(lease()).await;
    assert_eq!(fake.verbs(), Vec::<String>::new(), "restored twice");
}

/// Catches a second lease started while another holds the node's record (two users,
/// one session), and a helper call that panics taking the daemon with it: the lease
/// fails instead, and nothing it never did is undone.
#[tokio::test]
async fn a_second_lease_is_refused_and_a_panicking_call_fails_the_lease() {
    let dir = scratch("whole-second");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    fake.state().ready_after = usize::MAX;
    let config = config(&dir);
    let before = start(&config, &fake, &cas);
    let running = {
        let before = Arc::clone(&before);
        let action = the_action(&cas);
        tokio::spawn(async move { before.run(work(action)).await })
    };
    until_logged(&fake, "session-ready").await;
    running.abort();
    let _ = running.await;
    drop(before);
    let after = start(&config, &fake, &cas);
    let other = Work {
        lease_id: LeaseId::new(1, 2),
        ..work(the_action(&cas))
    };
    let refused = after.run(other).await.expect_err("two leases at once");
    assert!(
        refused
            .to_string()
            .contains("lease 1.1 still holds this node"),
        "{refused}"
    );
    drop(after);
    std::fs::remove_file(config.dir.join(LEASE_FILE)).expect("the record");

    fake.state().panic.insert("user-create");
    let runtime = start(&config, &fake, &cas);
    fake.verbs();
    let error = run(&runtime, &the_action(&cas)).await.expect_err("ran");
    assert!(error.to_string().contains("did not finish"), "{error}");
    assert_eq!(fake.verbs(), ["user-create 1.1 (preparing)"]);
    assert_eq!(phase(&runtime), NodePhase::Serving);
}

/// Catches a failed preparation or run step reported as anything but a failed lease
/// that names the step, or one that quarantines the node (its cleanup succeeded).
#[tokio::test]
async fn each_failed_step_fails_the_lease_by_name() {
    for verb in [
        "user-create",
        "session-login",
        "session-ready",
        "share",
        "run",
        "wait",
    ] {
        let dir = scratch(&format!("whole-fail-{verb}"));
        let cas = Arc::new(MemoryCas::default());
        let fake = FakeSession::default();
        fake.state().fail.insert(verb);
        let runtime = start(&config(&dir), &fake, &cas);
        let error = run(&runtime, &the_action(&cas)).await.expect_err(verb);
        let RuntimeError::Failed(why) = &error else {
            panic!("{verb}: {error:?}");
        };
        assert!(why.contains(&format!("{verb} failed")), "{verb}: {why}");
        assert_eq!(phase(&runtime), NodePhase::Serving, "{verb}");
    }
}

/// Catches a failed `kill-uid` after a timeout taken as a clean end (the action's
/// processes may live on): the lease fails, and the cleanup's own `kill-uid`, failing
/// too, quarantines the node.
#[tokio::test]
async fn a_kill_uid_that_fails_quarantines_the_node() {
    let dir = scratch("whole-kill-fails");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    fake.state().fail.insert("kill-uid");
    let runtime = start(&config(&dir), &fake, &cas);
    // The fake leaves the action alive when kill-uid fails; it ends by itself.
    let timed = Spec::act("sleep 2")
        .timeout(Duration::from_millis(100))
        .store(&cas);
    let error = run(&runtime, &timed).await.expect_err("ran");
    assert!(
        error.to_string().contains("kill-uid: kill-uid failed"),
        "{error}"
    );
    assert!(matches!(phase(&runtime), NodePhase::Quarantined(_)));
}

/// Catches an action the client got wrong reported as the farm's failure, and a record
/// that cannot be read or written taken as no record (the runtime would lose track of
/// a lease user it made).
#[tokio::test]
async fn bad_actions_and_bad_records_fail_the_lease() {
    let dir = scratch("whole-bad");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    let config = config(&dir);
    let runtime = start(&config, &fake, &cas);
    let nothing = Spec::argv(&[]).store(&cas);
    let error = run(&runtime, &nothing).await.expect_err("ran nothing");
    assert!(matches!(error, RuntimeError::Invalid(_)), "{error:?}");
    let action = kbf_proto::reapi::Action::default();
    let empty = cas.insert(prost::Message::encode_to_vec(&action));
    let error = run(&runtime, &empty).await.expect_err("ran no command");
    assert!(matches!(error, RuntimeError::Invalid(_)), "{error:?}");

    std::fs::write(config.dir.join(LEASE_FILE), "garbage\n").expect("write");
    let error = run(&runtime, &the_action(&cas)).await.expect_err("ran");
    assert!(
        error.to_string().contains("reading the lease record"),
        "{error}"
    );
    std::fs::remove_file(config.dir.join(LEASE_FILE)).expect("remove");

    if rustix::process::geteuid().is_root() {
        return; // root writes through a read-only directory
    }
    let mode = |m| std::os::unix::fs::PermissionsExt::from_mode(m);
    std::fs::set_permissions(&config.dir, mode(0o500)).expect("chmod");
    let error = run(&runtime, &the_action(&cas)).await.expect_err("ran");
    std::fs::set_permissions(&config.dir, mode(0o700)).expect("chmod back");
    assert!(
        error.to_string().contains("writing the lease record"),
        "{error}"
    );
}

/// Catches a lease record or quarantine that cannot be read taken as none (the daemon
/// would start serving over a lease user it made, or a node an operator has not
/// repaired): the runtime does not start.
#[tokio::test]
async fn unreadable_files_stop_the_start() {
    for file in [LEASE_FILE, QUARANTINE_FILE] {
        let dir = scratch(&format!("whole-unreadable-{file}"));
        let config = config(&dir);
        std::fs::create_dir_all(config.dir.join(file)).expect("a directory in its place");
        let cas = Arc::new(MemoryCas::default());
        let started = WholeMachineRuntime::new(config, FakeSession::default(), cas);
        assert!(started.is_err(), "{file}");
    }
}

/// Catches a cleanup that cannot record its steps carrying on as if it could, and a
/// quarantine that cannot be written leaving the node serving: the node is
/// quarantined for this daemon's life at least.
#[tokio::test]
async fn a_cleanup_that_cannot_record_quarantines_the_node() {
    if rustix::process::geteuid().is_root() {
        return; // root writes through a read-only directory
    }
    let dir = scratch("whole-cannot-record");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    fake.state().hold.insert("wait");
    let config = config(&dir);
    let runtime = start(&config, &fake, &cas);
    let running = {
        let runtime = Arc::clone(&runtime);
        let action = the_action(&cas);
        tokio::spawn(async move { runtime.run(work(action)).await })
    };
    until_logged(&fake, "wait").await;
    let mode = |m| std::os::unix::fs::PermissionsExt::from_mode(m);
    std::fs::set_permissions(&config.dir, mode(0o500)).expect("chmod");
    fake.release();
    let outcome = tokio::time::timeout(PROMPT, running)
        .await
        .expect("ends")
        .expect("ran");
    std::fs::set_permissions(&config.dir, mode(0o700)).expect("chmod back");
    assert!(outcome.is_ok(), "{outcome:?}");
    let NodePhase::Quarantined(why) = phase(&runtime) else {
        panic!("not quarantined: {:?}", phase(&runtime));
    };
    assert!(why.contains("writing the lease record"), "{why}");
    assert!(!config.dir.join(QUARANTINE_FILE).exists());
}

/// Catches a daemon that dies while the action runs (not a session switch: the helper
/// and the action's processes survive it) and whose restarted runtime starts the action
/// a second time beside the first, still running: no second `run` may come before a
/// `kill-uid` of the lease user.
#[tokio::test]
async fn a_restart_mid_action_does_not_run_it_beside_the_first() {
    let dir = scratch("whole-probe-mid-action");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    let config = config(&dir);
    let slow = Spec::act("sleep 3 ; append out/made.txt made")
        .outputs(&["out/made.txt"])
        .store(&cas);
    let before = start(&config, &fake, &cas);
    let running = {
        let before = Arc::clone(&before);
        let slow = slow.clone();
        tokio::spawn(async move { before.run(work(slow)).await })
    };
    until_logged(&fake, "run").await;
    running.abort();
    assert!(running.await.is_err());
    drop(before);
    fake.verbs();
    let after = start(&config, &fake, &cas);
    let _ = run(&after, &slow).await;
    let verbs = fake.verbs();
    let second_run = verbs.iter().position(|v| v.starts_with("run "));
    let kill = verbs.iter().position(|v| v.starts_with("kill-uid"));
    assert!(
        second_run.is_none() || kill.is_some_and(|k| k < second_run.unwrap()),
        "the action ran again beside the first with no kill-uid before: {verbs:?}"
    );
}

/// The verbs so far without the `wait` of a run left behind by a dropped runtime (its
/// blocking wait returns whenever that run's processes end, so it logs `wait` before the
/// restarted runtime's phase is set and `wait (restoring)` after).
fn verbs_but_wait(fake: &FakeSession) -> Vec<String> {
    fake.verbs()
        .into_iter()
        .filter(|v| v != "wait" && !v.starts_with("wait ("))
        .collect()
}

/// A runtime dropped while `action` runs (the daemon died mid-action), and a new one
/// started over the same directory, as the restarted daemon starts it.
async fn restart_mid_action(
    config: &WholeMachineConfig,
    fake: &FakeSession,
    cas: &Arc<MemoryCas>,
    action: &Digest,
) -> Arc<Rt> {
    let before = start(config, fake, cas);
    let running = {
        let before = Arc::clone(&before);
        let action = action.clone();
        tokio::spawn(async move { before.run(work(action)).await })
    };
    until_logged(fake, "run").await;
    if fake.state().hold_waits {
        // The run's `wait` is called only if the abort comes after it was spawned.
        let deadline = tokio::time::Instant::now() + PROMPT;
        while fake.state().waits_entered == 0 {
            assert!(tokio::time::Instant::now() < deadline, "no wait in time");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    running.abort();
    assert!(running.await.is_err());
    drop(before);
    fake.verbs();
    start(config, fake, cas)
}

/// Catches a restart mid-action that is taken up instead of ended: the defect that drops
/// the `started` record (the restarted runtime waits for the session and runs the action
/// again), and the one that skips the `kill-uid` at start (the first copy runs on until
/// the server's `Start` or `Cancel` comes, if it ever does). The restarted runtime kills
/// the lease user's processes before it says anything to the server, reports
/// `restoring`, and the lease fails as the farm's (its result is lost) once the node is
/// clean: `Lost` (issue #22), which the server runs again, not a plain failure, which
/// it would answer INTERNAL.
#[tokio::test]
async fn a_restart_mid_action_kills_the_action_at_start_and_fails_the_lease() {
    kills_the_action_at_start("whole-mid-action", false).await;
}

/// The same, with the dropped runtime's `wait` ending only after the restarted runtime
/// is up and its start-up verbs are read: the late `wait` is logged under the restarted
/// runtime's phase (`wait (restoring)`) among the restore's verbs, which are no business
/// of that wait (a held wait, not a sleep, orders it every time).
#[tokio::test]
async fn a_dropped_runtimes_wait_ending_after_the_restart_is_not_the_restores() {
    kills_the_action_at_start("whole-mid-action-late-wait", true).await;
}

async fn kills_the_action_at_start(name: &str, late_wait: bool) {
    let dir = scratch(name);
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    let config = config(&dir);
    let slow = Spec::act("sleep 60").store(&cas);
    fake.state().hold_waits = late_wait;
    let after = restart_mid_action(&config, &fake, &cas, &slow).await;
    assert_eq!(verbs_but_wait(&fake), ["boot-reset 1.1", "kill-uid 1.1"]);
    assert_eq!(phase(&after), NodePhase::Restoring(lease()));
    if late_wait {
        fake.release_waits();
        until_logged(&fake, "wait").await;
    }

    let error = run(&after, &slow)
        .await
        .expect_err("a result after the restart");
    let RuntimeError::Lost(LostWhy::DaemonRestarted, why) = &error else {
        panic!("{error:?}");
    };
    assert!(why.contains("its result is lost"), "{why}");
    assert_eq!(
        verbs_but_wait(&fake),
        ["session-idle (restoring)", "user-delete 1.1 (restoring)"]
    );
    assert_eq!(phase(&after), NodePhase::Serving);
    assert!(!config.dir.join(LEASE_FILE).exists());
    assert!(!config.dir.join("lease-1-1").exists());
}

/// Catches a `kill-uid` that fails at start taken as done (the restore would skip it,
/// and release a node where the first copy may still run): the restore asks again
/// first, and its failure quarantines the node. The lease still fails as lost.
#[tokio::test]
async fn a_restart_mid_action_whose_kill_fails_quarantines_the_node() {
    let dir = scratch("whole-mid-action-kill-fails");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    let config = config(&dir);
    // The fake leaves the action alive when kill-uid fails; it ends by itself.
    let slow = Spec::act("sleep 2").store(&cas);
    fake.state().fail.insert("kill-uid");
    let after = restart_mid_action(&config, &fake, &cas, &slow).await;
    assert_eq!(verbs_but_wait(&fake), ["boot-reset 1.1", "kill-uid 1.1"]);
    assert_eq!(phase(&after), NodePhase::Restoring(lease()));

    let error = run(&after, &slow)
        .await
        .expect_err("a result after the restart");
    assert!(error.to_string().contains("its result is lost"), "{error}");
    assert_eq!(verbs_but_wait(&fake), ["kill-uid 1.1 (restoring)"]);
    let NodePhase::Quarantined(why) = phase(&after) else {
        panic!("not quarantined: {:?}", phase(&after));
    };
    assert!(why.contains("kill-uid: kill-uid failed"), "{why}");
}

/// Catches a restart mid-action that kills (and then deletes) a lease user the boot
/// reset already deleted: the helper would refuse the unknown user, and the node be
/// quarantined for nothing. The lease still fails as lost, and the session is idled.
#[tokio::test]
async fn a_restart_mid_action_after_the_reset_deleted_the_user_kills_nothing() {
    let dir = scratch("whole-mid-action-reset");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    let config = config(&dir);
    let slow = Spec::act("sleep 2").store(&cas);
    fake.state().deleted_by_reset = vec![lease()];
    let after = restart_mid_action(&config, &fake, &cas, &slow).await;
    assert_eq!(verbs_but_wait(&fake), ["boot-reset 1.1"]);
    assert_eq!(phase(&after), NodePhase::Restoring(lease()));

    let error = run(&after, &slow)
        .await
        .expect_err("a result after the restart");
    assert!(error.to_string().contains("its result is lost"), "{error}");
    assert_eq!(verbs_but_wait(&fake), ["session-idle (restoring)"]);
    assert_eq!(phase(&after), NodePhase::Serving);
}

/// Catches a session wait counted from when this daemon took the lease up rather than
/// from when the lease was first taken (the defect "deadline from now": each restart
/// would start the wait again, and the action could start after the server's switch
/// grace has given the lease up). A record taken longer ago than the wait fails the
/// lease at the first `session-ready` that says not yet.
#[tokio::test]
async fn the_session_wait_counts_from_when_the_lease_was_first_taken() {
    let dir = scratch("whole-wait-since");
    let cas = Arc::new(MemoryCas::default());
    let fake = FakeSession::default();
    fake.state().ready_after = usize::MAX;
    let mut config = config(&dir);
    // Longer than `run` waits for the lease to end.
    config.session_wait = PROMPT * 4;
    std::fs::create_dir_all(&config.dir).expect("the directory");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis();
    let wait_ms = config.session_wait.as_millis() + 1000;
    let taken = u64::try_from(now_ms - wait_ms).expect("fits");
    let mut record = Record::new(lease(), taken);
    record.user = true;
    record.login = true;
    Files::new(config.dir.clone())
        .keep(&record)
        .expect("the record");

    let runtime = start(&config, &fake, &cas);
    assert_eq!(phase(&runtime), NodePhase::Preparing(lease()));
    let error = run(&runtime, &the_action(&cas)).await.expect_err("ran");
    assert!(error.to_string().contains("was not up within"), "{error}");
    assert_eq!(
        fake.verbs(),
        [
            "boot-reset 1.1",
            "session-ready 1.1 (preparing)",
            "kill-uid 1.1 (restoring)",
            "session-idle (restoring)",
            "user-delete 1.1 (restoring)",
        ]
    );
}
