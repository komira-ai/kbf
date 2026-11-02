//! The port is faithful: with the names kbf-deploy's `mdm` module fixed, this crate
//! produces what that module produced.
//!
//! The expected side is kbf's own files, copied unchanged into `tests/kbf_f89ed5c/` from
//! the commit `f89ed5c063728e2949c2bcc65c2cda99a313d771` of its repository
//! (`crates/kbf-deploy/templates/mdm/*` and
//! `crates/kbf-deploy/src/mdm/testdata/enroll.mobileconfig`). The units and the
//! Caddyfile are expanded here by plain text replacement of each `@KEY@`, not by this
//! crate's renderer. The settings that name kbf's values are
//! `tests/kbf_f89ed5c/kbf_names.conf`, which holds `@TEST_BIND@` and `@TEST_LAN@` for
//! kbf's bind address and LAN; the tests build those addresses at run time.
//!
//! Two differences from kbf are deliberate, and the tests name them: comment lines are
//! this crate's own wording (they say `js-mdm`, not `kbf-deploy`), and the slice's
//! `Description=` has lost the words ` on the dev box`. Everything else is compared byte
//! for byte.

mod common;

use common::{ip, read, read_bytes};
use mdm_core::acme::Acme;
use mdm_core::commands::{dep_cert_request_path, scep_ca_init_args};
use mdm_core::config::{KEYS, MdmConfig};
use mdm_core::pins::{BUILT_IN, Pins};
use mdm_core::profile::{AdeProfile, Challenge, Topic, enrollment, payload_uuids};
use mdm_core::render::{Comments, Unit, manifest, render, render_tls, strip_comments};
use plist::Value;

fn kbf(path: &str) -> String {
    read(&format!("kbf_f89ed5c/{path}"))
}

/// kbf's bind address and LAN, built at run time (a public file carries no private
/// address); `kbf_names.conf` holds `@TEST_BIND@` and `@TEST_LAN@` in their place.
fn kbf_bind() -> String {
    ip(10, 1, 0, 10)
}

fn kbf_lan() -> String {
    format!("{}/24", ip(10, 1, 0, 0))
}

fn site() -> (MdmConfig, Acme) {
    let text = kbf("kbf_names.conf")
        .replace("@TEST_BIND@", &kbf_bind())
        .replace("@TEST_LAN@", &kbf_lan());
    (
        MdmConfig::parse(&text).expect("kbf's names are valid settings"),
        Acme::parse(&text).expect("kbf's ACME settings are valid"),
    )
}

/// kbf's template `name` with every placeholder replaced by kbf's value for it.
fn expand(name: &str) -> String {
    let (lan, bind) = (kbf_lan(), kbf_bind());
    let pairs = [
        ("USER", "kbf-mdm"),
        ("STATE", "/var/lib/kbf-mdm"),
        ("ETC", "/etc/kbf-mdm"),
        ("BIN", "/opt/kbf-mdm/bin"),
        ("NANOHUB_PORT", "9004"),
        ("SCEP_PORT", "9005"),
        ("DEP_PORT", "9001"),
        ("DEP_NAME", "example"),
        ("LAN_CIDR", lan.as_str()),
        ("BIND", bind.as_str()),
        ("DNS_NAME", "mdm.example.com"),
        ("MEMORY_MAX", "1G"),
        ("CPU_QUOTA", "100%"),
        ("TASKS_MAX", "256"),
        ("ACME_EMAIL", "admin@example.com"),
        (
            "ACME_SERVER",
            "https://acme-v02.api.letsencrypt.org/directory",
        ),
        ("DNS_PROVIDER", "cloudflare"),
        ("DNS_RESOLVERS", "192.0.2.53:53,198.51.100.53:53"),
        ("PROPAGATION_WAIT", "300s"),
    ];
    let mut text = kbf(&format!("templates/{name}"));
    for (key, value) in pairs {
        text = text.replace(&format!("@{key}@"), value);
    }
    // The only `@` left is the one in the email address.
    let emails = text.matches("admin@example.com").count();
    assert_eq!(
        text.matches('@').count(),
        emails,
        "{name} holds a placeholder"
    );
    text
}

/// The one deliberate wording change outside the comments.
fn kbf_slice_wording(text: &str) -> String {
    text.replace(" on the dev box", "")
}

fn rendered() -> Vec<Unit> {
    let (c, acme) = site();
    let mut all = render(&c).expect("renders").files();
    all.extend(render_tls(&c, &acme).expect("renders"));
    all
}

