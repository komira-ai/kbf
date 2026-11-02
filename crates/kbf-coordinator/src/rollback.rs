//! The coordinator releases an operator keeps to go back to (`--rollback-target`), and the
//! services-file fields that a release before them cannot read.
//!
//! The coordinator is a container image updated by hand: going back means naming a kept,
//! older image in its unit, which then reads the same services file. A release before slot
//! pools refuses a file that names one (every table of the file refuses an unknown field), so
//! a file that names a pool is refused here while any listed target is a release before
//! [`FIRST_POOL_VERSION`], or is not a release version at all. With no target listed the file
//! is accepted: the list is declared by the operator, not discovered.

use kbf_updater::version::Version;

use crate::services::{Services, ServicesError};

/// The first coordinator release that reads `pool` in a services file. The release that
/// carries this check is that release.
pub const FIRST_POOL_VERSION: &str = "0.0.61";

/// [`FIRST_POOL_VERSION`]'s [`Version::release_numbers`] (a test holds the two together).
const FIRST_POOL: [u64; 3] = [0, 0, 61];

/// `text` as a release version: one [`Version`] that starts with at least two numbers joined
/// by single dots, each of one or more digits, and has nothing after them or a suffix that
/// starts with `+`, `-` or `~` (`0.0.57`, `0.0.57+g0123456789ab-8`). A bare commit id, a
/// word, an empty number (`0..60`, `.0.60`, `0.0.60.`) or digits run into letters
/// (`0.0.61abc`) is none: [`Version::release_numbers`] would order each of them as some
/// release, and a wrong order here would let a release that refuses `pool` through.
fn release(text: &str) -> Option<Version> {
    let version = Version::parse(text).ok()?;
    let end = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (lead, rest) = text.split_at(end);
    let numbers: Vec<&str> = lead.split('.').collect();
    let well_formed = numbers.len() >= 2
        && numbers.iter().all(|n| !n.is_empty())
        && (rest.is_empty() || rest.starts_with(['+', '-', '~']));
    well_formed.then_some(version)
}

/// Refuses `services` when an entry of its file names a pool and a target of `targets` (the
/// `--rollback-target` values, in order) cannot read it: a release before
/// [`FIRST_POOL_VERSION`], or a text that is not a release version. The error names the
/// first such target and the first pool of the file (service, role and pool).
///
/// # Errors
/// [`ServicesError::Invalid`], as above.
pub fn check(services: &Services, targets: &[String]) -> Result<(), ServicesError> {
    let Some((service, role, pool)) = services.pools_from_file().into_iter().next() else {
        return Ok(());
    };
    for target in targets {
        let why = match release(target) {
            None => {
                format!("--rollback-target {target:?} is not a release version (such as 0.0.57)")
            }
            Some(version) if version.release_numbers().as_slice() < FIRST_POOL.as_slice() => {
                format!(
                    "the rollback target {target} (--rollback-target) is a release before {FIRST_POOL_VERSION} and refuses the field `pool`"
                )
            }
            Some(_) => continue,
        };
        return Err(ServicesError::Invalid(format!(
            "{service}: pool {pool:?} on {role}: {why}; remove the pool, or the target once its image is no longer kept"
        )));
    }
    Ok(())
}
