//! Ending the two waits of the sequence by hand ([`Drain::end_lb_wait`] and
//! [`Drain::end_deadline`]). The waits given here are too long for any test to see run out,
//! so only the calls end them, and each step is waited for as an event of the drain.

use super::*;

/// The longest a test waits for an event: a bound that only a hang reaches.
const SAFETY: Duration = Duration::from_secs(30);

/// Waits until the phase of `drain` is `phase`, by the events of [`Drain::phases`].
async fn reaches(drain: &Drain, phase: Phase) {
    let mut phases = drain.phases();
    let reached = tokio::time::timeout(SAFETY, phases.wait_for(|now| *now == phase));
    let reached = reached.await.expect("the phase is reached in time");
    reached.expect("the drain is alive");
}

/// Waits until `stop` is given.
async fn fires(stop: &Stop) {
    let fired = tokio::time::timeout(SAFETY, stop.fired());
    fired.await.expect("the stop is given in time");
}

/// Asserts that `Drain::end_lb_wait` ends the load balancer wait whatever its length: with
/// waits no test lives to see run out, the sequence stays in `lb_wait`, serving everything
/// (no call is refused and no listener is sent GOAWAY), until the call; after it the phase is
/// `draining`, new Execute calls are refused, the REAPI listener, the worker listener and the
/// operator API are sent GOAWAY, and the sequence goes on waiting for the open execution
/// instead of ending.
#[tokio::test]
async fn ending_the_wait_by_hand_refuses_new_calls_and_sends_goaway() {
    let timing = DrainTiming {
        lb_wait: Duration::MAX,
        deadline: Duration::MAX,
    };
    let mut rig = Rig::new(timing);
    let _open = rig.backend.metrics().execute.open_executions.raise();

    assert!(rig.sequence.begin());
    assert_eq!(rig.drain.phase(), Phase::LbWait);
    assert!(!rig.closing.draining(), "refused calls in the wait");
    assert!(rig.goaway.try_recv().is_err(), "GOAWAY in the wait");

    rig.drain.end_lb_wait();
    reaches(&rig.drain, Phase::Draining).await;
    assert!(rig.closing.draining(), "new calls taken after the wait");
    let goaway = tokio::time::timeout(SAFETY, &mut rig.goaway);
    goaway
        .await
        .expect("GOAWAY is sent in time")
        .expect("the REAPI listener is told");
    fires(&rig.worker).await;
    fires(&rig.api).await;
    assert_eq!(rig.drain.phase(), Phase::Draining, "ended while open");
}

/// Asserts that `Drain::end_deadline` ends the wait for the open executions as the deadline
/// running out does: with an execution that never ends and a deadline no test lives to see
/// run out, the sequence stays in `draining`; after the call the phase is `closing`, the
/// worker's stream is ended UNAVAILABLE, and the farm takes no more work.
#[tokio::test]
async fn ending_the_deadline_by_hand_closes_the_streams_that_are_left() {
    let timing = DrainTiming {
        lb_wait: Duration::ZERO,
        deadline: Duration::MAX,
    };
    let rig = Rig::new(timing);
    let mut inbox = register(&rig);
    let _open = rig.backend.metrics().execute.open_executions.raise();

    assert!(rig.sequence.begin());
    reaches(&rig.drain, Phase::Draining).await;
    assert_eq!(rig.drain.phase(), Phase::Draining, "ended while open");

    rig.drain.end_deadline();
    reaches(&rig.drain, Phase::Closing).await;
    let ended: Vec<Code> = std::iter::from_fn(|| inbox.try_recv().ok())
        .filter_map(|message| message.err())
        .map(|status| status.code())
        .collect();
    assert_eq!(ended, [Code::Unavailable]);
    assert!(submit(&rig.farm, request(1)).is_err(), "still takes work");
}
