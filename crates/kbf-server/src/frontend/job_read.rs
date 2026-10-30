//! `--job-listen`: the job-facing read listener of `kbf frontend`, where a task on a worker
//! fetches the inputs of its Action with a read grant ([`kbf_front::job_read`] answers; this
//! module is its flags, its checks at start and its HTTPS).
//!
//! It is off unless `--job-listen` is given, with `--job-tls-cert`, `--job-tls-key` and
//! `--job-grant-secret-file`, and then no other listener changes. The jobs hold no client
//! certificate (they are untrusted code), so the TLS is the server's alone, from the farm CA
//! the task is given (`kbf-task --root-der`), and no client is asked for a certificate.
//! The task dials an IP address, so the certificate must name the addresses it dials as IP
//! subjectAltNames, which the start checks: a certificate with none, or one that does not
//! name a particular `--job-listen` address, refuses to start (a client that dials an IP
//! address accepts only a certificate that names it). The files are read again every
//! `--tls-reload-interval-secs`, as the other listeners' are; the names are checked once.
//!
//! The grant secret is a file (owner-only, as `--api-token-file` is), the same secret the
//! issuing side of the grants reads, so every frontend verifies the grants of every other.
//!
//! The HTTP is one connection one request at a time (HTTP/1.1, keep-alive), and the
//! listener bounds what an untrusted client can hold: [`MAX_CONNECTIONS`] open connections
//! (the others wait in the kernel's backlog), a time to send a request's headers, a
//! lifetime for a connection, and in [`kbf_front::job_read`] the reads open at once
//! (`--job-reads-in-flight`) and the largest blob served (`--job-max-blob-bytes`). A blob over
//! 64 MiB is streamed with its content length, and one whose bytes are not its digest's ends
//! its response short, which the client sees as a broken response. The
//! listener's socket is closed the moment the drain begins; a connection already open
//! finishes the request it is answering, within its lifetime.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{ALLOW, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use bytes::Bytes;
use clap::{Arg, ArgMatches};
use futures::FutureExt as _;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use kbf_front::Packs;
use kbf_front::grants::{GrantKey, Grants, KeyError};
use kbf_front::job_read::{
    BATCH_PATH, BlobSource, JobRead, Limits, MAX_BATCH_REQUEST_BYTES, Reply,
};
use kbf_front::large_backfill::LargeBackfill;
use kbf_tls::{Alpn, Conn, TlsError, TlsFiles, TlsSource};
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore};

use crate::listen::{Binder, Latch, Socket, Stoppable};
use crate::serve::ServeError;
use crate::token::{TokenFileError, read_owner_only};

const LISTEN: &str = "job_listen";
const TLS_CERT: &str = "job_tls_cert";
const TLS_KEY: &str = "job_tls_key";
const GRANT_SECRET: &str = "job_grant_secret_file";
const MAX_BLOB: &str = "job_max_blob_bytes";
const READS: &str = "job_reads_in_flight";
const BUDGET: &str = "job_read_budget_bytes";
const IDLE: &str = "job_read_idle_secs";

/// The default of `--job-max-blob-bytes`, as clap takes a default (a string); a test parses
/// it back and compares it with [`Limits::default`].
const DEFAULT_MAX_BLOB: &str = "536870912";

/// The default of `--job-reads-in-flight`, likewise.
const DEFAULT_READS: &str = "32";

/// The default of `--job-read-budget-bytes`, likewise (1 GiB).
const DEFAULT_BUDGET: &str = "1073741824";

/// The default of `--job-read-idle-secs`, likewise.
const DEFAULT_IDLE: &str = "60";

/// The most bytes the grant secret's file may hold, surrounding whitespace included.
const MAX_SECRET_FILE_BYTES: usize = 4096;

/// The most connections open at once.
pub(super) const MAX_CONNECTIONS: usize = 256;

/// The label of the listener's TLS files in `kbf_tls_reloads_total`.
const TLS_LABEL: &str = "job";

