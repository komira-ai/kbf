//! The commands, as argument vectors. Pure: nothing here runs anything.
//!
//! An install is two transient systemd units, one after the other, so an ssh drop that
//! kills this process cannot kill `dpkg` half way:
//!
//! ```text
//! systemd-run --wait --collect --pipe --quiet --unit=kbf-host-install-<v> ... -- \
//!     apt-get -o Dir::Etc::SourceList=<file> -o Dir::Etc::SourceParts=- update
//! systemd-run ... -- apt-get install -y --no-install-recommends ... kbf=<version>
//! ```
//!
//! Both read the one source file of the channel and no other, and the unit gets the same
//! fixed environment as every process here ([`crate::runner::PATH`], `LANG=C`) plus
//! `DEBIAN_FRONTEND=noninteractive`.

use crate::config::ChannelSource;
use crate::request::Version;
use crate::runner::Cmd;

/// The prefix of every unit this program starts; `query` asks systemd about `<prefix>*`.
pub const UNIT_PREFIX: &str = "kbf-host-install-";

/// Seconds `apt-get` waits for the dpkg lock before it gives up.
pub const LOCK_TIMEOUT_SECS: u32 = 300;

/// Seconds a unit may run before systemd stops it.
pub const RUNTIME_MAX_SECS: u32 = 1800;

/// `dpkg-query`.
pub const DPKG_QUERY: &str = "/usr/bin/dpkg-query";

/// `systemctl`.
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";

/// `systemd-run`.
pub const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";

/// `apt-get`.
pub const APT_GET: &str = "/usr/bin/apt-get";

/// The installed `kbf`.
pub const KBF: &str = "/usr/bin/kbf";

/// The unit name for installing `version`: the prefix and the version, with `+` as `_p`
/// and `~` as `_t` (neither is allowed in a unit name; `_` cannot occur in a version).
pub fn unit_name(version: &Version) -> String {
    let _ = version;
    todo!()
}

/// Read the installed version and package state of `kbf`.
pub fn installed_version() -> Cmd {
    todo!()
}

/// Ask the installed binary what build it is.
pub fn running_build() -> Cmd {
    todo!()
}

/// Ask systemd whether any install unit is active.
pub fn units_active() -> Cmd {
    todo!()
}

/// `apt-get update` of the channel's source alone, in a unit.
pub fn apt_update(source: &ChannelSource, version: &Version) -> Cmd {
    let _ = (source, version);
    todo!()
}

/// `apt-get install` of `kbf=<version>` from the channel's source alone, in a unit.
pub fn apt_install(source: &ChannelSource, version: &Version) -> Cmd {
    let _ = (source, version);
    todo!()
}

#[cfg(test)]
mod tests;
