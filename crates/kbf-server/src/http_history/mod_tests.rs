//! Reading an invocation back: how many records one read takes, what a restored record
//! says of what it was made from, and the answer when the reader stops answering for an
//! invocation whose names were authorized. Each test names the defect it catches.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::to_bytes;
use axum::http::StatusCode;
use axum::response::Response;
use kbf_history::records::{InvocationHeader, RunOutcome, RunRecord, WireDigest};
use kbf_history::{
    Event, EventQueue, HistoryIndex, HistoryRoot, IndexRoot, Inline, InvKey, InvRef, Limits,
    Minter, QueueLimits, Reader, Writer, WriterId,
};
use kbf_invocation::WallTime;
use kbf_objstore::{Capabilities, KeyPrefix, MemoryStore};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use serde_json::Value;

use super::{Durable, Gathered, MAX_RECORDS, RESTORED_SERIAL};
use crate::history::{HistoryReads, Shared};
use crate::http_invocations::Request;

/// A state store whose reads can be made to fail.
struct Flaky {
    inner: Fake,
    fail: AtomicBool,
}

impl Store for Flaky {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("planted".to_owned()));
        }
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

struct Rig {
    durable: Durable,
    state: Arc<Flaky>,
    inv: InvKey,
}

/// History of invocation `name` holding `n` runs of instance `main`, each in a chunk of
/// its own, closed, and a [`Durable`] over it.
async fn rig(name: &str, n: u64) -> Rig {
    let objects = Shared(Arc::new(MemoryStore::new(Capabilities {
        conditional_put: true,
        object_lock: false,
    })));
    let state = Arc::new(Flaky {
        inner: Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None),
        fail: AtomicBool::new(false),
    });
    let store: Arc<dyn Store> = state.clone();
    let root = HistoryRoot::under(&KeyPrefix::new("kbf/").expect("a prefix")).expect("a root");
    let index = HistoryIndex::new(store, IndexRoot::default(), Arc::new(Inline));
    let (minter, queue) = (
        Arc::new(Minter::new(0)),
        Arc::new(EventQueue::new(QueueLimits::default())),
    );
    let limits = Limits {
        chunk_bytes: 1,
        txns_per_step: 64,
        ..Limits::default()
    };
    let mut writer = Writer::new(
        objects.clone(),
        root.clone(),
        index.clone(),
        WriterId(1),
        Arc::clone(&minter),
        Arc::clone(&queue),
        limits,
    );
    let inv = InvRef::named(name);
    for i in 0..n {
        let minted = minter.mint(&inv.key, 1_000 + i);
        let mut record = RunRecord {
            instance: "main".to_owned(),
            action: Some(WireDigest::new([u8::try_from(i % 250).unwrap_or(0); 32], i)),
            outcome: RunOutcome::Ok.into(),
            ..RunRecord::default()
        };
        record.set_run_id(minted.id);
        assert!(queue.offer(Event::Run {
            inv: inv.clone(),
            record: Box::new(record),
            failed: None,
            stream_opened_ms: minted.stream_opened_ms,
        }));
    }
    assert!(queue.offer(Event::Close { inv: inv.clone() }));
    for _ in 0..50 {
        writer.step(5_000).await;
        if writer.pending_rows() == 0 && writer.unwritten_chunks() == 0 {
            break;
        }
    }
    let reads = HistoryReads::new(Reader::new(objects, root, index), Arc::default());
    Rig {
        durable: Durable::new(Arc::new(reads)),
        state,
        inv: inv.key,
    }
}

fn allow(_: &str) -> bool {
    true
}

/// Catches: a read that stops at the first page of chunks (an invocation of more than 32
/// chunks missing rows with nothing to say so), one that never stops (no bound on the
/// records one request holds), a bound that marks an invocation capped when its last
/// page was the one that reached it, and a reader that no longer answers (its header
/// lists an instance `allow` refuses) given records.
#[tokio::test]
async fn a_read_gathers_every_page_up_to_its_bound() {
    let mut rig = rig("big", 40).await;
    let all = rig.durable.gather(&rig.inv, &allow).await.unwrap().unwrap();
    assert_eq!(
        (all.runs.len(), all.capped),
        (40, false),
        "two pages of chunks"
    );
    rig.durable.max_records = 10;
    let cut = rig.durable.gather(&rig.inv, &allow).await.unwrap().unwrap();
    assert_eq!(
        (cut.runs.len(), cut.capped),
        (32, true),
        "one page, and more behind"
    );
    // The bound is reached by the last page: nothing more to read, so not capped.
    let small = self::rig("small", 3).await;
    let mut durable = small.durable;
    durable.max_records = 1;
    let got = durable.gather(&small.inv, &allow).await.unwrap().unwrap();
    assert_eq!((got.runs.len(), got.capped), (3, false));
    let none = durable.gather(&small.inv, &|_: &str| false).await.unwrap();
    assert!(none.is_none());
    assert!(MAX_RECORDS > 32, "a page is under the bound");
}

