//! Staged rotation: the new root, its cross-signature, the bundles of each step and what a
//! node holding only the old pin makes of them.

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use kbf_ca::{Authority, Bundle, CaError, Issued, NodeCert, Policy, PublicKey, Rotation};
use kbf_proto::kbf::enroll::v1::IssuedCert;
use kbf_types::{CaPin, NodeId, Role};
use rcgen::KeyPair;
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::server::WebPkiClientVerifier;
use support::answer::{AnswerError, Expect, check};
use support::{External, NOW, Stream, public_of, view};
use x509_parser::prelude::{FromDer, X509Certificate};

fn pair() -> (Authority, Rotation) {
    let old =
        Authority::create(External::new(), Policy::default(), NOW, &mut Stream(1)).expect("old");
    let rotation = old
        .stage_rotation(External::new(), NOW, &mut Stream(2))
        .expect("stage");
    (old, rotation)
}

fn leaf(ca: &Authority) -> (Issued, PublicKey) {
    let key = public_of(&KeyPair::generate().expect("key"));
    let issued = ca
        .issue_node(
            &NodeCert {
                node_id: &"n1".parse().expect("id"),
                role: Role::Daemon,
                key: &key,
                advertise: None,
            },
            NOW,
            &mut Stream(3),
        )
        .expect("issued");
    (issued, key)
}

/// What a node holding `pin` makes of a leaf and a bundle.
fn node_accepts(
    pin: &CaPin,
    bundle: &Bundle,
    issued: &Issued,
    key: &PublicKey,
) -> Result<(), AnswerError> {
    let id: NodeId = "n1".parse().expect("id");
    let cert = IssuedCert {
        role: "daemon".to_owned(),
        leaf_der: issued.der.clone(),
        serial: issued.serial.clone(),
        not_before: issued.not_before,
        not_after: issued.not_after,
    };
    check(
        &Expect {
            pin,
            node_id: &id,
            keys: &[(Role::Daemon, key.spki().to_vec())],
            now: NOW,
        },
        &[cert],
        bundle.to_pem().as_bytes(),
        NOW,
        NOW + 3 * 86_400,
    )
    .map(drop)
}

fn chain_ok(roots: &[&[u8]], chain: &[&[u8]]) -> bool {
    let mut store = RootCertStore::empty();
    for root in roots {
        store
            .add(CertificateDer::from(root.to_vec()))
            .expect("root");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(store), provider)
        .build()
        .expect("client verifier");
    let end = CertificateDer::from(chain[0].to_vec());
    let rest: Vec<CertificateDer<'_>> = chain[1..]
        .iter()
        .map(|c| CertificateDer::from(c.to_vec()))
        .collect();
    let now =
        UnixTime::since_unix_epoch(Duration::from_secs(u64::try_from(NOW).expect("positive")));
    verifier.verify_client_cert(&end, &rest, now).is_ok()
}

// Catches: a cross-signature that is not one: the new root's certificate must carry the new
// key under the new root's name, be issued and signed by the old root, and be a CA of pathlen 0.
#[test]
fn the_cross_signed_root_is_the_new_key_signed_by_the_old_root() {
    let (old, rotation) = pair();
    let next = view(rotation.next().root_der());
    let cross = view(rotation.cross_signed_der());
    let old_view = view(old.root_der());
    assert_eq!(cross.subject, next.subject, "one name for the new root");
    assert_eq!(cross.issuer, old_view.subject);
    assert_eq!(cross.spki, next.spki, "the new key");
    assert_ne!(cross.spki, old_view.spki);
    assert_eq!(cross.ca, Some((true, true, Some(0))));
    assert_ne!(rotation.cross_signed_der(), rotation.next().root_der());
    let (_, c) = X509Certificate::from_der(rotation.cross_signed_der()).expect("cross");
    let (_, o) = X509Certificate::from_der(old.root_der()).expect("old");
    let (_, n) = X509Certificate::from_der(rotation.next().root_der()).expect("next");
    assert!(
        c.verify_signature(Some(o.public_key())).is_ok(),
        "signed by the old root"
    );
    assert!(
        c.verify_signature(Some(n.public_key())).is_err(),
        "not by the new key"
    );
    assert!(
        n.verify_signature(None).is_ok(),
        "the new root is also self-signed"
    );
    assert_ne!(old.pin(), rotation.next().pin());
}

// Catches: bundles in the wrong order or with the wrong members at a step: the current root
// comes first (it carries the pin a fresh token gets), the cross-signature is in every
// bundle that must show the old pin reaching the new root, and a retired bundle is the new
// root alone.
#[test]
fn each_step_has_its_bundle() {
    let (old, rotation) = pair();
    let ders = |b: &Bundle| b.certs().to_vec();
    let (o, n, x) = (
        old.root_der().to_vec(),
        rotation.next().root_der().to_vec(),
        rotation.cross_signed_der().to_vec(),
    );
    assert_eq!(
        ders(&rotation.staged_bundle()),
        [o.clone(), n.clone(), x.clone()]
    );
    assert_eq!(ders(&rotation.activated_bundle()), [n.clone(), o, x]);
    assert_eq!(ders(&rotation.retired_bundle()), [n]);
    assert_eq!(rotation.staged_bundle().pin(), old.pin());
    assert_eq!(rotation.activated_bundle().pin(), rotation.next().pin());
    assert_eq!(rotation.retired_bundle().pin(), rotation.next().pin());
    assert_eq!(rotation.next().root_der(), rotation.next().root_der());
}

