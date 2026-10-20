//! The test farm's hosts, as they are today, through the migration: the files under
//! `testdata/testfarm/` are what the dev installer, the old helper and the deploy kit left
//! (the kit's own test keeps the units equal to what `render-units` writes). Each test
//! migrates one and then does what the host does next: reads the unit file the migration
//! wrote and checks that the node would start from it, with the command line it had.
//!
//! The release binaries are plain bytes, not programs: the repository's interpreter budget
//! allows no script, and the version probe is played by the rig's runner. So "the node
//! starts" is shown by what a start needs and the migration could break: the program the
//! unit names is a regular, executable file in the installer's tree, root's, with the
//! bytes of the release that ran, and the unit passes the arguments it passed before.
//! (kbf-deploy's test of the same fixtures runs the kit's rules on the migrated unit.)

use std::fs;
use std::os::unix::fs::MetadataExt;

use super::rig::*;
use super::*;

const NEW_BIN: &str = "/var/lib/kbf-installer/releases/current";
const MAC_NEW_BIN: &str = "/usr/local/kbf-installer/releases/current";

/// The migration of `rig` with the farm's units.
fn migrate_farm(rig: &mut Rig, units: &[&str], mode: Mode) -> Report {
    let mut m = rig.migration();
    m.units = units.iter().map(|u| unit(u)).collect();
    migrate(
        &m,
        &mut rig.host,
        &Hooks {
            runner: &runner(),
            verifier: &NotWired,
            frontend: &NoFrontend,
        },
        mode,
    )
    .expect("the farm host migrates")
}

/// The command line of a systemd unit's `ExecStart=`: the program and its arguments, with
/// the `\` continuations joined.
fn exec_start(unit: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut lines = unit.lines();
    while let Some(line) = lines.next() {
        let Some(mut rest) = line.strip_prefix("ExecStart=") else {
            continue;
        };
        let mut joined = String::new();
        loop {
            match rest.strip_suffix('\\') {
                Some(head) => {
                    joined.push_str(head);
                    joined.push(' ');
                    rest = lines.next().expect("a continued line");
                }
                None => {
                    joined.push_str(rest);
                    break;
                }
            }
        }
        words = joined.split_whitespace().map(str::to_owned).collect();
        break;
    }
    assert!(!words.is_empty(), "no ExecStart= in:\n{unit}");
    words
}

/// The `ProgramArguments` of a property list: its `<string>` elements before `</array>`.
fn program_arguments(plist: &str) -> Vec<String> {
    plist
        .split("<key>ProgramArguments</key>")
        .nth(1)
        .and_then(|rest| rest.split("</array>").next())
        .expect("ProgramArguments")
        .lines()
        .filter_map(|l| l.trim().strip_prefix("<string>")?.strip_suffix("</string>"))
        .map(str::to_owned)
        .collect()
}

/// What a start of the node needs from the unit: its program resolves, under the scratch
/// root, to a regular executable file that is the bytes of the release `commit`, and the
/// arguments are `args`. Returns the program's path.
fn assert_starts(rig: &Rig, command: &[String], commit: &str, args: &[String]) {
    let program = rig.at(command[0].trim_start_matches('/'));
    let meta = fs::metadata(&program).expect("the program the unit names is there");
    assert!(meta.is_file(), "{}", program.display());
    assert_eq!(
        meta.mode() & 0o111,
        0o111,
        "executable for all, as a release's are"
    );
    assert_eq!(meta.mode() & 0o022, 0, "not writable by group or others");
    assert_eq!(meta.uid(), me());
    assert_eq!(
        fs::read_to_string(&program).expect("binary"),
        farm_binary(commit)
    );
    assert_eq!(command[1..], *args);
}

