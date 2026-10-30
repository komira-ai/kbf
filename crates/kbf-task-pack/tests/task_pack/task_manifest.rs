//! The manifest of a task pack: the golden fixtures under `tests/fixtures/task_pack`, which the
//! frontend's ingest and the task tool test against too, and one test per refusal.

use std::path::Path;

use kbf_segments::sha256;
use kbf_task_pack::{
    EMPTY_DIGEST, InputStats, LargeEntry, MANIFEST_VERSION, MANIFEST_VERSION_2, MAX_LARGE,
    MAX_MANIFEST_BYTES, MAX_PACKS, MAX_RECORD_BYTES, ManifestError, OutputDirectory, OutputFile,
    OutputSymlink, PathError, SourceBytes, TaskKeyError, TaskManifest, Timing, Usage,
};
use kbf_types::{Digest, DigestFunction, ParseDigestError};

/// The bytes of a fixture under `tests/fixtures/task_pack`.
pub(crate) fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/task_pack")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {}: {e}", path.display()))
}

/// The golden pack: `task_pack_v1.pack.hex` is the pack in hexadecimal (64 digits a line), because
/// the repository's text lint refuses a file with a NUL byte.
pub(crate) fn golden_pack() -> Vec<u8> {
    let text = String::from_utf8(fixture("task_pack_v1.pack.hex")).expect("hex is text");
    let digits: String = text.split_whitespace().collect();
    hex::decode(digits).expect("hexadecimal digits")
}

pub(crate) fn digest(text: &str) -> Digest {
    Digest::parse(DigestFunction::Sha256, text).expect("a digest in text form")
}

const LEASE: &str = "0000000000000001000000000000002a";
const ACTION: &str = "33d8589de117bdf22bdc10c48a5908a4bbd88481d54cf7c0c358905e72b79abb/18";
const A_O: &str = "4b2cb5acb6856c5130b9bf553b2d0ff59552ea5e028f0b4e391b0581db48b71c/17";
const TOOL: &str = "81e6db07b4376d9a120a55c66f8812633f7733c2699a07cecac0bd363a9ce426/35";
const STDOUT: &str = "bd6c3301ad961288669682494372379ffe35526bf49c5f4bf3d85eba4bd220ae/14";
const STDERR: &str = "e5cde090a13f5b74917873ea155e00f8407867c992ab09c2102df7aa50c7d43a/25";
const TREE: &str = "793526f079189e7aaf35fa52cfdda4139699c8cd80832e8f404e1290b236a848/77";

/// Sets one timing or usage number.
type Setter = fn(&mut TaskManifest, u64);

fn base() -> TaskManifest {
    TaskManifest {
        lease: LEASE.to_owned(),
        action: digest(ACTION),
        exit_code: 0,
        stdout: None,
        stderr: None,
        output_files: Vec::new(),
        output_directories: Vec::new(),
        output_symlinks: Vec::new(),
        timing: Timing {
            fetch_ms: 12,
            run_ms: 3456,
            pack_ms: 7,
        },
        usage: Usage {
            cpu_usage_us: 3_300_000,
            memory_peak_bytes: 104_857_600,
        },
        packs: 1,
        large: Vec::new(),
        inputs: None,
    }
}

fn file(path: &str, text: &str, is_executable: bool) -> OutputFile {
    OutputFile {
        path: path.to_owned(),
        digest: digest(text),
        is_executable,
    }
}

/// The manifest of `task_pack_v1.pack`: every field set, an empty output file among the files.
pub(crate) fn golden() -> TaskManifest {
    TaskManifest {
        stdout: Some(digest(STDOUT)),
        stderr: Some(digest(STDERR)),
        output_files: vec![
            file("out/a.o", A_O, false),
            file("bin/tool", TOOL, true),
            OutputFile {
                path: "out/empty".to_owned(),
                digest: EMPTY_DIGEST,
                is_executable: false,
            },
        ],
        output_directories: vec![OutputDirectory {
            path: "out/d".to_owned(),
            tree: digest(TREE),
        }],
        output_symlinks: vec![OutputSymlink {
            path: "out/l".to_owned(),
            target: "a.o".to_owned(),
        }],
        ..base()
    }
}

fn parse_fixture(name: &str) -> Result<TaskManifest, ManifestError> {
    TaskManifest::parse(&fixture(name))
}

fn refusal(name: &str) -> ManifestError {
    parse_fixture(name).expect_err(name)
}

