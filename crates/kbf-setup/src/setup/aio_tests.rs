//! Tests of `setup --mode=all-in-one`, the one-process host: its one unit and its one `kbf.toml`
//! (the golden, byte for byte), the files and accounts around them, and the refusal of a host that
//! an earlier build set up behind the proxy's two-slot supervision. The helpers are `tests.rs`'s
//! and `kbf_settings_tests.rs`'s. Each test says the behaviour it asserts.

use super::kbf_settings_tests::{all_in_one, host_for};
use super::*;
use crate::setup::kbf_settings::KbfFlags;

/// The `kbf.toml` that `all_in_one()` gets: the file `kbf --mode=all-in-one` reads.
const GOLDEN: &str = include_str!("../../tests/golden/kbf_toml/all-in-one.toml");
/// The `installer.toml` of an all-in-one host that an earlier build set up behind the proxy: the
/// slot table, and the `frontend_config` key that named the file of the proxy's children.
const PROXY_LAYOUT: &str = include_str!("../../tests/golden/all_in_one/installer.toml");
/// The `installer.toml` of a frontend host that an earlier build set up with `--proxy`: the slot
/// table and no `frontend_config`.
const PROXY_HOST: &str = include_str!("../../tests/golden/proxy_host/installer.toml");
/// The `kbf.toml` of the sibling that host ran beside its proxy.
const SIBLING: &str =
    "mode = \"storage-and-daemon\"\n\n[listener]\nstorage-listen = \"127.0.0.1:8990\"\n";
/// Root with the group of `kbf`: who owns the `kbf.toml` the one unit reads.
const GROUP_KBF: Owner = Owner { uid: 0, gid: 990 };
const UNIT: &str = "kbf-all-in-one.service";
const DRY: Options = Options {
    apply: false,
    no_start: false,
};

/// Asserts that an all-in-one host gets the `kbf.toml` of the golden byte for byte (root's, 0640,
/// the group of `kbf`), the operator token and the spool for `kbf`, the rootless prerequisites of
/// the account that runs the daemon half, and nothing of a proxy or a sibling: no
/// `frontend.toml`, no `frontend_config`, no `[proxy]` table, and the one unit enabled and not
/// started, with the note that says so.
#[test]
fn an_all_in_one_host_gets_its_one_unit_and_the_golden_kbf_toml() {
    let raw = all_in_one();
    let mut exec = host_for(&raw);
    let out = go_ok(&FakeOs::linux(), &mut exec, &raw);
    assert_eq!(text(&exec, KBF_TOML), GOLDEN);
    assert_eq!(file_meta(&exec, KBF_TOML), (0o640, GROUP_KBF));
    assert_eq!(text(&exec, API_TOKEN), format!("{SECRET}\n"));
    assert_eq!(file_meta(&exec, API_TOKEN), (0o600, KBF));
    let orders = exec.entries.get(&hp("/var/lib/kbf-installer/orders"));
    assert!(
        matches!(orders, Some(Entry::Dir { owner, .. }) if *owner == KBF),
        "{orders:?}"
    );

    let commands = cmds(&exec);
    for want in [
        "run /usr/sbin/usermod --add-subuids 100000-165535 -- kbf",
        "run /usr/bin/loginctl enable-linger kbf",
    ] {
        assert!(commands.contains(&want.to_owned()), "{want}");
    }
    let enables: Vec<&String> = commands
        .iter()
        .filter(|c| c.contains("systemctl enable"))
        .collect();
    assert_eq!(
        enables,
        ["run /usr/bin/systemctl enable -- kbf-all-in-one.service",]
    );
    assert!(
        out.contains("note: kbf-all-in-one.service is enabled, not started"),
        "{out}"
    );
    assert!(!out.contains("proxy"), "{out}");
    assert!(!out.contains(SECRET), "{out}");

    assert!(!exec.entries.contains_key(&hp("/etc/kbf/frontend.toml")));
    let installer = text(&exec, INSTALLER_TOML);
    let line = format!("database = \"{DATABASE}\"\n");
    assert!(installer.contains(&line), "{installer}");
    assert!(!installer.contains("frontend_config"), "{installer}");
    assert!(!installer.contains("proxy"), "{installer}");
}

/// Asserts that the files are written in an order a crash cannot turn into a host that starts
/// wrongly: the operator token and `installer.toml` first, then `kbf.toml`, which the unit waits
/// for, and no unit before it.
#[test]
fn kbf_toml_is_the_last_file_and_the_unit_comes_after_it() {
    let raw = all_in_one();
    let mut exec = host_for(&raw);
    go_ok(&FakeOs::linux(), &mut exec, &raw);
    let at = |path: &str| write_index(&exec, path).expect("written");
    let first_unit = exec
        .log
        .iter()
        .position(|s| matches!(s, Step::Run(c) if c.to_string().contains("systemctl")))
        .expect("a unit is enabled");
    assert!(at(API_TOKEN) < at(KBF_TOML));
    assert!(at(INSTALLER_TOML) < at(KBF_TOML));
    assert!(at(KBF_TOML) < first_unit);
}

