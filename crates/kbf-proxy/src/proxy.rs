//! `Proxy::bind`, `ProxyRunner::run` and `ProxyHandle`: the proxy as a supervisor sees it.
//!
//! One multi-thread runtime, built inside [`Proxy::bind`] and reachable through
//! [`ProxyHandle::runtime`], so the supervisor's actor (U7a) runs on it and no second runtime
//! exists. The gate starts holding, so a proxy that restarts under running children waits for
//! the supervisor to adopt them instead of answering `UNAVAILABLE`.

use std::io;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex, PoisonError};

use http::HeaderMap;
use tokio::net::TcpListener;
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::sync::watch;

use crate::auth::{Buck2Auth, Tokens};
use crate::config::ProxyConfig;
use crate::control::{ProxySnapshot, UpdateHandler};
use crate::core::{Core, now};
use crate::identity::{ProxySecret, SecretOrigin, admin_headers};
use crate::metrics::Metrics;
use crate::pool::tcp_connector;
use crate::probe::poll;
use crate::security::Security;
use crate::serve::serve;
use crate::slot::{ActiveSlot, HoldGuard};
use crate::tls::{Acceptor, TlsPaths, TlsReloadError, TlsReloader};

/// The most worker threads of the runtime.
const MAX_WORKERS: usize = 8;

/// The entry point: [`Proxy::bind`].
#[derive(Debug)]
pub struct Proxy;

impl Proxy {
    /// Takes the already-bound public listener (the activated socket), builds the runtime,
    /// and returns without serving. The gate starts holding for the configured frontend hold
    /// (`ActiveSlot::starting`); the supervisor takes [`ProxyHandle::startup_hold`] and ends
    /// it with `release_into` once it has adopted the children.
    ///
    /// The listener speaks TLS: the server certificate, key and client CA are read now (a
    /// proxy does not start on a bad file), as are the operator token and, for
    /// `buck2_auth = "bearer"`, the Buck2 key. The secret file the children read is read or
    /// created (see [`ProxyHandle::secret_origin`]).
    ///
    /// # Errors
    /// A token or TLS file is refused, the secret file cannot be read or created, the runtime
    /// could not be built, or the socket could not be made non-blocking.
    pub fn bind(cfg: ProxyConfig, listener: OwnedFd) -> io::Result<(ProxyHandle, ProxyRunner)> {
        let buck2_token = (cfg.buck2_auth == Buck2Auth::Bearer)
            .then_some(cfg.buck2_token_file.as_deref())
            .flatten();
        let tokens =
            Tokens::from_files(&cfg.operator_token_file, buck2_token).map_err(io::Error::other)?;
        let workers = std::thread::available_parallelism().map_or(1, usize::from);
        let runtime = Builder::new_multi_thread()
            .worker_threads(workers.min(MAX_WORKERS))
            .thread_name("kbf-proxy")
            .enable_all()
            .build()?;
        let std_listener = std::net::TcpListener::from(listener);
        std_listener.set_nonblocking(true)?;
        let listener = {
            let _entered = runtime.enter();
            TcpListener::from_std(std_listener)?
        };
        let hold_max = cfg.frontend_hold();
        let (slots, startup) = ActiveSlot::starting(hold_max);
        let metrics = Arc::new(Metrics::default());
        let paths = TlsPaths {
            cert: cfg.server_cert_file,
            key: cfg.server_key_file,
            client_ca: cfg.client_ca_file,
        };
        let tls =
            Arc::new(TlsReloader::load(paths, Arc::clone(&metrics)).map_err(io::Error::other)?);
        // The last step that can fail: a start that exits earlier has not made a secret that
        // a child could have read and the supervisor then adopted (`SecretOrigin::Created`).
        let (secret, origin) = ProxySecret::create_or_read(&cfg.proxy_auth_file)?;
        let security = Security::new(cfg.buck2_auth, tokens, secret.clone());
        let core = Core::with_metrics(
            cfg.upstream,
            cfg.limits,
            slots,
            tcp_connector(),
            security,
            metrics,
        );
        let (stop, stopped) = watch::channel(false);
        let handle = ProxyHandle {
            core: Arc::clone(&core),
            runtime: runtime.handle().clone(),
            stop: Arc::new(stop),
            startup: Arc::new(Mutex::new(Some(startup))),
            tls: Arc::clone(&tls),
            secret,
            origin,
        };
        let runner = ProxyRunner {
            runtime,
            core,
            listener,
            tls,
            stopped,
            hold_max,
        };
        Ok((handle, runner))
    }
}

