//! R4's second input: the build `seq` of a release in the store, and the check that puts it
//! beside the running build's (`docs/design/frontend-proxy.md`, rule R4; the decision itself
//! is [`crate::update::age::check`]).
//!
//! The store does not record a release's `seq` yet (its `kbf --version --json` is kept per
//! commit and platform by a later change), so the lookup is a seam, [`SeqLookup`], with three
//! answers ([`TargetSeq`]). [`ReleaseStore`] answers [`TargetSeq::NotRecorded`] for every
//! commit, under which R4 does not apply and `POST /v1/update/desired` behaves as it did.
//! When the store records `seq`, it answers [`TargetSeq::Seq`], or [`TargetSeq::Missing`] for
//! a release stored before; only its [`SeqLookup`] changes.

use std::fmt;
use std::io;
use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;

use super::key::Commit;
use super::store::ReleaseStore;
use crate::update::age::{self, Build, Refusal};

/// What a [`SeqLookup`] knows of a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetSeq {
    /// The `seq` the release's build reported (0 is unknown, as in `kbf --version --json`).
    Seq(u64),
    /// The store records `seq`, and this commit has none: a release stored before it did.
    Missing,
    /// This store cannot record `seq` yet, so R4 has nothing to compare and does not apply.
    NotRecorded,
}

/// Where the `seq` of a release is learned.
pub trait SeqLookup: Send + Sync {
    /// The `seq` of `commit` (any platform: it is per commit), or the io error of reading it.
    fn seq<'a>(&'a self, commit: &'a Commit) -> BoxFuture<'a, io::Result<TargetSeq>>;
}

impl SeqLookup for ReleaseStore {
    fn seq<'a>(&'a self, _commit: &'a Commit) -> BoxFuture<'a, io::Result<TargetSeq>> {
        futures::future::ready(Ok(TargetSeq::NotRecorded)).boxed()
    }
}

/// An age check refused: what it compared, for the operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub refusal: Refusal,
    own_seq: u64,
    own_commit: String,
    target: String,
    target_seq: Option<u64>,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (own_seq, own, target) = (self.own_seq, &self.own_commit, &self.target);
        let seq = self
            .target_seq
            .map_or_else(|| "unknown".to_owned(), |s| s.to_string());
        let why = match self.refusal {
            Refusal::SeqUnknown => "its build seq is not known, so it cannot be put after",
            Refusal::Older => "its build seq is below that of",
            Refusal::SeqTie => {
                "its build seq is the same as that of, but it is another commit than,"
            }
        };
        write!(
            f,
            "{}: release {target} (build seq {seq}) is refused: {why} the running build {own} \
             (build seq {own_seq})",
            self.refusal.code()
        )
    }
}

/// The running child and where to learn a release's `seq`.
pub struct AgeCheck {
    own_seq: u64,
    own_commit: String,
    lookup: Arc<dyn SeqLookup>,
}

impl AgeCheck {
    /// A check for the build `own_commit` with `own_seq`, learning release `seq` from `lookup`.
    pub fn new(own_seq: u64, own_commit: &str, lookup: Arc<dyn SeqLookup>) -> Self {
        Self {
            own_seq,
            own_commit: own_commit.to_owned(),
            lookup,
        }
    }

    /// The check of this process: [`crate::BUILD_SEQ`] and its commit, in full when the build
    /// knew it ([`crate::BUILD_COMMIT_FULL`]), else [`crate::BUILD_COMMIT`].
    pub fn running(lookup: Arc<dyn SeqLookup>) -> Self {
        let commit = if crate::BUILD_COMMIT_FULL.is_empty() {
            crate::BUILD_COMMIT
        } else {
            crate::BUILD_COMMIT_FULL
        };
        Self::new(crate::BUILD_SEQ, commit, lookup)
    }

    /// Whether `commit` may be offered as the desired release.
    ///
    /// # Errors
    /// The io error of the lookup; `Ok(Err(_))` is a refusal.
    pub async fn check(&self, commit: &Commit) -> io::Result<Result<(), Refused>> {
        let target_seq = match self.lookup.seq(commit).await? {
            TargetSeq::NotRecorded => return Ok(Ok(())),
            TargetSeq::Seq(seq) => Some(seq),
            TargetSeq::Missing => None,
        };
        let own = Build {
            seq: self.own_seq,
            commit: &self.own_commit,
        };
        Ok(
            age::check(own, target_seq, commit.as_str()).map_err(|refusal| Refused {
                refusal,
                own_seq: self.own_seq,
                own_commit: self.own_commit.clone(),
                target: commit.as_str().to_owned(),
                target_seq,
            }),
        )
    }
}
