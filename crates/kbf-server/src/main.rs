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
//! ` log=<id>` under `--meta-log=raft`, the id of
//! the log in `--raft-dir` (16 hex digits), then
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
//! `--meta-log=raft` opens the log in `--raft-dir` (creating it if the directory is
//! empty) before anything is bound, and writes objects under the prefix the log
//! committed; a directory it refuses exits 2.
//!
//! `--blob-dir` keeps blob bytes in a local directory (`kbf_objstore::FsStore`) in place
//! of `--store`, under the fixed key prefix `kbf/<log id>/`; it needs `--meta-log=raft`. A directory that cannot be created or
//! locked (another server has it open) exits 2 before anything is bound.
//!
//! `--write-tier-dir` attaches the local write tier before anything is bound
//! (`kbf_server::write_tier`), over either metadata log: under `--meta-log=raft` it
//! first raises the log to machine version 2 (once; a server without a tier never
//! does, so its log stays one an older binary reads). A start the tier refuses (a
//! wrong or missing `TIER` while objects are staged, a log that cannot be raised)
//! exits 2. Its flusher runs every second until the server stops.

use std::error::Error;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use kbf_front::{Cache, MemoryMetaLog};
use kbf_log::FarmLogId;
use kbf_meta::Retention;
use kbf_objstore::{Capabilities, FsStore, KeyPrefix, MemoryStore, ObjectStore};
use kbf_server::write_tier::{FLUSH_EVERY, TierLog};
use kbf_server::{Args, RaftMetaLog, SERVER_VERSION, StoreKind, bind_server_with_policy};
use kbf_threads::Topology;

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
    let raft_dir = args.raft_dir()?;
    match (args.blob_dir.as_deref(), args.store) {
        (Some(dir), _) => {
            let store = FsStore::open(dir, args.blob_dir_min_free_bytes)?;
            over(args, raft_dir, store, KeyPrefix::default(), BLOB_DIR_PREFIX).await
        }
        (None, StoreKind::Memory) => {
            let store = MemoryStore::new(Capabilities::default());
            let memory = MemoryMetaLog::new(Retention::default());
            run(args, memory, store, KeyPrefix::default(), None).await
        }
        (None, StoreKind::S3) => {
            let (store, start_prefix) = args.s3_store(|name| std::env::var(name).ok())?;
            over(args, raft_dir, store, start_prefix, &args.s3_prefix).await
        }
    }
}

/// The key prefix root under `--blob-dir`: objects go under `kbf/<log id>/`.
const BLOB_DIR_PREFIX: &str = "kbf/";

/// Serves `store` under the metadata log `raft_dir` names: the Raft log there, writing
/// under the prefix it committed (`<prefix_root><log id>/` for a new log), or without
/// one a log in memory, writing under `start_prefix`. (`Args::raft_dir` refuses
/// `--blob-dir` without a Raft log.)
async fn over<O: ObjectStore + 'static>(
    args: &Args,
    raft_dir: Option<&Path>,
    store: O,
    start_prefix: KeyPrefix,
    prefix_root: &str,
) -> Result<(), Box<dyn Error>> {
    match raft_dir {
        None => {
            let memory = MemoryMetaLog::new(Retention::default());
            run(args, memory, store, start_prefix, None).await
        }
        Some(dir) => {
            let log = RaftMetaLog::open_dir(dir, Retention::default(), prefix_root).await?;
            let (prefix, id) = (log.store_prefix(), log.log_id());
            run(args, log, store, prefix, Some(id)).await
        }
    }
}

async fn run<M: TierLog, O: ObjectStore + 'static>(
    args: &Args,
    meta: M,
    store: O,
    prefix: KeyPrefix,
    log: Option<FarmLogId>,
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
    let (cache, tier) = kbf_server::write_tier::attach(cache, args).await?;
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
        start_line(bound.reapi, bound.worker, bound.api, routes, log, &plain)
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
