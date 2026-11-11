//! A manifest a later release writes: members this reader does not know are skipped wherever
//! they stand, and `inputs.from`, the member the tree job adds, is read with each value it can
//! take, a value this reader does not know included.

use kbf_task_pack::{InputStats, InputsFrom, ManifestError, TaskManifest};

use crate::task_manifest::{
    LARGE_MAX, all_inputs, base, file, fixture, golden, parse_fixture, parse_v2,
};

/// Asserts that a manifest with an unknown member in every object it has (the top level, a
/// file, a directory, a symlink, `timing` and `usage`), the values a number, a string, `null`,
/// a list and nested objects (one holding the names `version` and `objects`), reads as the
/// golden manifest and writes back as the golden bytes: each member is skipped as a whole and
/// changes nothing.
#[test]
fn an_unknown_member_is_skipped() {
    let name = "valid/unknown_members_everywhere.json";
    let parsed = parse_fixture(name).expect(name);
    assert_eq!(parsed, golden());
    assert_eq!(
        parsed.to_json().expect(name),
        fixture("task_pack_v1.manifest.json")
    );
    assert_eq!(
        TaskManifest::parse_with(&fixture(name), LARGE_MAX).expect(name),
        golden()
    );
}

/// Asserts that a version 3 manifest with unknown members (the top level, a file, `inputs` and
/// `bytes_by_source`) and a `from` this reader does not know reads by the objects reader, the
/// value read as `InputsFrom::Unknown` and the rest as written.
#[test]
fn a_v3_manifest_with_unknown_members_and_an_unknown_from_is_read() {
    let parsed =
        TaskManifest::parse_objects(&fixture("valid_v3/unknown_members.json")).expect("read");
    let a_o = parsed.output_files[0].digest;
    let mut inputs = all_inputs();
    inputs.from = Some(InputsFrom::Unknown);
    assert_eq!(
        parsed,
        TaskManifest {
            packs: 0,
            output_files: vec![file("out/a.o", &a_o.to_string(), false)],
            inputs: Some(inputs),
            objects: Some(vec![a_o]),
            ..base()
        }
    );
}

/// Asserts that each `from` value is read: `tree` and `fetch` as themselves, another string as
/// `Unknown` (so the manifest is not refused for it), and no `from` as `None`; and that `tree`
/// and `fetch` write back byte for byte.
#[test]
fn each_from_value_is_read() {
    let cases = [
        ("valid_v2/inputs_from_tree.json", Some(InputsFrom::Tree)),
        ("valid_v2/inputs_from_fetch.json", Some(InputsFrom::Fetch)),
        (
            "valid_v2/inputs_from_unknown_value.json",
            Some(InputsFrom::Unknown),
        ),
        ("valid_v2/inputs_all_fields.json", None),
    ];
    for (name, from) in cases {
        let parsed = TaskManifest::parse(&fixture(name)).expect(name);
        let mut want = all_inputs();
        want.from = from;
        assert_eq!(parsed.inputs, Some(want), "{name}");
        if from != Some(InputsFrom::Unknown) {
            assert_eq!(parsed.to_json().expect(name), fixture(name), "{name}");
        }
    }
}

/// Asserts that a `from` this reader does not know is never written: the writer refuses it
/// rather than write a value it would read differently.
#[test]
fn an_unknown_from_is_not_written() {
    let manifest = TaskManifest {
        inputs: Some(InputStats {
            from: Some(InputsFrom::Unknown),
            ..InputStats::default()
        }),
        ..base()
    };
    assert!(matches!(manifest.to_json(), Err(ManifestError::Json(_))));
}

/// Asserts that a manifest without `from` is written as before: `inputs` has no `from` key, so a
/// reader of the releases before this one, which refuses a member it does not know, reads what
/// this release writes; and the version 1 bytes of every golden fixture are unchanged.
#[test]
fn what_this_release_writes_without_from_is_what_the_last_release_wrote() {
    let with_inputs = TaskManifest {
        inputs: Some(all_inputs()),
        ..base()
    };
    let json = with_inputs.to_json().expect("json");
    assert_eq!(json, fixture("valid_v2/inputs_all_fields.json"));
    assert!(!String::from_utf8(json).expect("utf8").contains("from"));
    assert_eq!(
        golden().to_json().expect("json"),
        fixture("task_pack_v1.manifest.json")
    );
}

/// Asserts that a member this reader knows is still checked where its version does not have it,
/// even though unknown members are skipped: `objects` in versions 1 and 2, `inputs` in version
/// 1. And that a known member of the wrong type is still refused.
#[test]
fn a_known_member_out_of_its_version_is_still_refused() {
    for (name, version, field) in [
        ("invalid_v3/v1_with_objects.json", 1, "objects"),
        ("invalid_v3/v2_with_objects.json", 2, "objects"),
        ("invalid_v2/v1_with_inputs.json", 1, "inputs"),
    ] {
        let err = TaskManifest::parse_with(&fixture(name), LARGE_MAX).expect_err(name);
        assert!(
            matches!(err, ManifestError::UnexpectedField { version: v, field: f } if v == version && f == field),
            "{name}: {err}"
        );
    }
    let text = String::from_utf8(fixture("valid_v2/inputs_from_tree.json")).expect("utf8");
    let mistyped = text.replace("\"from\":\"tree\"", "\"from\":7");
    assert_ne!(mistyped, text);
    assert!(matches!(
        TaskManifest::parse(mistyped.as_bytes()),
        Err(ManifestError::Json(_))
    ));
}

/// Asserts that a field the contract does not name is skipped, in the timing (`put_ms`) and at
/// the top level (a list, `entries`): each manifest reads as the one without it and writes back
/// without it.
#[test]
fn a_manifest_with_an_unknown_field_reads_as_the_one_without_it() {
    for name in [
        "valid/unknown_field_put_ms.json",
        "valid/unknown_top_level_field.json",
    ] {
        let parsed = parse_fixture(name).expect(name);
        assert_eq!(parsed, base(), "{name}");
        assert_eq!(
            parsed.to_json().expect(name),
            fixture("valid/minimal.json"),
            "{name}"
        );
    }
}

/// Asserts that an unknown field inside `inputs` and inside `bytes_by_source` is skipped: each
/// manifest reads as the one without it and writes back as `inputs_all_fields.json`.
#[test]
fn an_unknown_field_inside_inputs_is_skipped() {
    for name in [
        "valid_v2/inputs_unknown_field.json",
        "valid_v2/inputs_unknown_source_field.json",
    ] {
        let parsed = parse_v2(name).expect(name);
        assert_eq!(parsed.inputs, Some(all_inputs()), "{name}");
        assert_eq!(
            parsed.to_json().expect(name),
            fixture("valid_v2/inputs_all_fields.json"),
            "{name}"
        );
    }
}
