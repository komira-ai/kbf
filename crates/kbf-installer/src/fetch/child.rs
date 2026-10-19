//! `kbf-installer fetch-child`: the program the root parent re-executes as the fetch user to
//! pull one component and stream it to standard output.
//!
//! It holds no authority: the parent started it with an empty environment, no input, no file
//! it may write, no supplementary groups and the limits of [`crate::run::Confine`], and it
//! believes nothing it reads: the parent counts and hashes the bytes, and the attestation
//! decides. The arguments are fixed flags the parent validated and this program checks again
//! (a program that only trusts its caller's checks is one mistake from a request for another
//! URL).
//!
//! - `--kind url`: `GET <url>/files/<sha256>` over https. It follows **no redirect**: any
//!   answer but 200 is a failed fetch.
//! - `--kind frontend`: `ReleaseService.Fetch` on the frontend's worker listener, over mutual
//!   TLS: the server is authenticated by the pinned CA and the URL's host is checked against
//!   its certificate's SAN; the client certificate and key arrive on the lent descriptor
//!   ([`super::identity`]) and exist nowhere else in this process.
//!
//! It writes at most `--limit` bytes. The parent passes the declared size plus one, so a source
//! that sends more than it declared is seen to (and the parent kills the child).
//!
//! With `--describe <commit>` in place of `--sha256` it asks the same source what the commit
//! holds (`GET <url>/releases/<commit>.json`, or `ReleaseService.Describe`) and writes the
//! answer in the small form of [`super::describe`]. The decoding of the source's JSON or
//! protobuf is done here, in the confined child, so that the parent reads the form and not the
//! source's bytes. A source that answers that it has no such release, or not a complete one,
//! is not a failure of the child: it is the answer, written as a refusal. Any other failure is
//! one line on standard error, as for a fetch.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, ValueEnum};
use kbf_types::ReleasePlatform;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;

use super::describe::{self, Answer};
use super::identity::{self, Identity, IdentityFault};
use super::{CONNECT_TIMEOUT, MAX_COMPONENT_CEILING, is_digest};
use crate::CommitId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Kind {
    Url,
    Frontend,
}

/// The fixed flags of `fetch-child`.
#[derive(Args, Debug)]
pub struct ChildArgs {
    #[arg(long, value_enum)]
    pub kind: Kind,
    /// `url`: the https base URL of the file server. `frontend`: `https://host[:port]`.
    #[arg(long)]
    pub url: String,
    /// The lowercase hex sha256 that names the blob. Not with `--describe`.
    #[arg(
        long,
        required_unless_present = "describe",
        conflicts_with = "describe"
    )]
    pub sha256: Option<String>,
    /// Say what the release of this commit (40 lowercase hex digits) holds, instead of
    /// fetching a blob.
    #[arg(long)]
    pub describe: Option<String>,
    /// `frontend` with `--describe`: the one platform to ask about (a `url` source's file
    /// lists every platform).
    #[arg(long, requires = "describe")]
    pub platform: Option<String>,
    /// The most bytes written to standard output (a description: at most
    /// [`describe::MAX_BYTES`] plus one).
    #[arg(long)]
    pub limit: u64,
    /// `url` only: a PEM file of the roots to trust instead of the system's.
    #[arg(long)]
    pub ca_file: Option<PathBuf>,
    /// `frontend` only: the descriptor the identity was lent on (moved to 4).
    #[arg(long)]
    pub identity_fd: Option<i32>,
}

