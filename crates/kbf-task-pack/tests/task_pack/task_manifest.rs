//! The manifest of a task pack: the golden fixtures under `tests/fixtures/task_pack`, which the
//! frontend's ingest and the task tool test against too, and one test per refusal.

use std::path::Path;

use kbf_segments::sha256;
use kbf_task_pack::{
    EMPTY_DIGEST, MAX_MANIFEST_BYTES, ManifestError, OutputDirectory, OutputFile, OutputSymlink,
    PathError, TaskKeyError, TaskManifest, Timing, Usage,
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

/// Asserts that a version other than 1 is refused with the version it names.
#[test]
fn a_manifest_of_another_version_is_refused() {
    assert!(matches!(
        refusal("invalid/version_2.json"),
        ManifestError::Version(2)
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
