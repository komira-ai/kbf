//! Version 3 of the manifest: a run that wrote no pack, its blobs stored as objects under their
//! digests and listed in `objects`. The golden fixtures under `tests/fixtures/task_pack/valid_v3`
//! and one test per refusal.

use kbf_segments::sha256;
use kbf_task_pack::{
    EMPTY_DIGEST, InputsFrom, LargeEntry, MANIFEST_VERSION, MANIFEST_VERSION_2, MANIFEST_VERSION_3,
    MAX_RECORD_BYTES, ManifestError, TaskManifest,
};
use kbf_types::{Digest, DigestFunction, ParseDigestError};

use crate::task_manifest::{
    A_O, BIG_1, LARGE_MAX, STDERR, STDOUT, TOOL, TREE, all_inputs, base, digest, file, fixture,
    golden,
};

/// The blob inside the golden `Tree` (`out/d/x`): stored as an object though no output names it.
fn inside_the_tree() -> Digest {
    sha256(b"x inside the directory\n")
}

/// The golden manifest as a run of objects: every output kind, the blob inside the tree, and
/// `inputs` saying the root was a tree.
fn golden_v3() -> TaskManifest {
    let mut objects: Vec<Digest> = [STDOUT, STDERR, A_O, TOOL, TREE].map(digest).to_vec();
    objects.push(inside_the_tree());
    let mut inputs = all_inputs();
    inputs.from = Some(InputsFrom::Tree);
    TaskManifest {
        packs: 0,
        inputs: Some(inputs),
        objects: Some(objects),
        ..golden()
    }
}

fn refusal_v3(name: &str) -> ManifestError {
    TaskManifest::parse_objects(&fixture(name)).expect_err(name)
}

/// Asserts that each version 3 fixture reads to the value written out here (the golden one with
/// every output kind and an object no output names; one with no output and no object), that it
/// is version 3 with no pack, and that writing that value gives the fixture's bytes back.
#[test]
fn the_golden_v3_manifests_parse_to_their_values_and_write_back_byte_for_byte() {
    let cases = [
        ("valid_v3/golden.json", golden_v3()),
        (
            "valid_v3/no_outputs.json",
            TaskManifest {
                packs: 0,
                objects: Some(Vec::new()),
                ..base()
            },
        ),
    ];
    for (name, expected) in cases {
        let parsed = TaskManifest::parse_objects(&fixture(name)).expect(name);
        assert_eq!(parsed, expected, "{name}");
        assert_eq!(parsed.version(), MANIFEST_VERSION_3, "{name}");
        assert_eq!(parsed.packs, 0, "{name}");
        assert_eq!(
            expected.objects_json().expect(name),
            fixture(name),
            "{name}"
        );
    }
}

/// Asserts that every reader and writer of packs refuses a manifest of objects by name, so no
/// consumer of packs takes one for pack 0 of a run: `parse`, `parse_with` (under any cap),
/// `validate`, `validate_with`, `to_json` and `to_json_with`.
#[test]
fn the_pack_forms_refuse_a_manifest_of_objects() {
    let bytes = fixture("valid_v3/golden.json");
    assert!(matches!(
        TaskManifest::parse(&bytes),
        Err(ManifestError::ByDigest)
    ));
    for cap in [0, LARGE_MAX] {
        assert!(matches!(
            TaskManifest::parse_with(&bytes, cap),
            Err(ManifestError::ByDigest)
        ));
        assert!(matches!(
            golden_v3().validate_with(cap),
            Err(ManifestError::ByDigest)
        ));
        assert!(matches!(
            golden_v3().to_json_with(cap),
            Err(ManifestError::ByDigest)
        ));
    }
    assert!(matches!(
        golden_v3().validate(),
        Err(ManifestError::ByDigest)
    ));
    assert!(matches!(
        golden_v3().to_json(),
        Err(ManifestError::ByDigest)
    ));
}

