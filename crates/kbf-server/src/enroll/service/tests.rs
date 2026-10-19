//! `Enroll` through the service: what is checked in which order, what a refusal looks like
//! on the wire, and what an answer holds. Each test names the defect it catches.

use kbf_enroll_registry::{list_nodes, list_tokens};
use kbf_proto::kbf::enroll::v1 as pb;
use kbf_proto::kbf::enroll::v1::enroll_server::Enroll as _;
use kbf_state::{Fenced, Store, StoreError};
use kbf_types::{RefusalReason, Role, TokenId};
use rcgen::PublicKeyData as _;
use tonic::{Code, Request, Status};
use x509_parser::prelude::{FromDer as _, X509Certificate};

use super::*;
use crate::enroll::testkit::{
    FRONTEND, Farm, START, csr_bad_signature, csr_oversized, csr_rsa, enroll_request, farm,
    farm_at, key, node, spec, token,
};

const ADVERTISE: &str = "s1.nodes.test:8983";

async fn call(farm: &Farm, request: pb::EnrollRequest) -> Result<pb::EnrollResponse, Status> {
    farm.service
        .enroll(Request::new(request))
        .await
        .map(tonic::Response::into_inner)
}

fn refused(result: Result<pb::EnrollResponse, Status>) -> Status {
    result.expect_err("the call was refused")
}

/// The status exactly as the closed set tells `reason`.
fn assert_is(status: &Status, reason: RefusalReason) {
    assert_eq!(
        status.code() as i32,
        reason.code().number(),
        "{}: code",
        reason.name()
    );
    assert_eq!(
        status.message(),
        reason.message(),
        "{}: message",
        reason.name()
    );
}

fn uses(farm: &Farm) -> u32 {
    list_tokens(&*farm.store).expect("tokens")[0].uses
}

/// Every reason becomes a status with exactly its code and its message.
///
/// Catches: a status built with the wrong code, or with `Display` text or extra detail in
/// the message, which the node client (matching a refusal on code and message together)
/// would not recognise.
#[test]
fn every_reason_becomes_a_status_with_exactly_its_code_and_message() {
    for reason in RefusalReason::ALL {
        let status = status_of(reason);
        assert_is(&status, reason);
        assert!(status.details().is_empty(), "{}", reason.name());
    }
}

/// A store failure is `StoreUnavailable` and a fenced handle `NotSigner`.
///
/// Catches: a fenced frontend telling nodes to retry as if the store were down (they would
/// hammer a frontend that can never answer), and the reverse.
#[test]
fn a_fenced_store_is_not_signer_and_any_other_store_failure_is_store_unavailable() {
    use kbf_enroll_registry::RegistryError;
    assert_eq!(
        registry_reason(&RegistryError::Store(StoreError::Fenced)),
        RefusalReason::NotSigner
    );
    assert_eq!(
        registry_reason(&RegistryError::Store(StoreError::Failed("down".to_owned()))),
        RefusalReason::StoreUnavailable
    );
    assert_eq!(
        registry_reason(&RegistryError::Contended),
        RefusalReason::StoreUnavailable
    );
}

