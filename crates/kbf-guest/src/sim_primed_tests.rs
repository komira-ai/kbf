//! Tests of the root steps a clone's `sim` skips because the base's priming boot did them:
//! which steps, on what evidence, and what the frames and the summary say. On the fake host
//! of the `sim` tests, with `staged.json` and the record in the shapes `image setup` and
//! `mkuser` write. Nothing here waits on a clock.

use serde_json::{Value, json};

use super::*;
use crate::mkuser_tests::prime::{
    BUNDLE, DMG, VERSION_PLIST, bundle_entry, dmg_entry, identity, staged_both, staged_json,
    xcode_entry,
};
use crate::sim_primed::{Primed, SKIPPED_NOTE};
use crate::simpaths;

const HASH_A: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
const HASH_B: &str = "1973223a21d99bbbe64bbe6d31aca7aeb155664eead280132676bc6695a79a37";

/// A record as `mkuser` writes it (the shape is pinned by the mkuser tests).
fn record_of(xcode: &Value, runtimes: &[Value]) -> String {
    let list: Vec<Value> = runtimes
        .iter()
        .map(|r| {
            let action = if r["kind"] == "bundle" {
                "import"
            } else {
                "add"
            };
            json!({"name": r["name"], "action": action, "staged": r})
        })
        .collect();
    format!(
        "{:#}\n",
        json!({
            "version": 1,
            "xcode_path": "/Applications/Xcode.app",
            "xcode": xcode,
            "xcode_identity": identity(),
            "runtimes": list,
        })
    )
}

fn host_with(staged: Option<&str>, record: Option<&str>) -> Fake {
    let mut host = Fake::new();
    host.runtimes = vec![DMG.to_owned(), BUNDLE.to_owned()];
    host.files.insert(
        simpaths::XCODE_VERSION_PLIST.to_owned(),
        VERSION_PLIST.to_vec(),
    );
    for (path, text) in [(simpaths::MANIFEST, staged), (simpaths::PRIMED, record)] {
        if let Some(t) = text {
            host.files.insert(path.to_owned(), t.as_bytes().to_vec());
        }
    }
    host
}

fn primed_record() -> String {
    record_of(
        &xcode_entry(1000),
        &[dmg_entry(DMG, HASH_A), bundle_entry(BUNDLE, HASH_B)],
    )
}

const ROOT_WORDS: [&str; 5] = [
    "xcode-select",
    "-license",
    "-runFirstLaunch",
    "runtime add",
    "-importPlatform",
];

fn root_ran(host: &Fake) -> Vec<&'static str> {
    let ran = host.commands();
    ROOT_WORDS
        .into_iter()
        .filter(|w| ran.iter().any(|c| c.contains(w)))
        .collect()
}

fn note_of(host: &Fake, id: &str) -> (Status, Option<String>) {
    let frames = host.step_frames();
    let f = frames.iter().find(|f| f.step == id).expect(id);
    (f.status, f.note.clone())
}

/// Catches a clone paying the root steps the prime did: with a record that matches
/// `staged.json`, the five root commands are not run, each has a frame that is `ok`, says
/// `skipped: done at prime` and still names its command, the user steps run as always, and
/// the summary lists the same steps in the same order as an unprimed run (the host's report
/// reads that shape).
#[test]
fn a_matching_record_skips_the_root_steps_and_keeps_the_report_shape() {
    let primed = host_with(Some(&staged_both()), Some(&primed_record()));
    let done = go(&primed);
    let unprimed = host_with(None, None);
    let base = go(&unprimed);
    assert!(done.ok, "{done:?}");
    assert_eq!(root_ran(&primed), Vec::<&str>::new());
    assert_eq!(root_ran(&unprimed), ROOT_WORDS);
    assert_eq!(done.steps, base.steps);
    let frames = primed.step_frames();
    assert_eq!(frames.len(), unprimed.step_frames().len());
    for id in [
        "xcode-select",
        "license",
        "first-launch",
        "runtime-add-iOS_27.dmg",
        "runtime-import-iOS_27.exportedBundle",
    ] {
        let f = frames.iter().find(|f| f.step == id).expect(id);
        assert_eq!(f.status, Status::Ok, "{id}");
        assert_eq!(f.who, Who::Root, "{id}");
        assert_eq!(f.note.as_deref(), Some(SKIPPED_NOTE), "{id}");
        assert!(f.argv[0].starts_with("/usr/bin/"), "{id}: {:?}", f.argv);
        assert_eq!(
            (f.exit, f.ms, f.skipped_because.clone()),
            (None, 0, None),
            "{id}"
        );
    }
    // The user steps ran, and the frames for them are as before.
    assert!(
        primed
            .commands()
            .iter()
            .any(|c| c.contains("simctl create"))
    );
    assert_eq!(
        note_of(&primed, "list-runtimes")
            .1
            .as_deref()
            .map(|n| n.starts_with("took")),
        Some(true)
    );
}