/// The job-facing read's flags: the first four, or none; the last four have defaults.
#[derive(Clone, Debug)]
pub struct JobReadFlags {
    /// `--job-listen`: where the listener listens.
    pub listen: SocketAddr,
    /// `--job-tls-cert`: its certificate chain (PEM), which names the addresses jobs dial.
    pub tls_cert: PathBuf,
    /// `--job-tls-key`: the key of the certificate (PEM).
    pub tls_key: PathBuf,
    /// `--job-grant-secret-file`: the farm secret the read grants are signed with.
    pub grant_secret_file: PathBuf,
    /// `--job-max-blob-bytes`: the largest blob a response carries.
    pub max_blob_bytes: u64,
    /// `--job-reads-in-flight`: the blob reads open at once.
    pub reads_in_flight: u32,
    /// `--job-read-budget-bytes`: the bytes of blob reads open at once.
    pub read_budget_bytes: u64,
    /// `--job-read-idle-secs`: how long a held response may go with no chunk taken.
    pub read_idle_secs: u64,
}

/// The eight flags.
pub(super) fn args() -> [Arg; 8] {
    [
        Arg::new(LISTEN)
            .long("job-listen")
            .value_name("ADDR")
            .value_parser(clap::value_parser!(SocketAddr))
            .requires_all([TLS_CERT, TLS_KEY, GRANT_SECRET])
            .help(
                "Serves the inputs of an action to the task that runs it, over HTTPS at this \
                 address: `GET /<hex>/<size>` and `POST /batch` (many small blobs in one \
                 request) with `Authorization: Bearer <read grant>`, and nothing else (any \
                 other path is 404 and any other method 405). A read grant \
                 is the token a run is given; until it expires it reads the Action it was \
                 issued for, its Command and its input tree, and no other blob. The TLS is the \
                 server's alone and no client certificate is asked for. The task dials an IP \
                 address, so give the address the jobs dial, and a `--job-tls-cert` that names \
                 it as an IP subjectAltName. Needs `--job-tls-cert`, `--job-tls-key` and \
                 `--job-grant-secret-file`. Left out, no such listener is opened",
            ),
        Arg::new(TLS_CERT)
            .long("job-tls-cert")
            .value_name("PATH")
            .value_parser(clap::value_parser!(PathBuf))
            .requires(LISTEN)
            .help(
                "The certificate chain (PEM) of `--job-listen`, issued by the farm CA that the \
                 jobs trust. It must carry an IP subjectAltName for each address a job dials \
                 the frontend at: the start is refused if it has none, or if `--job-listen` \
                 is on one address and the certificate does not name it, since a client that \
                 dials an IP address accepts only a certificate that names it. Read again \
                 every `--tls-reload-interval-secs`, as the other listeners' are; the names \
                 are checked at start only",
            ),
        Arg::new(TLS_KEY)
            .long("job-tls-key")
            .value_name("PATH")
            .value_parser(clap::value_parser!(PathBuf))
            .requires(LISTEN)
            .help("The private key (PEM) of `--job-tls-cert`"),
        Arg::new(GRANT_SECRET)
            .long("job-grant-secret-file")
            .value_name("PATH")
            .value_parser(clap::value_parser!(PathBuf))
            .requires(LISTEN)
            .help(
                "A file with the farm secret that read grants are signed with, at least 32 \
                 bytes (surrounding whitespace is dropped), readable by its owner alone (mode \
                 0600 or 0400). The frontends of a farm and whatever issues the grants must \
                 read the same secret. The secret is never a flag: a command line is \
                 world-readable",
            ),
        Arg::new(MAX_BLOB)
            .long("job-max-blob-bytes")
            .value_name("BYTES")
            .value_parser(clap::value_parser!(u64).range(1..))
            .default_value(DEFAULT_MAX_BLOB)
            .requires(LISTEN)
            .help(
                "The most bytes of one blob the listener serves, at least 1: a blob of more is \
                 refused 413 `too_large` before anything is read. A blob of more than 67108864 \
                 bytes is streamed from the store through SHA-256 and never held whole; one \
                 whose bytes are not its digest's ends its response early, and is counted. \
                 Needs `--job-listen`",
            ),
        Arg::new(READS)
            .long("job-reads-in-flight")
            .value_name("N")
            .value_parser(clap::value_parser!(u32).range(1..))
            .default_value(DEFAULT_READS)
            .requires(LISTEN)
            .help(
                "The blob reads this listener has open at once, at least 1: a streamed blob \
                 holds its place until its response ends, and a request over the bound waits \
                 for one to end. Needs `--job-listen`",
            ),
        Arg::new(BUDGET)
            .long("job-read-budget-bytes")
            .value_name("BYTES")
            .value_parser(clap::value_parser!(u64).range(1..))
            .default_value(DEFAULT_BUDGET)
            .requires(LISTEN)
            .help(
                "The bytes of blob reads this listener has open at once, at least 1: a read \
                 takes its blob's size (a streamed blob a bounded window of it, and the scope \
                 walk's reads their own sizes) and a request over the budget waits for room, it \
                 never fails; a blob larger than the whole budget is read alone. Needs \
                 `--job-listen`",
            ),
        Arg::new(IDLE)
            .long("job-read-idle-secs")
            .value_name("SECONDS")
            .value_parser(clap::value_parser!(u64).range(1..))
            .default_value(DEFAULT_IDLE)
            .requires(LISTEN)
            .help(
                "How long, in seconds, a response that holds read budget may go with no chunk taken by its \
                 client before it is dropped, its budget given back and its response ended, at \
                 least 1: a client that does not read does not hold the budget for ever, and one \
                 that reads is not cut off however long its response takes. Needs \
                 `--job-listen`",
            ),
    ]
}

