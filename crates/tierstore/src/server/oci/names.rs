//! What a request's path names: the route, the repository name, a digest and a tag.
//!
//! The grammars are those of the OCI distribution specification. The name of the repository in
//! a path is checked against its grammar and then ignored: the store is one content-addressed
//! namespace, and a key is built only from a [`Digest`], which holds 64 lowercase hex digits and
//! nothing else, so no path can name a key outside the prefix.

/// The longest repository name the specification allows.
const MAX_NAME_BYTES: usize = 255;

/// The longest tag the specification allows.
const MAX_TAG_BYTES: usize = 128;

/// A `sha256` digest, held as its 64 lowercase hex digits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Digest(String);

impl Digest {
    /// The digest `text` writes as `sha256:<64 lowercase hex digits>`; anything else (another
    /// algorithm, upper case, a short or long hex string) is `None`.
    pub(super) fn parse(text: &str) -> Option<Self> {
        let hex = text.strip_prefix("sha256:")?;
        let lower = hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        (hex.len() == 64 && lower).then(|| Self(hex.to_owned()))
    }

    /// The digest of `bytes`, from the hex of its SHA-256.
    pub(super) fn of_hex(hex: String) -> Self {
        Self(hex)
    }

    /// The 64 hex digits.
    pub(super) fn hex(&self) -> &str {
        &self.0
    }

    /// `sha256:<hex>`, as a client writes and reads it.
    pub(super) fn text(&self) -> String {
        format!("sha256:{}", self.0)
    }
}

/// Which call a path is.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Route<'a> {
    /// `/v2/`.
    Ping,
    /// `/v2/<name>/manifests/<reference>`.
    Manifest { name: &'a str, reference: &'a str },
    /// `/v2/<name>/blobs/<digest>`.
    Blob { name: &'a str, digest: &'a str },
    /// Anything else.
    Other,
}

/// The route of `path`. The name is the part between `/v2/` and the last two segments, so a
/// name may hold slashes, and a component of it may be called `blobs`.
pub(super) fn route(path: &str) -> Route<'_> {
    let Some(rest) = path.strip_prefix("/v2/") else {
        return Route::Other;
    };
    if rest.is_empty() {
        return Route::Ping;
    }
    let parts = rest
        .rsplit_once('/')
        .and_then(|(head, last)| Some((head.rsplit_once('/')?, last)));
    match parts {
        Some(((name, "manifests"), reference)) => Route::Manifest { name, reference },
        Some(((name, "blobs"), digest)) => Route::Blob { name, digest },
        _ => Route::Other,
    }
}

/// Whether `name` is a repository name: path components of lowercase letters and digits,
/// joined by `.`, `_`, `__` or runs of `-`, at most 255 bytes.
pub(super) fn valid_name(name: &str) -> bool {
    name.len() <= MAX_NAME_BYTES && name.split('/').all(valid_component)
}

/// Whether `component` is `[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*`.
fn valid_component(component: &str) -> bool {
    let mut rest = component.as_bytes();
    loop {
        let words = rest
            .iter()
            .take_while(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9'))
            .count();
        if words == 0 {
            return false;
        }
        rest = &rest[words..];
        let Some(&first) = rest.first() else {
            return true;
        };
        let skip = match first {
            b'.' => 1,
            b'_' if rest.get(1) == Some(&b'_') => 2,
            b'_' => 1,
            b'-' => rest.iter().take_while(|&&b| b == b'-').count(),
            _ => return false,
        };
        rest = &rest[skip..];
    }
}

/// Whether `text` is a tag: `[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`.
pub(super) fn is_tag(text: &str) -> bool {
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let bytes = text.as_bytes();
    let tail = |b: &u8| word(*b) || matches!(b, b'.' | b'-');
    bytes.len() <= MAX_TAG_BYTES
        && bytes.first().copied().is_some_and(word)
        && bytes.iter().all(tail)
}
