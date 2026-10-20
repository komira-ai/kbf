//! The node's `update-dir` and `orders-dir` added to an adopted host's `kbf.toml`.

use std::fs;
use std::os::unix::fs::MetadataExt;

use super::*;
use crate::exit::Exit;
use crate::migrate::rig::*;

const UPDATE: &str = "/var/lib/kbf-installer";
const ORDERS: &str = "/var/lib/kbf-installer/orders";

/// What the old tools left: comments, an order that is not alphabetical, another table.
const NODE: &str = "\
# kept by hand
mode = \"daemon\"

[identity]
server = \"https://frontend.example.invalid:8981\"
node-id = \"n1\"

[daemon]
# the driver of this host
driver = [\"native\"]
data-dir = \"/var/lib/kbf\"
";

fn kbf_toml(rig: &Rig) -> std::path::PathBuf {
    rig.at("etc/kbf/kbf.toml")
}

fn put_toml(rig: &Rig, text: &str) {
    put(&kbf_toml(rig), text, 0o640);
    fs::create_dir_all(rig.new_state_dir().join("migration-units")).expect("backup dir");
}

fn run(rig: &mut Rig, mode: Mode) -> (Outcome, Report) {
    let m = rig.migration();
    let mut report = Report::new(super::super::Outcome::Migrated);
    let outcome = add(&m, &mut rig.host, mode, &mut report).expect("add");
    (outcome, report)
}

fn read(rig: &Rig) -> String {
    fs::read_to_string(kbf_toml(rig)).expect("kbf.toml")
}

fn keys(text: &str) -> (Option<String>, Option<String>) {
    let table: Table = text.parse().expect("toml");
    let get = |k: &str| value(&table, k).expect("text").map(str::to_owned);
    (get("update-dir"), get("orders-dir"))
}

/// Catches the keys landing anywhere but the `[daemon]` table, any other byte of the file moving
/// (a comment, another table, the order), a lost file mode (the node reads it as group `kbf`:
/// 0640), or the order spool missing or not 0700 (the node cannot make it inside root's
/// directory, and does not start without it).
///
/// Mutants seen red: the keys appended in a table of their own when `[daemon]` exists (TOML
/// refuses the second table); `Perms::new(0o755)` for the spool.
#[test]
fn an_adopted_node_gets_both_keys_in_its_daemon_table_and_nothing_else_moves() {
    let mut rig = Rig::linux("keys-added");
    put_toml(&rig, NODE);
    let (outcome, report) = run(&mut rig, Mode::Apply);
    assert_eq!(outcome, Outcome::Added);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let now = read(&rig);
    assert_eq!(
        keys(&now),
        (Some(UPDATE.to_owned()), Some(ORDERS.to_owned()))
    );
    // Take the two lines out and the file is the old one, byte for byte.
    let without: String = now
        .lines()
        .filter(|l| !l.starts_with("update-dir") && !l.starts_with("orders-dir"))
        .map(|l| format!("{l}\n"))
        .collect();
    assert_eq!(without, NODE);
    assert!(now.find("[daemon]") < now.find("update-dir"));
    assert!(now.find("update-dir") < now.find("# the driver"));
    let meta = fs::metadata(kbf_toml(&rig)).expect("meta");
    assert_eq!(meta.mode() & 0o7777, 0o640);
    let spool = fs::metadata(rig.at("var/lib/kbf-installer/orders")).expect("orders");
    assert!(spool.is_dir());
    assert_eq!(spool.mode() & 0o7777, 0o700);
    assert_eq!(spool.uid(), me());
}

/// Catches a second run adding the keys again (a duplicate key is a file `kbf` refuses), or
/// writing a file that is merely equal: the second run must say they are there and touch
/// nothing (the file's modification time and bytes stay).
///
/// Mutant seen red: a key that is there already counted as missing.
#[test]
fn the_keys_are_added_once() {
    let mut rig = Rig::linux("keys-once");
    put_toml(&rig, NODE);
    assert_eq!(run(&mut rig, Mode::Apply).0, Outcome::Added);
    let (bytes, mtime) = (
        read(&rig),
        fs::metadata(kbf_toml(&rig))
            .expect("meta")
            .modified()
            .expect("mtime"),
    );
    let (outcome, report) = run(&mut rig, Mode::Apply);
    assert_eq!(outcome, Outcome::Already);
    assert_eq!(read(&rig), bytes);
    assert_eq!(
        fs::metadata(kbf_toml(&rig))
            .expect("meta")
            .modified()
            .expect("mtime"),
        mtime
    );
    assert!(report.to_string().contains("are there"), "{report}");
    assert_eq!(bytes.matches("update-dir").count(), 1);
    assert_eq!(bytes.matches("orders-dir").count(), 1);
}

/// Catches the keys written for a mode whose daemon refuses them (`all-in-one`'s in-process
/// daemon, `--update-dir` is refused at start) or for a mode with no daemon (frontend,
/// storage): a node that would not start, or keys for nothing. Daemon and storage-and-daemon
/// are the two that take them. Each file is left as it was, and no spool is made, where not.
///
/// Mutant seen red: `takes_orders` true for `all-in-one`.
#[test]
fn only_the_modes_whose_daemon_takes_orders_get_the_keys() {
    for (mode, takes) in [
        ("daemon", true),
        ("storage-and-daemon", true),
        ("all-in-one", false),
        ("frontend", false),
        ("storage", false),
    ] {
        let mut rig = Rig::linux(&format!("keys-mode-{mode}"));
        let text = format!("mode = \"{mode}\"\n[daemon]\ndriver = [\"native\"]\n");
        put_toml(&rig, &text);
        let (outcome, _) = run(&mut rig, Mode::Apply);
        if takes {
            assert_eq!(outcome, Outcome::Added, "{mode}");
            assert!(read(&rig).contains(UPDATE), "{mode}");
        } else {
            assert_eq!(outcome, Outcome::NotANode, "{mode}");
            assert_eq!(read(&rig), text, "{mode}");
            assert!(!rig.at("var/lib/kbf-installer/orders").exists(), "{mode}");
        }
    }
}

/// Catches the table-edit shapes that corrupt a file: no `[daemon]` table (a new one is
/// appended, after a missing final newline too), a header with spaces and a comment, and a file where `daemon` is written as
/// dotted keys or an inline table (a second `[daemon]` would be refused by TOML: the file is
/// left as it was and the migration says so).
///
/// Mutant seen red: the header never matched (a second `[daemon]` table is refused by TOML).
#[test]
fn the_edit_is_made_where_toml_allows_it_and_otherwise_not_at_all() {
    let cases: [(&str, &str, bool); 6] = [
        (
            "no table",
            "mode = \"daemon\"\n[identity]\nnode-id = \"n\"\n",
            true,
        ),
        ("no final newline", "mode = \"daemon\"", true),
        (
            "spaced header",
            "mode = \"daemon\"\n[ daemon ]  # mine\ndriver = [\"native\"]\n",
            true,
        ),
        (
            "a sibling table of a longer name",
            "mode = \"daemon\"\n[daemon-extra]\nx = 1\n",
            true,
        ),
        (
            "dotted keys",
            "mode = \"daemon\"\ndaemon.driver = [\"native\"]\n",
            false,
        ),
        (
            "inline table",
            "mode = \"daemon\"\ndaemon = { driver = [\"native\"] }\n",
            false,
        ),
    ];
    for (name, text, edited) in cases {
        let mut rig = Rig::linux(&format!("shape-{}", name.replace(' ', "-")));
        put_toml(&rig, text);
        let (outcome, report) = run(&mut rig, Mode::Apply);
        if edited {
            assert_eq!(outcome, Outcome::Added, "{name}");
            let now = read(&rig);
            assert_eq!(keys(&now).0.as_deref(), Some(UPDATE), "{name}");
            let table: Table = now.parse().expect("still TOML");
            let was: Table = text.parse().expect("toml");
            let both = [
                ("update-dir", UPDATE.to_owned()),
                ("orders-dir", ORDERS.to_owned()),
            ];
            assert!(only_the_keys_added(&was, &now, &both), "{name}: {table:?}");
        } else {
            assert!(matches!(outcome, Outcome::Left(_)), "{name}: {outcome:?}");
            assert_eq!(read(&rig), text, "{name}");
            assert!(
                report
                    .warnings
                    .iter()
                    .any(|w| w.contains("updates by hand")),
                "{:?}",
                report.warnings
            );
            assert!(!rig.at("var/lib/kbf-installer/orders").exists(), "{name}");
        }
    }
}

/// Catches an operator's choice overwritten: a key naming another directory is left, and the
/// other key is not added either (a half pair would point the spool at one place and the
/// status at another); a file that is not TOML, or has `daemon` as text, is left, each with a
/// warning that the node updates by hand. No `kbf.toml` at all is not an error (setup writes
/// it).
///
/// Mutant seen red: a key naming another directory counted as the layout's.
#[test]
fn what_the_operator_wrote_is_left_and_said() {
    let elsewhere = "mode = \"daemon\"\n[daemon]\nupdate-dir = \"/srv/updates\"\n";
    for (name, text) in [
        ("elsewhere", elsewhere),
        ("not toml", "mode = \n"),
        ("text daemon", "mode = \"daemon\"\ndaemon = \"x\"\n"),
        ("number", "mode = \"daemon\"\n[daemon]\norders-dir = 4\n"),
    ] {
        let mut rig = Rig::linux(&format!("left-{}", name.replace(' ', "-")));
        put_toml(&rig, text);
        let (outcome, report) = run(&mut rig, Mode::Apply);
        assert!(matches!(outcome, Outcome::Left(_)), "{name}: {outcome:?}");
        assert_eq!(read(&rig), text, "{name}");
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("updates by hand")),
            "{name}: {:?}",
            report.warnings
        );
    }
    let mut rig = Rig::linux("left-absent");
    assert_eq!(run(&mut rig, Mode::Apply).0, Outcome::NoFile);
}

