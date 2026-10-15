//! The server's assembly, as a library: [`start`] opens the store and metadata log the
//! flags name, attaches the write tier and binds the listeners, and returns a
//! [`ServerHandle`] that serves. The `kbf-server` binary is `start`, a print and
//! `serve`; a process that runs the server beside the worker daemon calls the same
//! function.
//!
//! [`serve_with`] is the seam under it: the same assembly over a Raft metadata log and
//! object store the caller opened, for a store kind the flags do not name.

use std::error::Error;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use kbf_front::Cache;
use kbf_log::FarmLogId;
use kbf_meta::Retention;
use kbf_objstore::{FsStore, KeyPrefix, ObjectStore};
use kbf_store::StdFs;
use kbf_threads::Topology;
use kbf_threads::disk::Statvfs;

use crate::stores::{COLD_FILE_MIN_FREE_BYTES, FILE_PREFIX_ROOT, Hot, StoreUri, Stores};
use crate::write_tier::FLUSH_EVERY;
use crate::{Args, RaftMetaLog, SERVER_VERSION, ServeError, bind_server_with_policy, s3_store};

/// The server's thread sizes on a given machine shape: every CPU, then the overrides.
pub fn sizes_on(args: &Args, topology: Topology) -> kbf_threads::Sizes {
    args.threads.resolve(topology)
}

/// A started server: bound, its write tier attached and its flusher running, not yet
/// serving. [`ServerHandle::serve`] serves until the shutdown future given to [`start`]
/// completes; dropping the handle first stops the flusher and releases the listeners.
pub struct ServerHandle {
    /// Where the REAPI listener is.
    pub reapi: SocketAddr,
    /// Where the worker listener is.
    pub worker: SocketAddr,
    /// Where the operator API listener is, if there is one.
    pub api: Option<SocketAddr>,
    start_line: String,
    serving: Pin<Box<dyn Future<Output = Result<(), ServeError>> + Send>>,
    _flusher: Option<AbortOnDrop>,
}

impl ServerHandle {
    /// The line `kbf-server` prints once it is bound (see the binary's docs for its
    /// fields).
    #[must_use]
    pub fn start_line(&self) -> &str {
        &self.start_line
    }

    /// Serves every listener until the shutdown future ends and its connections close
    /// or `--shutdown-timeout-secs` runs out.
    ///
    /// # Errors
    /// A listener fails while serving.
    pub async fn serve(self) -> Result<(), Box<dyn Error>> {
        let Self {
            serving, _flusher, ..
        } = self;
        serving.await?;
        Ok(())
    }
}

/// Builds the object store and the metadata log the flags name, binds the listeners
/// and attaches the write tier, as the `kbf-server` binary does before it prints its
/// start line.
///
/// `shutdown` is called once, after the stores are open and before anything is bound,
/// and returns the future whose completion stops the server; the binary's is
/// [`interrupted`]. A process that runs the server beside another service passes its
/// own.
///
/// # Errors
/// A flag the server refuses, a store or log that cannot be opened, a listener that
/// cannot be bound, a write tier that refuses to start, or a `shutdown` that fails.
pub async fn start<S, F>(args: &Args, shutdown: S) -> Result<ServerHandle, Box<dyn Error>>
where
    S: FnOnce() -> io::Result<F>,
    F: Future<Output = ()> + Send + 'static,
{
    let stores = args.stores(&Statvfs)?;
    for line in stores.describe() {
        tracing::info!("{line}");
    }
    match &stores.cold {
        StoreUri::File(dir) => {
            let store = FsStore::open(dir, COLD_FILE_MIN_FREE_BYTES)?;
            over(args, &stores, store, FILE_PREFIX_ROOT, shutdown).await
        }
        StoreUri::S3(uri) => {
            let store = s3_store(uri, |name| std::env::var(name).ok())?;
            over(args, &stores, store, &uri.prefix, shutdown).await
        }
    }
}

/// Serves `store` under the Raft metadata log in `stores.meta_dir`, writing under the
/// prefix it committed: `<prefix_root><log id>/` for a new log.
async fn over<O, S, F>(
    args: &Args,
    stores: &Stores,
    store: O,
    prefix_root: &str,
    shutdown: S,
) -> Result<ServerHandle, Box<dyn Error>>
where
    O: ObjectStore + 'static,
    S: FnOnce() -> io::Result<F>,
    F: Future<Output = ()> + Send + 'static,
{
    let log = RaftMetaLog::open_dir(&stores.meta_dir, Retention::default(), prefix_root).await?;
    let (prefix, id) = (log.store_prefix(), log.log_id());
    serve_with(args, stores.hot.as_ref(), log, store, prefix, id, shutdown).await
}

