//! `kbf-server`: serves REAPI and `kbf.worker.v1` until interrupted. See the library
//! docs for what it wires together.
//!
//! On start it prints one line, `kbf-server <version> reapi=<addr> worker=<addr>`, with
//! its version (`<package version>+<commit>`, [`kbf_server::SERVER_VERSION`]) and
//! the addresses it bound (a port of 0 picks a free one), and ` api=<addr>` at its end
//! when the operator API listens, then ` log=<id>` under `--meta-log=raft`, the id of
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

use std::error::Error;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use kbf_front::{Cache, MemoryMetaLog, MetaLog};
use kbf_log::FarmLogId;
use kbf_meta::Retention;
use kbf_objstore::{Capabilities, KeyPrefix, MemoryStore, ObjectStore};
use kbf_server::{Args, RaftMetaLog, SERVER_VERSION, StoreKind, bind_server_with_policy};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    match start(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf-server: {e}");
            ExitCode::from(2)
        }
    }
}

/// Builds the object store and the metadata log the flags name, then serves.
async fn start(args: &Args) -> Result<(), Box<dyn Error>> {
    let raft_dir = args.raft_dir()?;
    let memory = || MemoryMetaLog::new(Retention::default());
    match args.store {
        StoreKind::Memory => {
            let store = MemoryStore::new(Capabilities::default());
            run(args, memory(), store, KeyPrefix::default(), None).await
        }
        StoreKind::S3 => {
            let (store, start_prefix) = args.s3_store(|name| std::env::var(name).ok())?;
            match raft_dir {
                None => run(args, memory(), store, start_prefix, None).await,
                Some(dir) => {
                    let log =
                        RaftMetaLog::open_dir(dir, Retention::default(), &args.s3_prefix).await?;
                    let (prefix, id) = (log.store_prefix(), log.log_id());
                    run(args, log, store, prefix, Some(id)).await
                }
            }
        }
    }
}

async fn run<M: MetaLog, O: ObjectStore + 'static>(
    args: &Args,
    meta: M,
    store: O,
    prefix: KeyPrefix,
    log: Option<FarmLogId>,
) -> Result<(), Box<dyn Error>> {
    let listeners = args.listeners()?;
    let policy = args.reapi_auth_policy()?;
    let cache = Arc::new(Cache::open(meta, store, prefix).await?);
    let shutdown = interrupted()?;
    let api = args.api()?;
    let bound = bind_server_with_policy(cache, listeners, api, policy, shutdown)?;
    let plain = args.plain_text_off_loopback();
    println!(
        "{}",
        start_line(bound.reapi, bound.worker, bound.api, log, &plain)
    );
    bound.serving.await?;
    Ok(())
}

/// The start line: the version, the addresses bound and the Raft log's id, and one
/// warning naming the listeners in `plain` (those serving plain text off loopback)
/// when there are any.
fn start_line(
    reapi: SocketAddr,
    worker: SocketAddr,
    api: Option<SocketAddr>,
    log: Option<FarmLogId>,
    plain: &[&str],
) -> String {
    let api = api.map(|a| format!(" api={a}")).unwrap_or_default();
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
