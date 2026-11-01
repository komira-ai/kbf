//! The pack store flags on a started frontend over a real Postgres server and an S3 service on
//! loopback: an upload through the frontend lands in a pack of the S3 service and in the
//! Postgres pack index before it is acknowledged, and the index answers a lookup of it.
//!
//! The tests need a server and fail without one: `KBF_TEST_POSTGRES_URL` names a server they
//! may create and drop databases on. Each test makes a database of its own.

use std::future::pending;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use kbf_objstore::s3::fake::FakeS3;
use kbf_pack_index::BlobDigest;
use kbf_pack_index::schema::test_database;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{self, BatchUpdateBlobsRequest, batch_update_blobs_request};
use kbf_server::frontend::{self, FrontendFlags, start_frontend};
use kbf_state::postgres::TestDatabase;
use kbf_testkit::dir::TestDir;

/// The flags of a frontend on loopback ports over the state store at `url`, with the pack index
/// and a pack store at `endpoint` signed for with the key pair in `credentials`.
fn flags(url: &str, endpoint: &str, credentials: &Path) -> FrontendFlags {
    let argv = [
        "frontend".to_owned(),
        "--storage=http://127.0.0.1:1".to_owned(),
        "--listen=127.0.0.1:0".to_owned(),
        "--worker-listen=127.0.0.1:0".to_owned(),
        "--api-listen=127.0.0.1:0".to_owned(),
        // Parsed only: the URL of the test database holds a password, which the flag refuses.
        "--state-store=postgres:///kbf?host=/run/postgresql".to_owned(),
        "--pack-index".to_owned(),
        format!("--pack-store-endpoint={endpoint}"),
        "--pack-store-bucket=kbf-fake".to_owned(),
        format!("--pack-store-credentials-file={}", credentials.display()),
        "--pack-writer=fe2".to_owned(),
    ];
    let parsed = frontend::command().try_get_matches_from(argv);
    let matches = parsed.expect("flags");
    let mut flags = frontend::parse(&matches).expect("a frontend's flags");
    flags.server.state_store = Some(url.parse().expect("a postgres URL"));
    flags
}

/// Runs `f` on a thread of its own and returns its result. A test database makes a runtime of
/// its own to make and to drop a database, and tokio refuses that on a thread that is running
/// tasks.
fn off_runtime<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|scope| match scope.spawn(f).join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    })
}

/// A database of its own on the test server, made and dropped off the runtime (see
/// [`off_runtime`]), so that it is also dropped when a test fails.
struct Database(Option<TestDatabase>);

impl Database {
    fn new(make: impl FnOnce() -> TestDatabase + Send) -> Self {
        Self(Some(off_runtime(make)))
    }

    /// The URL of the database.
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

/// Asserts that a frontend given the pack store flags over Postgres writes an upload into one
/// pack of the store and into the Postgres pack index: once the call is answered the index
/// locates the blob, in the pack the store holds, at the blob's length.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upload_lands_in_a_pack_and_in_the_postgres_index() {
    let db = Database::new(test_database);
    let s3 = FakeS3::start().await;
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "pack-store-flags");
    let credentials = dir.path().join("pack.json");
    let keys_json = r#"{"access_key_id": "k", "secret_access_key": "s"}"#;
    std::fs::write(&credentials, keys_json).expect("written");
    let private = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(&credentials, private).expect("mode set");
    let started = flags(&db.url(), s3.endpoint(), &credentials);
    let handle = start_frontend(&started, || Ok(pending::<()>())).expect("starts");
    let index = handle.pack_index().expect("the index is open");
    let front = handle.reapi;
    let served = tokio::spawn(handle.serve());
    let bytes = b"a blob for the postgres index";
    let digest = kbf_segments::sha256(bytes);
    let request = BatchUpdateBlobsRequest {
        requests: vec![batch_update_blobs_request::Request {
            digest: Some(kbf_front::digest_to_proto(&digest)),
            data: bytes.to_vec(),
            compressor: reapi::compressor::Value::Identity as i32,
        }],
        ..BatchUpdateBlobsRequest::default()
    };
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{front}"));
    let endpoint = endpoint.expect("endpoint");
    let channel = endpoint.connect().await.expect("connects");

    let answered = ContentAddressableStorageClient::new(channel)
        .batch_update_blobs(request)
        .await
        .expect("answered");

    let status = answered.into_inner().responses[0].status.clone();
    assert_eq!(status.expect("a status").code, 0);
    let keys = s3.keys();
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert!(keys[0].starts_with("cas/f/fe2/"), "{keys:?}");
    let asked = BlobDigest {
        hash: digest.hash,
        size: digest.size_bytes,
    };
    let located = index.locate(&[asked]).expect("located");
    let place = located[0].as_ref().expect("a row");
    assert_eq!(place.pack_key, keys[0]);
    assert_eq!(place.length, bytes.len() as u64);
    served.abort();
}