fn request(id: &str) -> Request {
    Request::parse(&format!("/v1/invocations/{id}"), "").expect("a request")
}

/// Catches: a restored record that is complete when its reading was cut at the bound or
/// had records it could not use, a hole with no time, a serial that is a recorder's (a
/// page token made for a record the recorder held would be read against it) or that
/// repeats (two records restored one after the other taken for one), and times and
/// instances that are not the header's.
#[tokio::test]
async fn a_restored_record_says_what_it_was_made_from() {
    let rig = rig("made", 2).await;
    let view = rig
        .durable
        .reads
        .invocation(&rig.inv, &allow)
        .await
        .unwrap()
        .unwrap();
    assert!(view.complete);
    let gathered = Gathered {
        runs: rig
            .durable
            .gather(&rig.inv, &allow)
            .await
            .unwrap()
            .unwrap()
            .runs,
        hits: Vec::new(),
        capped: false,
    };
    let request = request("made");
    let whole = rig.durable.restore(&request, &view, &gathered);
    assert!(whole.complete() && !whole.rows_capped());
    assert_eq!(whole.gap_from(), None, "a complete record has no hole");
    assert!(whole.serial() & RESTORED_SERIAL != 0);
    let h = &view.header;
    assert_eq!(
        whole.first_seen(),
        WallTime::from_unix_micros(h.first_seen_ms * 1_000)
    );
    assert_eq!(
        whole.last_seen(),
        WallTime::from_unix_micros(h.last_seen_ms * 1_000)
    );
    assert_eq!(whole.instances().iter().collect::<Vec<_>>(), ["main"]);
    assert_eq!(whole.rows().len(), 2);

    let capped = Gathered {
        runs: gathered.runs.clone(),
        hits: Vec::new(),
        capped: true,
    };
    let cut = rig.durable.restore(&request, &view, &capped);
    assert!(!cut.complete() && cut.rows_capped());
    assert_eq!(
        cut.gap_from(),
        Some(WallTime::from_unix_micros(h.last_chunk_ms * 1_000))
    );
    assert_ne!(cut.serial(), whole.serial(), "each restoring has its own");
    // A record with a digest that is no digest is a hole too.
    let mut broken = gathered.runs;
    broken[0].action = None;
    let skipped = Gathered {
        runs: broken,
        hits: Vec::new(),
        capped: false,
    };
    let torn = rig.durable.restore(&request, &view, &skipped);
    assert!(!torn.complete() && !torn.rows_capped());
    assert_eq!(torn.rows().len(), 1);
}

async fn answer(response: Response) -> (StatusCode, Value) {
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn set_header(rig: &Rig, header: &InvocationHeader) {
    use prost::Message as _;
    let key = IndexRoot::default().invid(&rig.inv);
    let put = Txn::new().put(key, header.encode_to_vec(), kbf_state::Cond::Any);
    rig.state.inner.txn(put).unwrap().unwrap();
}

/// Catches: an invocation that stopped answering for a caller whose names were checked
/// for another reason than a changed list taken for a changed one (a `503` for a thing
/// that does not go away on retrying), a changed list taken for a gone record (the
/// retry that would have worked never made), an invocation that is gone told as changed,
/// and a store that fails told as a record that is not there.
#[tokio::test]
async fn a_reader_that_stops_answering_is_told_apart() {
    let rig = rig("told", 1).await;
    let since = WallTime::from_unix_micros(0);
    let names = vec!["main".to_owned()];
    let (code, body) = answer(rig.durable.refused(&rig.inv, &names, "told", since).await).await;
    assert_eq!(
        (code, &body["error"]),
        (StatusCode::NOT_FOUND, &Value::from("no_record"))
    );
    // The same names listed by another set: it changed.
    let other = vec!["other".to_owned()];
    let (code, body) = answer(rig.durable.refused(&rig.inv, &other, "told", since).await).await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("gained an instance name")
    );
    // An invocation history does not hold.
    let missing = InvKey::from_record_id("missing");
    let (code, _) = answer(
        rig.durable
            .refused(&missing, &names, "missing", since)
            .await,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    // A list that cannot be checked whole is not held.
    let header = InvocationHeader {
        record_id: "told".to_owned(),
        instances: names.clone(),
        instances_truncated: true,
        ..InvocationHeader::default()
    };
    set_header(&rig, &header);
    let (code, _) = answer(rig.durable.refused(&rig.inv, &names, "told", since).await).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    rig.state.fail.store(true, Ordering::SeqCst);
    let (code, body) = answer(rig.durable.refused(&rig.inv, &names, "told", since).await).await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "build history cannot be read right now");
}
