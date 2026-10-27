//! Tests of the simulator's root setup in the priming boot: the steps `mkuser` adds before
//! the ready marker, what a failure leaves, the record, and a base whose user is ready
//! already. On the fake host of the mkuser tests, with `staged.json` in the shapes
//! `kbf-vmm image setup` writes (`kbf-vmm`'s stage tests pin them from the other side).
//! Nothing waits on a clock.

use serde_json::{Value, json};

use super::*;
use crate::frame::{Frame, Kind, SendError};
use crate::mkuser_prime::{ASK_ATTEMPTS, Prep, QUESTION, RootSetup, Signal, plan_prep};
use crate::sim_primed::{SKIPPED_NOTE, parse_staged, record_json};

pub(crate) const DMG: &str = "iOS_27.dmg";
pub(crate) const BUNDLE: &str = "iOS_27.exportedBundle";
const HASH_A: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
const HASH_B: &str = "1973223a21d99bbbe64bbe6d31aca7aeb155664eead280132676bc6695a79a37";

/// The entry of the staged Xcode, as `image setup` writes it.
pub(crate) fn xcode_entry(bytes: u64) -> Value {
    json!({
        "dest": "/Applications/Xcode.app",
        "method": "clone",
        "bytes": bytes,
        "files": 3,
        "dirs": 2,
        "symlinks": 3,
        "external_links": [],
    })
}

/// The entry of a staged disk image.
pub(crate) fn dmg_entry(name: &str, sha256: &str) -> Value {
    json!({
        "name": name,
        "kind": "dmg",
        "bytes": 5,
        "sha256": sha256,
        "dest": format!("/private/var/kbf-sim/runtimes/{name}"),
    })
}

/// The entry of a staged exported bundle (one entry per file).
pub(crate) fn bundle_entry(name: &str, sha256: &str) -> Value {
    json!({
        "name": name,
        "kind": "bundle",
        "bytes": 20,
        "files": [
            {"path": "ExportedMetadata.plist", "bytes": 4, "sha256": HASH_A},
            {"path": "Restore/090-cryptex.dmg", "bytes": 7, "sha256": sha256},
        ],
        "dest": format!("/private/var/kbf-sim/runtimes/{name}"),
    })
}

/// `staged.json` with `xcode` and `runtimes`.
pub(crate) fn staged_json(xcode: &Value, runtimes: &[Value]) -> String {
    format!(
        "{:#}\n",
        json!({"xcode": xcode, "app": null, "tests": null, "runtimes": runtimes})
    )
}

/// The setup of one Xcode, one image and one bundle.
pub(crate) fn staged_both() -> String {
    staged_json(
        &xcode_entry(1000),
        &[dmg_entry(DMG, HASH_A), bundle_entry(BUNDLE, HASH_B)],
    )
}

/// The Xcode's `version.plist` on the fake guest, and the identity the record keeps of it.
pub(crate) const VERSION_PLIST: &[u8] =
    b"<plist><key>ProductBuildVersion</key><string>27A1</string></plist>";

pub(crate) fn identity() -> String {
    hex::encode(VERSION_PLIST)
}

/// A fake guest booted as a clone is: the host answers the prime question with no, and the
/// setup staged `staged`.
pub(crate) fn clone_staged(staged: &str) -> Fake {
    let host = Fake::new();
    let mut files = host.files.borrow_mut();
    files.insert(simpaths::MANIFEST.to_owned(), staged.as_bytes().to_vec());
    files.insert(
        simpaths::XCODE_VERSION_PLIST.to_owned(),
        VERSION_PLIST.to_vec(),
    );
    drop(files);
    *host.answer.borrow_mut() = Ok("no".to_owned());
    host
}

/// A fake guest in the base's priming boot: as [`clone_staged`], with the host saying yes.
pub(crate) fn host_staged(staged: &str) -> Fake {
    let host = clone_staged(staged);
    *host.answer.borrow_mut() = Ok("yes".to_owned());
    host
}

fn frame<'a>(frames: &'a [StepFrame], id: &str) -> &'a StepFrame {
    frames.iter().find(|f| f.step == id).expect(id)
}

fn step_ids(host: &Fake) -> Vec<String> {
    host.step_frames().into_iter().map(|f| f.step).collect()
}

