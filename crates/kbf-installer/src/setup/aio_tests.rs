//! Tests of `setup --mode=all-in-one`, which is always behind the proxy: the files of the proxy,
//! of its frontend children and of the `kbf-storage-and-daemon` sibling, the units, the one
//! `installer.toml` key that points the children at their file, `--storage-port`, and what tells
//! a host of the earlier layout (its move is `aio_migrate_tests.rs`'s). The helpers are
//! `tests.rs`'s. Each test says the defect it catches and names the mutants to plant to see it go
//! red; none is claimed to have run.

use super::proxy_tests::{proxy_host, proxy_raw};
use super::*;
use crate::setup::kbf_settings::KbfFlags;
use crate::setup::kbf_toml;
use crate::setup::plan::{PROXY_UNITS, mode_units};

/// The sibling's `kbf.toml` and the children's `frontend.toml` that `raw(Mode::AllInOne)` gets.
const SIBLING_GOLDEN: &str = include_str!("../../tests/golden/kbf_toml/all-in-one-sibling.toml");
const FRONTEND_GOLDEN: &str = include_str!("../../tests/golden/kbf_toml/all-in-one-frontend.toml");
/// The `installer.toml` of that host: a proxy host's, with the key that names the children's file.
const INSTALLER_GOLDEN: &str = include_str!("../../tests/golden/all_in_one/installer.toml");
/// The `kbf.toml` of the earlier layout (`mode = "all-in-one"`), which `kbf --check` still takes.
const EARLIER_KBF_TOML: &str = include_str!("../../tests/golden/kbf_toml/all-in-one.toml");
/// The line of `INSTALLER_GOLDEN` that a proxy host without a sibling does not have.
const FRONTEND_CONFIG_LINE: &str = "frontend_config = \"/etc/kbf/frontend.toml\"\n";
/// Root with the group of `kbf` and of `kbf-server` on the fake host: who owns the two files.
const GROUP_KBF: Owner = Owner { uid: 0, gid: 990 };
const GROUP_SERVER: Owner = Owner { uid: 0, gid: 989 };
const UNITS: [&str; 3] = [
    "kbf-proxy.socket",
    "kbf-proxy.service",
    "kbf-storage-and-daemon.service",
];
const DRY: Options = Options {
    apply: false,
    no_start: false,
};

fn some(text: &str) -> Option<String> {
    Some(text.to_owned())
}

/// A host with a release installed and the files `raw(Mode::AllInOne)` names.
fn aio_host() -> FakeExec {
    released(host(), Platform::Linux)
}

/// Puts `text` where the children's file is, as `setup` leaves it: root's, 0640, the group of
/// `kbf-server`.
fn children_file(exec: &mut FakeExec, text: &str) {
    put(exec, AIO_FRONTEND_TOML, 0o640, GROUP_SERVER, text);
}