/// Catches a port that changed what a unit or the Caddyfile does: every file this crate
/// renders with kbf's names is compared with kbf's own template expanded by plain text
/// replacement, comment lines left out, byte for byte. A changed directive, port,
/// sandbox option, order of lines or name fails here.
#[test]
fn rendering_with_kbfs_names_reproduces_kbfs_files() {
    let expected = [
        ("kbf-mdm.slice", "kbf-mdm.slice"),
        ("kbf-mdm-scep.service", "kbf-mdm-scep.service"),
        ("kbf-mdm-nanohub.service", "kbf-mdm-nanohub.service"),
        ("kbf-mdm-depserver.service", "kbf-mdm-depserver.service"),
        ("kbf-mdm-depsyncer.service", "kbf-mdm-depsyncer.service"),
        ("kbf-mdm-proxy.service", "kbf-mdm-proxy.service"),
        ("Caddyfile", "Caddyfile"),
        ("kbf-mdm-cert.service", "kbf-mdm-cert.service"),
        ("kbf-mdm-cert.timer", "kbf-mdm-cert.timer"),
        ("kbf-mdm-proxy-reload.path", "kbf-mdm-proxy-reload.path"),
        (
            "kbf-mdm-proxy-reload.service",
            "kbf-mdm-proxy-reload.service",
        ),
    ];
    let ours = rendered();
    assert_eq!(ours.len(), expected.len());
    for (unit, (name, template)) in ours.iter().zip(expected) {
        assert_eq!(unit.name, name);
        let want = strip_comments(&expand(template));
        let want = kbf_slice_wording(&want);
        assert_eq!(strip_comments(&unit.text), want, "{name}");
    }
}

/// Catches the deliberate differences growing: the comment lines are the only other
/// lines that differ, so a file's non-comment lines are exactly kbf's, and the only
/// line that changed outside a comment is the slice's `Description=`.
#[test]
fn the_only_lines_that_differ_are_comments_and_one_description() {
    for unit in rendered() {
        let want = expand(&unit.name);
        let mut ours = unit
            .text
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'));
        let mut theirs = want.lines().filter(|l| !l.trim_start().starts_with('#'));
        loop {
            let (a, b) = (ours.next(), theirs.next());
            assert_eq!(
                a.is_some(),
                b.is_some(),
                "{}: line counts differ",
                unit.name
            );
            let (Some(a), Some(b)) = (a, b) else { break };
            if a != b {
                assert_eq!(unit.name, "kbf-mdm.slice");
                assert_eq!(a, "Description=kbf-mdm services (capped)");
                assert_eq!(b, "Description=kbf-mdm services on the dev box (capped)");
            }
        }
        assert!(
            !unit.text.contains("kbf-deploy"),
            "{} names the old tool",
            unit.name
        );
    }
}

/// Catches the hook's reference data drifting from kbf's templates: the committed
/// hashes of what kbf-deploy installs (comments left out) must be the hashes of kbf's
/// templates expanded here, and this crate's manifest must equal them for every file
/// except the slice, whose hash must be that of the slice with the one wording change.
#[test]
fn the_manifest_equals_the_committed_hashes_of_kbfs_installed_files() {
    let committed = kbf("expected_installed.strip.sha256");
    let ours = manifest(&rendered(), Comments::Strip);
    let committed_lines: Vec<&str> = committed.lines().collect();
    let our_lines: Vec<&str> = ours.lines().collect();
    assert_eq!(committed_lines.len(), our_lines.len());
    let mut expanded = Vec::new();
    for line in &committed_lines {
        let name = line.split_once("  ").expect("hash and name").1;
        expanded.push(Unit {
            name: name.to_owned(),
            text: expand(name),
        });
    }
    let recomputed = manifest(&expanded, Comments::Strip);
    assert_eq!(recomputed, committed);
    for (i, (a, b)) in our_lines.iter().zip(&committed_lines).enumerate() {
        if i == 0 {
            let renamed = Unit {
                name: "kbf-mdm.slice".to_owned(),
                text: kbf_slice_wording(&expanded[0].text),
            };
            assert_eq!(*a, manifest(&[renamed], Comments::Strip).trim_end());
            assert_ne!(a, b, "the slice differs by its Description");
        } else {
            assert_eq!(a, b);
        }
    }
}