/// An enrollment returns one certificate per role for the key of that role, the CA bundle,
/// the store's clock and a renew time half-way through the shortest life.
///
/// Catches: a certificate for a different key than the CSR's, a role missing, a bundle
/// that is not the CA's, a `server_time` from the wrong clock (the node's skew check
/// would then fail on a correct farm), and `renew_after` not derived from the certificates.
#[tokio::test]
async fn an_enrollment_answers_a_certificate_per_key_with_the_bundle_and_the_clock() {
    let farm = farm("enroll-ok");
    let tok = token(&farm, 1, &spec(&[Role::Storage, Role::Daemon]));
    let (request, keys) =
        enroll_request(&tok, "node-01", &[Role::Storage, Role::Daemon], ADVERTISE);
    let answer = call(&farm, request).await.expect("enrolled");

    assert_eq!(answer.server_time, START);
    assert_eq!(answer.ca_bundle_pem, farm.ca.bundle_pem().into_bytes());
    let roles: Vec<&str> = answer.certs.iter().map(|c| c.role.as_str()).collect();
    assert_eq!(roles, ["storage", "daemon"]);
    for (cert, key) in answer.certs.iter().zip(&keys) {
        let (_, parsed) = X509Certificate::from_der(&cert.leaf_der).expect("x509");
        assert_eq!(parsed.public_key().raw, key.subject_public_key_info());
        assert_eq!(parsed.validity().not_after.timestamp(), cert.not_after);
        assert_eq!(cert.not_before, parsed.validity().not_before.timestamp());
        assert!(!cert.serial.is_empty());
    }
    assert!(answer.certs[0].not_after > START);
    assert_eq!(
        answer.renew_after,
        START + (answer.certs[0].not_after - START) / 2
    );
    let config = answer.config.expect("config");
    assert_eq!(config.generation, CONFIG_GENERATION);
    assert_eq!(config.roles, ["storage", "daemon"]);
    assert_eq!(config.worker_endpoint, "https://front.test:7443");
    assert_eq!(config.storage_port, 8983);

    let nodes = list_nodes(&*farm.store).expect("nodes");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].advertise, ADVERTISE);
    assert_eq!(uses(&farm), 1);
}

/// A storage certificate names the advertise host and a daemon-only one carries no
/// storage name.
///
/// Catches: the advertise host left out of the storage certificate (a client verifying
/// storage by name would refuse every storage node), or put in a daemon certificate.
#[tokio::test]
async fn only_a_storage_certificate_names_the_advertise_host() {
    let farm = farm("enroll-san");
    let tok = token(&farm, 1, &spec(&[Role::Storage, Role::Daemon]));
    let (request, _) = enroll_request(&tok, "node-01", &[Role::Storage, Role::Daemon], ADVERTISE);
    let answer = call(&farm, request).await.expect("enrolled");
    let names = |der: &[u8]| {
        let (_, parsed) = X509Certificate::from_der(der).expect("x509");
        format!("{:?}", parsed.subject_alternative_name().expect("san"))
    };
    assert!(names(&answer.certs[0].leaf_der).contains("s1.nodes.test"));
    assert!(!names(&answer.certs[1].leaf_der).contains("s1.nodes.test"));
}

/// A wrong secret and an unknown token id are the same answer, byte for byte.
///
/// Catches: a service that tells a probe which token ids exist (a different code, message
/// or detail for the unknown id), the order secret-before-everything leaking through the
/// refusal text.
#[tokio::test]
async fn a_wrong_secret_and_an_unknown_id_are_byte_identical() {
    let farm = farm("enroll-secret");
    let tok = token(&farm, 1, &spec(&[Role::Storage]));
    let (mut wrong_secret, _) = enroll_request(&tok, "node-01", &[Role::Storage], ADVERTISE);
    wrong_secret.token_secret = vec![0xee; 32];
    let (mut unknown, _) = enroll_request(&tok, "node-01", &[Role::Storage], ADVERTISE);
    unknown.token_id = TokenId::from_bits(99).to_string();
    let (mut short, _) = enroll_request(&tok, "node-01", &[Role::Storage], ADVERTISE);
    short.token_secret = vec![1; 5];

    let a = refused(call(&farm, wrong_secret).await);
    let b = refused(call(&farm, unknown).await);
    let c = refused(call(&farm, short).await);
    assert_is(&a, RefusalReason::TokenRefused);
    for other in [&b, &c] {
        assert_eq!(a.code(), other.code());
        assert_eq!(a.message(), other.message());
        assert_eq!(a.details(), other.details());
        assert_eq!(a.metadata().len(), other.metadata().len());
    }
    assert_eq!(uses(&farm), 0);
}

