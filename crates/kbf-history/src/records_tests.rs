//! Tests of the record types.

use super::*;
use crate::run_id::StreamId;

#[test]
fn the_test_flag_is_field_33_and_older_records_read_false() {
    // Catches: a flag renumbered or dropped from the wire (a chunk written by this
    // version would read back as no test), and a record written before the flag
    // existed failing to decode or reading true.
    use prost::Message as _;
    let test = RunRecord {
        test: true,
        ..RunRecord::default()
    };
    // Tag 33, varint: key 264 (0x88 0x02), value 1.
    assert_eq!(test.encode_to_vec(), [0x88, 0x02, 0x01]);
    assert!(
        RunRecord::decode([0x88, 0x02, 0x01].as_slice())
            .unwrap()
            .test
    );
    assert!(!RunRecord::default().test);
    assert!(RunRecord::default().encode_to_vec().is_empty());
    let older = RunRecord {
        seq: 3,
        ..RunRecord::default()
    };
    assert!(
        !RunRecord::decode(older.encode_to_vec().as_slice())
            .unwrap()
            .test
    );
}

#[test]
fn the_submit_evidence_fields_are_43_to_47_and_older_records_read_absent() {
    // Catches: a field renumbered into the usage fields (39 to 42) or onto another
    // (the wire would read one value as another), `optional` lost on the operation id or
    // a total (a 0 on the wire would read as "measured 0", where absent is "unknown"),
    // and a record written before the fields failing to decode or reading a value.
    use prost::Message as _;
    let r = RunRecord {
        operation_id: Some(0),
        cpu_source: BookSource::Hint.into(),
        memory_source: BookSource::Default.into(),
        input_bytes: Some(0),
        input_files: Some(0),
        ..RunRecord::default()
    };
    // Tag 43 varint: key 344 (0xd8 0x02); 44: 352 (0xe0 0x02); 45: 360 (0xe8 0x02);
    // 46: 368 (0xf0 0x02); 47: 376 (0xf8 0x02). A present 0 is written, an absent field
    // is not.
    assert_eq!(
        r.encode_to_vec(),
        [
            0xd8, 0x02, 0x00, 0xe0, 0x02, 0x02, 0xe8, 0x02, 0x01, 0xf0, 0x02, 0x00, 0xf8, 0x02,
            0x00
        ]
    );
    assert_eq!(RunRecord::decode(r.encode_to_vec().as_slice()).unwrap(), r);
    let older = RunRecord {
        seq: 3,
        ..RunRecord::default()
    };
    let read = RunRecord::decode(older.encode_to_vec().as_slice()).unwrap();
    assert_eq!(
        (read.operation_id, read.input_bytes, read.input_files),
        (None, None, None)
    );
    assert_eq!(
        (read.cpu_source(), read.memory_source()),
        (BookSource::Unspecified, BookSource::Unspecified)
    );
}