/// Catches a dry run that writes: it makes no spool, writes no file and keeps no original, and
/// still says what an apply would do (the line is marked DRY-RUN).
///
/// Mutant seen red: the `mode == Mode::Apply` test dropped.
#[test]
fn a_dry_run_changes_nothing_and_says_what_it_would_do() {
    let mut rig = Rig::linux("keys-dry");
    put_toml(&rig, NODE);
    let before = rig.snapshot("");
    let (outcome, report) = run(&mut rig, Mode::DryRun);
    assert_eq!(outcome, Outcome::Added);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.starts_with("DRY-RUN:") && l.contains("update-dir")),
        "{report}"
    );
    assert_eq!(rig.snapshot(""), before);
}

/// Catches an adopted host with no user to own the spool getting keys anyway: a node pointed
/// at a spool it cannot make does not start. The file is left and the warning says why.
///
/// Mutant seen red: the file's owner (root) made the spool's owner when there is no user.
#[test]
fn a_linux_host_without_the_node_user_is_left_alone() {
    let mut rig = Rig::linux("keys-nouser");
    put_toml(&rig, NODE);
    let m = Migration {
        node_user: None,
        ..rig.migration()
    };
    let mut report = Report::new(super::super::Outcome::Migrated);
    let outcome = add(&m, &mut rig.host, Mode::Apply, &mut report).expect("add");
    assert!(matches!(outcome, Outcome::Left(_)), "{outcome:?}");
    assert_eq!(read(&rig), NODE);
    assert!(!rig.at("var/lib/kbf-installer/orders").exists());
    assert!(
        report.warnings[0].contains("\"kbf\""),
        "{:?}",
        report.warnings
    );
}

