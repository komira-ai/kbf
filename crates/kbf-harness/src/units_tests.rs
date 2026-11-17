//! The unit of a farm's process: the mode's spelling on the command line, the settings
//! a mode starts with, and the strict edits to them.

use std::path::{Path, PathBuf};

use super::*;

const WIRING_PORTS: (u16, u16, u16) = (1111, 2222, 3333);

fn wiring() -> Wiring {
    Wiring {
        reapi: WIRING_PORTS.0,
        worker: WIRING_PORTS.1,
        api: WIRING_PORTS.2,
        storage_url: "https://127.0.0.1:4444".to_owned(),
    }
}

/// The unit of `mode` for a made-up process (root `/R`, node `n-1`).
fn unit(mode: Mode) -> Unit {
    let w = wiring();
    let s = Settings {
        root: Path::new("/R"),
        host: "127.0.0.1",
        storage_port: 5555,
        wiring: &w,
    };
    Unit::new(mode, &s)
}

fn has(unit: &Unit, flag: &str) -> bool {
    unit.flags().iter().any(|f| f == flag)
}

#[test]
fn a_unit_names_its_mode_as_its_one_mode_word_and_no_other_spelling_passes() {
    // Catches: a check so loose that a unit naming another mode passes, that takes the old
    // positional spelling (`kbf storage`), `--mode <name>` as two words, a second `--mode=`,
    // or a prefix of the name (`--mode=storage-node` for `storage`): a unit that
    // starts something else than its name says would start a farm of the wrong processes.
    // Catches: `names_mode` accepts the mode name as any word; takes the first `--mode=`
    // only; compares a prefix.
    assert!(names_mode(
        &["--mode=storage", "--cold-store=x"],
        Mode::Storage
    ));
    assert!(!names_mode(&["--mode=storage"], Mode::Frontend));
    assert!(!names_mode(&["storage", "--cold-store=x"], Mode::Storage));
    assert!(!names_mode(&["--mode", "storage"], Mode::Storage));
    assert!(!names_mode(
        &["--mode=storage", "--mode=frontend"],
        Mode::Storage
    ));
    assert!(!names_mode(
        &["--mode=frontend", "--mode=storage"],
        Mode::Storage
    ));
    assert!(!names_mode(&["--mode=storage-node"], Mode::Storage));
    assert!(!names_mode(&[], Mode::Storage));
}

#[test]
fn every_mode_is_started_by_its_packaged_unit_as_kbf_mode_and_the_config() {
    // Catches: a mode whose packaged unit the harness cannot read back (no `ExecStart=`,
    // not `kbf`, another mode's word), which every farm built on it would trip over; an
    // argv that is not the unit's `--mode=<name>` and the `--config` flag it is given.
    for mode in Mode::ALL {
        let argv = unit(mode).argv("--config=/R/kbf.toml".to_owned());
        assert_eq!(
            argv,
            [format!("--mode={mode}"), "--config=/R/kbf.toml".to_owned()]
        );
        assert_eq!(mode.to_string(), mode.name());
    }
}

#[test]
fn every_mode_has_its_settings_and_each_key_has_its_table() {
    // Catches: the file of a mode's settings read wrong (a key under no table, a switch
    // taken for a value flag, a commented key lost), so that the unit refuses a key the
    // mode takes or accepts one it does not.
    for mode in Mode::ALL {
        let keys = keys(mode);
        assert!(keys.len() > 30, "{mode}: {}", keys.len());
        assert!(keys.values().all(|(table, _)| !table.is_empty()));
    }
    let storage = keys(Mode::Storage);
    assert_eq!(storage["cold-store"], ("stores", false));
    // A commented key (no default) is a key, and a switch is a live `= false` line.
    assert_eq!(storage["hot-store-max-bytes"], ("stores", false));
    assert!(keys(Mode::Frontend)["no-memory-climb"].1);
    assert_eq!(keys(Mode::Frontend)["handover-socket"], ("handover", false));
    // The help text of a setting is not a key.
    assert!(!storage.contains_key("#"));
    assert!(storage.keys().all(|k| !k.contains(' ')));
}