/// Catches an all-in-one host set up as the earlier layout, or with the wrong file in the wrong
/// hands: the sibling's `kbf.toml` (a `storage-and-daemon` node on loopback, no enrollment, none
/// of the proxy's TLS files) is root's with the group of `kbf`, the children's `frontend.toml`
/// (the operator token, the state database, the sibling as storage node) is root's with the group
/// of `kbf-server`, `installer.toml` points the children at theirs, the operator token and the
/// spool belong to `kbf-server`, the sibling's account gets the rootless prerequisites, and the
/// output says what is started in what order and what a restart of the sibling costs. The files
/// are byte for byte the goldens.
///
/// Mutants to plant: the owners of the two files swapped; `[frontend]` or `api-listen` written
/// into the node file; the token or the spool owned by `kbf`; `frontend_config` left out of
/// `installer_settings`.
#[test]
fn an_all_in_one_host_gets_the_proxy_the_children_and_the_sibling() {
    let mut exec = aio_host();
    let out = go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::AllInOne));
    let sibling = text(&exec, KBF_TOML);
    assert_eq!(sibling, SIBLING_GOLDEN);
    for table in ["[frontend]", "[enroll]", "[tls]", "[scheduler]"] {
        assert!(!sibling.contains(table), "{table}");
    }
    assert_eq!(file_meta(&exec, KBF_TOML), (0o640, GROUP_KBF));
    assert_eq!(text(&exec, AIO_FRONTEND_TOML), FRONTEND_GOLDEN);
    assert_eq!(file_meta(&exec, AIO_FRONTEND_TOML), (0o640, GROUP_SERVER));
    assert_eq!(text(&exec, INSTALLER_TOML), INSTALLER_GOLDEN);
    assert_eq!(file_meta(&exec, INSTALLER_TOML), (0o644, ROOT));
    assert_eq!(text(&exec, API_TOKEN), format!("{SECRET}\n"));
    assert_eq!(file_meta(&exec, API_TOKEN), (0o600, KBF_SERVER));
    let orders = exec.entries.get(&hp("/var/lib/kbf-installer/orders"));
    assert!(
        matches!(orders, Some(Entry::Dir { owner, .. }) if *owner == KBF_SERVER),
        "{orders:?}"
    );

    let commands = cmds(&exec);
    for want in [
        "run /usr/sbin/usermod --add-subuids 100000-165535 -- kbf",
        "run /usr/bin/loginctl enable-linger kbf",
        "run /usr/bin/systemctl enable --now -- kbf-installer.service",
    ] {
        assert!(commands.contains(&want.to_owned()), "{want}");
    }
    let enables: Vec<&String> = commands
        .iter()
        .filter(|c| c.contains("enable -- "))
        .collect();
    assert_eq!(
        enables,
        [
            "run /usr/bin/systemctl enable -- kbf-proxy.socket",
            "run /usr/bin/systemctl enable -- kbf-proxy.service",
            "run /usr/bin/systemctl enable -- kbf-storage-and-daemon.service",
        ]
    );
    assert!(
        out.contains("start kbf-storage-and-daemon.service first"),
        "{out}"
    );
    assert!(out.contains("UNAVAILABLE"), "{out}");
    assert!(!out.contains(SECRET), "{out}");
}

/// Catches the files written in an order a crash can turn into a host that starts wrongly:
/// `installer.toml` (which names the children's file) first, then `frontend.toml`, then
/// `kbf.toml` last, which the sibling's unit waits for, and no unit before it.
///
/// Mutants to plant: `frontend.toml` planned after `kbf.toml`; the units before the files.
#[test]
fn kbf_toml_is_the_last_file_and_the_units_come_after_it() {
    let mut exec = aio_host();
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::AllInOne));
    let at = |path: &str| write_index(&exec, path).expect("written");
    let first_unit = exec
        .log
        .iter()
        .position(|s| matches!(s, Step::Run(c) if c.to_string().contains("systemctl")))
        .expect("a unit is enabled");
    assert!(at(INSTALLER_TOML) < at(AIO_FRONTEND_TOML));
    assert!(at(AIO_FRONTEND_TOML) < at(KBF_TOML));
    assert!(at(KBF_TOML) < first_unit);
}

/// Catches a rerun that does work, or a file whose text is right and whose owner is not being
/// trusted: the same flags on the finished host plan nothing, and the two files with their
/// owners swapped are put right with one step each.
///
/// Mutants to plant: the owner left out of the comparison of a file; a file written on every run.
#[test]
fn a_rerun_changes_nothing_and_a_file_with_the_wrong_owner_is_corrected() {
    let mut exec = aio_host();
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::AllInOne));
    let steps = exec.log.len();
    let os = settled(&UNITS);
    let out = go_ok(&os, &mut exec, &raw(Mode::AllInOne));
    assert_eq!(exec.log.len(), steps, "{:?}", &exec.log[steps..]);
    assert!(out.contains("\nnothing to do\n"), "{out}");

    for (path, owner, golden) in [
        (KBF_TOML, GROUP_SERVER, SIBLING_GOLDEN),
        (AIO_FRONTEND_TOML, GROUP_KBF, FRONTEND_GOLDEN),
    ] {
        put(&mut exec, path, 0o640, owner, golden);
    }
    go_ok(&os, &mut exec, &raw(Mode::AllInOne));
    assert_eq!(exec.log.len(), steps + 2, "{:?}", &exec.log[steps..]);
    assert_eq!(file_meta(&exec, KBF_TOML), (0o640, GROUP_KBF));
    assert_eq!(file_meta(&exec, AIO_FRONTEND_TOML), (0o640, GROUP_SERVER));
}

