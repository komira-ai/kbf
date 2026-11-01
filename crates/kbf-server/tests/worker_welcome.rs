//! The version `Welcome` names: the lesser of the `Hello`'s and the newest the server
//! speaks, when the `Hello`'s is not below the oldest. The real server speaks only
//! version 1, so the range is also tested on a service that speaks 1 to 3.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_front::Cache;
use kbf_proto::worker::server_message::Message;
use kbf_proto::worker::{Hello, daemon_message, worker_server::WorkerServer};
use kbf_server::PlacementLoad;
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

/// A service over an empty farm that says `on` in `Welcome.pack_commit`, on a port of its own.
fn serve_saying(on: bool) -> std::net::SocketAddr {
    let farm = Arc::new(Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    ));
    let service = WorkerService::new(
        farm,
        Peers::Unauthenticated,
        support::INTERVAL,
        Duration::from_secs(5),
    );
    let service = service.with_pack_commit(on);
    let incoming =
        tonic::transport::server::TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(WorkerServer::new(service))
            .serve_with_incoming(incoming),
    );
    addr
}

/// The `pack_commit` of the `Welcome` that the server at `addr` sends a daemon.
async fn says_commit(addr: std::net::SocketAddr) -> bool {
    let first = daemon_message::Message::Hello(hello_of(1));
    let mut daemon = FakeDaemon::open(addr, first).await.expect("opened");
    daemon
        .expect("Welcome", |m| match m {
            Message::Welcome(w) => Some(w.pack_commit),
            _ => None,
        })
        .await
}

/// Asserts that `Welcome` says whether the frontend takes `CommitPack`: false from a service that
/// was not told it does (the plain server of a cell among them), true from one that was, so that
/// a daemon that writes packs commits only to a frontend that can check them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn welcome_says_whether_the_frontend_takes_commits() {
    let cell = Cell::start().await;

    assert!(!says_commit(cell.worker_addr).await);
    assert!(!says_commit(serve_saying(false)).await);
    assert!(says_commit(serve_saying(true)).await);
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

/// A service over an empty farm told `mode` (`--placement-load`), on a port of its own.
fn serve_placing(mode: PlacementLoad) -> std::net::SocketAddr {
    let farm = Arc::new(Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    ));
    let service = WorkerService::new(
        farm,
        Peers::Unauthenticated,
        support::INTERVAL,
        Duration::from_secs(5),
    )
    .with_placement_load(mode);
    let incoming =
        tonic::transport::server::TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(WorkerServer::new(service))
            .serve_with_incoming(incoming),
    );
    addr
}

/// The `Welcome` the server at `addr` sends a daemon.
async fn welcome_of(addr: std::net::SocketAddr) -> kbf_proto::worker::Welcome {
    let first = daemon_message::Message::Hello(hello_of(1));
    let mut daemon = FakeDaemon::open(addr, first).await.expect("opened");
    daemon
        .expect("Welcome", |m| match m {
            Message::Welcome(w) => Some(w.clone()),
            _ => None,
        })
        .await
}

/// Asserts that `Welcome.placement_load` is true for `--placement-load=on` and absent (not
/// false) for `off` and a service told nothing, so that a farm that does not place by
/// measured load sends the bytes it sent before the field (the bytes of an absent field are
/// pinned in kbf-proto's `welcome_placement_load`). Catches `off` sent as `Some(false)` and
/// the default being on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn welcome_says_placement_load_only_for_on() {
    let cell = Cell::start().await;
    assert_eq!(welcome_of(cell.worker_addr).await.placement_load, None);
    for (mode, says) in [(PlacementLoad::Off, None), (PlacementLoad::On, Some(true))] {
        let welcome = welcome_of(serve_placing(mode)).await;
        assert_eq!(welcome.placement_load, says, "{mode:?}");
    }
}
