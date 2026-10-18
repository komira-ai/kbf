//! What a refusal word in `status.json` means to the update API (KI section 5).
//!
//! The installer refuses an order with one of a fixed set of words. For a
//! `frontend_update` order every one of them ends the update the same way: phase
//! `failed`, `failure.reason` the word, and no child started, because the child is only
//! started from a staged release the compatibility gate accepted. They differ in the
//! `failure.step`, which says what the operator should look at. A word this build does
//! not know fails closed the same way, with step `stage`.

/// Every word of KI section 5, in the order the document lists them.
pub const WORDS: [&str; 18] = [
    "trust_chain",
    "hash",
    "attestation",
    "commit_or_ref",
    "facts",
    "installer_too_old",
    "platform",
    "size",
    "disk",
    "fetch",
    "replay",
    "generation",
    "floor",
    "pending",
    "lease",
    "denied",
    "envoy_range",
    "frontend_busy",
];

/// The failing step for a refusal word.
pub fn step(word: &str) -> &'static str {
    match word {
        "trust_chain" | "hash" | "attestation" | "commit_or_ref" | "facts"
        | "installer_too_old" | "platform" | "size" => "verify",
        "fetch" | "disk" => "fetch",
        "replay" | "generation" | "floor" | "denied" => "order",
        "pending" | "lease" | "envoy_range" | "frontend_busy" => "host",
        _ => "stage",
    }
}
