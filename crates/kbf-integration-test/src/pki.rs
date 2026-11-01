//! A throwaway certificate authority for one integration cell: a CA, a server
//! certificate for `localhost`, one daemon client certificate, and the certificates of a
//! frontend and a storage node as `kbf --mode=...` requires them, written as PEM files.
//!
//! The keys are generated fresh on every call and never leave the cell's directory.
//! They are test material: the worker listener's mutual TLS needs them, and the
//! farm's real CA is not involved.

use std::path::{Path, PathBuf};

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};

/// The name in the server certificate. A daemon verifies it with `--tls-server-name`.
pub const SERVER_NAME: &str = "localhost";

/// The role the daemon's client certificate names, as the worker listener requires.
const DAEMON_ROLE_URI: &str = "urn:kbf:role:daemon";
/// The roles `kbf --mode=frontend` and `kbf --mode=storage` check their own certificates
/// for at start (each with both server and client use).
const FRONTEND_ROLE_URI: &str = "urn:kbf:role:frontend";
const STORAGE_ROLE_URI: &str = "urn:kbf:role:storage";

/// Where [`write`] put each file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PkiFiles {
    /// The CA certificate: the server's client CA and the daemon's trust root.
    pub ca_cert: PathBuf,
    /// The server certificate and its key.
    pub server_cert: PathBuf,
    pub server_key: PathBuf,
    /// The daemon's client certificate and its key.
    pub client_cert: PathBuf,
    pub client_key: PathBuf,
    /// The frontend's certificate and key (role `frontend`, for `localhost`): its worker
    /// listener's, and the one it presents to the storage node. The key is mode 0600, as
    /// `kbf` requires of a secret.
    pub frontend_cert: PathBuf,
    pub frontend_key: PathBuf,
    /// The storage node's certificate and key (role `storage`, for `localhost`), mode 0600
    /// for the key.
    pub storage_cert: PathBuf,
    pub storage_key: PathBuf,
}

/// Why the certificates could not be made or written.
#[derive(Debug, thiserror::Error)]
pub enum PkiError {
    #[error("generate certificates: {0}")]
    Generate(#[from] rcgen::Error),
    #[error("write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Generates a CA, a server certificate for [`SERVER_NAME`], a client certificate
/// that names `node_id` (as its one DNS subjectAltName, which the server binds to the
/// daemon's `Hello`, and as its common name) and the daemon role
/// (`urn:kbf:role:daemon`), and the certificates of a frontend and a storage node (each
/// for [`SERVER_NAME`], with its role and both server and client use), and writes them
/// under `dir` (created if missing).
///
/// # Errors
/// Generation fails, or a file cannot be written.
pub fn write(dir: &Path, node_id: &str) -> Result<PkiFiles, PkiError> {
    let mut ca = CertificateParams::new(Vec::<String>::new())?;
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(DnType::CommonName, "kbf integration cell CA");
    ca.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate()?)?;

    let leaf = |names: Vec<String>,
                cn: &str,
                ekus: Vec<ExtendedKeyUsagePurpose>,
                role: Option<&str>|
     -> Result<(String, String), PkiError> {
        let mut params = CertificateParams::new(names)?;
        if let Some(role) = role {
            params
                .subject_alt_names
                .push(SanType::URI(role.try_into()?));
        }
        params.distinguished_name.push(DnType::CommonName, cn);
        params.extended_key_usages = ekus;
        let key = KeyPair::generate()?;
        let cert = params.signed_by(&key, &ca)?;
        Ok((cert.pem(), key.serialize_pem()))
    };
    let (server_cert, server_key) = leaf(
        vec![SERVER_NAME.to_owned()],
        "kbf integration server",
        vec![ExtendedKeyUsagePurpose::ServerAuth],
        None,
    )?;
    let (client_cert, client_key) = leaf(
        vec![node_id.to_owned()],
        node_id,
        vec![ExtendedKeyUsagePurpose::ClientAuth],
        Some(DAEMON_ROLE_URI),
    )?;
    let both = || {
        vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ]
    };
    let (frontend_cert, frontend_key) = leaf(
        vec![SERVER_NAME.to_owned()],
        "kbf integration frontend",
        both(),
        Some(FRONTEND_ROLE_URI),
    )?;
    let (storage_cert, storage_key) = leaf(
        vec![SERVER_NAME.to_owned()],
        "kbf integration storage",
        both(),
        Some(STORAGE_ROLE_URI),
    )?;

    let create = std::fs::create_dir_all(dir);
    create.map_err(|source| PkiError::Write {
        path: dir.to_owned(),
        source,
    })?;
    let put = |file: &str, text: &str, mode: u32| {
        let path = dir.join(file);
        let wrote = std::fs::write(&path, text).and_then(|()| set_mode(&path, mode));
        match wrote {
            Ok(()) => Ok(path),
            Err(source) => Err(PkiError::Write { path, source }),
        }
    };
    Ok(PkiFiles {
        ca_cert: put("ca.pem", &ca.pem(), 0o644)?,
        server_cert: put("server.pem", &server_cert, 0o644)?,
        server_key: put("server.key", &server_key, 0o644)?,
        client_cert: put("client.pem", &client_cert, 0o644)?,
        client_key: put("client.key", &client_key, 0o644)?,
        frontend_cert: put("frontend.pem", &frontend_cert, 0o644)?,
        frontend_key: put("frontend.key", &frontend_key, 0o600)?,
        storage_cert: put("storage.pem", &storage_cert, 0o644)?,
        storage_key: put("storage.key", &storage_key, 0o600)?,
    })
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}
