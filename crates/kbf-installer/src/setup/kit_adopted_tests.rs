//! `setup` on a Mac the Mac kit installed and `migrate` adopted: the kit's `kbf.toml` stays as
//! the kit wrote it and setup adds what it needs. Each test says the defect it catches.

use std::fs;

use super::backend::{Backend, Migrations, Releases, Schemas};
use super::fake::FakeOs;
use super::kbf_toml::{Decision, Ours, UpdateDirs, decide, decide_adopted};
use super::validate::Raw;
use super::{Options, SetupError, run};
use crate::CommitId;
use crate::cli::Mode;
use crate::database::SchemaOps;
use crate::migrate::kit_host::{Form, KBF_TOML, KBF_TOML_PATH, at};
use crate::migrate::rig::{FnVerifier, Rig, me, run_migrate, unit};
use crate::migrate::{MigrateError, Unit};

const PATH: &str = "/usr/local/etc/kbf/kbf.toml";
const UPDATE_DIR: &str = "/var/db/kbf-installer";
const ORDERS_DIR: &str = "/var/db/kbf-installer/orders";

fn ours<'a>() -> Ours<'a> {
    Ours {
        mode: "daemon",
        frontend_url: Some("https://farm.example:8443"),
        token_file: Some("/usr/local/etc/kbf/enroll.token"),
        api_token_file: None,
        state_store: None,
        update: Some(UpdateDirs {
            update_dir: UPDATE_DIR,
            orders_dir: ORDERS_DIR,
        }),
        extra: &[],
    }
}

/// What `setup` asks of a Mac: the Mac's `[daemon]` keys are `kbf-mac-provision`'s, checked by
/// the caller and never part of what setup writes (G9), so `update` is `None`.
fn mac_ours<'a>() -> Ours<'a> {
    Ours {
        update: None,
        ..ours()
    }
}

/// Catches the refusal that left a migrated Mac without a setup: the kit's file has no
/// `[frontend] url` or `[enroll] token_file` (it keeps its identity under `[identity]`), which
/// the generic decision counts as "differing". The adopted arm adds the missing keys, leaves
/// every kit line as it was (the kit's text is a subsequence of the result, in order), and
/// parses with each added key set. It adds no `[daemon]` directory key: on a Mac those are
/// `kbf-mac-provision`'s (G9) and the kit's file already has them.
///
/// Mutants seen red: the generic `decide` used (refused); a kit line rewritten; a daemon key
/// added.
#[test]
fn the_kits_keys_are_kept_and_the_missing_ones_added() {
    assert!(matches!(
        decide(Some(KBF_TOML), &mac_ours(), PATH),
        Decision::Refuse(_)
    ));
    let Decision::Write(text) = decide_adopted(KBF_TOML, &mac_ours(), PATH) else {
        panic!("adopted arm refused");
    };
    let mut rest = text.lines();
    for line in KBF_TOML.lines() {
        assert!(
            rest.any(|l| l == line),
            "kit line lost or moved: {line:?}\n{text}"
        );
    }
    let table: toml::Table = text.parse().expect("toml");
    assert_eq!(
        table["frontend"]["url"].as_str(),
        Some("https://farm.example:8443")
    );
    assert_eq!(
        table["enroll"]["token_file"].as_str(),
        Some("/usr/local/etc/kbf/enroll.token")
    );
    assert_eq!(table["daemon"]["update-dir"].as_str(), Some(UPDATE_DIR));
    assert!(table["daemon"].get("orders-dir").is_none(), "{text}");
    assert_eq!(table["daemon"]["driver"][0].as_str(), Some("native"));
    // Only these two keys are new.
    let was: toml::Table = KBF_TOML.parse().expect("toml");
    let count = |t: &toml::Table| {
        t.values()
            .map(|v| v.as_table().map_or(1, |x| x.len()))
            .sum::<usize>()
    };
    assert_eq!(count(&table), count(&was) + 2);
    // Run again on the result: nothing to add.
    assert_eq!(decide_adopted(&text, &mac_ours(), PATH), Decision::Keep);
}

/// Catches a kit key rewritten: a key that is there with another value (the frontend URL, the
/// mode) is refused naming the key, not changed. (The `[daemon]` directories of a Mac are
/// checked by `check_provisioned`, not here.)
///
/// Mutant seen red: a differing key overwritten.
#[test]
fn a_kit_key_that_differs_is_refused_not_rewritten() {
    for (text, want) in [
        (
            format!("{KBF_TOML}\n[frontend]\nurl = \"https://other.example:1\"\n"),
            "[frontend] url",
        ),
        (
            KBF_TOML.replace("mode = \"daemon\"", "mode = \"storage\""),
            "mode",
        ),
    ] {
        let Decision::Refuse(why) = decide_adopted(&text, &mac_ours(), PATH) else {
            panic!("{want}: not refused");
        };
        assert!(
            why.contains(want) && why.contains("never rewrites"),
            "{why}"
        );
    }
}

