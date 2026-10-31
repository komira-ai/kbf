//! What the signer agrees to sign, on the to-be-signed structure itself. Each test builds
//! the structure a frontend would send (the farm's own builder, or rcgen with one thing
//! made wrong) and says which defect it catches. The control tests show the base
//! structure is accepted, so a refusal is the one wrong thing and not a broken fixture.

mod support;

use kbf_ca::Policy;
use kbf_signer::{Refusal, Route, check_tbs};
use kbf_types::Role;
use rcgen::{
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
    PKCS_ECDSA_P384_SHA384, PKCS_ED25519, SanType,
};
use std::sync::OnceLock;
use support::{Ca, Evil, NOW, ca_true, custom_extension, dns, host_role_uri, make_ca, role_uri};

fn ca() -> &'static Ca {
    static CA: OnceLock<Ca> = OnceLock::new();
    // A static is never dropped, so the CA keeps no directory (the tests read no file).
    CA.get_or_init(|| make_ca("refusals").without_files())
}

fn check(tbs: &[u8], route: Route) -> Result<(), Refusal> {
    check_tbs(tbs, route, Policy::default(), &ca().root_subject, NOW)
}

fn node(tweak: impl FnOnce(&mut Evil<'static>)) -> Result<(), Refusal> {
    let mut evil = Evil::new(ca());
    tweak(&mut evil);
    check(&evil.tbs(), Route::Node)
}

// Controls: the shapes of the table are accepted on their routes.

// Catches: a validator that refuses what the farm's own builder makes (so enrollment
// would stop working the day it is deployed).
#[test]
fn the_farms_own_certificates_are_accepted_on_their_routes() {
    let ca = ca();
    for role in [Role::Storage, Role::Daemon] {
        assert_eq!(
            check(&support::node_tbs(ca, role, None), Route::Node),
            Ok(())
        );
    }
    assert_eq!(
        check(
            &support::node_tbs(ca, Role::Storage, Some("192.0.2.3")),
            Route::Node
        ),
        Ok(())
    );
    assert_eq!(
        check(
            &support::node_tbs(ca, Role::Storage, Some("store.example")),
            Route::Node
        ),
        Ok(())
    );
    assert_eq!(check(&support::frontend_tbs(ca), Route::Frontend), Ok(()));
    assert_eq!(node(|_| {}), Ok(()), "the base of the defects below");
}

// Catches: a validator that signs the root's own structure (CA:TRUE, keyCertSign): a
// compromised frontend could then mint an intermediate CA.
#[test]
fn the_root_structure_is_refused_on_both_routes() {
    let tbs = support::root_tbs(ca());
    assert_eq!(check(&tbs, Route::Node), Err(Refusal::CaCertificate));
    assert_eq!(check(&tbs, Route::Frontend), Err(Refusal::CaCertificate));
}

// Catches: CA:TRUE accepted on an otherwise valid leaf; a certificate with no
// basicConstraints at all accepted (the table says CA:FALSE, and a missing extension is
// how a parser that misreads one would be fooled).
#[test]
fn a_leaf_must_say_ca_false() {
    assert_eq!(
        node(|e| e.params.is_ca = ca_true()),
        Err(Refusal::CaCertificate)
    );
    assert_eq!(
        node(|e| e.params.is_ca = IsCa::NoCa),
        Err(Refusal::CaCertificate)
    );
}

// Catches: the node route minting a frontend-role certificate, which a node holding it
// could use to impersonate the enrollment server (design section 5.2).
#[test]
fn the_node_route_refuses_the_frontend_role() {
    let ca = ca();
    assert_eq!(
        check(&support::frontend_tbs(ca), Route::Node),
        Err(Refusal::FrontendRole)
    );
}

// Catches: the frontend route minting a storage or daemon certificate, and a role the
// table does not have on either route; a certificate with two roles or none.
#[test]
fn roles_are_the_routes_and_exactly_one() {
    let ca = ca();
    assert_eq!(
        check(&support::node_tbs(ca, Role::Storage, None), Route::Frontend),
        Err(Refusal::RoleUri)
    );
    assert_eq!(
        node(|e| e.params.subject_alt_names = vec![role_uri("operator"), dns("node-a")]),
        Err(Refusal::RoleUri)
    );
    assert_eq!(
        node(|e| e.params.subject_alt_names =
            vec![role_uri("daemon"), role_uri("storage"), dns("node-a")]),
        Err(Refusal::RoleUri)
    );
    assert_eq!(
        node(|e| e.params.subject_alt_names = vec![dns("node-a")]),
        Err(Refusal::RoleUri)
    );
    assert_eq!(
        node(|e| e.params.subject_alt_names = vec![
            SanType::URI("https://role.example/daemon".try_into().expect("ia5")),
            dns("node-a")
        ]),
        Err(Refusal::RoleUri)
    );
}

// Catches: names the table does not have: a wildcard (one certificate for every host), an
// e-mail address, a certificate with no name, no subjectAltName, and a daemon naming
// more than its node.
#[test]
fn names_are_dns_names_and_addresses_only() {
    let wild = vec![role_uri("daemon"), dns("*.example")];
    assert_eq!(
        node(|e| e.params.subject_alt_names = wild),
        Err(Refusal::Name)
    );
    let mail = vec![
        role_uri("daemon"),
        dns("node-a"),
        SanType::Rfc822Name("a@example.com".try_into().expect("ia5")),
    ];
    assert_eq!(
        node(|e| e.params.subject_alt_names = mail),
        Err(Refusal::Name)
    );
    assert_eq!(
        node(|e| e.params.subject_alt_names = vec![role_uri("daemon")]),
        Err(Refusal::Name)
    );
    assert_eq!(
        node(|e| e.params.subject_alt_names = vec![]),
        Err(Refusal::Name)
    );
    let two = vec![role_uri("daemon"), dns("node-a"), dns("other")];
    assert_eq!(
        node(|e| e.params.subject_alt_names = two),
        Err(Refusal::Name)
    );
    // A storage node may name more than itself.
    let storage = vec![role_uri("storage"), dns("node-a"), dns("other")];
    assert_eq!(
        node(|e| {
            e.params.subject_alt_names = storage;
            e.params.extended_key_usages = vec![
                ExtendedKeyUsagePurpose::ServerAuth,
                ExtendedKeyUsagePurpose::ClientAuth,
            ];
        }),
        Ok(())
    );
}

// Catches: a lifetime over the policy (a stolen frontend minting a ten-year node
// certificate), an empty validity, and an off-by-one at the limit.
#[test]
fn the_lifetime_is_the_policys_and_not_a_second_more() {
    let start = time::OffsetDateTime::from_unix_timestamp(NOW - 300).expect("time");
    let limit = time::Duration::seconds(Policy::DEFAULT_LIFETIME_S + Policy::BACKDATE_S);
    assert_eq!(node(|e| e.params.not_after = start + limit), Ok(()));
    assert_eq!(
        node(|e| e.params.not_after = start + limit + time::Duration::seconds(1)),
        Err(Refusal::Lifetime)
    );
    assert_eq!(
        node(|e| e.params.not_after = start + time::Duration::days(3650)),
        Err(Refusal::Lifetime)
    );
    assert_eq!(node(|e| e.params.not_after = start), Err(Refusal::Lifetime));
    // The signer's own policy is what counts, not the default.
    let tbs = Evil::new(ca()).tbs();
    let short = Policy::new(Policy::MIN_LIFETIME_S).expect("policy");
    assert_eq!(
        check_tbs(&tbs, Route::Node, short, &ca().root_subject, NOW),
        Err(Refusal::Lifetime)
    );
}

// Catches: a compromised frontend pre-minting certificates that start in the future (a
// day ahead, or one second past the backdate allowance), and one already expired by the
// signer's own clock. The boundaries are accepted, so the rule is not stricter than the
// allowance (the builder backdates by that much).
#[test]
fn the_validity_is_checked_against_the_signers_clock() {
    let week = time::Duration::seconds(Policy::DEFAULT_LIFETIME_S);
    let at = |t: i64| time::OffsetDateTime::from_unix_timestamp(t).expect("time");
    let starting = |t: i64| {
        node(|e| {
            e.params.not_before = at(t);
            e.params.not_after = at(t) + week;
        })
    };
    assert_eq!(starting(NOW + 86_400), Err(Refusal::NotYetValid));
    assert_eq!(
        starting(NOW + Policy::BACKDATE_S + 1),
        Err(Refusal::NotYetValid)
    );
    assert_eq!(starting(NOW + Policy::BACKDATE_S), Ok(()));
    let ending = |t: i64| {
        node(|e| {
            e.params.not_before = at(t - 3_600);
            e.params.not_after = at(t);
        })
    };
    assert_eq!(ending(NOW - 1), Err(Refusal::Expired));
    assert_eq!(ending(NOW), Err(Refusal::Expired));
    assert_eq!(ending(NOW + 1), Ok(()));
}

// Catches: key usages outside the table (keyCertSign on a leaf, or any other bit), and a
// certificate with no key usage.
#[test]
fn the_key_usage_is_digital_signature_alone() {
    assert_eq!(
        node(|e| e.params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign
        ]),
        Err(Refusal::KeyUsage)
    );
    assert_eq!(
        node(|e| e.params.key_usages = vec![KeyUsagePurpose::KeyEncipherment]),
        Err(Refusal::KeyUsage)
    );
    assert_eq!(
        node(|e| e.params.key_usages = vec![]),
        Err(Refusal::KeyUsage)
    );
}