#[test]
fn each_mode_starts_with_the_settings_its_unit_had_for_the_farms_directories() {
    // Catches: a default dropped or renamed, which the farm would then start without (the
    // equivalence test in kbf-cli's tests holds the whole set to the golden of the flags
    // the farm started with).
    let frontend = unit(Mode::Frontend).flags();
    assert!(
        frontend.contains(&"--listen=127.0.0.1:1111".to_owned()),
        "{frontend:?}"
    );
    assert!(frontend.contains(&"--worker-listen=127.0.0.1:2222".to_owned()));
    assert!(frontend.contains(&"--api-listen=127.0.0.1:3333".to_owned()));
    assert!(frontend.contains(&"--storage=https://127.0.0.1:4444".to_owned()));
    assert!(frontend.contains(&"--api-token-file=/R/etc/api-token".to_owned()));
    let storage = unit(Mode::Storage).flags();
    assert!(storage.contains(&"--storage-listen=127.0.0.1:5555".to_owned()));
    assert!(storage.contains(&"--hot-store=file:///R/hot".to_owned()));
    // The registration with the frontend is the farm's (`node`), not the unit's.
    assert!(storage.iter().all(|f| !f.starts_with("--frontend")));
}

#[test]
fn replace_changes_the_set_flag_and_adds_nothing() {
    // Catches: a replaced flag duplicated, or one that lands in another key.
    let before = unit(Mode::Storage);
    let after = unit(Mode::Storage).replace(&["--cold-store=file:///x".to_owned()]);
    assert_eq!(after.flags().len(), before.flags().len());
    assert!(has(&after, "--cold-store=file:///x"));
    assert!(!has(
        &after,
        "--cold-store=s3://kbf-cas?endpoint=http://127.0.0.1:9000&conditional-put=true"
    ));
}

#[test]
#[should_panic(expected = "kbf storage takes no --no-such-flag")]
fn replace_of_a_flag_the_mode_does_not_take_panics() {
    // Catches: a silent no-op or a key `kbf` would refuse written into the file, so a
    // flag the mode stopped taking leaves the farm running with the unit's own value while
    // the test believes it changed it.
    let _ = unit(Mode::Storage).replace(&["--no-such-flag=1".to_owned()]);
}

#[test]
#[should_panic(expected = "storage sets no --hot-store-max-bytes")]
fn replace_of_a_flag_the_unit_does_not_set_panics() {
    // Catches: a `replace` that adds (a flag the unit stopped setting would then be
    // given silently, where `set` is the call that adds).
    let _ = unit(Mode::Storage).replace(&["--hot-store-max-bytes=7".to_owned()]);
}

#[test]
#[should_panic(expected = "storage already sets --cold-store")]
fn append_of_a_flag_the_unit_sets_panics() {
    // Catches: a key given twice (the later one would hide the unit's value).
    let _ = unit(Mode::Storage).append(&["--cold-store=file:///x".to_owned()]);
}

#[test]
#[should_panic(expected = "kbf storage takes no --no-such-flag")]
fn append_of_a_flag_the_mode_does_not_take_panics() {
    // Catches: a typo taken for a new setting nobody reads.
    let _ = unit(Mode::Storage).append(&["--no-such-flag=1".to_owned()]);
}

#[test]
fn append_adds_a_flag_the_unit_does_not_set() {
    let after = unit(Mode::Storage).append(&["--hot-store-max-bytes=7".to_owned()]);
    assert!(has(&after, "--hot-store-max-bytes=7"));
}

#[test]
fn set_replaces_or_adds() {
    // Catches: a limit that is dropped when the unit sets no such flag, or one that is
    // added next to the unit's own.
    let before = unit(Mode::Storage).flags();
    let after = unit(Mode::Storage)
        .set(&[
            "--cold-store=file:///x".to_owned(),
            "--hot-store-max-bytes=7".to_owned(),
        ])
        .flags();
    assert_eq!(after.len(), before.len() + 1);
    assert!(after.contains(&"--cold-store=file:///x".to_owned()));
    assert!(after.contains(&"--hot-store-max-bytes=7".to_owned()));
    assert_eq!(
        after
            .iter()
            .filter(|f| f.starts_with("--cold-store="))
            .count(),
        1
    );
}