/// Catches an enrollment profile that differs from the one kbf-deploy's tests hold as
/// the shell kit's profile: the same keys, values and types, and the same bytes. One
/// difference is deliberate and handled here: this crate derives the three payload UUIDs
/// from the profile identifier, where kbf-deploy had three fixed ones, so kbf's three are
/// put back in place of the derived ones before the comparison.
#[test]
fn the_enrollment_profile_is_kbfs_profile() {
    let (c, _) = site();
    let theirs = read_bytes("kbf_f89ed5c/enroll.mobileconfig");
    let topic = Topic::new("com.apple.mgmt.External.0a1b2c3d-4e5f-6a7b-8c9d-0e1f2a3b4c5d")
        .expect("a topic");
    let challenge =
        Challenge::new("0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0")
            .expect("a challenge");
    let mut ours =
        String::from_utf8(enrollment(&c, &topic, &challenge).expect("writes")).expect("utf-8");
    let theirs_text = String::from_utf8(theirs.clone()).expect("utf-8");
    let derived = payload_uuids(&c.profile_id);
    for (ours_uuid, kbf_uuid) in [
        (&derived.scep, "071DB849-C11F-4A24-B1E8-69113460B678"),
        (&derived.mdm, "25B0DB54-2EDA-4965-B430-24DB8BFC06DD"),
        (&derived.profile, "5F652B4C-C5C9-4D69-B44E-493CB00C698E"),
    ] {
        assert!(ours.contains(ours_uuid.as_str()), "{ours_uuid}");
        assert!(theirs_text.contains(kbf_uuid), "{kbf_uuid}");
        assert!(!theirs_text.contains(ours_uuid.as_str()), "{ours_uuid}");
        ours = ours.replace(ours_uuid.as_str(), kbf_uuid);
    }
    let ours = ours.into_bytes();
    let a = Value::from_reader_xml(ours.as_slice()).expect("ours parses");
    let b = Value::from_reader_xml(theirs.as_slice()).expect("theirs parses");
    assert_eq!(a, b);
    assert_eq!(ours, theirs);
}

/// Catches an ADE profile that differs from kbf's template once its two placeholders
/// are filled: the same fields with the same values, and the same field order (the order
/// in which they are sent).
#[test]
fn the_ade_profile_is_kbfs_profile() {
    let (c, _) = site();
    let template = kbf("templates/dep_profile.json")
        .replace("@ORG@", "Example")
        .replace("@DNS_NAME@", "mdm.example.com");
    let ours = String::from_utf8(AdeProfile::from_config(&c).to_json()).expect("utf-8");
    let a: serde_json::Value = serde_json::from_str(&ours).expect("ours parses");
    let b: serde_json::Value = serde_json::from_str(&template).expect("theirs parses");
    assert_eq!(a, b);
    let top = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|l| l.starts_with("  \""))
            .map(|l| l.split('"').nth(1).unwrap_or_default().to_owned())
            .collect()
    };
    assert_eq!(top(&ours), top(&template));
    assert_eq!(top(&ours).len(), 12);
}

/// Catches the pins drifting from kbf's: the same tools, versions, URLs and hashes.
#[test]
fn the_pins_are_kbfs_pins() {
    let theirs = kbf("pins.txt");
    let live = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(live(BUILT_IN), live(&theirs));
    assert_eq!(Pins::built_in(), Pins::parse(&theirs).expect("valid"));
}

/// Catches the SCEP CA or the DEP certificate request differing from kbf's, where they
/// were literals in the installer.
#[test]
fn the_scep_ca_and_the_dep_request_are_kbfs() {
    let (c, _) = site();
    assert_eq!(
        scep_ca_init_args(&c),
        [
            "ca",
            "-init",
            "-depot",
            "/var/lib/kbf-mdm/scep/depot",
            "-years",
            "10",
            "-common_name",
            "kbf-mdm SCEP CA",
            "-organization",
            "Example",
            "-organizational_unit",
            "kbf-mdm",
        ]
    );
    assert_eq!(
        dep_cert_request_path(&c),
        "/v1/tokenpki/example?cn=kbf-mdm&validity_days=7"
    );
}

/// Catches a key kbf-deploy read that this crate lost: every key of kbf's closed set is
/// still accepted, except `MDM_ERASE_NAMESPACE`, which `MDM_SIGNATURE_NAMESPACE`
/// replaces (it was the name of a constant in kbf's gate, accepted and not read).
#[test]
fn every_key_kbf_read_is_still_accepted() {
    const KBF_KEYS: [&str; 25] = [
        "MDM_DNS_NAME",
        "MDM_BIND",
        "MDM_LAN_CIDR",
        "MDM_USER",
        "MDM_STATE",
        "MDM_ETC",
        "MDM_BIN",
        "MDM_ERASE_STATE",
        "MDM_NANOHUB_PORT",
        "MDM_SCEP_PORT",
        "MDM_DEP_PORT",
        "MDM_MEMORY_MAX",
        "MDM_CPU_QUOTA",
        "MDM_TASKS_MAX",
        "MDM_DEP_NAME",
        "MDM_ORG",
        "MDM_PROFILE_ID",
        "MDM_ACME_EMAIL",
        "MDM_ACME_SERVER",
        "MDM_ACME_DNS_PROVIDER",
        "MDM_ACME_RESOLVERS",
        "MDM_ACME_PROPAGATION_WAIT",
        "MDM_ERASE_NAMESPACE",
        "MDM_ERASE_PRINCIPAL",
        "MDM_ERASE_MAX_AGE",
    ];
    for key in KBF_KEYS {
        let kept = KEYS.contains(&key);
        assert_eq!(kept, key != "MDM_ERASE_NAMESPACE", "{key}");
    }
    assert!(KEYS.contains(&"MDM_SIGNATURE_NAMESPACE"));
    assert_eq!(KEYS.len(), KBF_KEYS.len() - 1 + 14);
}
