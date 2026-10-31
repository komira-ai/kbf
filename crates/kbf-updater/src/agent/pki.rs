//! A farm CA and the certificates it issues, written to files, for the tests of the agent.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use kbf_ca::{Authority, FileSigner, NodeCert, Policy, PublicKey, SystemEntropy, UpdaterCert};
use kbf_types::{HostRole, Role};
use rcgen::{KeyPair, PublicKeyData};

use super::coordinator::TlsFiles;

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs()
        .try_into()
        .expect("fits")
}

pub fn pem_of(label: &str, der: &[u8]) -> String {
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = body
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).expect("ascii"))
        .collect();
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        lines.join("\n")
    )
}

/// A certificate and the key it was issued for.
#[derive(Clone)]
pub struct Leaf {
    pub der: Vec<u8>,
    pub key: Vec<u8>,
}

impl Leaf {
    pub fn cert_pem(&self) -> String {
        pem_of("CERTIFICATE", &self.der)
    }

    pub fn key_pem(&self) -> String {
        pem_of("PRIVATE KEY", &self.key)
    }
}

/// A farm CA.
pub struct Pki {
    pub ca: Authority,
}

fn key_pair() -> (KeyPair, PublicKey) {
    let pair = KeyPair::generate().expect("a key");
    let public = PublicKey::from_spki(&pair.subject_public_key_info()).expect("a P-256 key");
    (pair, public)
}

impl Pki {
    pub fn new() -> Self {
        let signer = Arc::new(FileSigner::generate().expect("a key"));
        let ca = Authority::create(signer, Policy::default(), now(), &mut SystemEntropy)
            .expect("a root");
        Self { ca }
    }

    pub fn root_pem(&self) -> String {
        self.ca.bundle().to_pem()
    }

    /// The coordinator's certificate, for `localhost`.
    pub fn coordinator(&self) -> Leaf {
        self.coordinator_for(&["localhost", "127.0.0.1"])
    }

    /// The coordinator's certificate for `names`.
    pub fn coordinator_for(&self, names: &[&str]) -> Leaf {
        let (pair, public) = key_pair();
        let issued = self
            .ca
            .issue_coordinator(names, &public, now(), &mut SystemEntropy)
            .expect("issued");
        Leaf {
            der: issued.der,
            key: pair.serialize_der(),
        }
    }

    /// The frontend's certificate, for `localhost`: what the public proxy presents.
    pub fn frontend(&self) -> Leaf {
        let (pair, public) = key_pair();
        let issued = self
            .ca
            .issue_frontend(
                &["localhost", "127.0.0.1"],
                &public,
                now(),
                &mut SystemEntropy,
            )
            .expect("issued");
        Leaf {
            der: issued.der,
            key: pair.serialize_der(),
        }
    }

    /// An updater's certificate for the node `name` of a host running as `role`.
    pub fn updater(&self, name: &str, role: HostRole) -> Leaf {
        let (pair, public) = key_pair();
        let node_id = name.parse().expect("a node id");
        let cert = UpdaterCert {
            node_id: &node_id,
            host_role: &role,
            key: &public,
        };
        let issued = self
            .ca
            .issue_updater(&cert, now(), &mut SystemEntropy)
            .expect("issued");
        Leaf {
            der: issued.der,
            key: pair.serialize_der(),
        }
    }

    /// A storage node's certificate for `localhost`: the farm's own, not a coordinator's.
    pub fn storage_node(&self, name: &str) -> Leaf {
        let (pair, public) = key_pair();
        let node_id = name.parse().expect("a node id");
        let cert = NodeCert {
            node_id: &node_id,
            role: Role::Storage,
            key: &public,
            advertise: None,
        };
        let issued = self
            .ca
            .issue_node(&cert, now(), &mut SystemEntropy)
            .expect("issued");
        Leaf {
            der: issued.der,
            key: pair.serialize_der(),
        }
    }
}

/// Write the three files of an updater: its certificate and key (as `leaf`) and the CA.
pub fn write_files(dir: &Path, pki: &Pki, leaf: &Leaf) -> TlsFiles {
    let write = |name: &str, text: &str| -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).expect("written");
        path
    };
    TlsFiles {
        cert: write("updater.pem", &leaf.cert_pem()),
        key: {
            let key = write("updater.key", &leaf.key_pem());
            std::fs::set_permissions(&key, std::os::unix::fs::PermissionsExt::from_mode(0o600))
                .expect("the key is private");
            key
        },
        ca: write("ca.pem", &pki.root_pem()),
    }
}