/// Why the child fetched nothing (one line of this is all it says).
#[derive(Debug, thiserror::Error)]
pub enum ChildFault {
    #[error("{0:?} is not a plain https URL")]
    Url(String),
    #[error("{0:?} is not a sha256 (64 lowercase hex digits)")]
    Digest(String),
    #[error("{0:?} is not a commit (40 lowercase hex digits)")]
    Commit(String),
    #[error("{0:?} is not a platform kbf is released for")]
    Platform(String),
    #[error("the limit {limit} is not 1 to {max}")]
    Limit { limit: u64, max: u64 },
    #[error("flags do not fit the kind: {0}")]
    Flags(&'static str),
    #[error("{0}")]
    Identity(#[from] IdentityFault),
    #[error("cannot read the roots in {path}: {why}")]
    Roots { path: PathBuf, why: String },
    #[error("source unreachable: {0}")]
    Unreachable(String),
    #[error("the source answered {0}")]
    Status(String),
    #[error("cannot write the bytes: {0}")]
    Out(io::Error),
}

/// Make this process unreadable to every other process of its user: not dumpable, so
/// `/proc/<pid>/fd`, `environ` and `mem` are root's alone and `ptrace` is refused. The first
/// thing `fetch-child` does (`main`), because the `exec` that started it made it dumpable again
/// (a `prctl` before the `exec` does not last), and a process this user's earlier child left
/// behind could otherwise open the lent identity through `/proc`. Linux only: macOS has no
/// such setting, and its confinement of the child is weaker for it (not built, not run).
///
/// Panics (so the child ends, holding nothing) if the call fails; it fails only for an argument
/// other than 0 and 1.
#[cfg(target_os = "linux")]
pub fn forbid_inspection() {
    // SAFETY: prctl with these arguments reads no memory.
    crate::run::check(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) })
        .expect("PR_SET_DUMPABLE accepts 0");
}

/// There is no `PR_SET_DUMPABLE` here.
#[cfg(not(target_os = "linux"))]
pub fn forbid_inspection() {}

/// The base URL as settings write it: `https://host[:port][/path]`, nothing else.
fn plain_url(url: &str, with_path: bool) -> Result<String, ChildFault> {
    let bad = || ChildFault::Url(url.to_owned());
    let clean = crate::installer_toml::https_base(url).map_err(|_| bad())?;
    if clean != url || (!with_path && url[8..].contains('/')) {
        return Err(bad());
    }
    Ok(clean)
}

fn certificates(pem: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let found: Vec<Vec<u8>> = CertificateDer::pem_slice_iter(pem)
        .map(|c| c.map(|c| c.as_ref().to_vec()).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    if found.is_empty() {
        return Err("no certificate".to_owned());
    }
    Ok(found)
}

fn url_roots(
    ca_file: Option<&PathBuf>,
) -> Result<Vec<ureq::tls::Certificate<'static>>, ChildFault> {
    let ders: Vec<Vec<u8>> = match ca_file {
        Some(path) => {
            let roots = |why: String| ChildFault::Roots {
                path: path.clone(),
                why,
            };
            let text = std::fs::read(path).map_err(|e| roots(e.to_string()))?;
            certificates(&text).map_err(roots)?
        }
        None => rustls_native_certs::load_native_certs()
            .certs
            .into_iter()
            .map(|c| c.as_ref().to_vec())
            .collect(),
    };
    Ok(ders
        .iter()
        .map(|der| ureq::tls::Certificate::from_der(der).to_owned())
        .collect())
}

/// Copy at most `limit` bytes of `body` to `out`; the count.
fn copy_limited(body: impl io::Read, limit: u64, out: &mut dyn Write) -> Result<u64, ChildFault> {
    let mut body = body.take(limit);
    io::copy(&mut body, out).map_err(|e| {
        if e.kind() == io::ErrorKind::BrokenPipe {
            ChildFault::Out(e)
        } else {
            ChildFault::Unreachable(e.to_string())
        }
    })
}

/// The https client of a `url` source and the base URL it is to ask: no redirect followed, no
/// proxy, only the roots it was given.
fn url_agent(args: &ChildArgs) -> Result<(ureq::Agent, String), ChildFault> {
    let base = plain_url(&args.url, true)?;
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::Specific(Arc::new(url_roots(
            args.ca_file.as_ref(),
        )?)))
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        // No proxy, whatever the environment says.
        .proxy(None)
        // No redirect, ever: a 3xx is a failed fetch.
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .tls_config(tls)
        .build()
        .into();
    Ok((agent, base))
}

/// `GET <base>/<path>` of a `url` source; the response, whatever its status.
fn url_get(
    args: &ChildArgs,
    path: &str,
) -> Result<(ureq::http::Response<ureq::Body>, String), ChildFault> {
    let (agent, base) = url_agent(args)?;
    let url = format!("{base}/{path}");
    let response = agent
        .get(&url)
        .call()
        .map_err(|e| ChildFault::Unreachable(e.to_string()))?;
    Ok((response, url))
}