/// Catches a dry run that writes, or one that leaves a file out of the plan: both files are
/// named with `DRY-RUN:` and the host is as it was.
///
/// Mutant to plant: the dry run applying a step.
#[test]
fn a_dry_run_names_both_files_and_changes_nothing() {
    let mut exec = aio_host();
    let out = go(&FakeOs::linux(), &mut exec, &raw(Mode::AllInOne), DRY).expect("dry run");
    for file in [KBF_TOML, AIO_FRONTEND_TOML] {
        assert!(out.contains(&format!("DRY-RUN: write {file}")), "{out}");
    }
    assert!(exec.log.is_empty(), "{:?}", exec.log);
}

/// Catches the test of the earlier layout taking more than it should, or less: only a TOML file
/// whose top-level `mode` is `all-in-one` is it. Another mode, a mode that is not text, no mode,
/// and a text that is not TOML are not. A run of another mode over such a file does not move it:
/// the file is decided on as any other (here it holds the mode only, so the daemon's is written).
///
/// Mutants to plant: the mode compared by prefix; a file that does not parse counted as it; the
/// move made for every mode (red: no daemon file, and a copy appears).
#[test]
fn only_a_toml_file_with_the_mode_all_in_one_is_the_earlier_layout() {
    assert!(kbf_toml::all_in_one_table("mode = \"all-in-one\"\n").is_some());
    assert!(kbf_toml::all_in_one_table(EARLIER_KBF_TOML).is_some());
    for other in [
        "mode = \"storage-and-daemon\"\n",
        "mode = \"all-in-one-x\"\n",
        "mode = 3\n",
        "[daemon]\ndriver = \"container\"\n",
        "mode = [",
        "",
    ] {
        assert!(kbf_toml::all_in_one_table(other).is_none(), "{other:?}");
    }

    let mut exec = aio_host();
    put(&mut exec, KBF_TOML, 0o640, ROOT, "mode = \"all-in-one\"\n");
    go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::Daemon));
    assert_eq!(text(&exec, KBF_TOML), DAEMON_TOML);
    let copy = "/var/lib/kbf-installer/migration-units/kbf.toml.all-in-one";
    assert!(!exec.entries.contains_key(&hp(copy)));
}

/// Catches the unit list of the proxy layout drifting from the proxy's: an all-in-one host behind
/// the proxy has the proxy's two units, in their order, then the sibling; without the proxy the
/// earlier layout's unit, which hosts that have not moved still run.
///
/// Mutants to plant: the sibling first; the proxy's units left out; the earlier layout's unit
/// returned for the proxy layout.
#[test]
fn the_units_of_an_all_in_one_host_are_the_proxys_then_the_sibling() {
    let units = mode_units(Mode::AllInOne, true);
    assert_eq!(&units[..2], PROXY_UNITS);
    assert_eq!(units, UNITS);
    assert_eq!(
        mode_units(Mode::AllInOne, false),
        ["kbf-all-in-one.service"]
    );
}

/// Catches `--storage-port` lost on the way to a file, or the default drifting: the port is the
/// sibling's `storage-listen` and the address the children dial, both from the one flag, and
/// without it both say 8990 (the port the documentation and the packaged examples use). A
/// frontend host has no such port.
///
/// Mutants to plant: the port written to one of the files only; the default changed.
#[test]
fn the_storage_port_is_the_siblings_listener_and_the_childrens_storage_node() {
    let mut flags = raw(Mode::AllInOne);
    flags.proxy.storage_port = Some(9000);
    let mut exec = aio_host();
    go_ok(&FakeOs::linux(), &mut exec, &flags);
    assert_eq!(
        text(&exec, KBF_TOML),
        SIBLING_GOLDEN.replace("8990", "9000")
    );
    assert_eq!(
        text(&exec, AIO_FRONTEND_TOML),
        FRONTEND_GOLDEN.replace("8990", "9000")
    );

    let valid = raw(Mode::AllInOne).validate(Platform::Linux);
    assert_eq!(valid.expect("valid").storage_port(), Some(8990));
    let frontend = proxy_raw().validate(Platform::Linux);
    assert_eq!(frontend.expect("valid").storage_port(), None);
}

