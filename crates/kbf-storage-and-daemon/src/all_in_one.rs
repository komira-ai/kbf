//! `kbf all-in-one` stopping: a stop signal drains both halves and abandons no lease.
//!
//! The halves talk over the server half's worker listener on loopback: the daemon half's
//! `Hello`, heartbeats, `Result`s and their acknowledgements all go through the server
//! half, and its lease blobs through the server's cache. So the server half must outlive
//! the daemon half's drain, and the daemon half must still take leases while the server
//! half has accepted work that has not run. The order of a stop:
//!
//! 1. The first SIGTERM or SIGINT begins the server half's drain, the plain one of
//!    `kbf-server`: `/readyz` answers 503, new Execute calls are refused `UNAVAILABLE`,
//!    the REAPI listener is sent GOAWAY, and every execution already open runs until its
//!    action is done. The daemon half is not told yet and goes on taking leases: an
//!    operation the server accepted before the signal and has not placed still runs,
//!    and so do all the others. (A drain the operator begins on the operator API,
//!    `POST /v1/frontend/drain`, is the same.)
//! 2. When the REAPI listener is done, so that no execution is open and none will be taken
//!    ([`kbf_server::Drain::settled`]), the daemon half drains
//!    ([`kbf_node::NodeHandle::serve_beside`]) as `kbf-daemon` does: it takes no new
//!    lease, lets the ones still running finish and report (a lease whose caller went
//!    away has no execution), and waits for their acknowledgements, however long that
//!    takes.
//! 3. The server half is held ([`kbf_server::Drain::hold`]) from its start. When the
//!    REAPI listener is done it does not stop, and goes on serving the worker listener,
//!    until the daemon half has returned: the hold is released then, the server half
//!    stands down, and the process exits 0.
//!
//! A second signal is the hard stop of both halves: the server half closes its open
//! streams (they end `UNAVAILABLE`), the daemon half returns at once and abandons the
//! leases it still runs, and the hold gives way. `--shutdown-timeout-secs` (default none)
//! is the server half's own hard stop and ends the process the same way: when it runs out
//! the server half closes its streams and stops, and the daemon half stops with it.
//! `--drain-deadline-secs` is the daemon half's and, as on `kbf daemon`, only logs one
//! warning with the number of leases still running; `kbf frontend`'s `--drain-deadline`
//! and `--drain-lb-wait` are not flags of this mode.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_node::Signals;
use kbf_threads::Topology;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use crate::LocalDataPlane;
use crate::cli::Flags;
use crate::run::{Failed, forwarded, in_process, with_draining_daemon};

/// Where each stop signal goes: the server half reads the first and the second as two
/// events, the daemon half as `()`s on a channel.
pub(crate) struct Fan {
    daemon: UnboundedSender<()>,
    /// The daemon half has been told to drain.
    told: AtomicBool,
}

impl Fan {
    pub(crate) fn new(daemon: UnboundedSender<()>) -> Self {
        Self {
            daemon,
            told: AtomicBool::new(false),
        }
    }

    /// Tells the daemon half to drain, unless it has been told.
    pub(crate) fn begin_daemon(&self) {
        if !self.told.swap(true, Ordering::SeqCst) {
            let _ = self.daemon.send(());
        }
    }

    /// Tells the daemon half to stop at once: it drains first if it had not begun, so what
    /// it reads is always the first and then the second.
    fn stop_daemon(&self) {
        self.begin_daemon();
        let _ = self.daemon.send(());
    }
}

/// Hands the stop signals read from `signals` to the halves. The first goes to the server
/// half (`first`), which begins its drain; the daemon half is not told by it ([`daemon_follows`]
/// begins the daemon half's drain once the server half has settled). The second and later go to
/// the server half (`second`: it ends its open streams) and stop the daemon half at once.
/// Returns when `signals` closes.
pub(crate) async fn fan_out(
    mut signals: UnboundedReceiver<()>,
    fan: Arc<Fan>,
    first: oneshot::Sender<()>,
    second: oneshot::Sender<()>,
) {
    let (mut first, mut second) = (Some(first), Some(second));
    while signals.recv().await.is_some() {
        if let Some(first) = first.take() {
            let _ = first.send(());
            continue;
        }
        // The second signal and every later one.
        if let Some(second) = second.take() {
            let _ = second.send(());
        }
        fan.stop_daemon();
    }
}