/// Asserts that each canonical fixture parses to the value written out here, and that writing
/// that value gives the fixture's bytes back: the golden pair both ends of the pack test against.
#[test]
fn the_golden_manifests_parse_to_their_values_and_write_back_byte_for_byte() {
    let cases = [
        ("valid/minimal.json", base()),
        (
            "valid/empty_output_file.json",
            TaskManifest {
                stdout: Some(digest(STDOUT)),
                output_files: vec![OutputFile {
                    path: "out/empty".to_owned(),
                    digest: EMPTY_DIGEST,
                    is_executable: false,
                }],
                ..base()
            },
        ),
        (
            "valid/failed_action.json",
            TaskManifest {
                exit_code: 2,
                stderr: Some(digest(STDERR)),
                ..base()
            },
        ),
        ("task_pack_v1.manifest.json", golden()),
    ];
    for (name, expected) in cases {
        assert_eq!(parse_fixture(name).expect(name), expected, "{name}");
        assert_eq!(expected.to_json().expect(name), fixture(name), "{name}");
    }
}

/// Asserts that names that merely contain dots are paths: only a whole `..` component is refused.
#[test]
fn a_dotted_name_that_is_not_a_dotdot_component_is_allowed() {
    let manifest = parse_fixture("valid/dotted_names.json").expect("valid");
    let paths: Vec<_> = manifest.output_files.iter().map(|f| &*f.path).collect();
    assert_eq!(paths, ["out/..a", "a..b/c."]);
}

/// Asserts that the constant is the SHA-256 of no bytes (so a typo in its 32 bytes is caught).
#[test]
fn the_empty_digest_is_the_sha256_of_nothing() {
    assert_eq!(EMPTY_DIGEST, sha256(b""));
}

/// Asserts that a field the contract does not name is refused, in the manifest and in the
/// timing (`put_ms`: the upload cannot be timed by a record written before it), and a list of
/// entries (the pack's footer is that list).
#[test]
fn a_manifest_with_an_unknown_field_is_refused() {
    for name in [
        "invalid/unknown_field_put_ms.json",
        "invalid/unknown_top_level_field.json",
    ] {
        let ManifestError::Json(e) = refusal(name) else {
            panic!("{name}: not a Json refusal");
        };
        assert!(e.to_string().contains("unknown field"), "{name}: {e}");
    }
}

/// Asserts that a key given twice is refused, not resolved to one of its values.
#[test]
fn a_duplicate_key_is_refused() {
    let ManifestError::Json(e) = refusal("invalid/duplicate_key.json") else {
        panic!("not a Json refusal");
    };
    assert!(e.to_string().contains("duplicate field"), "{e}");
}

/// Asserts that a missing field, a field of the wrong type and a cut-off document are refused.
#[test]
fn a_missing_or_mistyped_field_or_a_truncated_document_is_refused() {
    let ManifestError::Json(e) = refusal("invalid/missing_timing.json") else {
        panic!("not a Json refusal");
    };
    assert!(e.to_string().contains("missing field"), "{e}");
    for name in [
        "invalid/exit_code_is_a_string.json",
        "invalid/truncated_json.json",
    ] {
        assert!(matches!(refusal(name), ManifestError::Json(_)), "{name}");
    }
}

/// Asserts that a version other than 1 and 2 is refused with the version it names, and that
/// version 2 without the fields it requires is not read as version 1.
#[test]
fn a_manifest_of_another_version_is_refused() {
    assert!(matches!(
        refusal("invalid/version_3.json"),
        ManifestError::Version(3)
    ));
    assert!(matches!(
        refusal("invalid/version_2.json"),
        ManifestError::MissingField("packs")
    ));
}

/// Asserts that a digest in uppercase hex is refused, wherever it stands, and that so is a
/// digest with no size: one digest has one text form.
#[test]
fn uppercase_hex_is_refused() {
    match refusal("invalid/uppercase_hex_action.json") {
        ManifestError::Digest { field, source } => {
            assert_eq!(field, "action");
            assert_eq!(source, ParseDigestError::HashCharacter('D', 2));
        }
        other => panic!("{other}"),
    }
    match refusal("invalid/uppercase_hex_file_digest.json") {
        ManifestError::Digest { field, source } => {
            assert_eq!(field, "output_files[0].digest");
            assert_eq!(source, ParseDigestError::HashCharacter('B', 1));
        }
        other => panic!("{other}"),
    }
    assert!(matches!(
        refusal("invalid/digest_without_size.json"),
        ManifestError::Digest {
            source: ParseDigestError::MissingSeparator(_),
            ..
        }
    ));
}