/// Catches a whole migration that leaves a node without the keys, or one that adds them after
/// the node is started again (the node reads them at start): the migrated host has them, and
/// the daemon's unit is started after the file is written.
///
/// Mutant seen red: the call to `add` removed from the migration.
#[test]
fn a_migrated_host_has_the_keys_before_its_node_starts() {
    let mut rig = Rig::linux("keys-migrate");
    put(&kbf_toml(&rig), NODE, 0o640);
    let at = kbf_toml(&rig);
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    rig.on_command({
        let seen = seen.clone();
        move |verb, unit| {
            if verb == "start" && unit == "kbf-daemon.service" {
                seen.borrow_mut()
                    .push(fs::read_to_string(&at).expect("kbf.toml"));
            }
        }
    });
    let never = FnVerifier(Box::new(|_, _| Err("no pipeline here".to_owned())));
    let report = run_migrate(&mut rig, &never, Mode::Apply).expect("migrate");
    assert_eq!(report.outcome, super::super::Outcome::Migrated);
    assert_eq!(seen.borrow().len(), 1);
    assert_eq!(keys(&seen.borrow()[0]).0.as_deref(), Some(UPDATE));
    assert!(
        report
            .to_string()
            .contains("update-dir and orders-dir added"),
        "{report}"
    );
    // A second migrate is the no-op it always was, and the file is as the first left it.
    let after = read(&rig);
    let again = run_migrate(&mut rig, &never, Mode::Apply).expect("again");
    assert_eq!(again.outcome, super::super::Outcome::AlreadyMigrated);
    assert_eq!(read(&rig), after);
}

