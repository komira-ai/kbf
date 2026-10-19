//! The operator API's DTOs for the deny list (`docs/api.md`,
//! `GET /v1/denied`): what the frontend answers, in the one
//! spelling the server and a client share.

use serde::{Deserialize, Serialize};

/// One entry of the storage deny list, as `GET /v1/denied` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeniedEntry {
    /// The entry in its canonical spelling: `serial <hex>`, `spki-sha256 <64 hex>` or
    /// `node <id>`, the text `POST` and `DELETE /v1/denied` take.
    pub entry: String,
    /// Why the operator denied it.
    pub reason: String,
    /// When it was denied: milliseconds on the state store's clock (Unix time on a
    /// running frontend).
    pub at_ms: u64,
}

/// The answer to `GET /v1/denied`: every entry, serials first, then key hashes, then node
/// ids, each group in order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeniedList {
    /// The entries.
    pub denied: Vec<DeniedEntry>,
}