/// Asserts that the reader and writer of objects refuse a manifest of packs, naming its
/// version: version 1 and 2 files, and the typed manifests.
#[test]
fn the_objects_forms_refuse_a_manifest_of_packs() {
    for (name, version) in [
        ("valid/minimal.json", MANIFEST_VERSION),
        ("task_pack_v1.manifest.json", MANIFEST_VERSION),
        ("valid_v2/two_packs.json", MANIFEST_VERSION_2),
        ("valid_v2/inputs_all_fields.json", MANIFEST_VERSION_2),
    ] {
        let err = refusal_v3(name);
        assert!(
            matches!(err, ManifestError::NotByDigest(v) if v == version),
            "{name}: {err}"
        );
    }
    assert!(matches!(
        base().validate_objects(),
        Err(ManifestError::NotByDigest(1))
    ));
    let two = TaskManifest { packs: 2, ..base() };
    assert!(matches!(
        two.objects_json(),
        Err(ManifestError::NotByDigest(2))
    ));
}

/// Asserts that a version 3 manifest needs `objects` and carries neither `packs` nor `large`, and
/// that versions 1 and 2 may not carry `objects`, each refusal naming the version and the field.
#[test]
fn the_fields_of_version_3_are_exact() {
    let cases = [
        ("invalid_v3/missing_objects.json", 3, "objects", true),
        ("invalid_v3/with_packs.json", 3, "packs", false),
        ("invalid_v3/with_large.json", 3, "large", false),
        ("invalid_v3/v1_with_objects.json", 1, "objects", false),
        ("invalid_v3/v2_with_objects.json", 2, "objects", false),
    ];
    for (name, version, field, missing) in cases {
        let err = refusal_v3(name);
        let matched = if missing {
            matches!(err, ManifestError::MissingField { version: v, field: f } if v == version && f == field)
        } else {
            matches!(err, ManifestError::UnexpectedField { version: v, field: f } if v == version && f == field)
        };
        assert!(matched, "{name}: {err}");
        // The pack readers refuse the same file for the same reason.
        let again = TaskManifest::parse_with(&fixture(name), LARGE_MAX).expect_err(name);
        assert_eq!(again.to_string(), err.to_string(), "{name}");
    }
    assert!(matches!(
        refusal_v3("invalid_v3/objects_not_a_list.json"),
        ManifestError::Json(_)
    ));
}

/// Asserts that an object listed twice and the empty blob listed as an object are refused, by
/// the reader and by the writer, naming the digest and the position.
#[test]
fn an_object_listed_twice_or_the_empty_object_is_refused() {
    let err = refusal_v3("invalid_v3/object_listed_twice.json");
    assert!(
        matches!(err, ManifestError::DuplicateObject(d) if d == digest(A_O)),
        "{err}"
    );
    assert!(matches!(
        refusal_v3("invalid_v3/empty_object.json"),
        ManifestError::EmptyObject(2)
    ));
    let mut twice = golden_v3();
    twice
        .objects
        .get_or_insert_with(Vec::new)
        .push(digest(TOOL));
    assert!(matches!(
        twice.objects_json(),
        Err(ManifestError::DuplicateObject(d)) if d == digest(TOOL)
    ));
    let mut empty = golden_v3();
    empty.objects = Some(vec![EMPTY_DIGEST]);
    assert!(matches!(
        empty.validate_objects(),
        Err(ManifestError::EmptyObject(0))
    ));
}

/// Asserts that every kind of output that names a blob must have it listed: stdout, a file and a
/// tree in the fixtures, stderr here, each refusal naming the field and the digest; and that an
/// empty output file needs no object.
#[test]
fn an_output_whose_blob_is_not_listed_is_refused() {
    let cases = [
        ("invalid_v3/stdout_not_listed.json", "stdout", STDOUT),
        (
            "invalid_v3/output_not_listed.json",
            "output_files[1].digest",
            TOOL,
        ),
        (
            "invalid_v3/tree_not_listed.json",
            "output_directories[0].tree",
            TREE,
        ),
    ];
    for (name, want_field, want) in cases {
        let err = refusal_v3(name);
        assert!(
            matches!(&err, ManifestError::ObjectMissing { field, digest: d } if field == want_field && *d == digest(want)),
            "{name}: {err}"
        );
    }
    let mut no_stderr = golden_v3();
    no_stderr.objects = Some([STDOUT, A_O, TOOL, TREE].map(digest).to_vec());
    let err = no_stderr.objects_json().expect_err("stderr not listed");
    assert!(
        matches!(&err, ManifestError::ObjectMissing { field, digest: d } if field == "stderr" && *d == digest(STDERR)),
        "{err}"
    );
    // The golden manifest has an empty output file and no object for it.
    assert!(
        golden()
            .output_files
            .iter()
            .any(|f| f.digest == EMPTY_DIGEST)
    );
    assert!(golden_v3().validate_objects().is_ok());
}