/// Catches a rollback that leaves keys an old `kbf` may refuse: the file is the old one again,
/// byte for byte, with its mode; and a file the operator changed after the migration is not
/// overwritten (the rollback says so).
///
/// Mutant seen red: the restore skipping the "still the original plus the keys" check.
#[test]
fn a_rollback_puts_the_original_back_unless_the_operator_changed_it() {
    let never = FnVerifier(Box::new(|_, _| Err("no pipeline here".to_owned())));
    let mut rig = Rig::linux("keys-rollback");
    put(&kbf_toml(&rig), NODE, 0o640);
    run_migrate(&mut rig, &never, Mode::Apply).expect("migrate");
    assert!(read(&rig).contains(UPDATE));
    let mut m = rig.migration();
    m.installer_unit = None;
    let report = crate::migrate::rollback(&m, &mut rig.host, Mode::Apply).expect("rollback");
    assert_eq!(read(&rig), NODE);
    assert_eq!(
        fs::metadata(kbf_toml(&rig)).expect("meta").mode() & 0o7777,
        0o640
    );
    assert!(
        report.to_string().contains("the original is back"),
        "{report}"
    );

    let mut rig = Rig::linux("keys-rollback-edited");
    put(&kbf_toml(&rig), NODE, 0o640);
    run_migrate(&mut rig, &never, Mode::Apply).expect("migrate");
    let edited = format!("{}\n[extra]\nx = 1\n", read(&rig));
    fs::write(kbf_toml(&rig), &edited).expect("edit");
    let mut m = rig.migration();
    m.installer_unit = None;
    let report = crate::migrate::rollback(&m, &mut rig.host, Mode::Apply).expect("rollback");
    assert_eq!(read(&rig), edited);
    assert!(
        report.warnings.iter().any(|w| w.contains("left as it is")),
        "{:?}",
        report.warnings
    );
}