/// Catches a port that cannot work being accepted, and `--storage-port` where there is no
/// sibling: a port below 1024, the proxy's public socket port or its operator API port, or one of
/// the children's (either slot, at both edges), is refused naming the flag; the ports beside those
/// ranges and the highest port pass; an invalid base port is refused for itself and does not
/// overflow the check; a frontend host and a host without `--proxy` refuse the flag, and an
/// all-in-one host without `--proxy` is refused for that.
///
/// Mutants to plant: the lower bound 1024 to 1023; the proxy's ports left out of the refusal (red:
/// 8980 and 8982 pass); slot B left out of the cover test; the end of a slot's range off by one;
/// the all-in-one refusal of a missing `--proxy` dropped.
#[test]
fn the_storage_port_is_checked_and_belongs_to_all_in_one() {
    const RANGE: &str = "--storage-port: must be 1024 to 65535";
    const COVER: &str = "--storage-port: the children's ports would cover it";
    const PROXYS: &str = "--storage-port: the public socket or the operator API uses it";
    let check = |port: u16, base: Option<u16>| {
        let mut flags = raw(Mode::AllInOne);
        flags.proxy.storage_port = Some(port);
        flags.proxy.proxy_base_port = base;
        flags
            .validate(Platform::Linux)
            .map(|_| ())
            .map_err(|p| p.join("; "))
    };
    for ok in [1024, 8990, 9000, 18_005, 18_009, 18_015, 65_535] {
        assert!(check(ok, None).is_ok(), "{ok}: {:?}", check(ok, None));
    }
    for (bad, base, rule) in [
        (0, None, RANGE),
        (1023, None, RANGE),
        (8_980, None, PROXYS),
        (8_982, None, PROXYS),
        (18_000, None, COVER),
        (18_004, None, COVER),
        (18_010, None, COVER),
        (18_014, None, COVER),
        (8_997, Some(8_995), COVER),
        (65_535, Some(65_521), COVER),
    ] {
        let found = check(bad, base).expect_err("refused");
        assert!(found.contains(rule), "{bad}: {found}");
    }
    let found = check(8990, Some(65_535)).expect_err("refused");
    assert!(found.contains("must be 1024 to 65521"), "{found}");
    assert!(!found.contains("--storage-port"), "{found}");

    let mut frontend = proxy_raw();
    frontend.proxy.storage_port = Some(9000);
    let found = frontend.validate(Platform::Linux).expect_err("refused");
    let found = found.join("; ");
    assert!(
        found.contains("--storage-port is for --mode=all-in-one"),
        "{found}"
    );

    let mut bare = raw(Mode::AllInOne);
    bare.proxy.proxy = false;
    let found = bare.validate(Platform::Linux).expect_err("refused");
    let found = found.join("; ");
    assert!(found.contains("--proxy is required"), "{found}");
    bare.proxy.storage_port = Some(9000);
    let found = bare.validate(Platform::Linux).expect_err("refused");
    let found = found.join("; ");
    assert!(found.contains("--storage-port is for --proxy"), "{found}");
}

