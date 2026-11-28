//! The drain's wait for the REAPI listener's connections ([`Drain::reapi_closed`]): a call
//! leaves the count once the last frame of its answer is handed to its connection, which may
//! still hold bytes of it for a slow client, so the drain stands down only once every
//! connection has closed; and a forced end waits for them at most the clients' grace.

use futures::FutureExt as _;

use super::*;

/// Asserts that with nothing counted open but a REAPI connection still open (an answer still
/// being written), the drain does not stand down: an hour on it is still `draining`, not
/// handed over and not over; once the listener says every connection has closed it ends,
/// closing.
#[tokio::test(start_paused = true)]
async fn the_drain_stands_down_only_once_the_reapi_connections_have_closed() {
    let rig = Rig::new(no_deadline(0));
    let closed = rig.drain.serves_reapi();

    assert!(rig.sequence.begin());
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(
        rig.drain.phase(),
        Phase::Draining,
        "stood down over an open connection"
    );
    assert!(rig.drain.sequence_finished().now_or_never().is_none());
    assert!(
        !rig.readiness.is_handed_over(),
        "handed over over an open connection"
    );

    closed.fire();
    let ended = tokio::time::timeout(Duration::from_secs(10), rig.drain.sequence_finished());
    ended
        .await
        .expect("the drain ends once the connections have closed");
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

/// Asserts that a forced end (a second signal) with no stream open gives the REAPI connections
/// the clients' grace to write the UNAVAILABLE answers it gave, and no more: with a connection
/// that never closes the drain is not over a millisecond before `CLOSE_GRACE` and is over a
/// millisecond after it.
#[tokio::test(start_paused = true)]
async fn a_forced_end_waits_for_the_connections_at_most_the_grace() {
    let rig = Rig::new(timing(600, 900));
    let _never_closed = rig.drain.serves_reapi();

    assert!(rig.sequence.begin());
    rig.drain.close();
    tokio::time::sleep(CLOSE_GRACE - Duration::from_millis(1)).await;
    assert!(
        rig.drain.sequence_finished().now_or_never().is_none(),
        "the drain ended before the grace was over"
    );
    tokio::time::sleep(Duration::from_millis(2)).await;
    assert!(
        rig.drain.sequence_finished().now_or_never().is_some(),
        "the drain outlasted the grace"
    );
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

/// Asserts that a forced end whose REAPI connections have all closed does not wait out the
/// grace: with nothing open and every connection closed, the drain is over within a
/// millisecond of the close.
#[tokio::test(start_paused = true)]
async fn a_forced_end_with_the_connections_closed_goes_at_once() {
    let rig = Rig::new(timing(600, 900));
    rig.drain.serves_reapi().fire();

    assert!(rig.sequence.begin());
    rig.drain.close();
    let ended = tokio::time::timeout(Duration::from_millis(1), rig.drain.sequence_finished());
    ended
        .await
        .expect("the drain waited with nothing to wait for");
    assert_eq!(rig.drain.phase(), Phase::Closing);
}
