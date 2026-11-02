//! The units and the Caddyfile: golden bytes for the example site, the template rules,
//! the names and the manifest.

mod common;

use std::collections::BTreeMap;

use common::{acme, bind, config, ip, read, site_addresses, with};
use mdm_core::config::MdmConfig;
use mdm_core::render::{
    CADDYFILE_NAME, Comments, Rendered, Unit, UnitNames, manifest, render, render_tls,
    strip_comments,
};
use mdm_core::template::{self, TemplateError, Values};

/// A committed golden file, with the example site's addresses in place of its
/// `@TEST_BIND@` and `@TEST_LAN@`.
fn golden(name: &str) -> String {
    site_addresses(&read(&format!("golden/example/{name}")))
}

fn files(r: &Rendered, tls: &[Unit]) -> Vec<Unit> {
    let mut all = r.files();
    all.extend(tls.iter().cloned());
    all
}

/// Catches a template edit, a changed substitution or a reordered unit: every file the
/// install verb renders for the example site is compared with its committed bytes.
#[test]
fn the_install_units_and_the_caddyfile_are_the_golden_bytes() {
    let r = render(&config()).expect("renders");
    let names: Vec<&str> = r.units.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "js-mdm.slice",
            "js-mdm-scep.service",
            "js-mdm-nanohub.service",
            "js-mdm-depserver.service",
            "js-mdm-depsyncer.service",
            "js-mdm-proxy.service",
        ]
    );
    for unit in &r.units {
        assert_eq!(unit.text, golden(&unit.name), "{}", unit.name);
    }
    assert_eq!(r.caddyfile, golden("Caddyfile"));
}

/// Catches the certificate job, its timer or the reload units drifting: the files the
/// TLS verb renders for the example site, with lego's arguments, are compared with their
/// committed bytes.
#[test]
fn the_tls_units_are_the_golden_bytes() {
    let units = render_tls(&config(), &acme()).expect("renders");
    let names: Vec<&str> = units.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "js-mdm-cert.service",
            "js-mdm-cert.timer",
            "js-mdm-proxy-reload.path",
            "js-mdm-proxy-reload.service",
        ]
    );
    for unit in &units {
        assert_eq!(unit.text, golden(&unit.name), "{}", unit.name);
    }
}

/// Catches a render that varies between calls (the host layer would plan a write each
/// time), and a manifest that does not match the committed hashes of the golden files,
/// with and without the comment lines.
#[test]
fn the_render_is_stable_and_its_manifest_is_the_committed_one() {
    let c = config();
    let r = render(&c).expect("renders");
    assert_eq!(r, render(&c).expect("again"));
    let tls = render_tls(&c, &acme()).expect("renders");
    let all = files(&r, &tls);
    assert_eq!(all.len(), 11);
    assert_eq!(all[6].name, CADDYFILE_NAME);
    assert_eq!(
        manifest(&all, Comments::Keep),
        golden("manifest.keep.sha256")
    );
    assert_eq!(
        manifest(&all, Comments::Strip),
        golden("manifest.strip.sha256")
    );
}

/// Catches comment stripping that drops a line of settings (a `#` inside a value), keeps
/// an indented comment, or changes a blank line.
#[test]
fn strip_comments_drops_only_whole_comment_lines() {
    let text = "# top\nA=1 # not a comment\n\n\t# indented\n  # also\nB=2\n";
    assert_eq!(strip_comments(text), "A=1 # not a comment\n\nB=2\n");
    assert_eq!(strip_comments(""), "");
}

/// Catches unit names that ignore the prefix or the slice setting, so two kits on one
/// host would write the same files, and templates that still name each other by a
/// fixed prefix: every unit that another unit orders itself after, restarts or is
/// started by exists under the configured prefix.
#[test]
fn names_follow_the_prefix_and_the_slice_and_units_refer_to_each_other_by_them() {
    let text = with("MDM_UNIT_PREFIX", "site-a")
        .replace("MDM_SLICE=js-mdm.slice", "MDM_SLICE=site-a-cap.slice");
    let c = MdmConfig::parse(&text).expect("valid");
    let n = UnitNames::new(&c);
    assert_eq!(n.slice, "site-a-cap.slice");
    assert_eq!(n.scep, "site-a-scep.service");
    assert_eq!(n.proxy, "site-a-proxy.service");
    assert_eq!(n.reload_path, "site-a-proxy-reload.path");
    let r = render(&c).expect("renders");
    let tls = render_tls(&c, &acme()).expect("renders");
    let all = files(&r, &tls);
    let names: Vec<&str> = all.iter().map(|u| u.name.as_str()).collect();
    for unit in &all {
        let live = strip_comments(&unit.text);
        assert!(!live.contains("js-mdm-"), "{}", unit.name);
        assert!(!live.contains("js-mdm.slice"), "{}", unit.name);
        for word in live.split_whitespace() {
            let named = word
                .strip_prefix("Unit=")
                .or_else(|| word.strip_prefix("Slice="))
                .unwrap_or(word);
            if named.starts_with("site-a-") || named.ends_with(".slice") {
                assert!(names.contains(&named), "{}: {named}", unit.name);
            }
        }
    }
    let slice = r
        .units
        .iter()
        .find(|u| u.name == "site-a-cap.slice")
        .expect("the slice");
    assert!(slice.text.contains("Description=site-a services (capped)"));
    for unit in &r.units[1..] {
        assert!(
            unit.text.contains("Slice=site-a-cap.slice\n"),
            "{}",
            unit.name
        );
    }
}