/// Begins the daemon half's drain when the server half's has settled (the module docs,
/// step 2), however the drain began: a stop signal or the operator API.
pub(crate) async fn daemon_follows(settled: impl Future<Output = ()>, fan: Arc<Fan>) {
    settled.await;
    fan.begin_daemon();
}

/// Completes when `fired` is sent; never if the sender is dropped without sending, so a
/// half that is gone is not taken for a signal.
pub(crate) async fn when_sent(fired: oneshot::Receiver<()>) {
    if fired.await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Releases `hold` once `stopped` completes, or is dropped.
pub(crate) async fn release_after(stopped: oneshot::Receiver<()>, hold: kbf_server::Hold) {
    let _ = stopped.await;
    drop(hold);
}

/// The server half on a runtime of its own, then the daemon half over the in-process
/// plane; returns once the server half has stopped and the daemon half before it (the
/// module docs say in what order a stop goes). Both halves start from `flags`, as
/// [`crate::run::prepared`] made them: no half sees another copy.
pub(crate) fn serve(flags: Flags) -> Result<(), Failed> {
    serve_on(
        flags,
        Box::new(|tokio| {
            // First of all: from here a SIGTERM waits to be read, and is not the default
            // action that ends the process.
            let _context = tokio.enter();
            Signals::install().map(|signals| forwarded(tokio, signals))
        }),
        Box::new(|_| {}),
    )
}

/// What makes the stop signals on the runtime it is given.
pub(crate) type MakeSignals = Box<dyn FnOnce(&Runtime) -> std::io::Result<UnboundedReceiver<()>>>;

/// What is called with the server half once it is bound.
pub(crate) type Started = Box<dyn FnOnce(&kbf_server::ServerHandle)>;

/// [`serve`] on the stop signals `signals` makes on the runtime it is given (one `()` for
/// each SIGTERM or SIGINT, [`forwarded`]), calling `started` with the server half once it
/// is bound. Boxed, not generic: one body serves the mode and every test, so its branches
/// are measured once, whichever test reaches which.
pub(crate) fn serve_on(flags: Flags, signals: MakeSignals, started: Started) -> Result<(), Failed> {
    let sizes = kbf_server::sizes_on(&flags.server, Topology::detect());
    sizes.log();
    let tokio = sizes.runtime().map_err(Failed::server)?;
    let mut signals = signals(&tokio).map_err(Failed::server)?;
    // The server half's stop is the first signal as the fan-out below hands it on, not
    // the server's own handlers: one reader gives both halves the same first and second.
    let (first, first_sent) = oneshot::channel::<()>();
    let (second, second_sent) = oneshot::channel::<()>();
    let start = kbf_server::start(&flags.server, move || Ok(when_sent(first_sent)));
    // A signal before or while the server half starts abandons the start: nothing is
    // running yet, and the process exits 0. The signal is polled first, so one already
    // queued wins without the start being polled at all.
    let bound = tokio.block_on(async {
        match futures::future::select(Box::pin(signals.recv()), Box::pin(start)).await {
            futures::future::Either::Left(_) => None,
            futures::future::Either::Right((server, _)) => Some(server),
        }
    });
    let Some(server) = bound else {
        tracing::info!("stopped before the server half started");
        return Ok(());
    };
    let server = server.map_err(Failed::server)?;
    println!("{}", server.start_line());
    started(&server);
    let plane = Arc::new(LocalDataPlane::new(server.local(), tokio.handle().clone()));
    let drain = server.drain();
    // Held until the daemon half has drained (the module docs, step 3).
    let hold = drain.hold();
    let server = server.escalate_on(when_sent(second_sent));
    let (daemon, received) = tokio::sync::mpsc::unbounded_channel();
    let fan = Arc::new(Fan::new(daemon));
    tokio.spawn(fan_out(signals, Arc::clone(&fan), first, second));
    let settled = async move { drain.settled().await };
    tokio.spawn(daemon_follows(settled, fan));
    let (stop, stopped) = oneshot::channel::<()>();
    tokio.spawn(release_after(stopped, hold));
    let serving = Box::pin(server.serve());
    with_draining_daemon(
        &tokio,
        serving,
        stop,
        in_process(plane),
        received,
        &flags.daemon,
    )
}

#[cfg(test)]
#[path = "all_in_one_tests.rs"]
mod tests;