/// The root commands the probe runs for the two runtimes of `staged_both`.
fn root_commands() -> Vec<String> {
    vec![
        "/usr/bin/xcode-select -s /Applications/Xcode.app".to_owned(),
        "/usr/bin/xcodebuild -license accept".to_owned(),
        "/usr/bin/xcodebuild -runFirstLaunch".to_owned(),
        format!("/usr/bin/xcrun simctl runtime add /private/var/kbf-sim/runtimes/{DMG}"),
        format!("/usr/bin/xcodebuild -importPlatform /private/var/kbf-sim/runtimes/{BUNDLE}"),
    ]
}

/// The commands that are not the account's, the audit's or the power-off.
fn prep_commands(host: &Fake) -> Vec<String> {
    host.commands()
        .into_iter()
        .filter(|c| c.contains("xcode") || c.contains("simctl"))
        .collect()
}

/// Catches the four root steps not running in the priming boot (select, license, first
/// launch, each runtime), running after the ready marker, running in another order or with
/// other commands than the probe's, or not as steps of their own (a frame each, named like
/// the account's).
#[test]
fn the_priming_boot_runs_the_root_steps_before_the_ready_marker() {
    let host = host_staged(&staged_both());
    let done = go(&host);
    assert!(done.summary.ok, "{:?}", done.summary);
    assert_eq!(prep_commands(&host), root_commands());
    let ids = step_ids(&host);
    let at = |id: &str| ids.iter().position(|i| i == id).expect(id);
    let tail: Vec<&str> = ids[at("updates-critical") + 1..]
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(
        tail,
        [
            "prime-reset",
            "xcode-select",
            "license",
            "first-launch",
            "runtime-add-iOS_27.dmg",
            "runtime-import-iOS_27.exportedBundle",
            "prime-record",
            "ready-marker",
        ]
    );
    let names: Vec<String> = host.sent.borrow().iter().map(|f| f.name.clone()).collect();
    for (i, id) in ids.iter().enumerate() {
        assert!(names.contains(&step_frame_name(i, id)), "{id}");
    }
    // The commands ran before the record, and the record before the marker.
    let writes = host.writes();
    let write_at = |p: &str| writes.iter().position(|w| w.0 == p).expect(p);
    assert!(write_at(simpaths::PRIMED) < write_at(simpaths::USER_READY));
    let calls = host.calls.borrow();
    let call_at = |f: &dyn Fn(&Call) -> bool| calls.iter().position(f).expect("call");
    let import =
        call_at(&|c| matches!(c, Call::Run(a) if a.iter().any(|x| x == "-importPlatform")));
    let record = call_at(&|c| matches!(c, Call::Write(p, _, _) if p == simpaths::PRIMED));
    let marker = call_at(&|c| matches!(c, Call::Write(p, _, _) if p == simpaths::USER_READY));
    assert!(import < record && record < marker);
    // Every step needs only earlier ones, and the marker needs the record.
    let prep = plan_prep(&host);
    let steps = plan_with("kbfuser", false, &prep);
    for (i, s) in steps.iter().enumerate() {
        for n in &s.needs {
            assert!(steps[..i].iter().any(|x| &x.id == n), "{} needs {n}", s.id);
        }
    }
    let marker_step = steps
        .iter()
        .find(|s| s.id == "ready-marker")
        .expect("marker");
    assert!(marker_step.needs.contains(&"prime-record".to_owned()));
}

/// Catches a slow command held to the quick deadline (a runtime import killed after two
/// minutes), a quick one given the slow one, and a frame that states another deadline than
/// the one the command ran with.
#[test]
fn each_root_command_runs_with_the_deadline_of_its_class() {
    let host = host_staged(&staged_both());
    let args = args(&["--first-launch-secs", "31", "--runtime-add-secs", "37"]);
    let done = run(&host, &Config::from(&args), "kbfuser", &mut Vec::new());
    assert!(done.summary.ok);
    let ran = host.deadlines.borrow();
    let secs = |needle: &str| {
        ran.iter()
            .find(|(c, _)| c.contains(needle))
            .map(|(_, d)| d.as_secs())
            .expect(needle)
    };
    assert_eq!(secs("xcode-select"), 120);
    assert_eq!(secs("-license"), 120);
    assert_eq!(secs("-runFirstLaunch"), 31);
    assert_eq!(secs("runtime add"), 37);
    assert_eq!(secs("-importPlatform"), 37);
    let frames = host.step_frames();
    assert_eq!(frame(&frames, "license").deadline_secs, 120);
    assert_eq!(frame(&frames, "first-launch").deadline_secs, 31);
    assert_eq!(frame(&frames, "runtime-add-iOS_27.dmg").deadline_secs, 37);
    assert_eq!(
        frame(&frames, "runtime-import-iOS_27.exportedBundle").deadline_secs,
        37
    );
}