/// Asserts that a digest of size 0 passes only as the digest of no bytes: the empty output file
/// is listed with it, and a size-0 digest of another hash is refused, in a file, a tree and the
/// action.
#[test]
fn a_digest_of_size_zero_must_be_the_empty_digest() {
    match refusal("invalid/zero_size_not_the_empty_digest.json") {
        ManifestError::ZeroSizeNotEmpty { field, digest } => {
            assert_eq!(field, "output_files[0].digest");
            assert_eq!(digest.size_bytes, 0);
            assert_ne!(digest, EMPTY_DIGEST);
        }
        other => panic!("{other}"),
    }
    let wrong = Digest::new(DigestFunction::Sha256, [7; 32], 0);
    let mut tree = base();
    tree.output_directories = vec![OutputDirectory {
        path: "d".to_owned(),
        tree: wrong,
    }];
    assert!(matches!(
        tree.validate(),
        Err(ManifestError::ZeroSizeNotEmpty { field, .. }) if field == "output_directories[0].tree"
    ));
    let mut action = base();
    action.action = wrong;
    assert!(matches!(
        action.validate(),
        Err(ManifestError::ZeroSizeNotEmpty { field, .. }) if field == "action"
    ));
    let mut empty_tree = base();
    empty_tree.output_directories = vec![OutputDirectory {
        path: "d".to_owned(),
        tree: EMPTY_DIGEST,
    }];
    assert!(empty_tree.validate().is_ok());
}

/// Asserts that a stream that is empty is left out: listing the empty digest as `stdout` or
/// `stderr` is refused, so one run has one manifest.
#[test]
fn an_empty_stream_is_omitted_not_listed() {
    assert!(matches!(
        refusal("invalid/stdout_present_and_empty.json"),
        ManifestError::EmptyStream("stdout")
    ));
    let mut manifest = base();
    manifest.stderr = Some(EMPTY_DIGEST);
    assert!(matches!(
        manifest.validate(),
        Err(ManifestError::EmptyStream("stderr"))
    ));
}

fn path_reason(name: &str) -> PathError {
    match refusal(name) {
        ManifestError::Path { reason, .. } => reason,
        other => panic!("{name}: {other}"),
    }
}

/// Asserts that a path with a `..` component or a leading `/` is refused, and the other ways a
/// path is not one: empty, a NUL byte, over 4096 bytes.
#[test]
fn a_path_with_dotdot_or_a_leading_slash_is_refused() {
    assert_eq!(
        path_reason("invalid/path_with_dotdot.json"),
        PathError::ParentComponent
    );
    assert_eq!(
        path_reason("invalid/path_with_leading_slash.json"),
        PathError::Absolute
    );
    assert_eq!(path_reason("invalid/path_empty.json"), PathError::Empty);
    assert_eq!(path_reason("invalid/path_with_nul.json"), PathError::Nul);
    assert_eq!(
        path_reason("invalid/path_too_long.json"),
        PathError::TooLong(4097)
    );
}

/// Asserts that 4096 bytes is the longest path allowed.
#[test]
fn a_path_of_4096_bytes_is_allowed() {
    let mut manifest = base();
    manifest.output_files = vec![file(&"a".repeat(4096), A_O, false)];
    assert!(manifest.validate().is_ok());
}

/// Asserts that two outputs with one path are refused whatever their kinds: a file and a
/// symlink in the fixture, a file and a directory, a directory and a symlink here.
#[test]
fn a_path_listed_twice_is_refused() {
    assert!(matches!(
        refusal("invalid/path_listed_twice.json"),
        ManifestError::DuplicatePath(p) if p == "out/x"
    ));
    let mut manifest = base();
    manifest.output_files = vec![file("p", A_O, false)];
    manifest.output_directories = vec![OutputDirectory {
        path: "p".to_owned(),
        tree: digest(TREE),
    }];
    assert!(matches!(
        manifest.validate(),
        Err(ManifestError::DuplicatePath(_))
    ));
    manifest.output_files.clear();
    manifest.output_symlinks = vec![OutputSymlink {
        path: "p".to_owned(),
        target: "t".to_owned(),
    }];
    assert!(matches!(
        manifest.validate(),
        Err(ManifestError::DuplicatePath(_))
    ));
}

