//! The verified certificate as an [`Identity`], the per-boot [`ProxySecret`], and the
//! headers a child sees (design section 3, "Identity to the child"; the child's side is
//! `kbf-server/src/proxy_auth.rs`).
//!
//! TLS has already verified the certificate against the farm CA (chain, EKU client auth). What
//! is read here is what the child is told: the role, from the one role URI, and the node
//! id. **A node id is the certificate's one DNS name**: zero, several or a wildcard name
//! make the identity ambiguous, so the certificate is refused rather than guessed at.

use http::{HeaderMap, HeaderName, HeaderValue};
use kbf_peer::{PeerRole, RoleError, names_of, role_of};

/// The node id the proxy verified, `x-kbf-node-id`.
pub const NODE_ID: &str = "x-kbf-node-id";
/// The role of the verified certificate, `x-kbf-role` (the `PeerRole::name` string).
pub const ROLE: &str = "x-kbf-role";
/// The per-boot secret, `x-kbf-proxy-auth`.
pub const PROXY_AUTH: &str = "x-kbf-proxy-auth";
/// The reserved prefix: no header of a client or of a child's answer passes with it.
pub const RESERVED_PREFIX: &str = "x-kbf-";
/// The bytes of the secret.
pub const SECRET_BYTES: usize = 32;

/// Who a verified certificate says the peer is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The certificate's one DNS name.
    pub node_id: String,
    /// The certificate's role.
    pub role: PeerRole,
}

/// Why a certificate gives no identity.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// The role URI is missing, repeated or unknown, or the bytes are not a certificate.
    #[error("{0}")]
    Role(#[from] RoleError),
    /// The certificate has no usable node id: this many DNS names (a wildcard counts as none).
    #[error("the certificate must name exactly one node by DNS name; it names {0}")]
    NodeId(usize),
    /// A value that is not a legal header value.
    #[error("not a legal header value")]
    HeaderValue,
}

/// The identity of a DER certificate.
///
/// # Errors
/// [`IdentityError::Role`] or [`IdentityError::NodeId`].
pub fn identity_of(der: &[u8]) -> Result<Identity, IdentityError> {
    let role = role_of(der)?;
    let names = names_of(der)?;
    match names.dns() {
        [one] if !one.contains('*') => Ok(Identity {
            node_id: one.clone(),
            role,
        }),
        other => Err(IdentityError::NodeId(
            other.iter().filter(|n| !n.contains('*')).count(),
        )),
    }
}

/// The 32 random bytes in the file under `/run/kbf-proxy/`. The header value is their
/// 64-character lower-case hex; the child keeps only its SHA-256.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxySecret([u8; SECRET_BYTES]);

impl std::fmt::Debug for ProxySecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProxySecret(..)")
    }
}

impl ProxySecret {
    /// The secret of a file's bytes; `None` unless there are exactly [`SECRET_BYTES`].
    #[must_use]
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        <[u8; SECRET_BYTES]>::try_from(bytes).ok().map(Self)
    }

    /// The header value, as the child compares it.
    #[must_use]
    pub fn hex(&self) -> String {
        hex::encode(self.0)
    }
}

fn value(text: &str) -> Result<HeaderValue, IdentityError> {
    let mut v = HeaderValue::from_str(text).map_err(|_| IdentityError::HeaderValue)?;
    v.set_sensitive(true);
    Ok(v)
}

fn remove_reserved(headers: &mut HeaderMap) {
    let reserved: Vec<HeaderName> = headers
        .keys()
        .filter(|k| k.as_str().starts_with(RESERVED_PREFIX))
        .cloned()
        .collect();
    for name in reserved {
        headers.remove(name);
    }
}

/// Prepares a request's headers for the child: strips every inbound `x-kbf-*` header, then
/// sets `x-kbf-proxy-auth` and, with a verified certificate, `x-kbf-node-id` and
/// `x-kbf-role`. Each header has exactly one value afterwards.
///
/// # Errors
/// [`IdentityError::HeaderValue`]: the node id is not a legal header value. The caller
/// refuses the stream; the inbound headers are already stripped.
pub fn forward_headers(
    headers: &mut HeaderMap,
    identity: Option<&Identity>,
    secret: &ProxySecret,
) -> Result<(), IdentityError> {
    remove_reserved(headers);
    if let Some(id) = identity {
        headers.insert(NODE_ID, value(&id.node_id)?);
        headers.insert(ROLE, value(id.role.name())?);
    }
    headers.insert(PROXY_AUTH, value(&secret.hex())?);
    Ok(())
}

