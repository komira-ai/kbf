//! Rule `credential`, moved from `kbf-deploy linux check-units`: the part that reads the
//! host.
//!
//! check-units read each file the unit's `EnvironmentFile=` lines named. The units are
//! static now and name none, so kbf reads what the process has: the key file, `s3.env` beside
//! `kbf.toml` ([`super::key_file`], where a node still on a legacy unit keeps it), if it is
//! there, is a plain file (a link is not followed, so what it holds is unknown). The storage
//! node's ([`KeyRule::Holder`]) is root's or the user kbf runs as, with no group or other bit. A
//! missing file is no fault (a node gets its key from the frontend), and one that cannot be
//! read is a warning.
//!
//! What check-units read that kbf cannot: a link to an environment file or a wildcard
//! (`EnvironmentFile=` lines are the unit's, not kbf's settings).

use std::io;
use std::path::Path;

use super::host::Kind;
use super::{Host, KeyRule, Needs, Report};

pub(super) fn check(needs: &Needs, host: &dyn Host, report: &mut Report) {
    match &needs.key {
        KeyRule::None => {}
        KeyRule::Holder(path) => key_file(host, path, report, host.euid()),
    }
}

/// The key file at `path`, if there is one: a plain file, owned by root or by `me` (the user kbf
/// runs as), with no group or other bit.
fn key_file(host: &dyn Host, path: &Path, report: &mut Report, me: u32) {
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
    let why = "the cold store's key is the storage node's user's alone";
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
    if stat.uid != 0 && stat.uid != me {
        owned(
            format!("neither root's nor uid {me}'s, the user kbf runs as"),
            "read or replace",
            report,
        );
    }
    if stat.mode & 0o077 != 0 {
        report.refuse(
            format!(
                "{at} is mode {:04o}, so users other than uid {me} can read the key",
                stat.mode
            ),
            why,
            format!("chmod 600 {at}"),
        );
    }
}

#[cfg(test)]
#[path = "credential_tests.rs"]
mod tests;
