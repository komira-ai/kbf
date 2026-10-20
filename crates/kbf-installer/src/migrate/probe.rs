//! What the binary of an old release says about itself: the one place the migration
//! learns which commit a release is. The name of a directory or a link, and the old
//! state, are things `kbf` or an old tool could have written; the build's own answer to
//! `--version --json`, run as the unprivileged fetch user and bounded, is not taken from
//! any of them.

use std::path::PathBuf;
use std::time::Duration;

use super::error::MigrateError;
use crate::CommitId;
use crate::binary::Version;
use crate::refusal::VersionFault;
use crate::run::{RunAs, Runner, Spawn};

/// The most the probe may take (contract section 5: the probe's bound).
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Run `program --version --json` as `run_as`, with an empty environment and no
/// credentials, and check that the build is the release build of the commit whose first
/// 12 digits it reports and that `named` (the directory it was found in) continues.
pub fn probe(
    runner: &dyn Runner,
    program: PathBuf,
    run_as: RunAs,
    named: &CommitId,
) -> Result<Version, MigrateError> {
    let release = named.to_string();
    let fail = |why: String| MigrateError::Probe {
        release: release.clone(),
        why,
    };
    let spawn = Spawn {
        program,
        args: vec!["--version".into(), "--json".into()],
        env: Vec::new(),
        cwd: PathBuf::from("/"),
        run_as: Some(run_as),
        timeout: TIMEOUT,
    };
    let ran = runner.run(&spawn).map_err(|e| fail(e.to_string()))?;
    if ran.timed_out {
        return Err(fail(format!("no answer in {} s", TIMEOUT.as_secs())));
    }
    if ran.code != Some(0) {
        return Err(fail(format!("exit status {:?}", ran.code)));
    }
    let version = Version::parse(&ran.stdout).map_err(|e| fail(e.to_string()))?;
    match version.check(named) {
        Ok(()) => Ok(version),
        Err(VersionFault::Commit { got, .. }) => Err(MigrateError::Mislabeled {
            dir: release.clone(),
            probed: got,
        }),
        Err(other) => Err(fail(other.to_string())),
    }
}
