//! The CI credential: a GitHub Actions OIDC token for two update calls, off unless the proxy
//! is given an allow-list (`remote_operator_file` of the `[proxy]` table).
//!
//! The operator token stays the credential of every call. With the allow-list a call that
//! carries an OIDC token instead of it is admitted when, and only when, all of these hold:
//! - it is `POST /v1/update/desired` or `POST /v1/update/frontend`, the path exact and with
//!   no query (every other method and path, `abort`, `rollback`, `force-retire`, `pause`,
//!   `resume` and every read, still needs the operator token);
//! - the token is a compact JWS and the address has not presented too many wrong credentials;
//! - `kbf-oidc` accepts it (signature, `iss`, `aud`, times, repository, workflow, ref,
//!   environment, event, runner and a `jti` not seen: the claim set in `kbf-oidc`).
//!
//! The call then goes where the operator's goes: `/v1/update/frontend` to the update handler
//! as it is, `/v1/update/desired` to the active child's operator listener with the operator
//! token in place of the OIDC token (the child's gate takes that and nothing else, and the
//! OIDC token never leaves the proxy).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use http::{Method, Uri};
use kbf_oidc::{HttpsKeys, OidcConfig, Refusal, Roots, Verified, Verifier};

/// The calls an OIDC token may make: method and exact path.
pub const ROUTES: [(&str, &str); 2] = [
    ("POST", "/v1/update/desired"),
    ("POST", "/v1/update/frontend"),
];

/// How long the verifier waits before it fetches the issuer's keys again for an unknown `kid`.
const KEY_REFETCH: Duration = Duration::from_secs(30);

/// How long fetching the issuer's discovery document or keys may take.
const KEY_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The most bytes of a token looked at.
const MAX_TOKEN_BYTES: usize = 8192;

/// The verifier of the CI credential.
#[derive(Debug)]
pub struct RemoteCi {
    verifier: Arc<Verifier>,
}

impl RemoteCi {
    /// A gate over `verifier`.
    #[must_use]
    pub const fn new(verifier: Arc<Verifier>) -> Self {
        Self { verifier }
    }

    /// The gate of the allow-list in `file` (its `[remote_operator]` table). The issuer's keys
    /// are fetched over https with the host's roots, on first use.
    ///
    /// # Errors
    /// The file is unreadable or refused, or there is no root certificate to trust.
    pub fn from_file(file: &Path) -> Result<Self, String> {
        let config = OidcConfig::from_file(file).map_err(|e| e.to_string())?;
        let keys = HttpsKeys::new(&config.issuer, Roots::System, KEY_FETCH_TIMEOUT)?;
        Ok(Self::new(Arc::new(Verifier::new(
            config,
            Arc::new(keys),
            KEY_REFETCH,
            kbf_oidc::system_clock(),
        ))))
    }

    /// Reads the allow-list in `file` and fetches nothing: whether `from_file` would accept it.
    ///
    /// # Errors
    /// The file is unreadable or refused.
    pub fn check_file(file: &Path) -> Result<(), String> {
        OidcConfig::from_file(file)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Whether a call is one an OIDC token may make.
    #[must_use]
    pub fn allows(method: &Method, uri: &Uri) -> bool {
        uri.query().is_none()
            && ROUTES
                .iter()
                .any(|(m, p)| method.as_str() == *m && uri.path() == *p)
    }

    /// Whether `bearer` has the shape of a compact JWS: three base64url parts. A token of
    /// another shape (the operator token) is not looked at by the verifier.
    #[must_use]
    pub fn looks_like_jwt(bearer: &[u8]) -> bool {
        bearer.len() <= MAX_TOKEN_BYTES
            && bearer.iter().filter(|&&b| b == b'.').count() == 2
            && bearer
                .iter()
                .all(|&b| b == b'.' || b == b'-' || b == b'_' || b.is_ascii_alphanumeric())
    }

    /// Checks the token. On success its `jti` is spent.
    ///
    /// # Errors
    /// The verifier's refusal.
    pub async fn verify(&self, token: &[u8]) -> Result<Verified, Refusal> {
        let text = std::str::from_utf8(token).map_err(|_| Refusal::Malformed("not text"))?;
        self.verifier.verify(text).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(method: &str, uri: &str) -> bool {
        RemoteCi::allows(
            &Method::from_bytes(method.as_bytes()).expect("a method"),
            &uri.parse().expect("a uri"),
        )
    }

    /// Kills: a route outside the two let in (a prefix match, a method not checked, a query
    /// carried through).
    #[test]
    fn exactly_two_calls_are_allowed() {
        assert!(allowed("POST", "/v1/update/desired"));
        assert!(allowed("POST", "/v1/update/frontend"));
        for (method, uri) in [
            ("GET", "/v1/update/desired"),
            ("GET", "/v1/update/frontend"),
            ("PUT", "/v1/update/desired"),
            ("POST", "/v1/update/desired?x=1"),
            ("POST", "/v1/update/desired/"),
            ("POST", "/v1/update/desiredx"),
            ("POST", "/v1/update/frontend/abort"),
            ("POST", "/v1/update/frontend/rollback"),
            ("POST", "/v1/update/frontend/force-retire"),
            ("POST", "/v1/update/pause"),
            ("POST", "/v1/update/resume"),
            ("GET", "/v1/update"),
            ("POST", "/v1/members/n7"),
            ("POST", "/v1/nodes"),
            ("POST", "/"),
        ] {
            assert!(!allowed(method, uri), "{method} {uri}");
        }
    }

    #[test]
    fn only_a_compact_jws_shape_is_looked_at() {
        assert!(RemoteCi::looks_like_jwt(b"aGVhZA.Ym9keQ.c2ln"));
        for not in [
            &b"operator-token-0123456789-abcdefghijkl"[..],
            b"a.b",
            b"a.b.c.d",
            b"a b.c.d",
            b"a.b.c\xff",
            b"",
        ] {
            assert!(!RemoteCi::looks_like_jwt(not), "{not:?}");
        }
        let long = format!("a.{}.c", "b".repeat(MAX_TOKEN_BYTES));
        assert!(!RemoteCi::looks_like_jwt(long.as_bytes()));
    }
}