/// Asserts that a symlink target is refused when empty, holding a NUL or over 4096 bytes, and
/// allowed when absolute or leaving the directory: where a link points is the action's business.
#[test]
fn a_symlink_target_is_not_empty_nul_or_over_4096_bytes() {
    assert!(matches!(
        refusal("invalid/symlink_target_empty.json"),
        ManifestError::Target {
            reason: PathError::Empty,
            ..
        }
    ));
    let with_target = |target: &str| {
        let mut manifest = base();
        manifest.output_symlinks = vec![OutputSymlink {
            path: "l".to_owned(),
            target: target.to_owned(),
        }];
        manifest.validate()
    };
    assert!(matches!(
        with_target("a\0b"),
        Err(ManifestError::Target {
            reason: PathError::Nul,
            ..
        })
    ));
    assert!(matches!(
        with_target(&"a".repeat(4097)),
        Err(ManifestError::Target {
            reason: PathError::TooLong(4097),
            ..
        })
    ));
    assert!(with_target("/etc/passwd").is_ok());
    assert!(with_target("../up").is_ok());
}

/// Asserts that a lease that cannot be the middle of a key is refused: with a `/`, empty.
#[test]
fn a_lease_that_is_not_a_build_id_is_refused() {
    assert!(matches!(
        refusal("invalid/lease_with_slash.json"),
        ManifestError::Lease(TaskKeyError::BuildCharacter('/'))
    ));
    let mut manifest = base();
    manifest.lease.clear();
    assert!(matches!(
        manifest.validate(),
        Err(ManifestError::Lease(TaskKeyError::EmptyBuild))
    ));
}

/// Asserts that a timing or usage number beyond what an `i64` holds is refused, field by field,
/// and that `i64::MAX` itself is allowed: the numbers are advisory but end up in metrics.
#[test]
fn a_timing_or_usage_beyond_i64_is_refused() {
    assert!(matches!(
        refusal("invalid/timing_beyond_i64.json"),
        ManifestError::Advisory {
            field: "timing.run_ms",
            value: 9_223_372_036_854_775_808
        }
    ));
    let over = i64::MAX as u64 + 1;
    let fields: [(&str, Setter); 5] = [
        ("timing.fetch_ms", |m, v| m.timing.fetch_ms = v),
        ("timing.run_ms", |m, v| m.timing.run_ms = v),
        ("timing.pack_ms", |m, v| m.timing.pack_ms = v),
        ("usage.cpu_usage_us", |m, v| m.usage.cpu_usage_us = v),
        ("usage.memory_peak_bytes", |m, v| {
            m.usage.memory_peak_bytes = v
        }),
    ];
    for (name, set) in fields {
        let mut manifest = base();
        set(&mut manifest, i64::MAX as u64);
        assert!(manifest.validate().is_ok(), "{name} at i64::MAX");
        set(&mut manifest, over);
        assert!(
            matches!(manifest.validate(), Err(ManifestError::Advisory { field, value }) if field == name && value == over),
            "{name}"
        );
    }
}

/// Asserts that a manifest over 4 MiB is refused before it is read (garbage of that size says
/// `TooLarge`, not a parse error), and that exactly 4 MiB is read.
#[test]
fn a_manifest_over_four_mebibytes_is_refused_unread() {
    let limit = usize::try_from(MAX_MANIFEST_BYTES).expect("fits");
    let over = vec![b' '; limit + 1];
    assert!(matches!(
        TaskManifest::parse(&over),
        Err(ManifestError::TooLarge { len }) if len == MAX_MANIFEST_BYTES + 1
    ));
    let at = vec![b' '; limit];
    assert!(matches!(
        TaskManifest::parse(&at),
        Err(ManifestError::Json(_))
    ));
}

/// Asserts that the writer refuses what the reader refuses: an invalid manifest does not
/// serialise, and nor does one whose JSON passes 4 MiB.
#[test]
fn to_json_refuses_an_invalid_manifest_and_one_over_four_mebibytes() {
    let mut bad = base();
    bad.output_files = vec![file("../x", A_O, false)];
    assert!(matches!(bad.to_json(), Err(ManifestError::Path { .. })));

    let mut big = base();
    big.output_files = (0..1100)
        .map(|i| file(&format!("{i:04}{}", "a".repeat(4092)), A_O, false))
        .collect();
    assert!(matches!(big.to_json(), Err(ManifestError::TooLarge { .. })));
}

/// Asserts which digests need a record in the pack or a copy in the store: both streams, each
/// file and each tree, in that order; not the empty file and not the action.
#[test]
fn the_output_digests_skip_the_empty_file_and_the_action() {
    let listed: Vec<Digest> = golden().output_digests().collect();
    assert_eq!(listed, [STDOUT, STDERR, A_O, TOOL, TREE].map(digest));
}

// Version 2: several packs and large objects.

/// The large cap of the farm: 512 MiB.
const LARGE_MAX: u64 = 536_870_912;

const BIG_1: &str = "1111111111111111111111111111111111111111111111111111111111111111/138847232";
const BIG_2: &str = "2222222222222222222222222222222222222222222222222222222222222222/114300000";

