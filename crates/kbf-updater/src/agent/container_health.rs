//! The health of a frontend host whose frontend runs as a container, from what its helper says.
//!
//! The helper's switch returns only after the new container answered `/readyz` with 200 and
//! `/healthz` named the release's commit (`kbf-host-install --serve --frontend-container`),
//! and its `query` then says that release runs. The agent has no other way to the frontend of
//! its own host and needs none: the host is **healthy** when the commit the helper reports
//! is the target's by the rule every health check uses ([`names`]).

use std::sync::Arc;

use crate::health::{commit_of, names};
use crate::inventory::Host;
use crate::ports::{Health, HealthCheck, Installed, Installer, PortError};
use crate::version::Version;

/// Health of a frontend container, from the helper's report.
pub struct ContainerHealth {
    installer: Arc<dyn Installer>,
}

impl ContainerHealth {
    pub fn new(installer: Arc<dyn Installer>) -> Self {
        Self { installer }
    }
}

impl HealthCheck for ContainerHealth {
    fn refusal(&self, _host: &Host) -> Option<String> {
        None
    }

    fn check(&self, host: &Host, target: &Version) -> Result<Health, PortError> {
        let commit = match commit_of(target) {
            Ok(commit) => commit,
            Err(health) => return Ok(health),
        };
        let report = self.installer.report(host)?;
        if report.installed == Installed::InProgress {
            return Ok(Health::Waiting("the helper is working".to_owned()));
        }
        Ok(match report.running_commit {
            Some(built) if names(&built, commit) => Health::Healthy,
            Some(built) => Health::Waiting(format!(
                "the frontend container runs build {built}, not the target's"
            )),
            None => Health::Waiting("the helper does not say which release runs".to_owned()),
        })
    }

    /// A frontend host's own node is not held by the roll: the same as [`Self::check`].
    fn check_held(&self, host: &Host, target: &Version) -> Result<Health, PortError> {
        self.check(host, target)
    }
}
