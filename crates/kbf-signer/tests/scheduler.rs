//! The `scheduler` role on the frontend route: the job scheduler's server certificate.
//! serverAuth only, one to eight DNS names or addresses, and never a role another row of
//! the table could mistake it for. Each test names the defect it catches; the control
//! tests show the base structure is accepted, so a refusal is the one wrong thing.

mod support;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, OnceLock};

use kbf_ca::{Authority, CaError, Policy};
use kbf_signer::{Refusal, Route, SocketSigner, check_tbs};
use rcgen::{CustomExtension, ExtendedKeyUsagePurpose, SanType};
use support::{Ca, Evil, NOW, dns, euid, fresh_key, ip, make_ca, role_uri, serve};
use x509_parser::prelude::{FromDer, X509Certificate};

fn ca() -> &'static Ca {
    static CA: OnceLock<Ca> = OnceLock::new();
    // A static is never dropped, so the CA keeps no directory (the tests read no file).
    CA.get_or_init(|| make_ca("scheduler").without_files())
}

fn check(tbs: &[u8], route: Route) -> Result<(), Refusal> {
    check_tbs(tbs, route, Policy::default(), &ca().root_subject, NOW)
}

/// A scheduler-shaped structure with `sans`, then `tweak`; checked on the frontend route.
fn scheduler(sans: Vec<SanType>, tweak: impl FnOnce(&mut Evil<'static>)) -> Result<(), Refusal> {
    let mut evil = Evil::new(ca());
    evil.params.subject_alt_names = sans;
    evil.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    tweak(&mut evil);
    check(&evil.tbs(), Route::Frontend)
}

fn with_names(names: Vec<SanType>) -> Vec<SanType> {
    let mut sans = vec![role_uri("scheduler")];
    sans.extend(names);
    sans
}

// Catches: a signer that refuses the certificate the farm's own builder makes for the
// scheduler (so the lease server could never get one), with an address and a name, with
// an address alone, and with the most names allowed.
#[test]
fn the_scheduler_certificate_is_signed_on_the_frontend_route() {
    let ca = ca();
    let both = support::scheduler_tbs(ca, &["192.0.2.7", "sched.example"]);
    assert_eq!(check(&both, Route::Frontend), Ok(()));
    let one = support::scheduler_tbs(ca, &["192.0.2.7"]);
    assert_eq!(check(&one, Route::Frontend), Ok(()));
    let eight = [
        "a.example",
        "b.example",
        "c.example",
        "d.example",
        "e.example",
        "f.example",
        "g.example",
        "192.0.2.8",
    ];
    assert_eq!(
        check(&support::scheduler_tbs(ca, &eight), Route::Frontend),
        Ok(())
    );
    assert_eq!(
        scheduler(with_names(vec![dns("x.example")]), |_| {}),
        Ok(())
    );
}

// Catches: the node route minting a scheduler certificate: a token-holding node could
// then pass as the job scheduler.
#[test]
fn the_scheduler_role_is_refused_on_the_node_route() {
    let tbs = support::scheduler_tbs(ca(), &["192.0.2.7"]);
    assert_eq!(check(&tbs, Route::Node), Err(Refusal::RoleUri));
}

// Catches: a scheduler certificate that can also act as a client (the signer forced
// clientAuth on every role before this one), one that cannot serve, and one with any
// other usage.
#[test]
fn a_scheduler_certificate_is_server_auth_only() {
    let sans = || with_names(vec![ip("192.0.2.7")]);
    let refused = Err(Refusal::ExtendedKeyUsage);
    for ekus in [
        vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ],
        vec![ExtendedKeyUsagePurpose::ClientAuth],
        vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::OcspSigning,
        ],
        vec![],
    ] {
        assert_eq!(
            scheduler(sans(), |e| e.params.extended_key_usages = ekus.clone()),
            refused,
            "{ekus:?}"
        );
    }
}

// Catches: the name rules missing for the scheduler: no name, nine names, a wildcard, an
// unspecified or multicast address (either family), another kind of name, and a second URI.
#[test]
fn a_scheduler_names_one_to_eight_dns_names_and_addresses() {
    let name = Err(Refusal::Name);
    assert_eq!(scheduler(with_names(vec![]), |_| {}), name, "none");
    let nine = (1..=9).map(|n| dns(&format!("h{n}.example"))).collect();
    assert_eq!(scheduler(with_names(nine), |_| {}), name, "nine");
    assert_eq!(
        scheduler(with_names(vec![dns("*.example")]), |_| {}),
        name,
        "wildcard"
    );
    let undialable: [IpAddr; 5] = [
        Ipv4Addr::UNSPECIFIED.into(),
        Ipv6Addr::UNSPECIFIED.into(),
        Ipv4Addr::new(224, 0, 0, 1).into(),
        Ipv4Addr::new(239, 255, 255, 255).into(),
        Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1).into(),
    ];
    for addr in undialable {
        assert_eq!(
            scheduler(
                with_names(vec![ip("192.0.2.7"), SanType::IpAddress(addr)]),
                |_| {}
            ),
            name,
            "{addr}"
        );
    }
    let email = SanType::Rfc822Name("a@example.com".try_into().expect("ia5"));
    assert_eq!(scheduler(with_names(vec![email]), |_| {}), name, "email");
    let uri = SanType::URI("https://sched.example/".try_into().expect("ia5"));
    assert_eq!(
        scheduler(with_names(vec![dns("x.example"), uri]), |_| {}),
        Err(Refusal::RoleUri),
        "a second URI"
    );
}

