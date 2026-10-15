//! The profile parser and `print`: every refusal the script's tests make, each named
//! for the profile it refuses, and what `print` shows.

mod support;

use kbf_mac_provision::profile::{Profile, ProfileError};
use support::FakeMac;

/// The test profile with `edit` applied to its lines.
fn edited(edit: impl FnOnce(&mut Vec<String>)) -> String {
    let mac = FakeMac::fresh("profile-edit");
    let mut lines: Vec<String> = mac.profile_text().lines().map(str::to_owned).collect();
    edit(&mut lines);
    lines.iter().map(|l| format!("{l}\n")).collect()
}

fn set(key: &str, value: &str) -> impl FnOnce(&mut Vec<String>) {
    let (key, value) = (key.to_owned(), value.to_owned());
    move |lines| {
        let line = lines
            .iter_mut()
            .find(|l| l.starts_with(&format!("{key}=")))
            .unwrap();
        *line = format!("{key}={value}");
    }
}

fn drop_keys(keys: &[&str]) -> impl FnOnce(&mut Vec<String>) {
    let keys: Vec<String> = keys.iter().map(|k| format!("{k}=")).collect();
    move |lines| lines.retain(|l| !keys.iter().any(|k| l.starts_with(k)))
}

fn append(line: &str) -> impl FnOnce(&mut Vec<String>) {
    let line = line.to_owned();
    move |lines| lines.push(line)
}

/// Replaces `from` with `to` in the one line that holds it.
fn replace(from: &str, to: &str) -> impl FnOnce(&mut Vec<String>) {
    let (from, to) = (from.to_owned(), to.to_owned());
    move |lines| {
        let line = lines.iter_mut().find(|l| l.contains(&from)).unwrap();
        *line = line.replace(&from, &to);
    }
}

fn refusal(text: &str) -> String {
    match Profile::parse(text.as_bytes()) {
        Ok(_) => "accepted".into(),
        Err(e) => e.to_string(),
    }
}