/// Catches migrate writing a Mac's `kbf.toml` (G9: `kbf-mac-provision` is its one writer), on a
/// kit Mac and on a Mac the kit never touched, and a check that lets a wrong file pass. The
/// file with `update-dir` (and `orders-dir` absent or the layout's) is left byte for byte, no
/// spool is made and no original is kept; a file without `update-dir`, or with a key that is not
/// the layout's or not text, is refused (exit 5) naming the key and `kbf-mac-provision`, and
/// no path, in a dry run and in an apply, before anything is written. A Linux node still gets
/// both keys (`an_adopted_node_gets_both_keys_in_its_daemon_table_and_nothing_else_moves`).
///
/// Mutants seen red: a Mac written like a Linux host; the check dropped; `orders-dir` required.
#[test]
fn a_mac_kbf_toml_is_checked_and_never_written() {
    use crate::migrate::kit_host::{Form, KBF_TOML_PATH, at};
    let provisioned = |orders: &str| {
        format!(
            "mode = \"daemon\"\n\n[daemon]\nupdate-dir = \"/var/db/kbf-installer\"\n{orders}driver = [\"native\"]\n"
        )
    };
    for (name, text) in [
        ("absent", provisioned("")),
        (
            "equal",
            provisioned("orders-dir = \"/var/db/kbf-installer/orders\"\n"),
        ),
    ] {
        for mode in [Mode::DryRun, Mode::Apply] {
            // A kit Mac, whose file is the kit fixture.
            let mut rig = Rig::kit(
                &format!("keys-mac-{name}-{mode:?}"),
                crate::layout::Platform::MacOs,
                Form::New,
            );
            fs::create_dir_all(rig.new_state_dir().join("migration-units")).expect("dir");
            let path = at(&rig.host.root, KBF_TOML_PATH);
            let before = fs::read(&path).expect("kbf.toml");
            let (outcome, report) = run(&mut rig, mode);
            assert_eq!(outcome, Outcome::Provisioned, "{name} {mode:?}");
            assert!(
                report
                    .to_string()
                    .contains("as kbf-mac-provision renders them")
            );
            assert_eq!(fs::read(&path).expect("kbf.toml"), before);
            assert!(!rig.at("var/db/kbf-installer/orders").exists());
            assert!(
                !rig.new_state_dir()
                    .join("migration-units/kbf.toml")
                    .exists()
            );

            // A Mac with no kit marker: any Mac is provisioned.
            let mut rig = Rig::mac(&format!("keys-mac-plain-{name}-{mode:?}"));
            let plain = at(&rig.host.root, KBF_TOML_PATH);
            put(&plain, &text, 0o640);
            fs::create_dir_all(rig.new_state_dir().join("migration-units")).expect("dir");
            let (outcome, _) = run(&mut rig, mode);
            assert_eq!(outcome, Outcome::Provisioned);
            assert_eq!(fs::read_to_string(&plain).expect("kbf.toml"), text);
            assert!(!rig.at("var/db/kbf-installer/orders").exists());
        }
    }
    for (daemon, why) in [
        ("", "has no [daemon] update-dir"),
        (
            "update-dir = \"/elsewhere\"\n",
            "[daemon] update-dir is not the installer's directory",
        ),
        (
            "update-dir = \"/var/db/kbf-installer\"\norders-dir = \"/elsewhere\"\n",
            "[daemon] orders-dir is not the installer's directory",
        ),
        ("update-dir = 7\n", "[daemon] update-dir is not text"),
    ] {
        for mode in [Mode::DryRun, Mode::Apply] {
            let mut rig = Rig::mac("keys-mac-refused");
            let plain = at(&rig.host.root, KBF_TOML_PATH);
            let text = format!("mode = \"daemon\"\n\n[daemon]\n{daemon}driver = [\"native\"]\n");
            put(&plain, &text, 0o640);
            let m = rig.migration();
            let mut report = Report::new(super::super::Outcome::Migrated);
            let err = add(&m, &mut rig.host, mode, &mut report).expect_err(why);
            assert_eq!(err.exit(), Exit::NeedsOperator, "{why}");
            let said = err.to_string();
            assert!(
                said.contains(why) && said.contains("kbf-mac-provision renders it"),
                "{said}"
            );
            assert!(
                !said.contains("/var/db") && !said.contains("/elsewhere"),
                "{said}"
            );
            assert_eq!(
                fs::read_to_string(&plain).expect("kbf.toml"),
                text,
                "{why}: nothing written"
            );
        }
    }
}

/// Catches a Mac `migrate --apply` that refuses only after it has stopped the node's job and
/// written the new tree: the refusal comes first, so the host is as it was.
///
/// Mutant seen red: the preflight call dropped (the refusal then comes after the first write).
#[test]
fn a_mac_apply_with_a_wrong_kbf_toml_changes_nothing() {
    use crate::migrate::kit_host::{Form, KBF_TOML_PATH, at};
    let mut rig = Rig::kit(
        "keys-mac-preflight",
        crate::layout::Platform::MacOs,
        Form::New,
    );
    let path = at(&rig.host.root, KBF_TOML_PATH);
    let text = fs::read_to_string(&path)
        .expect("kbf.toml")
        .replace("/var/db/kbf-installer", "/elsewhere");
    fs::write(&path, &text).expect("plant");
    let (before, events) = (rig.snapshot("usr/local"), rig.events.borrow().len());
    let never = FnVerifier(Box::new(|_, _| Err("no pipeline here".to_owned())));
    let err = run_migrate(&mut rig, &never, Mode::Apply).expect_err("refused");
    assert_eq!(err.exit(), Exit::NeedsOperator);
    assert_eq!(rig.snapshot("usr/local"), before);
    assert_eq!(rig.events.borrow().len(), events, "no job touched");
    assert!(!rig.new_state_dir().exists() || !rig.new_state_dir().join("migration.json").exists());
}

/// A host that fails the `n`th look or the `n`th step it is asked for, and does the rest on
/// the scratch host under it.
struct Flaky<'a> {
    under: &'a mut kbf_host::testkit::ScratchHost,
    inspects: std::cell::Cell<usize>,
    applies: usize,
    fail_inspect: Option<usize>,
    fail_apply: Option<usize>,
}

impl<'a> Flaky<'a> {
    fn new(
        under: &'a mut kbf_host::testkit::ScratchHost,
        fail_inspect: Option<usize>,
        fail_apply: Option<usize>,
    ) -> Self {
        Flaky {
            under,
            inspects: std::cell::Cell::new(0),
            applies: 0,
            fail_inspect,
            fail_apply,
        }
    }
}