/// The secret is checked before the CSRs are read and before the policy: a caller without
/// the secret learns nothing about its CSR or the token's policy.
///
/// Catches: a CSR parsed (CPU spent, a `BadCsr` told) for a guess, and a policy refusal
/// (`RoleNotGranted`, `TokenRevoked`) told to someone who does not hold the secret.
#[tokio::test]
async fn the_secret_is_checked_before_the_csr_and_before_the_policy() {
    let farm = farm("enroll-order");
    let tok = token(&farm, 1, &spec(&[Role::Daemon]));
    let (mut request, _) = enroll_request(&tok, "node-01", &[Role::Storage], ADVERTISE);
    request.token_secret = vec![0xee; 32];
    request.csrs[0].csr_der = csr_rsa();
    assert_is(
        &refused(call(&farm, request.clone()).await),
        RefusalReason::TokenRefused,
    );

    // With the right secret the same request is refused for its role, which is the policy,
    // before its CSR is read.
    request.token_secret = tok.1.as_bytes().to_vec();
    assert_is(
        &refused(call(&farm, request).await),
        RefusalReason::RoleNotGranted,
    );
}

/// A role the token does not grant is refused, and nothing is recorded or spent.
///
/// Catches: roles beyond the token's issued ("the token limits roles"), and a use burnt
/// by a refusal.
#[tokio::test]
async fn a_role_outside_the_token_is_refused_and_nothing_is_recorded() {
    let farm = farm("enroll-roles");
    let tok = token(&farm, 1, &spec(&[Role::Daemon]));
    let (request, _) = enroll_request(&tok, "node-01", &[Role::Daemon, Role::Storage], ADVERTISE);
    assert_is(
        &refused(call(&farm, request).await),
        RefusalReason::RoleNotGranted,
    );
    assert!(list_nodes(&*farm.store).expect("nodes").is_empty());
    assert_eq!(uses(&farm), 0);

    // The subset is fine.
    let (request, _) = enroll_request(&tok, "node-01", &[Role::Daemon], "");
    call(&farm, request)
        .await
        .expect("a subset of the token's roles");
}

/// An RSA key, an oversized CSR and a CSR whose signature does not verify are refused
/// `BadCsr`, and none uses the token up.
///
/// Catches: a certificate for a key the node cannot prove it holds (a bad signature), for
/// a key type the farm does not use (RSA), or an unbounded parse (oversized); and a
/// refused request spending the token.
#[tokio::test]
async fn an_rsa_an_oversized_and_a_badly_signed_csr_are_refused_bad_csr() {
    let farm = farm("enroll-csr");
    let tok = token(&farm, 1, &spec(&[Role::Daemon]));
    let key = key();
    for (name, der) in [
        ("rsa", csr_rsa()),
        ("oversized", csr_oversized(&key)),
        ("bad signature", csr_bad_signature(&key)),
        ("garbage", vec![0x30, 0x03, 0x01, 0x02, 0x03]),
        ("empty", Vec::new()),
    ] {
        let (mut request, _) = enroll_request(&tok, "node-01", &[Role::Daemon], "");
        request.csrs[0].csr_der = der;
        let status = refused(call(&farm, request).await);
        assert_eq!(
            status.code(),
            Code::from_i32(RefusalReason::BadCsr.code().number()),
            "{name}"
        );
        assert_is(&status, RefusalReason::BadCsr);
    }
    assert_eq!(uses(&farm), 0, "a refused request spends nothing");
    assert!(list_nodes(&*farm.store).expect("nodes").is_empty());
}

/// The same request again is answered with the same certificates and one use.
///
/// Catches: a retry after a lost answer minting a second certificate set (and spending a
/// second use), which would strand the node's first key.
#[tokio::test]
async fn the_same_request_again_is_the_same_answer_and_one_use() {
    let farm = farm("enroll-idem");
    let tok = token(&farm, 1, &spec(&[Role::Daemon]));
    let (request, _) = enroll_request(&tok, "node-01", &[Role::Daemon], "");
    let first = call(&farm, request.clone()).await.expect("first");
    farm.advance(5);
    let second = call(&farm, request).await.expect("second");
    assert_eq!(first.certs, second.certs);
    assert_eq!(uses(&farm), 1);
    assert_eq!(
        second.server_time,
        START + 5,
        "the clock is read for each answer"
    );
}

