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
    assert!(matches!(err, SignerError::CaDirState { .. }), "{err}");
    assert_eq!(
        err.to_string(),
        format!(
            "the CA directory {} has only ca.key: remove it if no certificate was ever issued, \
             or restore ca.pem",
            ca.dir.display()
        )
    );
    assert_eq!(std::fs::read(ca.dir.join("ca.key")).expect("key"), before);
    assert!(!ca.dir.join("ca.pem").exists());

    let other = make_ca("ensure_stray");
    std::fs::write(other.dir.join("notes"), b"x").expect("write");
    let err = ensure(&other.dir, Policy::default(), NOW, &mut Stream(5)).expect_err("stray");
    assert!(
        err.to_string()
            .contains("holds ca.key, ca.pem, notes: it must be empty or exactly"),
        "{err}"
    );

    // A lone ca.pem: the advice is to restore the key, not to delete the certificate.
    let pem_only = make_ca("ensure_pem_only");
    std::fs::remove_file(pem_only.dir.join("ca.key")).expect("rm");
    let err = ensure(&pem_only.dir, Policy::default(), NOW, &mut Stream(9)).expect_err("pem");
    assert!(
        err.to_string().contains("has only ca.pem: restore ca.key"),
        "{err}"
    );

    let file = scratch("ensure_file").join("f");
    std::fs::write(&file, b"x").expect("write");
    assert!(matches!(
        ensure(&file, Policy::default(), NOW, &mut Stream(6)),
        Err(SignerError::Io(_))
    ));
}

// Catches: a two-file directory that is not exactly ca.key + ca.pem being loaded or
// completed (key missing; cert missing), and a lone foreign file being taken for a CA
// state with advice about ca.key. Nothing may be written in any of them.
#[test]
fn two_files_that_are_not_a_ca_and_one_foreign_file_are_refused_untouched() {
    for (name, files, listed) in [
        ("ensure_nokey", ["ca.pem", "notes.txt"], "holds ca.pem, notes.txt:"),
        ("ensure_nocert", ["ca.key", "notes.txt"], "holds ca.key, notes.txt:"),
    ] {
        let dir = scratch(name).join("ca");
        std::fs::create_dir_all(&dir).expect("mkdir");
        for f in files {
            std::fs::write(dir.join(f), b"x").expect("write");
        }
        let err = ensure(&dir, Policy::default(), NOW, &mut Stream(7)).expect_err(name);
        assert!(matches!(err, SignerError::CaDirState { .. }), "{err}");
        assert!(err.to_string().contains(listed), "{err}");
        let mut now: Vec<_> = std::fs::read_dir(&dir)
            .expect("ls")
            .map(|e| e.expect("e").file_name().to_string_lossy().into_owned())
            .collect();
        now.sort();
        assert_eq!(now, files, "{name}: nothing written");
        for f in files {
            assert_eq!(std::fs::read(dir.join(f)).expect("r"), b"x");
        }
    }

    let dir = scratch("ensure_lone").join("ca");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("notes.txt"), b"x").expect("write");
    let err = ensure(&dir, Policy::default(), NOW, &mut Stream(8)).expect_err("lone");
    assert_eq!(
        err.to_string(),
        format!(
            "the CA directory {} holds notes.txt: it must be empty or exactly ca.key and \
             ca.pem; move the other files out",
            dir.display()
        )
    );
}
