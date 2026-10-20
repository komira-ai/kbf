//! The proposals of the release poller (`release::poll`): `update/proposed/<commit>`, one row
//! per release the poller found that could be run, for the operator to read in `GET
//! /v1/update`.
//!
//! **A proposal is not a run.** Nothing here touches `update/desired`, `update/policy` or any
//! host row: only an operator's `POST /v1/update/desired` starts a run. The row is JSON (a
//! field added later must be valid absent: serde defaults), written once per commit with
//! "must be absent", so a second poll that finds the same release writes nothing.

use kbf_state::{Cond, Txn};
use serde::{Deserialize, Serialize};

use super::rows::{RowError, Rows};

/// The key family of the proposals (fenced, `write_rules::FAMILIES`).
pub const PROPOSED: &str = "update/proposed/";

/// The key of the proposal for `commit`.
#[must_use]
pub fn proposal_key(commit: &str) -> String {
    format!("{PROPOSED}{commit}")
}

/// A release the poller found: what the operator sees, and what a later poll compares the
/// feed with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    /// The 40-hex commit the release was built from (the commit its notes name).
    pub commit: String,
    /// The release's `v*` tag.
    pub tag: String,
    /// When the release was published, in seconds since the Unix epoch (GitHub's
    /// `published_at`).
    pub published_at_secs: i64,
    /// When the poller wrote the proposal, in milliseconds since the Unix epoch.
    #[serde(default)]
    pub proposed_at_ms: u64,
}

impl Rows {
    /// Records `proposal` unless a proposal for its commit is already held (nothing is
    /// changed then). `true` when the row is new.
    ///
    /// # Errors
    /// [`RowError`]: the store failed, or this handle is no longer its writer.
    pub fn propose(&self, proposal: &Proposal) -> Result<bool, RowError> {
        let bytes = serde_json::to_vec(proposal).map_err(|e| RowError::Corrupt {
            key: proposal_key(&proposal.commit),
            why: e.to_string(),
        })?;
        let txn = Txn::new().put(proposal_key(&proposal.commit), bytes, Cond::Absent);
        Ok(self.store().txn(txn)?.is_ok())
    }

    /// Every proposal held, oldest release first. A row that cannot be read is left out and
    /// logged: one bad row does not hide the rest from the operator.
    ///
    /// # Errors
    /// [`RowError`]: the store failed.
    pub fn proposals(&self) -> Result<Vec<Proposal>, RowError> {
        let mut held: Vec<Proposal> = self
            .store()
            .scan_prefix(PROPOSED)?
            .into_iter()
            .filter_map(|(key, row)| match serde_json::from_slice(&row.value) {
                Ok(proposal) => Some(proposal),
                Err(e) => {
                    tracing::warn!("update: the proposal row {key} cannot be read: {e}");
                    None
                }
            })
            .collect();
        held.sort_by(|a, b| {
            (a.published_at_secs, &a.commit).cmp(&(b.published_at_secs, &b.commit))
        });
        Ok(held)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kbf_state::fake::{Fake, Flaw};
    use kbf_state::{ManualClock, Store};

    use super::*;

    fn rows() -> (Arc<dyn Store>, Rows) {
        let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        ));
        (Arc::clone(&store), Rows::new(store))
    }

    fn proposal(commit: char, at: i64) -> Proposal {
        Proposal {
            commit: commit.to_string().repeat(40),
            tag: format!("v{at}"),
            published_at_secs: at,
            proposed_at_ms: 7,
        }
    }

    /// Catches a second proposal for one commit replacing the first (the idempotence the
    /// poller relies on) or reporting itself new.
    #[test]
    fn a_commit_is_proposed_once_and_the_first_row_stays() {
        let (_, rows) = rows();
        assert!(rows.propose(&proposal('a', 10)).expect("first"));
        let mut again = proposal('a', 10);
        again.tag = "other".to_owned();
        assert!(!rows.propose(&again).expect("second"));
        assert_eq!(rows.proposals().expect("read"), vec![proposal('a', 10)]);
    }

    /// Catches proposals listed in key order instead of release order, and one unreadable row
    /// hiding the others.
    #[test]
    fn proposals_are_oldest_first_and_a_bad_row_is_skipped() {
        let (store, rows) = rows();
        rows.propose(&proposal('b', 20)).expect("b");
        rows.propose(&proposal('a', 30)).expect("a");
        rows.propose(&proposal('c', 10)).expect("c");
        store
            .txn(Txn::new().put(proposal_key("junk"), b"not json".to_vec(), Cond::Absent))
            .expect("store")
            .expect("junk");
        let order: Vec<i64> = rows
            .proposals()
            .expect("read")
            .iter()
            .map(|p| p.published_at_secs)
            .collect();
        assert_eq!(order, [10, 20, 30]);
    }

    /// Catches a proposal write touching the run's rows.
    #[test]
    fn a_proposal_writes_nothing_but_its_own_row() {
        let (store, rows) = rows();
        rows.propose(&proposal('a', 10)).expect("proposed");
        let keys: Vec<String> = store
            .scan_prefix("")
            .expect("scan")
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys, [proposal_key(&"a".repeat(40))]);
        assert!(rows.desired().expect("desired").value.is_none());
    }
}
