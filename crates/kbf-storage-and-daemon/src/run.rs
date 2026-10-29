//! The two halves started and run together (the crate docs say how), for
//! `kbf all-in-one` (the server half) and `kbf storage-and-daemon` (the storage half,
//! `crate::storage_and_daemon`).

use std::fmt;
use std::future::Future;
use std::io::IsTerminal as _;
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;

use kbf_server::{ServeError, StartFailure};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::LocalDataPlane;
use crate::cli::Flags;

/// Starts both halves on the parsed flags and serves until the server half stops.
pub(crate) fn run(flags: &Flags) -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    exit(
        "kbf-all-in-one",
        "server",
        crate::all_in_one::serve(prepared(flags)),
    )
}

/// The exit status of a run that ended as `served` did, its failure said on stderr
/// after `name`, `half` naming the half beside the daemon (`server` or `storage`).
pub(crate) fn exit(name: &str, half: &str, served: Result<(), Failed>) -> ExitCode {
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(failed) => {
            eprint!("{}", failed.text(name, half));
            failed.code()
        }
    }
}

/// Why the process stopped other than on a signal. Which half is "that half" (the one
/// beside the daemon) is the caller's to say ([`Failed::said`]), so each failure is
/// made by a function, not a closure.
pub(crate) enum Failed {
    /// That half refused to start.
    Server(Box<dyn std::error::Error>),
    /// The daemon half refused to start.
    Daemon(kbf_node::Error),
    /// That half stopped serving on its own.
    Stopped(String),
}

impl Failed {
    pub(crate) fn server(e: impl Into<Box<dyn std::error::Error>>) -> Self {
        Self::Server(e.into())
    }

    fn stopped(e: impl fmt::Display) -> Self {
        Self::Stopped(e.to_string())
    }

    /// The exit status: for a start the server half refused, 2 as `kbf-server` exits, or
    /// 75 when a later try can succeed ([`StartFailure`]: a restart is for that one); the
    /// daemon's own for a start it refused; 1 for a server half that stopped.
    fn code(&self) -> ExitCode {
        match self {
            Self::Server(e) => StartFailure::of(e.as_ref()).code(),
            Self::Daemon(e) => kbf_node::exit_code(e),
            Self::Stopped(_) => ExitCode::FAILURE,
        }
    }

    /// What failed, `half` naming the half beside the daemon.
    /// What the process says on stderr, each line after `name`: the failure as
    /// [`Failed::said`] puts it, then [`Failed::hint`]'s line if there is one.
    fn text(&self, name: &str, half: &str) -> String {
        let mut text = format!("{name}: {}\n", self.said(half));
        if let Some(line) = self.hint() {
            text.push_str(&format!("{name}: {line}\n"));
        }
        text
    }

    /// What an operator must know beside the failure ([`kbf_server::start_hint`]).
    fn hint(&self) -> Option<&'static str> {
        match self {
            Self::Server(e) => kbf_server::start_hint(e.as_ref()),
            Self::Daemon(_) | Self::Stopped(_) => None,
        }
    }

    pub(crate) fn said(&self, half: &str) -> String {
        match self {
            Self::Server(e) => format!("{half}: {e}"),
            Self::Daemon(e) => format!("daemon: {e}"),
            Self::Stopped(e) => format!("the {half} half stopped: {e}"),
        }
    }
}

/// Both halves' flags as the mode runs them: given one cache when they share a disk
/// (`crate::one_cache`).
pub(crate) fn prepared(flags: &Flags) -> Flags {
    let mut flags = flags.clone();
    crate::one_cache::share(&mut flags.server, &mut flags.daemon);
    flags
}

/// What the half beside the daemon serves until it stops.
pub(crate) type Serving = Pin<Box<dyn Future<Output = Result<(), ServeError>> + Send>>;