fn parse_v2(name: &str) -> Result<TaskManifest, ManifestError> {
    TaskManifest::parse_with(&fixture(name), LARGE_MAX)
}

fn refusal_v2(name: &str) -> ManifestError {
    parse_v2(name).expect_err(name)
}

fn large(slot: u8, text: &str) -> LargeEntry {
    LargeEntry {
        slot,
        digest: digest(text),
    }
}

/// Asserts that each version 2 fixture parses (under the farm's large cap) to the value written
/// out here, that it is written back byte for byte, and that it is written as version 2: two
/// packs, one large object, and both.
#[test]
fn the_golden_v2_manifests_parse_to_their_values_and_write_back_byte_for_byte() {
    let cases = [
        (
            "valid_v2/two_packs.json",
            TaskManifest { packs: 2, ..base() },
        ),
        (
            "valid_v2/one_large.json",
            TaskManifest {
                output_files: vec![file("bin/mojo", BIG_1, true)],
                large: vec![large(0, BIG_1)],
                ..base()
            },
        ),
        (
            "valid_v2/packs_and_large.json",
            TaskManifest {
                output_files: vec![
                    file("bin/mojo", BIG_1, true),
                    file("bin/lld", BIG_2, true),
                    file("lib/a.o", A_O, true),
                ],
                packs: 3,
                large: vec![large(0, BIG_1), large(2, BIG_2)],
                ..base()
            },
        ),
    ];
    for (name, expected) in cases {
        let parsed = parse_v2(name).expect(name);
        assert_eq!(parsed, expected, "{name}");
        assert_eq!(parsed.version(), MANIFEST_VERSION_2, "{name}");
        assert_eq!(
            expected.to_json_with(LARGE_MAX).expect(name),
            fixture(name),
            "{name}"
        );
    }
    let one = parse_v2("valid_v2/one_large.json").expect("valid");
    assert_eq!(one.large[0].size(), 138_847_232);
}

/// Asserts that a version 1 manifest still reads as before: one pack, no large object, version
/// 1, under any cap; and that a manifest of one pack and no large object is written as version
/// 1, so a small run's bytes are what they were.
#[test]
fn a_v1_manifest_is_one_pack_and_is_written_as_v1() {
    for cap in [0, LARGE_MAX] {
        let parsed = TaskManifest::parse_with(&fixture("valid/minimal.json"), cap).expect("valid");
        assert_eq!((parsed.packs, parsed.large.len()), (1, 0));
        assert_eq!(parsed.version(), MANIFEST_VERSION);
    }
    assert_eq!(
        base().to_json().expect("json"),
        fixture("valid/minimal.json")
    );
}

/// Asserts that a large object is refused by a reader given no cap (what `parse` is), even when
/// the manifest is right under the farm's cap: a reader opts in to large objects.
#[test]
fn parse_without_a_cap_refuses_a_large_object() {
    assert!(matches!(
        TaskManifest::parse(&fixture("valid_v2/one_large.json")),
        Err(ManifestError::LargeTooBig {
            slot: 0,
            size: 138_847_232,
            cap: 0
        })
    ));
}

/// Asserts that `packs` is 1 to 8: 0 and 9 are refused, 1 and 8 are not.
#[test]
fn packs_is_one_to_eight() {
    assert!(matches!(
        refusal_v2("invalid_v2/packs_zero.json"),
        ManifestError::Packs(0)
    ));
    assert!(matches!(
        refusal_v2("invalid_v2/packs_nine.json"),
        ManifestError::Packs(9)
    ));
    for packs in [1, MAX_PACKS] {
        let manifest = TaskManifest { packs, ..base() };
        assert!(manifest.validate_with(LARGE_MAX).is_ok(), "{packs}");
    }
    for packs in [0, MAX_PACKS + 1] {
        let manifest = TaskManifest { packs, ..base() };
        assert!(matches!(manifest.to_json(), Err(ManifestError::Packs(p)) if p == packs));
    }
}

/// Asserts that a large slot must be below 8: slot 8 is refused, slot 7 is not.
#[test]
fn a_large_slot_is_below_eight() {
    assert!(matches!(
        refusal_v2("invalid_v2/large_slot_eight.json"),
        ManifestError::LargeSlot(8)
    ));
    let named = |slot| TaskManifest {
        output_files: vec![file("bin/x", BIG_1, false)],
        large: vec![large(slot, BIG_1)],
        ..base()
    };
    assert!(named(MAX_LARGE - 1).validate_with(LARGE_MAX).is_ok());
    assert!(matches!(
        named(MAX_LARGE).validate_with(LARGE_MAX),
        Err(ManifestError::LargeSlot(8))
    ));
}

