//! What each mode asks of its host, read from its parsed flags: which files it reads,
//! which certificate is whose, which store needs which credential.

use std::path::{Path, PathBuf};

use kbf_server::Args;
use kbf_server::proxy_auth::SECRET_BYTES;
use kbf_server::stores::StoreUri;

use super::{CertNeed, ContainerNeed, FileNeed, KeyRule, Needs, Row, key_file};
use crate::Invocation;

/// The needs of `invocation`.
#[must_use]
pub fn needs(invocation: &Invocation) -> Needs {
    needs_in(invocation, Path::new(crate::config::DEFAULT_PATH))
}

/// The needs of `invocation` started with the settings file `config`: the legacy key file
/// is the `s3.env` in its directory.
#[must_use]
pub fn needs_in(invocation: &Invocation, config: &Path) -> Needs {
    match invocation {
        // The flags of a start that enrolls are made after it has: the start checks them
        // then (`node::run`).
        Invocation::Registered(start) => Needs::new(start.mode.unwrap_or("kbf")),
        Invocation::Daemon(cli) => {
            let mut n = Needs::new("daemon");
            daemon(&mut n, cli);
            n.key = KeyRule::Absent;
            n
        }
        Invocation::Storage(flags) => {
            let mut n = Needs::new("storage");
            storage(&mut n, flags);
            n.key = KeyRule::Holder(key_file(config));
            n
        }
        Invocation::Frontend(flags) => {
            let mut n = Needs::new("frontend");
            server_files(&mut n, &flags.server);
            if let Some([cert, key, ca]) = &flags.tls {
                n.cert(
                    ("--storage-client-cert", cert),
                    Some(("--storage-client-key", key)),
                    Row::Frontend,
                    None,
                    None,
                );
                n.secret("--storage-client-key", key);
                n.file("--storage-ca", ca);
            }
            n
        }
        Invocation::StorageAndDaemon(flags) => {
            let mut n = Needs::new("storage-and-daemon");
            storage(&mut n, &flags.storage);
            daemon(&mut n, &flags.daemon);
            n.key = KeyRule::RootAlone(key_file(config));
            n
        }
        Invocation::AllInOne(flags) => {
            let mut n = Needs::new("all-in-one");
            server_files(&mut n, &flags.server);
            cold_keys(&mut n, &flags.server, true);
            n.hot_store = hot_dir(&flags.server);
            daemon(&mut n, &flags.daemon);
            // An all-in-one with an S3 cold store holds the key on purpose.
            if !s3(&flags.server) {
                n.key = KeyRule::Absent;
            }
            n
        }
    }
}

impl Needs {
    fn new(mode: &'static str) -> Self {
        Self {
            mode,
            files: Vec::new(),
            certs: Vec::new(),
            s3: false,
            container: None,
            hot_store: None,
            cpuset: false,
            key: KeyRule::None,
        }
    }

    fn file(&mut self, flag: &'static str, path: &Path) {
        self.files.push(FileNeed {
            flag,
            path: path.to_owned(),
            secret: false,
            exact_len: None,
        });
    }

    fn secret(&mut self, flag: &'static str, path: &Path) {
        self.files.push(FileNeed {
            flag,
            path: path.to_owned(),
            secret: true,
            exact_len: None,
        });
    }

    /// A secret file that must hold exactly `len` bytes.
    fn secret_of_len(&mut self, flag: &'static str, path: &Path, len: usize) {
        self.files.push(FileNeed {
            flag,
            path: path.to_owned(),
            secret: true,
            exact_len: Some(len),
        });
    }

