//! A compact JWS, taken apart and its RS256 signature checked (RFC 7515, RFC 8725).
//!
//! The algorithm is fixed here and never read from the header as a choice: a header that
//! names anything but `RS256` is refused, so `none` and an HMAC keyed with the public key
//! have no path in. Headers that tell a verifier where to find a key (`jku`, `x5u`,
//! `jwk`, `x5c`) or that carry `crit` are refused.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde_json::{Map, Value};

use super::Refusal;

/// The longest token read.
pub const MAX_TOKEN_BYTES: usize = 8192;

/// Header members a verifier must not act on.
const FORBIDDEN_HEADERS: &[&str] = &["jku", "x5u", "jwk", "x5c", "crit", "zip"];

/// A token whose shape is valid and whose signature is not yet checked.
pub(crate) struct Parsed<'a> {
    pub kid: String,
    pub signing_input: &'a str,
    pub signature: Vec<u8>,
    pub claims: Map<String, Value>,
}

fn b64(part: &str) -> Result<Vec<u8>, Refusal> {
    URL_SAFE_NO_PAD
        .decode(part)
        .map_err(|_| Refusal::Malformed("a part is not unpadded base64url"))
}

/// Splits `token` into header, payload and signature, and checks the header.
pub(crate) fn parse(token: &str) -> Result<Parsed<'_>, Refusal> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(Refusal::Malformed("the token is too long"));
    }
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Refusal::Malformed("a token has three dot-separated parts"));
    };
    let header: Value = serde_json::from_slice(&b64(h)?)
        .map_err(|_| Refusal::Malformed("the header is not JSON"))?;
    let Value::Object(header) = header else {
        return Err(Refusal::Malformed("the header is not an object"));
    };
    match header.get("alg") {
        Some(Value::String(alg)) if alg == "RS256" => {}
        _ => return Err(Refusal::Algorithm),
    }
    if FORBIDDEN_HEADERS.iter().any(|k| header.contains_key(*k)) {
        return Err(Refusal::HeaderParameter);
    }
    let kid = match header.get("kid") {
        Some(Value::String(kid)) if !kid.is_empty() && kid.len() <= 128 => kid.clone(),
        _ => return Err(Refusal::Malformed("the header has no usable kid")),
    };
    let claims: Value = serde_json::from_slice(&b64(p)?)
        .map_err(|_| Refusal::Malformed("the payload is not JSON"))?;
    let Value::Object(claims) = claims else {
        return Err(Refusal::Malformed("the payload is not an object"));
    };
    let signing_input = &token[..h.len() + 1 + p.len()];
    Ok(Parsed {
        kid,
        signing_input,
        signature: b64(s)?,
        claims,
    })
}

/// Whether `signature` is `n`/`e`'s RSASSA-PKCS1-v1_5 SHA-256 signature of `input`
/// (a modulus under 2048 bits is refused by the verification parameters).
pub(crate) fn signature_holds(n: &[u8], e: &[u8], input: &str, signature: &[u8]) -> bool {
    RsaPublicKeyComponents { n, e }
        .verify(&RSA_PKCS1_2048_8192_SHA256, input.as_bytes(), signature)
        .is_ok()
}

/// A JWK's `n` or `e`: unpadded base64url of a big-endian integer.
pub(crate) fn jwk_integer(value: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(value).ok().filter(|v| !v.is_empty())
}
