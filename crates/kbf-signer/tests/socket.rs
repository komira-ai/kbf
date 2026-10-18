//! The socket: who may connect, what a client gets back, and the files it sits in.

mod support;

use std::io::{Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;

use kbf_ca::{Algorithm, Authority, CaError, Custody, FileSigner, Policy, SignError, Signer};
use kbf_signer::{Route, SignerError, SignerServer, SocketSigner, bind, harden};
use kbf_types::Role;
use support::{NOW, Stream, euid, make_ca, scratch, serve};
use x509_parser::prelude::{FromDer, X509Certificate};

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o7777
}

/// A connection that sends `request` raw and reads whatever comes back until it closes.
fn raw(socket: &Path, request: &[u8]) -> Vec<u8> {
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream.write_all(request).expect("write");
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("shutdown");
    let mut answer = Vec::new();
    // A reset after the server closed is the same as a close for what these tests ask.
    let _ = stream.read_to_end(&mut answer);
    answer
}

fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(body.len() + 1)
        .expect("len")
        .to_be_bytes()
        .to_vec();
    out.push(tag);
    out.extend_from_slice(body);
    out
}

// Catches: SO_PEERCRED not checked, so any account that can reach the socket gets
// signatures. The peer's real credentials are read by the kernel; with no second account
// in CI, the test configures the signer for a user id that is not the test's own, which
// makes the connecting peer (this process) "another uid" exactly as a stranger would be.
// The control run, same code with the test's own uid configured, is served.
#[test]
fn a_peer_of_another_uid_is_refused_and_the_configured_uid_is_served() {
    let ca = make_ca("peer");
    let mine = serve(&ca, euid());
    SocketSigner::connect(&mine, Route::Node).expect("the configured uid is served");

    let theirs = serve(&ca, euid().wrapping_add(1));
    let refused = SocketSigner::connect(&theirs, Route::Node).expect_err("another uid is refused");
    assert!(refused.0.contains("signer socket"), "{refused}");
    // And the raw request gets no answer at all: not a refusal text, nothing.
    assert!(raw(&theirs, &frame(0, &[])).is_empty());
}

// Catches: a signature made with the wrong key or algorithm: the certificate the frontend
// builds through the socket must verify against the root, and carry the key and role
// asked for.
#[test]
fn a_certificate_signed_through_the_socket_verifies_against_the_root() {
    let ca = make_ca("verify");
    let socket = serve(&ca, euid());
    let client = Arc::new(SocketSigner::connect(&socket, Route::Node).expect("connect"));
    assert_eq!(client.public_key(), ca.spki.as_slice());
    let authority = Authority::import(client, Policy::default(), &ca.root).expect("authority");
    let id = "node-a".parse().expect("id");
    let key = support::fresh_key();
    let issued = authority
        .issue_node(
            &kbf_ca::NodeCert {
                node_id: &id,
                role: Role::Storage,
                key: &key,
                advertise: None,
            },
            NOW,
            &mut Stream(3),
        )
        .expect("issued through the socket");
    let (_, leaf) = X509Certificate::from_der(&issued.der).expect("leaf");
    let (_, root) = X509Certificate::from_der(&ca.root).expect("root");
    leaf.verify_signature(Some(root.public_key()))
        .expect("verifies under the root");
    assert_eq!(leaf.public_key().raw, key.spki());
    // Another CA's root does not verify it: the check above is not vacuous.
    let other = make_ca("verify_other");
    let (_, other_root) = X509Certificate::from_der(&other.root).expect("root");
    assert!(
        leaf.verify_signature(Some(other_root.public_key()))
            .is_err()
    );
}

// Catches: the frontend's own certificate unreachable (the frontend route), and the
// frontend route used to mint a node certificate.
#[test]
fn the_frontend_route_signs_the_frontend_certificate_and_only_that() {
    let ca = make_ca("frontend");
    let socket = serve(&ca, euid());
    let client = Arc::new(SocketSigner::connect(&socket, Route::Frontend).expect("connect"));
    let authority = Authority::import(client.clone(), Policy::default(), &ca.root).expect("a");
    let issued = authority
        .issue_frontend(
            &["frontend.example"],
            &support::fresh_key(),
            NOW,
            &mut Stream(4),
        )
        .expect("the frontend certificate");
    let (_, leaf) = X509Certificate::from_der(&issued.der).expect("leaf");
    let (_, root) = X509Certificate::from_der(&ca.root).expect("root");
    leaf.verify_signature(Some(root.public_key()))
        .expect("verifies");
    let id = "node-a".parse().expect("id");
    let key = support::fresh_key();
    let node = authority.issue_node(
        &kbf_ca::NodeCert {
            node_id: &id,
            role: Role::Daemon,
            key: &key,
            advertise: None,
        },
        NOW,
        &mut Stream(5),
    );
    match node {
        Err(CaError::Build(_)) => {}
        other => panic!("the frontend route signed a node certificate: {other:?}"),
    }
}