#[test]
#[should_panic(expected = "kbf storage takes no --no-such-flag")]
fn set_of_a_flag_the_mode_does_not_take_panics() {
    // Catches: a limit a mode refuses (a frontend refuses a storage node's) written to its
    // file, where `kbf` would exit 2 at the start instead of the test naming the key.
    let _ = unit(Mode::Storage).set(&["--no-such-flag=1".to_owned()]);
}

#[test]
fn remove_takes_a_set_flag_out() {
    let before = unit(Mode::Storage);
    let after = unit(Mode::Storage).remove(&["--hot-store"]);
    assert_eq!(after.flags().len(), before.flags().len() - 1);
    assert!(after.flags().iter().all(|f| !f.starts_with("--hot-store=")));
}

#[test]
#[should_panic(expected = "storage sets no --hot-store-max-bytes")]
fn remove_of_a_flag_the_unit_does_not_set_panics() {
    // Catches: a removal that hides that the unit stopped having the flag.
    let _ = unit(Mode::Storage).remove(&["--hot-store-max-bytes"]);
}

#[test]
#[should_panic(expected = "kbf storage takes no --no-such-flag")]
fn remove_of_a_flag_the_mode_does_not_take_panics() {
    let _ = unit(Mode::Storage).remove(&["--no-such-flag"]);
}

#[test]
fn a_switch_is_given_without_a_value_and_a_value_flag_with_one() {
    // Catches: `--switch=true` written as a string (kbf refuses a switch that is not a
    // boolean), or a value flag written as a switch.
    let on = unit(Mode::Frontend).set(&["--no-memory-climb".to_owned()]);
    assert!(has(&on, "--no-memory-climb"));
    assert!(
        on.toml().contains("no-memory-climb = true"),
        "{}",
        on.toml()
    );
}

#[test]
#[should_panic(expected = "--no-memory-climb is a switch")]
fn a_switch_with_a_value_panics() {
    let _ = unit(Mode::Frontend).set(&["--no-memory-climb=true".to_owned()]);
}

#[test]
#[should_panic(expected = "--meta-dir takes a value")]
fn a_value_flag_without_a_value_panics() {
    let _ = unit(Mode::Storage).set(&["--meta-dir".to_owned()]);
}

#[test]
fn the_file_is_each_setting_under_the_table_of_its_group_as_text() {
    // Catches: a key under another table than its group's (kbf refuses it, naming the
    // key), a number written where the text is (`kbf` reads either, but the file is the
    // flag's own text), a value lost in quoting (the cold store's `&` and `?`).
    let unit = unit(Mode::Storage);
    let parsed: toml::Table = unit.toml().parse().expect("TOML");
    let stores = parsed["stores"].as_table().expect("[stores]");
    assert_eq!(stores["meta-dir"].as_str(), Some("/R/meta"));
    assert_eq!(
        stores["cold-store"].as_str(),
        Some("s3://kbf-cas?endpoint=http://127.0.0.1:9000&conditional-put=true")
    );
    let listener = parsed["listener"].as_table().expect("[listener]");
    assert_eq!(listener["storage-listen"].as_str(), Some("127.0.0.1:5555"));
    // Every key of the file is in the table the mode's keys say.
    let keys = keys(Mode::Storage);
    for (table, body) in &parsed {
        for key in body.as_table().expect("a table").keys() {
            assert_eq!(keys[key.as_str()].0, table, "{key}");
        }
    }
}

