//! R4 of `docs/design/frontend-proxy.md` section 9: nodes are never driven to a release other
//! than the frontend's own, unless that is an explicit rollback.
//!
//! Commits have no order (a [`Release`] is compared for equality only), so "a rollback" is
//! not "an older commit": it is **any release other than the one the holder was built from**.
//! A downgrade is a rollback and is allowed; it is recorded on the `update/desired` row
//! ([`Desired::rollback`]) by an offer that says so. What R4 refuses is the other thing: a
//! frontend that moves the farm to a release that is neither its own nor stated as a
//! rollback, and a controller that drives nodes to such a row, as an old child does when the
//! proxy has rolled the frontend back after the new child wrote the next release.
//!
//! One type holds the rule for both sides, so the two cannot drift: [`OwnCommit::rollback_of`]
//! for a writer of the row ([`Rows`](super::Rows)) and [`OwnCommit::drives`] for the
//! controller ([`Controller`](super::Controller)).

use super::codec::Desired;
use super::facts::Release;

/// The length of a build's commit stamp (`crate::BUILD_COMMIT`): 12 lowercase hex digits.
const DIGITS: usize = 12;

/// The commit the holder was built from: the stamp of `crate::BUILD_COMMIT`, which is a
/// prefix of the 40-digit commit of the release it stands for.
///
/// A stamp that is not 12 lowercase hex digits (`unknown`, the stamp of a build with no git
/// checkout) names no release: such a holder owns nothing, so it drives and offers only what
/// a row states as a rollback. Every unknown fails closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnCommit(String);

impl OwnCommit {
    /// The holder built from `stamp`.
    #[must_use]
    pub fn new(stamp: impl Into<String>) -> Self {
        Self(stamp.into())
    }

    /// The holder this process is: the commit it was built from.
    #[must_use]
    pub fn this_build() -> Self {
        Self::new(crate::BUILD_COMMIT)
    }

    /// The stamp, as the build carries it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `release` is the holder's own: the stamp is a well-formed one and `release`
    /// begins with it.
    #[must_use]
    pub fn owns(&self, release: &Release) -> bool {
        let hex = self
            .0
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        self.0.len() == DIGITS && hex && release.as_str().starts_with(&self.0)
    }

    /// Whether a controller of this holder may drive nodes to `desired`: the release is its
    /// own, or the row says it is a rollback.
    #[must_use]
    pub fn drives(&self, desired: &Desired) -> bool {
        desired.rollback || self.owns(&desired.commit)
    }

    /// The `rollback` the row records for an offer of `commit` that asked for `asked`, or
    /// `None` when the offer is refused. The holder's own release is not a rollback, whatever
    /// the offer said (the row is accurate); any other needs `asked`, and no offer is refused
    /// for being one.
    #[must_use]
    pub fn rollback_of(&self, commit: &Release, asked: bool) -> Option<bool> {
        if self.owns(commit) {
            Some(false)
        } else {
            asked.then_some(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use kbf_types::FarmTime;

    use super::super::facts::Generation;
    use super::*;

    const OWN: &str = "0123456789ab";
    const FULL: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn row(commit: &str, rollback: bool) -> Desired {
        Desired {
            generation: Generation(1),
            commit: Release::new(commit),
            set_at: FarmTime::from_millis(1),
            rollback,
        }
    }

    /// Catches an own release judged by equality (the stamp is 12 digits and the release 40,
    /// so the equal case is red), a prefix shorter than the stamp, a release that merely
    /// contains the stamp, and a stamp that is not a stamp (`unknown`, upper case, short,
    /// long, empty) owning anything, among them a release that begins with the same text.
    /// Mutants seen red: `==` for `starts_with`; the length check dropped; the hex check
    /// dropped.
    #[test]
    fn a_release_is_owned_by_the_prefix_a_build_is_stamped_with_and_nothing_else_is() {
        let own = OwnCommit::new(OWN);
        assert!(own.owns(&Release::new(FULL)));
        assert!(own.owns(&Release::new(OWN)), "the stamp stands for itself");
        assert!(!own.owns(&Release::new(OTHER)));
        assert!(!own.owns(&Release::new(&FULL[1..])), "not a prefix");
        assert!(
            !own.owns(&Release::new(format!("0123{}", &OTHER[4..]))),
            "four digits of twelve"
        );
        assert!(
            !own.owns(&Release::new(format!("f{FULL}"))),
            "only contained"
        );
        assert!(
            !own.owns(&Release::new("0123456789a")),
            "shorter than the stamp"
        );
        for stamp in [
            "unknown",
            "0123456789AB",
            "0123456789a",
            "0123456789abc",
            "",
        ] {
            assert!(
                !OwnCommit::new(stamp).owns(&Release::new(format!("{stamp}{FULL}"))),
                "{stamp:?} names no release"
            );
        }
        // Not all of the stamp's characters are hex.
        assert!(!OwnCommit::new("0123456789ag").owns(&Release::new("0123456789agaaaaaaaa")));
    }

    /// Catches a controller that drives only for the release it owns (a rollback row is
    /// refused), one that drives for any row (a foreign row passes), and one that reads the
    /// flag as the permission to drive a release of nobody's choosing for a build that owns
    /// nothing. Mutants seen red: `||` for `&&`; the flag ignored (`drives` is `owns`).
    #[test]
    fn a_controller_drives_for_its_own_release_or_a_rollback_and_for_nothing_else() {
        let own = OwnCommit::new(OWN);
        assert!(own.drives(&row(FULL, false)));
        assert!(own.drives(&row(FULL, true)), "its own, flagged or not");
        assert!(own.drives(&row(OTHER, true)), "a rollback");
        assert!(
            !own.drives(&row(OTHER, false)),
            "another release, not a rollback"
        );
        let unknown = OwnCommit::new("unknown");
        assert!(!unknown.drives(&row(FULL, false)), "owns nothing");
        assert!(unknown.drives(&row(FULL, true)), "a rollback is a rollback");
    }

    /// Catches an offer judged wrong: the own release records `false` even when it asked
    /// for a rollback; another needs the ask and records `true`; without it the offer is
    /// refused; a holder that owns nothing refuses every offer that does not ask. Mutants
    /// seen red: the refusal dropped (`Some(asked)` for another release: it is written
    /// without the ask); `asked` returned as it came for the own release (written as a
    /// rollback); the ask written as `false` (a rollback recorded as none).
    #[test]
    fn an_offer_records_a_rollback_only_for_a_release_that_is_not_the_holders_own() {
        let own = OwnCommit::new(OWN);
        let (full, other) = (Release::new(FULL), Release::new(OTHER));
        assert_eq!(own.rollback_of(&full, false), Some(false));
        assert_eq!(own.rollback_of(&full, true), Some(false));
        assert_eq!(own.rollback_of(&other, true), Some(true));
        assert_eq!(own.rollback_of(&other, false), None);
        let unknown = OwnCommit::new("unknown");
        assert_eq!(unknown.rollback_of(&full, false), None);
        assert_eq!(unknown.rollback_of(&full, true), Some(true));
    }
}
