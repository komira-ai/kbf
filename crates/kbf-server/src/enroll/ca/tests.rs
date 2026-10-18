//! The CA at the frontend's start: an empty directory makes one, a directory holding one
//! loads it without touching a byte, an external signer is the only way a production start
//! gets a CA, and the pin a token carries is the root's. Each test names its defect.

use std::os::unix::fs::PermissionsExt as _;

use kbf_ca::{Policy, SystemEntropy};
use kbf_signer::{SignerServer, Started, bind};

use super::*;
use crate::enroll::testkit::{START, scratch};

fn policy() -> Policy {
    Policy::new(86_400).expect("policy")
}

fn open_dev(dir: &Path, now: i64) -> Result<Ca, CaOpenError> {
    Ca::open(dir, &SignerChoice::DevFile, policy(), now, &mut SystemEntropy)
}

fn bytes(dir: &Path, name: &str) -> Vec<u8> {
    std::fs::read(dir.join(name)).expect("file")
}

/// An empty `--ca-dir` gets a new CA (key mode 0600, root), and a directory that holds one
/// is loaded: the same root, the same pin, the same bytes in both files.
///
/// Catches: a start that makes a new CA over an existing one (every node would be locked
/// out: the pin in their tokens and certificates would no longer match), or one that
/// writes the key with a loose mode.
#[test]
fn an_empty_ca_dir_makes_a_ca_and_a_filled_one_is_loaded_untouched() {
    let dir = scratch("ca-dir");
    let made = open_dev(&dir, START).expect("made");
    assert_eq!(made.started(), Started::Created);
    let (key, root) = (bytes(&dir, "ca.key"), bytes(&dir, "ca.pem"));
    assert_eq!(
        std::fs::metadata(dir.join("ca.key")).expect("meta").permissions().mode() & 0o777,
        0o600
    );

    let loaded = open_dev(&dir, START + 10_000_000).expect("loaded");
    assert_eq!(loaded.started(), Started::Loaded);
    assert_eq!(loaded.pin(), made.pin());
    assert_eq!(loaded.bundle_pem(), made.bundle_pem());
    assert_eq!(bytes(&dir, "ca.key"), key, "the key was not rewritten");
    assert_eq!(bytes(&dir, "ca.pem"), root, "the root was not rewritten");
}

/// A directory that is not empty and not a CA is an error and is left as it was; so is one
/// holding half a CA.
///
/// Catches: a CA made in a directory holding other things (an operator's typo of
/// `--ca-dir` such as `/etc`), and a half-made CA completed over.
#[test]
fn a_directory_that_is_not_a_ca_is_refused_and_left_alone() {
    let stray = scratch("ca-stray");
    std::fs::create_dir_all(&stray).expect("dir");
    std::fs::write(stray.join("notes.txt"), b"keep").expect("file");
    assert!(matches!(open_dev(&stray, START), Err(CaOpenError::Dir(_))));
    assert_eq!(std::fs::read_dir(&stray).expect("dir").count(), 1);

    let source = scratch("ca-half-source");
    open_dev(&source, START).expect("made");
    let half = scratch("ca-half");
    std::fs::create_dir_all(&half).expect("dir");
    std::fs::copy(source.join("ca.pem"), half.join("ca.pem")).expect("copy");
    assert!(matches!(open_dev(&half, START), Err(CaOpenError::Dir(_))));
    assert!(!half.join("ca.key").exists(), "a key was not made beside a root");
}

/// A token's pin is the root's pin.
///
/// Catches: a pin source that returns something other than the hash of the root (a node
/// would refuse the frontend it is told to trust).
#[test]
fn the_pin_source_gives_the_roots_pin() {
    let dir = scratch("ca-pin");
    let ca = Arc::new(open_dev(&dir, START).expect("ca"));
    let pins = CaPins(Arc::clone(&ca));
    assert_eq!(pins.pin().expect("pin"), ca.node().pin());
    assert_eq!(ca.pin(), ca.node().pin());
    assert_eq!(ca.frontend().root_der(), ca.node().root_der());
}

/// With an external signer the frontend holds no key: both authorities are external,
/// certificates are issued through the socket, and the directory only needs the root.
///
/// Catches: production silently holding the CA key in the frontend's memory, and a
/// frontend that cannot issue through the signer.
#[test]
fn an_external_signer_issues_and_the_frontend_holds_no_key() {
    let dir = scratch("ca-external");
    let (authority, _) = kbf_signer::ensure(&dir, policy(), START, &mut SystemEntropy).expect("ensure");
    let uid = rustix::process::geteuid().as_raw();
    let server = SignerServer::from_dir(&dir, uid, policy()).expect("server");
    let socket = scratch("ca-external-sock").join("signer.sock");
    let listener = bind(&socket).expect("bind");
    std::thread::spawn(move || {
        let _ = server.serve(&listener);
    });

    let ca = Ca::open(
        &dir,
        &SignerChoice::External(socket.clone()),
        policy(),
        START,
        &mut SystemEntropy,
    )
    .expect("opened through the signer");
    assert_eq!(ca.pin(), authority.pin());
    assert_eq!(ca.started(), Started::Loaded);
    let key = rcgen::KeyPair::generate().expect("key");
    let public = kbf_ca::PublicKey::from_spki(&rcgen::PublicKeyData::subject_public_key_info(&key)).expect("spki");
    let id: kbf_types::NodeId = "node-01".parse().expect("id");
    let issued = ca
        .node()
        .issue_node(
            &kbf_ca::NodeCert {
                node_id: &id,
                role: kbf_types::Role::Daemon,
                key: &public,
                advertise: None,
            },
            START,
            &mut SystemEntropy,
        )
        .expect("issued through the socket");
    assert!(!issued.der.is_empty());
}

/// With an external signer, a directory without `ca.pem` and a root that is not the
/// signer's are refused, and nothing is written.
///
/// Catches: the frontend making a CA of its own when told the key lives elsewhere, and a
/// swapped `ca.pem` changing what the CA is.
#[test]
fn an_external_signer_needs_the_signers_root_and_makes_nothing() {
    let dir = scratch("ca-external-bad");
    let (_, _) = kbf_signer::ensure(&dir, policy(), START, &mut SystemEntropy).expect("ensure");
    let uid = rustix::process::geteuid().as_raw();
    let server = SignerServer::from_dir(&dir, uid, policy()).expect("server");
    let socket = scratch("ca-external-bad-sock").join("signer.sock");
    let listener = bind(&socket).expect("bind");
    std::thread::spawn(move || {
        let _ = server.serve(&listener);
    });
    let external = SignerChoice::External(socket);

    let empty = scratch("ca-external-empty");
    std::fs::create_dir_all(&empty).expect("dir");
    let err = Ca::open(&empty, &external, policy(), START, &mut SystemEntropy).expect_err("no root");
    assert!(matches!(err, CaOpenError::NoRoot { .. }), "{err}");
    assert!(err.to_string().contains("kbf-signer"));
    assert_eq!(std::fs::read_dir(&empty).expect("dir").count(), 0, "nothing was made");

    let other = scratch("ca-external-other");
    open_dev(&other, START).expect("another CA");
    let swapped = scratch("ca-external-swapped");
    std::fs::create_dir_all(&swapped).expect("dir");
    std::fs::copy(other.join("ca.pem"), swapped.join("ca.pem")).expect("copy");
    let err = Ca::open(&swapped, &external, policy(), START, &mut SystemEntropy).expect_err("wrong root");
    assert!(matches!(err, CaOpenError::Ca(_)), "{err}");
}