/// Catches a half-primed base being used: a runtime that fails to import must leave no
/// ready marker and no record, skip the record and the marker with the reason, and still
/// power the guest off (a failed run starts again from the top).
#[test]
fn a_failed_import_leaves_neither_record_nor_ready_marker() {
    for (label, ended) in [
        ("exit", Ended::Exited(1)),
        ("timeout", Ended::TimedOut),
        ("signal", Ended::Signaled(9)),
    ] {
        let host = host_staged(&staged_both());
        host.answer("-importPlatform", out(ended, "", "no space left"));
        let done = go(&host);
        assert!(!done.summary.ok, "{label}");
        assert_eq!(done.power_off_started, Some(true), "{label}");
        assert!(
            host.writes()
                .iter()
                .all(|w| w.0 != simpaths::USER_READY && w.0 != simpaths::PRIMED),
            "{label}: {:?}",
            host.writes()
        );
        let frames = host.step_frames();
        assert_ne!(
            frame(&frames, "runtime-import-iOS_27.exportedBundle").status,
            Status::Ok,
            "{label}"
        );
        let record = frame(&frames, "prime-record");
        assert_eq!(record.status, Status::Skipped, "{label}");
        assert_eq!(
            record.skipped_because.as_deref(),
            Some("runtime-import-iOS_27.exportedBundle"),
            "{label}"
        );
        let marker = frame(&frames, "ready-marker");
        assert_eq!(marker.status, Status::Skipped, "{label}");
        assert_eq!(
            marker.skipped_because.as_deref(),
            Some("prime-record"),
            "{label}"
        );
        assert!(
            host.commands()
                .last()
                .is_some_and(|c| c.contains("shutdown"))
        );
    }
}

/// Catches a failure of the first command (the license) leaving the later ones to run, and
/// the failure of any one root step being masked: each root step failing must skip the ones
/// after it and the marker.
#[test]
fn a_failed_root_step_skips_what_needs_it_and_the_marker() {
    for (needle, id, then) in [
        ("xcode-select", "xcode-select", "license"),
        ("-license", "license", "first-launch"),
        ("-runFirstLaunch", "first-launch", "runtime-add-iOS_27.dmg"),
        ("runtime add", "runtime-add-iOS_27.dmg", "prime-record"),
    ] {
        let host = host_staged(&staged_both());
        host.answer(needle, out(Ended::Exited(1), "", "no"));
        go(&host);
        let frames = host.step_frames();
        assert_eq!(frame(&frames, id).status, Status::Failed, "{id}");
        assert_eq!(frame(&frames, then).status, Status::Skipped, "{id}");
        assert!(
            host.writes().iter().all(|w| w.0 != simpaths::USER_READY),
            "{id}"
        );
    }
}

/// Catches a setup with nothing to prime (no `staged.json`, an unreadable one, one without
/// an Xcode) changing the run: the plan, the commands and the frames must be the account's
/// alone, as before.
#[test]
fn a_base_with_no_staged_xcode_gets_the_account_steps_only() {
    let plain = Fake::new();
    go(&plain);
    let plain_ids = step_ids(&plain);
    assert_eq!(
        plain_ids,
        plan("kbfuser", false)
            .iter()
            .map(|s| s.id.clone())
            .collect::<Vec<_>>()
    );
    for text in [
        staged_json(&Value::Null, &[dmg_entry(DMG, HASH_A)]),
        "not json".to_owned(),
        "{\"xcode\": {}}".to_owned(),
    ] {
        let host = host_staged(&text);
        go(&host);
        assert_eq!(step_ids(&host), plain_ids, "{text}");
        assert_eq!(host.commands(), plain.commands(), "{text}");
        assert!(host.writes().iter().all(|w| w.0 != simpaths::PRIMED));
    }
    assert_eq!(plan_prep(&Fake::new()), Prep::default());
}

