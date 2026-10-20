//! The version `Welcome` names: the lesser of the `Hello`'s and the newest the server
//! speaks, when the `Hello`'s is not below the oldest. The real server speaks only
//! version 1, so the range is also tested on a service that speaks 1 to 3.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_front::Cache;
use kbf_proto::worker::server_message::Message;
use kbf_proto::worker::{Hello, daemon_message, worker_server::WorkerServer};
use kbf_server::worker::WorkerService;
use kbf_server::{Farm, Peers};
use support::{Cell, FakeDaemon, hello};
use tonic::Code;

fn hello_of(version: u32) -> Hello {
    Hello {
        protocol_version: version,
        ..hello("node-a", 4, 8)
    }
}

/// The version of the `Welcome` a daemon that says `version` gets from the server at
/// `addr`, or the code it is refused with.
async fn welcomed(addr: std::net::SocketAddr, version: u32) -> Result<u32, Code> {
    let first = daemon_message::Message::Hello(hello_of(version));
    let mut daemon = FakeDaemon::open(addr, first)
        .await
        .map_err(|status| status.code())?;
    let welcome = daemon
        .expect("Welcome", |m| match m {
            Message::Welcome(w) => Some(w.protocol_version),
            _ => None,
        })
        .await;
    Ok(welcome)
}

/// Catches: a server that refuses a daemon newer than itself (a daemon update ahead of
/// its frontend would be locked out of the farm, with no way back but a downgrade), and
/// one that answers it with the daemon's own version, which this server does not speak.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_newer_than_the_server_is_welcomed_at_the_servers_version() {
    let cell = Cell::start().await;
    assert_eq!(welcomed(cell.worker_addr, 2).await, Ok(1));
    assert_eq!(welcomed(cell.worker_addr, u32::MAX).await, Ok(1));
    assert_eq!(welcomed(cell.worker_addr, 1).await, Ok(1));
}

/// A service speaking versions 1 to 3 over an empty farm, on a port of its own.
fn serve_one_to_three() -> (std::net::SocketAddr, Arc<Farm>) {
    let farm = Arc::new(Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    ));
    let service = WorkerService::new(
        Arc::clone(&farm),
        Peers::Unauthenticated,
        support::INTERVAL,
        Duration::from_secs(5),
    )
    .with_protocols(1, 3);
    let incoming =
        tonic::transport::server::TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(WorkerServer::new(service))
            .serve_with_incoming(incoming),
    );
    (addr, farm)
}

/// Catches: a negotiation that echoes the `Hello` for every version (a daemon of version
/// 5 told 5 by a server that speaks 3), one that answers every version with the newest (a
/// daemon of version 1 refuses a Welcome of 3), and a floor that moved: version 0, below
/// the oldest, is refused FAILED_PRECONDITION.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_speaking_one_to_three_welcomes_the_lesser_version() {
    let (addr, _farm) = serve_one_to_three();
    for (hello, welcome) in [(1, 1), (2, 2), (3, 3), (5, 3)] {
        assert_eq!(welcomed(addr, hello).await, Ok(welcome), "hello {hello}");
    }
    assert_eq!(welcomed(addr, 0).await, Err(Code::FailedPrecondition));
}

/// Catches: a node recorded as speaking the version its `Hello` named, not the one
/// `Welcome` named: a daemon whose `Hello` names 5 against a server speaking 3 is
/// answered 3, and `GET /v1/nodes` (`Farm::node_view`) must say 3, not 5, or the
/// rolling-update check (R1) counts a version the stream does not speak.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_is_listed_at_the_version_welcome_named() {
    let (addr, farm) = serve_one_to_three();
    let first = daemon_message::Message::Hello(hello_of(5));
    let _daemon = FakeDaemon::open(addr, first).await.expect("registered");
    let node = kbf_types::WorkerId::new("node-a");
    let mut listed = None;
    for _ in 0..100 {
        listed = farm.node_view(&node);
        if listed.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(listed.expect("registered").protocol, Some(3));
}
