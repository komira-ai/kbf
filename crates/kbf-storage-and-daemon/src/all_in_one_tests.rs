//! The stop of `kbf all-in-one`: where each signal goes, the order of the two halves'
//! drains, and the whole mode stopped through the crate's own API. Time is paused where
//! a clock is involved; the rest waits on events.

use std::time::Duration;

use clap::Parser as _;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::*;
use crate::run::tests::fake_daemon;
use crate::run::{drain_then_stop, forward_signals, prepared};

/// A pause long enough for every task that can run to have run, on a paused clock.
const SETTLE: Duration = Duration::from_millis(10);

/// What the daemon half has been told so far.
fn told(daemon: &mut UnboundedReceiver<()>) -> usize {
    std::iter::from_fn(|| daemon.try_recv().ok()).count()
}

/// The pieces of the signal fan-out: the sender of stop signals, the daemon half's
/// channel, the server half's two events, and the fan.
struct Rig {
    signals: UnboundedSender<()>,
    daemon: UnboundedReceiver<()>,
    first: oneshot::Receiver<()>,
    second: oneshot::Receiver<()>,
    fan: Arc<Fan>,
    fanning: tokio::task::JoinHandle<()>,
}

impl Rig {
    fn new() -> Self {
        let (signals, received) = unbounded_channel();
        let (daemon_tx, daemon) = unbounded_channel();
        let (first_tx, first) = oneshot::channel();
        let (second_tx, second) = oneshot::channel();
        let fan = Arc::new(Fan::new(daemon_tx));
        let fanning = tokio::spawn(fan_out(received, Arc::clone(&fan), first_tx, second_tx));
        Self {
            signals,
            daemon,
            first,
            second,
            fan,
            fanning,
        }
    }

    /// Sends one stop signal and lets the fan-out take it.
    async fn signal(&self) {
        self.signals.send(()).expect("the fan-out reads");
        tokio::time::sleep(SETTLE).await;
    }
}

/// Catches a first signal that stops the daemon half (the old behaviour of the mode, and
/// the way a lease is cut), or one the server half does not see, and a second that does
/// not stop both: the first signal reaches the server half as its stop and tells the daemon
/// half nothing; the second reaches the server half as its escalation and stops the daemon
/// half at once, after the drain that stop needs to begin first, so the daemon half reads
/// a begin and then a stop and never a stop alone.
#[tokio::test(start_paused = true)]
async fn the_first_signal_drains_the_server_half_and_the_second_stops_both() {
    let mut rig = Rig::new();
    rig.signal().await;
    assert_eq!(rig.first.try_recv(), Ok(()), "the server half was not told");
    assert!(rig.second.try_recv().is_err(), "one signal escalated");
    assert_eq!(
        told(&mut rig.daemon),
        0,
        "the first signal reached the daemon"
    );

    rig.signal().await;
    assert_eq!(rig.second.try_recv(), Ok(()), "the second did not escalate");
    assert_eq!(told(&mut rig.daemon), 2, "a begin and then a stop");

    // A third signal has no server event left to fire and stops the daemon half again; the
    // fan-out ends when the signals do.
    rig.signal().await;
    assert_eq!(told(&mut rig.daemon), 1, "a later signal was dropped");
    let Rig {
        signals, fanning, ..
    } = rig;
    drop(signals);
    fanning.await.expect("the fan-out ends with its signals");
}

/// Catches a daemon half that drains before the server half has settled, and one that never
/// drains: once the server half has settled (its executions ended, however the drain
/// began) the daemon half is told to drain once, and a first signal that comes after it
/// (an operator who stops a host whose drain the API began) adds nothing: it is not a hard
/// stop; the second signal then is.
#[tokio::test(start_paused = true)]
async fn the_daemon_half_drains_once_the_server_half_has_settled_and_a_late_signal_is_not_a_stop() {
    let mut rig = Rig::new();
    let (settle, settled) = oneshot::channel::<()>();
    tokio::spawn(daemon_follows(
        async move {
            let _ = settled.await;
        },
        Arc::clone(&rig.fan),
    ));
    tokio::time::sleep(SETTLE).await;
    assert_eq!(told(&mut rig.daemon), 0, "told before the server settled");

    settle.send(()).expect("waiting");
    tokio::time::sleep(SETTLE).await;
    assert_eq!(told(&mut rig.daemon), 1, "told to drain");

    rig.signal().await;
    assert_eq!(
        told(&mut rig.daemon),
        0,
        "the first signal stopped the daemon"
    );
    rig.signal().await;
    assert_eq!(told(&mut rig.daemon), 1, "the second did not stop it");
}