/// Catches a stale record surviving a run that stops part way: the record is removed before
/// the first command, so a failure leaves none, and a failed removal stops the run before
/// any command.
#[test]
fn the_old_record_is_removed_before_the_first_command() {
    let host = host_staged(&staged_both());
    host.files
        .borrow_mut()
        .insert(simpaths::PRIMED.to_owned(), b"{}".to_vec());
    host.answer("-license", out(Ended::Exited(1), "", "no"));
    go(&host);
    let calls = host.calls.borrow();
    let remove = calls
        .iter()
        .position(|c| *c == Call::Remove(simpaths::PRIMED.to_owned()))
        .expect("removed");
    let first = calls
        .iter()
        .position(|c| matches!(c, Call::Run(a) if a.iter().any(|x| x.contains("xcode-select"))))
        .expect("select");
    assert!(remove < first);
    assert!(!host.files.borrow().contains_key(simpaths::PRIMED));
    drop(calls);

    let mut host = host_staged(&staged_both());
    host.remove_fails = Some(simpaths::PRIMED.to_owned());
    go(&host);
    let frames = host.step_frames();
    assert_eq!(frame(&frames, "prime-reset").status, Status::Error);
    assert_eq!(
        frame(&frames, "xcode-select").skipped_because.as_deref(),
        Some("prime-reset")
    );
    assert!(prep_commands(&host).is_empty());
    assert!(host.writes().iter().all(|w| w.0 != simpaths::USER_READY));
}

/// Catches a record that does not hold what was done (the Xcode path and entry, and each
/// runtime with its action and entry) or that is written with the wrong mode, and a failed
/// write still giving a marker.
#[test]
fn the_record_names_what_was_done_and_its_write_gates_the_marker() {
    let host = host_staged(&staged_both());
    go(&host);
    let written = host.writes();
    let (_, bytes, mode) = written
        .iter()
        .find(|w| w.0 == simpaths::PRIMED)
        .expect("record");
    assert_eq!(*mode, 0o644);
    let record: Value = serde_json::from_slice(bytes).expect("json");
    assert_eq!(record["version"], 1);
    assert_eq!(record["xcode_path"], "/Applications/Xcode.app");
    assert_eq!(record["xcode"], xcode_entry(1000));
    assert_eq!(
        record["runtimes"],
        json!([
            {"name": DMG, "action": "add", "staged": dmg_entry(DMG, HASH_A)},
            {"name": BUNDLE, "action": "import", "staged": bundle_entry(BUNDLE, HASH_B)},
        ])
    );
    let staged = parse_staged(&staged_both()).expect("staged");
    assert_eq!(record["xcode_identity"], identity());
    assert_eq!(Some(record), record_json(&staged, Some(&identity())));

    let mut host = host_staged(&staged_both());
    host.write_fails = Some(simpaths::PRIMED.to_owned());
    let done = go(&host);
    assert!(!done.summary.ok);
    let frames = host.step_frames();
    assert_eq!(frame(&frames, "prime-record").status, Status::Error);
    assert_eq!(frame(&frames, "ready-marker").status, Status::Skipped);
    assert!(host.writes().iter().all(|w| w.0 != simpaths::USER_READY));
}

/// Catches a base whose user is ready but that was never primed (made before this existed)
/// being left as it is, or being given a second account: only the root steps run, no
/// password is drawn, no account command runs, the marker is not rewritten, and the guest
/// powers off so that its disk is flushed.
#[test]
fn a_ready_user_without_a_record_runs_the_root_steps_alone() {
    let mut host = host_staged(&staged_both());
    host.ready = Some("kbfuser".to_owned());
    let done = go(&host);
    let s = &done.summary;
    assert!(
        s.ok && !s.already_ready && !s.created && s.powering_off,
        "{s:?}"
    );
    assert_eq!(done.power_off_started, Some(true));
    assert_eq!(
        step_ids(&host),
        [
            "prime-reset",
            "xcode-select",
            "license",
            "first-launch",
            "runtime-add-iOS_27.dmg",
            "runtime-import-iOS_27.exportedBundle",
            "prime-record",
        ]
    );
    assert_eq!(prep_commands(&host), root_commands());
    assert!(
        host.calls
            .borrow()
            .iter()
            .all(|c| !matches!(c, Call::Random(_))),
        "no password for a user that is ready"
    );
    assert!(host.commands().iter().all(|c| !c.contains("sysadminctl")));
    let paths: Vec<String> = host.writes().into_iter().map(|w| w.0).collect();
    assert_eq!(paths, [simpaths::PRIMED]);
    assert!(s.note.as_deref().is_some_and(|n| n.contains("root setup")));
}

