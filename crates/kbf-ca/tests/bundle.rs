//! The PEM bundle: the file every trust surface reads.

mod support;

use kbf_ca::{Authority, Bundle, CaError, Policy};
use sha2::Digest as _;
use support::{External, NOW, Stream};

fn root() -> Vec<u8> {
    Authority::create(External::new(), Policy::default(), NOW, &mut Stream(1))
        .expect("root")
        .root_der()
        .to_vec()
}

// Catches: a bundle that does not survive its own text, loses the order (current first),
// or whose pin is not the SHA-256 of the first certificate.
#[test]
fn a_bundle_round_trips_through_pem_in_order() {
    let (a, b) = (root(), root());
    let bundle = Bundle::new(vec![a.clone(), b.clone()]).expect("bundle");
    let pem = bundle.to_pem();
    assert_eq!(pem.matches("-----BEGIN CERTIFICATE-----\n").count(), 2);
    assert_eq!(pem.matches("-----END CERTIFICATE-----\n").count(), 2);
    assert!(
        pem.lines().all(|l| l.len() <= 64 || l.starts_with("-----")),
        "64 columns"
    );
    let again = Bundle::from_pem(pem.as_bytes()).expect("parse");
    assert_eq!(again, bundle);
    assert_eq!(again.certs(), [a.clone(), b]);
    let hash: [u8; 32] = sha2::Sha256::digest(&a).into();
    assert_eq!(bundle.pin().as_bytes(), &hash);
}

// Catches: a bundle built from nothing, from bytes that are not certificates, or with bytes
// after a certificate: a pin of the first certificate must be of a whole certificate.
#[test]
fn a_bundle_is_whole_certificates() {
    let good = root();
    let mut trailing = good.clone();
    trailing.push(0);
    assert_eq!(Bundle::new(Vec::new()).err(), Some(CaError::Bundle));
    assert_eq!(
        Bundle::new(vec![b"junk".to_vec()]).err(),
        Some(CaError::Bundle)
    );
    assert_eq!(
        Bundle::new(vec![good.clone(), trailing]).err(),
        Some(CaError::Bundle)
    );
    assert_eq!(
        Bundle::new(vec![good, Vec::new()]).err(),
        Some(CaError::Bundle)
    );
}

// Catches: PEM that is not certificates being read as a bundle: a private key file named by
// mistake, text with nothing in it, damaged base64, and blocks of another kind.
#[test]
fn text_that_is_not_certificates_is_not_a_bundle() {
    let key = rcgen::KeyPair::generate().expect("key").serialize_pem();
    let good = Bundle::new(vec![root()]).expect("bundle").to_pem();
    let broken = good.replace("MI", "M!");
    let mixed = format!("{good}{key}");
    for (what, text) in [
        ("a private key", key.as_str()),
        ("nothing", ""),
        ("prose", "this is not pem"),
        ("damaged base64", broken.as_str()),
        ("a certificate and a key", mixed.as_str()),
    ] {
        assert_eq!(
            Bundle::from_pem(text.as_bytes()).err(),
            Some(CaError::Bundle),
            "{what}"
        );
    }
    let empty_block = "-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n";
    assert_eq!(
        Bundle::from_pem(empty_block.as_bytes()).err(),
        Some(CaError::Bundle)
    );
}
