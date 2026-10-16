//! The reader: a run id or an invocation to its records, filtered by instance, and the
//! errors a broken index or object gives.

use kbf_history::container::{self, Header};
use kbf_history::records::{
    ChunkRecord, ChunkRow, ChunkSummary, RunOutcome, SECTION_RUN, StreamRow, chunk_key,
};
use kbf_history::{Day, Family, IndexRoot, InvRef, Limits, ReadError, RunId, StreamId, WriterId};
use kbf_objstore::ObjectStoreError;
use prost::Message;

use crate::support::{Life, T, all, root};

/// A second into the day, so a time before a stream's first run is the same day.
const B: u64 = T + 1_000;

fn only_a(instance: &str) -> bool {
    instance == "a"
}

fn closed_build(life: &mut Life) -> (InvRef, Vec<kbf_history::records::RunRecord>) {
    let inv = InvRef::named("build");
    let runs = vec![
        life.run(&inv, B, "a", RunOutcome::Ok),
        life.run(&inv, B + 1, "b", RunOutcome::Failed),
        life.run(&inv, B + 2, "a", RunOutcome::TimedOut),
    ];
    life.hit(&inv, B + 3, "a");
    life.hit(&inv, B + 4, "b");
    life.close(&inv);
    life.step(B + 5);
    (inv, runs)
}

#[test]
fn a_run_reads_back_with_its_failed_section_and_an_ok_run_without() {
    // Catches: a run located in the wrong chunk or block, a failed run whose tails and
    // command are not found, or an ok run given another run's section.
    let mut life = Life::fresh(Limits::default());
    let (inv, runs) = closed_build(&mut life);
    let reader = life.reader();
    for r in &runs {
        let got = futures::executor::block_on(reader.run(r.run_id(), &all))
            .unwrap()
            .unwrap();
        assert_eq!((&got.inv, &got.run), (&inv.key, r));
        assert_eq!(got.failed.is_some(), r.failed());
        if let Some(f) = got.failed {
            assert_eq!(f.seq, r.seq);
            let stderr = f.stderr.unwrap().bytes;
            assert_eq!(stderr, format!("error in {}\n", r.instance).as_bytes());
            assert_eq!(f.command.unwrap().env_names, ["PATH"]);
        }
    }
    // A failed run written without a section reads back with none.
    let other = InvRef::named("bare");
    let minted = life.minter.mint(&other.key, T + 10);
    let id = minted.id;
    let mut r = crate::support::run_record("a", RunOutcome::Failed, T + 10);
    r.set_run_id(id);
    life.queue.offer(kbf_history::Event::Run {
        inv: other.clone(),
        record: Box::new(r.clone()),
        failed: None,
        stream_opened_ms: minted.stream_opened_ms,
    });
    life.close(&other);
    life.step(T + 10);
    let got = futures::executor::block_on(life.reader().run(id, &all))
        .unwrap()
        .unwrap();
    assert_eq!((got.run, got.failed), (r, None));
}

#[test]
fn an_invocation_reads_back_every_record_of_every_chunk() {
    // Catches: a chunk skipped (only the newest read), or a record kind dropped.
    let mut life = Life::fresh(Limits {
        chunk_bytes: 1,
        ..Limits::default()
    });
    let (inv, runs) = closed_build(&mut life);
    let got = futures::executor::block_on(life.reader().invocation(&inv.key, &all))
        .unwrap()
        .unwrap();
    assert!(got.header.chunks >= 4, "{} chunks", got.header.chunks);
    assert_eq!(got.runs, runs);
    assert_eq!((got.failed.len(), got.hits.len()), (2, 2));
    assert!(got.complete);
}

#[test]
fn the_instance_filter_leaves_out_records_of_refused_instances() {
    // Catches: a reader that returns every record of an invocation whatever instances
    // its caller may see (a caller allowed only on `a` would read `b`'s runs, failure
    // tails and hits).
    let mut life = Life::fresh(Limits::default());
    let (inv, runs) = closed_build(&mut life);
    let reader = life.reader();
    let got = futures::executor::block_on(reader.invocation(&inv.key, &only_a))
        .unwrap()
        .unwrap();
    assert!(got.runs.iter().all(|r| r.instance == "a"));
    assert_eq!(got.runs.len(), 2);
    assert!(got.failed.iter().all(|f| f.instance == "a"));
    assert_eq!(got.failed.len(), 1);
    assert_eq!(got.hits.len(), 1);
    assert_eq!(got.hits[0].instance, "a");
    let b = futures::executor::block_on(reader.run(runs[1].run_id(), &only_a)).unwrap();
    assert_eq!(b, None);
    let a = futures::executor::block_on(reader.run(runs[0].run_id(), &only_a)).unwrap();
    assert!(a.is_some());
}