/// Catches a primed base being primed again: when the user is ready and the record covers
/// every staged thing, nothing runs, the guest is not powered off and the summary says
/// `already_ready`, as before this existed.
#[test]
fn a_ready_user_with_a_matching_record_is_left_alone() {
    let first = host_staged(&staged_both());
    go(&first);
    let mut host = host_staged(&staged_both());
    host.ready = Some("kbfuser".to_owned());
    host.files.borrow_mut().insert(
        simpaths::PRIMED.to_owned(),
        first.files.borrow()[simpaths::PRIMED].clone(),
    );
    let done = go(&host);
    assert!(done.summary.already_ready && done.summary.ok);
    assert_eq!(done.power_off_started, None);
    assert!(host.commands().is_empty());
    assert!(host.writes().is_empty());
    assert!(host.step_frames().is_empty());
}

/// Catches an import repeated for a runtime the record covers (xcodebuild may refuse a
/// platform that is installed already) and a new runtime not being imported: priming a
/// restaged base runs the new or changed runtime only, the covered steps are reported with
/// the reason, and the record then holds all.
#[test]
fn priming_a_restaged_base_imports_only_what_is_new() {
    let first = host_staged(&staged_json(&xcode_entry(1000), &[dmg_entry(DMG, HASH_A)]));
    go(&first);
    let record = first.files.borrow()[simpaths::PRIMED].clone();
    for (restaged, changed) in [
        (staged_both(), false),
        (
            staged_json(
                &xcode_entry(1000),
                &[dmg_entry(DMG, HASH_B), bundle_entry(BUNDLE, HASH_B)],
            ),
            true,
        ),
    ] {
        let mut host = host_staged(&restaged);
        host.ready = Some("kbfuser".to_owned());
        host.files
            .borrow_mut()
            .insert(simpaths::PRIMED.to_owned(), record.clone());
        let done = go(&host);
        assert!(done.summary.ok);
        let frames = host.step_frames();
        for id in ["xcode-select", "license", "first-launch"] {
            let f = frame(&frames, id);
            assert_eq!(
                (f.status, f.note.as_deref()),
                (Status::Ok, Some(SKIPPED_NOTE)),
                "{id}"
            );
            assert!(!f.argv.is_empty(), "{id} still names its command");
        }
        let dmg = frame(&frames, "runtime-add-iOS_27.dmg");
        if changed {
            assert_eq!(dmg.note, None);
            assert!(host.commands().iter().any(|c| c.contains("runtime add")));
        } else {
            assert_eq!(dmg.note.as_deref(), Some(SKIPPED_NOTE));
            assert!(host.commands().iter().all(|c| !c.contains("runtime add")));
        }
        assert_eq!(
            frame(&frames, "runtime-import-iOS_27.exportedBundle").note,
            None
        );
        assert!(
            host.commands()
                .iter()
                .any(|c| c.contains("-importPlatform"))
        );
        assert!(host.commands().iter().all(|c| !c.contains("xcode-select")));
        let now: Value =
            serde_json::from_slice(&host.files.borrow()[simpaths::PRIMED]).expect("json");
        assert_eq!(now["runtimes"].as_array().map(Vec::len), Some(2));
    }
}

/// Catches the frame budget: the host keeps at most 64 files (and its e3 test keeps 8 for
/// itself); the worst plan, eight bundles, must fit with the audit, the summary and the
/// power-off frame.
#[test]
fn the_worst_case_fits_the_hosts_file_budget() {
    let entries: Vec<Value> = (0..8)
        .map(|i| bundle_entry(&format!("r{i}.exportedBundle"), HASH_A))
        .collect();
    let host = host_staged(&staged_json(&xcode_entry(1), &entries));
    let steps = plan_with("kbfuser", false, &plan_prep(&host));
    assert!(steps.len() + 3 + 8 <= 64, "{}", steps.len());
}

