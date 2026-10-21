//! Rule `credential`, moved from `kbf-deploy linux check-units`: the part that reads the
//! host. A build node does not hold the cold store's key (the units' text is checked by
//! `crates/kbf-integration-test/tests/package_units.rs`: no `EnvironmentFile=`, no
//! `AWS_*` in `Environment=`, no systemd credential).
//!
//! check-units read each file the unit's `EnvironmentFile=` lines named. The units are
//! static now and name none, so kbf reads what the process has:
//!
//! - a mode with no storage half (`daemon`, and `all-in-one` with a file cold store) has no
//!   `AWS_*` variable in its environment, from whatever file or unit line it came
//!   ([`KeyRule::Absent`]): a build node talks to no AWS-style store, and its builds run as
//!   its user;
//! - the key file, `s3.env` beside `kbf.toml` ([`super::key_file`], where a node still on a
//!   legacy unit keeps it), if it is there: a plain file (a link is not followed, so what
//!   it holds is unknown). The storage node's ([`KeyRule::Holder`]) is root's or the user
//!   kbf runs as, with no group or other bit. The key beside builds
//!   ([`KeyRule::RootAlone`], `storage-and-daemon`) is root's alone, mode 0600: systemd
//!   reads it as root, and the user the builds run as must not be able to replace it. A
//!   missing file is no fault (a node gets its key from the frontend), and one that cannot
//!   be read is a warning.
//!
//! What check-units read that kbf cannot: a link to an environment file or a wildcard
//! (`EnvironmentFile=` lines are the unit's, not kbf's settings).

use std::io;
use std::path::Path;

use super::host::Kind;
use super::{Host, KeyRule, Needs, Report};

/// The prefix of the variables an AWS SDK or CLI reads.
const AWS_PREFIX: &str = "AWS_";

pub(super) fn check(needs: &Needs, host: &dyn Host, report: &mut Report) {
    match &needs.key {
        KeyRule::None => {}
        KeyRule::Absent => aws_variables(host, report),
        KeyRule::Holder(path) => key_file(host, path, report, Owner::RootOr(host.euid())),
        KeyRule::RootAlone(path) => key_file(host, path, report, Owner::Root),
    }
}

/// A build node holds no credential: no `AWS_*` variable, which would be a copy of the key
/// or point an SDK at one.
fn aws_variables(host: &dyn Host, report: &mut Report) {
    let mut names: Vec<String> = host
        .var_names()
        .into_iter()
        .filter(|n| n.starts_with(AWS_PREFIX))
        .collect();
    if names.is_empty() {
        return;
    }
    names.sort();
    report.refuse(
        format!("the environment sets {}", names.join(", ")),
        "a build node holds no credential, and its builds run as its user, who can read what kbf reads",
        "remove it where it comes from (the unit's Environment=, EnvironmentFile= or \
         PassEnvironment=): kbf gets a cold store key from the frontend, not from its environment"
            .to_owned(),
    );
}

/// Who may own the key file.
#[derive(Clone, Copy)]
enum Owner {
    /// Root, or the user kbf runs as (the storage node's key).
    RootOr(u32),
    /// Root alone.
    Root,
}

/// The key file at `path`, if there is one: a plain file, owned by `owner`, with no group or
/// other bit.
fn key_file(host: &dyn Host, path: &Path, report: &mut Report, owner: Owner) {
    let at = path.display();
    let stat = match host.lstat(path) {
        Ok(lstat) => lstat.stat,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
        Err(e) => {
            report.warnings.push(format!(
                "credential: cannot read {at} ({e}), so its owner and mode were not checked"
            ));
            return;
        }
    };
    let why = "the cold store's key is the storage node's user's alone, never a build's";
    if stat.kind != Kind::File {
        report.refuse(
            format!("{at} is not a plain file (a link or a directory)"),
            why,
            format!("make {at} a plain file holding the key, root's, mode 0600"),
        );
        return;
    }
    let owned = |name: String, can: &str, report: &mut Report| {
        report.refuse(
            format!(
                "{at} is uid {}'s, {name}: that user can {can} the key",
                stat.uid
            ),
            why,
            format!("chown root {at}"),
        );
    };
    match owner {
        Owner::Root if stat.uid != 0 => owned("not root's".to_owned(), "replace", report),
        Owner::RootOr(me) if stat.uid != 0 && stat.uid != me => owned(
            format!("neither root's nor uid {me}'s, the user kbf runs as"),
            "read or replace",
            report,
        ),
        _ => {}
    }
    if stat.mode & 0o077 != 0 {
        let who = match owner {
            Owner::Root => "other users".to_owned(),
            Owner::RootOr(me) => format!("users other than uid {me}"),
        };
        report.refuse(
            format!("{at} is mode {:04o}, so {who} can read the key", stat.mode),
            why,
            format!("chmod 600 {at}"),
        );
    }
}

#[cfg(test)]
#[path = "credential_tests.rs"]
mod tests;