/// Asserts that a farm gives `--drain-lb-wait=0` to a frontend of this release, whether the
/// farm runs an older release's `kbf` on no role or on a node only, and gives it to a
/// frontend that runs an older release's `kbf` never, since that release does not know the
/// flag and fails its start on it.
#[test]
fn only_a_frontend_of_this_release_is_given_the_drain_wait() {
    let w = wiring();
    let place = Settings {
        root: Path::new("/R"),
        host: "127.0.0.1",
        storage_port: 5555,
        wiring: &w,
    };
    let older = |roles: &[Role]| {
        let mut topo = Topology::rehearsal(2);
        topo.binaries = roles
            .iter()
            .map(|role| (*role, PathBuf::from("/older/kbf")))
            .collect();
        frontend(&place, &topo)
    };

    assert!(has(&older(&[]), "--drain-lb-wait=0"));
    assert!(has(&older(&[Role::Node(1)]), "--drain-lb-wait=0"));
    assert!(!has(&older(&[Role::Frontend]), "--drain-lb-wait=0"));
    let both = older(&[Role::Node(1), Role::Frontend]);
    assert!(!has(&both, "--drain-lb-wait=0"));
    assert!(has(&both, "--default-platform=none"));
}

#[test]
fn the_compositions_apply_the_edits_of_a_farm_to_the_units_defaults() {
    // Catches: an edit of the farm that goes missing or lands on the wrong role: the
    // frontend's SQLite store, the nodes' directory cold store, scrape route and
    // registration with the frontend, and the limits over them.
    let w = wiring();
    let place = Settings {
        root: Path::new("/R"),
        host: "127.0.0.1",
        storage_port: 5555,
        wiring: &w,
    };
    let topo = Topology::beta();
    let front: Unit = frontend(&place, &topo);
    assert!(has(&front, "--state-store=sqlite:///R/state/state.db"));
    assert!(has(&front, "--default-platform=none"));
    assert!(has(&front, "--drain-lb-wait=0"));
    let first: Unit = node(&place, 9999, &topo);
    assert_eq!(first.mode, Mode::Storage);
    assert!(has(&first, "--cold-store=file:///R/cold"));
    assert!(has(&first, "--metrics-listen=127.0.0.1:9999"));
    assert!(has(&first, "--frontend=https://localhost:2222"));
    assert!(has(&first, "--advertise-addr=127.0.0.1:5555"));
    assert!(has(&first, "--hot-store-max-bytes=268435456"));
    // The rehearsal's limits are the cold store's floor only.
    let rehearsal: Unit = node(&place, 9999, &Topology::rehearsal(2));
    assert!(has(&rehearsal, "--cold-store-floor-bytes=0"));
    assert!(
        rehearsal
            .flags()
            .iter()
            .all(|f| !f.starts_with("--hot-store-max-bytes"))
    );
}

#[test]
fn the_keys_of_a_file_are_its_live_and_commented_settings_under_their_tables() {
    // Catches: a help line that holds ` = ` taken for a key, a commented setting lost, a
    // setting under the table above it, a switch taken for a value flag or a value flag for
    // a switch (`kbf` refuses a switch given a string).
    let got = parse_keys(
        "# kbf x: a header\n# Prefix with a = b inside\n[one]\nlive = 5\n\
         # commented = \"SHAPE\"\n# Set a = b\nswitch = false\n[two]\nother = \"x\"\n",
    );
    assert_eq!(
        got.into_iter().collect::<Vec<_>>(),
        [
            ("commented", ("one", false)),
            ("live", ("one", false)),
            ("other", ("two", false)),
            ("switch", ("one", true)),
        ]
    );
}

#[test]
fn a_unit_that_starts_another_binary_or_mode_is_refused() {
    // Catches: a packaged unit that starts a program other than `kbf`, another mode, or
    // none, read as the farm's command line: the farm would start the wrong processes.
    let ok = "[Service]\nExecStart=/var/lib/x/kbf --mode=storage\n";
    assert_eq!(mode_word_in(ok, Mode::Storage), "--mode=storage");
    for bad in [
        "[Service]\nExecStart=/var/lib/x/kbf --mode=frontend\n",
        "[Service]\nExecStart=/var/lib/x/other --mode=storage\n",
        "[Service]\nExecStart=/var/lib/x/kbf storage\n",
        "[Service]\nRestart=no\n",
    ] {
        let refused = std::panic::catch_unwind(|| mode_word_in(bad, Mode::Storage));
        assert!(refused.is_err(), "{bad}");
    }
}
