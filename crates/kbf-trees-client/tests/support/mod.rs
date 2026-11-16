//! What the tests of the client share: a fake on a fixed clock and a client of it, and
//! builders of requests.

#![allow(
    dead_code,
    reason = "each test binary includes this module and uses a different part of it"
)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_trees_client::fake::FakeTreesServer;
use kbf_trees_client::v1::location::Where;
use kbf_trees_client::v1::{
    ChildTree, CreateTreeRequest, Entry, File, FsLocation, Location, Prefetch, Source, entry,
};
use kbf_trees_client::{Client, Config, TreeDigest};

/// The time both sides read: 2027-01-15 08:00:00 UTC.
pub fn fixed() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

/// A fake on the fixed clock, and the config of a client of it on the same clock.
pub async fn fake() -> (FakeTreesServer, Config) {
    let server = FakeTreesServer::start().await.expect("a fake");
    server.set_clock(fixed);
    let config = server.config().expect("its config").with_clock(fixed);
    (server, config)
}

/// A fake and a client of it.
pub async fn fake_and_client() -> (FakeTreesServer, Client) {
    let (server, config) = fake().await;
    let client = Client::new(config).expect("a client");
    (server, client)
}

/// A file of `size` bytes whose `sha256` is `fill` repeated, read whole from `cas/l/`.
pub fn whole(path: &str, fill: u8, size: u64) -> Entry {
    Entry {
        path: path.to_owned(),
        kind: Some(entry::Kind::File(File {
            mode: None,
            sha256: vec![fill; 32],
            size,
            from: None,
        })),
    }
}

/// A file of `size` bytes that is the span at `offset` of the object `key`.
pub fn span(path: &str, fill: u8, size: u64, key: &str, offset: u64) -> Entry {
    Entry {
        path: path.to_owned(),
        kind: Some(entry::Kind::File(File {
            mode: Some(0o755),
            sha256: vec![fill; 32],
            size,
            from: Some(Source {
                key: key.to_owned(),
                offset,
            }),
        })),
    }
}

/// An entry that places the node's tree `tree` at `path`.
pub fn child(path: &str, tree: TreeDigest) -> Entry {
    Entry {
        path: path.to_owned(),
        kind: Some(entry::Kind::Tree(ChildTree {
            tree: tree.as_bytes().to_vec(),
        })),
    }
}

/// The `fs` location `jobs/<job>`, where a job's mount finds its tree.
pub fn job(id: &str) -> Location {
    Location {
        r#where: Some(Where::Fs(FsLocation {
            path: format!("jobs/{id}"),
        })),
    }
}

/// A create of `entries` at `at`, with `prefetch` left to the node.
pub fn create(entries: Vec<Entry>, at: Vec<Location>) -> CreateTreeRequest {
    CreateTreeRequest {
        entries,
        at,
        ..CreateTreeRequest::default()
    }
}

/// [`create`] with `prefetch`.
pub fn create_with(
    entries: Vec<Entry>,
    at: Vec<Location>,
    prefetch: Prefetch,
) -> CreateTreeRequest {
    CreateTreeRequest {
        prefetch: prefetch.into(),
        ..create(entries, at)
    }
}

/// A self-signed CA and a leaf it signs for `names`, as PEM: (CA, leaf certificate, leaf
/// key).
pub fn pki(names: &[&str]) -> (String, String, String) {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca = rcgen::CertifiedIssuer::self_signed(params, ca_key).unwrap();
    let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(names)
        .unwrap()
        .signed_by(&leaf_key, &ca)
        .unwrap();
    (ca.pem(), leaf.pem(), leaf_key.serialize_pem())
}

/// The TLS settings of a server presenting `cert` with `key` (PEM).
pub fn server_tls(cert: &str, key: &str) -> std::sync::Arc<rustls::ServerConfig> {
    use rustls::pki_types::pem::PemObject as _;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let chain = CertificateDer::pem_slice_iter(cert.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_slice(key.as_bytes()).unwrap();
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
    std::sync::Arc::new(config)
}
