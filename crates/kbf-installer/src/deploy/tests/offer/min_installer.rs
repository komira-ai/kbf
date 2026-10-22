//! `min_installer`: an offer names the lowest installer level that may deploy it, and an
//! older installer refuses before it reads the bundle's files, uploads a byte or asks the
//! frontend for anything. The installer never updates itself, so the refusal is how an older
//! host is held with its current release still serving. As in the parent module, every
//! refusal also proves the farm was not touched.

use super::in_bundle::both_ways;
use super::*;

/// A staged `kbf` that must not be asked: a refusal before the bundle is read never runs it.
struct Never;

impl Probe for Never {
    fn meta_log(
        &self,
        _: &CommitId,
        _: &[u8],
        _: Option<&[u8]>,
    ) -> Result<Option<u64>, ProbeFault> {
        panic!("the staged kbf was run under an offer the installer is too old for");
    }

    fn running_meta_log(&self) -> Result<Option<u64>, ProbeFault> {
        panic!("the running kbf was asked under an offer the installer is too old for");
    }
}

/// Fields of an offer that needs installer level `min`.
fn needing(min: Option<u32>) -> Fields {
    let mut f = Fields::good();
    f.min_installer = min;
    f
}

/// Proves an installer below the offer's `min_installer` refuses with `offer_installer_too_old`,
/// naming both levels, for an offer given as a file and as the bundle's own: exit 3, no upload,
/// no update asked, no later step run (the bundle's files and the staged `kbf` are not looked
/// at), so the node keeps serving the release it has. Catches a check that runs after the
/// upload, one that reads the level the wrong way round, and one that is skipped for the
/// bundle's offer.
///
/// Mutants planted: the check removed; `min > level` written `min < level`; the check made only
/// in the file's path (`OfferPolicy::Offer`).
#[test]
fn an_installer_older_than_the_offer_refuses_before_anything_is_fetched_or_staged() {
    for (level, min) in [(1, 2), (1, 9), (4, 5)] {
        for (under, bundle) in both_ways(&needing(Some(min))) {
            let (f, s) = deploy_at_level(level, true, under, Some(bundle), &Never);
            refused(&f, &s, "offer_installer_too_old");
            let said = s.detail.clone();
            assert!(
                said.contains(&format!("level {min}")) && said.contains(&format!("level {level}")),
                "both levels are named: {said}"
            );
            assert!(s.offer.is_some(), "the offer is recorded: {}", s.line());
            assert_eq!(
                s.offer.as_ref().and_then(|o| o.min_installer),
                Some(min),
                "{}",
                s.line()
            );
        }
    }
}

/// Proves an installer at or above the offer's `min_installer` deploys it, and an offer with
/// no `min_installer` (one made before the field) is accepted by every level. Catches an off by
/// one (`>=` for `>`), and a missing field read as a requirement.
///
/// Mutants planted: `min > level` written `min >= level`; an absent field read as level 1 or as
/// `u32::MAX`.
#[test]
fn an_installer_at_or_above_the_offers_level_and_an_offer_without_one_are_accepted() {
    for (level, min) in [
        (2, Some(2)),
        (3, Some(2)),
        (1, Some(1)),
        (0, None),
        (1, None),
        (7, None),
    ] {
        for (under, bundle) in both_ways(&needing(min)) {
            let probe = Says(Ok(Some(4)));
            let (f, s) = deploy_at_level(level, true, under, Some(bundle), &probe);
            assert_eq!(
                (s.exit, s.reason.as_str()),
                (0, ""),
                "level {level} min {min:?}: {}",
                s.line()
            );
            assert_eq!(f.requests("PUT"), 2, "level {level} min {min:?}");
        }
    }
}

/// Proves the refusal is also made by a plan (`apply` false), so an operator sees it before the
/// deploy they run, and that a deploy of this tree's installer is at the level the tree's offers
/// name (`version::LEVEL`): the default of `Options`. Catches a check made only on apply, and
/// an `Options::new` that starts at another level.
///
/// Mutant planted: `Options::new` giving level 0.
#[test]
fn a_plan_refuses_too_and_a_new_options_is_this_installers_level() {
    let probe = Says(Ok(Some(4)));
    let (f, s) = deploy_at_level(
        1,
        false,
        policy(Stage::Gamma, needing(Some(2)).doc()),
        Some(good_bundle()),
        &probe,
    );
    refused(&f, &s, "offer_installer_too_old");
    assert_eq!(opts(true).installer_level, crate::version::LEVEL);
    let (_, ok) = with(&needing(Some(crate::version::LEVEL)));
    assert_eq!(ok.exit, 0, "{}", ok.line());
}
