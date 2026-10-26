//! The start gate of the enroll listener (`docs/design/install-and-enrollment.md`,
//! section 8.3).
//!
//! Revoking a node writes `deny/` rows, and this build enforces them everywhere a node
//! presents a certificate: the enroll service refuses `Renew` and `Enroll` at once, storage
//! refuses the node through `Map.denied`, and the worker listener reads the rows at every
//! check beside `--worker-deny-list` ([`crate::identity::Peers::with_denied_rows`]). A
//! revoked node is therefore refused at once, not when its certificate expires.
//!
//! [`DENY_ENFORCED`] records that, and the gate is open for it. The gate stays so that a
//! build which could not enforce the rows (the marker `false`) would again refuse
//! `--enroll-listen` unless `--enroll-allow-unenforced-deny` accepted that. The flag is
//! still accepted and does nothing; it is removed in a later release.

/// Whether this build's storage and worker listener enforce the deny rows a revocation
/// writes.
pub const DENY_ENFORCED: bool = true;

/// Why the listener may not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "--enroll-listen is refused: this build's storage does not enforce the deny rows a node \
     revocation writes (DENY_ENFORCED is off), so a revoked node would still be served until \
     its certificate expires (design section 8.3). Pass \
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
