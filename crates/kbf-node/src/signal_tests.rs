use std::time::Duration;

use crate::handle::{Signals, forward};

fn raise(signal: libc::c_int) {
    // SAFETY: raise(3) on this process, whose handler for `signal` the test installed.
    assert_eq!(unsafe { libc::raise(signal) }, 0);
}

/// Catches handlers that do not keep a signal sent before they are read (the start of
/// the other half of `kbf storage-and-daemon` takes long, and a SIGTERM in it must
/// still be the first signal): one sent after `install` and before `recv` is
/// received, by its name, and is not the default action that ends this test.
#[tokio::test]
async fn a_signal_sent_before_it_is_read_is_kept() {
    let mut signals = Signals::install().expect("handlers");
    raise(libc::SIGTERM);
    let within = Duration::from_secs(10);
    let name = tokio::time::timeout(within, signals.recv()).await;
    assert_eq!(name.expect("kept"), "SIGTERM");
    raise(libc::SIGINT);
    let name = tokio::time::timeout(within, signals.recv()).await;
    assert_eq!(name.expect("kept"), "SIGINT");
}

/// Catches a forwarder that drops a signal (the daemon would never drain), one that
/// reads SIGINT differently from SIGTERM, and one that outlives its receiver (a task
/// per stopped daemon): each SIGTERM and SIGINT is one `()`, and the task ends once
/// nobody listens.
#[tokio::test]
async fn each_sigterm_and_sigint_is_one_signal_and_the_task_ends_with_its_receiver() {
    let signals = Signals::install().expect("handlers");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(forward(signals, tx));
    let within = Duration::from_secs(10);
    raise(libc::SIGTERM);
    tokio::time::timeout(within, rx.recv())
        .await
        .expect("SIGTERM forwarded")
        .expect("open");
    raise(libc::SIGINT);
    tokio::time::timeout(within, rx.recv())
        .await
        .expect("SIGINT forwarded")
        .expect("open");
    drop(rx);
    raise(libc::SIGTERM);
    tokio::time::timeout(within, task)
        .await
        .expect("the forwarder ends")
        .expect("no panic");
}
