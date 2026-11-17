//! A server whose blobs are in a real S3 store: a client uploads an action's blobs over REAPI, the
//! server answers that it holds them, and the bytes are in the bucket.
//!
//! Ignored by default, because it needs a store. CI's `objstore-s3` job runs it against
//! MinIO and RustFS: `cargo test -p kbf-server --test s3 -- --ignored`, with
//! `KBF_TEST_S3_ENDPOINT` naming the store and the standard `AWS_ACCESS_KEY_ID` and
//! `AWS_SECRET_ACCESS_KEY` holding its throwaway key pair.

mod support;

use std::future::pending;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Cache, MemoryMetaLog};
use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{Capabilities, KeyPrefix, ObjectStore, PageSize};
use kbf_server::{Listeners, bind_server};
use support::{Client, Job};

const BUCKET: &str = "kbf-server-e2e";

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for this test"))
}

/// A handle on the bucket at `endpoint`.
fn store(endpoint: &str) -> S3Store {
    S3Store::new(S3Config {
        endpoint: endpoint.to_owned(),
        region: "us-east-1".to_owned(),
        bucket: BUCKET.to_owned(),
        credentials: Credentials::new(env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY")),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    })
    .expect("store")
}

fn listeners() -> Listeners {
    let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
    Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: None,
        executor: None,
        finished_retention: kbf_server::executor::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

/// Catches: an S3 store that does not reach the cache (blobs kept in memory, or nowhere).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs an S3 store; see the module docs"]
async fn blobs_uploaded_to_a_server_over_s3_are_in_the_bucket() {
    let endpoint = env("KBF_TEST_S3_ENDPOINT");
    let bucket = store(&endpoint);
    bucket.create_bucket().await.expect("create the bucket");

    // The server runs in this process, over the S3 store.
    let prefix = KeyPrefix::new("e2e/").expect("prefix");
    let cache = Cache::open(
        MemoryMetaLog::new(kbf_meta::Retention::default()),
        store(&endpoint),
        prefix,
    )
    .await
    .expect("open the cache");
    let bound = bind_server(Arc::new(cache), listeners(), pending()).expect("bind");
    let (reapi, worker) = (bound.reapi, bound.worker);
    tokio::spawn(async move { bound.serving.await.expect("serve") });
    let client = Client::connect(reapi, worker).await;
    let job = Job::new("on s3", &[]);
    client.upload(&job.blobs()).await;
    for blob in job.blobs() {
        assert!(
            client.holds(blob).await,
            "the server serves what it was sent"
        );
    }

    let prefix = KeyPrefix::new("e2e/").expect("prefix");
    let page = bucket
        .list(&prefix, None, PageSize::new(10).expect("page size"))
        .await
        .expect("list");
    assert!(!page.objects.is_empty(), "no blob reached the bucket");
}
