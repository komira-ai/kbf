//! The open operation streams of each build, for a drain that is stopped by force while streams
//! are open: it says how many streams of how many builds it cuts
//! ([`super::ExecuteMetrics::open_streams`]). A build is a client's invocation, named by the
//! `RequestMetadata` header's `tool_invocation_id` ([`crate::request_metadata`]); a stream whose
//! call named none is counted apart.
//!
//! Unlike the gauges beside it, this takes a lock: once when a stream begins and once when it
//! ends, never per message.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::request_metadata::InvocationId;

/// The Execute and WaitExecution streams open now, and the builds they belong to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamCensus {
    /// The streams open.
    pub streams: u64,
    /// The builds they belong to: the distinct invocation ids among them.
    pub builds: u64,
    /// The streams among them whose call named no build.
    pub unnamed: u64,
}

/// How many streams are open for each build, `None` for the calls that named none.
#[derive(Debug, Default)]
pub(super) struct Builds(Mutex<BTreeMap<Option<InvocationId>, u64>>);

impl Builds {
    fn locked(&self) -> MutexGuard<'_, BTreeMap<Option<InvocationId>, u64>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A stream of `build` begins.
    pub(super) fn open(&self, build: Option<&InvocationId>) {
        *self.locked().entry(build.cloned()).or_default() += 1;
    }

    /// A stream of `build` ends: once per [`Builds::open`].
    pub(super) fn close(&self, build: Option<&InvocationId>) {
        let mut open = self.locked();
        let key = build.cloned();
        let left = open.get(&key).map_or(0, |n| n.saturating_sub(1));
        if left == 0 {
            open.remove(&key);
        } else {
            open.insert(key, left);
        }
    }

    /// The streams open now.
    pub(super) fn census(&self) -> StreamCensus {
        let open = self.locked();
        let unnamed = open.get(&None).copied().unwrap_or(0);
        let streams = open.values().sum();
        let builds = open.keys().filter(|build| build.is_some()).count();
        StreamCensus {
            streams,
            builds: u64::try_from(builds).unwrap_or(u64::MAX),
            unnamed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(text: &str) -> InvocationId {
        InvocationId::parse(text).expect("an id kbf accepts")
    }

    /// Catches a census that counts streams as builds, counts a stream that named no build as a
    /// build of its own, or keeps a build whose streams have all ended.
    #[test]
    fn the_census_counts_streams_by_build_and_forgets_a_build_whose_streams_ended() {
        let builds = Builds::default();
        let (a, b) = (id("build-a"), id("build-b"));
        assert_eq!(builds.census(), StreamCensus::default());
        builds.open(Some(&a));
        builds.open(Some(&a));
        builds.open(Some(&b));
        builds.open(None);
        let all = StreamCensus {
            streams: 4,
            builds: 2,
            unnamed: 1,
        };
        assert_eq!(builds.census(), all);
        builds.close(Some(&b));
        builds.close(None);
        let only_a = StreamCensus {
            streams: 2,
            builds: 1,
            unnamed: 0,
        };
        assert_eq!(builds.census(), only_a);
        builds.close(Some(&a));
        builds.close(Some(&a));
        assert_eq!(builds.census(), StreamCensus::default());
    }

    /// Catches a close of a build with no stream open that underflows or leaves a row behind.
    #[test]
    fn a_close_with_nothing_open_leaves_the_census_empty() {
        let builds = Builds::default();
        builds.close(Some(&id("never-opened")));
        builds.close(None);
        assert_eq!(builds.census(), StreamCensus::default());
    }
}