/// Asserts that a rerun of the same flags on the finished host plans nothing and says so.
#[test]
fn a_rerun_of_an_all_in_one_host_changes_nothing() {
    let raw = all_in_one();
    let mut exec = host_for(&raw);
    go_ok(&FakeOs::linux(), &mut exec, &raw);
    let steps = exec.log.len();
    let out = go_ok(&settled(&[UNIT]), &mut exec, &raw);
    assert_eq!(exec.log.len(), steps, "{:?}", &exec.log[steps..]);
    assert!(out.contains("\nnothing to do\n"), "{out}");
}

/// Asserts that an all-in-one host of the proxy layout (its `installer.toml` names the
/// `frontend_config` that only that setup wrote, and its `kbf.toml` is the sibling's) is refused
/// with exit 5 for an apply and a dry run, saying what the host is and to leave it: nothing is
/// written, the server is not asked, both files are as they were, and the refusal comes before
/// the flags are checked (the flags here are none, which alone would be seven problems).
#[test]
fn an_all_in_one_host_of_the_proxy_layout_is_refused_before_any_change() {
    for options in [APPLY, DRY] {
        let raw = Raw {
            kbf: KbfFlags::default(),
            ..all_in_one()
        };
        let mut exec = host_for(&raw);
        put(&mut exec, INSTALLER_TOML, 0o644, ROOT, PROXY_LAYOUT);
        put(&mut exec, KBF_TOML, 0o640, GROUP_KBF, SIBLING);
        let server = Server::fresh();
        let os = FakeOs::linux();
        let err = go_on(&os, &mut exec, &server, &raw, options).expect_err("refused");
        let how = (err.exit(), err.code());
        assert_eq!(how, (Exit::NeedsOperator, "needs_operator"));
        let said = err.to_string();
        assert!(said.contains("names a frontend_config"), "{said}");
        assert!(said.contains("two-slot supervision"), "{said}");
        assert!(said.contains("leave this host as it is"), "{said}");
        assert!(!said.contains('\n'), "one line: {said}");
        assert!(exec.log.is_empty(), "{:?}", exec.log);
        assert!(server.asked.borrow().is_empty());
        assert_eq!(text(&exec, INSTALLER_TOML), PROXY_LAYOUT);
        assert_eq!(text(&exec, KBF_TOML), SIBLING);
    }
}

/// Asserts that the same `installer.toml` does not stop the modes that were never the proxy
/// layout: a frontend is set up over it and the file is kept byte for byte.
#[test]
fn only_an_all_in_one_run_is_refused_for_the_proxy_layout() {
    let mut exec = released(host(), Platform::Linux);
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, PROXY_LAYOUT);
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Frontend));
    assert_eq!(text(&exec, INSTALLER_TOML), PROXY_LAYOUT);
}

/// Asserts that an `installer.toml` that cannot be judged for the key is left to the checks that
/// follow, which refuse it as they do for any mode and before any change: a file that is not TOML
/// is told to be fixed or removed, and one that is not a regular file is named so.
#[test]
fn an_installer_toml_that_cannot_be_read_is_refused_as_for_any_mode() {
    let raw = all_in_one();
    let mut exec = host_for(&raw);
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, "= broken\n");
    let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    assert!(err.to_string().contains("or remove it"), "{err}");
    assert!(exec.log.is_empty(), "{:?}", exec.log);

    let mut exec = host_for(&raw);
    exec.entries.insert(
        hp(INSTALLER_TOML),
        Entry::Dir {
            perms: Perms::new(0o755),
            owner: ROOT,
        },
    );
    let err = go(&FakeOs::linux(), &mut exec, &raw, APPLY).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    assert!(err.to_string().contains("is not a regular file"), "{err}");
    assert!(exec.log.is_empty(), "{:?}", exec.log);
}

/// Asserts that an `installer.toml` with a `[proxy]` table and no `frontend_config` (a proxy over
/// frontends, not the removed layout) is kept byte for byte on an all-in-one host, which says so
/// and enables its one unit and no proxy unit.
#[test]
fn a_proxy_table_without_the_layouts_key_is_kept_and_the_one_unit_is_enabled() {
    let raw = all_in_one();
    let mut exec = host_for(&raw);
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, PROXY_HOST);
    let out = go_ok(&FakeOs::linux(), &mut exec, &raw);
    assert_eq!(text(&exec, INSTALLER_TOML), PROXY_HOST);
    assert!(out.contains("installer.toml has a [proxy] table"), "{out}");
    let enables: Vec<String> = cmds(&exec)
        .into_iter()
        .filter(|c| c.contains("enable -- "))
        .collect();
    let want = format!("run /usr/bin/systemctl enable -- {UNIT}");
    assert_eq!(enables, [want]);
}
