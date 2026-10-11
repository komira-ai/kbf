//! Tests of the `devicectl` fixture lint. Each says which defect it catches. The planted
//! identifiers are made up; this file is outside the fixture directory, so the lint
//! does not read it.

use super::*;

fn kinds(path: &str, text: &str) -> Vec<Kind> {
    scan(path, text).into_iter().map(|f| f.kind).collect()
}

/// A `devicectl list devices --json-output` document holding one device, with
/// `udid`, `ecid`, `serial`, `identifier` and `name` as given.
fn list_devices(udid: &str, ecid: &str, serial: &str, identifier: &str, name: &str) -> String {
    format!(
        r#"{{
  "info": {{ "jsonVersion": 2, "outcome": "success" }},
  "result": {{
    "devices": [
      {{
        "deviceProperties": {{ "name": "{name}", "osVersionNumber": "18.0" }},
        "hardwareProperties": {{
          "ecid": {ecid},
          "marketingName": "iPhone 16",
          "serialNumber": "{serial}",
          "udid": "{udid}"
        }},
        "identifier": "{identifier}"
      }}
    ]
  }}
}}
"#
    )
}

#[test]
fn a_udid_is_refused_in_upper_lower_and_mixed_case() {
    // Catches: a UDID matcher that knows only the uppercase form devicectl prints, so
    // a lowercased or hand-edited UDID gets into a fixture.
    for udid in [
        "00008110-001A2D3E0C38802E",
        "00008110-001a2d3e0c38802e",
        "00008110-001a2D3e0C38802E",
    ] {
        let line = format!("Device {udid} connected");
        assert_eq!(
            kinds("help.txt", &line),
            vec![Kind::Udid(udid.to_owned())],
            "{udid}"
        );
    }
}

#[test]
fn a_legacy_forty_hex_udid_is_refused() {
    // Catches: a matcher that knows only the 8-16 form, missing the 40-hex UDID of
    // older devices.
    let udid = "3f6a1c2b9e8d7f6a5b4c3d2e1f0a9b8c7d6e5f4a";
    assert_eq!(
        kinds("a.txt", &format!("udid {udid}")),
        vec![Kind::Udid(udid.to_owned())]
    );
}

#[test]
fn a_coredevice_id_is_refused_in_either_case() {
    // Catches: a CoreDevice id (a UUID) let through, or matched in one case only.
    for id in [
        "4C1D2E3F-5A6B-7C8D-9E0F-1A2B3C4D5E6F",
        "4c1d2e3f-5a6b-7c8d-9e0f-1a2b3c4d5e6f",
    ] {
        assert_eq!(
            kinds("a.txt", &format!("identifier: {id}.")),
            vec![Kind::CoreDeviceId(id.to_owned())],
            "{id}"
        );
    }
}