/// Catches a second signal that stops the daemon half before it drains (it would read the
/// stop as its first signal and begin a drain that nothing ends), and a drain that begins
/// twice: a second signal during the server half's drain gives the daemon half a begin and
/// a stop, and the settle that follows adds nothing.
#[tokio::test(start_paused = true)]
async fn a_second_signal_before_the_settle_still_gives_the_daemon_half_a_begin_then_a_stop() {
    let mut rig = Rig::new();
    let (settle, settled) = oneshot::channel::<()>();
    tokio::spawn(daemon_follows(
        async move {
            let _ = settled.await;
        },
        Arc::clone(&rig.fan),
    ));
    rig.signal().await;
    rig.signal().await;
    assert_eq!(told(&mut rig.daemon), 2);
    settle.send(()).expect("waiting");
    tokio::time::sleep(SETTLE).await;
    assert_eq!(told(&mut rig.daemon), 0, "the settle began a second drain");
}

/// Catches a server half that is taken for stopped when its sender is gone: `when_sent`
/// completes on a send and never on a drop, so a fan-out that ends does not stop a half.
#[tokio::test(start_paused = true)]
async fn a_dropped_sender_is_not_a_signal() {
    let (sent, received) = oneshot::channel::<()>();
    let waiting = tokio::spawn(when_sent(received));
    drop(sent);
    tokio::time::sleep(SETTLE).await;
    assert!(!waiting.is_finished(), "a dropped sender was a signal");
    let (sent, received) = oneshot::channel::<()>();
    sent.send(()).expect("waiting");
    when_sent(received).await;
}

/// A drain over a fresh closer and readiness, as the server half has.
fn server_drain() -> Arc<kbf_server::Drain> {
    let (closer, _closing) = kbf_front::closing();
    let readiness = Arc::new(kbf_server::Readiness::default());
    let (drain, _goaway) = kbf_server::Drain::new(closer, readiness);
    Arc::new(drain)
}

/// Catches the order that cuts work: the server half stands down while the daemon half
/// still drains, so a lease it runs can no longer report to a server that is gone. The
/// server half's end waits on its hold; the hold lasts until the daemon half has returned
/// from its drain (here blocked on a lease that the test ends) and is released then, so
/// the stop finishes only after the lease. Catches: `release_after` dropping the hold
/// before `stop` is sent, or the stop sent before the drain returned.
#[test]
fn the_server_half_is_held_until_the_daemon_half_has_drained() {
    let tokio = Runtime::new().expect("runtime");
    let drain = server_drain();
    let hold = drain.hold();
    assert!(drain.is_held());
    let (stop, stopped) = oneshot::channel::<()>();
    tokio.spawn(release_after(stopped, hold));
    let serving = {
        let drain = Arc::clone(&drain);
        tokio.spawn(async move {
            // What the server half's end waits for once its executions are gone.
            drain.may_stand_down().await;
            Ok(())
        })
    };
    let (draining, is_draining) = std::sync::mpsc::channel::<()>();
    let (lease_ends, lease_ended) = std::sync::mpsc::channel::<()>();
    let stopping = std::thread::spawn(move || {
        drain_then_stop(&tokio, serving, stop, move || {
            draining.send(()).expect("the test waits");
            lease_ended.recv().expect("the lease ends");
            Ok(())
        })
        .map_err(|failed| failed.said("server"))
    });
    is_draining.recv().expect("the daemon half drains");
    // The drain is blocked on a running lease: the server half must still be up.
    assert!(drain.is_held(), "the hold was released while a lease runs");
    assert!(
        !stopping.is_finished(),
        "the stop finished with a lease running"
    );
    lease_ends.send(()).expect("the drain waits");
    let stopped = stopping.join().expect("the stop ends");
    assert_eq!(stopped, Ok(()), "a clean stop");
    assert!(!drain.is_held(), "the hold outlived the stop");
}

/// Catches a hold that outlives a hard stop: a second signal closes the drain, and the
/// server half then stands down whatever is held, so the hard stop ends the process.
#[tokio::test(start_paused = true)]
async fn a_close_gives_way_to_the_hold() {
    let drain = server_drain();
    let _hold = drain.hold();
    let waiting = tokio::spawn({
        let drain = Arc::clone(&drain);
        async move { drain.may_stand_down().await }
    });
    tokio::time::sleep(SETTLE).await;
    assert!(!waiting.is_finished(), "a hold did not hold");
    drain.close();
    waiting.await.expect("a close ends the wait");
}

/// The flags of the whole mode over temp stores, and a handle to remove them with.
fn flags(name: &str) -> (kbf_testkit::dir::TestDir, Flags) {
    let (dir, daemon) = fake_daemon(name);
    let at = |name: &str| dir.join(name).display().to_string();
    let server = kbf_server::Args::try_parse_from([
        "kbf-server".to_owned(),
        "--listen=127.0.0.1:0".to_owned(),
        "--worker-listen=127.0.0.1:0".to_owned(),
        "--default-platform=none".to_owned(),
        format!("--cold-store=file://{}", at("cold")),
        "--cold-store-floor-bytes=1".to_owned(),
        format!("--meta-dir={}", at("meta")),
    ])
    .expect("the server's flags");
    (dir, Flags { server, daemon })
}