/// The flags `m` makes, or `None` without `--job-listen` (clap makes the other three come
/// with it).
pub(super) fn from_matches(m: &ArgMatches) -> Option<JobReadFlags> {
    let listen = *m.get_one::<SocketAddr>(LISTEN)?;
    let path = |id| {
        m.get_one::<PathBuf>(id)
            .expect("clap requires it with --job-listen")
            .clone()
    };
    Some(JobReadFlags {
        listen,
        tls_cert: path(TLS_CERT),
        tls_key: path(TLS_KEY),
        grant_secret_file: path(GRANT_SECRET),
        max_blob_bytes: *m.get_one::<u64>(MAX_BLOB).expect("clap gives it a default"),
        reads_in_flight: *m.get_one::<u32>(READS).expect("clap gives it a default"),
        read_budget_bytes: *m.get_one::<u64>(BUDGET).expect("clap gives it a default"),
        read_idle_secs: *m.get_one::<u64>(IDLE).expect("clap gives it a default"),
    })
}

/// Why the job-facing read cannot start.
#[derive(Debug, thiserror::Error)]
pub enum JobReadError {
    /// The grant secret's file cannot be used.
    #[error("--job-grant-secret-file: {0}")]
    Secret(#[from] TokenFileError),
    /// The grant secret is too short.
    #[error("--job-grant-secret-file: {0}")]
    Key(#[from] KeyError),
    /// The certificate file cannot be read.
    #[error("--job-tls-cert {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// Why it cannot be read.
        #[source]
        source: std::io::Error,
    },
    /// The certificate is not PEM, or does not name the address jobs dial.
    #[error("--job-tls-cert {path}: {why}")]
    Certificate {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        why: String,
    },
    /// The certificate and key do not make a TLS configuration.
    #[error("--job-tls-cert and --job-tls-key: {0}")]
    Tls(#[from] TlsError),
    /// The listen address cannot be bound.
    #[error(transparent)]
    Bind(#[from] ServeError),
}

/// The job-facing read at a frontend's start, up to the backend it reads: the secret read,
/// the certificate checked, the socket bound.
pub(super) struct Prepared {
    socket: Socket,
    tls: TlsSource,
    grants: Arc<Grants>,
    /// What the flags bound: the reads open at once and the largest blob served.
    limits: Limits,
    /// The copy of large blobs to tierstore, when there are packs to take it from.
    backfill: Option<LargeBackfill>,
}

/// The waits that bound a client of the listener.
#[derive(Clone, Copy, Debug)]
pub(super) struct Timeouts {
    /// How long a client has to send the headers of a request, an idle one included.
    pub header: Duration,
    /// The longest a connection lives.
    pub connection: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            header: Duration::from_secs(10),
            connection: Duration::from_secs(600),
        }
    }
}

/// A started listener.
pub(super) struct Serving {
    /// Closes the listener's socket.
    pub latch: Latch,
    /// The accept loop, ending when the latch closes.
    pub task: BoxFuture<'static, ()>,
    /// The listener's TLS configuration, for the reload counters.
    pub tls: TlsSource,
    /// What answers, for the counters.
    pub read: Arc<JobRead>,
}

impl Prepared {
    /// Reads the secret, checks the certificate and the key, and binds the address.
    ///
    /// # Errors
    /// The secret, the certificate or the key is refused (see [`JobReadError`]), or the
    /// address cannot be bound.
    pub(super) fn new(
        flags: &JobReadFlags,
        reload: Duration,
        binder: &Binder,
    ) -> Result<Self, JobReadError> {
        let grants = Arc::new(Grants::system(grant_key(&flags.grant_secret_file)?));
        let pem = std::fs::read(&flags.tls_cert).map_err(|source| JobReadError::Read {
            path: flags.tls_cert.clone(),
            source,
        })?;
        names_the_address(&pem, flags.listen).map_err(|why| JobReadError::Certificate {
            path: flags.tls_cert.clone(),
            why,
        })?;
        let files = TlsFiles {
            cert: flags.tls_cert.clone(),
            key: flags.tls_key.clone(),
            client_ca: None,
        };
        let tls = TlsSource::files_alpn(TLS_LABEL, files, reload, Alpn::Http1)?;
        let socket = Socket::bind(flags.listen, binder)?;
        let limits = Limits {
            reads_in_flight: usize::try_from(flags.reads_in_flight).unwrap_or(usize::MAX),
            max_blob_bytes: flags.max_blob_bytes,
            read_budget_bytes: flags.read_budget_bytes,
            held_idle: Duration::from_secs(flags.read_idle_secs),
            ..Limits::default()
        };
        Ok(Self {
            socket,
            tls,
            grants,
            limits,
            backfill: None,
        })
    }

