//! The stored form of a size summary, as bytes: one fixed-size record (a version byte,
//! little-endian fields, an FNV-1a checksum) that the state store keeps in an `est/` row
//! and that every later build must read as the summary it was written from.
//!
//! `tests/golden/codec-v2/` holds the record of each summary below as hex text, after comment
//! lines (beginning with `#`) that say what the file pins. The summaries are synthetic: no
//! farm data was read to make them. Each is made by feeding made-up runs to the estimator's
//! own update path (`Summary::new` and `Summary::observe`, the calls `Summaries::observe`
//! makes) and encoding what it holds, so a summary is a state that path can reach. The runs
//! have the shapes a farm records: quiet successes of several sizes on several days, an
//! own-limit kill that sets a floor, node-pressure kills, a Mac footprint and a mix of small
//! and large runs that wraps the sample ring. This test requires, for each file:
//!
//! - this build encodes the summary to the bytes of the file;
//! - this build decodes the bytes to that summary, and encodes what it decoded to the same
//!   bytes (a record that decodes and encodes to something else would change under every
//!   rewrite of a row);
//! - a record with its version byte changed, or any other byte changed, is refused.
//!
//! `tests/golden/codec/` holds the records the build before the version 2 record (which adds
//! the 32-byte minute ring) wrote for the same corpus. Those files are what stores written by
//! that build hold: this build reads each as a summary and writes it back as version 2, which
//! reads as the same summary.
//!
//! Both directories are append-only: a file that exists is never rewritten to match a new
//! encoding, because its bytes are what an earlier build wrote. A change to a file needs a
//! reason in the pull request that makes it. A summary without a file in `codec-v2` is
//! written by `cargo test -p kbf-estimator --test codec_golden -- --ignored
//! write_missing_files`, which never replaces a file.

use std::path::{Path, PathBuf};

use kbf_estimator::{Basis, Bounds, Day, GIB, MIB, Outcome, Run, Summary};

/// The version byte of the record, its first byte.
const VERSION: u8 = 2;
/// The length of the record: every field, fixed.
const LEN: usize = 718;
/// The version byte of the record the build before wrote.
const VERSION_1: u8 = 1;
/// The length of that record: [`LEN`] without the 32 bytes of the minute ring.
const LEN_1: usize = LEN - 32;

/// A run of `outcome` that took 2 s of wall time and used `peak` bytes of a booking of 4 GiB.
fn run(outcome: Outcome, peak: u64) -> Run {
    Run {
        outcome,
        wall_us: 2_000_000,
        peak_memory_bytes: Some(peak),
        booked_memory_bytes: 4 * GIB,
        raised_memory_bytes: None,
        limit_hits: 0,
        swap_peak_bytes: 0,
        cpu_usage_us: 0,
        node_cpu_pressure_permille: None,
    }
}

/// A summary of `basis` first seen on `first`, fed `runs` (each with the day it ran on).
fn summary(basis: Basis, first: u32, runs: &[(u32, Run)]) -> Summary {
    let mut summary = Summary::new(basis, Day(first));
    for (day, run) in runs {
        summary.observe(run, Day(*day), &Bounds::default());
    }
    summary
}