/// Catches: a refusal rule removed or loosened (an unknown, repeated or missing key; a
/// value that could be run as a command; a value of the wrong shape; values that
/// contradict each other). Each row is a case of the script's `t_refused`.
#[test]
fn every_malformed_profile_is_refused() {
    let long = format!("{}-mac-1", "a".repeat(60));
    let x = "0".repeat(64);
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "unknown key",
            edited(append("sleep=0")),
            "unknown key: sleep",
        ),
        (
            "duplicate key",
            edited(append("firewall=on")),
            "duplicate key: firewall",
        ),
        (
            "key with a space",
            edited(|l| {
                drop_keys(&[
                    "xcode",
                    "expect_probe",
                    "default_developer_dir",
                    "xcode_xip_dir",
                ])(l);
                l.push("default_developer_dir xcode_xip_dir=/x".into());
            }),
            "unknown key: default_developer_dir xcode_xip_dir",
        ),
        ("not KEY=VALUE", edited(append("justtext")), "not KEY=VALUE"),
        (
            "missing key",
            edited(drop_keys(&["firewall"])),
            "missing key: firewall",
        ),
        (
            "command substitution",
            edited(set("hostname", "$(mkdir PWNED)kbf-mac-07")),
            "may hold only",
        ),
        (
            "backquote",
            edited(set("hostname", "`mkdir PWNED`")),
            "may hold only",
        ),
        (
            "semicolon",
            edited(set("time_server", "a;mkdir PWNED")),
            "may hold only",
        ),
        (
            "bad profile name",
            edited(set("profile", "Mac Pool")),
            "profile: ",
        ),
        (
            "bad version",
            edited(set("macos_version", "27.x")),
            "macos_version: ",
        ),
        (
            "bad build",
            edited(set("macos_build", "26a000")),
            "macos_build: ",
        ),
        (
            "upper-case name",
            edited(set("hostname", "Kbf-Mac-07")),
            "hostname: ",
        ),
        (
            "name with a space",
            edited(set("hostname", "kbf mac 07")),
            "hostname: ",
        ),
        (
            "name without -mac-",
            edited(set("hostname", "kbf-07")),
            "hostname: ",
        ),
        (
            "long name",
            edited(set("hostname", &long)),
            "longer than 63",
        ),
        (
            "bad admin",
            edited(set("admin_user", "Admin")),
            "admin_user: ",
        ),
        (
            "role without _",
            edited(set("role_user", "kbf")),
            "role_user: ",
        ),
        ("role id text", edited(set("role_id", "x")), "role_id: "),
        ("role id low", edited(set("role_id", "150")), "below 200"),
        ("role id high", edited(set("role_id", "501")), "below 500"),
        (
            "role id huge",
            edited(set("role_id", "99999999999")),
            "below 500",
        ),
        (
            "lease range text",
            edited(set("lease_uids", "600")),
            "lease_uids: ",
        ),
        (
            "lease range huge",
            edited(set("lease_uids", "600-99999999999999999999")),
            "a range FIRST-LAST",
        ),
        (
            "lease range low",
            edited(set("lease_uids", "400-699")),
            "starts below 501",
        ),
        (
            "lease range empty",
            edited(set("lease_uids", "700-600")),
            "is empty",
        ),
        (
            "sleep text",
            edited(set("power_sleep", "never")),
            "power_sleep: ",
        ),
        (
            "offset huge",
            edited(set("time_max_offset_ms", "99999999999999999999")),
            "time_max_offset_ms: ",
        ),
        (
            "autorestart 2",
            edited(set("power_autorestart", "2")),
            "power_autorestart: ",
        ),
        (
            "firewall maybe",
            edited(set("firewall", "maybe")),
            "firewall: ",
        ),
        ("filevault x", edited(set("filevault", "x")), "filevault: "),
        (
            "autologin user",
            edited(set("autologin", "farmadmin")),
            "autologin: ",
        ),
        (
            "simulator runtimes",
            edited(set("simulator_runtimes", "ios")),
            "simulator_runtimes: ",
        ),
        (
            "time server",
            edited(set("time_server", "-x")),
            "time_server: ",
        ),
        (
            "label",
            edited(set("launchd_label", ".x")),
            "launchd_label: ",
        ),
        (
            "plain http server",
            edited(set("kbf_server", "http://farm.example.net:8981")),
            "kbf_server: ",
        ),
        (
            "cas with a path",
            edited(set("kbf_cas", "https://farm.example.net/x")),
            "kbf_cas: ",
        ),
        (
            "relative cert",
            edited(set("kbf_cert", "node.pem")),
            "kbf_cert: ",
        ),
        (
            "bad label",
            edited(set("kbf_labels", "pool=mac rack")),
            "is not key=value",
        ),
        (
            "profile label",
            edited(set("kbf_labels", "profile=x")),
            "profile label",
        ),
        (
            "relative xip dir",
            edited(set("xcode_xip_dir", "xip")),
            "xcode_xip_dir: ",
        ),
        (
            "xcode outside /Applications",
            edited(|l| {
                let line = l
                    .iter_mut()
                    .find(|l| l.starts_with("xcode=17A000:"))
                    .unwrap();
                *line = line.replace("xcode=17A000:/Applications", "xcode=17A000:/opt/x");
            }),
            "xcode: ",
        ),
        (
            "xcode twice",
            edited(|l| {
                let line = l
                    .iter_mut()
                    .find(|l| l.starts_with("xcode=17A000"))
                    .unwrap();
                *line = line.replace("xcode=17A000", "xcode=18A000");
            }),
            "pinned twice",
        ),
        (
            "default dir not pinned",
            edited(set(
                "default_developer_dir",
                "/Applications/Xcode.app/Contents/Developer",
            )),
            "default_developer_dir: ",
        ),
        (
            "default dir without xcode",
            edited(drop_keys(&["xcode", "expect_probe"])),
            "no xcode is pinned",
        ),
        (
            "bad probe",
            edited(|l| {
                let line = l.iter_mut().find(|l| l.starts_with("probe=")).unwrap();
                *line = line.replace("probe=host_identity:", "probe=Host:");
            }),
            "probe: ",
        ),
        (
            "probe twice",
            edited(append(&format!("probe=host_identity:/x:{x}"))),
            "named twice",
        ),
        (
            "bad expectation",
            edited(|l| {
                let line = l
                    .iter_mut()
                    .find(|l| l.starts_with("expect_probe=host_identity:17A000="))
                    .unwrap();
                *line = "expect_probe=host_identity=1".into();
            }),
            "expect_probe: ",
        ),
        (
            "expectation of no probe",
            edited(|l| {
                let line = l
                    .iter_mut()
                    .find(|l| l.starts_with("expect_probe=host_identity:17A000"))
                    .unwrap();
                *line = line.replace("host_identity:17A000", "other:17A000");
            }),
            "names no probe",
        ),
        (
            "expectation of no xcode",
            edited(|l| {
                let line = l
                    .iter_mut()
                    .find(|l| l.starts_with("expect_probe=host_identity:17A000"))
                    .unwrap();
                *line = line.replace("host_identity:17A000", "host_identity:19A000");
            }),
            "names no pinned xcode",
        ),
        (
            "expectation twice",
            edited(append("expect_probe=host_identity:18A000=x")),
            "host_identity:18A000 is named twice",
        ),
        ("no final newline", "profile=x".into(), "no newline"),
        (
            "lease range end text",
            edited(set("lease_uids", "600-x")),
            "a range FIRST-LAST",
        ),
        (
            "lease range start text",
            edited(set("lease_uids", "x-600")),
            "a range FIRST-LAST",
        ),
        (
            "xcode build",
            edited(replace("xcode=17A000:", "xcode=17a000:")),
            "xcode: 17a000:",
        ),
        (
            "xcode SHA-256",
            edited(replace(
                ":/Applications/Xcode-26.6.app/Contents/Developer:",
                ":/Applications/Xcode-26.6.app/Contents/Developer:x",
            )),
            "is not BUILD:",
        ),
        (
            "probe path",
            edited(replace(
                "probe=host_identity:/usr",
                "probe=host_identity:usr",
            )),
            "is not KEY:/PATH:SHA256",
        ),
        (
            "probe SHA-256",
            edited(replace("host_identity.sh:", "host_identity.sh:x")),
            "is not KEY:/PATH:SHA256",
        ),
        (
            "expectation key",
            edited(replace(
                "expect_probe=host_identity:17A000",
                "expect_probe=Host:17A000",
            )),
            "is not KEY:XCODE_BUILD=VALUE",
        ),
        (
            "expectation build",
            edited(replace(
                "expect_probe=host_identity:17A000",
                "expect_probe=host_identity:17a000",
            )),
            "is not KEY:XCODE_BUILD=VALUE",
        ),
        (
            "expectation value",
            edited(replace("=26.6-0123456789abcdef", "=26.6 x")),
            "is not KEY:XCODE_BUILD=VALUE",
        ),
    ];
    let wrong: Vec<String> = cases
        .iter()
        .filter_map(|(name, text, want)| {
            let got = refusal(text);
            (!got.contains(want)).then(|| format!("{name}: got {got:?}, want {want:?}"))
        })
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// Catches: an unknown key ignored instead of refused (the script's mutant "an unknown
/// key is ignored").
#[test]
fn an_unknown_key_is_refused() {
    assert_eq!(
        Profile::parse(edited(append("sleep=0")).as_bytes()).unwrap_err(),
        ProfileError::UnknownKey {
            line: 42,
            key: "sleep".into()
        }
    );
}

