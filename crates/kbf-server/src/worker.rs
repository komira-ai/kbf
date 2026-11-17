//! The `kbf.worker.v1` versions this build speaks.
//!
//! [`PROTOCOL_VERSION`] and [`OLDEST_PROTOCOL_VERSION`] are the versions this build speaks and
//! the oldest it accepts: `kbf --version --json` reports them, and the registration of a
//! storage node names them.

/// The `kbf.worker.v1` version this server speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The oldest version it accepts. The protocol promises N-1 and N; version 1 is the
/// first, so there is no N-1 yet.
pub const OLDEST_PROTOCOL_VERSION: u32 = 1;

/// The protocol version a wire field names: `None` for 0. Versions start at 1, and
/// proto3 cannot tell a 0 from a field the sender never set (a `Hello` or a storage
/// registration from a build that predates the field, or a stored row written before
/// the field existed), so a 0 is a node whose version is not known and not a node that
/// speaks version 0.
#[must_use]
pub const fn named_protocol(wire: u32) -> Option<u32> {
    if wire == 0 { None } else { Some(wire) }
}
