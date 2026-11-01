//! A drain past its configured `--drain-deadline`, and with none: the deadline only reports
//! (a WARN and `deadline_passed` in the status), nothing is ended, and the operator's hard
//! stops (a second signal, `--shutdown-timeout-secs`) still end the drain.

use super::*;

/// Asserts that with an execution that stays open past a configured deadline, the drain
/// reports and goes on waiting: a WARN names the open count, the status says the deadline
/// passed, and the worker's stream is not ended, the farm is not fenced and the phase stays
/// `draining`; and that once the execution is gone the drain ends.
#[tokio::test(start_paused = true)]
async fn at_the_deadline_the_drain_reports_and_goes_on_waiting() {
    let log = crate::test_log::capture();
    let rig = Rig::new(timing(0, 30));
    let mut inbox = register(&rig);
    let open = rig.backend.metrics().execute.open_executions.raise();

    assert!(rig.sequence.begin());
    pass_feeding(&rig, 29).await;
    assert!(!rig.drain.deadline_passed());
    pass_feeding(&rig, 3).await;
    assert!(rig.drain.deadline_passed());
    let text = log.text();
    assert!(
        text.contains("deadline passed with executions open"),
        "{text}"
    );
    assert!(text.contains("open=1"), "{text}");
    assert!(text.contains("WARN"), "{text}");

    pass_feeding(&rig, 600).await;
    assert!(rig.drain.sequence_finished().now_or_never().is_none());
    assert_eq!(rig.drain.phase(), Phase::Draining);
    assert_eq!(errors(&mut inbox), [], "a stream was ended");
    let later = log.text();
    assert_eq!(later.matches("deadline passed").count(), 1, "{later}");

    drop(open);
    rig.drain.sequence_finished().await;
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

/// Asserts that with no deadline an execution that runs for hours keeps the drain going
/// and its stream open, nothing is reported, and the drain ends when the execution does.
#[tokio::test(start_paused = true)]
async fn with_no_deadline_the_drain_waits_for_an_execution_however_long() {
    let log = crate::test_log::capture();
    let rig = Rig::new(no_deadline(0));
    let open = rig.backend.metrics().execute.open_executions.raise();

    assert!(rig.sequence.begin());
    pass(Duration::from_secs(10 * 3600)).await;
    assert!(rig.drain.sequence_finished().now_or_never().is_none());
    assert_eq!(rig.drain.phase(), Phase::Draining);
    assert!(!rig.drain.deadline_passed());
    assert!(!log.text().contains("deadline"), "{}", log.text());

    drop(open);
    rig.drain.sequence_finished().await;
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

/// Asserts that a drain whose running action outlasts `--drain-deadline` reports the
/// deadline (the WARN, and `deadline_passed` in the status) and does not cut the action: its
/// stream is open long after the deadline, the worker's stream is not ended, the drain does
/// not end, and when the action is done the caller gets its result and the drain ends.
async fn a_running_action_is_not_cut_at_the_drain_deadline(how: Begin) {
    let log = crate::test_log::capture();
    let rig = Rig::new(timing(0, 30));
    let (node, mut inbox) = register_stream(&rig);
    let running = submit(&rig.farm, request(1)).expect("queued");
    rig.farm.tick().await;
    assert_eq!(starts(&mut inbox), 1, "the operation did not start");
    let views = rig.farm.lease_views(None);
    let [view] = &views.leases[..] else {
        panic!("one lease expected");
    };
    let lease = view.lease;
    let stream = follow(&rig, &running).await;
    let stop = begin(&rig, how, Duration::MAX);

    pass_beating(&rig, node, lease, 29).await;
    assert!(!rig.drain.deadline_passed(), "{how:?}");
    pass_beating(&rig, node, lease, 3).await;
    assert!(rig.drain.deadline_passed(), "{how:?}");
    assert!(log.text().contains("open=1"), "{how:?}: {}", log.text());

    pass_beating(&rig, node, lease, 1_000).await;
    assert!(!stream.is_finished(), "{how:?}: the stream was ended");
    assert!(
        !stop.is_finished(),
        "{how:?}: the stop ended with a lease running"
    );
    assert_eq!(rig.drain.phase(), Phase::Draining, "{how:?}");
    assert_eq!(
        errors(&mut inbox),
        [],
        "{how:?}: the worker stream was ended"
    );

    rig.farm
        .report(&WorkerId::new("node-a"), finished(lease))
        .await;
    tokio::time::timeout(Duration::from_secs(60), stop)
        .await
        .expect("the stop ends once the action is done")
        .expect("task");
    stream
        .await
        .expect("task")
        .expect("the done operation, then a clean end");
}

#[tokio::test(start_paused = true)]
async fn a_stop_does_not_cut_a_running_action_at_the_drain_deadline() {
    a_running_action_is_not_cut_at_the_drain_deadline(Begin::Signal).await;
}

#[tokio::test(start_paused = true)]
async fn the_endpoints_drain_does_not_cut_a_running_action_at_the_drain_deadline() {
    a_running_action_is_not_cut_at_the_drain_deadline(Begin::Endpoint).await;
}

/// Asserts that with no deadline a running action far past the old 900 seconds keeps its
/// stream, and that the drain ends when it is done.
async fn a_running_action_is_waited_for_with_no_deadline(how: Begin) {
    let rig = Rig::new(no_deadline(0));
    let (node, mut inbox) = register_stream(&rig);
    let running = submit(&rig.farm, request(1)).expect("queued");
    rig.farm.tick().await;
    assert_eq!(starts(&mut inbox), 1, "the operation did not start");
    let views = rig.farm.lease_views(None);
    let [view] = &views.leases[..] else {
        panic!("one lease expected");
    };
    let lease = view.lease;
    let stream = follow(&rig, &running).await;
    let stop = begin(&rig, how, Duration::MAX);

    pass_beating(&rig, node, lease, 3 * 900).await;
    assert!(!stream.is_finished(), "{how:?}: the stream was ended");
    assert!(!stop.is_finished(), "{how:?}: the stop ended");
    assert!(!rig.drain.deadline_passed(), "{how:?}");

    rig.farm
        .report(&WorkerId::new("node-a"), finished(lease))
        .await;
    tokio::time::timeout(Duration::from_secs(60), stop)
        .await
        .expect("the stop ends once the action is done")
        .expect("task");
    stream.await.expect("task").expect("a clean end");
}

#[tokio::test(start_paused = true)]
async fn a_stop_with_no_deadline_waits_for_a_running_action_however_long() {
    a_running_action_is_waited_for_with_no_deadline(Begin::Signal).await;
}

#[tokio::test(start_paused = true)]
async fn the_endpoints_drain_with_no_deadline_waits_for_a_running_action_however_long() {
    a_running_action_is_waited_for_with_no_deadline(Begin::Endpoint).await;
}

/// Asserts that the operator's hard stops still end a drain that is past its deadline with
/// an action running: a second signal ends it at once, the open stream ending UNAVAILABLE.
#[tokio::test(start_paused = true)]
async fn a_second_signal_ends_a_drain_that_is_past_its_deadline() {
    let rig = Rig::new(timing(0, 30));
    let mut inbox = register(&rig);
    let running = submit(&rig.farm, request(1)).expect("queued");
    rig.farm.tick().await;
    assert_eq!(starts(&mut inbox), 1, "the operation did not start");
    let stream = follow(&rig, &running).await;
    let stop = begin(&rig, Begin::Signal, Duration::MAX);

    pass_feeding(&rig, 40).await;
    assert!(rig.drain.deadline_passed());
    assert!(!stop.is_finished());
    let second = Instant::now();
    rig.drain.close();
    stop.await.expect("task");
    assert!(second.elapsed() < CLOSE_GRACE + Duration::from_secs(2));
    let ended = stream.await.expect("task").expect_err("ended");
    assert_eq!(ended.code(), Code::Unavailable, "{ended:?}");
}

/// Asserts that an explicit stop timeout still bounds a signal-begun stop whose action
/// outlasts the deadline: it ends at the timeout, the open stream ending UNAVAILABLE.
#[tokio::test(start_paused = true)]
async fn the_stop_timeout_ends_a_drain_that_is_past_its_deadline() {
    let rig = Rig::new(timing(0, 30));
    let mut inbox = register(&rig);
    let running = submit(&rig.farm, request(1)).expect("queued");
    rig.farm.tick().await;
    assert_eq!(starts(&mut inbox), 1, "the operation did not start");
    let stream = follow(&rig, &running).await;
    let began = Instant::now();
    let stop = begin(&rig, Begin::Signal, Duration::from_secs(120));

    pass_feeding(&rig, 100).await;
    assert!(rig.drain.deadline_passed());
    assert!(!stop.is_finished(), "ended before the stop timeout");
    stop.await.expect("task");
    assert!(began.elapsed() >= Duration::from_secs(120));
    let ended = stream.await.expect("task").expect_err("ended");
    assert_eq!(ended.code(), Code::Unavailable, "{ended:?}");
}

/// Asserts that a deadline that passes in the moment between the last execution ending and
/// the sequence's next look at the open count reports nothing: no WARN, no
/// `deadline_passed`, and the drain ends as it would have.
#[tokio::test(start_paused = true)]
async fn a_deadline_passing_as_the_last_execution_ends_reports_nothing() {
    let log = crate::test_log::capture();
    let rig = Rig::new(timing(0, 30));
    let open = rig.backend.metrics().execute.open_executions.raise();

    assert!(rig.sequence.begin());
    let mut phases = rig.drain.phases();
    phases
        .wait_for(|phase| *phase == Phase::Draining)
        .await
        .expect("the drain is alive");
    // Both happen before the sequence looks at the count again.
    drop(open);
    rig.drain.pass_deadline();
    rig.drain.sequence_finished().await;

    assert!(!rig.drain.deadline_passed());
    assert!(!log.text().contains("deadline passed"), "{}", log.text());
    assert_eq!(rig.drain.phase(), Phase::Closing);
}
