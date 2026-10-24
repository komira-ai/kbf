//! The `[daemon] update-dir` and `orders-dir` `setup` writes for a node that takes orders. The
//! helpers are `tests.rs`'s. Each test says the defect it catches.

use kbf_host::Root;

use super::*;
use crate::layout::Layout;

const KEYS: [&str; 2] = ["update-dir", "orders-dir"];

fn layout(platform: Platform) -> Layout {
    Layout::new(Root::new("/").expect("root"), platform)
}

/// The two keys of `[daemon]` in the file `setup` wrote, if any.
fn written(exec: &FakeExec, path: &str) -> [Option<String>; 2] {
    let table: toml::Table = text(exec, path).parse().expect("setup writes TOML");
    let daemon = table.get("daemon").and_then(toml::Value::as_table);
    KEYS.map(|k| {
        daemon
            .and_then(|d| d.get(k))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    })
}

/// Catches a mode writing the wrong set: the two modes whose daemon takes orders get both keys,
/// with the layout's paths (not literals in setup), and the others get neither. `all-in-one`'s
/// in-process daemon refuses `--update-dir` at start, so a key there would stop the host; a
/// frontend and a storage node have no daemon.
///
/// Catches: `takes_orders` true for all-in-one (its file gets the keys).
#[test]
fn the_modes_whose_daemon_takes_orders_get_the_installers_directories() {
    let l = layout(Platform::Linux);
    for (mode, takes) in [
        (Mode::Daemon, true),
        (Mode::StorageAndDaemon, true),
        (Mode::AllInOne, false),
        (Mode::Frontend, false),
        (Mode::Storage, false),
    ] {
        let raw = if mode == Mode::Frontend {
            super::kbf_settings_tests::frontend()
        } else if mode == Mode::AllInOne {
            super::kbf_settings_tests::all_in_one()
        } else {
            raw(mode)
        };
        let mut exec = super::kbf_settings_tests::host_for(&raw);
        go_ok(&FakeOs::linux(), &mut exec, &raw);
        let got = written(&exec, KBF_TOML);
        if takes {
            assert_eq!(
                got,
                [
                    Some(l.update_dir().to_string()),
                    Some(l.orders_dir().to_string())
                ],
                "{mode:?}"
            );
        } else {
            assert_eq!(got, [None, None], "{mode:?}");
        }
    }
}

/// Catches the keys and the spool `setup` makes drifting apart: on Linux the directory the
/// orders key names is the one made for the service user (0700). A Mac's state directory differs
/// (`/var/db/kbf-installer`), and there the keys are `kbf-mac-provision`'s: setup leaves the
/// file as it was and still makes the same spool.
///
/// Catches: the spool made 0755.
#[test]
fn the_orders_key_names_the_directory_setup_makes() {
    let mut exec = released(host(), Platform::Linux);
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon));
    let [update, orders] = written(&exec, Platform::Linux.kbf_toml());
    let l = layout(Platform::Linux);
    assert_eq!(update.as_deref(), Some(l.update_dir().as_str()));
    assert_eq!(orders.as_deref(), Some(l.orders_dir().as_str()));
    let made = exec.entries.get(&hp(orders.as_deref().expect("orders")));
    assert!(
        matches!(made, Some(Entry::Dir { perms, .. }) if perms.bits() == 0o700),
        "{made:?}"
    );
    assert_eq!(l.update_dir().as_str(), "/var/lib/kbf-installer");
    assert_eq!(
        layout(Platform::MacOs).orders_dir().as_str(),
        "/var/db/kbf-installer/orders"
    );
}

/// A Mac's `kbf.toml` has no `[daemon]` update keys (`kbf-mac-provision` renders none), and
/// `setup` neither requires them nor writes them: the file is accepted and kept byte for byte,
/// and one that still has the keys from an earlier provisioning is kept too. With no file at
/// all, `setup` writes its own keys and no update key.
#[test]
fn a_mac_without_the_update_keys_is_accepted_and_none_are_written() {
    let base = "mode = \"daemon\"\n\n[frontend]\nurl = \"https://farm.example:8443\"\n\n[enroll]\ntoken_file = \"/etc/kbf/enroll.token\"\n\n";
    let token = Platform::MacOs.enroll_token();
    let base = base.replace("/etc/kbf/enroll.token", token);
    for daemon in [
        "[daemon]\ndriver = [\"native\"]\n",
        "[daemon]\nupdate-dir = \"/var/db/kbf-installer\"\norders-dir = \"/var/db/kbf-installer/orders\"\n",
    ] {
        let file = format!("{base}{daemon}");
        let mut exec = released(host(), Platform::MacOs);
        put(&mut exec, Platform::MacOs.kbf_toml(), 0o640, ROOT, &file);
        go_ok(&FakeOs::mac(), &mut exec, &raw(Mode::Daemon));
        assert_eq!(text(&exec, Platform::MacOs.kbf_toml()), file);
    }
    let mut exec = released(host(), Platform::MacOs);
    exec.entries.remove(&hp(Platform::MacOs.kbf_toml()));
    go_ok(&FakeOs::mac(), &mut exec, &raw(Mode::Daemon));
    assert_eq!(written(&exec, Platform::MacOs.kbf_toml()), [None, None]);
}