// Catches: a signer that signs whatever it is sent: a CA certificate, a frontend-role
// certificate on the node route, and an over-long lifetime each come back refused with
// the reason, over the real socket, and the signer keeps serving afterwards.
#[test]
fn the_socket_refuses_ca_frontend_and_long_lifetimes_and_keeps_serving() {
    let ca = make_ca("evil");
    let socket = serve(&ca, euid());
    let client = SocketSigner::connect(&socket, Route::Node).expect("connect");
    let sign = |tbs: &[u8]| client.sign(tbs, Algorithm::EcdsaP256Sha256);

    let root = sign(&support::root_tbs(&ca)).expect_err("CA:TRUE");
    assert!(
        root.0.contains("refused") && root.0.contains("CA:FALSE"),
        "{root}"
    );

    let frontend = sign(&support::frontend_tbs(&ca)).expect_err("frontend role");
    assert!(frontend.0.contains("frontend role"), "{frontend}");

    let mut long = support::Evil::new(&ca);
    long.params.not_after = long.params.not_before + time::Duration::days(3650);
    let long = sign(&long.tbs()).expect_err("ten years");
    assert!(long.0.contains("longer than the policy"), "{long}");

    sign(&support::node_tbs(&ca, Role::Daemon, None)).expect("a table certificate is signed");
}

// Catches: a client that is not an external signer (so a production frontend would accept
// a key in its own memory), and a file signer that passes the production gate.
#[test]
fn the_socket_client_is_external_custody_and_the_file_signer_is_not() {
    let ca = make_ca("custody");
    let socket = serve(&ca, euid());
    let client = SocketSigner::connect(&socket, Route::Node).expect("connect");
    assert_eq!(client.custody(), Custody::External);
    let over_socket = Authority::import(Arc::new(client), Policy::default(), &ca.root).expect("a");
    over_socket
        .require_external()
        .expect("the production gate passes the socket signer");
    let in_process = Authority::load_dir(&ca.dir, Policy::default()).expect("load");
    assert!(matches!(
        in_process.require_external(),
        Err(CaError::InProcessSigner)
    ));
}

// Catches: frames the server believes: a length over the limit or zero (memory and hang
// attacks) answered with anything; an unknown request answered as a success; a peer that
// sends half a frame and leaves, which must not wedge or kill the server.
#[test]
fn bad_frames_get_no_answer_and_unknown_requests_fail() {
    let ca = make_ca("frames");
    let socket = serve(&ca, euid());
    assert!(raw(&socket, &(1_000_000u32).to_be_bytes()).is_empty());
    assert!(raw(&socket, &0u32.to_be_bytes()).is_empty());
    assert!(raw(&socket, &[0, 0]).is_empty(), "half a length");
    assert!(raw(&socket, &[0, 0, 0, 9, 1]).is_empty(), "half a body");
    let unknown = raw(&socket, &frame(77, b"x"));
    assert_eq!(unknown[4], 2, "tag FAILED");
    assert_eq!(&unknown[5..], b"unknown request");
    SocketSigner::connect(&socket, Route::Node).expect("still serving");
}

// Catches: a client that trusts what the socket says: a server that sends a key that is
// not P-256, one that answers with a refusal or failure, one that vanishes, and a body
// too long to send.
#[test]
fn the_client_distrusts_the_socket() {
    let scratch = scratch("client");
    let socket = scratch.join("s");
    let listener = UnixListener::bind(&socket).expect("bind");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.expect("accept");
            let mut head = [0u8; 5];
            stream.read_exact(&mut head).expect("request head");
            let mut rest =
                vec![0u8; u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize - 1];
            stream.read_exact(&mut rest).expect("request body");
            // Not a key: the answer to the public key request.
            stream.write_all(&frame(0, b"not a key")).expect("answer");
        }
    });
    let bad = SocketSigner::connect(&socket, Route::Node).expect_err("a bad key");
    assert!(bad.0.contains("ECDSA"), "{bad}");

    let missing = SocketSigner::connect(scratch.join("none"), Route::Node).expect_err("no socket");
    assert!(missing.0.contains("signer socket"), "{missing}");

    // A body over the frame limit never leaves the client.
    let ca = make_ca("client_long");
    let real = serve(&ca, euid());
    let client = SocketSigner::connect(&real, Route::Node).expect("connect");
    let long = client
        .sign(&vec![0u8; 5000], Algorithm::EcdsaP256Sha256)
        .expect_err("too long");
    assert!(long.0.contains("signer socket"), "{long}");
}