/// The mode through the crate's own API, not the `kbf` binary: the server half starts on
/// its flags over temp stores, the daemon half (fake driver, dialling nothing that
/// answers) starts over the in-process plane, and `signals` stop signals sent once the
/// server half is bound make both stop cleanly, `serve_on` returning `Ok`, on a thread the
/// test waits on for 60 s so a stop that never ends fails here instead of hanging the run.
fn stopped_by(name: &str, signals: usize) -> Result<(), String> {
    let (dir, flags) = flags(name);
    let (ended, result) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (sender, received) = unbounded_channel::<()>();
        let served = serve_on(
            prepared(&flags),
            Box::new(move |_| Ok(received)),
            Box::new(move |_server| {
                for _ in 0..signals {
                    sender.send(()).expect("the fan-out reads");
                }
            }),
        );
        ended
            .send(served.map_err(|failed| failed.said("server")))
            .ok();
    });
    let served = result
        .recv_timeout(Duration::from_secs(60))
        .expect("both halves stopped");
    drop(dir);
    served
}

/// Catches a mode that does not stop on a signal, or stops one half only: one signal
/// sent once the server half is bound drains the server half, which settles, which drains
/// the daemon half, which has nothing to wait for; the hold is released and the process
/// ends `Ok` (the lease-running cases are the daemon's own drain tests and the server's
/// hold tests). Catches: `daemon_follows` never told, or the hold never released.
#[test]
fn one_signal_drains_both_halves_and_the_mode_stops() {
    assert_eq!(stopped_by("aio-one-signal", 1), Ok(()));
}

/// Catches a hard stop that does not end the mode: two signals stop both halves at once.
#[test]
fn two_signals_stop_both_halves() {
    assert_eq!(stopped_by("aio-two-signals", 2), Ok(()));
}

/// Catches the forwarding task of the signals outliving its reader (a task per stopped
/// mode) or dropping a signal: a signal reaches the channel as one `()`, and once the
/// receiver is gone the next signal ends the task. The task is awaited under a bound, so
/// a task that never ends fails here instead of hanging the run.
#[tokio::test]
async fn forwarded_signals_reach_the_channel_and_the_task_ends_with_its_reader() {
    let within = Duration::from_secs(10);
    let raise = || {
        // SAFETY: raise(3) on this process, whose SIGTERM handler `install` made.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
    };
    let signals = kbf_node::Signals::install().expect("handlers");
    let (sender, mut received) = unbounded_channel();
    let task = tokio::spawn(forward_signals(signals, sender));
    raise();
    tokio::time::timeout(within, received.recv())
        .await
        .expect("the signal is forwarded")
        .expect("open");
    drop(received);
    raise();
    tokio::time::timeout(within, task)
        .await
        .expect("the task ends once nobody reads")
        .expect("no panic");
}

/// Catches a stop signal that arrives while the server half starts and is lost or obeyed
/// late: with one waiting before the start, the start is abandoned (the server half is
/// never bound) and the mode ends `Ok`, nothing running yet.
#[test]
fn a_signal_while_the_server_half_starts_abandons_the_start() {
    let (dir, flags) = flags("aio-signal-in-start");
    let (sender, received) = unbounded_channel::<()>();
    sender.send(()).expect("the receiver is kept");
    let served = serve_on(
        prepared(&flags),
        Box::new(move |_| Ok(received)),
        Box::new(|_| panic!("the server half started")),
    );
    assert!(served.is_ok());
    drop((dir, sender));
}

/// The mode as `kbf` runs it, [`serve`], with the process's own SIGTERM: sent again and
/// again from a thread until `serve` returns (a handler of the test's own is installed
/// first, so none is fatal before the mode has installed its own). Catches: `serve` not
/// installing the handlers or not reading them, which hangs the stop; it runs on a thread
/// the test waits on for 60 s.
#[test]
fn serve_stops_on_the_process_sigterm() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let (dir, flags) = flags("aio-sigterm");
    let guard = Runtime::new().expect("runtime");
    let _handled = {
        let _context = guard.enter();
        Signals::install().expect("handlers")
    };
    let done = Arc::new(AtomicBool::new(false));
    let sender = {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                // SAFETY: raise(3) on this process, which handles SIGTERM above.
                assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let (ended, result) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        ended
            .send(serve(prepared(&flags)).map_err(|f| f.said("server")))
            .ok();
    });
    let served = result.recv_timeout(Duration::from_secs(60));
    done.store(true, Ordering::SeqCst);
    sender.join().expect("the sender");
    assert_eq!(served.expect("the mode stopped"), Ok(()));
    drop(dir);
}