    /// This listener, copying the large blobs it streams to the packs' store through `packs`
    /// ([`kbf_front::large_backfill`]); `None` copies nothing.
    pub(super) fn with_backfill(mut self, packs: Option<Packs>) -> Self {
        self.backfill = packs.map(|packs| {
            LargeBackfill::new(
                Arc::new(packs),
                kbf_front::large_backfill::DEFAULT_CONCURRENCY,
            )
        });
        self
    }

    /// Where the listener listens.
    pub(super) fn local(&self) -> SocketAddr {
        self.socket.local
    }

    /// Serves the blobs of `source` to the grants this read verifies.
    ///
    /// # Errors
    /// The runtime cannot register the socket.
    pub(super) fn start(
        self,
        source: Arc<dyn BlobSource>,
        timeouts: Timeouts,
    ) -> Result<Serving, JobReadError> {
        let read =
            Arc::new(JobRead::new(source, self.grants, self.limits).with_backfill(self.backfill));
        let (incoming, latch) = self.socket.grpc(Some(self.tls.clone()))?;
        let task = accept(
            incoming,
            router(Arc::clone(&read), timeouts.header),
            timeouts,
        )
        .boxed();
        Ok(Serving {
            latch,
            task,
            tls: self.tls,
            read,
        })
    }
}

/// The grant key the secret in the file `path` makes.
fn grant_key(path: &Path) -> Result<GrantKey, JobReadError> {
    let content = read_owner_only(path, MAX_SECRET_FILE_BYTES)?;
    if content.len() > MAX_SECRET_FILE_BYTES {
        return Err(TokenFileError::Content {
            path: path.to_owned(),
        }
        .into());
    }
    Ok(GrantKey::from_secret(content.trim_ascii())?)
}

/// Whether the leaf of the PEM chain `pem` names `listen`: as an IP subjectAltName when the
/// address is a particular one, and some IP subjectAltName when it is the unspecified one.
fn names_the_address(pem: &[u8], listen: SocketAddr) -> Result<(), String> {
    let bundle = kbf_ca::Bundle::from_pem(pem).map_err(|_| "no PEM certificate".to_owned())?;
    let names = kbf_peer::names_of(&bundle.certs()[0]).unwrap_or_default();
    let ip = listen.ip();
    let named = if ip.is_unspecified() {
        !names.ips().is_empty()
    } else {
        names.covers(&ip.to_string())
    };
    if named {
        return Ok(());
    }
    Err(format!(
        "the certificate has no IP subjectAltName for {ip} (it names {names}); a job dials \
         the frontend by IP address and accepts only a certificate that names it"
    ))
}

/// What a request is answered with: the read, and how long a request's body is waited for.
#[derive(Clone)]
struct Served {
    read: Arc<JobRead>,
    body_wait: Duration,
}

fn router(read: Arc<JobRead>, body_wait: Duration) -> Router {
    Router::new()
        .fallback(answer)
        .with_state(Served { read, body_wait })
}

async fn answer(
    State(served): State<Served>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let authorization = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok());
    let path = uri.path();
    // Only a batch has a body to read, and it is read within a size and a time: a body that is
    // longer, or does not arrive, is none, which the read answers as a malformed request (after
    // the token, which is checked first).
    let bytes = if method == Method::POST && path == BATCH_PATH {
        batch_body(body, served.body_wait).await
    } else {
        Bytes::new()
    };
    let reply = served
        .read
        .respond_with_body(&method, path, authorization, &bytes)
        .await;
    response(reply, if path == BATCH_PATH { "POST" } else { "GET" })
}