#[test]
fn an_ecid_is_refused_in_text_and_as_a_json_number() {
    // Catches: an ECID let through because it is a bare number, in decimal or hex,
    // after a label in text or as a JSON value.
    assert_eq!(
        kinds("a.txt", "ECID: 0x1A2B3C4D5E6F"),
        vec![Kind::Ecid("0x1A2B3C4D5E6F".to_owned())]
    );
    assert_eq!(
        kinds("a.txt", "ecid=28833183478120494"),
        vec![Kind::Ecid("28833183478120494".to_owned())]
    );
    let found = kinds("a.json", r#"{"ecid": 28833183478120494}"#);
    assert_eq!(
        found,
        vec![
            Kind::Ecid("28833183478120494".to_owned()),
            Kind::Ecid("28833183478120494".to_owned())
        ],
        "the text rule and the JSON walk each report it"
    );
}

#[test]
fn a_serial_number_is_refused_in_text_and_json() {
    // Catches: a serial number let through after `Serial Number:` or as the value of
    // a `serialNumber` key.
    assert_eq!(
        kinds("a.txt", "Serial Number: C02XK1ZZJGH5"),
        vec![Kind::Serial("C02XK1ZZJGH5".to_owned())]
    );
    let found = kinds("a.json", r#"{"serialNumber": "F2LXQ7ABCD12"}"#);
    assert!(
        found
            .iter()
            .all(|k| *k == Kind::Serial("F2LXQ7ABCD12".to_owned()))
            && found.len() == 2,
        "{found:?}"
    );
}

#[test]
fn a_possessive_device_name_is_refused_with_either_apostrophe() {
    // Catches: a device name such as "<owner>'s iPhone" let through, or matched only
    // with a straight apostrophe (Apple's default names use the curly one).
    assert_eq!(
        kinds("a.txt", "Connected to Lab's iPhone"),
        vec![Kind::DeviceName("Lab's iPhone".to_owned())]
    );
    assert_eq!(
        kinds("a.txt", "name: Lab\u{2019}s Apple Watch"),
        vec![Kind::DeviceName("Lab\u{2019}s Apple Watch".to_owned())]
    );
}

#[test]
fn a_device_name_nested_in_json_is_refused() {
    // Catches: a JSON walk that checks only the top-level keys, so the device name
    // nested at result.devices[0].deviceProperties.name (where devicectl puts it, and
    // where no text rule can tell it is a name) gets through.
    let doc = list_devices(
        "00000000-0000000000000000",
        "0",
        "XXXXXXXXXXXX",
        "00000000-0000-0000-0000-000000000000",
        "Lab phone 3",
    );
    let found = scan("list-devices.json", &doc);
    assert_eq!(
        found,
        vec![Finding {
            place: Place::Json("result.devices[0].deviceProperties.name".to_owned()),
            kind: Kind::DeviceName("Lab phone 3".to_owned()),
        }]
    );
}

#[test]
fn every_identifier_nested_in_a_device_list_is_refused() {
    // Catches: a lint that misses any of the five identifiers where devicectl nests
    // them in `list devices --json-output`.
    let doc = list_devices(
        "00008110-001A2D3E0C38802E",
        "28833183478120494",
        "F2LXQ7ABCD12",
        "4C1D2E3F-5A6B-7C8D-9E0F-1A2B3C4D5E6F",
        "Lab\u{2019}s iPhone",
    );
    let found = kinds("list-devices.json", &doc);
    for want in [
        Kind::Udid("00008110-001A2D3E0C38802E".to_owned()),
        Kind::Ecid("28833183478120494".to_owned()),
        Kind::Serial("F2LXQ7ABCD12".to_owned()),
        Kind::CoreDeviceId("4C1D2E3F-5A6B-7C8D-9E0F-1A2B3C4D5E6F".to_owned()),
        Kind::DeviceName("Lab\u{2019}s iPhone".to_owned()),
    ] {
        assert!(found.contains(&want), "{want:?} not in {found:?}");
    }
}

#[test]
fn scrubbed_placeholders_pass() {
    // Catches: a lint that refuses the placeholders a scrubbed fixture must use, so
    // no device fixture could ever be committed.
    let doc = list_devices(
        "00000000-0000000000000000",
        "0",
        "XXXXXXXXXXXX",
        "00000000-0000-0000-0000-000000000000",
        NAME_PLACEHOLDER,
    );
    assert_eq!(scan("list-devices.json", &doc), vec![]);
    let text = "UDID 00000000-0000000000000000 ECID: 0x0 Serial Number: XXXXXXXXXX\n\
                udid 0000000000000000000000000000000000000000";
    assert_eq!(scan("a.txt", text), vec![]);
    let empty = r#"{"udid": "", "serialNumber": null, "deviceName": "Device Name"}"#;
    assert_eq!(scan("a.json", empty), vec![]);
}

#[test]
fn help_text_that_names_the_identifiers_passes() {
    // Catches: false positives on devicectl's own help, which names the identifier
    // kinds and shows options, so the P-D0 fixtures could not be committed.
    let text = "\
  -d, --device <uuid|ecid|serial_number|udid|name|dns_name>
        The identifier, ECID, serial number, UDID, user-provided name, or DNS name of the device.
  --json-output <path>   Write JSON to the file at <path>.
Device's name and the iPhone's state are shown with --verbose.
SERIAL NUMBER and ECID columns are listed when known.
build 16F6, version 477.29, devicectl-477.29";
    assert_eq!(scan("help.txt", text), vec![]);
}

#[test]
fn a_json_fixture_that_does_not_parse_is_refused() {
    // Catches: a broken .json fixture skipped by the walk, which would hide a nested
    // device name from it.
    let found = kinds("list-devices.json", r#"{"deviceName": "Lab phone""#);
    assert!(matches!(found.as_slice(), [Kind::Unparsed(_)]), "{found:?}");
    assert_eq!(kinds("notes.txt", r#"{"deviceName": "Lab phone""#), vec![]);
}

#[test]
fn findings_name_their_line_or_json_path() {
    // Catches: off-by-one line numbers, or a finding that does not say where it is,
    // so the lint's message cannot be acted on.
    let found = scan("a.txt", "ok\nok\nSerial: C02XK1ZZJGH5");
    assert_eq!(found[0].place, Place::Line(3));
    assert_eq!(found[0].to_string(), "line 3: serial number C02XK1ZZJGH5");
    let found = scan("a.json", r#"{"a": [{"deviceName": "Lab phone"}]}"#);
    assert_eq!(
        found[0].to_string(),
        "at a[0].deviceName: device name \"Lab phone\" (use \"Device Name\")"
    );
}

#[test]
fn only_files_under_the_fixture_directory_are_linted() {
    // Catches: a selection that misses the fixture directory (the lint then reads
    // nothing and passes) or that reaches past it into files with other hex ids.
    assert!(is_fixture(
        "crates/kbf-driver-native/tests/fixtures/devicectl/xcode-16.4-16F6/help.txt"
    ));
    assert!(is_fixture(
        "crates/kbf-driver-native/tests/fixtures/devicectl/README.md"
    ));
    for other in [
        "crates/kbf-driver-native/tests/fixtures/devicectl-old/help.txt",
        "crates/kbf-caps/tests/fixtures/pci/cpu_server/0000-02-00.0/uevent",
        "Cargo.lock",
    ] {
        assert!(!is_fixture(other), "{other}");
    }
}