/// [`start`] over a metadata log and object store already open: where the flags'
/// store kinds end and the rest of the assembly (cache, write tier, flusher, listeners)
/// begins. `hot` is the resolved hot store, if any; `prefix` is where objects are
/// written and `log` the Raft log's id, for the start line.
///
/// # Errors
/// As [`start`], but for opening the store and log.
pub async fn serve_with<O, S, F>(
    args: &Args,
    hot: Option<&Hot>,
    meta: RaftMetaLog<StdFs>,
    store: O,
    prefix: KeyPrefix,
    log: FarmLogId,
    shutdown: S,
) -> Result<ServerHandle, Box<dyn Error>>
where
    O: ObjectStore + 'static,
    S: FnOnce() -> io::Result<F>,
    F: Future<Output = ()> + Send + 'static,
{
    let listeners = args.listeners()?;
    let policy = args.reapi_auth_policy()?;
    let routes = Routes {
        invocations: policy.invocations_get.is_some(),
        blobs: policy.invocations_get.is_some() && policy.blobs_get.is_some(),
    };
    let cache = Cache::open(meta, store, prefix)
        .await?
        .with_writes(args.write_config())
        .with_input_trees(args.input_tree_config());
    let (cache, tier) = crate::write_tier::attach(cache, hot).await?;
    let cache = Arc::new(cache);
    let _flusher = tier.map(|_| {
        let flusher = crate::write_tier::run_flusher(Arc::clone(&cache), FLUSH_EVERY);
        AbortOnDrop(tokio::spawn(flusher))
    });
    let shutdown = shutdown()?;
    let api = args.api()?;
    let bound = bind_server_with_policy(cache, listeners, api, policy, shutdown)?;
    let plain = args.plain_text_off_loopback();
    Ok(ServerHandle {
        reapi: bound.reapi,
        worker: bound.worker,
        api: bound.api,
        start_line: start_line(
            bound.reapi,
            bound.worker,
            bound.api,
            routes,
            Some(log),
            &plain,
        ),
        serving: Box::pin(bound.serving),
        _flusher,
    })
}

/// Aborts a task when dropped: the flusher stops with the server.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Which of the operator API's routes that are off unless the policy names them are on.
#[derive(Clone, Copy, Debug)]
struct Routes {
    /// The build pages, `GET /v1/invocations/{id}` and below.
    invocations: bool,
    /// `GET /v1/blobs/{digest}`.
    blobs: bool,
}

/// `on` or `off`.
const fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// The start line: the version, the addresses bound (with the operator API's, whether
/// it serves the build pages and `blobs`) and the Raft log's id, and one warning
/// naming the listeners in `plain` (those serving plain text off loopback) when there
/// are any.
fn start_line(
    reapi: SocketAddr,
    worker: SocketAddr,
    api: Option<SocketAddr>,
    routes: Routes,
    log: Option<FarmLogId>,
    plain: &[&str],
) -> String {
    let (invocations, blobs) = (on_off(routes.invocations), on_off(routes.blobs));
    let api = api
        .map(|a| format!(" invocations={invocations} blobs={blobs} api={a}"))
        .unwrap_or_default();
    let log = log.map(|id| format!(" log={id}")).unwrap_or_default();
    let warning = if plain.is_empty() {
        String::new()
    } else {
        format!(" warning=plaintext-off-loopback:{}", plain.join(","))
    };
    format!("kbf-server {SERVER_VERSION} reapi={reapi} worker={worker}{api}{log}{warning}")
}

/// Installs the SIGINT and SIGTERM handlers now and returns a future that completes on
/// either signal.
///
/// `tokio::signal::ctrl_c()` installs its handler only when first polled, which is
/// after the start line is printed; a SIGINT in between killed the process by the
/// default action instead of stopping it cleanly (issue #86). `signal` installs the
/// handler when it is called.
///
/// # Errors
/// The handler cannot be installed.
#[cfg(unix)]
pub fn interrupted() -> io::Result<impl Future<Output = ()> + Send + 'static> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
    })
}

/// Returns a future that completes on Ctrl-C. Elsewhere than Unix the handler is
/// installed when the future is first polled, after the start line is printed.
#[cfg(not(unix))]
#[expect(clippy::unnecessary_wraps, reason = "the same signature as on Unix")]
pub fn interrupted() -> io::Result<impl Future<Output = ()> + Send + 'static> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    /// Catches the server sizing itself from anything but the CPU count: on the 44-core,
    /// 2-threads-per-core farm node it runs 88 async workers, 512 blocking threads
    /// (tokio's cap) and 88 CPU threads, on a 3-CPU machine 3, 512 and 3; a flag still
    /// wins over its default (alone), and `--reserve-cpus` and `--reserve-share` are
    /// refused, so a stale unit file is noticed.
    #[test]
    fn the_server_uses_every_cpu() {
        let triple = |args: &Args, topology| {
            let sizes = sizes_on(args, topology);
            (
                sizes.async_threads(),
                sizes.blocking_threads(),
                sizes.cpu_threads(),
            )
        };
        let args = Args::parse_from(["kbf-server"]);
        assert_eq!(triple(&args, Topology::new(88, 2)), (88, 512, 88));
        assert_eq!(triple(&args, Topology::new(3, 1)), (3, 512, 3));
        assert_eq!(args.cpu_threads_on(Topology::new(88, 2)).get(), 88);
        let args = Args::parse_from(["kbf-server", "--blocking-threads=9"]);
        assert_eq!(triple(&args, Topology::new(88, 2)), (88, 9, 88));
        let args = Args::parse_from(["kbf-server", "--async-threads=5", "--cpu-threads=6"]);
        assert_eq!(triple(&args, Topology::new(88, 2)), (5, 512, 6));
        assert_eq!(args.cpu_threads_on(Topology::new(88, 2)).get(), 6);
        for flag in ["--reserve-cpus=16", "--reserve-share=2"] {
            assert!(
                Args::try_parse_from(["kbf-server", flag]).is_err(),
                "{flag}"
            );
        }
    }
}