/// Catches a clone's boot running the root setup and powering the clone off under the probe:
/// the disk is the base's, so a ready user with a missing or stale record looks the same in
/// a clone and in the base; only the host's answer to the prime question says which boot this is. A
/// boot without it does nothing, whatever the record says: no command, no write, no
/// power-off, no password, and a summary that says `sim` will run the steps.
#[test]
fn a_clone_boot_never_runs_the_root_setup_or_powers_off() {
    let primed = host_staged(&staged_both());
    go(&primed);
    let stale = primed.files.borrow()[simpaths::PRIMED].clone();
    for record in [None, Some(stale), Some(b"{}".to_vec())] {
        let mut host = clone_staged(&staged_both());
        host.ready = Some("kbfuser".to_owned());
        if let Some(r) = record {
            host.files
                .borrow_mut()
                .insert(simpaths::PRIMED.to_owned(), r);
        }
        host.files
            .borrow_mut()
            .remove(simpaths::XCODE_VERSION_PLIST);
        let done = go(&host);
        let s = &done.summary;
        assert!(s.ok && s.already_ready && !s.powering_off, "{s:?}");
        assert_eq!(done.power_off_started, None);
        assert!(host.commands().is_empty() && host.writes().is_empty());
        assert!(host.step_frames().is_empty());
        assert_eq!(host.pauses.get(), 0, "the host answered at once");
    }
    // The record that does not match (no version.plist now) says `sim` runs the steps.
    let mut host = clone_staged(&staged_both());
    host.ready = Some("kbfuser".to_owned());
    let done = go(&host);
    assert!(
        done.summary
            .note
            .as_deref()
            .is_some_and(|n| n.contains("`sim` runs it"))
    );
}

/// Catches a clone boot of a base that is not ready yet being given the root steps too (the
/// account is made as before this existed, and nothing else).
#[test]
fn without_a_yes_only_the_account_steps_run() {
    let host = clone_staged(&staged_both());
    go(&host);
    assert_eq!(
        step_ids(&host),
        plan("kbfuser", false)
            .iter()
            .map(|s| s.id.clone())
            .collect::<Vec<_>>()
    );
    assert!(prep_commands(&host).is_empty());
    assert!(host.writes().iter().all(|w| w.0 != simpaths::PRIMED));
}

/// Catches an Xcode taken for the primed one on tree sizes alone: the record keeps the
/// `version.plist` of the Xcode it primed, and the identity of an unreadable one is none.
#[test]
fn the_record_keeps_the_xcode_identity_and_none_when_it_cannot_be_read() {
    let host = host_staged(&staged_both());
    host.files
        .borrow_mut()
        .remove(simpaths::XCODE_VERSION_PLIST);
    go(&host);
    let record: Value =
        serde_json::from_slice(&host.files.borrow()[simpaths::PRIMED]).expect("json");
    assert_eq!(record["xcode_identity"], Value::Null);
}

