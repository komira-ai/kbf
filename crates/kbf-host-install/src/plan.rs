//! The commands, as argument vectors. Pure: nothing here runs anything.
//!
//! An install is ONE transient systemd unit, so an ssh drop that kills the process sshd
//! started cannot kill `dpkg` half way, and the install lock has one holder for the whole
//! install:
//!
//! ```text
//! systemd-run --wait --collect --pipe --quiet --unit=kbf-host-install-<v>.service \
//!     --property=RuntimeMaxSec=1800 --setenv=KBF_HOST_INSTALL_UNIT=<request> -- \
//!     /usr/libexec/kbf/kbf-host-install
//! ```
//!
//! The unit runs this program again, with the request in the one variable only systemd-run
//! sets ([`UNIT_ENV`]); that process takes the lock, then runs the two commands below,
//! directly (`apt-get update`, then `apt-get install -y ... kbf=<version>`), and reads the
//! package database before and after. Both apt commands read the one source file of the
//! channel and no other, and run with the fixed environment of every process here
//! ([`crate::runner::PATH`], `LANG=C`, `DEBIAN_FRONTEND=noninteractive`).

use std::path::Path;
use std::time::Duration;

use crate::config::ChannelSource;
use crate::request::{Channel, Version};
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
        ..Cmd::default()
    }
}

/// The variable that makes the program run as the body of the install unit, holding the
/// request. Only `systemd-run` sets it (see [`unit_install`]); an ssh session's is ignored.
pub const UNIT_ENV: &str = "KBF_HOST_INSTALL_UNIT";

/// Seconds `kbf --version --json` may run before `query` gives up on it.
pub const RUNNING_BUILD_TIMEOUT_SECS: u64 = 10;

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

/// Ask the installed binary what build it is. It runs as root and may hang: the runner
/// kills it after [`RUNNING_BUILD_TIMEOUT_SECS`].
pub fn running_build() -> Cmd {
    Cmd {
        timeout: Some(Duration::from_secs(RUNNING_BUILD_TIMEOUT_SECS)),
        ..cmd(KBF, &["--version", "--json"])
    }
}

/// List the install units that are not over. `is-active` is true only for `active`, and a
/// unit that is starting is `activating` (and one stopping is `deactivating`), so the
/// states are named; the answer is the listing, and an empty one means none.
pub fn units_running() -> Cmd {
    cmd(
        SYSTEMCTL,
        &[
            "list-units",
            "--type=service",
            "--state=active,activating,deactivating,reloading",
            "--plain",
            "--no-legend",
            "--no-pager",
            "--",
            "kbf-host-install-*",
        ],
    )
}

/// The install unit: this program again (`self_exe`, an absolute path) as the body of a
/// transient service that is waited for, collected and piped back, with the request in
/// [`UNIT_ENV`]. Everything else the unit needs it sets for itself.
pub fn unit_install(self_exe: &Path, version: &Version, channel: &Channel) -> Cmd {
    let mut args: Vec<String> = ["--wait", "--collect", "--pipe", "--quiet"]
        .iter()
        .map(|a| (*a).to_owned())
        .collect();
    args.push(format!("--unit={}", unit_name(version)));
    args.push(format!("--property=RuntimeMaxSec={RUNTIME_MAX_SECS}"));
    args.push(format!(
        "--setenv={UNIT_ENV}=install kbf={} from {}",
        version.as_str(),
        channel.as_str()
    ));
    args.push("--".to_owned());
    args.push(self_exe.display().to_string());
    Cmd {
        program: SYSTEMD_RUN.to_owned(),
        args,
        ..Cmd::default()
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

/// `apt-get update` of the channel's source alone. The other lists are kept:
/// `List-Cleanup=0`, since a cleanup would delete every list this source does not name.
pub fn apt_update(source: &ChannelSource) -> Cmd {
    let mut args = only_source(source).to_vec();
    args.extend([
        "-o".to_owned(),
        "APT::Get::List-Cleanup=0".to_owned(),
        "update".to_owned(),
    ]);
    Cmd {
        program: APT_GET.to_owned(),
        args,
        ..Cmd::default()
    }
}

/// `apt-get install` of `kbf=<version>` (older than the installed one too) from the channel's source alone.
pub fn apt_install(source: &ChannelSource, version: &Version) -> Cmd {
    let mut args: Vec<String> = [
        "install",
        "-y",
        "--no-install-recommends",
        // A rollback installs an older version; without this `-y` refuses a downgrade.
        "--allow-downgrades",
        "-o",
        "Dpkg::Options::=--force-confold",
        "-o",
    ]
    .iter()
    .map(|a| (*a).to_owned())
    .collect();
    args.push(format!("DPkg::Lock::Timeout={LOCK_TIMEOUT_SECS}"));
    args.extend(only_source(source));
    args.extend([
        "-t".to_owned(),
        source.suite.clone(),
        format!("kbf={}", version.as_str()),
    ]);
    Cmd {
        program: APT_GET.to_owned(),
        args,
        ..Cmd::default()
    }
}

#[cfg(test)]
mod tests;
