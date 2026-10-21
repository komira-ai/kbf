//! The kit-host fixtures load, lay out as the kit leaves a Mac, and agree with each other
//! and with the installer's paths. They test the fixtures, not `migrate`: a change to the
//! kit moves the fixture files, and these say which fact moved.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use kbf_testkit::dir::TestDir;

use super::kit_host::*;
use super::roots::Roots;
use crate::layout::Platform;

fn mode_of(root: &std::path::Path, host: &str) -> u32 {
    fs::symlink_metadata(at(root, host))
        .expect("metadata")
        .permissions()
        .mode()
        & 0o7777
}

fn entry(path: &str) -> Entry {
    entries()
        .into_iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("{path} is not in the manifest"))
}

/// Catches a manifest that drifts from the kit's layout: release binaries that are not
/// root:wheel 0755 `kbf`, links that are absolute or point at the wrong release, certificate
/// owners or modes that differ, a data directory the daemon's user cannot own.
///
/// Goes red on (not run as mutants): `previous` and `current` swapped; the release binary named `kbf-daemon`;
/// the key at 0644; an absolute link target.
#[test]
fn the_tree_is_the_kits() {
    let dir = TestDir::under(std::env::temp_dir(), "kit-host-tree");
    let root = dir.path().to_path_buf();
    lay_out(&root, Form::New);

    for sha in [PREVIOUS, CURRENT] {
        let bin = format!("{TREE}/releases/{sha}/kbf");
        let e = entry(&bin);
        assert_eq!((e.user.as_str(), e.group.as_str()), ("root", "wheel"));
        assert_eq!(mode_of(&root, &bin), 0o755);
        assert!(!at(&root, &format!("{TREE}/releases/{sha}/kbf-daemon")).exists());
    }
    for (link, sha) in [("current", CURRENT), ("previous", PREVIOUS)] {
        let target = fs::read_link(at(&root, &format!("{TREE}/{link}"))).expect("a link");
        assert_eq!(target.to_str(), Some(format!("releases/{sha}").as_str()));
        assert!(at(&root, &format!("{TREE}/{link}/kbf")).is_file());
    }

    let etc = format!("{TREE}/etc");
    for (path, mode, owner) in [
        (format!("{etc}/ca.pem"), 0o644, "root:wheel"),
        (format!("{etc}/v2"), 0o750, "root:_kbf"),
        (format!("{etc}/v2/daemon-mac-a.pem"), 0o644, "root:_kbf"),
        (format!("{etc}/v2/daemon-mac-a.key"), 0o400, "_kbf:_kbf"),
        (format!("{etc}/av2_mac.conf"), 0o644, "root:wheel"),
        (format!("{TREE}/log"), 0o750, "_kbf:_kbf"),
        (format!("{TREE}/work/daemon/data"), 0o700, "_kbf:_kbf"),
        (format!("{TREE}/removed"), 0o755, "root:wheel"),
        (KBF_TOML_PATH.to_owned(), 0o644, "root:wheel"),
        (PLIST_PATH.to_owned(), 0o644, "root:wheel"),
    ] {
        let e = entry(&path);
        assert_eq!(format!("{}:{}", e.user, e.group), owner, "{path}");
        assert_eq!(mode_of(&root, &path), mode, "{path}");
    }
}

/// Catches a fixture that gives a kit host the old tools' state: on a Mac the links are the
/// truth, so `state.json`, `helper.json`, the old state directory and any lock must be absent.
///
/// Goes red on (not run as mutants): `/var/db/kbf-deploy/state.json` added to the manifest.
#[test]
fn a_kit_host_has_no_old_state() {
    let dir = TestDir::under(std::env::temp_dir(), "kit-host-no-state");
    let root = dir.path().to_path_buf();
    lay_out(&root, Form::New);
    let roots = Roots::new(Platform::MacOs);
    assert_eq!(roots.old_tree.as_str(), TREE);
    assert_eq!(roots.old_state.as_str(), "/var/db/kbf-deploy");
    assert_eq!(roots.unit_dir.as_str(), "/Library/LaunchDaemons");
    assert!(at(&root, roots.old_tree.as_str()).is_dir());
    assert!(!at(&root, roots.old_state.as_str()).exists());
    assert!(!at(&root, roots.old_lock.as_str()).exists());
    for e in entries() {
        assert!(!e.path.ends_with("state.json") && !e.path.ends_with("helper.json"));
        assert!(!e.path.starts_with("/var/"), "{}", e.path);
    }
    // The plist is where migrate looks for units.
    assert_eq!(
        format!("{}/{LABEL}.plist", roots.unit_dir.as_str()),
        PLIST_PATH
    );
    assert!(at(&root, PLIST_PATH).is_file());
}