/// Catches: a repeated key accepted, so a later line silently wins (the script's
/// mutant "a duplicate key is accepted").
#[test]
fn a_repeated_key_is_refused() {
    assert_eq!(
        Profile::parse(edited(append("firewall=off")).as_bytes()).unwrap_err(),
        ProfileError::Duplicate {
            line: 42,
            key: "firewall".into()
        }
    );
}

/// Catches: a profile without one of its keys accepted (the script's mutant "a missing
/// key is accepted"): every required key, one at a time.
#[test]
fn each_missing_required_key_is_refused() {
    for key in kbf_mac_provision::profile::REQUIRED {
        assert_eq!(
            refusal(&edited(drop_keys(&[key]))),
            format!("missing key: {key}"),
            "{key}"
        );
    }
}

/// Catches: a value allowed any character (the script's mutant "any character in a
/// value"), so `$(...)`, backquotes or `;` reach a program's arguments.
#[test]
fn a_value_outside_the_character_set_is_refused() {
    for bad in [
        "$", "`", ";", "|", "&", "<", "\"", "'", "\\", "\t", "*", "\r",
    ] {
        let text = edited(set("kbf_labels", &format!("pool=m{bad}c")));
        assert_eq!(
            Profile::parse(text.as_bytes()).unwrap_err(),
            ProfileError::BadCharacter {
                line: 33,
                key: "kbf_labels".into()
            },
            "{bad:?}"
        );
    }
}