/// Asserts that two large entries with one slot, and two with one digest, are refused: every
/// digest is in exactly one place.
#[test]
fn a_large_slot_or_digest_listed_twice_is_refused() {
    assert!(matches!(
        refusal_v2("invalid_v2/duplicate_large_slot.json"),
        ManifestError::DuplicateLargeSlot(1)
    ));
    let err = refusal_v2("invalid_v2/duplicate_large_digest.json");
    assert!(
        matches!(err, ManifestError::DuplicateLargeDigest(d) if d == digest(BIG_1)),
        "{err}"
    );
}

/// Asserts the size window of a large object: at the record cap it is a pack's business (refused),
/// one byte over is large; at the large cap it is allowed, one byte over is refused; the cap
/// is the one given, not a constant.
#[test]
fn a_large_size_is_over_the_record_cap_and_at_most_the_given_cap() {
    assert!(matches!(
        refusal_v2("invalid_v2/large_size_at_record_cap.json"),
        ManifestError::LargeTooSmall {
            slot: 0,
            size: MAX_RECORD_BYTES
        }
    ));
    assert!(matches!(
        refusal_v2("invalid_v2/large_over_cap.json"),
        ManifestError::LargeTooBig {
            slot: 0,
            size: 536_870_913,
            cap: LARGE_MAX
        }
    ));
    let sized = |size: u64| {
        let text = format!("{}/{size}", "3".repeat(64));
        TaskManifest {
            output_files: vec![file("bin/x", &text, false)],
            large: vec![large(0, &text)],
            ..base()
        }
    };
    assert!(sized(MAX_RECORD_BYTES + 1).validate_with(LARGE_MAX).is_ok());
    assert!(sized(LARGE_MAX).validate_with(LARGE_MAX).is_ok());
    assert!(matches!(
        sized(LARGE_MAX).validate_with(LARGE_MAX - 1),
        Err(ManifestError::LargeTooBig { .. })
    ));
    assert!(sized(LARGE_MAX + 1).validate_with(LARGE_MAX + 1).is_ok());
}

/// Asserts that a large entry no output names is refused: it would be streamed into the store
/// for nothing.
#[test]
fn a_large_object_no_output_names_is_refused() {
    let err = refusal_v2("invalid_v2/large_unnamed.json");
    assert!(
        matches!(err, ManifestError::LargeUnnamed { slot: 0, digest: d } if d == digest(BIG_1)),
        "{err}"
    );
}

/// Asserts the fields of each version: an unknown field (top level or in a large entry) is
/// refused, a slot that is not a number is a JSON error, version 1 may not carry `packs` or
/// `large`, and version 2 must carry both.
#[test]
fn the_fields_of_each_version_are_exact() {
    for name in [
        "invalid_v2/unknown_field.json",
        "invalid_v2/unknown_large_field.json",
    ] {
        let ManifestError::Json(e) = refusal_v2(name) else {
            panic!("{name}: not a Json refusal");
        };
        assert!(e.to_string().contains("unknown field"), "{name}: {e}");
    }
    assert!(matches!(
        refusal_v2("invalid_v2/large_slot_not_a_number.json"),
        ManifestError::Json(_)
    ));
    assert!(matches!(
        refusal_v2("invalid_v2/v1_with_packs.json"),
        ManifestError::UnexpectedField("packs")
    ));
    assert!(matches!(
        refusal_v2("invalid_v2/v1_with_large.json"),
        ManifestError::UnexpectedField("large")
    ));
    assert!(matches!(
        refusal_v2("invalid_v2/missing_packs.json"),
        ManifestError::MissingField("packs")
    ));
    assert!(matches!(
        refusal_v2("invalid_v2/missing_large.json"),
        ManifestError::MissingField("large")
    ));
}

/// Asserts that a large digest that is not `<hex>/<size>` names its position in the refusal.
#[test]
fn a_bad_large_digest_names_its_position() {
    let text = String::from_utf8(fixture("valid_v2/one_large.json")).expect("utf8");
    let broken = text.replace(
        &format!("\"digest\":\"{BIG_1}\"}}]}}"),
        "\"digest\":\"zz\"}]}",
    );
    assert_ne!(broken, text);
    assert!(matches!(
        TaskManifest::parse_with(broken.as_bytes(), LARGE_MAX),
        Err(ManifestError::Digest { field, .. }) if field == "large[0].digest"
    ));
}

// The advisory `inputs` object.