// Catches: an iPAddress name that is no address (five bytes) read as a usable name: a
// structure the farm's builder never makes, written by hand as a subjectAltName.
#[test]
fn a_scheduler_name_that_is_no_address_is_refused() {
    let uri = b"urn:kbf:role:scheduler";
    let mut inner = vec![0x86, u8::try_from(uri.len()).expect("short")];
    inner.extend_from_slice(uri);
    inner.extend_from_slice(&[0x87, 5, 1, 2, 3, 4, 5]);
    let mut san = vec![0x30, u8::try_from(inner.len()).expect("short")];
    san.extend(inner);
    let result = scheduler(vec![], |e| {
        e.params
            .custom_extensions
            .push(CustomExtension::from_oid_content(&[2, 5, 29, 17], san));
    });
    assert_eq!(result, Err(Refusal::Name));
}

// Catches: a certificate with two roles, or the scheduler's role beside the frontend's:
// one certificate could then pass for both.
#[test]
fn a_scheduler_certificate_carries_exactly_the_scheduler_role_uri() {
    let refused = Err(Refusal::RoleUri);
    let twice = vec![
        role_uri("scheduler"),
        role_uri("scheduler"),
        dns("x.example"),
    ];
    assert_eq!(scheduler(twice, |_| {}), refused);
    let both = vec![
        role_uri("scheduler"),
        role_uri("frontend"),
        dns("x.example"),
    ];
    assert_eq!(scheduler(both, |_| {}), refused);
    let none = vec![dns("x.example")];
    assert_eq!(scheduler(none, |_| {}), refused);
}

// Catches: the frontend role losing its usage when the table gains a server-only row: it
// stays serverAuth and clientAuth, exactly.
#[test]
fn the_frontend_role_keeps_its_eku() {
    let frontend = |ekus: Vec<ExtendedKeyUsagePurpose>| {
        let mut evil = Evil::new(ca());
        evil.params.subject_alt_names = vec![role_uri("frontend"), dns("front.example")];
        evil.params.extended_key_usages = ekus;
        check(&evil.tbs(), Route::Frontend)
    };
    assert_eq!(check(&support::frontend_tbs(ca()), Route::Frontend), Ok(()));
    assert_eq!(
        frontend(vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth
        ]),
        Ok(())
    );
    let refused = Err(Refusal::ExtendedKeyUsage);
    assert_eq!(frontend(vec![ExtendedKeyUsagePurpose::ServerAuth]), refused);
    assert_eq!(frontend(vec![ExtendedKeyUsagePurpose::ClientAuth]), refused);
}

// Catches: the two roles sharing a usage: the scheduler's role with the frontend's usage
// is refused, and the frontend's role with the scheduler's usage is refused, so the lease
// server's key cannot pass as the frontend nor the other way round.
#[test]
fn a_scheduler_certificate_is_no_frontend_and_a_frontend_no_scheduler() {
    let ca = ca();
    let mut evil = Evil::new(ca);
    evil.params.subject_alt_names = vec![role_uri("scheduler"), dns("x.example")];
    evil.params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    assert_eq!(
        check(&evil.tbs(), Route::Frontend),
        Err(Refusal::ExtendedKeyUsage)
    );
    let mut evil = Evil::new(ca);
    evil.params.subject_alt_names = vec![role_uri("frontend"), dns("x.example")];
    evil.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    assert_eq!(
        check(&evil.tbs(), Route::Frontend),
        Err(Refusal::ExtendedKeyUsage)
    );
}

// Catches: the real socket not carrying the scheduler role: the frontend's door signs the
// builder's certificate and the node door refuses it, with the signer serving on after.
#[test]
fn the_socket_signs_a_scheduler_certificate_on_the_frontend_route_only() {
    let ca = make_ca("scheduler-socket");
    let socket = serve(&ca, euid());
    let sign_on = |route| {
        let client = Arc::new(SocketSigner::connect(&socket, route).expect("connect"));
        let authority = Authority::import(client, Policy::default(), &ca.root).expect("authority");
        authority.issue_scheduler(
            &["192.0.2.7", "sched.example"],
            &fresh_key(),
            NOW,
            &mut support::Stream(4),
        )
    };
    let issued = sign_on(Route::Frontend).expect("the scheduler certificate");
    let (_, leaf) = X509Certificate::from_der(&issued.der).expect("leaf");
    let (_, root) = X509Certificate::from_der(&ca.root).expect("root");
    leaf.verify_signature(Some(root.public_key()))
        .expect("verifies");
    match sign_on(Route::Node) {
        Err(CaError::Build(_)) => {}
        other => panic!("the node route signed a scheduler certificate: {other:?}"),
    }
    sign_on(Route::Frontend).expect("still serving");
}