/// Catches: a key allowed a space (the script's mutant "a key may hold a space"),
/// which in the script matched two adjacent known keys at once.
#[test]
fn a_key_with_a_space_or_capital_is_refused() {
    for key in ["firewall x", "Firewall", "fire-wall", ""] {
        let text = edited(append(&format!("{key}=on")));
        assert_eq!(
            Profile::parse(text.as_bytes()).unwrap_err(),
            ProfileError::UnknownKey {
                line: 42,
                key: key.into()
            }
        );
    }
}

/// Catches: the node name scheme loosened to allow upper case and spaces (the script's
/// mutant "the name scheme accepts upper case and spaces").
#[test]
fn a_hostname_outside_the_scheme_is_refused() {
    for name in ["Kbf-Mac-07", "kbf mac 07", "kbf-07", "kbf-mac-x"] {
        assert!(
            refusal(&edited(set("hostname", name))).starts_with("hostname: "),
            "{name}"
        );
    }
}

/// Catches: a role id of 500 or more accepted (the script's mutant), which is not a
/// hidden role account's id.
#[test]
fn a_role_id_of_500_or_more_is_refused() {
    assert_eq!(
        refusal(&edited(set("role_id", "500"))),
        "role_id: 500 is not a hidden role id (below 500)"
    );
    assert_eq!(refusal(&edited(set("role_id", "200"))), "accepted");
    assert_eq!(refusal(&edited(set("role_id", "499"))), "accepted");
    assert_eq!(
        refusal(&edited(set("role_id", "199"))),
        "role_id: 199 is below 200"
    );
}

/// Catches: an expectation allowed to name an Xcode the profile does not pin (the
/// script's mutant "an expectation may name an unpinned Xcode").
#[test]
fn an_expectation_of_an_unpinned_xcode_is_refused() {
    let text = edited(append("expect_probe=host_identity:19A000=x"));
    assert_eq!(refusal(&text), "expect_probe: 19A000 names no pinned xcode");
}

/// Catches: a file that does not exist, or a directory, read as an empty profile.
#[test]
fn an_unreadable_profile_is_refused() {
    let dir = support::scratch("unreadable");
    for path in [dir.join("missing"), dir.clone()] {
        assert_eq!(
            Profile::load(&path).unwrap_err(),
            ProfileError::Unreadable(path.display().to_string())
        );
    }
}

/// Catches: blank lines or comments read as keys, or an empty file refused for its
/// form rather than for its first missing key.
#[test]
fn blank_lines_and_comments_are_skipped() {
    let mut text = String::from("# a comment\n\n");
    text.push_str(&edited(|_| {}));
    text.push_str("\n#=x\n");
    assert_eq!(refusal(&text), "accepted");
    assert_eq!(refusal(""), "missing key: profile");
    assert_eq!(refusal("\n"), "missing key: profile");
}