fn all_inputs() -> InputStats {
    InputStats {
        blobs: 19_312,
        files: 19_000,
        directories: 300,
        requests: 4_100,
        retries: 7,
        max_in_flight: 32,
        bytes_written_scratch: 344_000_000,
        walk_ms: 120,
        fetch_ms: 8_800,
        hash_ms: 900,
        write_ms: 1_500,
        request_p50_ms: 5,
        request_p99_ms: 240,
        bytes_by_source: SourceBytes {
            listener_get: 1,
            listener_batch: 2,
            cache_hit: 3,
            tierstore_direct: 4,
        },
    }
}

/// Asserts that a manifest with every `inputs` field parses to the value written out here, is
/// written back byte for byte, and is version 2 though it names one pack and no large object.
#[test]
fn a_manifest_with_inputs_is_v2_and_writes_back_byte_for_byte() {
    let name = "valid_v2/inputs_all_fields.json";
    let expected = TaskManifest {
        inputs: Some(all_inputs()),
        ..base()
    };
    let parsed = parse_v2(name).expect(name);
    assert_eq!(parsed, expected);
    assert_eq!(parsed.version(), MANIFEST_VERSION_2);
    assert_eq!(expected.to_json().expect(name), fixture(name));
}

/// Asserts that a partial `inputs` object reads with 0 for each missing field, in `inputs` and in
/// `bytes_by_source`, and is written back in full.
#[test]
fn a_partial_inputs_object_reads_missing_fields_as_zero() {
    let parsed = parse_v2("valid_v2/inputs_partial.json").expect("valid");
    let expected = InputStats {
        blobs: 5,
        bytes_by_source: SourceBytes {
            cache_hit: 9,
            ..SourceBytes::default()
        },
        ..InputStats::default()
    };
    assert_eq!(parsed.inputs, Some(expected));
    let json = String::from_utf8(parsed.to_json().expect("json")).expect("utf8");
    assert!(json.contains("\"walk_ms\":0"), "{json}");
    assert!(json.contains("\"tierstore_direct\":0"), "{json}");
}

/// Asserts that a manifest without `inputs` is unchanged: one pack and no large object is still
/// written as version 1 and has no `inputs` key; with `inputs` it is version 2.
#[test]
fn a_manifest_without_inputs_stays_v1() {
    let plain = base();
    assert_eq!(plain.version(), MANIFEST_VERSION);
    let json = String::from_utf8(plain.to_json().expect("json")).expect("utf8");
    assert!(!json.contains("inputs"), "{json}");
    let with = TaskManifest {
        inputs: Some(InputStats::default()),
        ..base()
    };
    assert_eq!(with.version(), MANIFEST_VERSION_2);
}

/// Asserts that an unknown field is refused inside `inputs` and inside `bytes_by_source`.
#[test]
fn an_unknown_field_inside_inputs_is_refused() {
    for name in [
        "invalid_v2/inputs_unknown_field.json",
        "invalid_v2/inputs_unknown_source_field.json",
    ] {
        let ManifestError::Json(e) = refusal_v2(name) else {
            panic!("{name}: not a Json refusal");
        };
        assert!(e.to_string().contains("unknown field"), "{name}: {e}");
    }
}

/// Asserts that an `inputs` number over `i64::MAX` is refused with its field name, in `inputs`
/// and in `bytes_by_source`, and that `i64::MAX` itself is allowed.
#[test]
fn an_inputs_number_beyond_i64_is_refused() {
    let over = i64::MAX as u64 + 1;
    assert!(matches!(
        refusal_v2("invalid_v2/inputs_over_i64.json"),
        ManifestError::Advisory { field: "inputs.retries", value } if value == over
    ));
    assert!(matches!(
        refusal_v2("invalid_v2/inputs_source_over_i64.json"),
        ManifestError::Advisory { field: "inputs.bytes_by_source.cache_hit", value } if value == over
    ));
    let mut at_max = all_inputs();
    at_max.requests = i64::MAX as u64;
    at_max.bytes_by_source.tierstore_direct = i64::MAX as u64;
    let manifest = TaskManifest {
        inputs: Some(at_max),
        ..base()
    };
    assert!(manifest.validate().is_ok());
    let mut past = at_max;
    past.walk_ms = over;
    let manifest = TaskManifest {
        inputs: Some(past),
        ..base()
    };
    assert!(matches!(
        manifest.validate(),
        Err(ManifestError::Advisory {
            field: "inputs.walk_ms",
            ..
        })
    ));
}

/// Asserts that a version 1 body carrying `inputs` is refused: only version 2 has the field.
#[test]
fn a_v1_manifest_with_inputs_is_refused() {
    assert!(matches!(
        refusal_v2("invalid_v2/v1_with_inputs.json"),
        ManifestError::UnexpectedField("inputs")
    ));
}

