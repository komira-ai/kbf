//! The endpoints over the S3 client, which is what tells a refused connection and a 5xx (a
//! failure) from a 404 (an answer): each kind of failure of a real endpoint, with services on
//! loopback.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use kbf_objstore::s3::fake::FakeS3;
use kbf_objstore::s3::{Credentials, S3Config};
use kbf_objstore::{Capabilities, ObjectKey, ObjectStore};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use crate::{Backend, Error, S3Backend, TestClock};

use super::rank::rank;
use super::testing::FakeProbe;
use super::{Endpoints, EndpointsConfig, Passed};

const BODY: &[u8] = b"0123456789abcdefghij";

fn backend(endpoint: &str) -> Arc<dyn Backend> {
    let config = S3Config {
        endpoint: endpoint.to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "kbf-fake".to_owned(),
        credentials: Credentials::new("test-key", "test-secret"),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    };
    Arc::new(S3Backend::new(config).unwrap())
}

/// A service on loopback that answers every request with `reply`. Its URL.
async fn canned(reply: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            // The request is the headers of a GET, which the client sends in one piece.
            let mut request = [0; 8192];
            let _ = stream.read(&mut request).await;
            let _ = stream.write_all(reply.as_bytes()).await;
        }
    });
    url
}

/// The URL of a port that nothing listens on.
async fn refused() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    url
}

/// Endpoints over S3 clients of `other` and of `live`, in that order, and a key that the endpoint
/// at `first` ranks first for. When `put` is set `live` holds the key.
async fn pair(other: &str, live: &FakeS3, first: usize, put: bool) -> (Endpoints, String) {
    let names = vec![other.to_owned(), live.endpoint().to_owned()];
    let key = (0..)
        .map(|n| format!("cas/b/pack-{n}"))
        .find(|key| rank(&names, key)[0] == first)
        .unwrap();
    if put {
        let store = live.store(Capabilities {
            conditional_put: true,
            object_lock: false,
        });
        let object = ObjectKey::new(&key).unwrap();
        let body = Bytes::from_static(BODY);
        store.put_new(&object, body, None).await.unwrap();
    }
    let members = names
        .iter()
        .map(|name| (name.clone(), backend(name)))
        .collect();
    let endpoints = Endpoints::new(
        members,
        EndpointsConfig::default(),
        Arc::new(TestClock::new()),
        Arc::new(FakeProbe::new()),
    );
    (endpoints, key)
}

/// Asserts that a refused connection and a 429, a 500 and a 503 from the endpoint that ranks
/// first each fail the call over to the endpoint that holds the object, with one failed request
/// counted. It would catch a classification that took any of these for an answer.
#[tokio::test]
async fn a_refused_connection_and_a_5xx_fail_over() {
    const TOO_MANY: &str = "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\n\r\n";
    const BROKEN: &str = "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n";
    const BUSY: &str = "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n";
    let failing = [
        refused().await,
        canned(TOO_MANY).await,
        canned(BROKEN).await,
        canned(BUSY).await,
    ];
    for failing in failing {
        let live = FakeS3::start().await;
        let (endpoints, key) = pair(&failing, &live, 0, true).await;
        let body = endpoints.get(&key, None).await.unwrap();
        assert_eq!(&body[..], BODY, "{failing}");
        assert_eq!(endpoints.failovers(Passed::RequestFailed), 1, "{failing}");
    }
}

/// Asserts that a 404 of the endpoint that ranks first goes back as `NotFound`: the endpoint
/// that ranks second, which is down, is not asked (it would have turned the answer into a
/// failure), and nothing is counted.
#[tokio::test]
async fn a_404_is_final_over_the_s3_client() {
    let live = FakeS3::start().await;
    let down = refused().await;
    let (endpoints, key) = pair(&down, &live, 1, false).await;
    let error = endpoints.get(&key, None).await.unwrap_err();
    assert_eq!(error, Error::NotFound(key));
    assert_eq!(endpoints.failovers(Passed::RequestFailed), 0);
    assert_eq!(endpoints.unavailable(), 0);
}
