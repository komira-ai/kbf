use super::*;

/// A daemon set up and not yet serving, with the async runtime the thread flags size.
/// [`NodeHandle::serve`] runs it until a signal; [`NodeHandle::serve_until`] until a
/// future of the caller's.
pub struct NodeHandle {
    pub(crate) tokio: tokio::runtime::Runtime,
    pub(crate) daemon: Daemon,
    /// `--drain-deadline-secs`, when to warn that leases still run; `None` waits without
    /// a word.
    pub(crate) drain_deadline: Option<Duration>,
    /// The storage network check of a daemon that keeps the frontend's map.
    pub(crate) reach: Option<ReachCheck>,
}

/// What [`kbf_daemon::reach`] needs once the daemon serves: the map it keeps and the
/// member that is this process, which is not dialled.
pub(crate) struct ReachCheck {
    pub(crate) maps: Arc<MapHolder>,
    local: Option<String>,
    pub(crate) plan: kbf_daemon::reach::Plan,
}

impl ReachCheck {
    /// Completes with the error when no storage member answers the first map's probe;
    /// never otherwise.
    async fn refused(self) -> Error {
        let checked =
            kbf_daemon::reach::check_first_map(self.maps, self.local.as_deref(), self.plan).await;
        match checked {
            Err(error) => error.into(),
            Ok(()) => std::future::pending().await,
        }
    }
}

/// `serving` until it ends, or until `reach` finds no storage member reachable: then the
/// error, and the daemon is dropped (its leases abandoned, as at any stop).
async fn serve_or_refuse(
    serving: std::pin::Pin<Box<dyn Future<Output = ()> + '_>>,
    reach: Option<ReachCheck>,
) -> Result<(), Error> {
    let refused = async {
        match reach {
            Some(reach) => reach.refused().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        () = serving => Ok(()),
        error = refused => Err(error),
    }
}

impl NodeHandle {
    /// This node, ending with an error when the first storage map it holds names members
    /// and none answers a connect (`local`, this process's own member, is not tried).
    pub(crate) fn checking_reach(mut self, maps: Arc<MapHolder>, local: Option<String>) -> Self {
        self.reach = Some(ReachCheck {
            maps,
            local,
            plan: kbf_daemon::reach::Plan::DEFAULT,
        });
        self
    }

    /// Serves until SIGTERM or SIGINT.
    ///
    /// # Errors
    /// The signal handlers cannot be installed, or no storage member is reachable from
    /// this host ([`kbf_daemon::reach`]).
    pub fn serve(self) -> Result<(), Error> {
        let signals = {
            let _context = self.tokio.enter();
            Signals::install()?
        };
        self.serve_alongside(signals, std::future::pending())
    }

    /// [`Self::serve`] for a daemon beside another half in the same process, on `signals`
    /// the caller installed earlier ([`Signals::install`]: one that arrived since then is
    /// the first, so a stop during the other half's start is not lost). The first signal
    /// drains as `serve` does (the half beside it is not told until the drain is over, so
    /// a running lease still reaches its blobs through it), and `other` completing (that
    /// half stopped on its own) stops the daemon at once, abandoning the leases still
    /// running.
    ///
    /// # Errors
    /// As [`Self::serve`], less the handlers.
    pub fn serve_alongside(
        self,
        signals: Signals,
        other: impl Future<Output = ()>,
    ) -> Result<(), Error> {
        let (sender, received) = tokio::sync::mpsc::unbounded_channel();
        self.tokio.spawn(forward(signals, sender));
        self.serve_beside(received, other)
    }

    /// [`Self::serve_alongside`] on stop signals the caller reads itself: drains on the
    /// first `()` of `received` and ends at the second, when the drain is complete, or
    /// when `other` completes, whichever is first ([`Daemon::run_draining`]); the drain
    /// deadline does not end it. A process that hands each signal to another half too
    /// (`kbf all-in-one`) reads the signals once and feeds this.
    ///
    /// # Errors
    /// As [`Self::serve_alongside`].
    pub fn serve_beside(
        self,
        received: tokio::sync::mpsc::UnboundedReceiver<()>,
        other: impl Future<Output = ()>,
    ) -> Result<(), Error> {
        let Self {
            tokio,
            daemon,
            drain_deadline,
            reach,
        } = self;
        let serving = Box::pin(async {
            tokio::select! {
                () = daemon.run_draining(received, drain_deadline) => {}
                () = other => tracing::warn!("the half beside the daemon stopped: stopping"),
            }
        });
        tokio.block_on(serve_or_refuse(serving, reach))
    }

    /// Serves until `shutdown` completes, then abandons the leases still running.
    ///
    /// # Errors
    /// No storage member is reachable from this host ([`kbf_daemon::reach`]).
    pub fn serve_until(self, shutdown: impl Future<Output = ()>) -> Result<(), Error> {
        let Self {
            tokio,
            daemon,
            reach,
            ..
        } = self;
        let serving = Box::pin(daemon.run(shutdown));
        tokio.block_on(serve_or_refuse(serving, reach))
    }
}

/// The SIGTERM and SIGINT handlers of a process.
pub struct Signals {
    term: Signal,
    int: Signal,
}

impl Signals {
    /// Installs the handlers now, in the context of the tokio runtime that is entered:
    /// from here on a signal is kept until [`Self::recv`] takes it, not given its default
    /// action (which ends the process).
    ///
    /// # Errors
    /// The handlers cannot be installed.
    pub fn install() -> std::io::Result<Self> {
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
        })
    }

    /// The next SIGTERM or SIGINT (two that arrived before it count as one), by name.
    pub async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.term.recv() => "SIGTERM",
            _ = self.int.recv() => "SIGINT",
        }
    }
}

/// Sends a `()` on `signals` for every SIGTERM or SIGINT, until the receiver is gone.
pub(crate) async fn forward(mut signals: Signals, sender: tokio::sync::mpsc::UnboundedSender<()>) {
    loop {
        let name = signals.recv().await;
        tracing::info!("{name}: shutting down");
        if sender.send(()).is_err() {
            return;
        }
    }
}
