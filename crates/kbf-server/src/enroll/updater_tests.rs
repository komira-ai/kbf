//! A host's updater through `Enroll` and `Renew` on the frontend: the certificate its token
//! makes, what a token for something else cannot make, and its renewal. Each test names the
//! behaviour it holds.

use kbf_enroll_registry::{TokenSpec, list_nodes};
use kbf_peer::{PeerRole, names_of, role_of, uri_values_of};
use kbf_state::{Cond, Txn};
use kbf_types::{HostRole, RefusalReason, Role};
use rcgen::{KeyPair, PublicKeyData as _};
use x509_parser::prelude::{FromDer as _, X509Certificate};

use crate::enroll::testkit::{
    Farm, enroll_request, farm, farm_at, key, node, renew_request, spec, token,
};

const UPDATER: [Role; 1] = [Role::Updater];

fn updater_spec(host: &str, host_role: HostRole) -> TokenSpec {
    TokenSpec {
        host_role: Some(host_role),
        node_id: Some(node(host)),
        ..spec(&UPDATER)
    }
}

/// A farm with the updater of `host` enrolled as `host_role`, and its key.
fn enrolled(name: &str, host: &str, host_role: HostRole) -> (Farm, Vec<KeyPair>) {
    let farm = farm(name);
    let tok = token(&farm, 1, &updater_spec(host, host_role));
    let (request, keys) = enroll_request(&tok, host, &UPDATER, "");
    farm.service.enroll_now(&request).expect("enrolled");
    (farm, keys)
}

/// The updater's token makes the certificate of the design's table: the role `updater`, the host
/// role the token fixed, the node's one DNS name, client use only, for the key of the request;
/// the CA bundle and the clock come with it, and the node listing does not show the updater.
#[test]
fn an_updater_token_makes_an_updater_certificate_for_its_node_and_host_role() {
    for host_role in [
        HostRole::FRONTEND,
        HostRole::STORAGE,
        HostRole::DAEMON,
        HostRole::MAC_DAEMON,
    ] {
        let farm = farm("updater-cert");
        let tok = token(&farm, 1, &updater_spec("host-1", host_role.clone()));
        let (request, keys) = enroll_request(&tok, "host-1", &UPDATER, "");
        let answer = farm.service.enroll_now(&request).expect("enrolled");

        assert_eq!(answer.certs.len(), 1);
        let issued = &answer.certs[0];
        assert_eq!(issued.role, "updater");
        let der = &issued.leaf_der;
        assert_eq!(role_of(der), Ok(PeerRole::Updater));
        assert_eq!(
            uri_values_of(der, HostRole::URI_PREFIX),
            Ok(vec![host_role.name().to_owned()])
        );
        assert_eq!(names_of(der).expect("names").dns(), ["host-1"]);
        let (_, parsed) = X509Certificate::from_der(der).expect("x509");
        assert_eq!(parsed.public_key().raw, keys[0].subject_public_key_info());
        let usage = parsed.extended_key_usage().expect("eku").expect("present");
        assert!(usage.value.client_auth && !usage.value.server_auth);
        assert_eq!(answer.ca_bundle_pem, farm.ca.bundle_pem().into_bytes());
        assert_eq!(answer.config.expect("config").roles, ["updater"]);
        assert!(
            list_nodes(&*farm.store).expect("nodes").is_empty(),
            "an updater is not a node of the registry's listing"
        );
    }
}

/// A token for something else makes no updater, and an updater's token makes nothing else:
/// each refused with the reason the node client matches, and the token left unspent.
#[test]
fn only_an_updater_token_for_this_node_enrolls_an_updater() {
    let farm = farm("updater-refused");
    let node_token = token(&farm, 1, &spec(&[Role::Storage, Role::Daemon]));
    let updater_token = token(&farm, 2, &updater_spec("host-1", HostRole::DAEMON));
    let cases = [
        (
            &node_token,
            "host-1",
            UPDATER.to_vec(),
            RefusalReason::RoleNotGranted,
        ),
        (
            &updater_token,
            "host-1",
            vec![Role::Daemon],
            RefusalReason::RoleNotGranted,
        ),
        (
            &updater_token,
            "host-2",
            UPDATER.to_vec(),
            RefusalReason::NodeIdNotAllowed,
        ),
        (
            &updater_token,
            "host-1",
            vec![Role::Updater, Role::Daemon],
            RefusalReason::BadRoles,
        ),
    ];
    for (tok, host, roles, why) in cases {
        let advertise = "";
        let (request, _) = enroll_request(tok, host, &roles, advertise);
        assert_eq!(
            farm.service.enroll_now(&request).err(),
            Some(why),
            "{host} {roles:?}"
        );
    }
    let (with_address, _) =
        enroll_request(&updater_token, "host-1", &UPDATER, "host-1.nodes.test:8983");
    assert_eq!(
        farm.service.enroll_now(&with_address).err(),
        Some(RefusalReason::MalformedRequest),
        "an updater has no storage address"
    );
    // None of that spent a use: the token still enrolls.
    let (good, _) = enroll_request(&updater_token, "host-1", &UPDATER, "");
    farm.service.enroll_now(&good).expect("enrolled");
    let (again, _) = enroll_request(&updater_token, "host-1", &UPDATER, "");
    assert_eq!(
        farm.service.enroll_now(&again).err(),
        Some(RefusalReason::TokenUsedUp)
    );
}

