//! `linux issue-cert`: a node's certificate for the mutual TLS of the new modes, signed
//! by the farm's CA (docs/deploy/kbf-modes.md, "A node's certificate").
//!
//! The certificate names the node by each `--name` (a DNS name) and `--ip` in its
//! subjectAltName, and its role as the URI `urn:kbf:role:<role>`: a storage node admits
//! each caller by the role its certificate names (kbf-storage's `role_of`). How a
//! certificate spells its role is provisional until role certificates are settled; a
//! kbf-cli test issues one of each role here and reads it back with `role_of`, so the two
//! cannot drift apart unseen. Its key is a fresh ECDSA P-256 key, made where the verb
//! runs; it allows serverAuth and clientAuth (a storage node serves, a frontend serves
//! clients and calls storage nodes); it is valid from five minutes ago for `--days`.
//!
//! Before printing anything the verb checks the result in process, as a TLS client
//! would: the certificate chains to `--ca-cert` for serverAuth under every name, and its
//! key is its own (so a `--ca-key` that is not the CA's is refused).
//!
//! Without `--apply` nothing is written: it prints what it would write. With it, it
//! writes `<out>/<role>.pem` (0644) and `<out>/<role>.key` (0600), and refuses to
//! replace either. Install them on the node as the unit's user's (`chown`): `install-units
//! --mode` refuses a key another user owns or others can read.

use std::fs::OpenOptions;
use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, ValueEnum};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};

use crate::pilot::certs::{self, CertError};

/// How a certificate names its role: `urn:kbf:role:<name>` in its subjectAltName.
pub const ROLE_URI_PREFIX: &str = "urn:kbf:role:";

/// How long before now a certificate is valid from: a node whose clock is behind still
/// takes it.
const SKEW: Duration = Duration::from_secs(300);

/// The role a certificate is issued for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Role {
    /// A frontend (`kbf frontend`): serves clients, calls storage nodes.
    Frontend,
    /// A storage node (`kbf storage`): `--storage-tls-cert` and `--storage-tls-key`.
    Storage,
}

impl Role {
    /// The role's name in the certificate, and the files' stem.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Role::Frontend => "frontend",
            Role::Storage => "storage",
        }
    }
}

#[derive(Args)]
pub struct IssueArgs {
    /// The node's role.
    #[arg(long, value_enum)]
    role: Role,
    /// A DNS name the node is reached by (repeat for more); the first is the subject's
    /// common name.
    #[arg(long = "name", required = true)]
    names: Vec<String>,
    /// An address the node is reached by (repeat for more).
    #[arg(long = "ip")]
    ips: Vec<IpAddr>,
    /// The farm CA's certificate (PEM).
    #[arg(long)]
    ca_cert: PathBuf,
    /// The farm CA's private key (PKCS#8 PEM).
    #[arg(long)]
    ca_key: PathBuf,
    /// The directory to write `<role>.pem` and `<role>.key` into.
    #[arg(long)]
    out: PathBuf,
    /// Days the certificate is valid.
    #[arg(long, default_value_t = 365, value_parser = clap::value_parser!(u32).range(1..=825))]
    days: u32,
    /// Write the two files.
    #[arg(long)]
    apply: bool,
}

