//! REAPI `RequestMetadata`, read once per call: the build a call belongs to.
//!
//! REAPI clients send a binary header, [`HEADER`], holding a serialized
//! `RequestMetadata`. [`RequestMetadataLayer`] reads it on every call a tonic server
//! takes, beside the authentication layer, and puts what it found in the request's
//! extensions as an `Arc<CallMetadata>`, which [`call_metadata`] reads. Its
//! `tool_invocation_id` is the call's invocation id (`docs/design/invocations.md`,
//! section 2).
//!
//! **A bad header never fails a call.** A header whose decoded bytes are over
//! [`MAX_HEADER_BYTES`], or that is not base64 or not a `RequestMetadata`, is treated
//! as absent. An id that is empty, over [`MAX_ID_BYTES`], has a byte outside
//! `[A-Za-z0-9._-]` or does not start with a letter or digit is treated as absent, and
//! the header's other fields are kept. Either case is counted
//! ([`RequestMetadataLayer::invalid_count`], `kbf_invocation_metadata_invalid`) and
//! logged at debug level; the call goes on. The text fields are kept up to
//! [`MAX_TEXT_BYTES`] each and cut on a character boundary past that, marked
//! truncated.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};

use base64::Engine as _;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use kbf_proto::reapi::RequestMetadata;
use prost::Message as _;
use tonic::codegen::{Service, http};
use tower_layer::Layer;

/// The header REAPI clients put a serialized `RequestMetadata` in.
pub const HEADER: &str = "build.bazel.remote.execution.v2.requestmetadata-bin";

/// The largest `RequestMetadata` read, in decoded bytes.
pub const MAX_HEADER_BYTES: usize = 8 << 10;

/// The longest invocation id, in bytes.
pub const MAX_ID_BYTES: usize = 128;

/// The longest text field kept, in bytes.
pub const MAX_TEXT_BYTES: usize = 512;

/// gRPC's binary-header encoding: standard base64, padded or not (as tonic reads it).
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// A client's `tool_invocation_id` that kbf accepts: 1 to [`MAX_ID_BYTES`] bytes of
/// `[A-Za-z0-9._-]`, the first a letter or digit. Such an id needs no escaping in a URL
/// path, in HTML or in a header, and is never `.` or `..`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InvocationId(String);

impl InvocationId {
    /// `id` as an invocation id, or `None` when kbf does not accept it.
    #[must_use]
    pub fn parse(id: &str) -> Option<Self> {
        let bytes = id.as_bytes();
        let first_ok = bytes.first().is_some_and(u8::is_ascii_alphanumeric);
        let rest_ok = bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        (first_ok && rest_ok && bytes.len() <= MAX_ID_BYTES).then(|| Self(id.to_owned()))
    }

    /// The id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InvocationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A text field of the header, kept up to [`MAX_TEXT_BYTES`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Text {
    /// The field, cut on a character boundary at [`MAX_TEXT_BYTES`] at most.
    pub value: String,
    /// Whether the client sent more than `value`.
    pub truncated: bool,
}

impl Text {
    fn new(mut value: String) -> Self {
        let truncated = value.len() > MAX_TEXT_BYTES;
        if truncated {
            let mut end = MAX_TEXT_BYTES;
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            value.truncate(end);
        }
        Self { value, truncated }
    }
}

/// What a call's `RequestMetadata` header said. Every field is empty when the call
/// sent no header, or one that was treated as absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallMetadata {
    /// `tool_invocation_id`, when kbf accepts it.
    pub invocation_id: Option<InvocationId>,
    /// `tool_details.tool_name`.
    pub tool_name: Text,
    /// `tool_details.tool_version`.
    pub tool_version: Text,
    /// `action_id`.
    pub action_id: Text,
    /// `correlated_invocations_id`.
    pub correlated_invocations_id: Text,
    /// `action_mnemonic`.
    pub action_mnemonic: Text,
    /// `target_id`.
    pub target_id: Text,
    /// `configuration_id`.
    pub configuration_id: Text,
}

