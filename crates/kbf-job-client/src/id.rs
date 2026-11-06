//! The job id, as the client checks it before anything is sent (design section 5.3): 1 to
//! 63 bytes of letters, digits, dot, underscore and dash. The service is the authority and
//! checks it again; this check only fails early with a clear error.

use crate::error::BadId;

/// The longest job id, in bytes.
pub(crate) const MAX_ID: usize = 63;

/// Whether `id` is a job id the service takes.
pub(crate) fn check(id: &str) -> Result<(), BadId> {
    if id.is_empty() || id.len() > MAX_ID {
        return Err(BadId::Length(id.len()));
    }
    let allowed = |c: &char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    match id.chars().find(|c| !allowed(c)) {
        Some(c) => Err(BadId::Character(c)),
        None => Ok(()),
    }
}

#[cfg(test)]
#[path = "id_tests.rs"]
mod tests;
