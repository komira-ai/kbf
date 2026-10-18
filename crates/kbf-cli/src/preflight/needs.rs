//! What each mode asks of its host, read from its parsed flags: which files it reads,
//! which certificate is whose, which store needs which credential.

use std::path::Path;

use kbf_server::Args;
use kbf_server::stores::StoreUri;

use super::{CertNeed, ContainerNeed, FileNeed, Needs, Row};
use crate::Invocation;

/// The needs of `invocation`.
#[must_use]
pub fn needs(invocation: &Invocation) -> Needs {
    match invocation {
        Invocation::Daemon(cli) => {
            let mut n = Needs::new("daemon");
            daemon(&mut n, cli);
            n
        }
        Invocation::Storage(flags) => {
            let mut n = Needs::new("storage");
            storage(&mut n, flags);
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
            n
        }
        Invocation::AllInOne(flags) => {
            let mut n = Needs::new("all-in-one");
            server_files(&mut n, &flags.server);
            n.s3 = s3(&flags.server);
            daemon(&mut n, &flags.daemon);
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
        }
    }

    fn file(&mut self, flag: &'static str, path: &Path) {
        self.files.push(FileNeed {
            flag,
            path: path.to_owned(),
            secret: false,
        });
    }

    fn secret(&mut self, flag: &'static str, path: &Path) {
        self.files.push(FileNeed {
            flag,
            path: path.to_owned(),
            secret: true,
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

/// The files the frontend half of `kbf-server` reads: the REAPI listener's pair, the auth
/// policy, the operator API's token, the worker listener's certificate, key, client CA
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

fn storage(n: &mut Needs, flags: &kbf_server::storage_node::StorageFlags) {
    // A node that registers with a frontend may be given the key pair by it
    // (`kbf_server::cold_apply`); the start says which variable is missing if the
    // frontend has no store registered either.
    n.s3 = s3(&flags.server) && flags.frontend.is_none();
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
}