struct Failing(Vec<u8>);

impl Signer for Failing {
    fn public_key(&self) -> &[u8] {
        &self.0
    }

    fn custody(&self) -> Custody {
        Custody::External
    }

    fn sign(&self, _: &[u8], _: Algorithm) -> Result<Vec<u8>, SignError> {
        Err(SignError("hardware module offline".to_owned()))
    }
}

// Catches: a failing key reported as a success (an empty or partial signature in a
// certificate), and a failure reported as a refusal of the request.
#[test]
fn a_failing_key_is_a_failure_not_a_signature() {
    let ca = make_ca("failing");
    let server = SignerServer::new(
        Failing(ca.spki.clone()),
        &ca.root,
        euid(),
        Policy::default(),
    )
    .expect("server");
    assert_eq!(format!("{server:?}"), "SignerServer", "no key in Debug");
    let socket = ca.scratch.join("f").join("sock");
    let listener = bind(&socket).expect("bind");
    std::thread::spawn(move || {
        let _ = server.serve(&listener);
    });
    let client = SocketSigner::connect(&socket, Route::Node).expect("connect");
    let err = client
        .sign(
            &support::node_tbs(&ca, Role::Daemon, None),
            Algorithm::EcdsaP256Sha256,
        )
        .expect_err("fails");
    assert!(
        err.0.contains("failed") && err.0.contains("hardware module offline"),
        "{err}"
    );
}

// Catches: a server started with a root that is not its key's (it would sign under a name
// whose certificate does not verify), or with garbage for a root.
#[test]
fn the_root_must_be_the_keys() {
    let ca = make_ca("root");
    let other = make_ca("root_other");
    let wrong = SignerServer::new(
        FileSigner::load(&ca.dir).expect("key"),
        &other.root,
        euid(),
        Policy::default(),
    );
    assert!(matches!(wrong, Err(SignerError::Root)));
    let garbage = SignerServer::new(
        FileSigner::load(&ca.dir).expect("key"),
        b"not a certificate",
        euid(),
        Policy::default(),
    );
    assert!(matches!(garbage, Err(SignerError::Root)));
}

// Catches: from_dir loading a key or directory that is open to others.
#[test]
fn from_dir_refuses_a_key_open_to_others() {
    let ca = make_ca("open_key");
    std::fs::set_permissions(
        ca.dir.join("ca.key"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("chmod");
    let err = SignerServer::from_dir(&ca.dir, euid(), Policy::default()).expect_err("open key");
    assert!(matches!(err, SignerError::Ca(CaError::KeyFile(_))), "{err}");
}

// Catches: a socket other accounts can reach: the socket must be 0600 in a 0700 directory
// (made as such when missing), a symlinked or open directory refused, a stale socket
// replaced, and a regular file at the path left alone.
#[test]
fn the_socket_is_closed_to_others() {
    let scratch = scratch("bind");
    let socket = scratch.join("run").join("sock");
    let first = bind(&socket).expect("bind");
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(socket.parent().expect("dir")), 0o700);
    drop(first);
    // The socket file stays after its listener closes: a stale socket, replaced.
    let second = bind(&socket).expect("a stale socket is replaced");
    drop(second);

    std::fs::write(scratch.join("run").join("file"), b"x").expect("write");
    let over_file = bind(&scratch.join("run").join("file"));
    assert!(matches!(over_file, Err(SignerError::SocketPath)));
    assert!(scratch.join("run").join("file").exists());

    let open = scratch.join("open");
    std::fs::create_dir(&open).expect("dir");
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert!(matches!(
        bind(&open.join("sock")),
        Err(SignerError::SocketDir)
    ));

    symlink(scratch.join("run"), scratch.join("link")).expect("symlink");
    assert!(matches!(
        bind(&scratch.join("link").join("sock")),
        Err(SignerError::SocketDir)
    ));

    assert!(matches!(bind(Path::new("/")), Err(SignerError::SocketPath)));
    assert!(matches!(
        bind(&scratch.join("no").join("such").join("sock")),
        Err(SignerError::Io(_))
    ));
}

// Catches: a process that keeps its dumpable flag (the key readable through ptrace by
// any process of the account, or in a core file).
#[test]
fn harden_clears_the_dumpable_flag() {
    harden().expect("harden");
    assert_eq!(
        rustix::process::dumpable_behavior().expect("read the flag"),
        rustix::process::DumpableBehavior::NotDumpable
    );
}
