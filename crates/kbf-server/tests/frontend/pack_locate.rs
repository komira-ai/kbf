//! `kbf.worker.v1.PackIndex` on the worker listener of a real `kbf frontend` under mutual TLS:
//! the path the binary takes (`start_frontend`). With `--pack-index` a daemon's certificate is
//! told where blobs lie in packs, and the storage and frontend roles are refused by the role
//! table; without the flag the service is not registered and the daemon's call is UNIMPLEMENTED.
//!
//! The tests with the flag need a Postgres server (`KBF_TEST_POSTGRES_URL`), as the pack index
//! does; each makes a database of its own.

use kbf_proto::reapi::Digest;
use kbf_proto::worker::PackLocateRequest;
use kbf_proto::worker::pack_index_client::PackIndexClient;
use kbf_state::postgres::{TestDatabase, test_database};
use kbf_state::{BlobDigest, MAX_BATCH, NewPack, PackBlob, PackIndex};
use tonic::Code;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

use super::*;

const LOCATE: &str = "/kbf.worker.v1.PackIndex/Locate";

/// Runs `f` on a thread of its own and returns its result. A test database makes a runtime of
/// its own to make and to drop a database, and tokio refuses that on a thread running tasks.
fn off_runtime<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|scope| match scope.spawn(f).join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    })
}

/// A database of its own on the test server, dropped off the runtime (see [`off_runtime`]) so
/// that it is also dropped when a test fails.
struct Database(Option<TestDatabase>);

impl Database {
    fn new() -> Self {
        Self(Some(off_runtime(test_database)))
    }

    fn url(&self) -> String {
        self.0.as_ref().expect("held until dropped").url()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        if let Some(database) = self.0.take() {
            off_runtime(move || drop(database));
        }
    }
}

/// A started frontend, serving until `_stop` is dropped.
struct Packed {
    worker: SocketAddr,
    index: Option<Arc<dyn PackIndex>>,
    _stop: oneshot::Sender<()>,
}

fn start(flags: &FrontendFlags) -> Packed {
    let (stop, stopped) = oneshot::channel::<()>();
    let handle = start_frontend(flags, || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .expect("starts");
    let (worker, index) = (handle.worker, handle.pack_index());
    tokio::spawn(handle.serve());
    Packed {
        worker,
        index,
        _stop: stop,
    }
}

/// A connection to the worker listener at `worker` presenting a certificate that names `role`
/// and the one DNS name `node`.
async fn channel(pki: &Pki, worker: SocketAddr, role: &str, node: &str) -> Channel {
    let mut params = rcgen::CertificateParams::new(vec![node.to_owned()]).expect("params");
    params.subject_alt_names.push(rcgen::SanType::URI(
        format!("{}{role}", kbf_storage::service::ROLE_URI_PREFIX)
            .try_into()
            .expect("an IA5 name"),
    ));
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let key = rcgen::KeyPair::generate().expect("key");
    let cert = params.signed_by(&key, &pki.ca).expect("sign");
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(pki.ca.pem()))
        .domain_name("localhost")
        .identity(Identity::from_pem(cert.pem(), key.serialize_pem()));
    Endpoint::from_shared(format!("https://{worker}"))
        .expect("endpoint")
        .tls_config(tls)
        .expect("TLS")
        .connect()
        .await
        .expect("connect")
}

/// The wire digest of a blob of `size` bytes whose hash is `byte` repeated.
fn wire(byte: u8, size: i64) -> Digest {
    Digest {
        hash: hex::encode([byte; 32]),
        size_bytes: size,
    }
}

fn ask(digests: Vec<Digest>) -> PackLocateRequest {
    PackLocateRequest { digests }
}

/// The refusal the role table gives `role` for `Locate`, which only the daemon role may call.
fn table_refusal(role: &str) -> String {
    format!("role {role} may not call {LOCATE} on the worker listener; it takes daemon")
}

/// Asserts that a frontend started with `--pack-index` serves a daemon's `Locate` from the
/// index: a blob recorded in a pack is answered with the pack, its offset and its length, one the
/// index has no row for as not located, and a call over the bound is INVALID_ARGUMENT. The role
/// table refuses a storage certificate and a frontend certificate PERMISSION_DENIED, with its own
/// message, before the service is reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_the_pack_index_tells_a_daemon_where_blobs_are() {
    let pki = Pki::new("pack-locate-on");
    let dir = scratch_dir("pack-locate-on");
    let db = Database::new();
    let mut flags = membership_tls::tls_flags(&pki, &dir.join("state.db"), &[]);
    flags.server.state_store = Some(db.url().parse().expect("a postgres URL"));
    flags.server.pack_index = true;
    let front = start(&flags);
    let index = front.index.as_ref().expect("the index is open");
    let key = "cas/d/node-a/boot1/0000000000000001";
    let pack = NewPack {
        key: key.to_owned(),
        writer: "node-a".to_owned(),
        size: 40,
        blobs: vec![PackBlob {
            digest: BlobDigest {
                hash: [3; 32],
                size: 6,
            },
            offset: 12,
        }],
    };
    index.record_pack(&pack).expect("recorded");

    let daemon = channel(&pki, front.worker, "daemon", "node-a").await;
    let mut client = PackIndexClient::new(daemon);
    let answer = client
        .locate(ask(vec![wire(9, 4), wire(3, 6)]))
        .await
        .expect("a daemon certificate is served")
        .into_inner();
    let [unknown, known] = &answer.locations[..] else {
        panic!("two answers: {answer:?}");
    };
    assert!(!unknown.located);
    assert!(known.located);
    assert_eq!(known.pack, key);
    assert_eq!((known.offset, known.length), (12, 6));

    let over = vec![wire(3, 6); MAX_BATCH + 1];
    let answer = client.locate(ask(over)).await;
    let refused = answer.expect_err("over the bound");
    assert_eq!(refused.code(), Code::InvalidArgument);

    for role in ["storage", "frontend"] {
        let channel = channel(&pki, front.worker, role, "node-a").await;
        let refused = PackIndexClient::new(channel)
            .locate(ask(vec![wire(3, 6)]))
            .await
            .expect_err("not a daemon");
        assert_eq!(refused.code(), Code::PermissionDenied, "{role}");
        assert_eq!(refused.message(), table_refusal(role), "{role}");
    }
}

/// Asserts that a frontend started without `--pack-index` does not register the service: a
/// daemon's `Locate` is UNIMPLEMENTED, which tells the daemon to read where it did before, while
/// the role table still refuses the other roles PERMISSION_DENIED, as it does for any method it
/// lists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_without_the_pack_index_leaves_locate_unimplemented() {
    let pki = Pki::new("pack-locate-off");
    let dir = scratch_dir("pack-locate-off");
    let front = start(&membership_tls::tls_flags(&pki, &dir.join("state.db"), &[]));
    assert!(front.index.is_none());

    let daemon = channel(&pki, front.worker, "daemon", "node-a").await;
    let unserved = PackIndexClient::new(daemon)
        .locate(ask(vec![wire(3, 6)]))
        .await
        .expect_err("not registered");
    assert_eq!(unserved.code(), Code::Unimplemented);

    let storage = channel(&pki, front.worker, "storage", "node-a").await;
    let refused = PackIndexClient::new(storage)
        .locate(ask(vec![wire(3, 6)]))
        .await
        .expect_err("not a daemon");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert_eq!(refused.message(), table_refusal("storage"));
}