/// A token that expired enrolls no updater, wherever the clock moved it.
#[test]
fn an_expired_updater_token_is_refused() {
    let farm = farm("updater-expired");
    let tok = token(&farm, 1, &updater_spec("host-1", HostRole::STORAGE));
    farm.advance(2 * 86_400);
    let (request, _) = enroll_request(&tok, "host-1", &UPDATER, "");
    assert_eq!(
        farm.service.enroll_now(&request).err(),
        Some(RefusalReason::TokenExpired)
    );
}

/// A renewal gives the updater a new certificate for the same key with the same host role and
/// node, signed by the key the registry holds; the frontend asks nothing else of the request.
#[test]
fn an_updater_renews_with_its_own_key_and_keeps_its_host_role() {
    let (farm, keys) = enrolled("updater-renew", "host-1", HostRole::MAC_DAEMON);
    farm.advance(3_600);
    let request = renew_request("host-1", &UPDATER, &keys, farm.now());
    let answer = farm.service.renew_now(&request).expect("renewed");
    assert_eq!(answer.certs.len(), 1);
    let der = &answer.certs[0].leaf_der;
    assert_eq!(role_of(der), Ok(PeerRole::Updater));
    assert_eq!(
        uri_values_of(der, HostRole::URI_PREFIX),
        Ok(vec!["mac-daemon".to_owned()])
    );
    assert_eq!(names_of(der).expect("names").dns(), ["host-1"]);
    let (_, parsed) = X509Certificate::from_der(der).expect("x509");
    assert_eq!(parsed.public_key().raw, keys[0].subject_public_key_info());
    assert!(answer.certs[0].not_before > 0);
}

/// A renewal is refused for a key the registry does not hold, for a host that has no updater
/// (a node of the same id is not one), and for the roles of a node on an updater's id.
#[test]
fn an_updater_renewal_needs_the_updaters_row_and_its_key() {
    let (farm, keys) = enrolled("updater-renew-refused", "host-1", HostRole::DAEMON);
    farm.advance(3_600);
    let other = [key()];
    let wrong_key = renew_request("host-1", &UPDATER, &other, farm.now());
    assert_eq!(
        farm.service.renew_now(&wrong_key).err(),
        Some(RefusalReason::WrongKey)
    );
    let stranger = renew_request("host-2", &UPDATER, &keys, farm.now());
    assert_eq!(
        farm.service.renew_now(&stranger).err(),
        Some(RefusalReason::UnknownNode)
    );
    let as_node = renew_request("host-1", &[Role::Daemon], &keys, farm.now());
    assert_eq!(
        farm.service.renew_now(&as_node).err(),
        Some(RefusalReason::UnknownNode)
    );

    // A node of the same id, enrolled beside it, renews as a node and leaves the updater be.
    let node_token = token(&farm, 2, &spec(&[Role::Daemon]));
    let (request, node_keys) = enroll_request(&node_token, "host-1", &[Role::Daemon], "");
    farm.service
        .enroll_now(&request)
        .expect("the node enrolls beside its updater");
    farm.advance(3_600);
    let renewed = renew_request("host-1", &[Role::Daemon], &node_keys, farm.now());
    let answer = farm.service.renew_now(&renewed).expect("the node renews");
    assert_eq!(role_of(&answer.certs[0].leaf_der), Ok(PeerRole::Daemon));
    let again = renew_request("host-1", &UPDATER, &keys, farm.now());
    assert_eq!(
        role_of(
            &farm
                .service
                .renew_now(&again)
                .expect("the updater renews")
                .certs[0]
                .leaf_der
        ),
        Ok(PeerRole::Updater)
    );
}

/// A CA that cannot sign now (its clock is before the build's floor) answers `NotSigner` to an
/// updater as to a node, and the updater is recorded nowhere.
#[test]
fn a_ca_that_cannot_sign_makes_no_updater() {
    let farm = farm_at("updater-clock", 1_000_000);
    let tok = token(&farm, 1, &updater_spec("host-1", HostRole::DAEMON));
    let (request, _) = enroll_request(&tok, "host-1", &UPDATER, "");
    assert_eq!(
        farm.service.enroll_now(&request).err(),
        Some(RefusalReason::NotSigner)
    );
    assert!(
        farm.store
            .scan_prefix("enroll/updater/")
            .expect("scan")
            .is_empty()
    );
}

/// A renewal while the updater's row does not read is `StoreUnavailable` (the host retries), and
/// no certificate is issued for a host role nobody could read.
#[test]
fn an_updater_renewal_while_its_row_is_unreadable_is_store_unavailable() {
    let (farm, keys) = enrolled("updater-unreadable", "host-1", HostRole::DAEMON);
    farm.advance(3_600);
    farm.store
        .txn(Txn::new().put("enroll/updater/host-1", [0xffu8, 0xff], Cond::Any))
        .expect("store")
        .expect("put");
    let request = renew_request("host-1", &UPDATER, &keys, farm.now());
    assert_eq!(
        farm.service.renew_now(&request).err(),
        Some(RefusalReason::StoreUnavailable)
    );
}
