//! The binary's `run`, in this process: serving, and failing to start. (A child killed to
//! stop it writes no coverage data, so the start path is also walked here.)

mod support;

use std::process::ExitCode;
use std::time::{Duration, Instant};

use kbf_ca::{Algorithm, Authority, Policy, Signer};
use kbf_signer::{Flags, Route, SocketSigner, run};
use support::{euid, real_now, scratch};
use x509_parser::prelude::{FromDer, X509Certificate};

fn flags(scratch: &std::path::Path, socket: &std::path::Path) -> Flags {
    Flags {
        ca_dir: scratch.join("ca"),
        socket: socket.to_path_buf(),
        frontend_uid: euid(),
        lifetime_secs: Policy::DEFAULT_LIFETIME_S,
    }
}

// Catches: a start path that only works under a subprocess: `run` on a fresh directory
// makes the CA, binds, and serves a client that gets the CA's key and a signature valid
// under it; a second `run` on the same socket (the first still serving, holding the lock)
// fails with a message and a failure exit, touching nothing.
#[test]
fn run_serves_a_fresh_ca_and_a_second_run_on_its_socket_fails() {
    let scratch = scratch("run_in_process");
    let socket = scratch.join("run").join("sock");
    let served = flags(&scratch, &socket);
    std::thread::spawn({
        let served = served.clone();
        move || run(&served)
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let client = loop {
        match SocketSigner::connect(&socket, Route::Node) {
            Ok(client) => break client,
            Err(e) => assert!(Instant::now() < deadline, "never came up: {e}"),
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let ca = Authority::load_dir(&served.ca_dir, Policy::default()).expect("the CA it made");
    let root = ca.root_der().to_vec();
    let (_, cert) = X509Certificate::from_der(&root).expect("root");
    assert_eq!(client.public_key(), cert.public_key().raw);
    let made = support::Ca {
        dir: served.ca_dir.clone(),
        root: root.clone(),
        spki: client.public_key().to_vec(),
        root_subject: cert.subject().as_raw().to_vec(),
        scratch: scratch.clone(),
    };
    let tbs = support::node_tbs_at(&made, kbf_types::Role::Daemon, None, real_now());
    let signature = client
        .sign(&tbs, Algorithm::EcdsaP256Sha256)
        .expect("signed by the clock it reads");
    assert!(!signature.is_empty());

    // The same socket again: the lock is held by the first.
    assert_eq!(run(&served), ExitCode::FAILURE);
}
