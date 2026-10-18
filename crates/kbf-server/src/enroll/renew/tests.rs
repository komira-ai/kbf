//! `Renew` through the service: the checks the registry makes (revocation, key continuity,
//! `issued_at`, spacing) and the ones this layer adds (the signed `issued_at` equals the
//! request's). Each test names the defect it catches.

use kbf_enroll_registry::{list_nodes, revoke_node};
use kbf_proto::kbf::enroll::v1 as pb;
use kbf_types::{RefusalReason, Role};
use rcgen::{KeyPair, PublicKeyData as _};
use x509_parser::prelude::{FromDer as _, X509Certificate};

use crate::enroll::service::CONFIG_GENERATION;
use crate::enroll::testkit::{
    Farm, START, csr, csr_bad_signature, enroll_request, farm, key, node, renew_request, spec,
    token,
};

const ADVERTISE: &str = "s1.nodes.test:8983";
const ROLES: [Role; 2] = [Role::Storage, Role::Daemon];

/// A farm with `node-01` enrolled for both roles at `START`, and its keys.
fn enrolled(name: &str) -> (Farm, Vec<KeyPair>) {
    let farm = farm(name);
    let tok = token(&farm, 1, &spec(&ROLES));
    let (request, keys) = enroll_request(&tok, "node-01", &ROLES, ADVERTISE);
    farm.service.enroll_now(&request).expect("enrolled");
    (farm, keys)
}

fn refused(result: Result<pb::RenewResponse, RefusalReason>) -> RefusalReason {
    result.expect_err("the renewal was refused")
}

/// A renewal for the same keys gives new certificates for them, with the clock, the bundle
/// and no configuration the node already holds.
///
/// Catches: a renewal that rotates the key (the registry would then refuse every later
/// one), reuses the serial (revocation by serial would hit the live certificate), leaves
/// the storage host out of the certificate, or sends a configuration the node holds.
#[test]
fn a_renewal_issues_new_certificates_for_the_same_keys() {
    let (farm, keys) = enrolled("renew-ok");
    let before = list_nodes(&*farm.store).expect("nodes");
    farm.advance(120);
    let mut request = renew_request("node-01", &ROLES, &keys, farm.now());
    request.config_generation = CONFIG_GENERATION;
    let answer = farm.service.renew_now(&request).expect("renewed");

    assert_eq!(answer.server_time, START + 120);
    assert_eq!(answer.ca_bundle_pem, farm.ca.bundle_pem().into_bytes());
    assert_eq!(answer.config, None, "the node holds this generation");
    assert_eq!(answer.certs.len(), 2);
    for (cert, key) in answer.certs.iter().zip(&keys) {
        let (_, parsed) = X509Certificate::from_der(&cert.leaf_der).expect("x509");
        assert_eq!(parsed.public_key().raw, key.subject_public_key_info());
        assert_eq!(parsed.validity().not_before.timestamp(), cert.not_before);
        assert!(cert.not_before >= START + 120 - 600, "issued now, not at enrollment");
    }
    let storage = X509Certificate::from_der(&answer.certs[0].leaf_der).expect("x509").1;
    let san = format!("{:?}", storage.subject_alternative_name().expect("san"));
    assert!(san.contains("s1.nodes.test"), "{san}");
    assert_eq!(answer.renew_after, START + 120 + (answer.certs[0].not_after - START - 120) / 2);

    let after = list_nodes(&*farm.store).expect("nodes");
    assert_eq!(after[0].last_renewed_at, Some(u64::try_from(START + 120).expect("t")));
    assert!(after[0].not_after >= before[0].not_after);
    let again = farm.service.renew_now(&renew_request("node-01", &ROLES, &keys, farm.now()));
    assert_eq!(refused(again), RefusalReason::RenewTooSoon);
}

/// A node whose configuration is older than the frontend's is sent the new one.
///
/// Catches: a configuration change that never reaches nodes that only renew.
#[test]
fn a_node_with_an_older_configuration_is_sent_the_current_one() {
    let (farm, keys) = enrolled("renew-config");
    farm.advance(120);
    let mut request = renew_request("node-01", &ROLES, &keys, farm.now());
    request.config_generation = CONFIG_GENERATION + 1;
    let answer = farm.service.renew_now(&request).expect("renewed");
    let config = answer.config.expect("a configuration");
    assert_eq!(config.generation, CONFIG_GENERATION);
    assert_eq!(config.roles, ["storage", "daemon"]);
}

/// A renewal signed by any key but the one the registry holds is `WrongKey`.
///
/// Catches: a stolen or guessed node id being enough to get a certificate for a new key
/// (the whole point of the registered key).
#[test]
fn a_renewal_under_another_key_is_wrong_key() {
    let (farm, mut keys) = enrolled("renew-wrongkey");
    farm.advance(120);
    keys[1] = key();
    let result = farm.service.renew_now(&renew_request("node-01", &ROLES, &keys, farm.now()));
    assert_eq!(refused(result), RefusalReason::WrongKey);
}

