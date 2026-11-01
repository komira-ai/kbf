//! The drain's wait on a paused clock: no real time passes, so hours cost nothing and
//! nothing here depends on how fast the machine is.

use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use tokio::sync::{Notify, mpsc};

use super::wait_for_drain;

const HOUR: Duration = Duration::from_secs(3600);

/// Whether `wait` has finished after the paused clock moved on by `by`.
async fn finished_after(wait: &mut tokio::task::JoinHandle<()>, by: Duration) -> bool {
    tokio::time::advance(by).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    wait.is_finished()
}

fn spawn(
    deadline: Option<Duration>,
) -> (
    tokio::task::JoinHandle<()>,
    mpsc::UnboundedSender<()>,
    std::sync::Arc<Notify>,
) {
    let (signals_tx, mut signals) = mpsc::unbounded_channel();
    let drained = std::sync::Arc::new(Notify::new());
    let notify = std::sync::Arc::clone(&drained);
    let wait = tokio::spawn(async move {
        let running = AtomicUsize::new(2);
        wait_for_drain(&mut signals, &drained, deadline, &running).await;
    });
    (wait, signals_tx, notify)
}

/// Catches a default deadline: with none set, a lease that runs for a day keeps the
/// drain waiting (the old default stopped it after 600 s), and it ends when the daemon
/// has nothing left to wait for.
#[tokio::test(start_paused = true)]
async fn with_no_deadline_the_drain_waits_however_long_and_ends_when_drained() {
    let (mut wait, _signals, drained) = spawn(None);
    for _ in 0..24 {
        assert!(!finished_after(&mut wait, HOUR).await, "it gave up waiting");
    }
    drained.notify_one();
    wait.await.expect("ends once drained");
}

/// Catches a deadline that ends the drain (and so abandons the leases): past it the
/// wait is still going, a day later too, and it ends only when drained.
#[tokio::test(start_paused = true)]
async fn a_deadline_that_passes_does_not_end_the_drain() {
    let (mut wait, _signals, drained) = spawn(Some(Duration::from_secs(600)));
    assert!(!finished_after(&mut wait, Duration::from_secs(599)).await);
    assert!(!finished_after(&mut wait, Duration::from_secs(2)).await);
    assert!(!finished_after(&mut wait, 24 * HOUR).await);
    drained.notify_one();
    wait.await.expect("ends once drained");
}

/// Catches the two ways 0 could go wrong: it ends the drain at once (the old meaning),
/// or it becomes "no deadline" and the report never comes. 0 reports at once and the
/// wait goes on.
#[tokio::test(start_paused = true)]
async fn a_zero_deadline_reports_at_once_and_keeps_waiting() {
    let (mut wait, _signals, drained) = spawn(Some(Duration::ZERO));
    assert!(!finished_after(&mut wait, HOUR).await);
    drained.notify_one();
    wait.await.expect("ends once drained");
}

/// Catches a second signal that no longer stops the drain at once, with or without a
/// deadline, before or after it has passed.
#[tokio::test(start_paused = true)]
async fn a_second_signal_ends_the_wait_at_once() {
    for deadline in [None, Some(Duration::from_secs(600))] {
        for before in [Duration::ZERO, 2 * HOUR] {
            let (mut wait, signals, _drained) = spawn(deadline);
            assert!(!finished_after(&mut wait, before).await);
            signals.send(()).expect("waiting");
            wait.await.expect("the hard stop ends it");
        }
    }
}

/// Catches a closed signal channel taken for a second signal (the operator's hard
/// stop): with every sender gone nothing asks for it, so the drain goes on.
#[tokio::test(start_paused = true)]
async fn a_closed_signal_channel_is_not_a_second_signal() {
    let (mut wait, signals, drained) = spawn(None);
    drop(signals);
    assert!(!finished_after(&mut wait, HOUR).await);
    drained.notify_one();
    wait.await.expect("ends once drained");
}