/// Catches a migration that does not work on the hosts it exists for. A Linux test-farm
/// host (daemon and storage-and-daemon units, the old helper, the helper's memory as the
/// deployed build wrote it) comes over: the floor carries the helper's last order, both
/// releases are adopted unverified, each unit differs from the kit's output only in the
/// path of the binary, and from that unit the node starts with the arguments it had. The old
/// tree and state are as they were.
///
/// This was red before `old::read` accepted `handled_entry`, a key every deployed helper
/// writes: the migration stopped with "helper.json is damaged". Mutants seen red: the
/// `handled_entry` key refused again; a unit rewritten wholesale (its arguments lost); the
/// old tree touched; the units started before the links moved.
#[test]
fn a_linux_test_farm_host_migrates_and_its_node_starts() {
    let mut rig = Rig::testfarm_linux("migrate");
    let old_tree = rig.snapshot("usr/local/kbf");
    let old_state = rig.snapshot("var/lib/kbf-deploy");
    let units = ["kbf-daemon.service", "kbf-storage-and-daemon.service"];
    let originals = [FARM_DAEMON_UNIT, FARM_SD_UNIT];

    let report = migrate_farm(&mut rig, &units, Mode::Apply);
    assert_eq!(report.outcome, Outcome::Migrated);
    assert_eq!(
        report.unverified,
        vec![commit(FARM_X), commit(FARM_Y)],
        "no attestation exists on the farm's hosts: unverified, said so"
    );

    // The state the installer starts from.
    let state = rig.json(&rig.new_state_dir().join("state.json"));
    assert_eq!(
        (&state["schema"], &state["current"], &state["previous"]),
        (&2.into(), &FARM_X.into(), &FARM_Y.into())
    );
    assert_eq!(
        state["floor"],
        serde_json::json!({"epoch": 2, "order_id": 41, "generation": 17}),
        "the helper's last order is the floor"
    );
    for commit in [FARM_X, FARM_Y] {
        let dir = rig.new_tree().join("releases").join(commit);
        assert!(dir.join("kbf").is_file(), "{commit}");
        assert!(!dir.join("VERIFIED").exists(), "{commit}");
        assert_eq!(
            fs::read_to_string(dir.join("kbf")).expect("binary"),
            farm_binary(commit)
        );
    }

    // The units: only the binary's directory changed, and the node starts from them.
    for (name, original) in units.iter().zip(originals) {
        let now = fs::read_to_string(rig.at(&format!("etc/systemd/system/{name}"))).expect("unit");
        assert_eq!(
            now,
            original.replace("/usr/local/kbf/current", NEW_BIN),
            "{name}"
        );
        let command = exec_start(&now);
        assert_eq!(command[0], format!("{NEW_BIN}/kbf"));
        let before = exec_start(original);
        assert_eq!(before[0], "/usr/local/kbf/current/kbf");
        assert_starts(&rig, &command, FARM_X, &before[1..]);
    }

    // The old layout is still there for a rollback, and only a note was added to it.
    assert_eq!(rig.snapshot("usr/local/kbf"), old_tree);
    let mut state_now = rig.snapshot("var/lib/kbf-deploy");
    assert!(state_now.remove("MIGRATED").is_some());
    assert_eq!(state_now, old_state);

    // Services: the three that ran were stopped before any link existed, and the two units
    // run again after everything was written, from the new tree; the old helper stays off.
    let events = rig.events.borrow().clone();
    let off: Vec<usize> = (0..events.len())
        .filter(|&i| events[i].starts_with("stop ") || events[i].starts_with("disable "))
        .collect();
    let on: Vec<usize> = (0..events.len())
        .filter(|&i| events[i].starts_with("start "))
        .collect();
    assert_eq!((off.len(), on.len()), (4, 2), "{events:?}");
    assert!(
        off.iter().all(|&i| events[i].ends_with("links=false")),
        "{events:?}"
    );
    assert!(
        on.iter().all(|&i| events[i].ends_with("links=true")),
        "{events:?}"
    );
    assert!(off.iter().max() < on.iter().min(), "{events:?}");
    let active = rig.active.borrow().clone();
    assert!(
        active.contains("kbf-daemon.service") && active.contains("kbf-storage-and-daemon.service")
    );
    assert!(!active.contains("kbf-helper.service"), "{active:?}");
}

/// Catches a rollback that does not give the farm host back: the units are the kit's
/// output again, byte for byte, and the node starts from the old tree with the same
/// arguments, as it did before.
#[test]
fn a_rolled_back_test_farm_host_starts_its_node_from_the_old_tree() {
    let mut rig = Rig::testfarm_linux("rollback");
    let units = ["kbf-daemon.service", "kbf-storage-and-daemon.service"];
    migrate_farm(&mut rig, &units, Mode::Apply);
    let mut m = rig.migration();
    m.units = units.iter().map(|u| unit(u)).collect();
    rollback(&m, &mut rig.host, Mode::Apply).expect("rollback");
    for (name, original) in units.iter().zip([FARM_DAEMON_UNIT, FARM_SD_UNIT]) {
        let now = fs::read_to_string(rig.at(&format!("etc/systemd/system/{name}"))).expect("unit");
        assert_eq!(now, original, "{name}");
        let command = exec_start(&now);
        assert_eq!(command[0], "/usr/local/kbf/current/kbf");
        assert_starts(&rig, &command, FARM_X, &exec_start(original)[1..]);
    }
}

/// The same for a test-farm Mac: the daemon's LaunchDaemon plist keeps every argument and
/// runs `kbf-daemon` from the installer's tree, and the node starts from it.
#[test]
fn a_macos_test_farm_host_migrates_and_its_node_starts() {
    let mut rig = Rig::testfarm_mac();
    let report = migrate_farm(&mut rig, &["org.example.kbf-daemon.plist"], Mode::Apply);
    assert_eq!(report.outcome, Outcome::Migrated);
    let now = fs::read_to_string(rig.at("Library/LaunchDaemons/org.example.kbf-daemon.plist"))
        .expect("plist");
    assert_eq!(
        now,
        FARM_PLIST.replace("/usr/local/kbf/current", MAC_NEW_BIN)
    );
    let command = program_arguments(&now);
    assert_eq!(command[0], format!("{MAC_NEW_BIN}/kbf-daemon"));
    let before = program_arguments(FARM_PLIST);
    assert_starts(&rig, &command, FARM_X, &before[1..]);
    let state = rig.json(&rig.new_state_dir().join("state.json"));
    assert_eq!(state["current"], FARM_X);
}