// Catches: extended key usages outside the table: a daemon that can also serve, a storage
// node that cannot also be a client, anyExtendedKeyUsage or code signing added, and none.
#[test]
fn the_extended_key_usage_is_the_roles() {
    use ExtendedKeyUsagePurpose as Eku;
    let ekus = |list: Vec<Eku>| move |e: &mut Evil<'static>| e.params.extended_key_usages = list;
    assert_eq!(
        node(ekus(vec![Eku::ServerAuth, Eku::ClientAuth])),
        Err(Refusal::ExtendedKeyUsage)
    );
    assert_eq!(
        node(ekus(vec![Eku::ServerAuth])),
        Err(Refusal::ExtendedKeyUsage)
    );
    assert_eq!(
        node(ekus(vec![Eku::ClientAuth, Eku::CodeSigning])),
        Err(Refusal::ExtendedKeyUsage)
    );
    assert_eq!(node(ekus(vec![Eku::Any])), Err(Refusal::ExtendedKeyUsage));
    assert_eq!(node(ekus(vec![])), Err(Refusal::ExtendedKeyUsage));
    assert_eq!(
        node(ekus(vec![
            Eku::ClientAuth,
            Eku::Other(vec![1, 3, 6, 1, 4, 1, 99999, 2])
        ])),
        Err(Refusal::ExtendedKeyUsage)
    );
    // A storage certificate that lost its serverAuth.
    assert_eq!(
        node(|e| {
            e.params.subject_alt_names = vec![role_uri("storage"), dns("node-a")];
        }),
        Err(Refusal::ExtendedKeyUsage)
    );
}