fn fetch_url(args: &ChildArgs, sha256: &str, out: &mut dyn Write) -> Result<u64, ChildFault> {
    let (mut response, _) = url_get(args, &format!("files/{sha256}"))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(ChildFault::Status(status.to_string()));
    }
    copy_limited(response.body_mut().as_reader(), args.limit, out)
}

/// Write `answer` in the form of [`describe`], at most `limit` bytes of it (a longer one is cut
/// at the limit, which the parent, who allows one byte fewer, sees and refuses).
fn write_answer(answer: &Answer, limit: u64, out: &mut dyn Write) -> Result<u64, ChildFault> {
    let text = answer.render();
    let kept = &text.as_bytes()[..text.len().min(usize::try_from(limit).unwrap_or(usize::MAX))];
    out.write_all(kept).map_err(ChildFault::Out)?;
    Ok(kept.len() as u64)
}

/// What the `url` source's file `releases/<commit>.json` says. A `404` or a `409` is the
/// answer that there is no description to be had (the route of `docs/api.md` answers those for
/// a commit it holds nothing of, or nothing complete of); any other status is a failure.
fn describe_url(args: &ChildArgs, commit: &str, out: &mut dyn Write) -> Result<u64, ChildFault> {
    let (mut response, url) = url_get(args, &format!("releases/{commit}.json"))?;
    let status = response.status().as_u16();
    let answer = match status {
        200 => {
            let mut file = Vec::new();
            response
                .body_mut()
                .as_reader()
                .take(args.limit)
                .read_to_end(&mut file)
                .map_err(|e| ChildFault::Unreachable(e.to_string()))?;
            Answer::from_json(&file)
        }
        404 | 409 => Answer::Refused(format!("{url} answered {status}")),
        _ => return Err(ChildFault::Status(status.to_string())),
    };
    write_answer(&answer, args.limit, out)
}

/// The channel to a frontend's worker listener and the runtime that drives it.
struct Session {
    runtime: tokio::runtime::Runtime,
    endpoint: tonic::transport::Endpoint,
}

fn unreachable(e: &dyn std::fmt::Display) -> ChildFault {
    ChildFault::Unreachable(e.to_string())
}

/// `status` as the one line the child says: the code and the server's words.
fn status_line(s: &tonic::Status) -> String {
    format!("{:?}: {}", s.code(), s.message())
}

fn frontend_session(args: &ChildArgs, identity: &Identity) -> Result<Session, ChildFault> {
    use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity as TlsIdentity};

    let url = plain_url(&args.url, false)?;
    let host = url[8..].rsplit_once(':').map_or(&url[8..], |(h, _)| h);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(&identity.ca))
        .identity(TlsIdentity::from_pem(&identity.cert, &identity.key))
        // The name in the SAN: the URL's host and no other.
        .domain_name(host);
    let endpoint = Endpoint::from_shared(url.clone()).map_err(|e| unreachable(&e))?;
    let endpoint = endpoint.tls_config(tls).map_err(|e| unreachable(&e))?;
    let endpoint = endpoint.connect_timeout(CONNECT_TIMEOUT);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(ChildFault::Out)?;
    Ok(Session { runtime, endpoint })
}

/// What `ReleaseService.Describe` says. `NOT_FOUND` and `FAILED_PRECONDITION` are the answer
/// that there is no description to be had (the message names what is lacking); any other
/// status is a failure.
fn describe_frontend(
    args: &ChildArgs,
    identity: &Identity,
    commit: &str,
    out: &mut dyn Write,
) -> Result<u64, ChildFault> {
    use kbf_proto::update::DescribeRequest;
    use kbf_proto::update::release_service_client::ReleaseServiceClient;
    use tonic::Code;

    let Session { runtime, endpoint } = frontend_session(args, identity)?;
    let answer = runtime.block_on(async {
        let channel = endpoint.connect().await.map_err(|e| unreachable(&e))?;
        let asked = DescribeRequest {
            commit: commit.to_owned(),
            platform: args.platform.clone().unwrap_or_default(),
        };
        match ReleaseServiceClient::new(channel).describe(asked).await {
            Ok(said) => Ok(Answer::from_proto(said.into_inner())),
            Err(s) if matches!(s.code(), Code::NotFound | Code::FailedPrecondition) => {
                Ok(Answer::Refused(status_line(&s)))
            }
            Err(s) => Err(ChildFault::Status(status_line(&s))),
        }
    })?;
    write_answer(&answer, args.limit, out)
}

