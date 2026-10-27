//! The guest's priming record against the real `staged.json`: what `image setup` writes
//! must be what `kbf-guest` reads to decide that a root step of the simulator probe was done
//! at prime. The guest's own tests use hand-written JSON in these shapes; this is the other
//! side of that pin.

use kbf_guest::sim_primed::{Primed, parse_staged, record_json, root_steps};

use super::*;

/// The Xcode's version file as the guest sees it.
const PLIST: &[u8] = b"<plist>27A1</plist>";

/// Reads the volume's `staged.json` as the guest would and returns the ids of the root steps
/// a record made from `recorded` (another `staged.json`) covers against it.
fn covered(volume: &Path, recorded: &str) -> Vec<String> {
    let manifest =
        std::fs::read_to_string(volume.join("private/var/kbf-sim/staged.json")).expect("manifest");
    let staged = parse_staged(&manifest).expect("the guest parses what setup wrote");
    let record = record_json(
        &parse_staged(recorded).expect("recorded"),
        Some(&hex::encode(PLIST)),
    )
    .expect("an Xcode");
    let primed = Primed::load(&|path: &str, _cap: usize| {
        if path == "/private/var/kbf-sim/staged.json" {
            Ok(manifest.clone().into_bytes())
        } else if path == "/Applications/Xcode.app/Contents/version.plist" {
            Ok(PLIST.to_vec())
        } else if path == "/private/var/kbf-sim/primed.json" {
            Ok(record.to_string().into_bytes())
        } else {
            Err("none".to_owned())
        }
    });
    root_steps(&staged)
        .into_iter()
        .filter(|s| primed.covers(&s.id))
        .map(|s| s.id)
        .collect()
}

fn manifest_text(volume: &Path) -> String {
    std::fs::read_to_string(volume.join("private/var/kbf-sim/staged.json")).expect("manifest")
}

/// Catches the guest and the host disagreeing on the manifest: an Xcode entry the guest does
/// not find, runtimes it cannot name, an image or a bundle whose restaging (other bytes,
/// another file) does not change its entry, or an Xcode restaged with another tree whose
/// steps would still be skipped.
#[test]
fn the_guest_reads_the_manifest_setup_writes_and_sees_a_restage() {
    let dir = tmp("stage-primed");
    let app = fake_xcode(&dir);
    let bundle = bundle::fake_bundle(&dir, "iOS_27.exportedBundle");
    write(&dir.join("iOS_27.dmg"), "hello", 0o644);
    let volume = volume_in(&dir);
    let h = Harness::new(100 * GIB);
    let paths = [dir.join("iOS_27.dmg"), bundle.clone()];
    h.run(&volume, &plan(Some(&app), &paths, false).expect("plan"))
        .expect("first");
    let first = manifest_text(&volume);
    let staged = parse_staged(&first).expect("parse");
    assert!(staged.xcode.is_some());
    assert_eq!(
        staged
            .runtimes
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["iOS_27.dmg", "iOS_27.exportedBundle"]
    );
    let all: Vec<String> = root_steps(&staged).into_iter().map(|s| s.id).collect();
    assert_eq!(
        all,
        [
            "xcode-select",
            "license",
            "first-launch",
            "runtime-add-iOS_27.dmg",
            "runtime-import-iOS_27.exportedBundle",
        ]
    );
    // The same setup again: everything is covered.
    assert_eq!(covered(&volume, &first), all);

    // An image with other bytes: only its step is not covered.
    write(&dir.join("iOS_27.dmg"), "hellO", 0o644);
    h.run(&volume, &plan(Some(&app), &paths, false).expect("plan"))
        .expect("restage image");
    let ids = covered(&volume, &first);
    assert_eq!(ids.len(), 4, "{ids:?}");
    assert!(!ids.contains(&"runtime-add-iOS_27.dmg".to_owned()));

    // A bundle with a file changed.
    write(
        &bundle.join("Restore/BuildManifest.plist"),
        "manifest?",
        0o644,
    );
    h.run(&volume, &plan(Some(&app), &paths, false).expect("plan"))
        .expect("restage bundle");
    let ids = covered(&volume, &first);
    assert!(
        !ids.contains(&"runtime-import-iOS_27.exportedBundle".to_owned()),
        "{ids:?}"
    );

    // A setup that stages no Xcode has nothing to prime.
    let none = plan(None, &[dir.join("iOS_27.dmg")], false).expect("plan");
    h.run(&volume, &none).expect("no xcode");
    let staged = parse_staged(&manifest_text(&volume)).expect("parse");
    assert!(staged.xcode.is_none() && record_json(&staged, None).is_none());
}

/// Catches an Xcode restaged with another tree being taken for the primed one.
#[test]
fn an_xcode_restaged_with_another_tree_is_not_the_primed_one() {
    let dir = tmp("stage-primed-xcode");
    let app = fake_xcode(&dir);
    let volume = volume_in(&dir);
    let h = Harness::new(100 * GIB);
    h.run(&volume, &plan(Some(&app), &[], false).expect("plan"))
        .expect("first");
    let first = manifest_text(&volume);
    assert_eq!(covered(&volume, &first).len(), 3);
    write(&app.join("Extra"), "more", 0o644);
    h.run(&volume, &plan(Some(&app), &[], false).expect("plan"))
        .expect("restage");
    assert!(covered(&volume, &first).is_empty());
}
