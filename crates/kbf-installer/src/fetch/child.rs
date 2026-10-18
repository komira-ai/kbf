//! `kbf-installer fetch-child`: the program the root parent re-executes as the fetch user to
//! pull one component over HTTPS and stream it to standard output.
//!
//! It holds no authority: the parent started it with a cleared environment, no input, no
//! file it may write, no supplementary groups and the limits of [`crate::run::Confine`], and
//! it believes nothing it reads: the parent counts and hashes the bytes and the attestation
//! decides. The arguments are the fixed flags of [`ChildArgs`], which the parent validated
//! and this program checks again (a program that only trusts its caller's checks is one
//! mistake from a request for another URL).
//!
//! It follows **no redirect**: any answer but 200 is a failed fetch and one line on standard
//! error. The roots it trusts are the system's, or the PEM file the configuration names.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::fetch::config::{ReleaseUrl, UrlFault};

/// How long the child waits to connect.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The fixed flags of `fetch-child`.
#[derive(clap::Args, Debug)]
pub struct ChildArgs {
    /// The https base URL of the file server (`<url>/files/<sha256>` is fetched).
    #[arg(long)]
    pub url: String,
    /// The lowercase hex sha256 that names the blob.
    #[arg(long)]
    pub sha256: String,
    /// The most bytes written to standard output.
    #[arg(long)]
    pub max_bytes: u64,
    /// A PEM file of the roots to trust, instead of the system's.
    #[arg(long)]
    pub ca_file: Option<PathBuf>,
}

/// Why the child fetched nothing (one line of this is all it says).
#[derive(Debug, thiserror::Error)]
pub enum ChildFault {
    #[error("{0}")]
    Url(#[from] UrlFault),
    #[error("{0:?} is not a sha256 (64 lowercase hex digits)")]
    Digest(String),
    #[error("cannot read the roots in {path}: {why}")]
    Roots { path: PathBuf, why: String },
    #[error("source unreachable: {0}")]
    Unreachable(String),
    #[error("the source answered {0}")]
    Status(u16),
    #[error("cannot write the bytes: {0}")]
    Out(io::Error),
}

fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The certificates of the PEM text `pem`, as DER.
pub fn pem_certificates(pem: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut found = Vec::new();
    let mut body: Option<String> = None;
    for line in pem.lines().map(str::trim) {
        match (line, &mut body) {
            ("-----BEGIN CERTIFICATE-----", None) => body = Some(String::new()),
            ("-----END CERTIFICATE-----", Some(text)) => {
                let der = STANDARD
                    .decode(text.as_bytes())
                    .map_err(|e| format!("a certificate is not base64: {e}"))?;
                found.push(der);
                body = None;
            }
            (line, Some(text)) => text.push_str(line),
            _ => {}
        }
    }
    if body.is_some() {
        return Err("a certificate has no END line".to_owned());
    }
    if found.is_empty() {
        return Err("no certificate".to_owned());
    }
    Ok(found)
}

fn roots(ca_file: Option<&PathBuf>) -> Result<Vec<ureq::tls::Certificate<'static>>, ChildFault> {
    let ders: Vec<Vec<u8>> = match ca_file {
        Some(path) => {
            let roots = |why: String| ChildFault::Roots {
                path: path.clone(),
                why,
            };
            let text = std::fs::read_to_string(path).map_err(|e| roots(e.to_string()))?;
            pem_certificates(&text).map_err(roots)?
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

/// Fetch the blob `args` names and write at most `args.max_bytes` bytes of it to `out`.
/// Returns how many were written.
pub fn fetch(args: &ChildArgs, out: &mut dyn Write) -> Result<u64, ChildFault> {
    let base = ReleaseUrl::parse(&args.url)?;
    if !is_digest(&args.sha256) {
        return Err(ChildFault::Digest(args.sha256.clone()));
    }
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::Specific(Arc::new(roots(
            args.ca_file.as_ref(),
        )?)))
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .tls_config(tls)
        .build()
        .into();
    let mut response = agent
        .get(&base.file(&args.sha256))
        .call()
        .map_err(|e| ChildFault::Unreachable(e.to_string()))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(ChildFault::Status(status));
    }
    let mut body = response.body_mut().as_reader().take(args.max_bytes);
    io::copy(&mut body, out).map_err(|e| {
        if e.kind() == io::ErrorKind::BrokenPipe {
            ChildFault::Out(e)
        } else {
            ChildFault::Unreachable(e.to_string())
        }
    })
}

/// The `fetch-child` verb: the exit code, 0 only for a body written through to its end.
pub fn main(args: &ChildArgs) -> std::process::ExitCode {
    let mut stdout = io::stdout().lock();
    let result = fetch(args, &mut stdout).and_then(|n| {
        stdout.flush().map_err(ChildFault::Out)?;
        Ok(n)
    });
    match result {
        Ok(_) => std::process::ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("fetch-child: {why}");
            std::process::ExitCode::FAILURE
        }
    }
}
