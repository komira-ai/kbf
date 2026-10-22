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

use std::path::Path;

use crate::config::ChannelSource;
use crate::request::{Channel, Version};
use crate::runner::{Cmd, LANG, PATH};

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
/// and `~` as `_t` (neither is allowed in a unit name; `_` cannot occur in a version),
/// and always `.service`, so a version ending `.timer` or `.mount` is not a unit of that
/// type. The request grammar bounds the length (`MAX_UNIT_VERSION_LEN`).
pub fn unit_name(version: &Version) -> String {
    let mut name = String::from(UNIT_PREFIX);
    for c in version.as_str().chars() {
        match c {
            '+' => name.push_str("_p"),
            '~' => name.push_str("_t"),
            c => name.push(c),
        }
    }
    name.push_str(".service");
    name
}

fn cmd(program: &str, args: &[&str]) -> Cmd {
    Cmd {
        program: program.to_owned(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        timeout: None,
    }
}

/// The variable that makes the program run as the body of the install unit; only
/// `systemd-run` sets it (see [`unit_install`]).
pub const UNIT_ENV: &str = "KBF_HOST_INSTALL_UNIT";

/// Seconds `kbf --version --json` may run before `query` gives up on it.
pub const RUNNING_BUILD_TIMEOUT_SECS: u64 = 10;

/// The install unit: this program again, as the body of a transient unit.
pub fn unit_install(_self_exe: &Path, _version: &Version, _channel: &Channel) -> Cmd {
    cmd(SYSTEMD_RUN, &[])
}

/// Ask systemd whether any install unit exists in a state that is not over.
pub fn units_running() -> Cmd {
    units_active()
}

/// Read the installed version and package state of `kbf`: `<status> <version>`.
pub fn installed_version() -> Cmd {
    cmd(
        DPKG_QUERY,
        &[
            "--show",
            "--showformat=${db:Status-Abbrev} ${Version}\\n",
            "kbf",
        ],
    )
}

/// Ask the installed binary what build it is.
pub fn running_build() -> Cmd {
    cmd(KBF, &["--version", "--json"])
}

/// Ask systemd whether any install unit is active.
pub fn units_active() -> Cmd {
    cmd(
        SYSTEMCTL,
        &["is-active", "--quiet", "--", "kbf-host-install-*"],
    )
}

/// `systemd-run` around `inner`: waits for the unit, collects it, pipes its output, and
/// gives it the fixed environment (the unit does not inherit this process's).
fn in_unit(version: &Version, inner: Vec<String>) -> Cmd {
    let mut args: Vec<String> = ["--wait", "--collect", "--pipe", "--quiet"]
        .iter()
        .map(|a| (*a).to_owned())
        .collect();
    args.push(format!("--unit={}", unit_name(version)));
    args.push(format!("--property=RuntimeMaxSec={RUNTIME_MAX_SECS}"));
    args.push(format!("--setenv=PATH={PATH}"));
    args.push(format!("--setenv=LANG={LANG}"));
    args.push("--setenv=DEBIAN_FRONTEND=noninteractive".to_owned());
    args.push("--".to_owned());
    args.push(APT_GET.to_owned());
    args.extend(inner);
    Cmd {
        program: SYSTEMD_RUN.to_owned(),
        args,
        timeout: None,
    }
}

/// The options that restrict `apt-get` to the channel's one source file.
fn only_source(source: &ChannelSource) -> [String; 4] {
    [
        "-o".to_owned(),
        format!("Dir::Etc::SourceList={}", source.source_list.display()),
        "-o".to_owned(),
        "Dir::Etc::SourceParts=-".to_owned(),
    ]
}

/// `apt-get update` of the channel's source alone, in a unit. The other lists are kept:
/// `List-Cleanup=0`, since a cleanup would delete every list this source does not name.
pub fn apt_update(source: &ChannelSource) -> Cmd {
    let version = &Version::new("0").unwrap();
    let mut inner = only_source(source).to_vec();
    inner.extend([
        "-o".to_owned(),
        "APT::Get::List-Cleanup=0".to_owned(),
        "update".to_owned(),
    ]);
    in_unit(version, inner)
}

/// `apt-get install` of `kbf=<version>` from the channel's source alone, in a unit.
pub fn apt_install(source: &ChannelSource, version: &Version) -> Cmd {
    let mut inner: Vec<String> = [
        "install",
        "-y",
        "--no-install-recommends",
        "-o",
        "Dpkg::Options::=--force-confold",
        "-o",
    ]
    .iter()
    .map(|a| (*a).to_owned())
    .collect();
    inner.push(format!("DPkg::Lock::Timeout={LOCK_TIMEOUT_SECS}"));
    inner.extend(only_source(source));
    inner.extend([
        "-t".to_owned(),
        source.suite.clone(),
        format!("kbf={}", version.as_str()),
    ]);
    in_unit(version, inner)
}

#[cfg(test)]
mod tests;
