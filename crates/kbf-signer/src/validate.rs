//! What the signer agrees to sign: the table of the design's section 5.5, checked on the
//! to-be-signed structure itself, not on what the frontend says it meant.

use kbf_ca::{Policy, PublicKey};
use kbf_types::HostRole;
use std::net::{Ipv4Addr, Ipv6Addr};
use x509_parser::oid_registry::{
    OID_SIG_ECDSA_WITH_SHA256, OID_X509_EXT_AUTHORITY_KEY_IDENTIFIER,
    OID_X509_EXT_BASIC_CONSTRAINTS, OID_X509_EXT_EXTENDED_KEY_USAGE, OID_X509_EXT_KEY_USAGE,
    OID_X509_EXT_SUBJECT_ALT_NAME, OID_X509_EXT_SUBJECT_KEY_IDENTIFIER,
};
use x509_parser::prelude::{FromDer, GeneralName, TbsCertificate};

/// How a role URI names its role: this, then the role's name.
const ROLE_URI_PREFIX: &str = "urn:kbf:role:";

/// `KeyUsage.flags` of digitalSignature alone (bit 0).
const DIGITAL_SIGNATURE: u16 = 1;

/// Which door the request came through. The route fixes the role a certificate may carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// A certificate for an enrolled node: role `storage` or `daemon`, or a host's `updater`
    /// (with its host-role URI).
    Node,
    /// The frontend's own certificates: role `frontend`, and the job scheduler's server
    /// certificate, role `scheduler`.
    Frontend,
}

/// Why the signer would not sign.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// Not a to-be-signed certificate structure.
    #[error("not a to-be-signed certificate")]
    Malformed,
    /// Bytes after the structure.
    #[error("bytes follow the to-be-signed certificate")]
    Trailing,
    /// The structure names a signature algorithm other than ecdsa-with-SHA256.
    #[error("the signature algorithm is not ecdsa-with-SHA256")]
    SignatureAlgorithm,
    /// The certified key is not ECDSA on P-256.
    #[error("the certified key is not ECDSA on P-256")]
    KeyType,
    /// The issuer is not the root this signer holds the key of.
    #[error("the issuer is not this CA")]
    Issuer,
    /// The validity is empty or longer than the policy.
    #[error("the validity is empty or longer than the policy allows")]
    Lifetime,
    /// The validity starts later than the signer's clock plus the backdate allowance.
    #[error("the validity starts later than the signer's clock allows")]
    NotYetValid,
    /// The validity has already ended by the signer's clock.
    #[error("the validity has already ended")]
    Expired,
    /// Not an end-entity certificate that says so (basicConstraints `CA:FALSE`).
    #[error("not a certificate that says CA:FALSE")]
    CaCertificate,
    /// The key usage is missing or is anything but digitalSignature.
    #[error("the key usage is not digitalSignature alone")]
    KeyUsage,
    /// The extended key usage is not the one of the role.
    #[error("the extended key usage is not the table's for the role")]
    ExtendedKeyUsage,
    /// An extension the table does not have.
    #[error("an extension the table does not have")]
    Extension,
    /// Not exactly one role URI the route may carry (or, for `updater`, that and one host-role
    /// URI naming a host role).
    #[error("not exactly one role URI this route may carry")]
    RoleUri,
    /// The `frontend` role on the node route.
    #[error("the frontend role is not for this route")]
    FrontendRole,
    /// A subject alternative name the table does not have, or too few or too many.
    #[error("the subject alternative names are not the table's")]
    Name,
}