/// Catches: `print` leaving a key out, showing an empty optional key, or getting the
/// SHA-256 or node label wrong (the script's `t_print`).
#[test]
fn print_shows_the_resolved_profile_its_sha256_and_the_node_label() {
    let mac = FakeMac::fresh("print");
    let text = mac.profile_text();
    let sha = support::sha256_hex(text.as_bytes());
    let printed = Profile::parse(text.as_bytes()).unwrap().print();
    let lines: Vec<&str> = printed.lines().collect();
    for want in [
        "profile=mac-arm64-test",
        "hostname=kbf-mac-07",
        "xcode_xip_dir=/Library/kbf/xip",
        "kbf_labels=pool=mac rack=r2",
        "default_developer_dir=/Applications/Xcode-27.0.app/Contents/Developer",
        "expect_probe=host_identity:17A000=26.6-0123456789abcdef",
        "expect_probe=host_identity:18A000=",
        &format!("profile_sha256={sha}"),
        &format!("node_label=profile=mac-arm64-test@{}", &sha[..12]),
    ] {
        assert!(lines.contains(&want), "print lacks {want:?}:\n{printed}");
    }
    let file_lines = text.lines().filter(|l| !l.is_empty()).count();
    // Every line of the file, then the SHA-256 and the node label.
    assert_eq!(lines.len(), file_lines + 2, "{printed}");
    for line in text.lines() {
        assert!(lines.contains(&line), "print lacks {line:?}");
    }
    assert!(!printed.contains('#'));
    // The repeated keys come after the single ones, in the file's order.
    let first_xcode = lines.iter().position(|l| l.starts_with("xcode=")).unwrap();
    assert!(
        lines[first_xcode - 1].starts_with("xcode_xip_dir="),
        "{printed}"
    );
    assert!(
        lines[first_xcode + 1].starts_with("xcode=18A000"),
        "{printed}"
    );
    assert!(lines[first_xcode + 2].starts_with("probe="), "{printed}");
}

/// Catches: an optional key left out of the file printed as `key=` (the script's
/// `t_print`), or an empty `kbf_labels` printed.
#[test]
fn print_leaves_out_empty_and_absent_keys() {
    let text = edited(|l| {
        drop_keys(&["xcode_xip_dir"])(l);
        set("kbf_labels", "")(l);
    });
    let printed = Profile::parse(text.as_bytes()).unwrap().print();
    assert!(!printed.contains("xcode_xip_dir"), "{printed}");
    assert!(!printed.contains("kbf_labels"), "{printed}");
}

/// Catches: the example profile drifting from the parser's rules, so the example the
/// README points to is refused (the script's `t_print_real_root`).
#[test]
fn the_example_profile_is_accepted() {
    let p = Profile::parse(support::EXAMPLE.as_bytes()).unwrap();
    assert!(p.node_label().starts_with("profile=mac-arm64-pool@"));
    assert_eq!(p.role_id, 480);
    assert_eq!(p.lease_uids, 600..=699);
    assert_eq!(p.time_max_offset_ms, 250);
    assert_eq!(p.labels, ["pool=mac", "rack=r2"]);
    assert_eq!(p.xcodes[1].app(), "/Applications/Xcode-27.0.app");
    assert_eq!(p.probes[0].key, "host_identity");
}

/// Catches: the other value of the on/off keys refused: FileVault and the firewall
/// may be on or off.
#[test]
fn on_and_off_are_both_accepted() {
    for (key, value) in [("filevault", "on"), ("firewall", "off")] {
        assert_eq!(
            refusal(&edited(set(key, value))),
            "accepted",
            "{key}={value}"
        );
    }
}