// Catches: an extension the table does not have riding along (name constraints, a policy,
// anything an operator's tooling or an attacker could add).
#[test]
fn no_extension_outside_the_table() {
    assert_eq!(
        node(|e| e.params.custom_extensions = vec![custom_extension()]),
        Err(Refusal::Extension)
    );
}

// Catches: a structure that signs with another algorithm than the CA's, a certified key
// of another curve, a structure issued under another name.
#[test]
fn algorithm_key_and_issuer_are_the_cas() {
    assert_eq!(
        node(|e| e.algorithm = &PKCS_ED25519),
        Err(Refusal::SignatureAlgorithm)
    );
    assert_eq!(
        node(|e| e.subject = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).expect("key")),
        Err(Refusal::KeyType)
    );
    assert_eq!(
        node(|e| {
            let mut other = support::daemon_params();
            let mut name = rcgen::DistinguishedName::new();
            name.push(rcgen::DnType::CommonName, "another-ca");
            other.distinguished_name = name;
            other.is_ca = ca_true();
            e.issuer = Some(other);
        }),
        Err(Refusal::Issuer)
    );
    let _ = PKCS_ECDSA_P256_SHA256;
}

// Catches: bytes that are not a certificate body signed anyway (the signer is a signing
// oracle), and bytes smuggled after a valid body.
#[test]
fn only_a_certificate_body_and_nothing_after_it() {
    assert_eq!(check(b"sign me", Route::Node), Err(Refusal::Malformed));
    assert_eq!(check(&[], Route::Node), Err(Refusal::Malformed));
    let mut tbs = Evil::new(ca()).tbs();
    tbs.push(0);
    assert_eq!(check(&tbs, Route::Node), Err(Refusal::Trailing));
}

