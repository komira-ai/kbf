//! The installer and the health check that this build does not have.
//!
//! The ssh installer is a separate change and the health check is the next one. Until they
//! land, the binary is wired with these two, which refuse everything: a roll is accepted over
//! the API and fails at the first host's install, with a clear reason and before any host is
//! touched. A binary that is wired this way cannot change a host.

use crate::inventory::Host;
use crate::ports::{Health, HealthCheck, Installed, Installer, PortError};
use crate::version::Version;

/// What every refusal says.
pub const NOT_BUILT: &str =
    "this build of kbf-updater has no host installer yet; no host was touched";

/// An [`Installer`] that refuses every call.
pub struct NotBuiltInstaller;

impl Installer for NotBuiltInstaller {
    fn install(&self, _host: &Host, _version: &Version) -> Result<(), PortError> {
        Err(PortError::other(NOT_BUILT))
    }

    fn query(&self, _host: &Host) -> Result<Installed, PortError> {
        Err(PortError::other(NOT_BUILT))
    }
}

/// A [`HealthCheck`] that refuses every call.
pub struct NotBuiltHealth;

impl HealthCheck for NotBuiltHealth {
    fn check(&self, _host: &Host, _target: &Version) -> Result<Health, PortError> {
        Err(PortError::other(
            "this build of kbf-updater has no health check yet",
        ))
    }
}
