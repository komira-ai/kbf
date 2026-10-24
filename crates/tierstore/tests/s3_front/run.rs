//! `tierstore::server::cli::run` in this process: it serves what it is asked, until it is
//! stopped, and then drains.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use clap::Parser;
use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{ByteRange, Capabilities, ObjectKey, ObjectStore};
use tierstore::server::Drained;
use tierstore::server::cli::{Cli, run};
use tokio::sync::oneshot;

use crate::files::{BIN_KEY_ID, BIN_SECRET, Files};
use crate::signer::{Request, now, sign};
use crate::stalled::Stalled;

fn store(addr: SocketAddr) -> S3Store {
    let config = S3Config {
        endpoint: format!("http://{addr}"),
        region: "us-east-1".to_owned(),
        bucket: "tier-bucket".to_owned(),
        credentials: Credentials::new(BIN_KEY_ID, BIN_SECRET),
        capabilities: Capabilities {
            conditional_put: true,
            object_lock: false,
        },
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    };
    S3Store::new(config).unwrap()
}

/// Asserts that `run` binds the address, tells it to `ready` before it takes a request, serves
/// the S3 calls of the key in the key file, and returns, when its stop future resolves, a drain
/// with nothing cut.
#[tokio::test]
async fn run_serves_until_it_is_stopped() {
    let files = Files::new("run-serve");
    let extra = ["--drain-lb-wait", "50ms"];
    let cli = Cli::try_parse_from(files.flags(&extra)).unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, addr) = oneshot::channel::<SocketAddr>();
    let running = run(
        &cli,
        async move {
            let _ = stopped.await;
        },
        move |bound| {
            let _ = told.send(bound);
        },
    );
    let driving = async {
        let store = store(addr.await.unwrap());
        let k = ObjectKey::new("cas/run").unwrap();
        let body = Bytes::from_static(b"in process");
        store.put_new(&k, body, None).await.unwrap();
        let first = ByteRange::new(0, 2).unwrap();
        assert_eq!(store.get_range(&k, first).await.unwrap(), "in");
        tokio::time::sleep(Duration::from_millis(200)).await;
        stop.send(()).unwrap();
    };
    let (drained, ()) = tokio::join!(running, driving);
    assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
}

/// Asserts that `run` reports the requests it cut at the deadline: a request that is still in
/// flight when the deadline passes is counted in the drain it returns.
#[tokio::test]
async fn run_counts_the_requests_the_deadline_cut() {
    let files = Files::new("run-cut");
    let extra = ["--drain-lb-wait", "20ms", "--drain-deadline", "200ms"];
    let cli = Cli::try_parse_from(files.flags(&extra)).unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, addr) = oneshot::channel::<SocketAddr>();
    let running = run(
        &cli,
        async move {
            let _ = stopped.await;
        },
        move |bound| {
            let _ = told.send(bound);
        },
    );
    let driving = async {
        let addr = addr.await.unwrap();
        let body = b"0123456789";
        let host = addr.to_string();
        let request = Request {
            method: "PUT",
            host: &host,
            path: "/tier-bucket/k",
            query: &[],
            headers: &[("if-none-match", "*")],
            body,
            stamp: &now(),
        };
        let headers = sign(&request, BIN_KEY_ID, BIN_SECRET);
        let stalled = Stalled::open(addr, &headers, "/tier-bucket/k", body, 4).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        stop.send(()).unwrap();
        stalled
    };
    let (drained, stalled) = tokio::join!(running, driving);
    assert_eq!(drained.unwrap(), Drained { abandoned: 1 });
    drop(stalled);
}
