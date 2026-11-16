//! Small helpers of [`crate::task`]: the digests a manifest is written with and the time a Command
//! may run.

use std::time::Duration;

use kbf_proto::reapi::{self, Action};
use kbf_types::Digest;

use crate::digest;
use crate::message::FetchError;
use crate::task::Failure;

/// The digest an Action field names, or a fetch failure saying which field is missing.
pub(crate) fn named(
    what: &str,
    digest: Option<&reapi::Digest>,
) -> Result<reapi::Digest, FetchError> {
    digest
        .cloned()
        .ok_or_else(|| FetchError::Invalid(format!("the Action names no {what}")))
}

/// A result's digest as the manifest writes it.
pub(crate) fn digest_of(digest: Option<&reapi::Digest>) -> Result<Digest, Failure> {
    let digest = digest.ok_or_else(|| Failure::Result("a digest is absent".to_owned()))?;
    digest::from_reapi(digest).map_err(|e| Failure::Result(e.to_string()))
}

/// A stream's digest as the manifest writes it: absent when the stream is empty.
pub(crate) fn stdio(digest: Option<&reapi::Digest>) -> Result<Option<Digest>, Failure> {
    Ok(Some(digest_of(digest)?).filter(|d| d.size_bytes != 0))
}

/// How long the Command may run: the Action's timeout when it sets a positive one, but never more
/// than `cap`, the operator's limit; else `cap`.
pub(crate) fn deadline(action: &Action, cap: Duration) -> Duration {
    match &action.timeout {
        Some(t) if t.seconds > 0 || (t.seconds == 0 && t.nanos > 0) => {
            Duration::new(t.seconds.unsigned_abs(), t.nanos.unsigned_abs()).min(cap)
        }
        _ => cap,
    }
}

/// A duration in whole milliseconds, saturating.
pub(crate) fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}