#[test]
fn ids_and_invocations_history_does_not_hold_read_as_none() {
    // Catches: an unknown id resolved to the nearest stream row of another day or
    // stream, or a sequence past the chunk's range answered with a neighbour.
    let mut life = Life::fresh(Limits::default());
    let (_, runs) = closed_build(&mut life);
    let reader = life.reader();
    let id = runs[0].run_id();
    let unknown = [
        RunId::new(T + 86_400_000, id.stream(), id.seq()),
        RunId::new(id.time_ms(), StreamId(id.stream().0 + 1), 0),
        RunId::new(id.time_ms(), id.stream(), 99),
        // Before the stream's first commit: no row at or before it.
        RunId::new(id.time_ms() - 1, id.stream(), 0),
    ];
    for u in unknown {
        assert_eq!(
            futures::executor::block_on(reader.run(u, &all)).unwrap(),
            None,
            "{u}"
        );
    }
    let nobody = InvRef::named("nobody");
    assert_eq!(
        futures::executor::block_on(reader.invocation(&nobody.key, &all)).unwrap(),
        None
    );
}

#[test]
fn a_missing_or_newer_object_is_an_error_not_an_empty_answer() {
    // Catches: an expired or unreadable chunk reported as a run that does not exist,
    // or a newer container version misread.
    let mut life = Life::fresh(Limits::default());
    let (inv, runs) = closed_build(&mut life);
    let key = life.objects.keys().remove(0);
    let mut bytes = life.objects.get(&key).to_vec();
    bytes[5] = 2;
    let n = bytes.len();
    bytes[n - 7] = 2;
    life.objects.replace(&key, bytes);
    let reader = life.reader();
    let err = futures::executor::block_on(reader.invocation(&inv.key, &all)).unwrap_err();
    assert!(err.to_string().contains("version 2"), "{err}");
    let err = futures::executor::block_on(reader.run(runs[0].run_id(), &all)).unwrap_err();
    assert!(matches!(err, ReadError::Format(_)), "{err}");
    life.objects.remove(&key);
    let err = futures::executor::block_on(reader.run(runs[0].run_id(), &all)).unwrap_err();
    assert!(
        matches!(err, ReadError::Object(ObjectStoreError::NotFound(_))),
        "{err}"
    );
}

#[test]
fn broken_index_rows_are_errors() {
    // Catches: an undecodable row read as an empty one (a stream row with no
    // invocation would resolve every run to nothing, silently).
    let mut life = Life::fresh(Limits::default());
    let (inv, runs) = closed_build(&mut life);
    let id = runs[0].run_id();
    let reader = life.reader();
    let stream_key = life
        .index
        .keys(&IndexRoot::default().stream_prefix(id.day(), id.stream()))
        .remove(0);
    let bad_inv = StreamRow {
        inv_key: "nope".into(),
        writer: 1,
    };
    life.index.put(&stream_key, bad_inv.encode_to_vec());
    let err = futures::executor::block_on(reader.run(id, &all)).unwrap_err();
    assert!(err.to_string().contains("not an invocation key"), "{err}");
    life.index.put(&stream_key, vec![0xFF]);
    let err = futures::executor::block_on(reader.run(id, &all)).unwrap_err();
    assert!(matches!(err, ReadError::Row { .. }), "{err}");
    let row_key = life
        .index
        .keys(&IndexRoot::default().ichunk_prefix(&inv.key))
        .remove(0);
    life.index.put(&row_key, vec![0xFF]);
    let err = futures::executor::block_on(reader.invocation(&inv.key, &all)).unwrap_err();
    assert!(err.to_string().contains(&row_key), "{err}");
    life.index
        .put(&IndexRoot::default().invid(&inv.key), vec![0xFF]);
    let err = futures::executor::block_on(reader.invocation(&inv.key, &all)).unwrap_err();
    assert!(matches!(err, ReadError::Row { .. }), "{err}");
    // The index itself failing is its own error.
    let as_text = ReadError::from(kbf_state::StoreError::Failed("down".into())).to_string();
    assert!(as_text.contains("down"));
}

