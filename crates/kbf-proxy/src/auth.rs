//! The per-path-class auth decision (design section 3): a pure function of the path class,
//! the `buck2_auth` mode, the verified identity if any, and the bearer token if one came.
//!
//! A certificate that fails verification never gets here: it is a handshake failure. So
//! "present" always means "verified". Rules:
//!
//! - Worker class: no certificate is `UNAUTHENTICATED`; a certificate whose role is not
//!   worker or storage is `PERMISSION_DENIED` (the peer is known, the role is wrong).
//! - Reapi class: `none` allows everyone, a certificate or not; `mtls` needs a verified
//!   certificate of any role; `bearer` needs the right token and nothing else stands in for
//!   it (a certificate is neither needed nor enough, and a wrong token is a refusal even
//!   next to a good certificate: one mechanism per mode, no fallback).
//! - Operator and update classes: the operator token, whatever the certificate says.
//! - Probe class: open.

use kbf_peer::PeerRole;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::identity::Identity;
use crate::route::AuthClass;

/// How REAPI and Buck2 callers authenticate (`buck2_auth` of `installer.toml`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Buck2Auth {
    /// Network-level trust (for example a private overlay network).
    #[default]
    None,
    /// An API-key header, compared in constant time.
    Bearer,
    /// A verified client certificate.
    Mtls,
}

/// Why a call is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No credential, or the wrong one: gRPC `UNAUTHENTICATED` (16), HTTP 401.
    Unauthenticated,
    /// A known peer that may not call this: gRPC `PERMISSION_DENIED` (7), HTTP 403.
    PermissionDenied,
}

impl Refusal {
    /// The gRPC status code.
    #[must_use]
    pub const fn grpc_status(self) -> u8 {
        match self {
            Self::Unauthenticated => 16,
            Self::PermissionDenied => 7,
        }
    }

    /// The HTTP status of a non-gRPC path.
    #[must_use]
    pub const fn http_status(self) -> u16 {
        match self {
            Self::Unauthenticated => 401,
            Self::PermissionDenied => 403,
        }
    }
}

/// A token as the proxy keeps it: its SHA-256, so the comparison is over equal-length
/// digests and the token itself is not held after the file is read.
#[derive(Clone, PartialEq, Eq)]
pub struct Token([u8; 32]);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

impl Token {
    /// The token whose value is `secret`.
    #[must_use]
    pub fn new(secret: &[u8]) -> Self {
        Self(Sha256::digest(secret).into())
    }

    /// Whether `presented` is this token, in constant time. The caller rate limits before
    /// calling this, so a flood cannot make the proxy hash without bound.
    #[must_use]
    pub fn admits(&self, presented: &[u8]) -> bool {
        let digest: [u8; 32] = Sha256::digest(presented).into();
        self.0.ct_eq(&digest).into()
    }
}

/// The tokens the proxy checks; a missing one refuses every call that needs it.
#[derive(Clone, Debug, Default)]
pub struct Tokens {
    /// The Buck2 API key (`buck2_auth = "bearer"`).
    pub buck2: Option<Token>,
    /// The operator token, which is also the update credential until OIDC (#1088).
    pub operator: Option<Token>,
}

/// What a request carried.
#[derive(Clone, Copy, Debug, Default)]
pub struct Presented<'a> {
    /// The verified certificate's identity.
    pub identity: Option<&'a Identity>,
    /// The bearer token from `Authorization: Bearer`, never from a query string.
    pub bearer: Option<&'a [u8]>,
}

fn token_ok(expected: Option<&Token>, presented: Presented<'_>) -> Result<(), Refusal> {
    match (expected, presented.bearer) {
        (Some(t), Some(b)) if t.admits(b) => Ok(()),
        _ => Err(Refusal::Unauthenticated),
    }
}