/// How the daemon half starts: from its flags, over the data plane its mode gives it.
pub(crate) type Starter =
    Box<dyn FnOnce(&kbf_node::Cli) -> Result<kbf_node::NodeHandle, kbf_node::Error>>;

/// The daemon half over `plane`, the in-process plane of its half.
pub(crate) fn in_process(plane: Arc<LocalDataPlane>) -> Starter {
    Box::new(move |cli| kbf_node::start_with(cli, move |_| Ok(Arc::clone(&plane))))
}

/// The daemon half of `daemon` started by `start`, beside the half (storage or server)
/// `serving` on `tokio`: the daemon half drains on the first `()` of `received`, the
/// stop signals ([`forwarded`]; [`kbf_node::NodeHandle::serve_beside`]), while `serving`
/// goes on, and only then is `stop` fired, which stops the half beside it. A lease the
/// daemon half runs reads and writes its blobs through that half, so it must outlive the
/// drain. Returns once that half has stopped and the daemon half before it.
pub(crate) fn with_draining_daemon(
    tokio: &Runtime,
    serving: Serving,
    stop: tokio::sync::oneshot::Sender<()>,
    start: Starter,
    received: UnboundedReceiver<()>,
    daemon: &kbf_node::Cli,
) -> Result<(), Failed> {
    let (ended, other_ended) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio.spawn(async move {
        let _ended = ended;
        serving.await
    });
    let node = start(daemon).map_err(Failed::Daemon)?;
    drain_then_stop(tokio, serving, stop, || {
        node.serve_beside(received, async move {
            let _ = other_ended.await;
        })
    })
}

/// Reads every SIGTERM and SIGINT of `signals` on `tokio` and sends one `()` for each on
/// the returned channel, until the receiver is dropped: the stop signals as the daemon
/// half takes them, and as a process that hands them to another half takes them from.
pub(crate) fn forwarded(tokio: &Runtime, signals: kbf_node::Signals) -> UnboundedReceiver<()> {
    let (sender, received) = tokio::sync::mpsc::unbounded_channel();
    tokio.spawn(forward_signals(signals, sender));
    received
}

/// [`forwarded`]'s task: ends once nobody reads the channel.
pub(crate) async fn forward_signals(
    mut signals: kbf_node::Signals,
    sender: tokio::sync::mpsc::UnboundedSender<()>,
) {
    loop {
        let name = signals.recv().await;
        tracing::info!("{name}: shutting down");
        if sender.send(()).is_err() {
            return;
        }
    }
}

/// Runs `drain`, then fires `stop` and waits for `serving` to end: the order of
/// [`with_draining_daemon`]. A daemon that failed is the error unless the half beside it
/// failed first.
pub(crate) fn drain_then_stop(
    tokio: &Runtime,
    serving: tokio::task::JoinHandle<Result<(), ServeError>>,
    stop: tokio::sync::oneshot::Sender<()>,
    drain: impl FnOnce() -> Result<(), kbf_node::Error>,
) -> Result<(), Failed> {
    let drained = drain();
    let _ = stop.send(());
    let served = tokio.block_on(serving);
    let served = served.map_err(Failed::stopped)?;
    served.map_err(Failed::stopped)?;
    drained.map_err(Failed::Daemon)
}

#[cfg(test)]
pub(crate) mod tests {
    use clap::Parser as _;

    use super::*;
    use crate::cli::{command, parse};

    /// Catches a half started from flags other than the ones given the one cache (the
    /// daemon half handed a copy made before the sharing, or the server half): with the
    /// hot store on the data directory's disk, both halves of the flags `serve` takes
    /// hold the same cache.
    #[test]
    fn both_halves_start_with_the_one_cache() {
        let dir = crate::one_cache::under_target("prepared-all-in-one");
        let at = |name: &str| dir.join(name).display().to_string();
        let argv = [
            "kbf-all-in-one".to_owned(),
            "--server=https://127.0.0.1:8981".to_owned(),
            "--ca-cert=ca.pem".to_owned(),
            "--cert=node.pem".to_owned(),
            "--key=node.key".to_owned(),
            "--node-id=node-1".to_owned(),
            "--driver=container".to_owned(),
            format!("--hot-store=file://{}", at("hot")),
            format!("--data-dir={}", at("data")),
        ];
        let flags = parse(&command().get_matches_from(argv)).expect("flags");
        let flags = prepared(&flags);
        let local = flags
            .server
            .local_cache
            .as_ref()
            .expect("the server half's");
        assert!(crate::one_cache::daemon_holds(local, &flags.daemon));
    }

