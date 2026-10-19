//! An invocation's header as a reader sees it: the rows of every writer, merged.
//!
//! Each writer keeps a header row of its own ([`IndexRoot::header`](crate::IndexRoot::header)),
//! counting the records it wrote, so two writers never race on a header: a process that a
//! swap has replaced still writes its row while the next one writes its own. A reader
//! adds them up. A row an earlier build wrote for every writer
//! ([`IndexRoot::invid`](crate::IndexRoot::invid)) is one more row.

use crate::keys::InvKey;
use crate::reader::{ReadError, Reader, decode_row};
use crate::records::{InvState, InvocationHeader};
use crate::writer::{add_instance, earliest};

/// The headers of one invocation as one: counts summed, instances joined, the time span
/// widened, the first listing time kept (the entry that exists first is the one shown),
/// and the invocation closed only when every writer closed it. `None` for no rows.
pub(crate) fn merge_headers(rows: Vec<InvocationHeader>) -> Option<InvocationHeader> {
    let mut rows = rows.into_iter();
    let mut merged = rows.next()?;
    let mut others = false;
    let mut closed = merged.state() == InvState::Closed;
    for h in rows {
        others = true;
        for instance in &h.instances {
            add_instance(&mut merged, instance);
        }
        merged.instances_truncated |= h.instances_truncated;
        merged.idless |= h.idless;
        if merged.record_id.is_empty() {
            merged.record_id.clone_from(&h.record_id);
        }
        merged.runs += h.runs;
        merged.hits += h.hits;
        merged.failed += h.failed;
        merged.dropped += h.dropped;
        merged.chunks += h.chunks;
        merged.first_seen_ms = earliest(merged.first_seen_ms, h.first_seen_ms);
        merged.last_seen_ms = merged.last_seen_ms.max(h.last_seen_ms);
        merged.last_chunk_ms = merged.last_chunk_ms.max(h.last_chunk_ms);
        merged.list_ms = earliest(merged.list_ms, h.list_ms);
        closed &= h.state() == InvState::Closed;
    }
    if others {
        merged.state = if closed {
            InvState::Closed
        } else {
            InvState::Open
        }
        .into();
    }
    Some(merged)
}

impl<O> Reader<O> {
    /// `inv`'s header, merged from the rows of every writer: one scan, so a row a writer
    /// adds meanwhile is in it or not, whole. `None` when no writer has a row.
    ///
    /// # Errors
    /// [`ReadError`] when the index fails or a row does not decode.
    pub(crate) async fn stored_header(
        &self,
        inv: &InvKey,
    ) -> Result<Option<InvocationHeader>, ReadError> {
        let rows = self
            .index
            .scan_prefix(self.index.root().header_rows(inv))
            .await?;
        let headers = rows
            .iter()
            .map(|(key, row)| decode_row::<InvocationHeader>(key, &row.value))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(merge_headers(headers))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(runs: u64, state: InvState) -> InvocationHeader {
        InvocationHeader {
            record_id: "build".to_owned(),
            instances: vec!["main".to_owned()],
            runs,
            first_seen_ms: 100,
            last_seen_ms: 200,
            last_chunk_ms: 300,
            chunks: 1,
            list_ms: 100,
            state: state.into(),
            ..InvocationHeader::default()
        }
    }

    // Catches: a merge that changes a header that has no other (a single writer's row
    // would read back different from what it wrote), and one invented from no rows.
    #[test]
    fn one_header_is_itself_and_none_is_none() {
        assert_eq!(merge_headers(Vec::new()), None);
        for state in [InvState::Open, InvState::Closed] {
            let one = header(2, state);
            assert_eq!(merge_headers(vec![one.clone()]), Some(one));
        }
    }

    // Catches: a merge that loses a count (a swapped child's records not in the totals),
    // takes the last writer's times instead of the span, takes a later listing time (the
    // entry the reader shows would be one no header names), or closes an invocation that
    // a writer still has open.
    #[test]
    fn headers_add_up_widen_and_close_only_when_all_closed() {
        let mut later = header(3, InvState::Closed);
        later.instances = vec!["a".to_owned(), "main".to_owned()];
        later.failed = 1;
        later.hits = 4;
        later.dropped = 2;
        later.first_seen_ms = 50;
        later.last_seen_ms = 250;
        later.last_chunk_ms = 260;
        later.list_ms = 0;
        let m = merge_headers(vec![header(2, InvState::Closed), later.clone()]).expect("rows");
        assert_eq!(
            (m.runs, m.hits, m.failed, m.dropped, m.chunks),
            (5, 4, 1, 2, 2)
        );
        assert_eq!(m.instances, ["a", "main"]);
        assert_eq!(
            (m.first_seen_ms, m.last_seen_ms, m.last_chunk_ms),
            (50, 250, 300)
        );
        assert_eq!(m.list_ms, 100, "the earliest nonzero listing time");
        assert_eq!(m.state(), InvState::Closed);
        let open = merge_headers(vec![header(2, InvState::Closed), header(1, InvState::Open)])
            .expect("rows");
        assert_eq!(open.state(), InvState::Open);
        let open = merge_headers(vec![header(1, InvState::Open), header(2, InvState::Closed)])
            .expect("rows");
        assert_eq!(open.state(), InvState::Open);
    }

    // Catches: a merge that loses a bound or a flag: the instance list stays within
    // `MAX_INSTANCES` and says it was cut, a writer's truncation shows in the whole, an
    // id-less row makes the whole id-less, and a record id comes from the first row that
    // has one.
    #[test]
    fn bounds_and_flags_survive_a_merge() {
        let mut wide = header(1, InvState::Open);
        wide.instances = (0..crate::records::MAX_INSTANCES)
            .map(|i| format!("i{i:03}"))
            .collect();
        let mut bare = header(1, InvState::Open);
        bare.record_id.clear();
        let m = merge_headers(vec![bare, wide.clone(), header(1, InvState::Open)]).expect("rows");
        assert_eq!(m.record_id, "build");
        assert_eq!(m.instances.len(), crate::records::MAX_INSTANCES);
        assert!(m.instances_truncated, "main did not fit");
        let mut cut = header(1, InvState::Open);
        cut.instances_truncated = true;
        cut.idless = true;
        let m = merge_headers(vec![header(1, InvState::Open), cut]).expect("rows");
        assert!(m.instances_truncated && m.idless);
    }
}