/// Why a header, or its id, was treated as absent.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Invalid {
    /// The decoded header is over [`MAX_HEADER_BYTES`].
    #[error("the header is over {MAX_HEADER_BYTES} bytes")]
    TooLarge,
    /// The header is not base64, or its bytes are not a `RequestMetadata`.
    #[error("the header does not decode")]
    Undecodable,
    /// The header decoded, but kbf does not accept its `tool_invocation_id`.
    #[error("the tool_invocation_id is not an id kbf accepts")]
    BadId,
}

/// What [`CallMetadata::from_headers`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Read {
    /// The call sent no [`HEADER`].
    Absent,
    /// The header, every field kept.
    Valid(CallMetadata),
    /// A header treated as absent (`kept` is `None`), or whose id was (`kept` has every
    /// other field).
    Invalid {
        /// What was kept.
        kept: Option<CallMetadata>,
        /// Why the rest was not.
        why: Invalid,
    },
}

impl CallMetadata {
    /// The metadata in `headers`, read as the module documentation says.
    #[must_use]
    pub fn from_headers(headers: &http::HeaderMap) -> Read {
        let Some(value) = headers.get(HEADER) else {
            return Read::Absent;
        };
        let refused = |why| Read::Invalid { kept: None, why };
        let encoded = value.as_bytes();
        // Refused before decoding: base64 of MAX_HEADER_BYTES is at most this long.
        if encoded.len() > MAX_HEADER_BYTES.div_ceil(3) * 4 {
            return refused(Invalid::TooLarge);
        }
        let Ok(bytes) = BASE64.decode(encoded) else {
            return refused(Invalid::Undecodable);
        };
        if bytes.len() > MAX_HEADER_BYTES {
            return refused(Invalid::TooLarge);
        }
        let Ok(message) = RequestMetadata::decode(bytes.as_slice()) else {
            return refused(Invalid::Undecodable);
        };
        let tool = message.tool_details.unwrap_or_default();
        let metadata = Self {
            invocation_id: InvocationId::parse(&message.tool_invocation_id),
            tool_name: Text::new(tool.tool_name),
            tool_version: Text::new(tool.tool_version),
            action_id: Text::new(message.action_id),
            correlated_invocations_id: Text::new(message.correlated_invocations_id),
            action_mnemonic: Text::new(message.action_mnemonic),
            target_id: Text::new(message.target_id),
            configuration_id: Text::new(message.configuration_id),
        };
        if metadata.invocation_id.is_none() {
            return Read::Invalid {
                kept: Some(metadata),
                why: Invalid::BadId,
            };
        }
        Read::Valid(metadata)
    }

    /// The invocation id as a span or log field: `-` when there is none.
    #[must_use]
    pub fn invocation_display(&self) -> &str {
        self.invocation_id
            .as_ref()
            .map_or("-", InvocationId::as_str)
    }
}

static EMPTY: LazyLock<Arc<CallMetadata>> = LazyLock::new(Arc::default);

/// The metadata [`RequestMetadataLayer`] put on `request`, or empty metadata when the
/// call sent none or no layer ran.
#[must_use]
pub fn call_metadata<T>(request: &tonic::Request<T>) -> Arc<CallMetadata> {
    request
        .extensions()
        .get::<Arc<CallMetadata>>()
        .map_or_else(|| Arc::clone(&EMPTY), Arc::clone)
}

/// Reads [`HEADER`] on every call a tonic server takes, before routing. Add it with
/// `Server::builder().layer(RequestMetadataLayer::new())`, beside the authentication
/// layer; clones share one count of headers treated as absent.
#[derive(Clone, Debug, Default)]
pub struct RequestMetadataLayer {
    invalid: Arc<AtomicU64>,
}

impl RequestMetadataLayer {
    /// A layer whose count starts at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many calls sent a header, or an id, that was treated as absent:
    /// `kbf_invocation_metadata_invalid`. No endpoint exports it yet.
    #[must_use]
    pub fn invalid_count(&self) -> u64 {
        self.invalid.load(Ordering::Relaxed)
    }
}

impl<S> Layer<S> for RequestMetadataLayer {
    type Service = ReadRequestMetadata<S>;

    fn layer(&self, inner: S) -> ReadRequestMetadata<S> {
        ReadRequestMetadata {
            inner,
            invalid: Arc::clone(&self.invalid),
        }
    }
}