/// The rows: the name of a file (without `.hex`) and the summary its bytes decode to.
fn corpus() -> Vec<(&'static str, Summary)> {
    let success = |peak| run(Outcome::Success, peak);

    let cold = summary(Basis::Cgroup, 100, &[(100, success(3 * GIB / 2))]);

    // 40 successes over four days, 600 MiB to 1000 MiB, each using 1 to 2 cores of CPU.
    let steady: Vec<(u32, Run)> = (0..40_u64)
        .map(|i| {
            let wall_us = 2_000_000 + i * 50_000;
            let busy = Run {
                wall_us,
                cpu_usage_us: wall_us * (1_000 + i % 5 * 250) / 1_000,
                ..success((600 + i * 37 % 400) * MIB)
            };
            (100 + u32::try_from(i / 10).expect("a day"), busy)
        })
        .collect();
    let steady = summary(Basis::Cgroup, 100, &steady);

    // Twelve quiet successes, a kill at the 4 GiB booking that the ladder retries at 16 GiB,
    // and three successes after it.
    let mut killed: Vec<(u32, Run)> = (0..12_u64)
        .map(|i| {
            (
                100 + u32::try_from(i / 4).expect("a day"),
                success((1_100 + i * 20) * MIB),
            )
        })
        .collect();
    killed.push((
        105,
        Run {
            raised_memory_bytes: Some(16 * GIB),
            limit_hits: 3,
            swap_peak_bytes: 256 * MIB,
            ..run(Outcome::OwnLimitKill, 6 * GIB + GIB / 2)
        },
    ));
    killed.extend((0..3_u64).map(|i| (106, success((5 + i) * GIB))));
    let killed = summary(Basis::Cgroup, 100, &killed);

    // Eight runs on a busy node: a kill by node pressure, then successes that began under
    // CPU pressure.
    let mut pressed: Vec<(u32, Run)> = vec![(
        300,
        Run {
            wall_us: 3_000_000,
            ..run(Outcome::NodePressureKill, 3 * GIB)
        },
    )];
    pressed.extend((0..7_u64).map(|i| {
        let busy = Run {
            node_cpu_pressure_permille: Some(u16::try_from(50 + i * 40).expect("a permille")),
            cpu_usage_us: 4_000_000,
            ..success((900 + i * 10) * MIB)
        };
        (300 + u32::try_from(i / 3).expect("a day"), busy)
    }));
    let pressed = summary(Basis::Cgroup, 300, &pressed);

    // A Mac footprint: ten successes, footprints of 2 to 3 GiB.
    let mac: Vec<(u32, Run)> = (0..10_u64)
        .map(|i| {
            (
                200 + u32::try_from(i / 5).expect("a day"),
                success(2 * GIB + i * 100 * MIB),
            )
        })
        .collect();
    let mac = summary(Basis::MacFootprint, 200, &mac);

    // Seventy runs of two sizes, small and large, over ten days: more than the sample ring
    // holds, so it has wrapped.
    let mixed: Vec<(u32, Run)> = (0..70_u64)
        .map(|i| {
            let peak = if i % 3 == 0 {
                5 * GIB + i * MIB
            } else {
                700 * MIB + i * MIB
            };
            (400 + u32::try_from(i / 7).expect("a day"), success(peak))
        })
        .collect();
    let mixed = summary(Basis::Cgroup, 400, &mixed);

    vec![
        ("cold-one-success", cold),
        ("steady-cgroup-four-days", steady),
        ("own-limit-kill-sets-a-floor", killed),
        ("node-pressure-and-cpu-pressure", pressed),
        ("mac-footprint", mac),
        ("mixed-sizes-ring-wrapped", mixed),
    ]
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/codec-v2")
}

/// The files of the version 1 records.
fn golden_dir_1() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/codec")
}

fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "an odd number of hex digits");
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex digits"))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The record in the file `name`.hex of the golden directory, without its comment lines.
fn record(name: &str) -> Vec<u8> {
    record_in(&golden_dir(), name)
}