/// Catches an edit that cannot be trusted: invalid TOML is refused; a table written inline (the
/// text edit would add a second `[daemon]`) is refused by the check of what the result parses
/// to; a file with no `mode` and no tables, and one with no final newline, get the keys at the
/// right place.
///
/// Mutants seen red: the parse check of the result removed (a duplicate table is written).
#[test]
fn an_edit_that_would_change_a_kit_key_is_refused_and_odd_files_are_handled() {
    let Decision::Refuse(why) = decide_adopted("mode = [", &ours(), PATH) else {
        panic!("not refused");
    };
    assert!(why.contains("not valid TOML"), "{why}");
    let inline = "mode = \"daemon\"\ndaemon = { driver = [\"native\"] }\n";
    let Decision::Refuse(why) = decide_adopted(inline, &ours(), PATH) else {
        panic!("not refused");
    };
    assert!(
        why.contains("cannot add") && why.contains("[daemon] update-dir"),
        "{why}"
    );
    let bare = "[identity]\nnode-id = \"a\"";
    let Decision::Write(text) = decide_adopted(bare, &ours(), PATH) else {
        panic!("refused");
    };
    let table: toml::Table = text.parse().expect("toml");
    assert_eq!(table["mode"].as_str(), Some("daemon"));
    assert_eq!(table["identity"]["node-id"].as_str(), Some("a"));
    assert!(text.starts_with("mode = \"daemon\"\n"), "{text}");
    // Keys go after the table's last line, before a following table and its comment.
    let two = "mode = \"daemon\"\n[frontend]\nx = \"1\"\n\n# next\n[daemon]\nupdate-dir = \"/var/db/kbf-installer\"\n";
    let Decision::Write(text) = decide_adopted(two, &ours(), PATH) else {
        panic!("refused");
    };
    let table: toml::Table = text.parse().expect("toml");
    assert_eq!(table["frontend"]["x"].as_str(), Some("1"));
    assert_eq!(
        table["frontend"]["url"].as_str(),
        Some("https://farm.example:8443")
    );
}

struct NoRelease;
impl Releases for NoRelease {
    fn install_first(&self, _: &CommitId) -> Result<(), SetupError> {
        unreachable!("no --commit")
    }
}
struct NoSchema;
impl Schemas for NoSchema {
    fn of(&self, _: &str) -> Result<Box<dyn SchemaOps>, SetupError> {
        unreachable!("a node has no database")
    }
}

/// What the journal of the host says, read for real by [`crate::migrate`].
struct Journal(crate::layout::Layout);
impl Migrations for Journal {
    fn units(&self) -> Result<Vec<Unit>, MigrateError> {
        crate::migrate::standing(&self.0, me())
    }
    fn kit_adopted(&self) -> Result<bool, MigrateError> {
        crate::migrate::kit_adopted(&self.0, me())
    }
}

fn raw() -> Raw {
    Raw {
        mode: Mode::Daemon,
        frontend_url: Some("https://farm.example:8443".to_owned()),
        token_file: Some("/root/kbf.token".to_owned()),
        release_source: None,
        release_url: None,
        min_generation: None,
        commit: None,
        database: None,
        kbf: Default::default(),
        proxy: Default::default(),
        deploy: Default::default(),
        create_local_database: false,
        installer_config: None,
    }
}

fn setup_on(rig: &mut Rig) -> Result<String, SetupError> {
    let mut os = FakeOs::mac();
    let mut fetch = super::fake::user("_kbf-fetch", 303);
    fetch.home = "/var/empty".to_owned();
    os.users.push(fetch);
    os.root = kbf_host::Owner {
        uid: me(),
        gid: rustix::process::getegid().as_raw(),
    };
    crate::migrate::rig::put(&rig.at("root/kbf.token"), "tok-abc123\n", 0o600);
    let mut out = Vec::new();
    let journal = Journal(rig.layout.clone());
    let backend = Backend {
        releases: &NoRelease,
        schemas: &NoSchema,
        migrations: &journal,
    };
    let options = Options {
        apply: true,
        no_start: true,
    };
    let host: &mut dyn kbf_host::Exec = &mut rig.host;
    // `setup` reads the journal through the rig's layout; the host it writes is the rig's root.
    run(&os, host, &backend, &raw(), options, &mut out)?;
    Ok(String::from_utf8(out).expect("text"))
}

fn adopt(rig: &mut Rig) {
    let verifier = FnVerifier(Box::new(|_, _| Err("no pipeline here".to_owned())));
    run_migrate(rig, &verifier, kbf_host::Mode::Apply).expect("migrate");
}