/// Catches a record that is trusted when it should not be: no record, no `staged.json`, a
/// record of another version, another Xcode path, one that does not parse, or one that is
/// too big all leave every root step to run, as an unprimed base does.
#[test]
fn without_a_usable_record_every_root_step_runs() {
    let good = primed_record();
    let mut other_version: Value = serde_json::from_str(&good).expect("json");
    other_version["version"] = json!(2);
    let mut other_path: Value = serde_json::from_str(&good).expect("json");
    other_path["xcode_path"] = json!("/Applications/Xcode-27.app");
    let mut no_runtimes: Value = serde_json::from_str(&good).expect("json");
    no_runtimes
        .as_object_mut()
        .expect("object")
        .remove("runtimes");
    let cases: Vec<(&str, Option<String>, Option<String>)> = vec![
        ("no record", Some(staged_both()), None),
        ("no staged.json", None, Some(good.clone())),
        (
            "staged.json not JSON",
            Some("{".to_owned()),
            Some(good.clone()),
        ),
        (
            "staged.json without runtimes",
            Some("{\"xcode\":{}}".to_owned()),
            Some(good.clone()),
        ),
        (
            "record not JSON",
            Some(staged_both()),
            Some("nope".to_owned()),
        ),
        (
            "record version",
            Some(staged_both()),
            Some(other_version.to_string()),
        ),
        (
            "record path",
            Some(staged_both()),
            Some(other_path.to_string()),
        ),
        (
            "record without runtimes",
            Some(staged_both()),
            Some(no_runtimes.to_string()),
        ),
        (
            "record over the cap",
            Some(staged_both()),
            Some(" ".repeat(crate::sim_primed::MAX_READ_BYTES + 1)),
        ),
    ];
    for (label, staged, record) in cases {
        let host = host_with(staged.as_deref(), record.as_deref());
        go(&host);
        assert_eq!(root_ran(&host), ROOT_WORDS, "{label}");
        assert!(
            host.step_frames()
                .iter()
                .all(|f| f.note.as_deref() != Some(SKIPPED_NOTE)),
            "{label}"
        );
    }
}

/// Catches a step skipped on evidence that is stale: a runtime added after the prime, a
/// runtime restaged with other bytes, an Xcode restaged with another size, and a runtime
/// that is on the disk but not in `staged.json`. Each runs exactly the steps it changes and
/// skips the rest, and the frames name the reason for those skipped.
#[test]
fn a_step_whose_entry_differs_from_the_record_runs() {
    let record = record_of(&xcode_entry(1000), &[dmg_entry(DMG, HASH_A)]);
    // A runtime added after the prime.
    let host = host_with(Some(&staged_both()), Some(&record));
    assert!(go(&host).ok);
    assert_eq!(root_ran(&host), ["-importPlatform"]);
    for id in [
        "xcode-select",
        "license",
        "first-launch",
        "runtime-add-iOS_27.dmg",
    ] {
        assert_eq!(
            note_of(&host, id),
            (Status::Ok, Some(SKIPPED_NOTE.to_owned())),
            "{id}"
        );
    }
    assert_eq!(
        note_of(&host, "runtime-import-iOS_27.exportedBundle"),
        (Status::Ok, None)
    );

    // A runtime restaged with other bytes.
    let restaged = staged_json(
        &xcode_entry(1000),
        &[dmg_entry(DMG, HASH_B), bundle_entry(BUNDLE, HASH_B)],
    );
    let both = primed_record();
    let host = host_with(Some(&restaged), Some(&both));
    go(&host);
    assert_eq!(root_ran(&host), ["runtime add"]);
    assert_eq!(note_of(&host, "runtime-add-iOS_27.dmg").1, None);
    assert_eq!(
        note_of(&host, "runtime-import-iOS_27.exportedBundle")
            .1
            .as_deref(),
        Some(SKIPPED_NOTE)
    );

    // A bundle restaged with one file changed.
    let restaged = staged_json(
        &xcode_entry(1000),
        &[dmg_entry(DMG, HASH_A), bundle_entry(BUNDLE, HASH_A)],
    );
    let host = host_with(Some(&restaged), Some(&both));
    go(&host);
    assert_eq!(root_ran(&host), ["-importPlatform"]);

    // Another Xcode: its three steps run; the runtimes' entries are unchanged.
    let restaged = staged_json(
        &xcode_entry(2000),
        &[dmg_entry(DMG, HASH_A), bundle_entry(BUNDLE, HASH_B)],
    );
    let host = host_with(Some(&restaged), Some(&both));
    go(&host);
    assert_eq!(
        root_ran(&host),
        ["xcode-select", "-license", "-runFirstLaunch"]
    );
    assert_eq!(note_of(&host, "xcode-select").1, None);
    assert_eq!(
        note_of(&host, "runtime-add-iOS_27.dmg").1.as_deref(),
        Some(SKIPPED_NOTE)
    );

    // A runtime on the disk that `staged.json` does not list has nothing to match.
    let only_dmg = staged_json(&xcode_entry(1000), &[dmg_entry(DMG, HASH_A)]);
    let host = host_with(Some(&only_dmg), Some(&both));
    go(&host);
    assert_eq!(root_ran(&host), ["-importPlatform"]);
}