#[test]
fn a_digest_prefix_is_its_first_eight_bytes() {
    // Catches: a little-endian prefix, which would not match the 16 hex digits the
    // digest is shown with.
    let mut h = [0u8; 32];
    h[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(WireDigest::new(h, 9).prefix16(), 0x0102_0304_0506_0708);
    let short = WireDigest {
        hash: vec![0xAB],
        size: 1,
    };
    assert_eq!(short.prefix16(), 0xAB00_0000_0000_0000);
}

#[test]
fn a_run_record_carries_its_id_and_times() {
    // Catches: id fields written to the wrong parts, a non-ok run counted as ok, or
    // a time computed from the wrong timestamps.
    let mut r = RunRecord::default();
    let id = RunId::new(42, StreamId(7), 3);
    r.set_run_id(id);
    assert_eq!(r.run_id(), id);
    assert!(r.failed(), "unspecified is not ok");
    r.outcome = RunOutcome::Ok.into();
    assert!(!r.failed());
    assert_eq!((r.execution_us(), r.wall_us()), (0, 0));
    r.timestamps = Some(Timestamps {
        queued_us: 10,
        execution_start_us: 20,
        execution_completed_us: 50,
        worker_completed_us: 70,
        ..Timestamps::default()
    });
    assert_eq!((r.execution_us(), r.wall_us()), (30, 60));
}

#[test]
fn a_tail_keeps_the_end_of_the_log() {
    // Catches: the head kept instead of the tail, or the length of the tail stored
    // as the log's length.
    let log: Vec<u8> = (0..TAIL_BYTES + 10).map(|i| (i % 251) as u8).collect();
    let t = Tail::of(&log);
    assert_eq!(t.bytes.len(), TAIL_BYTES);
    assert_eq!(t.bytes[..], log[10..]);
    assert_eq!(t.total_bytes, log.len() as u64);
    assert_eq!(t.omitted(), TailOmitted::No);
    assert_eq!(Tail::of(b"short").bytes, b"short");
}

#[test]
fn a_command_rendering_is_bounded_at_four_kib() {
    // Catches: a bound looser than the design's 4 KiB rendering (ui-data.md 5.3 and
    // 4.2 size the failed-run section and its cost on it).
    let mut c = CommandRendering {
        argv: vec!["x".repeat(4 * 1024 + 1)],
        ..CommandRendering::default()
    };
    c.bound();
    assert!(c.truncated);
    assert!(c.argv.is_empty());
    let mut fits = CommandRendering {
        argv: vec!["x".repeat(4 * 1024)],
        ..CommandRendering::default()
    };
    fits.bound();
    assert!(!fits.truncated);
}

#[test]
fn a_command_rendering_is_bounded_from_the_end() {
    // Catches: an unbounded rendering, arguments dropped before output paths, or a
    // cut rendering not marked truncated.
    let big = "x".repeat(COMMAND_BYTES / 4);
    let mut c = CommandRendering {
        argv: vec!["cc".into(), big.clone()],
        env_names: vec![big.clone(), "PATH".into()],
        working_directory: "w".into(),
        output_paths: vec![big.clone(), big.clone()],
        truncated: false,
    };
    c.bound();
    assert!(c.truncated);
    assert_eq!(c.argv.len(), 2);
    assert_eq!(c.env_names.len(), 2);
    assert_eq!(c.output_paths.len(), 1, "only the last output path went");
    let mut fits = CommandRendering {
        argv: vec!["cc".into()],
        ..CommandRendering::default()
    };
    fits.bound();
    assert!(!fits.truncated);
    assert_eq!(fits.argv, ["cc"]);
    // A working directory alone over the bound is cleared.
    let mut wd = CommandRendering {
        working_directory: "d".repeat(COMMAND_BYTES + 1),
        argv: vec!["a".into()],
        ..CommandRendering::default()
    };
    wd.bound();
    assert!(wd.truncated);
    assert!(wd.working_directory.is_empty());
    assert!(wd.argv.is_empty());
}

#[test]
fn chunk_keys_sort_runs_then_failures_then_hits() {
    // Catches: a key that sorts a failure among the runs (the failed section would
    // not be adjacent blocks) or a little-endian sequence (run 256 before run 1).
    let run = |seq| {
        ChunkBody::Run(RunRecord {
            seq,
            instance: "a".into(),
            ..RunRecord::default()
        })
    };
    let failed = ChunkBody::Failed(FailedRun {
        seq: 0,
        instance: "b".into(),
        ..FailedRun::default()
    });
    let hit = ChunkBody::Hit(HitRecord {
        row: 0,
        instance: "c".into(),
        ..HitRecord::default()
    });
    assert!(run(1).key() < run(256).key());
    assert!(run(u32::MAX).key() < failed.key());
    assert!(failed.key() < hit.key());
    let instances: Vec<_> = [run(0), failed, hit]
        .iter()
        .map(|b| b.instance().to_owned())
        .collect();
    assert_eq!(instances, ["a", "b", "c"]);
}

#[test]
fn a_chunk_row_holds_only_its_stream_and_range() {
    // Catches: a row matched on sequence alone (another stream's run would resolve
    // to this chunk) or a hits-only chunk taken as holding a run.
    let row = ChunkRow {
        stream: 5,
        first_seq: 10,
        last_seq: 20,
        runs: 11,
        ..ChunkRow::default()
    };
    assert!(row.holds(5, 10));
    assert!(row.holds(5, 20));
    assert!(!row.holds(5, 9));
    assert!(!row.holds(5, 21));
    assert!(!row.holds(6, 15));
    let hits_only = ChunkRow {
        stream: 0,
        ..ChunkRow::default()
    };
    assert!(!hits_only.holds(0, 0));
}

#[test]
fn the_target_and_the_list_time_have_their_own_field_numbers() {
    // Catches: a field renumbered or dropped from the wire, and a record written
    // before the fields existed failing to decode or reading them set.
    use prost::Message as _;
    let run = RunRecord {
        target: "//a:t".into(),
        ..RunRecord::default()
    };
    // Tag 34, length-delimited: key 274 (0x92 0x02).
    assert_eq!(
        run.encode_to_vec(),
        [0x92, 0x02, 5, b'/', b'/', b'a', b':', b't']
    );
    let header = InvocationHeader {
        list_ms: 5,
        ..InvocationHeader::default()
    };
    // Tag 14, varint: key 112.
    assert_eq!(header.encode_to_vec(), [112, 5]);
    assert_eq!(RunRecord::decode(&[][..]).unwrap().target, "");
    assert_eq!(InvocationHeader::decode(&[][..]).unwrap().list_ms, 0);
}

#[test]
fn the_basis_footprint_pressure_and_booking_have_their_field_numbers() {
    // Catches: a field renumbered or dropped from the wire, a zero written as absent
    // (a measured zero pressure must read back as 0), and a record written before the
    // fields existed failing to decode or reading them set.
    use prost::Message as _;
    let run = RunRecord {
        basis: UsageBasis::MacFootprint.into(),
        footprint: Some(Footprint {
            peak_footprint_bytes: Some(9),
            cpu_user_us: None,
            cpu_system_us: Some(0),
            wall_us: None,
        }),
        node_cpu_pressure_centi: Some(0),
        booked_millicpus: Some(1500),
        ..RunRecord::default()
    };
    // Tag 39 varint (key 312: 0xB8 0x02), tag 40 length-delimited (key 322: 0xC2 0x02)
    // holding fields 1 and 3, tag 41 varint (key 328: 0xC8 0x02), tag 42 varint (key 336:
    // 0xD0 0x02), 1500 is 0xDC 0x0B.
    assert_eq!(
        run.encode_to_vec(),
        [
            0xB8, 0x02, 2, 0xC2, 0x02, 4, 0x08, 9, 0x18, 0, 0xC8, 0x02, 0, 0xD0, 0x02, 0xDC, 0x0B
        ]
    );
    let back = RunRecord::decode(run.encode_to_vec().as_slice()).unwrap();
    assert_eq!(back, run);
    assert_eq!(back.basis(), UsageBasis::MacFootprint);
    let older = RunRecord::decode(RunRecord::default().encode_to_vec().as_slice()).unwrap();
    assert_eq!(older.basis(), UsageBasis::Unspecified);
    assert_eq!(older.footprint, None);
    assert_eq!(older.node_cpu_pressure_centi, None);
    assert_eq!(older.booked_millicpus, None);
}

#[test]
fn the_peak_and_the_wall_are_optional_on_the_old_tags() {
    // Catches: tag 7 or 9 renumbered, a measured zero dropped from the wire (a zero
    // peak must stay distinct from none), an absent value written as 0, and a non-zero
    // value encoded differently from the plain field older writers used (older readers
    // and chunks must read it unchanged).
    use prost::Message as _;
    let block = |peak, wall| Cgroup {
        memory_peak_bytes: peak,
        wall_us: wall,
        ..Cgroup::default()
    };
    // Tag 7 varint is key 0x38, tag 9 is key 0x48.
    assert_eq!(block(Some(0), None).encode_to_vec(), [0x38, 0]);
    assert_eq!(block(None, Some(0)).encode_to_vec(), [0x48, 0]);
    assert!(block(None, None).encode_to_vec().is_empty());
    assert_eq!(
        block(Some(5), Some(300)).encode_to_vec(),
        [0x38, 5, 0x48, 0xAC, 0x02]
    );
    // What an older writer wrote for a zero (field omitted) reads as none; non-zero reads.
    let old = Cgroup::decode([0x38, 5, 0x48, 0xAC, 0x02].as_slice()).unwrap();
    assert_eq!((old.memory_peak_bytes, old.wall_us), (Some(5), Some(300)));
    let omitted = Cgroup::decode([].as_slice()).unwrap();
    assert_eq!((omitted.memory_peak_bytes, omitted.wall_us), (None, None));
}

#[test]
fn the_phase_times_marker_round_trips_and_an_old_record_reads_as_measured() {
    // Catches: the marker lost on the wire, a record without it read as a claim, and the
    // marker taking a field number another record uses (field 50; 48 and 49 are #2200's).
    use prost::Message as _;
    let mut r = RunRecord::default();
    assert_eq!(r.phase_times(), PhaseTimes::Unspecified);
    assert_eq!(r.phase_times().name(), "measured");
    r.phase_times = PhaseTimes::TaskClaim.into();
    let back = RunRecord::decode(r.encode_to_vec().as_slice()).expect("decodes");
    assert_eq!(back.phase_times(), PhaseTimes::TaskClaim);
    assert_eq!(back.phase_times().name(), "task_claim");
    r.phase_times = PhaseTimes::Measured.into();
    let back = RunRecord::decode(r.encode_to_vec().as_slice()).expect("decodes");
    assert_eq!(back.phase_times().name(), "measured");
    let only = RunRecord {
        phase_times: PhaseTimes::TaskClaim.into(),
        ..RunRecord::default()
    };
    assert_eq!(
        only.encode_to_vec(),
        [0x90, 0x03, 0x02],
        "field 50, varint 2"
    );
}