/// Catches `setup` unusable after a kit-host migration, on jerry's fixtures: adopt the kit Mac
/// (`testdata/kit_host`), then `setup --mode=daemon` succeeds, every kit line of `kbf.toml` is
/// still there in order, and the two keys it adds ([frontend] url, [enroll] token_file) are the
/// only new ones: the Mac's `[daemon]` directories were checked, never written (G9). Setup also
/// writes `installer.toml`.
///
/// Mutants seen red: the journal's `kit` ignored (the old refusal); `Done` not required.
#[test]
fn setup_after_a_kit_host_migration_keeps_the_kits_file_and_adds_its_keys() {
    let mut rig = Rig::kit("setup-after", crate::layout::Platform::MacOs, Form::New);
    adopt(&mut rig);
    setup_on(&mut rig).expect("setup after migrate");
    let now = fs::read_to_string(at(&rig.host.root, KBF_TOML_PATH)).expect("kbf.toml");
    let mut rest = now.lines();
    for line in KBF_TOML.lines() {
        assert!(rest.any(|l| l == line), "kit line lost: {line:?}\n{now}");
    }
    let (was, is): (toml::Table, toml::Table) =
        (KBF_TOML.parse().expect("kit"), now.parse().expect("now"));
    for (k, v) in &was {
        if let Some(t) = v.as_table() {
            for (ik, iv) in t {
                assert_eq!(&is[k][ik], iv, "[{k}] {ik}");
            }
        } else {
            assert_eq!(&is[k], v);
        }
    }
    let count = |t: &toml::Table| {
        t.values()
            .map(|v| v.as_table().map_or(1, |x| x.len()))
            .sum::<usize>()
    };
    assert_eq!(count(&is), count(&was) + 2);
    assert!(rig.at("usr/local/etc/kbf/installer.toml").is_file());
}

/// Catches the old guard weakened: before migrate, `setup` on the kit Mac still refuses with
/// the "run migrate first" message (unchanged), and writes nothing. A host whose journal is not
/// a finished kit migration (a non-kit host with a differing `kbf.toml`) is still refused as
/// before, the kit's arm is not taken, and its file is untouched.
///
/// Mutants seen red: the kit arm taken for any journal that is `Done`; the refusal removed.
#[test]
fn before_migrate_and_on_a_non_kit_host_setup_refuses_as_before() {
    let mut rig = Rig::kit("setup-before", crate::layout::Platform::MacOs, Form::New);
    let err = setup_on(&mut rig).expect_err("before migrate");
    assert!(matches!(err, SetupError::Unmigrated { .. }), "{err}");
    assert!(
        err.to_string()
            .contains("run `kbf-installer migrate --apply"),
        "{err}"
    );
    assert!(!rig.at("usr/local/etc/kbf/installer.toml").exists());

    // A migrated old (non-kit) Mac whose kbf.toml differs: the generic refusal.
    let mut rig = Rig::mac("setup-nonkit");
    adopt(&mut rig);
    let path = rig.at("usr/local/etc/kbf/kbf.toml");
    crate::migrate::rig::put(
        &path,
        "mode = \"daemon\"\n[frontend]\nurl = \"https://other.example:1\"\n[x]\ny = \"1\"\n\
         [daemon]\nupdate-dir = \"/var/db/kbf-installer\"\n",
        0o644,
    );
    let before = fs::read(&path).expect("file");
    let err = setup_on(&mut rig).expect_err("differs");
    assert!(
        err.to_string()
            .contains("holds other settings and these keys differ"),
        "{err}"
    );
    assert_eq!(fs::read(&path).expect("file"), before);
    let _ = unit;
}

/// Catches the journal read wrong by the real `Migrations`: `kit_adopted` is false before the
/// migration, true after a finished kit migration, false after a finished migration of an old
/// host (`kit` unset), false once a kit migration is rolled back (the phase is not `Done`), and
/// an unreadable journal is an error, not a "no".
///
/// Mutants seen red: `kit` not required; `Done` not required.
#[test]
fn the_real_journal_says_kit_adopted_only_for_a_finished_kit_migration() {
    use super::backend::Journaled;
    let mut rig = Rig::kit("journal-kit", crate::layout::Platform::MacOs, Form::New);
    let journaled = Journaled::new(rig.layout.clone(), me());
    assert!(!journaled.kit_adopted().expect("none yet"));
    adopt(&mut rig);
    assert!(journaled.kit_adopted().expect("adopted"));
    let m = rig.migration();
    crate::migrate::rollback(&m, &mut rig.host, kbf_host::Mode::Apply).expect("rollback");
    assert!(!journaled.kit_adopted().expect("rolled back"));

    let mut old = Rig::mac("journal-old");
    adopt(&mut old);
    let journaled = Journaled::new(old.layout.clone(), me());
    assert!(!journaled.kit_adopted().expect("old host"));
    fs::write(old.new_state_dir().join("migration.json"), "{").expect("damage");
    assert!(journaled.kit_adopted().is_err());
}

/// Catches an edit that parses but is not what was asked: the kit's file holds a header-looking
/// line inside a multi-line string, so the text edit puts `url` after the string, as a top-level
/// key, and the file parses with no `[frontend] url`. It is refused, naming the key, and not
/// written.
///
/// Mutant seen red: the check that each added key reads back where it was asked removed (the
/// file is written without the key).
#[test]
fn an_added_key_that_does_not_read_back_where_asked_is_refused() {
    let tricky = "mode = \"daemon\"\nnote = \"\"\"\n[frontend]\nbody\n\"\"\"\n";
    let Decision::Refuse(why) = decide_adopted(tricky, &ours(), PATH) else {
        panic!("written");
    };
    assert!(
        why.contains("cannot add") && why.contains("[frontend] url"),
        "{why}"
    );
}