/// A token's use is its limit: a second node with a one-use token is refused.
///
/// Catches: a use count that is not enforced across requests with different keys.
#[tokio::test]
async fn a_used_up_revoked_or_wrong_node_token_is_refused_with_its_reason() {
    let farm = farm("enroll-policy");
    let one = token(&farm, 1, &spec(&[Role::Daemon]));
    let (first, _) = enroll_request(&one, "node-01", &[Role::Daemon], "");
    call(&farm, first).await.expect("first node");
    let (second, _) = enroll_request(&one, "node-02", &[Role::Daemon], "");
    assert_is(
        &refused(call(&farm, second).await),
        RefusalReason::TokenUsedUp,
    );

    let revoked = token(&farm, 2, &spec(&[Role::Daemon]));
    kbf_enroll_registry::revoke_token(&*farm.store, &revoked.0).expect("revoke");
    let (request, _) = enroll_request(&revoked, "node-03", &[Role::Daemon], "");
    assert_is(
        &refused(call(&farm, request).await),
        RefusalReason::TokenRevoked,
    );

    let mut exact = spec(&[Role::Daemon]);
    exact.node_id = Some(node("node-09"));
    let exact = token(&farm, 3, &exact);
    let (request, _) = enroll_request(&exact, "node-04", &[Role::Daemon], "");
    assert_is(
        &refused(call(&farm, request).await),
        RefusalReason::NodeIdNotAllowed,
    );
}

/// The advertise host of a storage node may not be a name the frontend serves, loopback or
/// link-local.
///
/// Catches: a storage node claiming the frontend's own name (a certificate that would let
/// it impersonate the frontend to a client that trusts the CA), or an address that is
/// never a storage node's.
#[tokio::test]
async fn an_advertise_host_the_frontend_will_not_certify_is_refused() {
    let farm = farm("enroll-advertise");
    let hosts = [
        format!("{FRONTEND}:8983"),
        format!("x.{FRONTEND}:8983"),
        "localhost:8983".to_owned(),
        "127.0.0.1:8983".to_owned(),
        "0.0.0.0:8983".to_owned(),
        // Link-local and multicast addresses, built rather than written out: the repo's
        // hygiene lint refuses non-documentation IPv4 literals in source.
        format!("{}:8983", std::net::Ipv4Addr::new(169, 254, 1, 1)),
        format!("{}:8983", std::net::Ipv4Addr::new(224, 0, 0, 1)),
        "x.localhost:8983".to_owned(),
        // IPv6 loopback, link-local and unspecified, and the IPv4 ones in their
        // IPv4-mapped spelling, which name the same hosts.
        "::1:8983".to_owned(),
        "fe80::1:8983".to_owned(),
        format!("::ffff:{}:8983", std::net::Ipv4Addr::LOCALHOST),
        format!("::ffff:{}:8983", std::net::Ipv4Addr::new(169, 254, 1, 1)),
        format!("::ffff:{}:8983", std::net::Ipv4Addr::UNSPECIFIED),
        format!("::ffff:{}:8983", std::net::Ipv4Addr::new(224, 0, 0, 1)),
        "ff02::1:8983".to_owned(),
    ];
    for (bits, host) in (10..).zip(&hosts) {
        let tok = token(&farm, bits, &spec(&[Role::Storage]));
        let (request, _) = enroll_request(&tok, "node-01", &[Role::Storage], host);
        let status = refused(call(&farm, request).await);
        assert_is(&status, RefusalReason::AdvertiseInvalid);
    }
    assert!(list_nodes(&*farm.store).expect("nodes").is_empty());
}

