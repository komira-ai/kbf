//! Tests of what `setup` does about the proxy now that the two-slot supervision is removed: it
//! writes no proxy table and enables no proxy unit, and keeps a `[proxy]` table that
//! `installer.toml` already has: the table is read and written back in the canonical form with
//! every key kept, so a file that is in that form is the same bytes. The proxy's own flags are
//! refused before `setup` runs (`raw_tests.rs`), and an all-in-one host of the removed proxy
//! layout is refused by `aio_tests.rs`. The helpers are `tests.rs`'s. Each test says the
//! behaviour it asserts.

use super::*;
use crate::installer_toml::InstallerToml;

/// The `installer.toml` of a frontend host that an earlier build set up with the removed
/// `--proxy`: the slot table (`[proxy.upstream]`), the operator's key `hold_max_s` at its default
/// and the release source of a file server, in the form `render` writes.
const PROXY_GOLDEN: &str = include_str!("../../tests/golden/proxy_host/installer.toml");

/// The `installer.toml` of an all-in-one host that an earlier build set up: the same, and the
/// `frontend_config` key that named the file the proxy's children read.
const ALL_IN_ONE_GOLDEN: &str = include_str!("../../tests/golden/all_in_one/installer.toml");

fn systemctl(exec: &FakeExec) -> Vec<String> {
    cmds(exec)
        .into_iter()
        .filter(|c| c.contains("systemctl"))
        .collect()
}

/// The units a frontend host is set up with: its own and the installer's, and no proxy unit.
const FRONTEND_UNITS: [&str; 2] = [
    "run /usr/bin/systemctl enable -- kbf-frontend.service",
    "run /usr/bin/systemctl enable --now -- kbf-installer.service",
];

/// Asserts that a frontend host that has no `[proxy]` table gets none, and the unit of a frontend
/// and not a proxy's: setup writes no proxy table and enables no proxy unit.
#[test]
fn a_frontend_host_gets_no_proxy_table_and_no_proxy_unit() {
    let mut exec = released(host(), Platform::Linux);
    let out = go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Frontend));
    assert!(!text(&exec, INSTALLER_TOML).contains("proxy"));
    assert_eq!(systemctl(&exec), FRONTEND_UNITS);
    assert!(!out.contains("[proxy] table"), "{out}");
}

/// Asserts that an `installer.toml` of an earlier build, with the slot table or the keys of an
/// all-in-one host, still parses and is not rewritten: after a setup without any proxy flag the
/// file is the same bytes, no write of it is planned, setup says it kept the table (the slot
/// table included, which only the proxy refuses), and it enables the frontend unit and no proxy
/// unit.
#[test]
fn a_proxy_table_that_is_there_is_kept_byte_for_byte() {
    for (name, golden) in [
        ("proxy host", PROXY_GOLDEN),
        ("all-in-one host", ALL_IN_ONE_GOLDEN),
    ] {
        assert!(golden.contains("\n[proxy.upstream]\n"), "{name}");
        InstallerToml::parse(golden).expect("an earlier build's file still parses");
        let mut exec = released(host(), Platform::Linux);
        put(&mut exec, INSTALLER_TOML, 0o644, ROOT, golden);
        let out = go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Frontend));
        assert_eq!(text(&exec, INSTALLER_TOML), golden, "{name}");
        assert!(write_index(&exec, INSTALLER_TOML).is_none(), "{name}");
        assert_eq!(systemctl(&exec), FRONTEND_UNITS, "{name}");
        assert!(out.contains("installer.toml has a [proxy] table"), "{out}");
    }
}

/// Asserts that a table an operator wrote by hand for the pool (one `[[proxy.frontend]]` table
/// for each frontend, in place of the slot table) is kept byte for byte too, whatever else the
/// table holds, and that a rerun over it plans no write of the file.
#[test]
fn a_hand_written_pool_table_is_kept_byte_for_byte() {
    let slot = "[proxy.upstream]\nbase_port = 18000";
    let frontends = "[[proxy.frontend]]\nname = \"a\"\nbase_port = 18000\n\n\
                     [[proxy.frontend]]\nname = \"b\"\nbase_port = 18010";
    assert!(PROXY_GOLDEN.contains(slot));
    let pool = PROXY_GOLDEN
        .replacen(slot, frontends, 1)
        .replace("hold_max_s = 20", "hold_max_s = 33");
    let parsed = InstallerToml::parse(&pool).expect("the pool table parses");
    assert_eq!(parsed.proxy.as_ref().map(|p| p.frontend.len()), Some(2));
    assert_eq!(parsed.render(), pool);
    let mut exec = released(host(), Platform::Linux);
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, &pool);
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Frontend));
    assert_eq!(text(&exec, INSTALLER_TOML), pool);
    assert!(write_index(&exec, INSTALLER_TOML).is_none());
}