/// Strips every `x-kbf-*` header from a child's answer before it goes to a client. The
/// proxy reads the one it defines (the fenced marker, [`crate::replay::FENCED`]) first.
pub fn strip_reserved(headers: &mut HeaderMap) {
    remove_reserved(headers);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertificateParams, KeyPair, SanType};

    fn cert(dns: &[&str], role: Option<&str>) -> Vec<u8> {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        let mut sans: Vec<SanType> = dns
            .iter()
            .map(|d| SanType::DnsName((*d).try_into().expect("ia5")))
            .collect();
        if let Some(role) = role {
            sans.push(SanType::URI(
                format!("{}{role}", kbf_peer::ROLE_URI_PREFIX)
                    .try_into()
                    .expect("ia5"),
            ));
        }
        params.subject_alt_names = sans;
        let key = KeyPair::generate().expect("key");
        params.self_signed(&key).expect("sign").der().to_vec()
    }

    fn secret() -> ProxySecret {
        ProxySecret::from_slice(&[0xab; 32]).expect("32 bytes")
    }

    #[test]
    fn a_certificate_gives_its_one_dns_name_and_role() {
        let id = identity_of(&cert(&["node-1.farm"], Some("daemon"))).expect("identity");
        assert_eq!(id.node_id, "node-1.farm");
        assert_eq!(id.role, PeerRole::Daemon);
    }

    #[test]
    fn an_ambiguous_or_roleless_certificate_gives_none() {
        assert_eq!(
            identity_of(&cert(&["a", "b"], Some("daemon"))),
            Err(IdentityError::NodeId(2))
        );
        assert_eq!(
            identity_of(&cert(&["*.farm"], Some("daemon"))),
            Err(IdentityError::NodeId(0))
        );
        assert_eq!(
            identity_of(&cert(&[], Some("daemon"))),
            Err(IdentityError::NodeId(0))
        );
        assert!(matches!(
            identity_of(&cert(&["a"], None)),
            Err(IdentityError::Role(_))
        ));
        assert!(matches!(identity_of(b"junk"), Err(IdentityError::Role(_))));
    }

    #[test]
    fn the_secret_is_32_bytes_and_prints_as_64_hex() {
        assert!(ProxySecret::from_slice(&[0; 31]).is_none());
        assert!(ProxySecret::from_slice(&[0; 33]).is_none());
        assert_eq!(secret().hex(), "ab".repeat(32));
        assert_eq!(format!("{:?}", secret()), "ProxySecret(..)");
    }

    #[test]
    fn inbound_identity_headers_never_reach_the_child() {
        let mut h = HeaderMap::new();
        h.append(NODE_ID, HeaderValue::from_static("forged"));
        h.append(NODE_ID, HeaderValue::from_static("forged-2"));
        h.insert(ROLE, HeaderValue::from_static("storage"));
        h.insert(PROXY_AUTH, HeaderValue::from_static("guess"));
        h.insert("x-kbf-other", HeaderValue::from_static("1"));
        h.insert("x-other", HeaderValue::from_static("kept"));
        // No certificate: both identity headers are absent, the secret is the proxy's.
        forward_headers(&mut h, None, &secret()).expect("forward");
        assert!(h.get(NODE_ID).is_none());
        assert!(h.get(ROLE).is_none());
        assert!(h.get("x-kbf-other").is_none());
        assert_eq!(h.get("x-other").expect("kept"), "kept");
        assert_eq!(h.get_all(PROXY_AUTH).iter().count(), 1);
        assert_eq!(h.get(PROXY_AUTH).expect("secret"), "ab".repeat(32).as_str());
    }

    #[test]
    fn a_verified_certificate_sets_one_value_of_each() {
        let id = Identity {
            node_id: "node-1.farm".into(),
            role: PeerRole::Storage,
        };
        let mut h = HeaderMap::new();
        h.append(NODE_ID, HeaderValue::from_static("forged"));
        forward_headers(&mut h, Some(&id), &secret()).expect("forward");
        assert_eq!(h.get_all(NODE_ID).iter().count(), 1);
        assert_eq!(h.get(NODE_ID).expect("node"), "node-1.farm");
        assert_eq!(h.get(ROLE).expect("role"), "storage");
        assert!(h.get(PROXY_AUTH).expect("secret").is_sensitive());
    }

    #[test]
    fn an_unprintable_node_id_is_refused_after_the_strip() {
        let id = Identity {
            node_id: "bad\nname".into(),
            role: PeerRole::Daemon,
        };
        let mut h = HeaderMap::new();
        h.insert(ROLE, HeaderValue::from_static("storage"));
        assert_eq!(
            forward_headers(&mut h, Some(&id), &secret()),
            Err(IdentityError::HeaderValue)
        );
        assert!(h.get(ROLE).is_none());
    }

    #[test]
    fn a_childs_answer_loses_its_reserved_headers() {
        let mut h = HeaderMap::new();
        h.insert("x-kbf-fenced", HeaderValue::from_static("1"));
        h.insert("x-child-id", HeaderValue::from_static("b"));
        strip_reserved(&mut h);
        assert!(h.get("x-kbf-fenced").is_none());
        assert!(h.get("x-child-id").is_some());
    }
}
