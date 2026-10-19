//! The start gate of the enroll listener (`docs/design/install-and-enrollment.md`,
//! section 8.3).
//!
//! Revoking a node writes `deny/` rows, but a revoked node is only refused at once by the
//! enroll service itself (`Renew` and `Enroll`). The worker listener and a storage node
//! enforce the deny rows only when the revocation unit (B3c: the store-backed deny source,
//! `Map.denied` and the storage admission) has landed. Until then a node that was revoked
//! keeps being served by storage until its certificate expires, which is a promise the
//! operator should not be given by a listener switched on by mistake.
//!
//! [`DENY_ENFORCED`] is the marker B3c sets to `true`, in the change that makes storage
//! apply `Map.denied`. While it is `false`, `--enroll-listen` is refused at start unless
//! `--enroll-allow-unenforced-deny` is also given, which is for development, tests and
//! a farm that accepts "refuse renewal, then wait for the certificate to expire" as the
//! meaning of revocation (the operator guide says so).

/// Whether this build's storage and worker listener enforce the deny rows a revocation
/// writes. B3c sets it.
pub const DENY_ENFORCED: bool = false;

/// Why the listener may not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "--enroll-listen is refused: this build's storage does not enforce the deny rows a node \
     revocation writes (DENY_ENFORCED is off), so a revoked node would still be served by \
     storage until its certificate expires (design section 8.3). Pass \
     --enroll-allow-unenforced-deny to run it anyway, for development or a farm that accepts \
     that"
)]
pub struct DenyNotEnforced;

/// Whether the listener may start: when the build enforces deny rows, or the operator
/// accepted that it does not.
///
/// # Errors
/// [`DenyNotEnforced`].
pub const fn check(enforced: bool, allowed: bool) -> Result<(), DenyNotEnforced> {
    if enforced || allowed {
        Ok(())
    } else {
        Err(DenyNotEnforced)
    }
}

/// [`check`] for this build.
///
/// # Errors
/// [`DenyNotEnforced`].
pub const fn start_gate(allowed: bool) -> Result<(), DenyNotEnforced> {
    check(DENY_ENFORCED, allowed)
}

#[cfg(test)]
mod tests;