/// Writes a chunk by hand: `records` under a valid header, with its rows, so a test can
/// plant what no writer writes.
fn plant(life: &Life, records: Vec<(Vec<u8>, Vec<u8>)>, row: ChunkRow) -> (InvRef, RunId) {
    let inv = InvRef::named("planted");
    let writer = WriterId(77);
    let day = Day::from_ms(T);
    let header = Header {
        family: Family::InvocationChunk,
        writer,
        day,
        seq: 0,
        base_ms: T,
    };
    let enc = container::encode(&header, records, ChunkSummary::default().encode_to_vec());
    let key = root().chunk(day, &inv.key, writer, 0);
    life.objects.replace(key.as_str(), enc.bytes.clone());
    let row = ChunkRow {
        day: day.number(),
        bytes: if row.bytes == u64::MAX {
            0
        } else {
            enc.bytes.len() as u64
        },
        footer_offset: enc.footer_offset,
        footer_len: if row.footer_len == u32::MAX {
            0
        } else {
            enc.footer_len
        },
        ..row
    };
    let stream = StreamId(0x7F_0000);
    life.index.put(
        &IndexRoot::default().ichunk(&inv.key, writer, 0),
        row.encode_to_vec(),
    );
    let s = StreamRow {
        inv_key: inv.key.as_str().into(),
        writer: writer.0,
    };
    life.index.put(
        &IndexRoot::default().stream(day, stream, T),
        s.encode_to_vec(),
    );
    life.index
        .put(&IndexRoot::default().invid(&inv.key), Vec::new());
    (inv, RunId::new(T, stream, 0))
}

fn holds_run0() -> ChunkRow {
    ChunkRow {
        stream: 0x7F_0000,
        first_seq: 0,
        last_seq: 0,
        runs: 1,
        ..ChunkRow::default()
    }
}

#[test]
fn records_that_are_not_chunk_records_are_errors() {
    // Catches: a record that does not decode, or decodes to nothing, skipped silently.
    let life = Life::fresh(Limits::default());
    let key = chunk_key(SECTION_RUN, 0).to_vec();
    let (inv, id) = plant(&life, vec![(key.clone(), vec![0xFF])], holds_run0());
    let reader = life.reader();
    let err = futures::executor::block_on(reader.invocation(&inv.key, &all)).unwrap_err();
    assert!(err.to_string().contains("chunk record"), "{err}");
    let err = futures::executor::block_on(reader.run(id, &all)).unwrap_err();
    assert!(err.to_string().contains("chunk record"), "{err}");
    let empty = ChunkRecord { body: None }.encode_to_vec();
    plant(&life, vec![(key, empty)], holds_run0());
    let err = futures::executor::block_on(reader.invocation(&inv.key, &all)).unwrap_err();
    assert!(err.to_string().contains("empty record"), "{err}");
}

#[test]
fn a_chunk_row_that_disagrees_with_its_object_is_an_error() {
    // Catches: a row whose footer range or length is wrong read as an empty chunk, or
    // a chunk said to hold a run whose footer has no block for it.
    let life = Life::fresh(Limits::default());
    let reader = life.reader();
    let (_, id) = plant(&life, Vec::new(), holds_run0());
    let err = futures::executor::block_on(reader.run(id, &all)).unwrap_err();
    assert!(err.to_string().contains("block"), "{err}");
    let zero_footer = ChunkRow {
        footer_len: u32::MAX,
        ..holds_run0()
    };
    let (_, id) = plant(&life, Vec::new(), zero_footer);
    assert!(futures::executor::block_on(reader.run(id, &all)).is_err());
    let zero_bytes = ChunkRow {
        bytes: u64::MAX,
        ..holds_run0()
    };
    let (inv, _) = plant(&life, Vec::new(), zero_bytes);
    // The planted header row is empty bytes: a default header.
    let err = futures::executor::block_on(reader.invocation(&inv.key, &all)).unwrap_err();
    assert!(err.to_string().contains("chunk row"), "{err}");
}
