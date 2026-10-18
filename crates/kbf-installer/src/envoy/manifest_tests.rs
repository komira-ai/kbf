use super::manifest::{ManifestFault, pin_for, section};
use super::testing::manifest;
use super::{EnvoyId, valid_sha256, valid_version};
use crate::binary::Arch;
use crate::layout;

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const URL: &str = "https://upstream.example/envoy";

fn pin(text: &str) -> Result<super::manifest::EnvoyPin, ManifestFault> {
    pin_for(text.as_bytes(), Arch::X86_64)
}

/// Catches an entry that is read wrong (a field swapped or dropped), the other fields of
/// the manifest being looked at (they are the manifest parser's, not this one's), and the
/// platform picked by the host's CPU.
///
/// Mutant seen red: `platform_of` returning the x86-64 key for aarch64.
#[test]
fn the_entry_for_this_cpu_is_read_and_the_rest_of_the_manifest_ignored() {
    let text = format!(
        "{{\"schema\":1,\"unknown_to_this_parser\":[1],\"envoy\":{{\
         \"linux-x86_64\":{{\"version\":\"1.31.2\",\"url\":\"{URL}\",\"sha256\":\"{SHA}\"}},\
         \"linux-arm64\":{{\"version\":\"1.31.3\",\"url\":\"{URL}-arm\",\"sha256\":\"{}\"}}}}}}",
        SHA.replace('0', "f")
    );
    let x86 = pin_for(text.as_bytes(), Arch::X86_64).expect("x86");
    assert_eq!(
        (x86.version.as_str(), x86.url.as_str(), x86.sha256.as_str()),
        ("1.31.2", URL, SHA)
    );
    let arm = pin_for(text.as_bytes(), Arch::Aarch64).expect("arm");
    assert_eq!(arm.version, "1.31.3");
    assert_eq!(arm.url, format!("{URL}-arm"));
}

/// Catches each manifest the contract refuses being accepted: an http URL, a field this
/// parser does not know (in an entry), a version that is not three numbers, a missing or
/// malformed sha256, a platform that is not a Linux one, and a URL that is not a plain
/// https one (no host, credentials, a space).
///
/// Mutants seen red: the scheme check dropped (http accepted); `deny_unknown_fields`
/// removed (an extra field accepted); the version check accepting `1.31`; the sha256
/// check accepting an empty string.
#[test]
fn a_manifest_the_contract_refuses_is_refused_with_its_reason() {
    let ok = |version: &str, url: &str, sha: &str| manifest(version, url, sha);
    let cases = [
        (ok("1.31.2", "http://upstream.example/envoy", SHA), "url"),
        (ok("1.31.2", "ftp://upstream.example/envoy", SHA), "url"),
        (ok("1.31.2", "https://", SHA), "url"),
        (ok("1.31.2", "https:///envoy", SHA), "url"),
        (
            ok("1.31.2", "https://user:pw@upstream.example/envoy", SHA),
            "url",
        ),
        (ok("1.31.2", "https://upstream.example/en voy", SHA), "url"),
        (ok("1.31.2", "HTTPS://upstream.example/envoy", SHA), "url"),
        (ok("1.31", URL, SHA), "version"),
        (ok("v1.31.2", URL, SHA), "version"),
        (ok("1.31.2-rc1", URL, SHA), "version"),
        (ok("1.31.2.4", URL, SHA), "version"),
        (ok("1..2", URL, SHA), "version"),
        (ok("", URL, SHA), "version"),
        (ok("1.31.2", URL, ""), "sha256"),
        (ok("1.31.2", URL, &SHA.to_uppercase()), "sha256"),
        (ok("1.31.2", URL, &SHA[..63]), "sha256"),
        (ok("1.31.2", URL, "../../../etc/passwd"), "sha256"),
    ];
    for (text, field) in cases {
        let fault = pin(&text).expect_err(&text);
        let said = fault.to_string();
        assert!(said.contains(field), "{text}: {said}");
    }
    let extra = format!(
        "{{\"envoy\":{{\"linux-x86_64\":{{\"version\":\"1.31.2\",\"url\":\"{URL}\",\
         \"sha256\":\"{SHA}\",\"mirror\":\"https://m.example\"}}}}}}"
    );
    assert!(matches!(pin(&extra), Err(ManifestFault::Shape(m)) if m.contains("mirror")));
    let missing =
        format!("{{\"envoy\":{{\"linux-x86_64\":{{\"version\":\"1.31.2\",\"url\":\"{URL}\"}}}}}}");
    assert!(matches!(pin(&missing), Err(ManifestFault::Shape(_))));
    let number = "{\"envoy\":{\"linux-x86_64\":{\"version\":1,\"url\":\"u\",\"sha256\":\"s\"}}}";
    assert!(matches!(pin(number), Err(ManifestFault::Shape(_))));
}