/// A request that is not well formed is `MalformedRequest` (a node id) or `BadRoles` (no
/// roles, a repeated role, a role no token grants) before anything is read.
///
/// Catches: a malformed id reaching the store, a repeated role issuing two certificates
/// for one role, a role name the service invents.
#[tokio::test]
async fn a_malformed_request_is_refused_before_the_store_is_read() {
    let farm = farm("enroll-shape");
    let tok = token(&farm, 1, &spec(&[Role::Storage, Role::Daemon]));
    let ask = |edit: &dyn Fn(&mut pb::EnrollRequest)| {
        let (mut request, _) = enroll_request(&tok, "node-01", &[Role::Daemon], "");
        edit(&mut request);
        request
    };
    let cases: Vec<(&str, pb::EnrollRequest, RefusalReason)> = vec![
        (
            "node id",
            ask(&|r| r.node_id = "Node 01".to_owned()),
            RefusalReason::MalformedRequest,
        ),
        (
            "token id",
            ask(&|r| r.token_id = "nope".to_owned()),
            RefusalReason::MalformedRequest,
        ),
        (
            "zone",
            ask(&|r| r.zone = "z".repeat(64)),
            RefusalReason::MalformedRequest,
        ),
        (
            "advertise",
            ask(&|r| r.advertise = "a".repeat(261)),
            RefusalReason::MalformedRequest,
        ),
        (
            "no roles",
            ask(&|r| r.csrs.clear()),
            RefusalReason::BadRoles,
        ),
        (
            "twice",
            ask(&|r| r.csrs.push(r.csrs[0].clone())),
            RefusalReason::BadRoles,
        ),
        (
            "role name",
            ask(&|r| r.csrs[0].role = "admin".to_owned()),
            RefusalReason::BadRoles,
        ),
    ];
    for (name, request, reason) in cases {
        let status = refused(call(&farm, request).await);
        assert_is(&status, reason);
        assert!(!name.is_empty());
    }
    assert_eq!(uses(&farm), 0);
}

/// A frontend that is no longer the writer answers `NotSigner`, whatever the request.
///
/// Catches: a superseded frontend issuing certificates the successor's registry does not
/// know (they would be unrevocable), or mapping the fence to a retryable store error.
#[tokio::test]
async fn a_fenced_frontend_answers_not_signer() {
    let farm = farm("enroll-fenced");
    let base = Arc::clone(&farm.store);
    let stale: Arc<dyn Store> = Arc::new(Fenced::take(Arc::clone(&base), None).expect("first"));
    let _newer = Fenced::take(base, None).expect("second");
    let service = crate::enroll::testkit::service_over(&stale, &farm.ca);
    let tok = token(&farm, 1, &spec(&[Role::Daemon]));
    let (request, _) = enroll_request(&tok, "node-01", &[Role::Daemon], "");
    let status = refused(
        service
            .enroll(Request::new(request))
            .await
            .map(tonic::Response::into_inner),
    );
    assert_is(&status, RefusalReason::NotSigner);
    assert!(list_nodes(&*farm.store).expect("nodes").is_empty());
}

/// A CA that cannot sign now (the clock is before the build's floor) is `NotSigner`, and
/// nothing is recorded.
///
/// Catches: a node recorded with no certificate, and a CA failure reported as the node's
/// fault (`BadCsr`), which would make it give up instead of retrying.
#[tokio::test]
async fn a_ca_that_cannot_sign_answers_not_signer_and_records_nothing() {
    let farm = farm_at("enroll-clock", 1_000_000);
    let tok = token(&farm, 1, &spec(&[Role::Daemon]));
    let (request, _) = enroll_request(&tok, "node-01", &[Role::Daemon], "");
    assert_is(
        &refused(call(&farm, request).await),
        RefusalReason::NotSigner,
    );
    assert!(list_nodes(&*farm.store).expect("nodes").is_empty());
    assert_eq!(uses(&farm), 0);
}