// Catches: a rotation that strands nodes. A node holding the old pin accepts a certificate
// of the new CA through the cross-signature; a node with the new pin accepts it directly;
// and once the old root is retired the old pin finds nothing and the old CA's certificates
// are no longer trusted.
#[test]
fn a_node_with_the_old_pin_follows_the_rotation() {
    let (old, rotation) = pair();
    let old_pin = old.pin();
    let new_pin = rotation.next().pin();
    let (old_leaf, old_key) = leaf(&old);
    let (new_leaf, new_key) = leaf(rotation.next());

    // Staged: certificates still come from the old CA.
    assert_eq!(
        node_accepts(&old_pin, &rotation.staged_bundle(), &old_leaf, &old_key),
        Ok(())
    );
    // Activated: certificates come from the new CA, to nodes of either pin.
    assert_eq!(
        node_accepts(&old_pin, &rotation.activated_bundle(), &new_leaf, &new_key),
        Ok(())
    );
    assert_eq!(
        node_accepts(&new_pin, &rotation.activated_bundle(), &new_leaf, &new_key),
        Ok(())
    );
    assert_eq!(
        node_accepts(&old_pin, &rotation.activated_bundle(), &old_leaf, &old_key),
        Ok(())
    );
    // Retired: the new pin works, the old finds no root, and the old CA's leaf is untrusted.
    assert_eq!(
        node_accepts(&new_pin, &rotation.retired_bundle(), &new_leaf, &new_key),
        Ok(())
    );
    assert_eq!(
        node_accepts(&old_pin, &rotation.retired_bundle(), &new_leaf, &new_key),
        Err(AnswerError::PinNotInBundle)
    );
    assert!(matches!(
        node_accepts(&new_pin, &rotation.retired_bundle(), &old_leaf, &old_key),
        Err(AnswerError::Cert { .. })
    ));
}

// Catches: a cross-signature the TLS stack does not accept: the chain a listener presents
// (leaf, then the cross-signed root) must verify for a node holding only the old root, the
// new root alone must verify the leaf directly, and without the cross-signature the old root
// verifies nothing of the new CA's.
#[test]
fn webpki_follows_the_cross_signature() {
    let (old, rotation) = pair();
    let (new_leaf, _) = leaf(rotation.next());
    let (old_leaf, _) = leaf(&old);
    let (o, n, x) = (
        old.root_der(),
        rotation.next().root_der(),
        rotation.cross_signed_der(),
    );

    assert!(
        chain_ok(&[o], &[&new_leaf.der, x]),
        "an old-root node, with the cross-signed root"
    );
    assert!(!chain_ok(&[o], &[&new_leaf.der]), "but not without it");
    assert!(chain_ok(&[n], &[&new_leaf.der]), "a new-root node");
    assert!(
        chain_ok(&[o], &[&old_leaf.der]),
        "the old CA's own leaf, before retirement"
    );
    assert!(
        !chain_ok(&[n], &[&old_leaf.der]),
        "and not after: the new root does not vouch for it"
    );
}

// Catches: a rotation to the key already in use (no rotation at all), a signer that fails
// halfway, and a clock before the floor.
#[test]
fn a_rotation_needs_a_new_key_a_working_signer_and_a_good_clock() {
    let signer = External::new();
    let old =
        Authority::create(signer.clone(), Policy::default(), NOW, &mut Stream(1)).expect("old");
    assert_eq!(
        old.stage_rotation(signer.clone(), NOW, &mut Stream(2))
            .err(),
        Some(CaError::SameKey)
    );

    let next = External::new();
    next.fail.store(true, Ordering::SeqCst);
    assert!(matches!(
        old.stage_rotation(next.clone(), NOW, &mut Stream(2)),
        Err(CaError::Build(_))
    ));
    next.fail.store(false, Ordering::SeqCst);

    signer.fail.store(true, Ordering::SeqCst);
    assert!(matches!(
        old.stage_rotation(next.clone(), NOW, &mut Stream(2)),
        Err(CaError::Build(_))
    ));
    signer.fail.store(false, Ordering::SeqCst);

    assert!(matches!(
        old.stage_rotation(next.clone(), 1, &mut Stream(2)),
        Err(CaError::ClockBeforeFloor { .. })
    ));
    assert!(old.stage_rotation(next, NOW, &mut Stream(2)).is_ok());
}

// Catches: `into_next` returning something other than the staged authority.
#[test]
fn into_next_is_the_staged_authority() {
    let (_, rotation) = pair();
    let root = rotation.next().root_der().to_vec();
    let next = rotation.into_next();
    assert_eq!(next.root_der(), root);
}