fn fetch_frontend(
    args: &ChildArgs,
    identity: &Identity,
    sha256: &str,
    out: &mut dyn Write,
) -> Result<u64, ChildFault> {
    use kbf_proto::update::FetchRequest;
    use kbf_proto::update::release_service_client::ReleaseServiceClient;

    let Session { runtime, endpoint } = frontend_session(args, identity)?;
    runtime.block_on(async {
        let channel = endpoint.connect().await.map_err(|e| unreachable(&e))?;
        let mut stream = ReleaseServiceClient::new(channel)
            .fetch(FetchRequest {
                sha256: sha256.to_owned(),
            })
            .await
            .map_err(|s| ChildFault::Status(status_line(&s)))?
            .into_inner();
        let mut written = 0_u64;
        while written < args.limit {
            let chunk = match stream.message().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(s) => return Err(ChildFault::Status(status_line(&s))),
            };
            let room = usize::try_from(args.limit - written).unwrap_or(usize::MAX);
            let data = &chunk.data[..chunk.data.len().min(room)];
            out.write_all(data).map_err(ChildFault::Out)?;
            written += data.len() as u64;
        }
        Ok(written)
    })
}

/// What the child was asked for, once the flags that say so are checked.
enum Ask<'a> {
    /// The blob with this sha256.
    Blob(&'a str),
    /// What the release of this commit holds.
    Release(&'a str),
}

/// Fetch what `args` name, or say what the release they name holds, and write at most
/// `args.limit` bytes of it to `out`; the count.
pub fn fetch(args: &ChildArgs, out: &mut dyn Write) -> Result<u64, ChildFault> {
    let (ask, most) = match (&args.sha256, &args.describe) {
        (Some(sha256), None) => {
            if !is_digest(sha256) {
                return Err(ChildFault::Digest(sha256.clone()));
            }
            // The parser lets this through (it drops a requirement that a conflict rules out).
            if args.platform.is_some() {
                return Err(ChildFault::Flags("a platform belongs to a description"));
            }
            (Ask::Blob(sha256), MAX_COMPONENT_CEILING + 1)
        }
        (None, Some(commit)) => {
            if CommitId::new(commit).is_err() {
                return Err(ChildFault::Commit(commit.clone()));
            }
            (Ask::Release(commit), describe::MAX_BYTES + 1)
        }
        _ => {
            return Err(ChildFault::Flags(
                "it is asked for a blob or for a release, not both",
            ));
        }
    };
    if args.limit == 0 || args.limit > most {
        return Err(ChildFault::Limit {
            limit: args.limit,
            max: most,
        });
    }
    if let Some(platform) = &args.platform {
        if args.kind == Kind::Url {
            return Err(ChildFault::Flags(
                "a url source's file lists every platform",
            ));
        }
        if ReleasePlatform::parse(platform).is_none() {
            return Err(ChildFault::Platform(platform.clone()));
        }
    }
    match (args.kind, args.identity_fd) {
        (Kind::Url, None) => {
            identity::ensure_descriptors(&[0, 1, 2])?;
            match ask {
                Ask::Blob(sha256) => fetch_url(args, sha256, out),
                Ask::Release(commit) => describe_url(args, commit, out),
            }
        }
        (Kind::Url, Some(_)) => Err(ChildFault::Flags("a url fetch is lent no identity")),
        (Kind::Frontend, None) => Err(ChildFault::Flags("a frontend fetch needs --identity-fd")),
        (Kind::Frontend, Some(fd)) => {
            if args.ca_file.is_some() {
                return Err(ChildFault::Flags(
                    "a frontend fetch trusts the pinned CA only",
                ));
            }
            let lent = identity::adopt(fd)?;
            let identity = Identity::decode(lent)?;
            match ask {
                Ask::Blob(sha256) => fetch_frontend(args, &identity, sha256, out),
                Ask::Release(commit) => describe_frontend(args, &identity, commit, out),
            }
        }
    }
}

#[cfg(test)]
#[path = "child_tests.rs"]
mod tests;