/// Catches a failing root command being hidden by the skip logic: a step that is not
/// covered still fails the run as before, and what it blocks is skipped with its name.
#[test]
fn an_uncovered_step_that_fails_still_blocks_what_needs_it() {
    let record = record_of(&xcode_entry(1000), &[dmg_entry(DMG, HASH_A)]);
    let host = host_with(Some(&staged_both()), Some(&record));
    host.answer("-importPlatform", ended(Ended::Exited(1)));
    let done = go(&host);
    assert!(!done.ok);
    assert_eq!(
        note_of(&host, "runtime-import-iOS_27.exportedBundle").0,
        Status::Failed
    );
    let frames = host.step_frames();
    let list = frames
        .iter()
        .find(|f| f.step == "list-runtimes")
        .expect("list");
    assert_eq!(list.status, Status::Skipped);
    assert_eq!(
        list.skipped_because.as_deref(),
        Some("runtime-import-iOS_27.exportedBundle")
    );
}

/// Catches a plan built by a caller (`run_plan`) being altered by a record it did not ask
/// about: only `run`, which reads the guest's files, skips.
#[test]
fn a_plan_the_caller_built_runs_whatever_the_record_says() {
    let host = host_with(Some(&staged_both()), Some(&primed_record()));
    let steps = plan(&Inputs {
        runtimes: vec![DMG.to_owned()],
        ..Inputs::default()
    });
    run_plan(&host, &cfg(), &steps, &mut Vec::new());
    assert_eq!(
        root_ran(&host),
        ["xcode-select", "-license", "-runFirstLaunch", "runtime add"]
    );
}

/// Catches the two halves disagreeing: what the priming boot wrote, byte for byte, must be
/// what a clone's `sim` skips on, for the files `image setup` wrote.
#[test]
fn what_the_priming_boot_wrote_is_what_the_clone_skips_on() {
    let mk = crate::mkuser_tests::prime::host_staged(&staged_both());
    let done = crate::mkuser_tests::go(&mk);
    assert!(done.summary.ok);
    let mut host = Fake::new();
    host.runtimes = vec![DMG.to_owned(), BUNDLE.to_owned()];
    for path in [
        simpaths::MANIFEST,
        simpaths::PRIMED,
        simpaths::XCODE_VERSION_PLIST,
    ] {
        host.files
            .insert(path.to_owned(), mk.files.borrow()[path].clone());
    }
    assert!(go(&host).ok);
    assert_eq!(root_ran(&host), Vec::<&str>::new());
    // Restage after the prime with a third runtime: that one alone is added.
    let third = dmg_entry("iOS_28.dmg", HASH_B);
    let restaged = staged_json(
        &xcode_entry(1000),
        &[dmg_entry(DMG, HASH_A), bundle_entry(BUNDLE, HASH_B), third],
    );
    host.files
        .insert(simpaths::MANIFEST.to_owned(), restaged.into_bytes());
    host.runtimes.push("iOS_28.dmg".to_owned());
    let again = Fake {
        files: host.files.clone(),
        runtimes: host.runtimes.clone(),
        ..Fake::new()
    };
    go(&again);
    assert_eq!(root_ran(&again), ["runtime add"]);
    assert!(again.commands().iter().any(|c| c.contains("iOS_28.dmg")));
    assert!(again.commands().iter().all(|c| !c.contains("iOS_27.dmg")));
}