/// The service [`RequestMetadataLayer`] wraps around a server's routes.
#[derive(Clone, Debug)]
pub struct ReadRequestMetadata<S> {
    inner: S,
    invalid: Arc<AtomicU64>,
}

impl<S, B> Service<http::Request<B>> for ReadRequestMetadata<S>
where
    S: Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        let metadata = match CallMetadata::from_headers(request.headers()) {
            Read::Absent => None,
            Read::Valid(metadata) => Some(metadata),
            Read::Invalid { kept, why } => {
                self.invalid.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    call = request.uri().path(),
                    reason = %why,
                    "kbf_invocation_metadata_invalid: RequestMetadata treated as absent"
                );
                kept
            }
        };
        if let Some(metadata) = metadata {
            request.extensions_mut().insert(Arc::new(metadata));
        }
        self.inner.call(request)
    }
}

#[cfg(test)]
mod tests {
    use kbf_proto::reapi::ToolDetails;

    use super::*;

    fn headers(value: &[u8]) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        let value = http::HeaderValue::from_bytes(value).expect("a header value");
        headers.insert(HEADER, value);
        headers
    }

    fn encoded(message: &RequestMetadata) -> http::HeaderMap {
        headers(BASE64.encode(message.encode_to_vec()).as_bytes())
    }

    fn with_id(id: &str) -> RequestMetadata {
        RequestMetadata {
            tool_details: Some(ToolDetails {
                tool_name: "buck2".to_owned(),
                tool_version: "0.1.0".to_owned(),
            }),
            tool_invocation_id: id.to_owned(),
            ..Default::default()
        }
    }

    fn text(value: &str) -> Text {
        Text {
            value: value.to_owned(),
            truncated: false,
        }
    }

    /// Catches: a field read from another field of the message (two fields swapped in
    /// the mapping), and a field dropped. Every field holds a distinct value.
    #[test]
    fn a_prost_encoded_header_is_read_back_field_for_field() {
        let message = RequestMetadata {
            tool_details: Some(ToolDetails {
                tool_name: "bazel".to_owned(),
                tool_version: "8.8.1".to_owned(),
            }),
            action_id: "action-1".to_owned(),
            tool_invocation_id: "inv-1".to_owned(),
            correlated_invocations_id: "corr-1".to_owned(),
            action_mnemonic: "CppCompile".to_owned(),
            target_id: "//pkg:target".to_owned(),
            configuration_id: "k8-fastbuild".to_owned(),
        };
        let expected = CallMetadata {
            invocation_id: InvocationId::parse("inv-1"),
            tool_name: text("bazel"),
            tool_version: text("8.8.1"),
            action_id: text("action-1"),
            correlated_invocations_id: text("corr-1"),
            action_mnemonic: text("CppCompile"),
            target_id: text("//pkg:target"),
            configuration_id: text("k8-fastbuild"),
        };
        assert_eq!(
            CallMetadata::from_headers(&encoded(&message)),
            Read::Valid(expected)
        );
        assert_eq!(
            CallMetadata::from_headers(&http::HeaderMap::new()),
            Read::Absent
        );
    }

    /// Catches: an id rule that accepts a character a URL path, HTML or a header
    /// treats specially (the `[!-~]` set lets `~`, `<`, `/` and `%` through), one
    /// without the first-character rule (`.`, `..` and `-x` get through), one with no
    /// length bound, and one that drops the header's other fields with its id.
    #[test]
    fn ids_kbf_does_not_accept_are_absent_and_the_other_fields_kept() {
        let long = "a".repeat(MAX_ID_BYTES + 1);
        let refused = [
            "", "a~b", "a<b", "a/b", "a%41", "a b", "a\u{e9}", ".", "..", "-x", "_x", &long,
        ];
        let mut accepted: Vec<&str> = Vec::new();
        for id in refused {
            match CallMetadata::from_headers(&encoded(&with_id(id))) {
                Read::Invalid {
                    kept: Some(kept),
                    why: Invalid::BadId,
                } => assert_eq!(
                    (kept.invocation_id, kept.tool_name),
                    (None, text("buck2")),
                    "{id:?}"
                ),
                _ => accepted.push(id),
            }
        }
        assert_eq!(accepted, Vec::<&str>::new(), "ids that must be refused");
        let longest = "a".repeat(MAX_ID_BYTES);
        let good = [
            "3136aef1-b380-4ac0-ac06-c6d6bbb65842",
            "kbf-3136aef1-b380-4ac0-ac06-c6d6bbb65842",
            "9",
            "Z.a_b-c",
            &longest,
        ];
        let read: Vec<_> = good
            .iter()
            .map(
                |id| match CallMetadata::from_headers(&encoded(&with_id(id))) {
                    Read::Valid(read) => read.invocation_id.map(|id| id.to_string()),
                    _ => None,
                },
            )
            .collect();
        let expected: Vec<_> = good.iter().map(|id| Some((*id).to_owned())).collect();
        assert_eq!(read, expected);
        assert_eq!(CallMetadata::default().invocation_display(), "-");
    }

    /// Catches: a header over the bound that is read (or decoded before it is
    /// refused), a bound off by one, one that is not base64 or not a
    /// `RequestMetadata` (wrong wire type, a string that is not UTF-8) that is read as
    /// some metadata instead of none, and padded base64 refused (gRPC accepts both).
    #[test]
    fn oversize_and_undecodable_headers_are_absent() {
        let refused = |why| Read::Invalid { kept: None, why };
        // A message of exactly `n` encoded bytes: the id and a target id of the rest.
        let sized = |n: usize| {
            let mut message = with_id("inv");
            message.target_id = "t".repeat(n - message.encoded_len() - 3);
            assert_eq!(message.encoded_len(), n);
            message
        };
        let at_bound = sized(MAX_HEADER_BYTES);
        let Read::Valid(read) = CallMetadata::from_headers(&encoded(&at_bound)) else {
            panic!("a header at the bound was refused");
        };
        assert_eq!(read.invocation_display(), "inv");
        assert_eq!(
            CallMetadata::from_headers(&encoded(&sized(MAX_HEADER_BYTES + 1))),
            refused(Invalid::TooLarge)
        );
        let huge = "A".repeat(MAX_HEADER_BYTES.div_ceil(3) * 4 + 4);
        assert_eq!(
            CallMetadata::from_headers(&headers(huge.as_bytes())),
            refused(Invalid::TooLarge)
        );
        for value in [
            b"not base64!".to_vec(),
            // Field 1 with wire type 7, which does not exist.
            BASE64.encode([0x0f, 0x00]).into_bytes(),
            // Field 3 (tool_invocation_id), a string that is not UTF-8.
            BASE64.encode([0x1a, 0x01, 0xff]).into_bytes(),
        ] {
            assert_eq!(
                CallMetadata::from_headers(&headers(&value)),
                refused(Invalid::Undecodable),
                "{}",
                String::from_utf8_lossy(&value)
            );
        }
        let padded = general_purpose_padded(&with_id("inv1"));
        assert!(padded.ends_with('='), "{padded}");
        assert!(matches!(
            CallMetadata::from_headers(&headers(padded.as_bytes())),
            Read::Valid(_)
        ));
    }

    fn general_purpose_padded(message: &RequestMetadata) -> String {
        base64::engine::general_purpose::STANDARD.encode(message.encode_to_vec())
    }

    /// Catches: a text field kept whole past the bound, cut inside a character (which
    /// panics), cut one byte short or long, or cut without being marked.
    #[test]
    fn text_fields_are_cut_on_a_character_boundary_and_marked() {
        let at_bound = "m".repeat(MAX_TEXT_BYTES);
        assert_eq!(Text::new(at_bound.clone()), text(&at_bound));
        assert_eq!(
            Text::new(format!("{at_bound}x")),
            Text {
                value: at_bound.clone(),
                truncated: true
            }
        );
        // 511 bytes and a two-byte character: the character does not fit.
        let short = "m".repeat(MAX_TEXT_BYTES - 1);
        assert_eq!(
            Text::new(format!("{short}\u{e9}")),
            Text {
                value: short,
                truncated: true
            }
        );
        let mut message = with_id("inv");
        message.action_mnemonic = format!("{at_bound}tail");
        let Read::Valid(read) = CallMetadata::from_headers(&encoded(&message)) else {
            panic!("refused");
        };
        assert!(read.action_mnemonic.truncated);
        assert_eq!(read.action_mnemonic.value, at_bound);
    }
}