impl Exec for Flaky<'_> {
    fn euid(&self) -> u32 {
        self.under.euid()
    }

    fn inspect(&self, path: &HostPath) -> std::io::Result<Option<Entry>> {
        let n = self.inspects.get();
        self.inspects.set(n + 1);
        if self.fail_inspect == Some(n) {
            return Err(std::io::Error::other("planted: inspect failed"));
        }
        self.under.inspect(path)
    }

    fn stat(&self, path: &HostPath) -> std::io::Result<Option<kbf_host::Meta>> {
        self.under.stat(path)
    }

    fn query(&self, cmd: &kbf_host::Cmd) -> std::io::Result<std::process::Output> {
        self.under.query(cmd)
    }

    fn http_status(&self, url: &kbf_host::HttpUrl) -> std::io::Result<u16> {
        self.under.http_status(url)
    }

    fn apply(&mut self, step: &Step) -> std::io::Result<()> {
        let n = self.applies;
        self.applies += 1;
        if self.fail_apply == Some(n) {
            return Err(std::io::Error::other("planted: step failed"));
        }
        self.under.apply(step)
    }

    fn check_binary(&self) -> Result<(), kbf_host::Untrusted> {
        self.under.check_binary()
    }
}

fn add_flaky(
    rig: &mut Rig,
    inspect: Option<usize>,
    apply: Option<usize>,
) -> Result<Outcome, MigrateError> {
    let m = rig.migration();
    let mut report = Report::new(super::super::Outcome::Migrated);
    let mut exec = Flaky::new(&mut rig.host, inspect, apply);
    add(&m, &mut exec, Mode::Apply, &mut report)
}

fn restore_flaky(
    rig: &mut Rig,
    inspect: Option<usize>,
    apply: Option<usize>,
) -> Result<Report, MigrateError> {
    let m = rig.migration();
    let mut report = Report::new(super::super::Outcome::RolledBack);
    let mut exec = Flaky::new(&mut rig.host, inspect, apply);
    restore(&m, &mut exec, Mode::Apply, &mut report).map(|()| report)
}

/// Catches a host error swallowed or turned into a half edit: each look and each step that
/// fails ends the run with the error (the migration then stops with the services still
/// stopped and a run again starts from what is there), the file is never half written, and a
/// run that stopped between the steps (the original kept, the spool made, the file not yet
/// written) is finished by the next one, which keeps the first original.
///
/// Mutant seen red: the order of the steps swapped so the file is written before the spool
/// (the file then holds the keys when the spool step fails).
#[test]
fn a_host_that_fails_a_step_stops_the_run_and_the_next_run_finishes_it() {
    for (inspect, apply) in [(Some(0), None), (Some(1), None), (None, Some(0))] {
        let mut rig = Rig::linux(&format!("flaky-{inspect:?}-{apply:?}"));
        put_toml(&rig, NODE);
        assert!(
            add_flaky(&mut rig, inspect, apply).is_err(),
            "{inspect:?} {apply:?}"
        );
        assert_eq!(read(&rig), NODE);
    }
    for fail in [1, 2] {
        let mut rig = Rig::linux(&format!("flaky-apply-{fail}"));
        put_toml(&rig, NODE);
        assert!(add_flaky(&mut rig, None, Some(fail)).is_err());
        assert_eq!(
            read(&rig),
            NODE,
            "step {fail} failed: the file is as it was"
        );
        let kept = rig.new_state_dir().join("migration-units/kbf.toml");
        assert_eq!(fs::read_to_string(&kept).expect("kept"), NODE);
        // The run again: the same result as a clean one, and the original is still the first.
        assert_eq!(run(&mut rig, Mode::Apply).0, Outcome::Added);
        assert_eq!(keys(&read(&rig)).0.as_deref(), Some(UPDATE));
        assert_eq!(fs::read_to_string(&kept).expect("kept"), NODE);
    }
}