/// The serial of an issued certificate is told as the bytes of its hex.
///
/// Catches: an odd-length or non-hex serial text being truncated or guessed.
#[test]
fn a_serial_is_the_bytes_of_its_hex() {
    assert_eq!(serial_bytes("0a1b").expect("bytes"), [0x0a, 0x1b]);
    assert_eq!(serial_bytes("abc").expect("bytes"), [0x0a, 0xbc]);
    assert_eq!(serial_bytes("zz"), Err(RefusalReason::NotSigner));
}

/// The renewal time is half-way to the earliest expiry and never in the past.
///
/// Catches: a renew time computed from the latest certificate (a node would renew after
/// one of its certificates expired), or a negative wait.
#[test]
fn renew_after_is_half_way_to_the_earliest_expiry() {
    let cert = |not_after: u64| StoredCert {
        role: Role::Daemon,
        spki_sha256: [0; 32],
        serial: "01".to_owned(),
        not_before: 0,
        not_after,
        cert_der: Vec::new(),
    };
    assert_eq!(renew_after(1000, &[cert(3000), cert(2000)]), 1500);
    assert_eq!(renew_after(1000, &[cert(500)]), 1000);
}

/// A frontend that serves an IP address is not certified for storage in any spelling of
/// that address: zero-compressed or padded IPv6, or the IPv4-mapped form of an IPv4
/// name. Addresses are compared as addresses, not as text.
///
/// Catches: a storage token minting a serverAuth certificate for the frontend's own IP by
/// writing it differently from the `--frontend-name` flag (the node then impersonates the
/// frontend to any client that trusts the CA), and an unrelated address wrongly refused.
#[test]
fn a_frontend_ip_name_is_refused_in_every_spelling_of_it() {
    let v4 = std::net::Ipv4Addr::new(192, 0, 2, 5);
    let names = vec!["2001:db8::1".to_owned(), v4.to_string()];
    for spelling in [
        "2001:db8::1".to_owned(),
        "2001:db8:0::1".to_owned(),
        "2001:0db8::1".to_owned(),
        "2001:0db8:0000:0000:0000:0000:0000:0001".to_owned(),
        v4.to_string(),
        format!("::ffff:{v4}"),
        "::ffff:c000:205".to_owned(),
    ] {
        assert!(advertise_refused(&spelling, &names), "{spelling}");
    }
    for fine in [
        "2001:db8::2",
        "::ffff:192.0.2.6",
        "192.0.2.6",
        "s1.nodes.test",
    ] {
        assert!(!advertise_refused(fine, &names), "{fine}");
    }
}

/// The frontend NAME is judged as an address too: a name written in a non-canonical
/// spelling (padded or compressed IPv6, an IPv4-mapped IPv6 address) refuses the canonical
/// advertise, and the reverse.
///
/// Catches: `to_canonical()` dropped on the name side of `advertise_refused` (a name set as
/// `::ffff:192.0.2.5` or `2001:0db8::1` then compares unequal to the canonical advertise,
/// and a node is certified for an address the frontend serves).
#[test]
fn a_frontend_ip_name_in_any_spelling_refuses_the_canonical_advertise_and_the_reverse() {
    let v4 = std::net::Ipv4Addr::new(192, 0, 2, 5);
    let mapped = format!("::ffff:{v4}");
    for (name, advertise) in [
        (mapped.as_str(), v4.to_string()),
        ("::ffff:c000:205", v4.to_string()),
        ("2001:0db8::1", "2001:db8::1".to_owned()),
        ("2001:db8:0:0:0:0:0:1", "2001:db8::1".to_owned()),
        (
            "2001:0db8:0000:0000:0000:0000:0000:0001",
            "2001:db8::1".to_owned(),
        ),
        (v4.to_string().as_str(), mapped.clone()),
        ("2001:db8::1", "2001:0db8::1".to_owned()),
    ] {
        let names = vec![name.to_owned()];
        assert!(
            advertise_refused(&advertise, &names),
            "{name} vs {advertise}"
        );
    }
    // Neither spelling matches a different address.
    for (name, advertise) in [
        (mapped.as_str(), "192.0.2.6"),
        ("2001:0db8::1", "2001:db8::2"),
    ] {
        let names = vec![name.to_owned()];
        assert!(
            !advertise_refused(advertise, &names),
            "{name} vs {advertise}"
        );
    }
    // End to end through the service: a node cannot enroll under the other spelling.
    let farm = crate::enroll::testkit::farm("enroll-name-spelling");
    let service = EnrollService::new(
        Arc::clone(&farm.store),
        Arc::clone(&farm.ca),
        &[mapped],
        NodeSettings::default(),
    );
    let tok = token(&farm, 1, &spec(&[Role::Storage]));
    let (request, _) = enroll_request(&tok, "node-01", &[Role::Storage], "192.0.2.5:8983");
    assert_eq!(
        service.enroll_now(&request),
        Err(RefusalReason::AdvertiseInvalid)
    );
    assert!(list_nodes(&*farm.store).expect("nodes").is_empty());
}

