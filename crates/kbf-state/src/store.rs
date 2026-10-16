//! The state store trait.

use std::cmp::Ordering;
use std::num::NonZeroUsize;
use std::ops::Bound;

use crate::clock::StoreTime;
use crate::types::{
    Changes, Commit, CommitStats, Conflict, ResumeToken, Revision, ScanPage, ScanRange, StoreError,
    Txn, Versioned,
};

/// A durable, ordered key-value store with transactions, counters, time-to-live keys and
/// a change feed. The contract is clauses C1 to C8 of the architecture design; the
/// `conformance` module is the executable form every backend must pass.
///
/// Calls block until done; the SQLite backend's writer thread and the Postgres backend's
/// connections are behind them. A caller on an async runtime calls from a blocking task.
pub trait Store: Send + Sync {
    /// The group-commit entry point. Applies each transaction in order, each atomically
    /// and on its own precondition, and returns one result per transaction in the same
    /// order. The batch is made durable with one sync, and every result is returned only
    /// after that sync (C6, C7). A backend with a writer thread feeds the transactions
    /// of concurrent [`Store::txn`] calls through the same path.
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails; no result in the batch is then durable.
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError>;

    /// The live value of `key`. An expired key is absent whether or not it was reaped.
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError>;

    /// Every live key that starts with `prefix`, in key order.
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError>;

    /// A page of the live keys under `scan.prefix` that lie within `scan.lower` and
    /// `scan.upper`, in the key order of [`Store::scan_prefix`] (bytewise) or its
    /// reverse, at most `scan.limit` of them (C8). A bound outside the prefix is clipped
    /// to it, and bounds that cross give an empty page. [`ScanPage::resume`] is set when
    /// a live key of the range follows the last row in the scan's direction, and
    /// [`crate::ScanRange::resume`] makes the request for the next page, which begins
    /// exactly past that row.
    ///
    /// Under concurrent commits each page is read from one snapshot of committed state:
    /// it holds every transaction acknowledged before the call and none still in flight,
    /// each one whole or not at all. Pages are not one snapshot. A key committed behind
    /// the token after its page is not returned; one ahead of it is; a key deleted or
    /// expired before its page is read is absent; a key updated between pages shows the
    /// value of the page that returns it. So every key live in the range for the whole
    /// scan is returned exactly once, and the keys of a scan strictly ascend (or, in
    /// reverse, strictly descend).
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError>;

    /// At most `limit` feed rows with a revision after `after`, in revision order, never
    /// past a revision that can still commit (C5).
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError>;

    /// Removes every key the store's clock has expired, writing an `Expire` row for each,
    /// and returns how many. The reaper task calls this; reads do not wait for it (C4).
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn reap_expired(&self) -> Result<usize, StoreError>;

    /// The commit counters.
    fn commit_stats(&self) -> CommitStats;

    /// The store's clock reading, the one expiry is judged by.
    fn now(&self) -> StoreTime;

    /// Applies one transaction: a batch of one.
    ///
    /// # Errors
    /// [`StoreError`] when the backend fails.
    fn txn(&self, txn: Txn) -> Result<Result<Commit, Conflict>, StoreError> {
        // A backend returns one result per transaction; an empty answer is its bug.
        Ok(self.apply_batch(vec![txn])?.swap_remove(0))
    }
}

/// The keys a C8 request may return: its bounds clipped to its prefix. The lower bound
/// is never unbounded (every key under the prefix is at or after it); the upper is
/// unbounded only when no string follows every key under the prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub lower: Bound<String>,
    pub upper: Bound<String>,
}

impl Span {
    /// The span of `scan`, or `None` when no key can lie in it.
    pub(crate) fn of(scan: &ScanRange<'_>) -> Option<Span> {
        let lower = tighter(Bound::Included(scan.prefix), scan.lower, Ordering::Greater);
        let ceiling = successor(scan.prefix);
        let upper = match ceiling.as_deref() {
            Some(ceiling) => tighter(Bound::Excluded(ceiling), scan.upper, Ordering::Less),
            None => scan.upper,
        };
        let empty = match (lower, upper) {
            (Bound::Included(low), Bound::Included(high)) => low > high,
            (
                Bound::Included(low) | Bound::Excluded(low),
                Bound::Included(high) | Bound::Excluded(high),
            ) => low >= high,
            _ => false,
        };
        (!empty).then(|| Span {
            lower: lower.map(str::to_owned),
            upper: upper.map(str::to_owned),
        })
    }