/// Asserts that an object that is not `<64 lowercase hex>/<size>` names its position, and that
/// a size-0 object that is not the empty blob is refused as such.
#[test]
fn a_bad_object_digest_names_its_position() {
    match refusal_v3("invalid_v3/object_bad_digest.json") {
        ManifestError::Digest { field, source } => {
            assert_eq!(field, "objects[1]");
            assert!(matches!(source, ParseDigestError::MissingSeparator(_)));
        }
        other => panic!("{other}"),
    }
    match refusal_v3("invalid_v3/object_zero_size_not_empty.json") {
        ManifestError::ZeroSizeNotEmpty { field, digest } => {
            assert_eq!(field, "objects[0]");
            assert_eq!(digest, Digest::new(DigestFunction::Sha256, [0x77; 32], 0));
        }
        other => panic!("{other}"),
    }
}

/// Asserts that an object has no size cap (no blob size cap on this path): an object far over
/// the record cap and the large cap is listed and written.
#[test]
fn an_object_has_no_size_cap() {
    let huge = Digest::new(DigestFunction::Sha256, [0x11; 32], u64::MAX);
    let manifest = TaskManifest {
        packs: 0,
        output_files: vec![file("bin/huge", &huge.to_string(), true)],
        objects: Some(vec![huge, digest(BIG_1)]),
        ..base()
    };
    assert!(huge.size_bytes > MAX_RECORD_BYTES.max(LARGE_MAX));
    let json = manifest.objects_json().expect("written");
    assert_eq!(TaskManifest::parse_objects(&json).expect("read"), manifest);
}

/// Asserts that a typed manifest with `objects` and a pack count or a large object is refused
/// by the writer: a run of objects wrote no pack.
#[test]
fn a_manifest_of_objects_with_packs_or_large_objects_is_refused() {
    let with_packs = TaskManifest {
        packs: 1,
        ..golden_v3()
    };
    assert!(matches!(
        with_packs.objects_json(),
        Err(ManifestError::ByDigest)
    ));
    let mut with_large = golden_v3();
    with_large.output_files.push(file("bin/mojo", BIG_1, true));
    with_large
        .objects
        .get_or_insert_with(Vec::new)
        .push(digest(BIG_1));
    with_large.large = vec![LargeEntry {
        slot: 0,
        digest: digest(BIG_1),
    }];
    assert!(matches!(
        with_large.validate_objects(),
        Err(ManifestError::ByDigest)
    ));
}

/// Asserts that the rules every version shares hold in version 3 too: a bad path, a bad lease
/// and an advisory number over `i64::MAX` are refused by the objects reader and writer.
#[test]
fn the_shared_rules_hold_in_version_3() {
    let mut bad_path = golden_v3();
    bad_path.output_files[0].path = "../a.o".to_owned();
    assert!(matches!(
        bad_path.objects_json(),
        Err(ManifestError::Path { .. })
    ));
    let mut bad_lease = golden_v3();
    bad_lease.lease = "a/b".to_owned();
    assert!(matches!(
        bad_lease.validate_objects(),
        Err(ManifestError::Lease(_))
    ));
    let mut over = golden_v3();
    over.timing.run_ms = i64::MAX as u64 + 1;
    let text = String::from_utf8(golden_v3().objects_json().expect("json")).expect("utf8");
    let past = text.replace("\"run_ms\":3456", "\"run_ms\":9223372036854775808");
    assert_ne!(past, text);
    for got in [
        over.validate_objects(),
        TaskManifest::parse_objects(past.as_bytes()).map(|_| ()),
    ] {
        assert!(matches!(
            got,
            Err(ManifestError::Advisory {
                field: "timing.run_ms",
                ..
            })
        ));
    }
}
