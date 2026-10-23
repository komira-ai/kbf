//! What the node health check ([`crate::node_health`]) rests on: the one way a version is
//! tied to a commit.
//!
//! # A version and its commit
//!
//! The roll names a package [`Version`]; the hosts report commits. The link is the version
//! itself: a package of the series is `0.1.<seq>+g<sha12>-<run>` (`xtask`'s `pkg_version`),
//! so it carries the first 12 hex digits of its commit ([`Version::commit`]). A reported
//! commit matches when it begins with those digits ([`names`]); a proxy says 40 digits, a
//! storage node 40 or 12. A version that names no commit cannot be verified against a host,
//! so a host is not called healthy by it (48 bits of prefix: the target is chosen by a
//! trusted workflow, not by an attacker who could grind a collision).

use crate::ports::Health;
use crate::version::Version;

/// Whether `observed`, a commit a host reported (40 hex digits, 12, or `unknown`), is the
/// commit that begins with `commit` (hex digits, from [`Version::commit`]). A report shorter
/// than `commit` names less than the target and so is not it.
pub fn names(observed: &str, commit: &str) -> bool {
    observed
        .get(..commit.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(commit))
}

/// The commit `target` names, or the answer that says the host cannot be judged by it.
pub fn commit_of(target: &Version) -> Result<&str, Health> {
    target.commit().ok_or_else(|| {
        Health::Abandoned(format!(
            "the version {target} names no commit (+g<12 hex digits>), so no host can be shown to run it"
        ))
    })
}