/// A revoked node is refused `NodeRevoked`.
///
/// Catches: revocation that stops at the deny rows while the node keeps renewing for ever.
#[test]
fn a_revoked_node_is_refused_node_revoked() {
    let (farm, keys) = enrolled("renew-revoked");
    revoke_node(&*farm.store, &node("node-01"), "test").expect("revoked");
    farm.advance(120);
    let result = farm.service.renew_now(&renew_request("node-01", &ROLES, &keys, farm.now()));
    assert_eq!(refused(result), RefusalReason::NodeRevoked);
}

/// A request made too long ago or too far ahead is `StaleIssuedAt`, and so is a negative
/// time.
///
/// Catches: a captured renewal usable for ever (the window is the only bound on a replay
/// of a CSR whose marker has expired), and a negative time wrapping to a huge unsigned one.
#[test]
fn an_issued_at_outside_the_window_is_stale() {
    let (farm, keys) = enrolled("renew-stale");
    farm.advance(120);
    for offset in [-3600, 3600] {
        let request = renew_request("node-01", &ROLES, &keys, farm.now() + offset);
        assert_eq!(refused(farm.service.renew_now(&request)), RefusalReason::StaleIssuedAt);
    }
    let request = renew_request("node-01", &ROLES, &keys, -5);
    assert_eq!(refused(farm.service.renew_now(&request)), RefusalReason::StaleIssuedAt);
}

/// The time in the request must be the one the CSR signed.
///
/// `RenewRequest.issued_at` is not signed. Catches: an old CSR (captured, signed at a time
/// the window accepted then) replayed under a fresh `issued_at` field to pass the window;
/// and a CSR with no `issued_at` at all.
#[test]
fn the_requests_issued_at_must_equal_the_csrs() {
    let (farm, keys) = enrolled("renew-attribute");
    farm.advance(120);
    let now = farm.now();
    let mut replayed = renew_request("node-01", &ROLES, &keys, now - 3600);
    replayed.issued_at = now;
    assert_eq!(refused(farm.service.renew_now(&replayed)), RefusalReason::MalformedRequest);

    let mut unsigned = renew_request("node-01", &ROLES, &keys, now);
    unsigned.csrs[0].csr_der = csr(&keys[0], None);
    assert_eq!(refused(farm.service.renew_now(&unsigned)), RefusalReason::MalformedRequest);
}

/// A repeated CSR and a second renewal inside the spacing are `RenewTooSoon`.
///
/// Catches: a renewal that can be repeated to burn serials or CPU, and a captured request
/// replayed inside its window.
#[test]
fn a_repeated_renewal_is_too_soon() {
    let (farm, keys) = enrolled("renew-soon");
    farm.advance(120);
    let request = renew_request("node-01", &ROLES, &keys, farm.now());
    farm.service.renew_now(&request).expect("renewed");
    farm.advance(5);
    assert_eq!(refused(farm.service.renew_now(&request)), RefusalReason::RenewTooSoon);
    let fresh = renew_request("node-01", &ROLES, &keys, farm.now());
    assert_eq!(refused(farm.service.renew_now(&fresh)), RefusalReason::RenewTooSoon);
    farm.advance(120);
    let later = renew_request("node-01", &ROLES, &keys, farm.now());
    farm.service.renew_now(&later).expect("after the spacing");
}

/// A node the registry does not hold is `UnknownNode`; a request for roles it does not
/// hold, or a CSR that does not parse, is refused and changes nothing.
///
/// Catches: a renewal that enrolls a node by the back door, one that adds a role, and one
/// that signs what it could not parse.
#[test]
fn an_unknown_node_a_different_role_set_and_a_bad_csr_are_refused() {
    let (farm, keys) = enrolled("renew-shape");
    farm.advance(120);
    let now = farm.now();
    assert_eq!(
        refused(farm.service.renew_now(&renew_request("node-77", &ROLES, &keys, now))),
        RefusalReason::UnknownNode
    );
    assert_eq!(
        refused(farm.service.renew_now(&renew_request("node-01", &[Role::Daemon], &keys[1..], now))),
        RefusalReason::BadRoles
    );
    let mut bad = renew_request("node-01", &ROLES, &keys, now);
    bad.csrs[0].csr_der = csr_bad_signature(&keys[0]);
    assert_eq!(refused(farm.service.renew_now(&bad)), RefusalReason::BadCsr);
    let mut malformed = renew_request("node-01", &ROLES, &keys, now);
    malformed.node_id = "Not A Node".to_owned();
    assert_eq!(refused(farm.service.renew_now(&malformed)), RefusalReason::MalformedRequest);
    let after = list_nodes(&*farm.store).expect("nodes");
    assert_eq!(after[0].last_renewed_at, None);
}

/// A certificate that expired too long ago cannot be renewed.
///
/// Catches: renewal as a way back for a node whose certificates lapsed beyond the grace
/// (it must enroll again with a token).
#[test]
fn a_node_expired_beyond_the_grace_must_enroll_again() {
    let (farm, keys) = enrolled("renew-expired");
    farm.advance(86_400 * 40);
    let result = farm.service.renew_now(&renew_request("node-01", &ROLES, &keys, farm.now()));
    assert_eq!(refused(result), RefusalReason::RenewalExpired);
}