/// Catches settings that do not reach the files: the ports, the caps, the bind address
/// and the LAN, and the user, each changed and found where the unit uses it.
#[test]
fn settings_reach_the_units() {
    let other = ip(10, 0, 0, 11);
    let text = with("MDM_NANOHUB_PORT", "9104")
        .replace("MDM_MEMORY_MAX=1G", "MDM_MEMORY_MAX=2G")
        .replace(
            &format!("MDM_BIND={}", bind()),
            &format!("MDM_BIND={other}"),
        )
        .replace("MDM_USER=js-mdm", "MDM_USER=svc");
    let r = render(&MdmConfig::parse(&text).expect("valid")).expect("renders");
    let unit = |name: &str| {
        r.units
            .iter()
            .find(|u| u.name == name)
            .expect("unit")
            .text
            .clone()
    };
    assert!(unit("js-mdm-nanohub.service").contains("-listen 127.0.0.1:9104 "));
    assert!(unit("js-mdm.slice").contains("MemoryMax=2G\n"));
    assert!(unit("js-mdm-scep.service").contains("User=svc\nGroup=svc\n"));
    assert!(r.caddyfile.contains(&format!("\tdefault_bind {other}\n")));
    assert!(r.caddyfile.contains("reverse_proxy 127.0.0.1:9104\n"));
}

fn values() -> Values {
    BTreeMap::from([
        ("ETC", "/etc/x".to_owned()),
        ("IMAGE", "docker://r@sha256:ab".to_owned()),
        ("S3_KEY", "k".to_owned()),
    ])
}

/// Catches a placeholder left in place, two on one line not both replaced, a value
/// scanned again after it is inserted (the `@` in a digest would then read as a
/// placeholder), and a last line that loses its newline.
#[test]
fn every_placeholder_is_replaced_once() {
    let text = "# at @ETC@\n  --ca=@ETC@/ca.pem --image=@IMAGE@ --key=@ETC@/k\nlast @ETC@@S3_KEY@";
    assert_eq!(
        template::render("t", text, &values()),
        Ok("# at /etc/x\n  --ca=/etc/x/ca.pem --image=docker://r@sha256:ab --key=/etc/x/k\nlast /etc/xk\n"
            .to_owned())
    );
}

/// Catches a template whose placeholder has no value being rendered with the
/// placeholder left in (a leftover reaching the host), and a stray `@` passed through
/// (a typo such as `@ETC/ca.pem`, a lower-case key, a trailing `@`).
#[test]
fn an_unknown_placeholder_or_a_stray_at_is_refused() {
    let unknown = template::render("u.slice", "A=1\nB=@KBF_UID@\n", &values());
    assert_eq!(
        unknown,
        Err(TemplateError::Unknown {
            template: "u.slice".to_owned(),
            line: 2,
            key: "KBF_UID".to_owned()
        })
    );
    assert_eq!(
        unknown.expect_err("refused").to_string(),
        "u.slice:2: placeholder @KBF_UID@ has no value"
    );
    for text in [
        "A=@ETC/ca.pem\n",
        "A=@etc@\n",
        "A=user@\n",
        "A=@@\n",
        "A=@1X@\n",
        "A=@Etc@\n",
    ] {
        let got = template::render("s", text, &values());
        assert_eq!(
            got,
            Err(TemplateError::Stray {
                template: "s".to_owned(),
                line: 1
            }),
            "{text:?}"
        );
    }
    assert_eq!(
        template::render("s", "A\nB=@\n", &values())
            .expect_err("refused")
            .to_string(),
        "s:2: an @ that starts no @KEY@ placeholder"
    );
}