/// Catches a plist that is not the kit's: the new form names the binary, the mode and the file
/// and nothing else; the old form is the `daemon` word and flags; both run as `_kbf`, restart
/// is the same label, and the two differ only in the arguments.
///
/// Goes red on (not run as mutants): a flag after `--config`; `kbf-daemon` as the program; `UserName` root.
#[test]
fn the_plists_are_the_kits() {
    assert_eq!(
        program_arguments(PLIST_NEW),
        [
            "/usr/local/kbf/current/kbf",
            "--mode=daemon",
            "--config",
            KBF_TOML_PATH
        ]
    );
    let url = "https://frontend.example.invalid:8981";
    let etc = "/usr/local/kbf/etc";
    assert_eq!(
        program_arguments(PLIST_OLD),
        [
            "/usr/local/kbf/current/kbf",
            "daemon",
            "--driver",
            "native",
            "--server",
            url,
            "--cas",
            url,
            "--ca-cert",
            &format!("{etc}/ca.pem"),
            "--cert",
            &format!("{etc}/v2/daemon-mac-a.pem"),
            "--key",
            &format!("{etc}/v2/daemon-mac-a.key"),
            "--node-id",
            "mac-a",
            "--data-dir",
            "/usr/local/kbf/work/daemon/data",
        ]
    );
    for plist in [PLIST_NEW, PLIST_OLD] {
        assert_eq!(plist_string(plist, "Label").as_deref(), Some(LABEL));
        assert_eq!(plist_string(plist, "UserName").as_deref(), Some("_kbf"));
        assert_eq!(plist_string(plist, "GroupName").as_deref(), Some("_kbf"));
        assert_eq!(
            plist_string(plist, "WorkingDirectory").as_deref(),
            Some("/usr/local/kbf/work/daemon")
        );
        for key in ["RunAtLoad", "KeepAlive"] {
            assert!(
                plist.contains(&format!("<key>{key}</key>\n  <true/>")),
                "{key}"
            );
        }
        assert!(plist.contains("<key>ThrottleInterval</key>\n  <integer>10</integer>"));
        assert!(plist.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        assert!(plist.ends_with("</plist>\n"));
    }
    // Same text but for the arguments.
    let strip = |p: &str| {
        let a = p.find("<array>").expect("array");
        let b = p.find("</array>").expect("array end");
        format!("{}{}", &p[..a], &p[b..])
    };
    assert_eq!(strip(PLIST_NEW), strip(PLIST_OLD));
    assert_eq!(KICKSTART.join(" "), format!("kickstart -k system/{LABEL}"));
}

/// Catches a `kbf.toml` that is not what `daemon_toml` renders: keys, tables, no `label` key,
/// and every file it names being a file of the tree.
///
/// Goes red on (not run as mutants): a `label` key; `driver` as a string; a `[identity]` path that the tree
/// does not hold.
#[test]
fn the_kbf_toml_is_the_kits() {
    let table: toml::Table = KBF_TOML.parse().expect("kbf.toml parses");
    let keys: Vec<&str> = table.keys().map(String::as_str).collect();
    assert_eq!(keys, ["daemon", "identity", "mode"]);
    assert_eq!(table["mode"].as_str(), Some("daemon"));
    let identity = table["identity"].as_table().expect("[identity]");
    let mut got: Vec<&str> = identity.keys().map(String::as_str).collect();
    got.sort_unstable();
    assert_eq!(
        got,
        ["ca-cert", "cas", "cert", "key", "node-id", "server"],
        "no label key: the kit has no profile label"
    );
    let daemon = table["daemon"].as_table().expect("[daemon]");
    let mut got: Vec<&str> = daemon.keys().map(String::as_str).collect();
    got.sort_unstable();
    assert_eq!(
        got,
        ["data-dir", "driver", "update-dir"],
        "no data-max-bytes by default; update-dir is the provisioner's"
    );
    assert_eq!(
        daemon["driver"].as_array().map(|a| a.len()),
        Some(1),
        "driver is an array"
    );
    // The key order of the file is the renderer's.
    let at_of = |needle: &str| KBF_TOML.find(needle).expect(needle);
    assert!(at_of("mode =") < at_of("[identity]") && at_of("[identity]") < at_of("[daemon]"));
    assert!(at_of("server =") < at_of("cas =") && at_of("node-id =") < at_of("[daemon]"));

    let paths: Vec<String> = entries().into_iter().map(|e| e.path).collect();
    for key in ["ca-cert", "cert", "key"] {
        let named = identity[key].as_str().expect("a path");
        assert!(paths.iter().any(|p| p == named), "{key} names {named}");
    }
    let data = daemon["data-dir"].as_str().expect("a path");
    assert!(paths.iter().any(|p| p == data), "data-dir names {data}");
    assert_eq!(identity["node-id"].as_str(), Some("mac-a"));
    assert!(
        identity["cert"]
            .as_str()
            .is_some_and(|c| c.contains("daemon-mac-a.pem"))
    );
    // The new plist's `--config` is this file.
    assert_eq!(program_arguments(PLIST_NEW)[3], KBF_TOML_PATH);
    assert_eq!(KBF_TOML_PATH, Platform::MacOs.kbf_toml());
}

/// Catches the old form being laid out when the new one is asked for (and the reverse), and
/// a manifest whose plist differs from the fixture file: the rollback target is these exact
/// bytes.
///
/// Goes red on (not run as mutants): `lay_out` writing the new plist for `Form::Old`.
#[test]
fn the_form_picks_the_plist_bytes() {
    for form in [Form::New, Form::Old] {
        let dir = TestDir::under(std::env::temp_dir(), &format!("kit-host-{form:?}"));
        let root = dir.path().to_path_buf();
        lay_out(&root, form);
        let on_disk = fs::read_to_string(at(&root, PLIST_PATH)).expect("plist");
        assert_eq!(on_disk, form.plist());
        assert_eq!(
            on_disk.contains("--mode=daemon"),
            form == Form::New,
            "the kit reads the form from this text"
        );
        assert_eq!(
            fs::read_to_string(at(&root, KBF_TOML_PATH)).expect("kbf.toml"),
            KBF_TOML
        );
    }
    assert_ne!(PLIST_NEW, PLIST_OLD);
}

/// Catches a fixture that carries a real-looking secret, an internal name or a date before the
/// public repository's start: placeholders are plainly not keys, the frontend is
/// `frontend.example.invalid`, the commits are obviously fake.
///
/// Goes red on (not run as mutants): a base64 body between the PEM lines.
#[test]
fn the_fixtures_are_public_safe() {
    let all = [MANIFEST, PLIST_NEW, PLIST_OLD, KBF_TOML, KIT_CONF, README];
    for text in all {
        let lower = text.to_lowercase();
        for banned in ["farm", "fleet", "nightly", "/home/", "/users/", "tailnet"] {
            assert!(!lower.contains(banned), "a fixture says {banned:?}");
        }
        for line in text.lines().filter(|l| l.contains("https://")) {
            assert!(
                line.contains("frontend.example.invalid"),
                "a name that is not the example one: {line}"
            );
        }
    }
    for e in entries()
        .iter()
        .filter(|e| e.source.is_none() && e.kind == Kind::File)
    {
        let text = placeholder(&e.path);
        assert!(text.contains("test fixture"), "{}", e.path);
        assert!(!text.contains("PRIVATE KEY") && !text.contains("BEGIN CERTIFICATE"));
    }
    assert_eq!(PREVIOUS, "a".repeat(40));
    assert_eq!(CURRENT, "b".repeat(40));
    assert!(KIT_CONF.contains("FRONTEND_NAME=frontend.example.invalid"));
}

/// A manifest line of an unknown kind is a broken fixture, never a silently skipped entry.
#[test]
#[should_panic(expected = "manifest kind \"pipe\"")]
fn an_unknown_manifest_kind_is_a_broken_fixture() {
    parse("pipe 0644 root:wheel /usr/local/kbf/x\n");
}

/// Comments and blank lines are not entries; the kinds parse with their modes and targets.
#[test]
fn the_manifest_parser_reads_each_kind() {
    let got = parse(
        "# c\n\ndir 0755 root:wheel /a\nfile - _kbf:staff /a/f @kbf.toml\nlink - root:wheel /a/l releases/x\n",
    );
    assert_eq!(got.len(), 3);
    assert_eq!((got[0].kind, got[0].mode), (Kind::Dir, Some(0o755)));
    assert_eq!(
        (got[1].kind, got[1].mode, got[1].source.as_deref()),
        (Kind::File, None, Some("kbf.toml"))
    );
    assert_eq!(
        (got[2].kind, got[2].target.as_deref()),
        (Kind::Link, Some("releases/x"))
    );
}

/// A file that names a fixture the loader does not carry is a broken fixture.
#[test]
#[should_panic(expected = "no fixture file \"missing.toml\"")]
fn a_missing_fixture_file_is_a_broken_fixture() {
    super::kit_host::source("missing.toml");
}
