//! The CA directory at first start: created when empty, loaded when placed, never
//! completed or overwritten.

mod support;

use kbf_ca::{Authority, Policy};
use kbf_signer::{SignerError, Started, ensure};
use support::{NOW, Stream, make_ca, scratch};

// Catches: first start that does not make a CA (the frontend has nothing to sign with), a
// created CA that a second start does not load as the same one (a new root per restart
// orphans every certificate), and a pin that is not the root's.
#[test]
fn an_empty_directory_gets_a_ca_and_a_restart_loads_the_same_one() {
    let dir = scratch("ensure").join("ca");
    let (made, how) = ensure(&dir, Policy::default(), NOW, &mut Stream(1)).expect("create");
    assert_eq!(how, Started::Created);
    let (again, how) = ensure(&dir, Policy::default(), NOW, &mut Stream(2)).expect("load");
    assert_eq!(how, Started::Loaded);
    assert_eq!(made.pin(), again.pin());
    assert_eq!(
        made.pin(),
        Authority::load_dir(&dir, Policy::default())
            .expect("l")
            .pin()
    );
    assert_eq!(made.root_der(), again.root_der());
}

// Catches: a placed (imported) CA replaced by a new one: the files an operator put there
// are loaded with their pin, byte for byte.
#[test]
fn a_placed_ca_is_loaded_not_replaced() {
    let ca = make_ca("ensure_placed");
    let before = std::fs::read(ca.dir.join("ca.key")).expect("key");
    let (loaded, how) = ensure(&ca.dir, Policy::default(), NOW, &mut Stream(3)).expect("load");
    assert_eq!(how, Started::Loaded);
    assert_eq!(loaded.root_der(), ca.root.as_slice());
    assert_eq!(std::fs::read(ca.dir.join("ca.key")).expect("key"), before);
}

// Catches: a half-made CA completed over (a new key beside an old certificate, or the
// reverse), and stray files accepted; the existing key must survive untouched.
#[test]
fn a_half_or_foreign_directory_is_refused_and_nothing_is_written() {
    let ca = make_ca("ensure_half");
    std::fs::remove_file(ca.dir.join("ca.pem")).expect("rm");
    let before = std::fs::read(ca.dir.join("ca.key")).expect("key");
    let err = ensure(&ca.dir, Policy::default(), NOW, &mut Stream(4)).expect_err("half");
    assert!(matches!(err, SignerError::CaDirState));
    assert_eq!(std::fs::read(ca.dir.join("ca.key")).expect("key"), before);
    assert!(!ca.dir.join("ca.pem").exists());

    let other = make_ca("ensure_stray");
    std::fs::write(other.dir.join("notes"), b"x").expect("write");
    assert!(matches!(
        ensure(&other.dir, Policy::default(), NOW, &mut Stream(5)),
        Err(SignerError::CaDirState)
    ));

    let file = scratch("ensure_file").join("f");
    std::fs::write(&file, b"x").expect("write");
    assert!(matches!(
        ensure(&file, Policy::default(), NOW, &mut Stream(6)),
        Err(SignerError::Io(_))
    ));
}
