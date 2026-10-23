//! The proposals of the removed release poller: `update/proposed/<commit>`, one row
//! per release the poller found that could be run, for the operator to read in `GET
//! /v1/update`. Nothing writes them now; a store written by an older build still holds them.
//!
//! **A proposal is not a run.** Only an operator's `POST /v1/update/desired` starts one. The
//! row is JSON (a field added later must be valid absent: serde defaults).

use serde::Deserialize;

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
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
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
    use kbf_state::{Cond, ManualClock, Store, Txn};

    use super::*;

    fn rows() -> (Arc<dyn Store>, Rows) {
        let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        ));
        (Arc::clone(&store), Rows::new(store))
    }

    /// A proposal row as an older build wrote it.
    fn seed(store: &Arc<dyn Store>, commit: char, at: i64) {
        let key = proposal_key(&commit.to_string().repeat(40));
        let row = serde_json::json!({
            "commit": commit.to_string().repeat(40),
            "tag": format!("v{at}"),
            "published_at_secs": at,
            "proposed_at_ms": 7,
        });
        put(store, &key, row.to_string().into_bytes());
    }

    fn put(store: &Arc<dyn Store>, key: &str, value: Vec<u8>) {
        store
            .txn(Txn::new().put(key, value, Cond::Absent))
            .expect("store")
            .expect("no condition");
    }

    /// Catches proposals listed in key order instead of release order, and one unreadable row
    /// hiding the others.
    #[test]
    fn proposals_are_oldest_first_and_a_bad_row_is_skipped() {
        let (store, rows) = rows();
        seed(&store, 'b', 20);
        seed(&store, 'a', 30);
        seed(&store, 'c', 10);
        put(&store, &proposal_key("junk"), b"not json".to_vec());
        let order: Vec<i64> = rows
            .proposals()
            .expect("read")
            .iter()
            .map(|p| p.published_at_secs)
            .collect();
        assert_eq!(order, [10, 20, 30]);
    }

    /// Catches a row written before `proposed_at_ms` existed being refused instead of read
    /// with the field at zero.
    #[test]
    fn a_proposal_without_the_time_it_was_proposed_reads_as_zero() {
        let (store, rows) = rows();
        let key = proposal_key(&"a".repeat(40));
        let row = format!(
            "{{\"commit\":\"{}\",\"tag\":\"v1\",\"published_at_secs\":5}}",
            "a".repeat(40)
        );
        put(&store, &key, row.into_bytes());
        let held = rows.proposals().expect("read");
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].proposed_at_ms, 0);
    }
}
