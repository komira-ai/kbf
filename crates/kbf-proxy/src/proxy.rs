//! `Proxy::bind`, `ProxyRunner::run` and `ProxyHandle`: the proxy as the process that hosts it
//! sees it.
//!
//! One multi-thread runtime, built inside [`Proxy::bind`] and reachable through
//! [`ProxyHandle::runtime`], so the signal handlers of the host run on it and no second
//! runtime exists.
//!
//! The proxy balances over the `[[frontend]]` tables of its config: the members are polled
//! (`pool_probe`), and the admin API is served on `admin_listen` if it is set (`pool_admin`).
//! The admin socket is bound by [`Proxy::bind`], so an address in use is a failed start.

use std::io;
use std::net::SocketAddr;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use http::HeaderMap;
use tokio::net::TcpListener;
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::sync::watch;

use crate::auth::{Buck2Auth, Tokens};
use crate::config::ProxyConfig;
use crate::control::UpdateHandler;
use crate::core::Core;
use crate::identity::{ProxySecret, SecretOrigin, admin_headers};
use crate::metrics::Metrics;
use crate::pool::tcp_connector;
use crate::pool_admin;
use crate::pool_probe;
use crate::security::Security;
use crate::serve::serve;
use crate::tls::{Acceptor, TlsPaths, TlsReloadError, TlsReloader};
use crate::upstreams::Upstreams;

/// The most worker threads of the runtime.
const MAX_WORKERS: usize = 8;

/// The entry point: [`Proxy::bind`].
#[derive(Debug)]
pub struct Proxy;

impl Proxy {
    /// Takes the already-bound public listener (the activated socket), builds the runtime,
    /// and returns without serving. The socket of `admin_listen`, if set, is bound here too.
    ///
    /// The listener speaks TLS: the server certificate, key and client CA are read now (a
    /// proxy does not start on a bad file), as are the operator token and, for
    /// `buck2_auth = "bearer"`, the Buck2 key. The secret file the children read is read or
    /// created (see [`ProxyHandle::secret_origin`]).
    ///
    /// # Errors
    /// The config has no `[[frontend]]` table, a token or TLS file is refused, the secret file
    /// cannot be read or created, the runtime could not be built, the socket could not be made
    /// non-blocking, or the admin address is in use.
    pub fn bind(cfg: ProxyConfig, listener: OwnedFd) -> io::Result<(ProxyHandle, ProxyRunner)> {
        if cfg.frontend.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the proxy balances over [[frontend]] tables, and the config has none",
            ));
        }
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
        let members = Upstreams::new(&cfg.frontend);
        // Bound before anything is created: an address in use is a start that failed.
        let admin = cfg
            .admin_listen
            .map(|addr| admin_socket(addr, &runtime))
            .transpose()?;
        let metrics = Arc::new(Metrics::default());
        let paths = TlsPaths {
            certs: cfg.cert_entries(),
            client_ca: cfg.client_ca_file,
        };
        let tls =
            Arc::new(TlsReloader::load(paths, Arc::clone(&metrics)).map_err(io::Error::other)?);
        // The last step that can fail: a start that exits earlier has not made a secret that
        // a child could have read (`SecretOrigin::Created`).
        let (secret, origin) = ProxySecret::create_or_read(&cfg.proxy_auth_file)?;
        let security = Security::new(cfg.buck2_auth, tokens, secret.clone());
        let core = Core::new(
            members,
            cfg.limits,
            tcp_connector(),
            security,
            metrics,
            cfg.ui,
        );
        let (stop, stopped) = watch::channel(false);
        let handle = ProxyHandle {
            core: Arc::clone(&core),
            runtime: runtime.handle().clone(),
            stop: Arc::new(stop),
            tls: Arc::clone(&tls),
            secret,
            origin,
        };
        let runner = ProxyRunner {
            runtime,
            core,
            listener,
            admin,
            tls,
            stopped,
        };
        Ok((handle, runner))
    }
}

/// The admin socket, bound now and made known to the runtime.
fn admin_socket(addr: SocketAddr, runtime: &Runtime) -> io::Result<TcpListener> {
    let listener = std::net::TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let _entered = runtime.enter();
    TcpListener::from_std(listener)
}

/// Serves until [`ProxyHandle::shutdown`].
#[derive(Debug)]
pub struct ProxyRunner {
    runtime: Runtime,
    core: Arc<Core>,
    listener: TcpListener,
    /// The admin API's socket, with `admin_listen` set.
    admin: Option<TcpListener>,
    tls: Arc<TlsReloader>,
    stopped: watch::Receiver<bool>,
}

impl ProxyRunner {
    /// Serves on the calling thread (a synchronous `main`) until `shutdown`.
    ///
    /// # Errors
    /// None today; the signature leaves room for a listener that fails.
    pub fn run<U: UpdateHandler + 'static>(self, updates: U) -> io::Result<()> {
        let Self {
            runtime,
            core,
            listener,
            admin,
            tls,
            stopped,
        } = self;
        runtime.block_on(async move {
            // A poll of each member, and the admin API if it has a socket.
            for index in 0..core.upstreams.len() {
                let (core, members) = (Arc::clone(&core), Arc::clone(&core.upstreams));
                tokio::spawn(pool_probe::poll(core, members, index));
            }
            if let Some(admin) = admin {
                let members = Arc::clone(&core.upstreams);
                tokio::spawn(pool_admin::serve(admin, members, core.limits));
            }
            serve(core, updates, listener, Acceptor::Tls(tls), stopped).await;
        });
        Ok(())
    }
}

/// What the host of the proxy holds: the runtime, the proxy's numbers and the stop.
#[derive(Clone, Debug)]
pub struct ProxyHandle {
    core: Arc<Core>,
    runtime: Handle,
    stop: Arc<watch::Sender<bool>>,
    tls: Arc<TlsReloader>,
    secret: ProxySecret,
    origin: SecretOrigin,
}

impl ProxyHandle {
    /// The proxy's runtime: the host's signal handlers run here. The runtime is dropped when
    /// [`ProxyRunner::run`] returns, so a task spawned after that never runs.
    #[must_use]
    pub fn runtime(&self) -> Handle {
        self.runtime.clone()
    }

    /// Streams in flight; a hint, read for `== 0`. The streams open on the members count too:
    /// a stream is on a member until it ends.
    #[must_use]
    pub fn open_streams(&self) -> u64 {
        self.core.open_streams()
    }

    /// Whether the secret file was there (`Existing`: children that read it earlier still
    /// hold this secret) or was written by this start (`Created`: a child that started
    /// earlier would read every call as anonymous until it is restarted).
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