/// Asserts that a version 2 body that needs nothing of version 2 (one pack, no large object, no
/// inputs) is refused, so every manifest has one spelling; and that each thing that does need
/// version 2 is accepted in a version 2 body.
#[test]
fn a_v2_manifest_that_needs_nothing_of_v2_is_refused() {
    assert!(matches!(
        refusal_v2("invalid_v2/v2_needing_nothing.json"),
        ManifestError::NotV2
    ));
    for name in [
        "valid_v2/two_packs.json",
        "valid_v2/one_large.json",
        "valid_v2/inputs_partial.json",
    ] {
        assert!(parse_v2(name).is_ok(), "{name}");
    }
}

/// Asserts that the plain forms are the single-pack path: `parse`, `validate` and `to_json`
/// refuse a manifest of 2 to 8 packs (`MultiPack`), as they refuse a large entry, so a consumer
/// of them cannot take pack 0 of a multi-pack run as the whole run; the `_with` forms accept
/// the same file and manifest; and `inputs`, being advisory, is accepted by both.
#[test]
fn the_plain_forms_refuse_a_multi_pack_manifest_and_the_with_forms_take_it() {
    let two = fixture("valid_v2/two_packs.json");
    assert!(matches!(
        TaskManifest::parse(&two),
        Err(ManifestError::MultiPack(2))
    ));
    assert!(TaskManifest::parse_with(&two, 0).is_ok());
    assert!(TaskManifest::parse_with(&two, LARGE_MAX).is_ok());
    for packs in 2..=MAX_PACKS {
        let manifest = TaskManifest { packs, ..base() };
        assert!(matches!(manifest.validate(), Err(ManifestError::MultiPack(p)) if p == packs));
        assert!(matches!(manifest.to_json(), Err(ManifestError::MultiPack(p)) if p == packs));
        assert!(manifest.validate_with(0).is_ok());
        assert!(manifest.to_json_with(0).is_ok());
    }
    let with_inputs = fixture("valid_v2/inputs_partial.json");
    assert!(TaskManifest::parse(&with_inputs).is_ok());
    assert!(TaskManifest::parse_with(&with_inputs, 0).is_ok());
}

/// Asserts, for every number `InputStats` carries (the thirteen of `inputs` and the four of
/// `bytes_by_source`), that setting just that field to `i64::MAX + 1` is refused as
/// `ManifestError::Advisory` naming that field and the value. Catches a number left out of the
/// check.
#[test]
fn every_inputs_number_beyond_i64_is_refused_by_name() {
    type Set = fn(&mut InputStats, u64);
    let table: [(&str, Set); 17] = [
        ("inputs.blobs", |s, v| s.blobs = v),
        ("inputs.files", |s, v| s.files = v),
        ("inputs.directories", |s, v| s.directories = v),
        ("inputs.requests", |s, v| s.requests = v),
        ("inputs.retries", |s, v| s.retries = v),
        ("inputs.max_in_flight", |s, v| s.max_in_flight = v),
        ("inputs.bytes_written_scratch", |s, v| {
            s.bytes_written_scratch = v;
        }),
        ("inputs.walk_ms", |s, v| s.walk_ms = v),
        ("inputs.fetch_ms", |s, v| s.fetch_ms = v),
        ("inputs.hash_ms", |s, v| s.hash_ms = v),
        ("inputs.write_ms", |s, v| s.write_ms = v),
        ("inputs.request_p50_ms", |s, v| s.request_p50_ms = v),
        ("inputs.request_p99_ms", |s, v| s.request_p99_ms = v),
        ("inputs.bytes_by_source.listener_get", |s, v| {
            s.bytes_by_source.listener_get = v;
        }),
        ("inputs.bytes_by_source.listener_batch", |s, v| {
            s.bytes_by_source.listener_batch = v;
        }),
        ("inputs.bytes_by_source.cache_hit", |s, v| {
            s.bytes_by_source.cache_hit = v;
        }),
        ("inputs.bytes_by_source.tierstore_direct", |s, v| {
            s.bytes_by_source.tierstore_direct = v;
        }),
    ];
    let over = i64::MAX as u64 + 1;
    for (name, set) in table {
        let mut inputs = InputStats::default();
        set(&mut inputs, over);
        let manifest = TaskManifest {
            inputs: Some(inputs),
            ..base()
        };
        let got = manifest.validate();
        assert!(
            matches!(&got, Err(ManifestError::Advisory { field, value }) if *field == name && *value == over),
            "{name}: {got:?}"
        );
    }
}