    /// The span as borrowed bounds, the form `BTreeMap::range` takes.
    pub(crate) fn bounds(&self) -> (Bound<&str>, Bound<&str>) {
        (
            self.lower.as_ref().map(String::as_str),
            self.upper.as_ref().map(String::as_str),
        )
    }
}

/// Of two bounds on the same side, the one that admits fewer keys: the key further in
/// the `inward` direction, and at a tie the excluded one. `base` is never unbounded.
fn tighter<'a>(base: Bound<&'a str>, other: Bound<&'a str>, inward: Ordering) -> Bound<&'a str> {
    let key = |bound: Bound<&'a str>| match bound {
        Bound::Included(key) | Bound::Excluded(key) => Some(key),
        Bound::Unbounded => None,
    };
    match (key(base), key(other)) {
        (Some(mine), Some(theirs)) if theirs.cmp(mine) == inward => other,
        (Some(mine), Some(theirs)) if theirs == mine && matches!(other, Bound::Excluded(_)) => {
            other
        }
        _ => base,
    }
}

/// The least string greater than every string that starts with `prefix`: the prefix
/// with its last character replaced by the next one, after dropping trailing characters
/// that have none. `None` when there is no such string (an empty prefix, or one of only
/// `char::MAX`). Bytewise order on UTF-8 is code point order, so this holds for the
/// byte order every backend sorts keys by.
pub(crate) fn successor(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        let next = match last {
            '\u{d7ff}' => Some('\u{e000}'),
            other => char::from_u32(u32::from(other) + 1),
        };
        if let Some(next) = next {
            chars.push(next);
            return Some(chars.into_iter().collect());
        }
    }
    None
}

/// A C8 page from up to `limit + 1` live rows read in the scan's order: the first
/// `limit`, and a resume token when there was one more.
pub(crate) fn page_of(mut rows: Vec<(String, Versioned)>, limit: NonZeroUsize) -> ScanPage {
    let more = rows.len() > limit.get();
    rows.truncate(limit.get());
    let resume = match rows.last() {
        Some((key, _)) if more => Some(ResumeToken::after(key.clone())),
        _ => None,
    };
    ScanPage { rows, resume }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::ops::Bound;

    use super::{Span, successor};
    use crate::types::ScanRange;

    // Catches a successor that steps into the surrogate gap (no char there, so the
    // character would be dropped and the bound would jump a whole level) or that keeps
    // a trailing char::MAX it cannot step.
    #[test]
    fn the_successor_of_a_prefix_is_the_next_string_past_everything_under_it() {
        assert_eq!(successor("c8/").as_deref(), Some("c80"));
        assert_eq!(successor("a\u{d7ff}").as_deref(), Some("a\u{e000}"));
        assert_eq!(successor("a\u{10ffff}").as_deref(), Some("b"));
        assert_eq!(successor("\u{10ffff}\u{10ffff}"), None);
        assert_eq!(successor(""), None);
    }

    // Catches a span that invents an upper bound for a prefix with no successor: the
    // empty prefix is every key, so only the request's own upper bound applies.
    #[test]
    fn a_prefix_with_no_successor_leaves_the_upper_bound_to_the_request() {
        let limit = NonZeroUsize::MIN;
        let all = Span::of(&ScanRange::new("", limit)).expect("a span");
        assert_eq!(all.bounds(), (Bound::Included(""), Bound::Unbounded));
        let until = ScanRange {
            upper: Bound::Excluded("m"),
            ..ScanRange::new("", limit)
        };
        let span = Span::of(&until).expect("a span");
        assert_eq!(span.bounds(), (Bound::Included(""), Bound::Excluded("m")));
    }
}