/// Catches the Mac check judging a file that is not TOML as missing keys: `decide` names it.
#[test]
fn a_mac_kbf_toml_that_is_not_toml_is_named_as_such() {
    let mut exec = released(host(), Platform::MacOs);
    put(
        &mut exec,
        Platform::MacOs.kbf_toml(),
        0o640,
        ROOT,
        "mode = [",
    );
    let err = go(&FakeOs::mac(), &mut exec, &raw(Mode::Daemon), APPLY).expect_err("refused");
    assert!(err.to_string().contains("not valid TOML"), "{err}");
}

/// Catches a rerun that rewrites or duplicates: setup run twice writes `kbf.toml` once, and a
/// file an earlier setup wrote without the keys (it holds only setup's keys) is brought
/// forward to have them.
///
/// Not covered: `has_other_settings` counting the update keys as foreign.
#[test]
fn a_rerun_changes_nothing_and_an_older_file_gains_the_keys() {
    let mut exec = released(host(), Platform::Linux);
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon));
    let wrote = exec
        .log
        .iter()
        .filter(|s| matches!(s, Step::WriteFile { path, .. } if path.as_str() == KBF_TOML))
        .count();
    assert_eq!(wrote, 1);
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon));
    let wrote = exec
        .log
        .iter()
        .filter(|s| matches!(s, Step::WriteFile { path, .. } if path.as_str() == KBF_TOML))
        .count();
    assert_eq!(wrote, 1, "the second run wrote it again");

    let older = DAEMON_TOML
        .split("\n[daemon]")
        .next()
        .expect("text")
        .to_owned();
    let mut exec = released(host(), Platform::Linux);
    put(&mut exec, KBF_TOML, 0o640, ROOT, &older);
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon));
    assert_eq!(text(&exec, KBF_TOML), DAEMON_TOML);
}

/// Catches the Keep and refuse rules not covering the new keys: a file with the operator's
/// settings and the keys is kept; one with the settings and no keys, or another directory in a
/// key, is refused (exit 5) naming the key and never printing the path, so the operator edits
/// it by hand; nothing is rewritten.
///
/// Not covered: the refusal message carrying the value; a missing key counted as a match.
#[test]
fn a_file_with_other_settings_is_kept_with_the_keys_and_refused_without_them() {
    let other = "\n[listener]\nstorage-listen = \"0.0.0.0:7000\"\n";
    let with = format!("{DAEMON_TOML}{other}");
    let mut exec = released(host(), Platform::Linux);
    put(&mut exec, KBF_TOML, 0o640, ROOT, &with);
    let out = go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon));
    assert_eq!(text(&exec, KBF_TOML), with);
    assert!(out.contains("other settings"), "{out}");

    let older = DAEMON_TOML.split("\n[daemon]").next().expect("text");
    let without = format!("{older}{other}");
    let elsewhere = with.replace(
        "update-dir = \"/var/lib/kbf-installer\"",
        "update-dir = \"/srv/secret-place\"",
    );
    for (name, file, named) in [
        ("missing", &without, ["update-dir", "orders-dir"].as_slice()),
        ("elsewhere", &elsewhere, ["update-dir"].as_slice()),
    ] {
        let mut exec = released(host(), Platform::Linux);
        put(&mut exec, KBF_TOML, 0o640, ROOT, file);
        let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon), APPLY).expect_err("refused");
        assert_eq!(err.exit(), Exit::NeedsOperator, "{name}");
        let said = err.to_string();
        for key in named {
            assert!(
                said.contains(&format!("[daemon] {key} differs")),
                "{name}: {said}"
            );
        }
        assert!(!said.contains("/srv/secret-place"), "{name}: {said}");
        assert!(!said.contains("/var/lib/kbf-installer"), "{name}: {said}");
        assert_eq!(&text(&exec, KBF_TOML), file, "{name}");
    }
}