/// Catches the summary hiding what the question to the host got, which `image prime` needs
/// to refuse a priming boot that did not do the setup it asked for, and which a person needs
/// to see why: each answer path says what the signal was and what became of the root setup.
#[test]
fn the_summary_says_what_the_host_answered_and_what_became_of_the_root_setup() {
    let seen = |host: &Fake| {
        let s = go(host).summary;
        (s.root_setup, s.signal, s.ok)
    };
    let with = |answer: Result<String, SendError>| {
        let host = clone_staged(&staged_both());
        *host.answer.borrow_mut() = answer;
        host
    };
    // The priming boot: the host says yes, the steps run.
    assert_eq!(
        seen(&host_staged(&staged_both())),
        (Some(RootSetup::Done), Some(Signal::Yes), true)
    );
    // No Xcode staged: nothing to ask for, so the host is not asked.
    let host = host_staged(&staged_json(&Value::Null, &[]));
    assert_eq!(
        seen(&host),
        (Some(RootSetup::NotNeeded), Some(Signal::NotChecked), true)
    );
    assert!(host.asked.borrow().is_empty());
    assert_eq!(host.pauses.get(), 0);
    // The host answers, and not yes: no (a clone's boot, another host).
    let host = clone_staged(&staged_both());
    assert_eq!(
        seen(&host),
        (Some(RootSetup::Skipped), Some(Signal::No), true)
    );
    assert!(
        go(&host)
            .summary
            .note
            .is_some_and(|n| n.contains("did not say this is the priming boot"))
    );
    // The host refuses the question: final, no retry, the reason kept.
    let host = with(Err(SendError::Refused("not a priming host".to_owned())));
    assert_eq!(
        seen(&host),
        (
            Some(RootSetup::Skipped),
            Some(Signal::Refused("not a priming host".to_owned())),
            true
        )
    );
    assert_eq!(host.asked.borrow().len(), 1);
    // No answer at all: every attempt is made, apart, and the last error is kept.
    let host = with(Err(SendError::Unavailable(
        "connect failed: errno 61".to_owned(),
    )));
    assert_eq!(
        seen(&host),
        (
            Some(RootSetup::Skipped),
            Some(Signal::NoAnswer("connect failed: errno 61".to_owned())),
            true
        )
    );
    assert_eq!(host.asked.borrow().len(), ASK_ATTEMPTS as usize);
    assert_eq!(host.pauses.get(), ASK_ATTEMPTS - 1);
    // A clone of a base that is primed already has nothing left: done, whatever the host says.
    let first = host_staged(&staged_both());
    go(&first);
    let host = clone_staged(&staged_both());
    host.files.borrow_mut().insert(
        simpaths::PRIMED.to_owned(),
        first.files.borrow()[simpaths::PRIMED].clone(),
    );
    assert_eq!(seen(&host), (Some(RootSetup::Done), Some(Signal::No), true));
}

/// Catches the question going to the wrong port or with the wrong frame (the host's sink
/// answers it by kind and name), and a host that is reached late: the first tries fail, a
/// later one answers yes, and the steps run.
#[test]
fn the_question_is_one_result_frame_on_the_results_port_and_is_retried_until_answered() {
    let host = host_staged(&staged_both());
    go(&host);
    let asked = host.asked.borrow();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].0, 5000);
    assert_eq!(asked[0].1, Frame::new(Kind::Result, QUESTION, ""));
    assert_eq!(QUESTION, "prime-question");
    drop(asked);

    // The host is not listening yet for two tries, then says yes.
    let host = host_staged(&staged_both());
    for _ in 0..2 {
        host.early
            .borrow_mut()
            .push_back(Err(SendError::Unavailable("connect failed".to_owned())));
    }
    let done = go(&host);
    assert!(done.summary.ok);
    assert_eq!(done.summary.signal, Some(Signal::Yes));
    assert_eq!(host.asked.borrow().len(), 3);
    assert_eq!(host.pauses.get(), 2);
    assert_eq!(prep_commands(&host), root_commands());
}

/// Catches only the exact answer being a yes: anything else the host says (an older host
/// acknowledges with no body) is a no.
#[test]
fn only_the_exact_word_yes_is_a_yes() {
    for body in ["", "Yes", "yes ", "no", "yes!", "true"] {
        let host = clone_staged(&staged_both());
        *host.answer.borrow_mut() = Ok(body.to_owned());
        let s = go(&host).summary;
        assert_eq!(
            (s.signal, s.root_setup),
            (Some(Signal::No), Some(RootSetup::Skipped)),
            "{body:?}"
        );
        assert!(prep_commands(&host).is_empty(), "{body:?}");
    }
}

/// Catches an older guest's summary (no such fields) failing to read: the fields are absent
/// there, which the host then treats as an error rather than a success.
#[test]
fn a_summary_without_the_new_fields_still_reads_and_says_nothing() {
    let host = host_staged(&staged_both());
    go(&host);
    let mut v: Value = serde_json::from_str(
        &host
            .sent
            .borrow()
            .iter()
            .find(|f| f.name == SUMMARY_NAME)
            .expect("summary")
            .body,
    )
    .expect("json");
    v.as_object_mut().expect("object").remove("root_setup");
    v.as_object_mut().expect("object").remove("signal");
    let s: MkUserSummary = serde_json::from_value(v).expect("reads");
    assert_eq!((s.root_setup, s.signal), (None, None));
}
