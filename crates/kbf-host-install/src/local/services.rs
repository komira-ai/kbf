//! The services other than kbf, behind the one socket.
//!
//! kbf's update is the [`super::core::Ab`] the helper is started with. Another service has
//! an `Ab` of its own, over a backend that lays its releases out in its own tree, with its
//! own state file (`state-<service>.json`, beside kbf's) and its own install lock. They are
//! made when a request or a tick asks, from the descriptors the root-owned configuration has
//! at that moment ([`describe`]), so a service added to the file is served at once, and one
//! removed from it is refused; nothing is held between requests but the files. A request for
//! a service the configuration does not describe is refused with
//! [`Code::ServiceNotConfigured`] and nothing runs.
//!
//! The deadline of a release on trial is kept by [`Services::tick`], as kbf's is: it asks
//! each described service whether a trial has run out. A configuration that cannot be read at
//! that moment hides the trials until the next tick finds it again; the markers are on disk.

use super::answer::{Answer, Code};
use super::request::LocalRequest;
use crate::service::ServiceName;

/// The services of a socket other than kbf.
pub trait Services {
    /// Answer a request for `service` (which is not kbf).
    fn handle(&self, service: &ServiceName, request: &LocalRequest) -> Answer;

    /// Go back, for each service with a release on trial past its deadline: what it did.
    fn tick(&self) -> Vec<(ServiceName, Answer)>;
}

/// A helper that serves kbf alone: every other service is refused.
pub struct NoServices;

impl Services for NoServices {
    fn handle(&self, service: &ServiceName, _request: &LocalRequest) -> Answer {
        not_configured(service)
    }

    fn tick(&self) -> Vec<(ServiceName, Answer)> {
        Vec::new()
    }
}

pub(super) fn not_configured(service: &ServiceName) -> Answer {
    Answer::Refused(
        Code::ServiceNotConfigured,
        format!("no service {} is configured here", service.as_str()),
    )
}