    /// A certificate: its file, and what it must be. `ca` is the flag and file of the CA it
    /// chains to, where that is the farm CA the mode holds.
    fn cert(
        &mut self,
        (flag, path): (&'static str, &Path),
        key: Option<(&'static str, &Path)>,
        row: Row,
        ca: Option<(&'static str, &Path)>,
        node_id: Option<&str>,
    ) {
        self.file(flag, path);
        self.certs.push(CertNeed {
            flag,
            path: path.to_owned(),
            row,
            key: key.map(|(flag, path)| (flag, path.to_owned())),
            ca: ca.map(|(flag, path)| (flag, path.to_owned())),
            node_id: node_id.map(str::to_owned),
        });
    }
}

/// Whether the cold store is an S3 bucket, whose requests the key pair signs.
fn s3(args: &Args) -> bool {
    matches!(args.cold_store, Some(StoreUri::S3(_)))
}

/// What the cold store's key pair asks of the host: the key file, as a secret, when
/// `--cold-store-credentials-file` names one (the environment is not read then); else, for an
/// S3 store and when `env_pair` (no registration may supply it), the two variables.
fn cold_keys(n: &mut Needs, args: &Args, env_pair: bool) {
    match &args.cold_store_credentials_file {
        Some(file) => n.secret("--cold-store-credentials-file", file),
        None => n.s3 = s3(args) && env_pair,
    }
}

/// The files the frontend half of `kbf-server` reads: the REAPI listener's pair, the auth
/// policy, the operator API's token, the proxy's secret, the worker listener's certificate, key, client CA
/// and deny list.
fn server_files(n: &mut Needs, a: &Args) {
    if let Some(cert) = &a.reapi_tls_cert {
        // Not the farm CA's (a public name's certificate): a file, nothing more.
        n.file("--reapi-tls-cert", cert);
    }
    if let Some(key) = &a.reapi_tls_key {
        n.secret("--reapi-tls-key", key);
    }
    if let Some(policy) = &a.reapi_auth_policy {
        n.file("--reapi-auth-policy", policy);
    }
    if let Some(token) = &a.api_token_file {
        n.secret("--api-token-file", token);
    }
    if let Some(secret) = &a.proxy_auth_file {
        n.secret_of_len("--proxy-auth-file", secret, SECRET_BYTES);
    }
    if let Some(cert) = &a.worker_tls_cert {
        let ca = a.worker_client_ca.as_deref();
        n.cert(
            ("--worker-tls-cert", cert),
            a.worker_tls_key
                .as_deref()
                .map(|key| ("--worker-tls-key", key)),
            Row::Frontend,
            ca.map(|ca| ("--worker-client-ca", ca)),
            None,
        );
    }
    if let Some(key) = &a.worker_tls_key {
        n.secret("--worker-tls-key", key);
    }
    if let Some(ca) = &a.worker_client_ca {
        n.file("--worker-client-ca", ca);
    }
    if let Some(list) = &a.worker_deny_list {
        n.file("--worker-deny-list", list);
    }
}

/// The directory of a `file://` hot store: the one directory whose ancestors the process
/// must be able to enter.
fn hot_dir(args: &Args) -> Option<PathBuf> {
    match &args.hot_store {
        Some(StoreUri::File(dir)) => Some(dir.clone()),
        _ => None,
    }
}

fn storage(n: &mut Needs, flags: &kbf_server::storage_node::StorageFlags) {
    n.hot_store = hot_dir(&flags.server);
    // A node that registers with a frontend may be given the key pair by it
    // (`kbf_server::cold_apply`); the start says which variable is missing if the
    // frontend has no store registered either.
    cold_keys(n, &flags.server, flags.frontend.is_none());
    if let Some([cert, key, ca]) = &flags.tls {
        n.cert(
            ("--storage-tls-cert", cert),
            Some(("--storage-tls-key", key)),
            Row::Storage,
            Some(("--storage-client-ca", ca)),
            None,
        );
        n.secret("--storage-tls-key", key);
        n.file("--storage-client-ca", ca);
    }
}

fn daemon(n: &mut Needs, cli: &kbf_node::Cli) {
    let d = cli.host_needs();
    n.cert(
        ("--cert", &d.cert),
        Some(("--key", &d.key)),
        Row::Daemon,
        Some(("--ca-cert", &d.ca_cert)),
        Some(&d.node_id),
    );
    n.file("--ca-cert", &d.ca_cert);
    n.secret("--key", &d.key);
    n.container = d.container.map(|c| ContainerNeed {
        oci_runtime: c.oci_runtime,
    });
    // Leases are placed on the node's CPUs and NUMA nodes by the container driver.
    n.cpuset = n.container.is_some();
}
