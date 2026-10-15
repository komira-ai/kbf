//! `kbf-server`: serves REAPI and `kbf.worker.v1` until interrupted. See the library
//! docs for what it wires together.
//!
//! On start it prints one line, `kbf-server <version> reapi=<addr> worker=<addr>`, with
//! its version (`<package version>+<commit>`, [`kbf_server::SERVER_VERSION`]) and
//! the addresses it bound (a port of 0 picks a free one), and ` invocations=<on|off>
//! blobs=<on|off> api=<addr>` at its end when the operator API listens
//! (`invocations=on` when the REAPI policy names `invocations.getAuthorizer` or
//! `--invocations-open` is given, so the build pages answer; `blobs=on` when it also
//! names `blobs.getAuthorizer`, or the flag is given, so `GET /v1/blobs` answers), then
//! ` log=<id>`, the id of
//! the metadata log (16 hex digits), then
//! ` warning=plaintext-off-loopback:<listeners>` at its very end when a listener serves
//! plain text on an address that is not loopback (`<listeners>` is `reapi`, `worker`
//! or `reapi,worker`; see [`Args::plain_text_off_loopback`]). The server starts either
//! way. On Unix the SIGINT and SIGTERM handlers are installed before that line is
//! printed, so either signal any time after it stops the server with exit 0: open
//! Execute and WaitExecution streams end UNAVAILABLE, REAPI connections are sent
//! GOAWAY, and the server exits once they close or `--shutdown-timeout-secs` runs out
//! (issue #168). If a handler cannot be installed it exits 2 without printing the
//! start line.
//!
//! The REAPI listener runs the policy `--reapi-auth-policy` names; a policy file that
//! cannot be read or is not a policy exits 2 before anything is bound.
//!
//! The metadata log (a single-voter Raft log) opens in `--meta-dir`, or in `meta/` of
//! the `--hot-store` directory, before anything is bound (creating it if the directory
//! is empty), and objects are written under the prefix the log committed; a directory
//! it refuses exits 2. `--cold-store` is the durable object store, `file:///dir` (a
//! `kbf_objstore::FsStore`, locked while the server runs, so a second server on it, or
//! a directory that cannot be created or locked, exits 2 before anything is bound) or
//! `s3://bucket/prefix?endpoint=...`; the start logs every store, path and budget it
//! resolved.
//!
//! `--hot-store` attaches the local write tier before anything is bound
//! (`kbf_server::write_tier`): it first raises the log to machine version 2 (once; a
//! server without a tier never does, so its log stays one an older binary reads). A
//! start the tier refuses (a wrong or missing `TIER` while objects are staged, a log
//! that cannot be raised) exits 2. Its flusher runs every second until the server stops.

use std::error::Error;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use kbf_front::Cache;
use kbf_log::FarmLogId;
use kbf_meta::Retention;
use kbf_objstore::{FsStore, KeyPrefix, ObjectStore};
use kbf_server::stores::{COLD_FILE_MIN_FREE_BYTES, FILE_PREFIX_ROOT, StoreUri, Stores};
use kbf_server::write_tier::{FLUSH_EVERY, TierLog};
use kbf_server::{Args, RaftMetaLog, SERVER_VERSION, bind_server_with_policy, s3_store};
use kbf_threads::Topology;
use kbf_threads::disk::Statvfs;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    match serve(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf-server: {e}");
            ExitCode::from(2)
        }
    }
}

/// Builds the async runtime the thread flags size, then runs [`start`] on it.
fn serve(args: &Args) -> Result<(), Box<dyn Error>> {
    let sizes = sizes_on(args, Topology::detect());
    sizes.log();
    sizes.runtime()?.block_on(start(args))
}

/// The server's thread sizes on a given machine shape: every CPU, then the overrides.
fn sizes_on(args: &Args, topology: Topology) -> kbf_threads::Sizes {
    args.threads.resolve(topology)
}

/// Builds the object store and the metadata log the flags name, then serves.
async fn start(args: &Args) -> Result<(), Box<dyn Error>> {
    let stores = args.stores(&Statvfs)?;
    for line in stores.describe() {
        tracing::info!("{line}");
    }
    match &stores.cold {
        StoreUri::File(dir) => {
            let store = FsStore::open(dir, COLD_FILE_MIN_FREE_BYTES)?;
            over(args, &stores, store, FILE_PREFIX_ROOT).await
        }
        StoreUri::S3(uri) => {
            let store = s3_store(uri, |name| std::env::var(name).ok())?;
            over(args, &stores, store, &uri.prefix).await
        }
    }
}

/// Serves `store` under the Raft metadata log in `stores.meta_dir`, writing under the
/// prefix it committed: `<prefix_root><log id>/` for a new log.
async fn over<O: ObjectStore + 'static>(
    args: &Args,
    stores: &Stores,
    store: O,
    prefix_root: &str,
) -> Result<(), Box<dyn Error>> {
    let log = RaftMetaLog::open_dir(&stores.meta_dir, Retention::default(), prefix_root).await?;
    let (prefix, id) = (log.store_prefix(), log.log_id());
    run(args, stores, log, store, prefix, id).await
}

async fn run<M: TierLog, O: ObjectStore + 'static>(
    args: &Args,
    stores: &Stores,
    meta: M,
    store: O,
    prefix: KeyPrefix,
    log: FarmLogId,
) -> Result<(), Box<dyn Error>> {
    let listeners = args.listeners()?;
    let policy = args.reapi_auth_policy()?;
    let routes = Routes {
        invocations: policy.invocations_get.is_some(),
        blobs: policy.invocations_get.is_some() && policy.blobs_get.is_some(),
    };
    let cache = Cache::open(meta, store, prefix)
        .await?
        .with_writes(args.write_config());
    let (cache, tier) = kbf_server::write_tier::attach(cache, stores.hot.as_ref()).await?;
    let cache = Arc::new(cache);
    let _flusher = tier.map(|_| {
        let flusher = kbf_server::write_tier::run_flusher(Arc::clone(&cache), FLUSH_EVERY);
        AbortOnDrop(tokio::spawn(flusher))
    });
    let shutdown = interrupted()?;
    let api = args.api()?;
    let bound = bind_server_with_policy(cache, listeners, api, policy, shutdown)?;
    let plain = args.plain_text_off_loopback();
    println!(
        "{}",
        start_line(
            bound.reapi,
            bound.worker,
            bound.api,
            routes,
            Some(log),
            &plain
        )
    );
    bound.serving.await?;
    Ok(())
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
fn interrupted() -> io::Result<impl Future<Output = ()> + Send + 'static> {
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
fn interrupted() -> io::Result<impl Future<Output = ()> + Send + 'static> {
    Ok(async {
        let _ = tokio::signal::ctrl_c().await;
    })
}

#[cfg(test)]
mod tests {
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