/// Serves until [`ProxyHandle::shutdown`].
#[derive(Debug)]
pub struct ProxyRunner {
    runtime: Runtime,
    core: Arc<Core>,
    listener: TcpListener,
    tls: Arc<TlsReloader>,
    stopped: watch::Receiver<bool>,
    hold_max: std::time::Duration,
}

impl ProxyRunner {
    /// Serves on the calling thread (a synchronous `main`) until `shutdown`. A startup hold
    /// the supervisor has not released within the frontend hold of this call is ended by the
    /// proxy into nothing, so a supervisor that never arrives is `UNAVAILABLE`, not a silent
    /// hold.
    ///
    /// # Errors
    /// None today; the signature leaves room for a listener that fails.
    pub fn run<U: UpdateHandler + 'static>(self, updates: U) -> io::Result<()> {
        let Self {
            runtime,
            core,
            listener,
            tls,
            stopped,
            hold_max,
        } = self;
        runtime.block_on(async move {
            let abandon = Arc::clone(&core);
            tokio::spawn(async move { abandon.abandon_startup_after(hold_max).await });
            tokio::spawn(poll(Arc::clone(&core)));
            serve(core, updates, listener, Acceptor::Tls(tls), stopped).await;
        });
        Ok(())
    }
}

/// What a supervisor holds: the gate and the proxy's numbers.
#[derive(Clone, Debug)]
pub struct ProxyHandle {
    core: Arc<Core>,
    runtime: Handle,
    stop: Arc<watch::Sender<bool>>,
    startup: Arc<Mutex<Option<HoldGuard>>>,
    tls: Arc<TlsReloader>,
    secret: ProxySecret,
    origin: SecretOrigin,
}

impl ProxyHandle {
    /// The proxy's runtime: the supervisor's async actor runs here. The runtime is dropped
    /// when [`ProxyRunner::run`] returns, so a task spawned after that never runs.
    #[must_use]
    pub fn runtime(&self) -> Handle {
        self.runtime.clone()
    }

    /// The gate: hold, release into a slot, the active slot, open and retire.
    #[must_use]
    pub fn slots(&self) -> &ActiveSlot {
        &self.core.slots
    }

    /// Fails every held stream past its bound now and returns how many: the sweep the swap
    /// calls on each poll. A stream's own deadline stays the guarantee. The count is
    /// `#[must_use]`: a caller that does not need it writes `let _ = handle.expire();`.
    #[must_use]
    pub fn expire(&self) -> usize {
        self.core.queue.expire(now())
    }

    /// The startup hold, once. `release_into` ends the adoption; dropping it ends the hold
    /// into nothing.
    #[must_use]
    pub fn startup_hold(&self) -> Option<HoldGuard> {
        self.startup
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Everything `GET /v1/update/frontend` reports; a hint.
    #[must_use]
    pub fn snapshot(&self) -> ProxySnapshot {
        self.core.snapshot()
    }

    /// Streams in flight, for restart-on-idle; a hint, read for `== 0`.
    #[must_use]
    pub fn open_streams(&self) -> u64 {
        self.core.slots.open_streams()
    }

    /// Whether the secret file was there (`Existing`: children that read it earlier still
    /// hold this secret) or was written by this start (`Created`: a child that started
    /// earlier would read every call as anonymous, so the supervisor restarts it and does not
    /// adopt it).
    #[must_use]
    pub fn secret_origin(&self) -> SecretOrigin {
        self.origin
    }

    /// The headers every call to a child's admin port carries: the secret, and no identity.
    #[must_use]
    pub fn admin_headers(&self) -> HeaderMap {
        // A 32-byte secret is 64 hex digits, always a legal header value.
        admin_headers(&self.secret).unwrap_or_default()
    }

    /// Re-reads the server certificate, key and client CA together, and serves new handshakes
    /// with them if the three make a working configuration. This is what SIGHUP calls. On a
    /// failure the old configuration stays, `kbf_proxy_tls_reload_failed_total` ticks and the
    /// error is returned; connections already open keep the configuration they were accepted
    /// under.
    ///
    /// # Errors
    /// [`TlsReloadError`]: a file cannot be read, or the files do not make a configuration.
    pub fn reload_tls(&self) -> Result<(), TlsReloadError> {
        self.tls.reload()
    }

    /// Stops accepting, sends each connection a graceful GOAWAY and lets
    /// [`ProxyRunner::run`] return. Safe to call twice, and from any thread.
    pub fn shutdown(&self) {
        self.stop.send_replace(true);
    }
}