/// Catches the listener flags accepted on an all-in-one host, where nothing would read them (the
/// proxy owns the public addresses and gives its children theirs): each is refused by name, as
/// on a frontend behind the proxy.
///
/// Mutants to plant: a flag removed from `PROXY_REFUSED` (red: the refusal says the mode does not
/// take it instead). A `Takes` entry moved back to all-in-one is caught by `kbf_settings_tests`
/// (the row count, and no `api-listen` in either file), not here.
#[test]
fn the_listener_flags_are_refused_on_an_all_in_one_host() {
    type Edit = fn(&mut KbfFlags);
    let edits: [(&str, Edit); 5] = [
        ("--listen", |k| k.listen = some("0.0.0.0:8980")),
        ("--worker-listen", |k| {
            k.worker_listen = some("0.0.0.0:8981")
        }),
        ("--api-listen", |k| k.api_listen = some("127.0.0.1:8982")),
        ("--worker-tls-cert", |k| {
            k.worker_tls_cert = some("/etc/kbf/worker.pem")
        }),
        ("--worker-tls-key", |k| {
            k.worker_tls_key = some("/etc/kbf/worker.key")
        }),
    ];
    for (flag, edit) in edits {
        let mut flags = raw(Mode::AllInOne);
        edit(&mut flags.kbf);
        let found = flags
            .validate(Platform::Linux)
            .expect_err("refused")
            .join("; ");
        assert!(
            found.contains(&format!("{flag} is not taken with --proxy")),
            "{flag}: {found}"
        );
    }
}

/// Catches a host set up without the proxy's user or the sibling's: the package makes both
/// accounts, and an all-in-one host needs both, so each missing one is named before any write.
///
/// Mutant to plant: the proxy's user not looked up (red: the second run goes on).
#[test]
fn an_all_in_one_host_needs_both_accounts() {
    for name in ["kbf", "kbf-server"] {
        let mut os = FakeOs::linux();
        os.users.retain(|u| u.name != name);
        let mut exec = host();
        let err = go(&os, &mut exec, &raw(Mode::AllInOne), APPLY);
        assert!(
            matches!(&err, Err(SetupError::NoUser { name: n }) if *n == name),
            "{err:?}"
        );
        assert!(exec.log.is_empty(), "{name}");
    }
}

/// Catches `frontend_config` kept after the host stops being an all-in-one one: the flags decide
/// the key as they decide `database`, so a frontend run over the all-in-one `installer.toml`
/// leaves the proxy host's file, without the line that would point its children at a file
/// `kbf.toml` no longer makes room for.
///
/// Mutant to plant: `frontend_config` kept from the file that is there.
#[test]
fn a_frontend_run_drops_the_childrens_file_from_installer_toml() {
    let mut exec = proxy_host();
    put(&mut exec, INSTALLER_TOML, 0o644, ROOT, INSTALLER_GOLDEN);
    go_ok(&FakeOs::linux(), &mut exec, &proxy_raw());
    assert_eq!(
        text(&exec, INSTALLER_TOML),
        INSTALLER_GOLDEN.replace(FRONTEND_CONFIG_LINE, "")
    );
}

/// Catches the children's file being outside the rules of `kbf.toml`: a `frontend.toml` that
/// holds the operator's own settings is kept when the keys setup writes match (said in the
/// output) and refused (exit 5) when one differs, naming the file and the key and never its
/// value, and nothing is written after it.
///
/// Mutants to plant: the file decided as if it were `kbf.toml`'s text; the refusal quoting the
/// storage address.
#[test]
fn the_childrens_file_follows_the_keep_and_refuse_rules_of_kbf_toml() {
    let theirs = format!("{FRONTEND_GOLDEN}\n[log]\nlog-files = 3\n");
    let mut exec = aio_host();
    children_file(&mut exec, &theirs);
    let out = go_ok(&FakeOs::linux(), &mut exec, &raw(Mode::AllInOne));
    assert_eq!(text(&exec, AIO_FRONTEND_TOML), theirs);
    assert!(out.contains("frontend.toml holds other settings"), "{out}");

    let moved = theirs.replace("127.0.0.1:8990", "127.0.0.1:9999");
    let mut exec = aio_host();
    children_file(&mut exec, &moved);
    let err = go(&FakeOs::linux(), &mut exec, &raw(Mode::AllInOne), APPLY).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    let said = err.to_string();
    assert!(
        said.contains("/etc/kbf/frontend.toml holds other settings"),
        "{said}"
    );
    assert!(
        said.contains("[storage-client] storage differs from its flag"),
        "{said}"
    );
    assert!(!said.contains("9999"), "{said}");
    assert_eq!(text(&exec, AIO_FRONTEND_TOML), moved);
    assert!(write_index(&exec, KBF_TOML).is_none());
}