/// Why no certificate was issued or written.
#[derive(Debug, thiserror::Error)]
pub enum IssueError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "the CA key is not a PKCS#8 PEM key this kit reads ({0}): openssl pkcs8 -topk8 -nocrypt"
    )]
    CaKey(String),
    #[error("the CA certificate cannot be read: {0}")]
    CaCert(String),
    #[error("the certificate cannot be made: {0}")]
    Make(String),
    /// The issued certificate does not check out against the CA: the CA key is not the
    /// CA certificate's, or a name is not one a TLS client takes.
    #[error("the issued certificate (read as server.pem, its key as server.key): {0}")]
    Check(#[from] CertError),
    #[error("{0} exists: this verb never replaces a certificate or key (move it aside first)")]
    Exists(PathBuf),
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// A certificate (PEM and DER) and its key (PEM).
pub struct Issued {
    pub cert_pem: String,
    pub cert_der: Vec<u8>,
    pub key_pem: String,
}

/// A certificate for `role` naming `names` and `ips`, signed by the CA, valid from
/// five minutes before `now` for `days`; checked against the CA as a client would.
///
/// # Errors
/// [`IssueError`]: the CA's key or certificate cannot be read, the certificate cannot be
/// made, or it does not check out.
pub fn issue(
    ca_cert_pem: &str,
    ca_key_pem: &str,
    role: Role,
    names: &[String],
    ips: &[IpAddr],
    now: SystemTime,
    days: u32,
) -> Result<Issued, IssueError> {
    let ca_key = KeyPair::from_pem(ca_key_pem).map_err(|e| IssueError::CaKey(e.to_string()))?;
    let issuer = Issuer::from_ca_cert_pem(ca_cert_pem, ca_key)
        .map_err(|e| IssueError::CaCert(e.to_string()))?;
    let make = |e: rcgen::Error| IssueError::Make(e.to_string());
    let mut params = CertificateParams::default();
    for name in names {
        params
            .subject_alt_names
            .push(SanType::DnsName(name.clone().try_into().map_err(make)?));
    }
    params
        .subject_alt_names
        .extend(ips.iter().copied().map(SanType::IpAddress));
    let role_uri = format!("{ROLE_URI_PREFIX}{}", role.name());
    params
        .subject_alt_names
        .push(SanType::URI(role_uri.try_into().map_err(make)?));
    let mut dn = DistinguishedName::new();
    if let Some(first) = names.first() {
        dn.push(DnType::CommonName, first.as_str());
    }
    params.distinguished_name = dn;
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    params.use_authority_key_identifier_extension = true;
    let since_epoch = now
        .checked_sub(SKEW)
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .unwrap_or_default();
    params.not_before = rcgen::date_time_ymd(1970, 1, 1) + since_epoch;
    params.not_after = params.not_before + Duration::from_secs(86_400 * u64::from(days)) + SKEW;
    let key = KeyPair::generate().map_err(make)?;
    let cert = params.signed_by(&key, &issuer).map_err(make)?;
    let issued = Issued {
        cert_pem: cert.pem(),
        cert_der: cert.der().to_vec(),
        key_pem: key.serialize_pem(),
    };
    let ips: Vec<String> = ips.iter().map(ToString::to_string).collect();
    for name in names.iter().chain(&ips) {
        certs::check(
            ca_cert_pem.as_bytes(),
            issued.cert_pem.as_bytes(),
            issued.key_pem.as_bytes(),
            name,
        )?;
    }
    Ok(issued)
}

/// The two files `issue-cert` writes for `role` into `out`: the certificate and the key.
#[must_use]
pub fn files(out: &Path, role: Role) -> [PathBuf; 2] {
    [
        out.join(format!("{}.pem", role.name())),
        out.join(format!("{}.key", role.name())),
    ]
}

/// `issue-cert`: issue, then print what would be written or write it; the lines to print.
///
/// # Errors
/// [`IssueError`].
pub fn run(a: &IssueArgs, now: SystemTime) -> Result<Vec<String>, IssueError> {
    let read = |path: &Path| {
        std::fs::read_to_string(path).map_err(|source| IssueError::Read {
            path: path.to_owned(),
            source,
        })
    };
    let issued = issue(
        &read(&a.ca_cert)?,
        &read(&a.ca_key)?,
        a.role,
        &a.names,
        &a.ips,
        now,
        a.days,
    )?;
    let [cert, key] = files(&a.out, a.role);
    for path in [&cert, &key] {
        if path.exists() {
            return Err(IssueError::Exists(path.clone()));
        }
    }
    let ips: Vec<String> = a.ips.iter().map(ToString::to_string).collect();
    let mut lines = vec![format!(
        "{} certificate for {} (role {}{}), valid {} days, chains to {}",
        a.role.name(),
        a.names
            .iter()
            .chain(&ips)
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
        ROLE_URI_PREFIX,
        a.role.name(),
        a.days,
        a.ca_cert.display()
    )];
    let writes = [
        (&cert, &issued.cert_pem, 0o644),
        (&key, &issued.key_pem, 0o600),
    ];
    for (path, text, bits) in writes {
        if a.apply {
            write_new(path, text.as_bytes(), bits)?;
            lines.push(format!("wrote {} ({bits:o})", path.display()));
        } else {
            lines.push(format!("DRY-RUN: write {} ({bits:o})", path.display()));
        }
    }
    if !a.apply {
        lines.push("DRY-RUN: nothing written; re-run with --apply".to_owned());
    }
    Ok(lines)
}

/// Write `bytes` into a new file at `path` with mode `bits` from its creation (a key is
/// never readable by others, not even for a moment).
fn write_new(path: &Path, bytes: &[u8], bits: u32) -> Result<(), IssueError> {
    let fail = |source| IssueError::Write {
        path: path.to_owned(),
        source,
    };
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(bits)
        .open(path)
        .map_err(fail)?;
    f.write_all(bytes).map_err(fail)?;
    f.sync_all().map_err(fail)
}

/// The verb: 0 issued (or would be), 2 refused, with a `REFUSED:` line.
pub fn cli(a: &IssueArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    match run(a, SystemTime::now()) {
        Ok(lines) => {
            for line in lines {
                let _ = writeln!(out, "{line}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            let _ = writeln!(err, "REFUSED: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
#[path = "issue_cert_tests.rs"]
mod tests;
