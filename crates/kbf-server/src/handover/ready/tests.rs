//! The frontend telling the service manager it serves, against a manager the test plays.

use std::os::unix::net::UnixDatagram;

use super::*;
use crate::handover::tests::Scratch;

/// Catches: `READY=1` never sent, and a manager that is not there (no `$NOTIFY_SOCKET`,
/// or a socket nobody listens on) stopping the frontend from running. That it is not
/// held back by `/readyz` is pinned by `ready_is_sent_while_the_storage_node_does_not_answer`
/// in `tests/frontend/handover.rs`.
#[tokio::test]
async fn ready_tells_the_manager_and_survives_its_absence() {
    let dir = Scratch::new();
    let path = dir.0.join("notify");
    let manager = UnixDatagram::bind(&path).expect("a datagram socket");
    Ready {
        notify: Some(path.into_os_string()),
    }
    .run()
    .await;
    let mut buffer = [0_u8; 32];
    let got = manager.recv(&mut buffer).expect("READY=1");
    assert_eq!(&buffer[..got], b"READY=1");

    for notify in [None, Some(dir.0.join("nobody-listens").into_os_string())] {
        Ready { notify }.run().await;
    }
}