/// An advertise longer than 260 bytes is refused `MalformedRequest` by the service's own
/// cap, on a request every other layer accepts: a storage token, a host of 253 characters
/// (the longest DNS name) and a port written with leading zeros (a `u16` that parses). The
/// 260-byte request at the cap is not refused for its length.
///
/// Catches: the cap removed (the request is then enrolled: the registry's `is_host`, the
/// port parse and the CA all accept it); an off-by-one in the cap. The case in
/// `a_malformed_request_is_refused_before_the_store_is_read` is a daemon token with an
/// advertise, which the registry refuses first, so it cannot pin the cap.
#[tokio::test]
async fn an_advertise_over_the_cap_is_refused_by_the_cap_and_one_at_it_is_not() {
    let host = [63, 63, 63, 61].map(|n| "a".repeat(n)).join(".");
    assert_eq!(host.len(), 253);
    let farm = farm("enroll-advertise-cap");
    let at_cap = format!("{host}:008983");
    let over = format!("{host}:0008983");
    assert_eq!((at_cap.len(), over.len()), (260, 261));

    let tok = token(&farm, 1, &spec(&[Role::Storage]));
    let (request, _) = enroll_request(&tok, "node-01", &[Role::Storage], &over);
    let status = refused(call(&farm, request).await);
    assert_is(&status, RefusalReason::MalformedRequest);
    assert_eq!(uses(&farm), 0);
    assert!(list_nodes(&*farm.store).expect("nodes").is_empty());

    let (request, _) = enroll_request(&tok, "node-01", &[Role::Storage], &at_cap);
    let answer = call(&farm, request).await;
    assert!(answer.is_ok(), "{answer:?}");
}

/// The service has a `Debug` form that names the type and holds nothing of the CA or the
/// store.
///
/// Catches: a derived `Debug` that prints the registry or the CA into a log line.
#[test]
fn the_service_debug_form_shows_no_state() {
    let farm = farm("enroll-debug");
    assert_eq!(format!("{:?}", farm.service), "EnrollService { .. }");
}

/// A daemon certificate never carries an advertise host, and a host that is no name is
/// not certified: the CA's refusals are told as `AdvertiseInvalid`, not as a signer fault
/// the node would retry.
///
/// Catches: a CA refusal of the request's own content mapped to `NotSigner`.
#[test]
fn an_advertise_host_on_a_daemon_certificate_is_advertise_invalid() {
    let farm = farm("enroll-daemon-advertise");
    let id: NodeId = "node-01".parse().expect("id");
    let public = PublicKey::from_spki(&key().subject_public_key_info()).expect("spki");
    let error = farm
        .service
        .core
        .issue(&id, Role::Daemon, &public, Some("s1.nodes.test"))
        .map(|_| ())
        .expect_err("refused");
    assert_eq!(error, RefusalReason::AdvertiseInvalid);
    let error = farm
        .service
        .core
        .issue(&id, Role::Storage, &public, Some("not a host"))
        .map(|_| ())
        .expect_err("refused");
    assert_eq!(error, RefusalReason::AdvertiseInvalid);
}