    /// Catches a failure that exits with another status than its half's (a refused
    /// server start must exit 2, as `kbf-server`'s does, and a server half that stops
    /// serving must not exit 0), or that says nothing of which half failed.
    #[test]
    fn each_failure_exits_with_its_halfs_status() {
        let stopped = Failed::stopped("listener gone");
        assert_eq!(
            stopped.said("server"),
            "the server half stopped: listener gone"
        );
        assert_eq!(
            stopped.said("storage"),
            "the storage half stopped: listener gone"
        );
        assert_eq!(stopped.code(), ExitCode::FAILURE);
        let server = Failed::server("no store");
        assert_eq!(server.said("server"), "server: no store");
        assert_eq!(server.said("storage"), "storage: no store");
        assert_eq!(server.code(), ExitCode::from(2));
        let daemon = Failed::Daemon("no scratch".into());
        assert_eq!(daemon.said("storage"), "daemon: no scratch");
        assert_eq!(daemon.code(), ExitCode::FAILURE);
    }

    /// Catches a storage half's start that can succeed on a later try but exits 2, which
    /// `RestartPreventExitStatus=2` never restarts (defects: `Failed::code` answering 2
    /// for every `Server`): a log directory the stopping process still holds, and a boot
    /// the frontend's restored state store handed out again (`StaleWriter`) exit 75, and
    /// a refusal that no restart clears (a log that is not one) still exits 2.
    #[test]
    fn a_start_a_later_try_can_pass_exits_75_and_a_refusal_2() {
        let stale = kbf_meta::StaleWriter {
            writer: kbf_meta::NodeWriter::new(std::num::NonZeroU64::MIN, std::num::NonZeroU64::MIN),
            current: std::num::NonZeroU64::MIN,
        };
        let stale = Failed::server(kbf_front::CacheError::StaleWriter(stale));
        assert_eq!(stale.code(), ExitCode::from(75));
        let held = Failed::server(kbf_server::RaftLogError::Locked {
            dir: "meta".into(),
            source: std::io::Error::other("held"),
        });
        assert_eq!(held.code(), ExitCode::from(75));
        let refused = Failed::server(kbf_server::RaftLogError::NotEmpty);
        assert_eq!(refused.code(), ExitCode::from(2));
    }

