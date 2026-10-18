//! The enroll listener's own certificate: issued by the CA for the frontend's names, kept
//! fresh under one key, and never replaced by a failed attempt.

use kbf_ca::SystemEntropy;

use super::*;
use crate::enroll::testkit::{FRONTEND, START, farm};

/// Certificates are judged by the real clock when they are installed, so a test that
/// installs one issues it now.
fn now() -> i64 {
    unix_now()
}

fn names() -> Vec<String> {
    vec![FRONTEND.to_owned()]
}

/// A certificate is issued for the names given, and a renewal serves a new one to new
/// handshakes under the same key.
///
/// Catches: a listener that never replaces its certificate (nodes enrolling after the
/// first week would see an expired frontend), and one whose renewal does not reach the
/// TLS configuration.
#[test]
fn the_certificate_is_renewed_into_the_served_configuration() {
    let farm = farm("tls-renew");
    let tls = FrontendTls::start(&farm.ca, &names(), now(), &mut SystemEntropy).expect("tls");
    assert_eq!(tls.source().reloads().applied, 0);
    tls.renew(&farm.ca, now() + 60, &mut SystemEntropy)
        .expect("renewed");
    assert_eq!(tls.source().reloads().applied, 1);
    tls.renew(&farm.ca, now() + 120, &mut SystemEntropy)
        .expect("renewed");
    assert_eq!(tls.source().reloads().applied, 2);
}

/// A renewal the CA refuses (its clock is before the build's floor) changes nothing and is
/// an error the loop logs and retries.
///
/// Catches: a failed renewal replacing the served certificate with nothing, or being
/// dropped silently.
#[test]
fn a_renewal_the_ca_refuses_leaves_the_served_certificate_alone() {
    let farm = farm("tls-refused");
    let tls = FrontendTls::start(&farm.ca, &names(), now(), &mut SystemEntropy).expect("tls");
    let refused = tls.renew(&farm.ca, 1_000, &mut SystemEntropy);
    assert!(
        matches!(refused, Err(TlsIdentityError::Ca(_))),
        "{refused:?}"
    );
    assert_eq!(tls.source().reloads().applied, 0);
}

/// A certificate for no name, or for a name no certificate can carry, is refused at
/// start.
///
/// Catches: a listener whose certificate no node could verify, found only when the first
/// node fails.
#[test]
fn a_certificate_needs_names_it_can_carry() {
    let farm = farm("tls-names");
    assert!(FrontendTls::start(&farm.ca, &[], START, &mut SystemEntropy).is_err());
    let bad = vec!["not a name".to_owned()];
    assert!(FrontendTls::start(&farm.ca, &bad, START, &mut SystemEntropy).is_err());
}