/// The record in the file `name`.hex of `dir`, without its comment lines.
fn record_in(dir: &Path, name: &str) -> Vec<u8> {
    let path = dir.join(format!("{name}.hex"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let digits: String = text.lines().filter(|line| !line.starts_with('#')).collect();
    unhex(digits.trim())
}

/// Catches: an encoder that writes another byte for the same summary (a field moved,
/// widened or re-ordered, a basis renumbered, a checksum computed over other bytes), which a
/// build that reads the old rows would take for a corrupt row and start the key cold.
#[test]
fn every_summary_encodes_to_the_bytes_of_its_file() {
    for (name, summary) in corpus() {
        let want = record(name);
        assert_eq!(want.len(), LEN, "{name}: the length of the record");
        assert_eq!(want[0], VERSION, "{name}: the version byte");
        assert_eq!(
            hex(&summary.encode()),
            hex(&want),
            "{name}: the encoding changed"
        );
    }
}

/// Catches: a decoder that reads a record an earlier build wrote as another summary, or not
/// at all, and a pair of codecs that do not agree (decode then encode changes the bytes).
#[test]
fn every_record_decodes_to_its_summary_and_encodes_back_to_the_same_bytes() {
    for (name, summary) in corpus() {
        let bytes = record(name);
        let read = Summary::decode(&bytes);
        assert_eq!(read.as_ref(), Some(&summary), "{name}: the decoded summary");
        let again = read.map(|read| read.encode());
        assert_eq!(again.as_deref(), Some(&bytes[..]), "{name}: the round trip");
    }
}

/// Catches: a decoder that accepts a record of another version, one cut short or one with a
/// byte changed (the checksum covers every byte before it, and the version is checked).
#[test]
fn a_record_that_is_not_exactly_one_of_the_files_is_refused() {
    for (name, _) in corpus() {
        let bytes = record(name);
        let mut other = bytes.clone();
        other[0] = VERSION + 1;
        assert_eq!(Summary::decode(&other), None, "{name}: another version");
        for at in [1, 2, 40, LEN / 2, LEN - 9, LEN - 1] {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0x01;
            assert_eq!(Summary::decode(&flipped), None, "{name}: byte {at} changed");
        }
        assert_eq!(
            Summary::decode(&bytes[..LEN - 1]),
            None,
            "{name}: cut short"
        );
        let longer = [&bytes[..], &[0]].concat();
        assert_eq!(Summary::decode(&longer), None, "{name}: a byte too many");
    }
}

/// The names of the files in `dir`, sorted.
fn files_in(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .expect("a golden directory")
        .map(|entry| entry.expect("an entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    found.sort();
    found
}

/// Asserts that each golden directory holds a file for every summary of the corpus and no
/// other file: a file added without the summary it must decode to, and a file deleted, each
/// fail here.
#[test]
fn the_golden_directories_hold_exactly_the_files_the_corpus_names() {
    let mut named: Vec<String> = corpus()
        .into_iter()
        .map(|(name, _)| format!("{name}.hex"))
        .collect();
    named.sort();
    assert_eq!(files_in(&golden_dir()), named);
    assert_eq!(files_in(&golden_dir_1()), named);
}

/// Catches: a build that cannot read a record the build before it wrote (a store written by
/// that build holds only these), and one that writes such a record back as something that
/// does not read as the same summary: each version 1 file decodes, is written as a version 2
/// record with the minute ring added, and that record decodes to the same summary. A version 1
/// record with a byte changed or cut short is refused as before.
#[test]
fn a_version_1_record_still_reads_and_is_written_back_as_version_2() {
    for (name, _) in corpus() {
        let bytes = record_in(&golden_dir_1(), name);
        assert_eq!(bytes.len(), LEN_1, "{name}: the length of the record");
        assert_eq!(bytes[0], VERSION_1, "{name}: the version byte");
        let read = Summary::decode(&bytes).unwrap_or_else(|| panic!("{name}: not read"));
        let again = read.encode();
        assert_eq!(again.len(), LEN, "{name}: written back at the new length");
        assert_eq!(again[0], VERSION, "{name}: written back as the new version");
        assert_eq!(
            Summary::decode(&again),
            Some(read),
            "{name}: the same summary"
        );
        let mut flipped = bytes.clone();
        flipped[LEN_1 / 2] ^= 0x01;
        assert_eq!(Summary::decode(&flipped), None, "{name}: a byte changed");
        assert_eq!(
            Summary::decode(&bytes[..LEN_1 - 1]),
            None,
            "{name}: cut short"
        );
    }
}

/// Catches: a corpus that has lost the shapes it is there for: a floor set by a kill, both
/// bases, and a ring that has wrapped, so every field of the record holds a value other than
/// its start.
#[test]
fn the_corpus_holds_a_floor_both_bases_and_a_wrapped_ring() {
    let all = corpus();
    let held = |name: &str| &all.iter().find(|(n, _)| *n == name).expect("a row").1;
    assert!(held("own-limit-kill-sets-a-floor").oom_floor() >= 16 * GIB);
    assert_eq!(held("mac-footprint").basis(), Basis::MacFootprint);
    assert_eq!(held("cold-one-success").basis(), Basis::Cgroup);
    assert!(held("mixed-sizes-ring-wrapped").samples() > 32);
    assert!(held("mixed-sizes-ring-wrapped").mixed());
    assert_eq!(held("steady-cgroup-four-days").last_seen(), Day(103));
}

/// Writes the file of each summary that has none, with a header and the record. A
/// release-time step: it never replaces a file.
#[test]
#[ignore = "writes the files of summaries that have none; run by a person with the row"]
fn write_missing_files() {
    std::fs::create_dir_all(golden_dir()).expect("the golden directory");
    for (name, summary) in corpus() {
        let path = golden_dir().join(format!("{name}.hex"));
        if path.exists() {
            continue;
        }
        let text = format!(
            "# Size summary `{name}`: the stored record of one summary, version {VERSION}.\n\
             # Synthetic: made-up runs fed to the estimator's update path, not farm data.\n\
             # Pins: this build encodes the summary to these bytes, decodes them to it, and\n\
             # refuses a record with any byte changed.\n\
             # A change to this file needs a reason in the pull request that makes it, and the\n\
             # file is never rewritten to match a new encoding: see tests/codec_golden.rs.\n\
             {}\n",
            hex(&summary.encode())
        );
        std::fs::write(&path, text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
}