// The updater shape: role `updater`, one host-role URI, one DNS name, clientAuth only.

fn updater(tweak: impl FnOnce(&mut Evil<'static>)) -> Result<(), Refusal> {
    node(|e| {
        e.params.subject_alt_names =
            vec![role_uri("updater"), host_role_uri("daemon"), dns("node-a")];
        tweak(e);
    })
}

// Catches: a signer that refuses the updater certificate the farm's own builder makes (the
// control for the refusals below), in either order of the two URIs.
#[test]
fn the_farms_updater_certificate_is_accepted_on_the_node_route() {
    assert_eq!(updater(|_| {}), Ok(()));
    assert_eq!(
        updater(|e| e.params.subject_alt_names = vec![
            host_role_uri("mac-daemon"),
            dns("node-a"),
            role_uri("updater")
        ]),
        Ok(())
    );
}

// Catches: an updater certificate with two host roles, none, an unknown one, or one that is
// not a host-role URI (the coordinator could be told a host is two things).
#[test]
fn an_updater_names_exactly_one_known_host_role() {
    let refused = Err(Refusal::RoleUri);
    assert_eq!(
        updater(|e| e.params.subject_alt_names = vec![
            role_uri("updater"),
            host_role_uri("daemon"),
            host_role_uri("storage"),
            dns("node-a")
        ]),
        refused
    );
    assert_eq!(
        updater(|e| e.params.subject_alt_names = vec![role_uri("updater"), dns("node-a")]),
        refused
    );
    for bad in ["cache", "nope", "Daemon", "", "daemon "] {
        assert_eq!(
            updater(|e| e.params.subject_alt_names =
                vec![role_uri("updater"), host_role_uri(bad), dns("node-a")]),
            refused,
            "{bad:?}"
        );
    }
    assert_eq!(
        updater(|e| e.params.subject_alt_names =
            vec![role_uri("updater"), role_uri("updater"), dns("node-a")]),
        refused
    );
}

// Catches: a host-role URI added to a daemon, storage or frontend certificate, or to the
// coordinator's, so a role could carry a second identity.
#[test]
fn a_host_role_uri_is_refused_on_every_other_role() {
    for role in ["daemon", "storage", "frontend", "coordinator", "operator"] {
        assert!(
            node(|e| e.params.subject_alt_names =
                vec![role_uri(role), host_role_uri("daemon"), dns("node-a")])
            .is_err(),
            "{role}"
        );
    }
}

// Catches: an updater certificate that can serve (serverAuth), or has two names.
#[test]
fn an_updater_is_client_auth_only_and_names_one_node() {
    assert_eq!(
        updater(|e| e.params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ClientAuth,
            ExtendedKeyUsagePurpose::ServerAuth
        ]),
        Err(Refusal::ExtendedKeyUsage)
    );
    assert_eq!(
        updater(|e| e.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth]),
        Err(Refusal::ExtendedKeyUsage)
    );
    assert_eq!(
        updater(|e| e.params.subject_alt_names = vec![
            role_uri("updater"),
            host_role_uri("daemon"),
            dns("node-a"),
            dns("node-b")
        ]),
        Err(Refusal::Name)
    );
}

// Catches: the frontend route (the frontend's own door) signing an updater certificate.
#[test]
fn the_frontend_route_refuses_an_updater() {
    let mut evil = Evil::new(ca());
    evil.params.subject_alt_names =
        vec![role_uri("updater"), host_role_uri("daemon"), dns("node-a")];
    assert_eq!(check(&evil.tbs(), Route::Frontend), Err(Refusal::RoleUri));
}

// Catches: the coordinator's certificate (serverAuth, role `coordinator`) signed on either
// route: it is not reachable from a token, so the signer holds no door for it.
#[test]
fn the_coordinators_certificate_is_not_signed_on_either_route() {
    let mut evil = Evil::new(ca());
    evil.params.subject_alt_names = vec![role_uri("coordinator"), dns("coord.example")];
    evil.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let tbs = evil.tbs();
    assert_eq!(check(&tbs, Route::Node), Err(Refusal::RoleUri));
    assert_eq!(check(&tbs, Route::Frontend), Err(Refusal::RoleUri));
}
