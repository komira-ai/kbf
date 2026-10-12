use super::*;
use crate::mdm::push_cert::test_challenge;
use crate::mdm::tests::config;

const TOPIC: &str = "com.apple.mgmt.External.0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5d";

/// The enrollment profile the shell kit rendered from its template, with the same
/// settings and its hard-coded payload identifiers replaced by the example's
/// MDM_PROFILE_ID: with that identifier set to the old one, the port hands the Macs the
/// same profile.
const SHELL_PROFILE: &[u8] = include_bytes!("testdata/enroll.mobileconfig");

fn topic() -> Topic {
    Topic::new(TOPIC).expect("a topic")
}

/// Catches a profile that differs from the one the shell kit served in any key, value
/// or type (a `Key Usage` as a string, a missing capability, a payload UUID changed so
/// a Mac would install a second profile), and one whose bytes vary between renders
/// (each render would plan a write).
#[test]
fn the_enrollment_profile_is_the_shell_kits_profile() {
    let c = config();
    let bytes = enrollment(&c, &topic(), &test_challenge());
    let ours = Value::from_reader_xml(bytes.as_slice()).expect("our profile parses");
    let theirs = Value::from_reader_xml(SHELL_PROFILE).expect("the shell profile parses");
    assert_eq!(ours, theirs);
    assert_eq!(bytes, enrollment(&c, &topic(), &test_challenge()));
    assert!(bytes.starts_with(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist"));
}

/// Catches an organisation or challenge that breaks out of its string (a `<` written
/// as markup would make a profile the Mac refuses, or one with a key of the
/// operator's choosing), and an MDM payload that names another identity than the
/// SCEP payload's.
#[test]
fn values_stay_inside_their_strings_and_the_identity_is_the_scep_payload() {
    let challenge = Secret::new("a</string><key>x");
    let mut c = config();
    c.org = "A & B <C>".to_owned();
    let bytes = enrollment(&c, &topic(), &challenge);
    let v = Value::from_reader_xml(bytes.as_slice()).expect("parses");
    let d = v.as_dictionary().expect("a dictionary");
    assert_eq!(
        d.get("PayloadOrganization").and_then(Value::as_string),
        Some("A & B <C>")
    );
    let payloads = d
        .get("PayloadContent")
        .and_then(Value::as_array)
        .expect("payloads");
    let scep = payloads[0].as_dictionary().expect("scep");
    let mdm = payloads[1].as_dictionary().expect("mdm");
    let inner = scep
        .get("PayloadContent")
        .and_then(Value::as_dictionary)
        .expect("scep content");
    assert_eq!(
        inner.get("Challenge").and_then(Value::as_string),
        Some("a</string><key>x")
    );
    assert_eq!(mdm.get("IdentityCertificateUUID"), scep.get("PayloadUUID"));
    assert_eq!(mdm.get("Topic").and_then(Value::as_string), Some(TOPIC));
}

/// Catches a topic check that takes another prefix, a short or long UUID, or a UUID
/// with a character that is not a hex digit or `-`.
#[test]
fn a_topic_is_an_mdm_topic() {
    assert_eq!(topic().to_string(), TOPIC);
    for bad in [
        "",
        "com.apple.mgmt.External.",
        "com.apple.mgmt.Internal.0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5d",
        "com.apple.mgmt.External.0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5",
        "com.apple.mgmt.External.0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5d0",
        "com.apple.mgmt.External.0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5g",
    ] {
        assert_eq!(
            Topic::new(bad),
            Err(ProfileError::Topic(bad.to_owned())),
            "{bad}"
        );
    }
}

/// The kit's ADE profile with `from` replaced by `to`.
fn edited(from: &str, to: &str) -> String {
    let text = ADE_TEMPLATE
        .replace("@ORG@", "Example")
        .replace("@DNS_NAME@", "mdm.example.com");
    assert!(text.contains(from), "the template holds {from:?}");
    text.replace(from, to)
}

/// Catches an ADE profile accepted that lets a device leave the MDM or never reach it:
/// removable (a person could remove the MDM, after which no erase reaches the Mac),
/// unsupervised, skippable, sent to another server, or carrying a key this kit does
/// not know (a typo of `is_mdm_removable` would otherwise leave Apple's default).
#[test]
fn an_ade_profile_a_device_could_leave_is_refused() {
    let dns = "mdm.example.com";
    let cases = [
        (
            edited("\"is_mdm_removable\": false", "\"is_mdm_removable\": true"),
            ProfileError::Removable,
        ),
        (
            edited("\"is_supervised\": true", "\"is_supervised\": false"),
            ProfileError::NotSupervised,
        ),
        (
            edited("\"is_mandatory\": true", "\"is_mandatory\": false"),
            ProfileError::NotMandatory,
        ),
        (
            edited(
                "https://mdm.example.com/enroll",
                "https://mdm.example.org/enroll",
            ),
            ProfileError::Url {
                got: "https://mdm.example.org/enroll".to_owned(),
                want: "https://mdm.example.com/enroll".to_owned(),
            },
        ),
    ];
    for (text, want) in cases {
        assert_eq!(AdeProfile::parse(&text, dns), Err(want));
    }
    for (from, to) in [
        ("\"is_mdm_removable\"", "\"is_mdm_removeable\""),
        ("\"is_supervised\": true", "\"is_supervised\": \"yes\""),
        ("{", "["),
    ] {
        let got = AdeProfile::parse(&edited(from, to), dns);
        assert!(matches!(got, Err(ProfileError::Json(_))), "{from}: {got:?}");
    }
}

/// Catches a kit template that no longer passes its own checks, an organisation that
/// breaks the JSON (a quote in MDM_ORG), and bytes sent to Apple that lose a field or
/// vary between renders (the recorded sha256 would never match).
#[test]
fn the_kits_ade_profile_renders_passes_and_is_stable() {
    let mut c = config();
    c.org = "Example \"Labs\"".to_owned();
    let p = AdeProfile::render(&c).expect("the kit's profile passes");
    assert_eq!(p.profile_name, "Example \"Labs\" build farm");
    assert_eq!(p.url, "https://mdm.example.com/enroll");
    assert!(p.is_supervised && p.is_mandatory && !p.is_mdm_removable);
    let json = p.to_json();
    assert_eq!(json, AdeProfile::render(&c).expect("again").to_json());
    let back: AdeProfile = serde_json::from_slice(&json).expect("round trip");
    assert_eq!(back, p);
    assert!(json.ends_with(b"}\n"));
}