    /// Catches a StaleWriter start that does not say why it keeps restarting (it would read
    /// as an ordinary crash loop), and a hint printed for a failure it does not fit: the
    /// stale start's stderr is its error and then the restored-state-store line, a refusal's
    /// its error alone. Catches: `Failed::text` without the hint line.
    #[test]
    fn a_stale_start_says_the_state_store_is_older_and_a_refusal_only_itself() {
        let stale = kbf_meta::StaleWriter {
            writer: kbf_meta::NodeWriter::new(std::num::NonZeroU64::MIN, std::num::NonZeroU64::MIN),
            current: std::num::NonZeroU64::MIN,
        };
        let text = Failed::server(kbf_front::CacheError::StaleWriter(stale)).text("aio", "storage");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].starts_with("aio: storage: "), "{text}");
        assert_eq!(lines[1], format!("aio: {}", kbf_server::STALE_WRITER_HINT));
        let refused = Failed::server(kbf_server::RaftLogError::NotEmpty).text("aio", "storage");
        assert_eq!(refused.lines().count(), 1, "{refused}");
    }
    /// Catches the order of a stop (the defect of kbf#1087's restart drill): the half beside
    /// the daemon stopped before the daemon half had drained, so a running action lost the
    /// storage it reads and writes through. `stop` must fire only after the drain
    /// returned. Catches: `drain_then_stop` sending `stop` before it calls `drain`.
    #[test]
    fn the_half_beside_the_daemon_stops_only_after_the_drain() {
        let tokio = Runtime::new().expect("runtime");
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let beside = {
            let log = Arc::clone(&log);
            tokio.spawn(async move {
                let _ = stopped.await;
                log.lock().expect("log").push("half stopped");
                Ok(())
            })
        };
        let drained = {
            let log = Arc::clone(&log);
            move || {
                // Long enough for a `stop` sent too early to have stopped the half.
                std::thread::sleep(std::time::Duration::from_millis(300));
                log.lock().expect("log").push("drained");
                Ok(())
            }
        };
        let stopped = drain_then_stop(&tokio, beside, stop, drained);
        assert!(stopped.is_ok(), "a clean stop");
        assert_eq!(*log.lock().expect("log"), ["drained", "half stopped"]);
    }

    /// Catches a daemon half that failed leaving the half beside it running (the process
    /// would not exit), and one whose failure is lost: the half is stopped, then the
    /// daemon's error is the one reported; a half that failed on its own is reported too,
    /// as `the storage half stopped`.
    #[test]
    fn a_failed_drain_still_stops_the_half_beside_it_and_is_reported() {
        let tokio = Runtime::new().expect("runtime");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let beside = tokio.spawn(async move {
            let _ = stopped.await;
            Ok(())
        });
        let failed = drain_then_stop(&tokio, beside, stop, || Err("no scratch".into()));
        assert_eq!(
            failed.expect_err("failed").said("storage"),
            "daemon: no scratch"
        );

        let (stop, _stopped) = tokio::sync::oneshot::channel::<()>();
        let addr = "127.0.0.1:1".parse().expect("addr");
        let beside = tokio.spawn(async move {
            Err(ServeError::Bind {
                addr,
                source: std::io::Error::other("in use"),
            })
        });
        let failed = drain_then_stop(&tokio, beside, stop, || Ok(()));
        let said = failed.expect_err("failed").said("storage");
        assert!(
            said.starts_with("the storage half stopped: bind 127.0.0.1:1"),
            "{said}"
        );
    }

    /// The flags of a daemon half on the fake driver, with certificates it can read, in a
    /// directory of `name`, removed when the first returned drops.
    #[cfg(target_os = "linux")]
    pub(crate) fn fake_daemon(name: &str) -> (kbf_testkit::dir::TestDir, kbf_node::Cli) {
        let dir = crate::one_cache::under_target(name);
        let at = |name: &str| dir.join(name);
        let ca = rcgen::CertifiedIssuer::self_signed(
            rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params"),
            rcgen::KeyPair::generate().expect("key"),
        )
        .expect("CA");
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = rcgen::CertificateParams::new(vec!["node-1".to_owned()])
            .expect("params")
            .signed_by(&key, &ca)
            .expect("sign");
        std::fs::write(at("ca.pem"), ca.pem()).expect("write");
        std::fs::write(at("node.pem"), cert.pem()).expect("write");
        std::fs::write(at("node.key"), key.serialize_pem()).expect("write");
        let cli = kbf_node::Cli::try_parse_from([
            "kbf-daemon".to_owned(),
            "--server=https://127.0.0.1:1".to_owned(),
            format!("--ca-cert={}", at("ca.pem").display()),
            format!("--cert={}", at("node.pem").display()),
            format!("--key={}", at("node.key").display()),
            "--node-id=node-1".to_owned(),
            "--driver=fake".to_owned(),
            format!("--data-dir={}", at("data").display()),
        ])
        .expect("the daemon's flags");
        (dir, cli)
    }

    /// The daemon half's starter over a plane on an in-memory cache, as the modes give it
    /// theirs. The fake driver the tests use never asks for the plane.
    pub(crate) fn starter(tokio: &Runtime) -> Starter {
        let _context = tokio.enter();
        let cache = Arc::new(kbf_front::Cache::memory());
        let store = kbf_server::LocalStore::new(cache, kbf_front::SegmentsConfig::DEFAULT);
        in_process(Arc::new(LocalDataPlane::new(store, tokio.handle().clone())))
    }

    /// Catches `with_draining_daemon` not draining on a signal or not stopping the half
    /// beside the daemon after it: a daemon half started as `kbf storage-and-daemon`
    /// starts it, with a SIGTERM sent to this process, returns `Ok` and the half beside it
    /// has stopped. A start the daemon refuses is its error.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_signal_drains_the_daemon_half_and_then_stops_the_half_beside_it() {
        let (_dir, cli) = fake_daemon("draining-daemon");
        let tokio = Runtime::new().expect("runtime");
        // Installed before the signal is sent: one that arrives before the daemon half
        // serves is kept, as in the deployed mode where the storage half starts first.
        let install = || {
            let _context = tokio.enter();
            kbf_node::Signals::install().expect("handlers")
        };
        let signals = install();
        let stopped_at = Arc::new(std::sync::Mutex::new(None));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let serving = {
            let stopped_at = Arc::clone(&stopped_at);
            Box::pin(async move {
                let _ = stopped.await;
                *stopped_at.lock().expect("flag") = Some(());
                Ok(())
            })
        };
        let start = starter(&tokio);
        // SAFETY: raise(3) on this process, which handles SIGTERM above.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        let received = forwarded(&tokio, signals);
        let served = with_draining_daemon(&tokio, serving, stop, start, received, &cli);
        assert!(served.is_ok(), "drained and stopped");
        assert!(
            stopped_at.lock().expect("flag").is_some(),
            "the half beside it stopped"
        );

        let (stop, _stopped) = tokio::sync::oneshot::channel::<()>();
        let serving = Box::pin(std::future::pending());
        let refused: Starter = Box::new(|_| Err("no scratch".into()));
        let received = forwarded(&tokio, install());
        let failed = with_draining_daemon(&tokio, serving, stop, refused, received, &cli);
        assert_eq!(
            failed.expect_err("refused").said("storage"),
            "daemon: no scratch"
        );
    }
    /// Catches a daemon half that goes on serving after the half beside it stopped on its
    /// own (`with_draining_daemon` not firing the daemon's `other`): no signal is sent, the
    /// half beside it fails after a moment, and the daemon half stops at once and the
    /// failure is reported as the half's (`the storage half stopped`, not a clean exit).
    /// The call runs on a thread the test waits on for 30 s, so a daemon that serves on
    /// fails here instead of hanging the run. Catches: `other_ended` never awaited.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_half_that_stops_on_its_own_stops_the_daemon_half_and_is_reported() {
        let (_dir, cli) = fake_daemon("half-stops");
        let (ended, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let tokio = Runtime::new().expect("runtime");
            let signals = {
                let _context = tokio.enter();
                kbf_node::Signals::install().expect("handlers")
            };
            let (stop, _stopped) = tokio::sync::oneshot::channel::<()>();
            let addr = "127.0.0.1:1".parse().expect("addr");
            let serving = Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                Err(ServeError::Bind {
                    addr,
                    source: std::io::Error::other("in use"),
                })
            });
            let start = starter(&tokio);
            let received = forwarded(&tokio, signals);
            let served = with_draining_daemon(&tokio, serving, stop, start, received, &cli);
            ended
                .send(served.expect_err("the half failed").said("storage"))
                .ok();
        });
        let said = result
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the daemon half stopped with the half beside it");
        assert!(
            said.starts_with("the storage half stopped: bind 127.0.0.1:1"),
            "{said}"
        );
    }
}