/// Catches: a pool, volume or daemon setting of the wrong shape accepted: a pool of
/// none or of more than 64 users, pool uids below 600, above 65535 or inside
/// `lease_uids` (the default, the uid after `lease_uids`, included), a volume that is
/// not `/Volumes/<name>`, a `daemon_user` other than `role` or `root`, and, for a root
/// daemon, a scratch or cache root off the volume.
#[test]
fn every_malformed_pool_volume_or_daemon_setting_is_refused() {
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "no pool user",
            edited(append("pool_users=0")),
            "pool_users: 0 is not a number from 1 to 64",
        ),
        (
            "65 pool users",
            edited(append("pool_users=65")),
            "pool_users: 65 is not a number from 1 to 64",
        ),
        (
            "pool users not a number",
            edited(append("pool_users=four")),
            "pool_users: four is not a number from 1 to 64",
        ),
        (
            "pool users too large to parse",
            edited(append("pool_users=99999999999999999999999")),
            "is not a number from 1 to 64",
        ),
        (
            "first pool uid below 600",
            edited(|l| {
                set("lease_uids", "501-550")(l);
                l.push("pool_first_uid=599".into());
            }),
            "pool_first_uid: 599 is below 600",
        ),
        (
            "first pool uid not a number",
            edited(append("pool_first_uid=7e2")),
            "pool_first_uid: 7e2 is not a number",
        ),
        (
            "first pool uid too large to parse",
            edited(append("pool_first_uid=99999999999999999999999")),
            "pool_first_uid: 99999999999999999999999 is not a number",
        ),
        (
            "pool inside lease_uids",
            edited(append("pool_first_uid=697")),
            "pool uids 697-700 overlap lease_uids 600-699",
        ),
        (
            "pool running into lease_uids from below",
            edited(|l| {
                set("lease_uids", "650-699")(l);
                l.push("pool_first_uid=648".into());
            }),
            "pool uids 648-651 overlap lease_uids 650-699",
        ),
        (
            "pool above 65535",
            edited(append("pool_first_uid=65533")),
            "pool uids 65533-65536 go above 65535",
        ),
        (
            "default pool above 65535",
            edited(set("lease_uids", "600-65533")),
            "pool uids 65534-65537 go above 65535",
        ),
        (
            "volume outside /Volumes",
            edited(append("kbf_volume=/private/kbf")),
            "kbf_volume: /private/kbf is not /Volumes/<name>",
        ),
        (
            "volume below a volume",
            edited(append("kbf_volume=/Volumes/kbf/x")),
            "kbf_volume: /Volumes/kbf/x is not /Volumes/<name>",
        ),
        (
            "volume with no name",
            edited(append("kbf_volume=/Volumes/")),
            "kbf_volume: /Volumes/ is not /Volumes/<name>",
        ),
        (
            "relative cache",
            edited(append("kbf_cache=cache")),
            "kbf_cache: cache is not an absolute path",
        ),
        (
            "daemon user",
            edited(append("daemon_user=nobody")),
            "daemon_user: nobody is not role or root",
        ),
        (
            "root scratch off the volume",
            edited(|l| {
                set("kbf_data_dir", "/private/var/kbf")(l);
                l.push("daemon_user=root".into());
            }),
            "kbf_data_dir: /private/var/kbf is not on kbf_volume /Volumes/kbf (daemon_user=root)",
        ),
        (
            "root cache off the volume",
            edited(|l| {
                l.push("kbf_cache=/Volumes/kbfcache".into());
                l.push("daemon_user=root".into());
            }),
            "kbf_cache: /Volumes/kbfcache is not on kbf_volume /Volumes/kbf (daemon_user=root)",
        ),
    ];
    let wrong: Vec<String> = cases
        .iter()
        .filter_map(|(name, text, want)| {
            let got = refusal(text);
            (!got.contains(want)).then(|| format!("{name}: got {got:?}, want {want:?}"))
        })
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// Catches: a valid pool, volume or daemon setting refused (a pool below `lease_uids`
/// included), a role daemon held to the root daemon's volume rule, or a set key left
/// out of `print`.
#[test]
fn the_pool_volume_and_daemon_settings_are_accepted_and_printed() {
    let text = edited(|l| {
        set("kbf_data_dir", "/private/var/kbf")(l);
        l.extend(
            [
                "pool_users=64",
                "pool_first_uid=700",
                "kbf_volume=/Volumes/farm",
                "kbf_cache=/private/var/cache",
                "daemon_user=role",
            ]
            .map(str::to_owned),
        );
    });
    let printed = Profile::parse(text.as_bytes()).unwrap().print();
    assert!(
        printed.contains(
            "pool_users=64\npool_first_uid=700\nkbf_volume=/Volumes/farm\nkbf_cache=/private/var/cache\ndaemon_user=role\n"
        ),
        "{printed}"
    );
    let text = edited(|l| {
        set("kbf_data_dir", "/Volumes/farm/data")(l);
        l.extend(
            [
                "pool_users=1",
                "kbf_volume=/Volumes/farm",
                "kbf_cache=/Volumes/farm/cache",
                "daemon_user=root",
            ]
            .map(str::to_owned),
        );
    });
    assert_eq!(refusal(&text), "accepted");
    let below = edited(|l| {
        set("lease_uids", "800-899")(l);
        l.push("pool_first_uid=600".into());
    });
    assert_eq!(refusal(&below), "accepted", "a pool below lease_uids");
}