/// Catches `covers` answering for a step it knows nothing about (a user step, a runtime
/// name with nothing after the prefix, an empty id) or for a runtime the record lists
/// but `staged.json` no longer does.
#[test]
fn only_the_root_steps_of_the_probe_can_be_covered() {
    let read = |staged: String, record: String| {
        Primed::load(&move |path: &str, _cap: usize| {
            if path == simpaths::MANIFEST {
                Ok(staged.clone().into_bytes())
            } else if path == simpaths::XCODE_VERSION_PLIST {
                Ok(VERSION_PLIST.to_vec())
            } else if path == simpaths::PRIMED {
                Ok(record.clone().into_bytes())
            } else {
                Err("no".to_owned())
            }
        })
    };
    let p = read(staged_both(), primed_record());
    for id in [
        "xcode-select",
        "license",
        "first-launch",
        "runtime-add-iOS_27.dmg",
        "runtime-import-iOS_27.exportedBundle",
    ] {
        assert!(p.covers(id), "{id}");
    }
    for id in [
        "",
        "list-runtimes",
        "create",
        "boot",
        "runtime-add-",
        "runtime-import-",
        "runtime-add-x.dmg",
        "runtime-",
        "xcode-select ",
    ] {
        assert!(!p.covers(id), "{id:?}");
    }
    let p = read(staged_json(&xcode_entry(1000), &[]), primed_record());
    assert!(p.covers("license") && !p.covers("runtime-add-iOS_27.dmg"));
    assert!(!Primed::default().covers("license"));
}

/// Catches the argument list of a fixed step being lost, and a late one inventing one.
#[test]
fn only_a_fixed_argument_list_is_given() {
    use crate::sim::Late;
    use crate::sim_primed::fixed_argv;
    assert_eq!(fixed_argv(Argv::Fixed(vec!["a".into()])), ["a"]);
    assert!(fixed_argv(Argv::Late(Late::Create)).is_empty());
}

/// Catches a different Xcode with the same tree sizes and file counts being taken for the
/// primed one: the three Xcode steps run when the `version.plist` on the disk is not the one
/// the record kept (or cannot be read), and the runtimes' steps, whose entries did not
/// change, are still skipped.
#[test]
fn another_xcode_with_the_same_counts_is_not_the_primed_one() {
    for plist in [Some(b"<plist>27B2</plist>".to_vec()), None] {
        let mut host = host_with(Some(&staged_both()), Some(&primed_record()));
        match plist {
            Some(p) => {
                host.files
                    .insert(simpaths::XCODE_VERSION_PLIST.to_owned(), p);
            }
            None => {
                host.files.remove(simpaths::XCODE_VERSION_PLIST);
            }
        }
        go(&host);
        assert_eq!(
            root_ran(&host),
            ["xcode-select", "-license", "-runFirstLaunch"]
        );
        assert_eq!(
            note_of(&host, "runtime-add-iOS_27.dmg").1.as_deref(),
            Some(SKIPPED_NOTE)
        );
    }
    // A record with no identity (an unreadable plist at prime) covers no Xcode step either.
    let mut record: Value = serde_json::from_str(&primed_record()).expect("json");
    record["xcode_identity"] = Value::Null;
    let host = host_with(Some(&staged_both()), Some(&record.to_string()));
    go(&host);
    assert_eq!(
        root_ran(&host),
        ["xcode-select", "-license", "-runFirstLaunch"]
    );
    // Nothing known on either side is not a match.
    let mut host = host_with(Some(&staged_both()), Some(&record.to_string()));
    host.files.remove(simpaths::XCODE_VERSION_PLIST);
    go(&host);
    assert_eq!(
        root_ran(&host),
        ["xcode-select", "-license", "-runFirstLaunch"]
    );
}
