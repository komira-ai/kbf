//! The service manager told `READY=1` by a `kbf frontend` (`$NOTIFY_SOCKET`), against a
//! manager socket the tests listen on: sent once the frontend serves, for a start with no
//! predecessor and for one that took over, and not held back by a storage node that is
//! down. The frontends start as the handover tests start them (`--handover-socket` and
//! `--instance`), in this process, over one state file.

use std::os::unix::net::UnixDatagram;

use kbf_server::ServeError;
use kbf_server::frontend::start_or_take_over;
use tokio::task::JoinHandle;

use super::*;

/// A directory of the test's own, removed when dropped: the state file, the control socket.
struct Dir(kbf_testkit::dir::TestDir);

impl Dir {
    fn new() -> Self {
        Self(kbf_testkit::dir::TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            "kbf-frontend",
        ))
    }
}

/// The flags of a frontend that hands over as instance `instance` (`a` or `b`), on new
/// loopback ports, with Envoy's check set to a millisecond (so the settle is three).
fn handover_flags(node: &Node, dir: &Dir, instance: &str, more: &[&str]) -> FrontendFlags {
    let mut argv = vec![
        format!("--storage=http://{}", node.addr),
        format!("--handover-socket={}/control.sock", dir.0.display()),
        format!("--instance={instance}"),
        "--handover-follow-interval-ms=10".to_owned(),
        "--heartbeat-interval-ms=100".to_owned(),
    ];
    if !more
        .iter()
        .any(|m| m.starts_with("--envoy-check-interval-ms="))
    {
        argv.extend(
            [
                "--envoy-check-interval-ms=1",
                "--envoy-check-timeout-ms=1",
                "--envoy-unhealthy-threshold=1",
            ]
            .map(str::to_owned),
        );
    }
    if !more.iter().any(|m| m.starts_with("--state-store=")) {
        argv.push(format!(
            "--state-store=sqlite://{}/state.db",
            dir.0.display()
        ));
    }
    argv.extend(more.iter().map(|m| (*m).to_owned()));
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    flags(&argv).expect("flags")
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

    /// Waits for this frontend to end, which it does once it has left Envoy's rotation
    /// and holds no stream, no relay and no caller.
    async fn ended(self) {
        let served = tokio::time::timeout(Duration::from_secs(20), self.served)
            .await
            .expect("the frontend ends")
            .expect("the task");
        served.expect("served without an error");
    }
}

async fn taking_over(
    flags: &FrontendFlags,
    stopped: oneshot::Receiver<()>,
) -> Result<FrontendHandle, Box<dyn std::error::Error>> {
    start_or_take_over(flags, || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .await
}

/// The status code `GET /readyz` of the operator API at `api` answers.
async fn readyz(api: SocketAddr) -> u16 {
    get(api, "/readyz").await.0
}

/// Asserts that `READY=1` reaches the service manager's socket only once the frontend
/// serves and its probes pass, for a start with no predecessor and for one that took over
/// alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_service_manager_is_told_when_the_frontend_serves() {
    let node = Node::start().await;
    let dir = Dir::new();
    let manager_path = dir.0.join("notify");
    let manager = UnixDatagram::bind(&manager_path).expect("a datagram socket");
    let expect_ready = |manager: &UnixDatagram| {
        manager
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        let mut buffer = [0_u8; 16];
        let got = manager.recv(&mut buffer).expect("READY=1");
        assert_eq!(&buffer[..got], b"READY=1");
    };

    let (stop, stopped) = oneshot::channel::<()>();
    let handle = taking_over(&handover_flags(&node, &dir, "a", &[]), stopped)
        .await
        .expect("a start with no predecessor");
    let handle = handle.notifying(Some(manager_path.clone().into_os_string()));
    manager.set_nonblocking(true).expect("nonblocking");
    let mut buffer = [0_u8; 16];
    assert!(manager.recv(&mut buffer).is_err(), "told before it served");
    manager.set_nonblocking(false).expect("blocking");
    let old = Proc::serving(handle, stop);
    expect_ready(&manager);

    let (stop, stopped) = oneshot::channel::<()>();
    let flags = handover_flags(&node, &dir, "b", &[]);
    let handle = taking_over(&flags, stopped).await.expect("takes over");
    let handle = handle.notifying(Some(manager_path.into_os_string()));
    manager.set_nonblocking(true).expect("nonblocking");
    assert!(manager.recv(&mut buffer).is_err(), "told before it served");
    manager.set_nonblocking(false).expect("blocking");
    let new = Proc::serving(handle, stop);
    expect_ready(&manager);
    old.ended().await;
    drop(new);
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
    let flags = handover_flags(&node, &dir, "a", &[]);
    node.stop().await;

    let (stop, stopped) = oneshot::channel::<()>();
    let handle = taking_over(&flags, stopped).await.expect("starts");
    let handle = handle.notifying(Some(manager_path.into_os_string()));
    let old = Proc::serving(handle, stop);
    manager
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let mut buffer = [0_u8; 16];
    let got = manager
        .recv(&mut buffer)
        .expect("READY=1 with the storage node down");
    assert_eq!(&buffer[..got], b"READY=1");
    assert_eq!(readyz(old.api).await, 503, "ready with no storage");
    assert!(
        std::net::TcpStream::connect(old.reapi).is_ok(),
        "the listener is gone"
    );
    assert!(!old.served.is_finished(), "the frontend ended");
}
