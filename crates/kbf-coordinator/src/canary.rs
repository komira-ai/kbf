//! The canary gate of a service.

/// The hosts of a service that go alone first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Canary {
    /// How many hosts.
    pub count: u32,
}
