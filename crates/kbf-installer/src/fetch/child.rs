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

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, ValueEnum};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject as _;

use super::identity::{self, Identity, IdentityFault};
use super::{CONNECT_TIMEOUT, MAX_COMPONENT_CEILING, is_digest};

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
    /// The lowercase hex sha256 that names the blob.
    #[arg(long)]
    pub sha256: String,
    /// The most bytes written to standard output.
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
    #[error("the limit {0} is not 1 to {max}", max = MAX_COMPONENT_CEILING + 1)]
    Limit(u64),
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

fn fetch_url(args: &ChildArgs, out: &mut dyn Write) -> Result<u64, ChildFault> {
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
    let mut response = agent
        .get(format!("{base}/files/{}", args.sha256))
        .call()
        .map_err(|e| ChildFault::Unreachable(e.to_string()))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(ChildFault::Status(status.to_string()));
    }
    copy_limited(response.body_mut().as_reader(), args.limit, out)
}

fn fetch_frontend(
    args: &ChildArgs,
    identity: &Identity,
    out: &mut dyn Write,
) -> Result<u64, ChildFault> {
    use kbf_proto::update::FetchRequest;
    use kbf_proto::update::release_service_client::ReleaseServiceClient;
    use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity as TlsIdentity};

    let url = plain_url(&args.url, false)?;
    let host = url[8..].rsplit_once(':').map_or(&url[8..], |(h, _)| h);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let unreachable = |e: &dyn std::fmt::Display| ChildFault::Unreachable(e.to_string());
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
    runtime.block_on(async {
        let channel = endpoint.connect().await.map_err(|e| unreachable(&e))?;
        let mut stream = ReleaseServiceClient::new(channel)
            .fetch(FetchRequest {
                sha256: args.sha256.clone(),
            })
            .await
            .map_err(|s| ChildFault::Status(format!("{:?}: {}", s.code(), s.message())))?
            .into_inner();
        let mut written = 0_u64;
        while written < args.limit {
            let chunk = match stream.message().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(s) => {
                    return Err(ChildFault::Status(format!(
                        "{:?}: {}",
                        s.code(),
                        s.message()
                    )));
                }
            };
            let room = usize::try_from(args.limit - written).unwrap_or(usize::MAX);
            let data = &chunk.data[..chunk.data.len().min(room)];
            out.write_all(data).map_err(ChildFault::Out)?;
            written += data.len() as u64;
        }
        Ok(written)
    })
}

/// Fetch what `args` name and write at most `args.limit` bytes of it to `out`; the count.
pub fn fetch(args: &ChildArgs, out: &mut dyn Write) -> Result<u64, ChildFault> {
    if !is_digest(&args.sha256) {
        return Err(ChildFault::Digest(args.sha256.clone()));
    }
    if args.limit == 0 || args.limit > MAX_COMPONENT_CEILING + 1 {
        return Err(ChildFault::Limit(args.limit));
    }
    match (args.kind, args.identity_fd) {
        (Kind::Url, None) => {
            identity::ensure_descriptors(&[0, 1, 2])?;
            fetch_url(args, out)
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
            fetch_frontend(args, &identity, out)
        }
    }
}
