//! The vocabulary of the update records: what is offered and ordered, and what the node says
//! back. These are plain values; nothing here knows protobuf.

use serde::{Deserialize, Serialize};

/// The most bytes one component's tarball may be.
pub const MAX_COMPONENT_BYTES: u64 = 512 << 20;

/// The most bytes one component's attestation may be.
pub const MAX_ATTESTATION_BYTES: usize = 256 << 10;

/// The components a node installs, by name. An offer naming another is refused. `kbf-daemon` is
/// the name of a component of earlier releases, which a record written by one still names.
pub const COMPONENT_NAMES: [&str; 2] = ["kbf", "kbf-daemon"];

/// One file of a release, as the offer names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Component {
    /// `kbf`, or `kbf-daemon` in a record an earlier release wrote.
    pub name: String,
    /// The SHA-256 of the tarball, as 64 lowercase hex digits; also the key the
    /// frontend serves it by.
    pub sha256: String,
    /// The tarball's length in bytes.
    pub size: u64,
    /// The build attestation of the tarball (a Sigstore bundle line), carried in the
    /// offer. The client copies it; the helper verifies it.
    pub attestation: Vec<u8>,
}

/// The release the frontend wants this node to have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    /// Grows with every change of the desired release; the node acts on a generation
    /// greater than any it has seen.
    pub generation: u64,
    /// The 40-digit commit the release was built from.
    pub commit: String,
    /// The platform the tarballs are for (`linux-x86_64`, `darwin-arm64`, ...).
    pub platform: String,
    /// What to fetch.
    pub components: Vec<Component>,
    /// What the frontend's version facts document states about the release (the offer's
    /// `platform` is the platform of these facts). Claims: the installer checks the fetched binary's
    /// own report against them. Zeros when the frontend predates the fields.
    pub expected: Expected,
}

/// The facts a release's `kbf --version --json` is expected to declare.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expected {
    pub protocol: u32,
    pub min_protocol: u32,
    pub state_schema: u32,
    /// The build profile, for example `release`.
    pub profile: String,
}

/// What an order asks the installer to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    /// Fetch the release if it is not installed, switch to it and restart.
    Activate,
    /// Keep the release that was switched to.
    Confirm,
    /// Go back to the previous release.
    Revert,
    /// The frontend's own update, which the frontend writes into its own host's spool. It
    /// is a word of the record format only: no wire message maps to it and this crate never
    /// writes it (the installer on a node host would refuse it anyway).
    #[serde(rename = "frontend_update")]
    FrontendUpdate,
}

impl Verb {
    /// The word the order file carries.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Activate => "activate",
            Self::Confirm => "confirm",
            Self::Revert => "revert",
            Self::FrontendUpdate => "frontend_update",
        }
    }
}

/// Where the release an order names comes from: the installer's configured source, or the
/// release its own package carries. It chooses between the two and never names a place: no
/// path or URL travels in an order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Pull it from the source `installer.toml` names, as every order has meant so far. This
    /// is what an order that says nothing means, so a sender writes the word only for
    /// [`Source::Staged`].
    #[default]
    Fetch,
    /// Use the release the installer's package carries (its seed): nothing is pulled, and the
    /// release goes through the same verification as a pulled one.
    Staged,
}

impl Source {
    /// The word the order file carries.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Fetch => "fetch",
            Self::Staged => "staged",
        }
    }

    /// Whether this is the default, which a record leaves out.
    #[must_use]
    pub const fn is_fetch(&self) -> bool {
        matches!(self, Self::Fetch)
    }
}

/// The frontend's order that this node change now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Order {
    /// From a counter that only grows; a node refuses one below the newest it has seen.
    pub order_id: u64,
    /// The epoch of the frontend that sent it; the node takes only its Welcome's.
    pub epoch: u64,
    /// The generation of the desired release the order belongs to (0: the frontend sent
    /// none). Carried to the installer, which holds the rule on it.
    pub generation: u64,
    /// `None` when the message named no verb.
    pub verb: Option<Verb>,
    /// The release the order is about.
    pub commit: String,
    /// How long the helper waits for a confirmation before it reverts.
    pub window_s: u32,
    /// Where the release comes from. `None` when the message named a number this build does
    /// not know: the client refuses that order. (Unlike `verb`, an unset message is
    /// `Some(Source::Fetch)`.)
    pub source: Option<Source>,
}

/// Whether `text` is exactly `len` lowercase hexadecimal digits.
#[must_use]
pub fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