/// Decides one call.
///
/// # Errors
/// The [`Refusal`] to answer with.
pub fn decide(
    class: AuthClass,
    mode: Buck2Auth,
    tokens: &Tokens,
    presented: Presented<'_>,
) -> Result<(), Refusal> {
    match class {
        AuthClass::Probe => Ok(()),
        AuthClass::Operator | AuthClass::Update => token_ok(tokens.operator.as_ref(), presented),
        AuthClass::Worker => match presented.identity {
            None => Err(Refusal::Unauthenticated),
            Some(id) if matches!(id.role, PeerRole::Daemon | PeerRole::Storage) => Ok(()),
            Some(_) => Err(Refusal::PermissionDenied),
        },
        AuthClass::Reapi => match mode {
            Buck2Auth::None => Ok(()),
            Buck2Auth::Mtls if presented.identity.is_some() => Ok(()),
            Buck2Auth::Mtls => Err(Refusal::Unauthenticated),
            Buck2Auth::Bearer => token_ok(tokens.buck2.as_ref(), presented),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(role: PeerRole) -> Identity {
        Identity {
            node_id: "n.farm".into(),
            role,
        }
    }

    fn tokens() -> Tokens {
        Tokens {
            buck2: Some(Token::new(b"buck2-key")),
            operator: Some(Token::new(b"op-key")),
        }
    }

    fn call(class: AuthClass, mode: Buck2Auth, p: Presented<'_>) -> Result<(), Refusal> {
        decide(class, mode, &tokens(), p)
    }

    #[test]
    fn worker_paths_need_a_worker_or_storage_certificate() {
        let none = Presented::default();
        assert_eq!(
            call(AuthClass::Worker, Buck2Auth::None, none),
            Err(Refusal::Unauthenticated)
        );
        for (role, ok) in [
            (PeerRole::Daemon, true),
            (PeerRole::Storage, true),
            (PeerRole::Frontend, false),
        ] {
            let i = id(role);
            let p = Presented {
                identity: Some(&i),
                bearer: Some(b"op-key"),
            };
            let want = if ok {
                Ok(())
            } else {
                Err(Refusal::PermissionDenied)
            };
            assert_eq!(
                call(AuthClass::Worker, Buck2Auth::None, p),
                want,
                "{role:?}"
            );
        }
    }

    #[test]
    fn reapi_follows_buck2_auth() {
        let i = id(PeerRole::Daemon);
        let none = Presented::default();
        let cert = Presented {
            identity: Some(&i),
            bearer: None,
        };
        let good = Presented {
            identity: None,
            bearer: Some(b"buck2-key"),
        };
        let bad = Presented {
            identity: Some(&i),
            bearer: Some(b"nope"),
        };
        assert_eq!(call(AuthClass::Reapi, Buck2Auth::None, none), Ok(()));
        assert_eq!(
            call(AuthClass::Reapi, Buck2Auth::Mtls, none),
            Err(Refusal::Unauthenticated)
        );
        assert_eq!(call(AuthClass::Reapi, Buck2Auth::Mtls, cert), Ok(()));
        assert_eq!(call(AuthClass::Reapi, Buck2Auth::Bearer, good), Ok(()));
        assert_eq!(
            call(AuthClass::Reapi, Buck2Auth::Bearer, cert),
            Err(Refusal::Unauthenticated)
        );
        // A good certificate does not rescue a wrong token.
        assert_eq!(
            call(AuthClass::Reapi, Buck2Auth::Bearer, bad),
            Err(Refusal::Unauthenticated)
        );
        // The operator token is not the Buck2 key.
        let op = Presented {
            identity: None,
            bearer: Some(b"op-key"),
        };
        assert_eq!(
            call(AuthClass::Reapi, Buck2Auth::Bearer, op),
            Err(Refusal::Unauthenticated)
        );
    }

    #[test]
    fn operator_and_update_need_the_operator_token_whatever_the_certificate() {
        let i = id(PeerRole::Storage);
        let good = Presented {
            identity: None,
            bearer: Some(b"op-key"),
        };
        let cert_only = Presented {
            identity: Some(&i),
            bearer: None,
        };
        for class in [AuthClass::Operator, AuthClass::Update] {
            assert_eq!(call(class, Buck2Auth::None, good), Ok(()));
            assert_eq!(
                call(class, Buck2Auth::None, cert_only),
                Err(Refusal::Unauthenticated)
            );
            assert_eq!(
                decide(class, Buck2Auth::None, &Tokens::default(), good),
                Err(Refusal::Unauthenticated)
            );
        }
        assert_eq!(
            call(AuthClass::Probe, Buck2Auth::Mtls, Presented::default()),
            Ok(())
        );
    }

    #[test]
    fn refusals_map_to_statuses() {
        assert_eq!(Refusal::Unauthenticated.grpc_status(), 16);
        assert_eq!(Refusal::PermissionDenied.grpc_status(), 7);
        assert_eq!(Refusal::Unauthenticated.http_status(), 401);
        assert_eq!(Refusal::PermissionDenied.http_status(), 403);
        assert_eq!(format!("{:?}", Token::new(b"x")), "Token(..)");
        assert!(!Token::new(b"x").admits(b""));
    }
}
