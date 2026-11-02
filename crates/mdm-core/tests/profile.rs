//! The enrollment profile and the ADE profile.

mod common;

use common::{config, read, read_bytes, with};
use mdm_core::config::MdmConfig;
use mdm_core::profile::{
    AdeProfile, Challenge, ProfileError, Topic, enrollment, enrollment_to, payload_uuids, uuid_v5,
};
use plist::Value;

const TOPIC: &str = "com.apple.mgmt.External.0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5d";
const CHALLENGE: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";

fn topic() -> Topic {
    Topic::new(TOPIC).expect("a topic")
}

fn challenge(text: &str) -> Challenge {
    Challenge::new(text).expect("a challenge")
}

fn bytes(c: &MdmConfig) -> Vec<u8> {
    enrollment(c, &topic(), &challenge(CHALLENGE)).expect("writes")
}

/// Catches a profile that differs from the committed bytes in any key, value, type or
/// whitespace (a `Key Usage` as a string, a missing capability, a payload UUID changed
/// so a Mac would install a second profile), one whose bytes vary between renders (each
/// render would plan a write), and one that lacks the XML header or the final newline.
#[test]
fn the_enrollment_profile_is_the_golden_bytes() {
    let c = config();
    let got = bytes(&c);
    assert_eq!(got, read_bytes("golden/example/enroll.mobileconfig"));
    assert_eq!(got, bytes(&c));
    assert!(got.starts_with(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist"));
    assert!(got.ends_with(b"</plist>\n"));
}

/// Catches an organisation or challenge that breaks out of its string (a `<` written as
/// markup would make a profile the Mac refuses, or one with a key of the operator's
/// choosing), an MDM payload that names another identity than the SCEP payload's, and
/// the display name or identifier not taken from the settings.
#[test]
fn values_stay_inside_their_strings_and_the_identity_is_the_scep_payload() {
    let text = with("MDM_ORG", "A & B <C>").replace(
        "MDM_PROFILE_DISPLAY_NAME=Example MDM",
        "MDM_PROFILE_DISPLAY_NAME=Macs of <A>",
    );
    let c = MdmConfig::parse(&text).expect("valid");
    let out = enrollment(&c, &topic(), &challenge("a-b_C9")).expect("writes");
    let v = Value::from_reader_xml(out.as_slice()).expect("parses");
    let d = v.as_dictionary().expect("a dictionary");
    let string =
        |d: &plist::Dictionary, key: &str| d.get(key).and_then(Value::as_string).map(str::to_owned);
    assert_eq!(
        string(d, "PayloadOrganization").as_deref(),
        Some("A & B <C>")
    );
    assert_eq!(
        string(d, "PayloadDisplayName").as_deref(),
        Some("Macs of <A>")
    );
    assert_eq!(
        string(d, "PayloadIdentifier").as_deref(),
        Some("com.example.mdm")
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
    assert_eq!(string(inner, "Challenge").as_deref(), Some("a-b_C9"));
    assert_eq!(mdm.get("IdentityCertificateUUID"), scep.get("PayloadUUID"));
    assert_eq!(string(mdm, "Topic").as_deref(), Some(TOPIC));
    assert_eq!(
        string(mdm, "ServerURL").as_deref(),
        Some("https://mdm.example.com/mdm")
    );
}

/// Catches a topic check that takes another prefix, a short or long UUID, or a UUID
/// with a character that is not a hex digit or `-`.
#[test]
fn a_topic_is_an_mdm_topic() {
    assert_eq!(topic().to_string(), TOPIC);
    assert_eq!(topic().as_str(), TOPIC);
    for bad in [
        "",
        "com.apple.mgmt.External.",
        "com.apple.mgmt.Internal.0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5d",
        "com.apple.mgmt.External.0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5",
        "com.apple.mgmt.External.0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5d0",
        "com.apple.mgmt.External.0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5g",
        "com.apple.mgmt.External.0a1b2c3d4e5f-6a7b-8c9d-0e1f-2a3b4c5d0000",
        "com.apple.mgmt.External.0a1b2c3d-4e5f6a7b-8c9d-0e1f2a3b4c5d-",
        "com.apple.mgmt.External.0a1b2c3d-4e5f-6a7b-8c9d",
        "com.apple.mgmt.External.0a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d",
    ] {
        assert_eq!(
            Topic::new(bad),
            Err(ProfileError::Topic(bad.to_owned())),
            "{bad}"
        );
    }
    assert!(
        ProfileError::Topic("x".to_owned())
            .to_string()
            .contains("com.apple.mgmt.External")
    );
}

/// Catches a challenge that prints (it must not appear in `Debug` output), and one that
/// takes a character that could break out of the profile or the SCEP request (markup,
/// a space, a quote), or nothing at all.
#[test]
fn a_challenge_is_a_word_and_hides_itself_from_debug() {
    let s = challenge(CHALLENGE);
    assert_eq!(s.expose(), CHALLENGE);
    assert_eq!(format!("{s:?}"), "Challenge(..)");
    for bad in ["", "a b", "a</string>", "a\"b", "a'b", "a\nb", "é"] {
        assert_eq!(Challenge::new(bad), Err(ProfileError::Challenge), "{bad:?}");
    }
    assert!(ProfileError::Challenge.to_string().contains("challenge"));
}

/// Catches payload UUIDs that are fixed or shared: they are the version 5 UUIDs of
/// `<profile id>/<role>`, differ by role and by identifier, and are the values in the
/// committed profile. The expected strings come from Python's `uuid.uuid5` over the
/// same namespace.
#[test]
fn payload_uuids_are_derived_from_the_profile_identifier() {
    let a = payload_uuids("com.example.mdm");
    assert_eq!(a.profile, "E4B877DF-1340-5821-9BA6-1D6F56C71FA3");
    assert_eq!(a.scep, "6B16C231-042A-5BEF-A128-3C0EF8217615");
    assert_eq!(a.mdm, "0163031F-C163-594F-9610-567832E15734");
    assert_eq!(a, payload_uuids("com.example.mdm"));
    let b = payload_uuids("com.example.kbf-mdm");
    assert_eq!(b.profile, "DDA11057-29F5-5E29-92E4-BC45B3B192E6");
    assert_eq!(b.scep, "90229A97-619A-5239-B41C-3D9AD41C95B7");
    assert_eq!(b.mdm, "FE1BDA4B-D1B6-5616-AD82-16090768F47E");
    let text = String::from_utf8(bytes(&config())).expect("utf-8");
    for u in [&a.profile, &a.scep, &a.mdm] {
        assert!(text.contains(&format!("<string>{u}</string>")), "{u}");
    }
}

/// Catches a SHA-1 or UUID construction that is wrong for some message length (the
/// padding differs at 55, 56 and 64 bytes): RFC 4122's example, and names of lengths
/// around each block boundary, against Python's `uuid.uuid5`.
#[test]
fn uuid_v5_matches_the_reference_for_every_padding_length() {
    let dns = [
        0x6b, 0xa7, 0xb8, 0x10, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30,
        0xc8,
    ];
    assert_eq!(
        uuid_v5(&dns, b"www.example.com"),
        "2ED6657D-E927-568B-95E1-2665A8AEA6A2"
    );
    let vectors = [
        (0, "4EBD0208-8328-5D69-8C44-EC50939C0967"),
        (1, "4F3F2898-69E3-5A0D-820A-C4E87987DBCE"),
        (39, "5824F981-4282-59D4-9716-ACB6D741350E"),
        (40, "39F39C20-DB47-5131-8879-62F8F67F9014"),
        (47, "660C273C-8A00-5941-B6F4-8D0AFED88966"),
        (48, "7280CC42-274A-5C4A-91FC-AE23F853EEB7"),
        (100, "56596F37-716C-57A9-A735-2561F8608390"),
        (200, "FE30BFA6-EF90-59B3-B108-118411D0F0AB"),
    ];
    for (n, want) in vectors {
        assert_eq!(uuid_v5(&dns, "a".repeat(n).as_bytes()), want, "{n}");
    }
}

/// A writer that accepts `room` bytes and then fails.
struct Limited {
    room: usize,
}

impl std::io::Write for Limited {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.room == 0 {
            return Err(std::io::Error::other("full"));
        }
        let n = buf.len().min(self.room);
        self.room -= n;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Catches a write failure that is swallowed or panics: a writer that fails in the
/// property list, or only at the final newline, makes the render an error.
#[test]
fn a_failing_writer_is_an_error_not_a_panic() {
    let c = config();
    let full = bytes(&c).len();
    for room in [0, 10, full - 1] {
        let got = enrollment_to(&c, &topic(), &challenge(CHALLENGE), &mut Limited { room });
        assert!(
            matches!(got, Err(ProfileError::Write(ref why)) if why.contains("full")),
            "{room}: {got:?}"
        );
    }
    let ok = enrollment_to(
        &c,
        &topic(),
        &challenge(CHALLENGE),
        &mut Limited { room: full },
    );
    assert_eq!(ok, Ok(()));
}

/// The committed ADE profile with `from` replaced by `to`.
fn edited(from: &str, to: &str) -> String {
    let text = read("golden/example/ade_profile.json");
    assert!(text.contains(from), "the profile holds {from:?}");
    text.replace(from, to)
}

/// Catches an ADE profile accepted that lets a device leave the MDM or never reach it:
/// removable (a person could remove the MDM, after which no erase reaches the Mac),
/// unsupervised, skippable, sent to another server, or carrying a key this kit does not
/// know (a typo of `is_mdm_removable` would otherwise leave Apple's default).
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
    let said = |text: &str| {
        AdeProfile::parse(text, dns)
            .expect_err("refused")
            .to_string()
    };
    assert!(said(&edited("{", "[")).contains("not the JSON this kit reads"));
    assert!(
        said(&edited(
            "\"is_mdm_removable\": false",
            "\"is_mdm_removable\": true"
        ))
        .contains("is_mdm_removable")
    );
    assert!(
        said(&edited(
            "\"is_supervised\": true",
            "\"is_supervised\": false"
        ))
        .contains("is_supervised")
    );
    assert!(
        said(&edited("\"is_mandatory\": true", "\"is_mandatory\": false")).contains("is_mandatory")
    );
    assert!(
        said(&edited(
            "https://mdm.example.com/enroll",
            "https://x/enroll"
        ))
        .contains("sends devices to")
    );
}

/// Catches a profile built from the settings that differs from the committed bytes
/// (the bytes sent to Apple, whose sha256 the host layer records), one that loses a
/// field or varies between builds, a profile the parser would refuse, and a setting
/// (name, org_magic, language, region, skipped panes) that does not reach it.
#[test]
fn the_ade_profile_is_built_from_the_settings_and_passes_its_own_checks() {
    let c = config();
    let p = AdeProfile::from_config(&c);
    let json = p.to_json().expect("the profile serializes");
    assert_eq!(json, read_bytes("golden/example/ade_profile.json"));
    assert_eq!(
        json,
        AdeProfile::from_config(&c)
            .to_json()
            .expect("the profile serializes")
    );
    let text = String::from_utf8(json.clone()).expect("utf-8");
    assert_eq!(AdeProfile::parse(&text, &c.dns_name), Ok(p.clone()));
    let back: AdeProfile = serde_json::from_slice(&json).expect("round trip");
    assert_eq!(back, p);
    assert!(json.ends_with(b"}\n"));

    let custom = with("MDM_ADE_PROFILE_NAME", "Site \"A\"")
        .replace("MDM_LANGUAGE=en", "MDM_LANGUAGE=fr")
        .replace("MDM_REGION=US", "MDM_REGION=CA")
        .replace("MDM_ADE_ORG_MAGIC=js-mdm", "MDM_ADE_ORG_MAGIC=site.a");
    let c = MdmConfig::parse(&custom).expect("valid");
    let p = AdeProfile::from_config(&c);
    assert_eq!(p.profile_name, "Site \"A\"");
    assert_eq!(
        (p.org_magic.as_str(), p.language.as_str(), p.region.as_str()),
        ("site.a", "fr", "CA")
    );
    assert!(p.is_supervised && p.is_mandatory && !p.is_mdm_removable);
    let text = String::from_utf8(p.to_json().expect("the profile serializes")).expect("utf-8");
    assert!(
        text.contains("\"profile_name\": \"Site \\\"A\\\"\""),
        "{text}"
    );
    assert_eq!(AdeProfile::parse(&text, &c.dns_name), Ok(p));
}
