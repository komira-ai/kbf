//! Two starters of one socket: exactly one binds, whatever the interleaving.

mod support;

use std::os::unix::net::UnixListener;
use std::sync::{Arc, Barrier};

use kbf_signer::{SignerError, bind};
use support::scratch;

/// Rounds of the race. Without the lock the race let both starters in about once in 300
/// rounds on a checker's box; 1500 makes a red run on the unlocked code likely.
const ROUNDS: usize = 1500;

// Catches: the check-then-act race in `bind` (connect, remove, bind with nothing between
// two starters): both starters found the same stale socket, both removed and bound, and
// the first listener was orphaned. Each round leaves a stale socket, releases two
// starters together, and holds both results until both are in: exactly one may be Ok and
// the other must be AlreadyServing.
#[test]
fn two_starters_on_a_stale_socket_never_both_bind() {
    let dir = scratch("bind_race");
    let socket = dir.join("run").join("sock");
    for round in 0..ROUNDS {
        // The previous round's winner is dropped by now: its socket file is stale. The
        // first round makes one.
        if !socket.exists() {
            std::fs::create_dir_all(socket.parent().expect("dir")).expect("dir");
            std::fs::set_permissions(
                socket.parent().expect("dir"),
                std::os::unix::fs::PermissionsExt::from_mode(0o700),
            )
            .expect("chmod");
            drop(UnixListener::bind(&socket).expect("a stale socket"));
        }
        let barrier = Arc::new(Barrier::new(2));
        let starters: Vec<_> = (0..2)
            .map(|_| {
                let (barrier, socket) = (barrier.clone(), socket.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    bind(&socket)
                })
            })
            .collect();
        let results: Vec<_> = starters
            .into_iter()
            .map(|t| t.join().expect("starter"))
            .collect();
        let won = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(won, 1, "round {round}: {results:?}");
        let lost = results
            .iter()
            .find_map(|r| r.as_ref().err())
            .expect("loser");
        assert!(
            matches!(lost, SignerError::AlreadyServing(_)),
            "round {round}: {lost}"
        );
    }
}