/// Checks that `tbs` is exactly a certificate the CA of `root_subject` (the DER Name of its
/// root) issues on `route` under `policy`, at the signer's clock reading `now` (Unix
/// seconds): it starts no later than the backdate allowance after `now`, and has not
/// already ended.
///
/// # Errors
/// The [`Refusal`] for the first thing found wrong.
pub fn check_tbs(
    tbs: &[u8],
    route: Route,
    policy: Policy,
    root_subject: &[u8],
    now: i64,
) -> Result<(), Refusal> {
    let (rest, t) = TbsCertificate::from_der(tbs).map_err(|_| Refusal::Malformed)?;
    if !rest.is_empty() {
        return Err(Refusal::Trailing);
    }
    if t.signature.algorithm != OID_SIG_ECDSA_WITH_SHA256 {
        return Err(Refusal::SignatureAlgorithm);
    }
    PublicKey::from_spki(t.subject_pki.raw).map_err(|_| Refusal::KeyType)?;
    if !matches!(t.basic_constraints(), Ok(Some(b)) if !b.value.ca) {
        return Err(Refusal::CaCertificate);
    }
    if t.issuer.as_raw() != root_subject {
        return Err(Refusal::Issuer);
    }
    let span = t.validity.not_after.timestamp() - t.validity.not_before.timestamp();
    if !(1..=policy.lifetime_s() + Policy::BACKDATE_S).contains(&span) {
        return Err(Refusal::Lifetime);
    }
    if t.validity.not_before.timestamp() > now.saturating_add(Policy::BACKDATE_S) {
        return Err(Refusal::NotYetValid);
    }
    if t.validity.not_after.timestamp() <= now {
        return Err(Refusal::Expired);
    }
    if t.extensions().iter().any(|e| !ALLOWED.contains(&e.oid)) {
        return Err(Refusal::Extension);
    }
    let usage = t.key_usage().ok().flatten().ok_or(Refusal::KeyUsage)?;
    if usage.value.flags != DIGITAL_SIGNATURE {
        return Err(Refusal::KeyUsage);
    }

    // Names: DNS names and addresses, and the role URI (an updater's two).
    let san = t
        .subject_alternative_name()
        .ok()
        .flatten()
        .ok_or(Refusal::Name)?;
    let mut uris = Vec::new();
    let mut names = 0usize;
    let mut undialable_address = false;
    for name in &san.value.general_names {
        match name {
            GeneralName::URI(uri) => uris.push(*uri),
            GeneralName::DNSName(dns) if dns.contains('*') => return Err(Refusal::Name),
            GeneralName::DNSName(_) => names += 1,
            GeneralName::IPAddress(bytes) => {
                names += 1;
                undialable_address |= undialable(bytes);
            }
            _ => return Err(Refusal::Name),
        }
    }
    // One role URI; an updater's certificate has a second, naming its host's role.
    let role = match uris[..] {
        [uri] => match uri.strip_prefix(ROLE_URI_PREFIX) {
            // An updater without its host role is not a shape.
            Some("updater") | None => return Err(Refusal::RoleUri),
            Some(role) => role,
        },
        [a, b] if route == Route::Node => updater_role(a, b)?,
        _ => return Err(Refusal::RoleUri),
    };
    let shape = match (route, role) {
        (Route::Node, "storage") | (Route::Frontend, "frontend") => Shape::BOTH,
        (Route::Node, "daemon" | "updater") => Shape::CLIENT_OF_ONE,
        (Route::Frontend, "scheduler") => Shape::SCHEDULER,
        (Route::Node, "frontend") => return Err(Refusal::FrontendRole),
        _ => return Err(Refusal::RoleUri),
    };
    if !shape.names.contains(&names) || (shape.refuse_undialable && undialable_address) {
        return Err(Refusal::Name);
    }

    let eku = t
        .extended_key_usage()
        .ok()
        .flatten()
        .ok_or(Refusal::ExtendedKeyUsage)?
        .value;
    let others = usize::from(eku.any)
        + usize::from(eku.code_signing)
        + usize::from(eku.email_protection)
        + usize::from(eku.time_stamping)
        + usize::from(eku.ocsp_signing)
        + eku.other.len();
    if (eku.server_auth, eku.client_auth, others) != (shape.server, shape.client, 0) {
        return Err(Refusal::ExtendedKeyUsage);
    }
    Ok(())
}

/// What a role's certificate may be: the usages it carries, how many names it has, and
/// whether an address that no peer can dial (unspecified or multicast) is refused.
struct Shape {
    /// serverAuth is present (and absent when false).
    server: bool,
    /// clientAuth is present (and absent when false).
    client: bool,
    /// How many DNS names and addresses it names.
    names: std::ops::RangeInclusive<usize>,
    /// Refuse an unspecified or multicast address.
    refuse_undialable: bool,
}

impl Shape {
    /// A node that serves and calls peers (`storage`), or the frontend: any names.
    const BOTH: Shape = Shape {
        server: true,
        client: true,
        names: 1..=usize::MAX,
        refuse_undialable: false,
    };
    /// A client of one node (`daemon`, `updater`): its own name alone.
    const CLIENT_OF_ONE: Shape = Shape {
        server: false,
        client: true,
        names: 1..=1,
        refuse_undialable: false,
    };
    /// The job scheduler's server certificate: it serves and calls nobody by certificate.
    const SCHEDULER: Shape = Shape {
        server: true,
        client: false,
        names: 1..=MAX_SCHEDULER_NAMES,
        refuse_undialable: true,
    };
}

/// The most names a scheduler certificate carries.
const MAX_SCHEDULER_NAMES: usize = 8;

/// Whether the bytes of an `iPAddress` name are an address no peer dials: unspecified or
/// multicast, in either family; or not an address at all (neither 4 nor 16 bytes).
fn undialable(bytes: &[u8]) -> bool {
    match <[u8; 4]>::try_from(bytes) {
        Ok(v4) => {
            let a = Ipv4Addr::from(v4);
            a.is_unspecified() || a.is_multicast()
        }
        Err(_) => match <[u8; 16]>::try_from(bytes) {
            Ok(v6) => {
                let a = Ipv6Addr::from(v6);
                a.is_unspecified() || a.is_multicast()
            }
            Err(_) => true,
        },
    }
}

/// The role of a certificate with two URIs: `updater`, with exactly one `host-role` URI
/// that names a host role, in either order. Anything else is refused.
fn updater_role<'a>(a: &'a str, b: &'a str) -> Result<&'a str, Refusal> {
    let (role, host) = if a.starts_with(HostRole::URI_PREFIX) {
        (b, a)
    } else {
        (a, b)
    };
    let host = host
        .strip_prefix(HostRole::URI_PREFIX)
        .ok_or(Refusal::RoleUri)?;
    HostRole::parse_named(host).map_err(|_| Refusal::RoleUri)?;
    match role.strip_prefix(ROLE_URI_PREFIX) {
        Some(role @ "updater") => Ok(role),
        _ => Err(Refusal::RoleUri),
    }
}

/// The extensions of the table for an end-entity certificate.
const ALLOWED: [x509_parser::der_parser::oid::Oid<'static>; 6] = [
    OID_X509_EXT_SUBJECT_KEY_IDENTIFIER,
    OID_X509_EXT_KEY_USAGE,
    OID_X509_EXT_SUBJECT_ALT_NAME,
    OID_X509_EXT_BASIC_CONSTRAINTS,
    OID_X509_EXT_AUTHORITY_KEY_IDENTIFIER,
    OID_X509_EXT_EXTENDED_KEY_USAGE,
];
