//! The service manager told `READY=1` by a `kbf frontend` (`$NOTIFY_SOCKET`), against a
//! manager socket the tests listen on: sent once the frontend serves, and not held back by
//! a storage node that is down. The frontends start in this process.

use std::os::unix::net::UnixDatagram;

use kbf_server::ServeError;
use tokio::task::JoinHandle;

use super::*;

/// A directory of the test's own, removed when dropped: the manager's socket.
struct Dir(kbf_testkit::dir::TestDir);

impl Dir {
    fn new() -> Self {
        Self(kbf_testkit::dir::TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            "kbf-frontend",
        ))
    }
}

/// A frontend serving in this process.
struct Proc {
    reapi: SocketAddr,
    api: SocketAddr,
    /// Dropped to stop the frontend.
    _stop: oneshot::Sender<()>,
    served: JoinHandle<Result<(), ServeError>>,
}

impl Proc {
    fn serving(handle: FrontendHandle, stop: oneshot::Sender<()>) -> Self {
        let (reapi, api) = (handle.reapi, handle.api);
        Self {
            reapi,
            api: api.expect("the operator API"),
            _stop: stop,
            served: tokio::spawn(handle.serve()),
        }
    }
}

/// The frontend `flags` describe, stopped when `stopped` completes.
fn starting(
    flags: &FrontendFlags,
    stopped: oneshot::Receiver<()>,
) -> Result<FrontendHandle, Box<dyn std::error::Error>> {
    start_frontend(flags, || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
}

/// The status code `GET /readyz` of the operator API at `api` answers.
async fn readyz(api: SocketAddr) -> u16 {
    get(api, "/readyz").await.0
}

/// Asserts that `READY=1` reaches the service manager's socket only once the frontend
/// serves and its probes pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_service_manager_is_told_when_the_frontend_serves() {
    let node = Node::start().await;
    let dir = Dir::new();
    let manager_path = dir.0.join("notify");
    let manager = UnixDatagram::bind(&manager_path).expect("a datagram socket");
    let url = format!("--storage=http://{}", node.addr);
    let front = flags(&[&url, "--heartbeat-interval-ms=100"]).expect("flags");

    let (stop, stopped) = oneshot::channel::<()>();
    let handle = starting(&front, stopped).expect("starts");
    let handle = handle.notifying(Some(manager_path.into_os_string()));
    manager.set_nonblocking(true).expect("nonblocking");
    let mut buffer = [0_u8; 16];
    assert!(manager.recv(&mut buffer).is_err(), "told before it served");
    manager.set_nonblocking(false).expect("blocking");
    let serving = Proc::serving(handle, stop);
    manager
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let got = manager.recv(&mut buffer).expect("READY=1");
    assert_eq!(&buffer[..got], b"READY=1");
    drop(serving);
}

/// Asserts that, with the `--storage` node stopped before the frontend starts (as at a cold
/// power-up where the node comes up after the frontend), the manager still gets `READY=1`
/// (a frontend that is never ready is killed at `TimeoutStartSec` and restarts with a new
/// epoch, in a loop), the listeners stay bound (the same REAPI address answers), `/readyz`
/// says 503 and the frontend keeps serving.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ready_does_not_wait_for_a_storage_node_that_is_down() {
    let mut node = Node::start().await;
    let dir = Dir::new();
    let manager_path = dir.0.join("notify");
    let manager = UnixDatagram::bind(&manager_path).expect("a datagram socket");
    let url = format!("--storage=http://{}", node.addr);
    let front = flags(&[&url, "--heartbeat-interval-ms=100"]).expect("flags");
    node.stop().await;

    let (stop, stopped) = oneshot::channel::<()>();
    let handle = starting(&front, stopped).expect("starts");
    let handle = handle.notifying(Some(manager_path.into_os_string()));
    let serving = Proc::serving(handle, stop);
    manager
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let mut buffer = [0_u8; 16];
    let got = manager
        .recv(&mut buffer)
        .expect("READY=1 with the storage node down");
    assert_eq!(&buffer[..got], b"READY=1");
    assert_eq!(readyz(serving.api).await, 503, "ready with no storage");
    assert!(
        std::net::TcpStream::connect(serving.reapi).is_ok(),
        "the listener is gone"
    );
    assert!(!serving.served.is_finished(), "the frontend ended");
}