/// Catches a rollback that mishandles what it finds: nothing kept (no keys were added), no
/// file, a file or kept original that is not text, a kept original that is not TOML, and a
/// file that is already the original (a rollback twice) are all left alone and without a
/// complaint; a failing look or step is an error.
///
/// Mutant seen red: the early return for a missing kept original replaced by an error.
#[test]
fn a_rollback_with_nothing_to_undo_changes_nothing() {
    let kept = |rig: &Rig| rig.new_state_dir().join("migration-units/kbf.toml");
    let mut rig = Rig::linux("restore-none");
    put_toml(&rig, NODE);
    let report = restore_flaky(&mut rig, None, None).expect("restore");
    assert!(report.warnings.is_empty() && report.lines.is_empty());
    assert_eq!(read(&rig), NODE);

    // Kept, and no kbf.toml now.
    put(&kept(&rig), NODE, 0o600);
    fs::remove_file(kbf_toml(&rig)).expect("remove");
    restore_flaky(&mut rig, None, None).expect("restore");
    assert!(!kbf_toml(&rig).exists());

    // The file is already the original: the kept copy goes, the file stays.
    put(&kbf_toml(&rig), NODE, 0o640);
    restore_flaky(&mut rig, None, None).expect("restore");
    assert_eq!(read(&rig), NODE);
    assert!(!kept(&rig).exists());

    // Not text, in the file or in the kept copy; not TOML in the kept copy.
    for (original, now) in [
        (NODE.as_bytes().to_vec(), vec![0xff, 0xfe]),
        (vec![0xff, 0xfe], NODE.as_bytes().to_vec()),
        (b"mode = \n".to_vec(), NODE.as_bytes().to_vec()),
    ] {
        fs::write(kept(&rig), &original).expect("kept");
        fs::write(kbf_toml(&rig), &now).expect("now");
        restore_flaky(&mut rig, None, None).expect("restore");
        assert_eq!(fs::read(kbf_toml(&rig)).expect("now"), now);
        assert_eq!(fs::read(kept(&rig)).expect("kept"), original, "kept stays");
    }

    // A failing look or step.
    fs::write(kept(&rig), NODE).expect("kept");
    let migrated = insert(
        NODE,
        &[
            ("update-dir", UPDATE.to_owned()),
            ("orders-dir", ORDERS.to_owned()),
        ],
    );
    fs::write(kbf_toml(&rig), &migrated).expect("now");
    for (inspect, apply) in [
        (Some(0), None),
        (Some(1), None),
        (None, Some(0)),
        (None, Some(1)),
    ] {
        assert!(
            restore_flaky(&mut rig, inspect, apply).is_err(),
            "{inspect:?} {apply:?}"
        );
    }
    assert_eq!(
        read(&rig),
        NODE,
        "the file was put back before the kept copy went"
    );
}

/// Catches the edit shapes the first table missed: a header on the file's last line with no
/// newline, and an array of tables named `daemon` (not the `[daemon]` table: the file is
/// left, since `daemon` is not a table of text).
#[test]
fn a_header_on_the_last_line_and_an_array_of_tables() {
    let mut rig = Rig::linux("shape-last-header");
    put_toml(&rig, "mode = \"daemon\"\n[daemon]");
    assert_eq!(run(&mut rig, Mode::Apply).0, Outcome::Added);
    assert_eq!(
        read(&rig),
        format!(
            "mode = \"daemon\"\n[daemon]\nupdate-dir = \"{UPDATE}\"\norders-dir = \"{ORDERS}\"\n"
        )
    );
    let mut rig = Rig::linux("shape-array-elsewhere");
    put_toml(&rig, "mode = \"daemon\"\n[[other]]\nx = 1\n");
    assert_eq!(run(&mut rig, Mode::Apply).0, Outcome::Added);
    assert!(read(&rig).ends_with(&format!("orders-dir = \"{ORDERS}\"\n")));
    let mut rig = Rig::linux("shape-array");
    put_toml(&rig, "mode = \"daemon\"\n[[daemon]]\ndriver = 1\n");
    assert!(matches!(run(&mut rig, Mode::Apply).0, Outcome::Left(_)));
    let mut rig = Rig::linux("shape-directory");
    fs::create_dir_all(kbf_toml(&rig)).expect("dir");
    assert!(matches!(run(&mut rig, Mode::Apply).0, Outcome::Left(_)));
    let mut rig = Rig::linux("shape-binary");
    put(&kbf_toml(&rig), "x", 0o640);
    fs::write(kbf_toml(&rig), [0xff, 0xfe]).expect("bytes");
    assert!(matches!(run(&mut rig, Mode::Apply).0, Outcome::Left(_)));
}