/// Catches the section being accepted for a platform it is not for (macOS, or a key this
/// build does not know, even beside a good entry), a good entry for another CPU standing
/// in for this one, and the shapes that are not a manifest at all.
///
/// Mutant seen red: the platform key not checked (a darwin entry accepted).
#[test]
fn only_linux_entries_are_accepted_and_this_cpu_must_have_one() {
    let entry = |platform: &str| {
        format!("\"{platform}\":{{\"version\":\"1.31.2\",\"url\":\"{URL}\",\"sha256\":\"{SHA}\"}}")
    };
    let darwin = format!("{{\"envoy\":{{{}}}}}", entry("darwin-arm64"));
    assert_eq!(
        pin(&darwin),
        Err(ManifestFault::Platform("darwin-arm64".to_owned()))
    );
    let beside = format!(
        "{{\"envoy\":{{{},{}}}}}",
        entry("linux-x86_64"),
        entry("linux-riscv")
    );
    assert_eq!(
        pin(&beside),
        Err(ManifestFault::Platform("linux-riscv".to_owned()))
    );
    let arm_only = format!("{{\"envoy\":{{{}}}}}", entry("linux-arm64"));
    assert_eq!(pin(&arm_only), Err(ManifestFault::Missing("linux-x86_64")));
    assert!(pin_for(arm_only.as_bytes(), Arch::Aarch64).is_ok());
    assert_eq!(pin("{\"schema\":1}"), Err(ManifestFault::NoEnvoy));
    assert!(matches!(pin("[1]"), Err(ManifestFault::NotJson(_))));
    assert!(matches!(pin("not json"), Err(ManifestFault::NotJson(_))));
    assert!(matches!(
        pin("{\"envoy\":[]}"),
        Err(ManifestFault::Shape(_))
    ));
    assert!(
        section(b"{\"envoy\":{}}")
            .expect("empty section")
            .is_empty()
    );
}

/// Catches a build id that can name a path outside `envoy/` or two builds as one: only
/// `<n>.<n>.<n>-<16 lowercase hex>` is an id, and the same text round-trips through
/// serde, so a `state.json` that names a traversal is refused on read.
///
/// Mutants seen red: the prefix length not checked; uppercase hex accepted.
#[test]
fn only_a_version_and_sixteen_hex_digits_is_a_build_id() {
    let id = EnvoyId::new("1.31.2", SHA).expect("id");
    assert_eq!(id.as_str(), "1.31.2-0123456789abcdef");
    assert_eq!(
        (id.version(), id.sha_prefix()),
        ("1.31.2", "0123456789abcdef")
    );
    assert_eq!(id.to_string(), "1.31.2-0123456789abcdef");
    for bad in [
        "",
        "1.31.2",
        "1.31.2-",
        "1.31.2-0123456789abcde",
        "1.31.2-0123456789abcdef0",
        "1.31.2-0123456789ABCDEF",
        "1.31-0123456789abcdef",
        "../1.31.2-0123456789abcdef",
        "1.31.2-0123456789abcdef/..",
        "x-0123456789abcdef",
    ] {
        assert!(EnvoyId::parse(bad).is_err(), "{bad:?}");
    }
    assert!(EnvoyId::new("1.31.2", "abc").is_err());
    let json = serde_json::to_string(&id).expect("json");
    assert_eq!(json, "\"1.31.2-0123456789abcdef\"");
    assert_eq!(serde_json::from_str::<EnvoyId>(&json).expect("back"), id);
    assert!(serde_json::from_str::<EnvoyId>("\"../../x\"").is_err());
    assert!(valid_version("0.0.0") && !valid_version("1.2") && !valid_version("1.2.x"));
    assert!(valid_sha256(SHA) && !valid_sha256(&SHA[1..]));
}

/// Catches the layout paths: where a build's directory and its two files are.
#[test]
fn a_build_lives_under_the_installer_tree() {
    let id = EnvoyId::new("1.31.2", SHA).expect("id");
    assert_eq!(
        layout::path(layout::ENVOY_DIR).as_str(),
        "/var/lib/kbf-installer/envoy"
    );
    assert_eq!(
        layout::envoy_dir(&id).as_str(),
        "/var/lib/kbf-installer/envoy/1.31.2-0123456789abcdef"
    );
    assert_eq!(
        layout::envoy_file(&id, layout::ENVOY_BINARY).as_str(),
        "/var/lib/kbf-installer/envoy/1.31.2-0123456789abcdef/envoy"
    );
    assert_eq!(
        layout::envoy_file(&id, layout::ENVOY_SHA256).as_str(),
        "/var/lib/kbf-installer/envoy/1.31.2-0123456789abcdef/envoy.sha256"
    );
}