/// A node id that is `localhost` or a single-label name the frontend serves is refused
/// `NodeIdNotAllowed` with a token that is otherwise good, and nothing is enrolled or used;
/// an ordinary id still enrolls. `LOCALHOST` and `x.localhost` are not node ids at all
/// (upper case and dots) and are refused as malformed before anything is read.
///
/// Catches: a token holder enrolling as `localhost` (or as the frontend's one-label name)
/// and getting a farm-CA certificate whose DNS name is that host's, serverAuth and clientAuth.
#[tokio::test]
async fn a_node_id_that_is_localhost_or_a_frontend_name_is_refused() {
    let farm = farm("enroll-node-id-names");
    let service = EnrollService::new(
        Arc::clone(&farm.store),
        Arc::clone(&farm.ca),
        &["Front".to_owned(), FRONTEND.to_owned()],
        NodeSettings::default(),
    );
    let ask = |service: &EnrollService, bits: u64, id: &str| {
        let tok = token(&farm, bits, &spec(&[Role::Daemon]));
        let (request, _) = enroll_request(&tok, id, &[Role::Daemon], "");
        service.enroll_now(&request)
    };
    for (bits, id) in [(1, "localhost"), (2, "front")] {
        assert_eq!(
            ask(&service, bits, id),
            Err(RefusalReason::NodeIdNotAllowed),
            "{id}"
        );
    }
    for (bits, id) in [(3, "LOCALHOST"), (4, "x.localhost"), (5, "FRONT")] {
        assert_eq!(
            ask(&service, bits, id),
            Err(RefusalReason::MalformedRequest),
            "{id}"
        );
    }
    assert!(list_nodes(&*farm.store).expect("nodes").is_empty());
    assert!(
        list_tokens(&*farm.store)
            .expect("tokens")
            .iter()
            .all(|t| t.uses == 0)
    );
    assert!(ask(&service, 6, "node-01").is_ok());
    assert_eq!(list_nodes(&*farm.store).expect("nodes").len(), 1);
}

/// The rule itself, case-insensitive, for what a [`NodeId`] cannot spell today: `.localhost`
/// names and upper-case spellings of `localhost` and of a frontend name.
///
/// Catches: a comparison that is case-sensitive or misses names under `localhost`, which
/// would let a later widening of the node id grammar reopen the hole.
#[test]
fn the_node_id_rule_ignores_case_and_covers_names_under_localhost() {
    let names = vec!["front".to_owned()];
    for refused in [
        "localhost",
        "LOCALHOST",
        "x.localhost",
        "X.LocalHost",
        "FRONT",
    ] {
        assert!(node_id_refused(refused, &names), "{refused}");
    }
    for allowed in ["node-01", "localhost2", "front-2", "xlocalhost"] {
        assert!(!node_id_refused(allowed, &names), "{allowed}");
    }
}

/// A token past its `expires_at` on the store's clock is refused `TokenExpired` by the
/// service, uses nothing and enrolls nothing.
///
/// Catches: the service never reaching the registry's expiry check (the registry crate's
/// own test passes whatever the service does with it).
#[tokio::test]
async fn a_token_past_its_expiry_is_refused_token_expired() {
    let farm = farm("enroll-token-expiry");
    let mut short = spec(&[Role::Daemon]);
    short.ttl = Some(std::time::Duration::from_secs(600));
    let tok = token(&farm, 1, &short);
    let (request, _) = enroll_request(&tok, "node-01", &[Role::Daemon], "");
    farm.advance(601);
    assert_is(
        &refused(call(&farm, request).await),
        RefusalReason::TokenExpired,
    );
    assert_eq!(uses(&farm), 0);
    assert!(list_nodes(&*farm.store).expect("nodes").is_empty());
}
