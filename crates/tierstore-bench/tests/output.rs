//! The result and sample files: where they go, the header written once, and what a directory that
//! is not there or a file that cannot be written does.

use std::collections::BTreeMap;

use kbf_testkit::dir::TestDir;
use tierstore_bench::cells::CellId;
use tierstore_bench::error::BenchError;
use tierstore_bench::output::{SAMPLES_FILE, SampleWriter, write_result};
use tierstore_bench::result::{CellResult, Op, Provenance, SCHEMA_VERSION, Sample, Window};
use tierstore_bench::role::Role;

fn result() -> CellResult {
    CellResult {
        schema_version: SCHEMA_VERSION,
        cell: CellId::ColdPut,
        arm: "direct-c4".to_owned(),
        started_unix_ms: 1,
        finished_unix_ms: 2,
        provenance: Provenance::unrecorded(),
        passes: Vec::new(),
        notes: vec!["a note".to_owned()],
    }
}

fn sample(latency_us: u64) -> Sample {
    Sample {
        cell: CellId::ColdPut,
        arm: "direct-c4".to_owned(),
        pass: 0,
        window: Window::Busy,
        role: Role::Cold,
        op: Op::Put,
        bytes: 8,
        in_flight: 4,
        started_unix_ms: 5,
        latency_us,
        status: Some(200),
    }
}

/// Catches a result written under another name or not readable back: the file is
/// `<cell>-<arm>.json` and is the result.
#[test]
fn a_result_is_written_as_its_cell_and_arm_and_reads_back() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-out-result");
    let path = write_result(dir.path(), &result()).unwrap();
    assert_eq!(path, dir.path().join("cold-put-direct-c4.json"));
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(CellResult::from_json(&text).unwrap(), result());
}

/// Catches a write into a directory that is not there being reported as done.
#[test]
fn a_directory_that_is_not_there_is_an_io_error_for_both_files() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-out-nodir");
    let missing = dir.path().join("missing");
    assert!(matches!(
        write_result(&missing, &result()),
        Err(BenchError::Io { .. })
    ));
    assert!(matches!(
        SampleWriter::open(&missing),
        Err(BenchError::Io { .. })
    ));
}

/// Catches the header repeated, lost or written after rows: a second writer on the same file
/// adds rows and no header, and the rows are in the order appended.
#[test]
fn the_header_is_written_once_and_rows_are_appended_in_order() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-out-csv");
    let provenance = Provenance {
        run_id: "r1".to_owned(),
        git_sha: String::new(),
        binary_digest: String::new(),
        tool_versions: BTreeMap::new(),
        hardware: BTreeMap::new(),
    };
    SampleWriter::open(dir.path())
        .unwrap()
        .append(&provenance, &[sample(10), sample(20)])
        .unwrap();
    SampleWriter::open(dir.path())
        .unwrap()
        .append(&provenance, &[sample(30)])
        .unwrap();
    let text = std::fs::read_to_string(dir.path().join(SAMPLES_FILE)).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    assert_eq!(lines[0], Sample::CSV_HEADER);
    let latencies: Vec<&str> = lines[1..]
        .iter()
        .map(|l| l.split(',').nth(12).unwrap())
        .collect();
    assert_eq!(latencies, ["10", "20", "30"]);
}

/// Catches a sample file that cannot take its header being opened as if it could: a file that is
/// full is an io error at open.
#[cfg(target_os = "linux")]
#[test]
fn a_samples_file_that_cannot_be_written_is_an_io_error_at_open() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-out-full");
    std::os::unix::fs::symlink("/dev/full", dir.path().join(SAMPLES_FILE)).unwrap();
    assert!(matches!(
        SampleWriter::open(dir.path()),
        Err(BenchError::Io { .. })
    ));
}
