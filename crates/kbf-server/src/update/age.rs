//! R4, the age check (`docs/design/frontend-proxy.md`, rule table): the release offered as
//! desired must not be older than the build of the child that holds the generation. "Older"
//! is by build `seq`, the number of commits behind a build (`kbf --version --json`,
//! [`crate::BUILD_SEQ`]), never by the text of a commit, which has no order.
//!
//! [`check`] is a pure decision: no store, no clock. Whether a request may skip it
//! (`rollback`) is the caller's, so a rollback never reaches here.
//!
//! | own seq | target seq | commits | answer |
//! |---|---|---|---|
//! | 0 (unknown) | any | any | allowed: R4 cannot apply to a build that could not count |
//! | known | unknown | any | [`Refusal::SeqUnknown`] |
//! | known | below own | any | [`Refusal::Older`] |
//! | known | equal | same | allowed (a repeat of the running build) |
//! | known | equal | different | [`Refusal::SeqTie`] |
//! | known | above own | any | allowed |
//!
//! Equal counts on different commits are two branches, which cannot be put in order, so a
//! tie of different commits is refused: a forward move needs a strictly larger `seq`.

use std::fmt;

use crate::build_seq::UNKNOWN;

/// A build, as R4 sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Build<'a> {
    /// Its commit count; [`UNKNOWN`] (0) when it could not count.
    pub seq: u64,
    /// Its commit: 12 hex digits for the running build ([`crate::BUILD_COMMIT`]), 40 for a
    /// release of the store. One names the other when it is a prefix of it.
    pub commit: &'a str,
}

/// Why the age check refuses a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The target's seq is not known, so it cannot be put after the running build.
    SeqUnknown,
    /// The target has a lower seq than the running build.
    Older,
    /// The target has the running build's seq and another commit (another branch).
    SeqTie,
}

impl Refusal {
    /// The stable machine code of the refusal.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::SeqUnknown => "seq_unknown",
            Self::Older => "older",
            Self::SeqTie => "seq_tie",
        }
    }
}

/// Whether `a` and `b` name one commit: the shorter is a prefix of the longer, at least a
/// full 12 digits (`unknown` and the empty text name nothing).
fn same_commit(a: &str, b: &str) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    short.len() >= 12 && long.starts_with(short)
}

/// Whether the release `target_commit` may become the desired release of the child that runs
/// `own`. A `target_seq` of 0 is unknown, as `None` is.
///
/// # Errors
/// The [`Refusal`] in the table above.
pub fn check(own: Build<'_>, target_seq: Option<u64>, target_commit: &str) -> Result<(), Refusal> {
    if own.seq == UNKNOWN {
        return Ok(());
    }
    let Some(seq) = target_seq.filter(|&s| s != UNKNOWN) else {
        return Err(Refusal::SeqUnknown);
    };
    match seq.cmp(&own.seq) {
        std::cmp::Ordering::Less => Err(Refusal::Older),
        std::cmp::Ordering::Greater => Ok(()),
        std::cmp::Ordering::Equal if same_commit(own.commit, target_commit) => Ok(()),
        std::cmp::Ordering::Equal => Err(Refusal::SeqTie),
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}