/// The body of a batch request: at most [`MAX_BATCH_REQUEST_BYTES`], within `wait`; empty if it
/// is longer, or is not all there in time.
async fn batch_body(body: Body, wait: Duration) -> Bytes {
    let read = axum::body::to_bytes(body, MAX_BATCH_REQUEST_BYTES);
    tokio::time::timeout(wait, read)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

/// The HTTP response of `reply`: its status and body, the headers a client needs to read
/// a refusal (`WWW-Authenticate` on 401, `Allow: <allow>` on 405) and a content type.
fn response(mut reply: Reply, allow: &'static str) -> Response {
    let status = reply.status;
    // A streamed blob states its length before its first byte: a response that ends short of
    // it is a broken one to the client, which is how a blob that fails its digest is refused.
    // A body whose bytes are charged to the read budget is sent in pieces that keep the charge
    // until the connection has taken the last of them.
    let streamed = reply.stream.take().or_else(|| reply.held_stream());
    let (body, length) = match streamed {
        Some(stream) => (Body::from_stream(stream.chunks), Some(stream.size)),
        None => (Body::from(reply.body), None),
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    if let Some(length) = length {
        headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
    }
    let kind = if status == StatusCode::OK {
        "application/octet-stream"
    } else {
        "text/plain; charset=utf-8"
    };
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(kind));
    if status == StatusCode::UNAUTHORIZED {
        headers.insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    if status == StatusCode::METHOD_NOT_ALLOWED {
        headers.insert(ALLOW, HeaderValue::from_static(allow));
    }
    response
}

/// Serves every connection `incoming` yields until it ends (its latch closed), at most
/// [`MAX_CONNECTIONS`] at once: a permit is taken before the next accept, so a flood waits
/// in the kernel's backlog and not in tasks.
async fn accept(mut incoming: Stoppable, router: Router, timeouts: Timeouts) {
    let open = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let permit = Arc::clone(&open).acquire_owned().await;
        match incoming.next().await {
            Some(Ok(conn)) => {
                tokio::spawn(connection(conn, router.clone(), timeouts, permit));
            }
            Some(Err(error)) => tracing::warn!(%error, "a job-facing connection was not accepted"),
            None => return,
        }
    }
}

/// One connection, until its client leaves, it breaks, or its lifetime ends; holds its
/// permit throughout.
async fn connection(
    conn: Conn,
    router: Router,
    timeouts: Timeouts,
    _permit: Result<OwnedSemaphorePermit, AcquireError>,
) {
    let served = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(timeouts.header)
        .serve_connection(TokioIo::new(conn), TowerToHyperService::new(router));
    let _ = tokio::time::timeout(timeouts.connection, served).await;
}

#[cfg(test)]
mod tests;
