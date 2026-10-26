//! Tests of the `<name>.exportedBundle` runtime: its plan, its refusals, and the copy with
//! the per-file manifest. Nothing here needs root or macOS.

use super::*;

/// The SHA-256 of the bytes `cryptex`.
const CRYPTEX: &str = "1973223a21d99bbbe64bbe6d31aca7aeb155664eead280132676bc6695a79a37";

/// A small exported bundle: metadata, and a restore directory with an image and a link.
pub(super) fn fake_bundle(dir: &Path, name: &str) -> PathBuf {
    let b = dir.join(name);
    write(&b.join("ExportedMetadata.plist"), "meta", 0o600);
    write(&b.join("Restore/090-cryptex.dmg"), "cryptex", 0o444);
    write(&b.join("Restore/BuildManifest.plist"), "manifest!", 0o644);
    link("090-cryptex.dmg", &b.join("Restore/latest.dmg"));
    b
}

fn plan_of(paths: &[PathBuf]) -> Result<Vec<RuntimePlan>, StageError> {
    plan_runtimes(paths)
}

/// Catches a bundle planned as a file, with a wrong size or a lost entry, a name that
/// the dmg rules would refuse being accepted for a bundle (or the reverse), and the
/// suffix being optional.
#[test]
fn a_bundle_is_planned_as_a_tree_with_the_dmg_name_rules() {
    let dir = tmp("bundle-plan");
    let b = fake_bundle(&dir, "iOS_27.exportedBundle");
    write(&dir.join("iOS_27.dmg"), "hello", 0o644);
    let plans = plan_of(&[b.clone(), dir.join("iOS_27.dmg")]).expect("plan");
    assert_eq!(plans[0].kind, RuntimeKind::Bundle);
    assert_eq!(plans[0].name, "iOS_27.exportedBundle");
    assert_eq!(plans[0].bytes, (4 + 7 + 9) as u64);
    assert_eq!(plans[1].kind, RuntimeKind::Dmg);
    assert!(plans[1].nodes.is_empty());
    let seen: Vec<String> = plans[0]
        .nodes
        .iter()
        .map(|n| match n {
            Node::Dir { rel, .. } => format!("d {}", rel.display()),
            Node::File { rel, bytes, .. } => format!("f {} {bytes}", rel.display()),
            Node::Link { rel, target } => format!("l {} -> {}", rel.display(), target.display()),
        })
        .collect();
    assert_eq!(
        seen,
        [
            "f ExportedMetadata.plist 4",
            "d Restore",
            "f Restore/090-cryptex.dmg 7",
            "f Restore/BuildManifest.plist 9",
            "l Restore/latest.dmg -> 090-cryptex.dmg",
        ]
    );
    assert_eq!(RuntimeKind::Dmg.as_str(), "dmg");
    assert_eq!(RuntimeKind::Bundle.as_str(), "bundle");
    // The bundle's bytes are part of what the free-space check counts.
    assert_eq!(
        Plan {
            runtimes: plans,
            ..Plan::default()
        }
        .bytes(),
        20 + 5
    );
}

/// Catches a bundle name that could climb out of the runtime directory, hide there, or make
/// a frame name over the limit, and a file or link taken for a bundle.
#[test]
fn a_bundle_name_is_checked_like_a_dmg_name_and_must_be_a_directory() {
    let dir = tmp("bundle-names");
    let longest = format!("{}.exportedBundle", "n".repeat(MAX_RUNTIME_NAME_BYTES - 15));
    std::fs::create_dir(dir.join(&longest)).expect("dir");
    assert_eq!(
        plan_of(&[dir.join(&longest)]).expect("longest")[0].name,
        longest
    );
    for bad in [
        "a b.exportedBundle",
        ".exportedBundle",
        ".hidden.exportedBundle",
        "a..b.exportedBundle",
        "x.exportedBundle.zip",
        "x.exportedbundle",
        "ü.exportedBundle",
        &format!("{}.exportedBundle", "n".repeat(MAX_RUNTIME_NAME_BYTES - 14)),
    ] {
        std::fs::create_dir(dir.join(bad)).expect("dir");
        assert_eq!(
            plan_of(&[dir.join(bad)]),
            Err(StageError::BadRuntimeName(dir.join(bad))),
            "{bad}"
        );
    }
    write(&dir.join("file.exportedBundle"), "x", 0o644);
    assert_eq!(
        plan_of(&[dir.join("file.exportedBundle")]),
        Err(StageError::NotDirectory(dir.join("file.exportedBundle")))
    );
    link("iOS.exportedBundle", &dir.join("l.exportedBundle"));
    std::fs::create_dir(dir.join("iOS.exportedBundle")).expect("dir");
    assert_eq!(
        plan_of(&[dir.join("l.exportedBundle")]),
        Err(StageError::Symlink(dir.join("l.exportedBundle")))
    );
    std::fs::create_dir_all(dir.join("other/iOS.exportedBundle")).expect("dir");
    assert_eq!(
        plan_of(&[
            dir.join("iOS.exportedBundle"),
            dir.join("other/iOS.exportedBundle")
        ]),
        Err(StageError::DuplicateRuntime("iOS.exportedBundle".into()))
    );
    assert!(plan_of(&[dir.join("missing.exportedBundle")]).is_err());
}

/// Catches a bundle walked with looser rules than Xcode.app: a link out of the bundle (or
/// to an absolute path elsewhere), a socket, or a tree nested past the bound being copied; and
/// a link inside the bundle (relative, or absolute under its own guest path) being refused.
#[test]
fn a_bundle_is_refused_what_the_app_walk_refuses_and_allows_inside_links() {
    let dir = tmp("bundle-refuse");
    let b = fake_bundle(&dir, "a.exportedBundle");
    link("../ExportedMetadata.plist", &b.join("Restore/up.plist"));
    link(
        "/private/var/kbf-sim/runtimes/a.exportedBundle/ExportedMetadata.plist",
        &b.join("abs"),
    );
    assert!(plan_of(std::slice::from_ref(&b)).is_ok());

    link("../../outside", &b.join("Restore/out"));
    let e = plan_of(std::slice::from_ref(&b)).expect_err("escapes");
    assert!(
        matches!(&e, StageError::EscapingLink { path, target }
            if path.ends_with("Restore/out") && target == Path::new("../../outside")),
        "{e}"
    );
    std::fs::remove_file(b.join("Restore/out")).expect("rm");
    link("/etc/passwd", &b.join("abs2"));
    assert!(matches!(
        plan_of(std::slice::from_ref(&b)),
        Err(StageError::EscapingLink { .. })
    ));
    std::fs::remove_file(b.join("abs2")).expect("rm");
    // The guest path of another bundle is not inside this one.
    link(
        "/private/var/kbf-sim/runtimes/b.exportedBundle/x",
        &b.join("abs3"),
    );
    assert!(matches!(
        plan_of(std::slice::from_ref(&b)),
        Err(StageError::EscapingLink { .. })
    ));
    std::fs::remove_file(b.join("abs3")).expect("rm");
    let _socket = std::os::unix::net::UnixListener::bind(b.join("s")).expect("socket");
    let e = plan_of(&[b]).expect_err("socket").to_string();
    assert!(e.contains("socket"), "{e}");
}

/// Catches a bundle copy that drops a file, a directory or a link, hashes the wrong bytes,
/// records an absolute host path, leaves a mode other than 0755/0644, keeps a stale bundle or
/// file of an earlier setup, skips the ownership of an entry, or writes a different report
/// than the manifest on the volume; and a setup that stages a bundle and an image together.
#[test]
fn a_bundle_is_copied_with_a_manifest_of_path_and_sha256_per_file() {
    let dir = tmp("bundle-exec");
    let b = fake_bundle(&dir, "iOS_27.exportedBundle");
    write(&dir.join("sim.dmg"), "hello", 0o644);
    let volume = volume_in(&dir);
    let rt_dir = volume.join("private/var/kbf-sim/runtimes");
    // A bundle of an earlier setup, with a file this bundle does not have, is replaced whole.
    write(&rt_dir.join("iOS_27.exportedBundle/stale"), "old", 0o644);
    write(&rt_dir.join("gone.exportedBundle/x"), "old", 0o644);
    let plan = plan(None, &[b, dir.join("sim.dmg")], false).expect("plan");
    let h = Harness::new(100 * GIB);
    let out = h.run(&volume, &plan).expect("execute");
    assert_eq!(
        listing(&rt_dir),
        [
            "d iOS_27.exportedBundle 755",
            "f iOS_27.exportedBundle/ExportedMetadata.plist 644 meta",
            "d iOS_27.exportedBundle/Restore 755",
            "f iOS_27.exportedBundle/Restore/090-cryptex.dmg 644 cryptex",
            "f iOS_27.exportedBundle/Restore/BuildManifest.plist 644 manifest!",
            "l iOS_27.exportedBundle/Restore/latest.dmg -> 090-cryptex.dmg",
            "f sim.dmg 644 hello",
        ]
    );
    let r = &out["runtimes"][0];
    assert_eq!(r["name"], "iOS_27.exportedBundle");
    assert_eq!(r["kind"], "bundle");
    assert_eq!(r["bytes"], 20);
    assert_eq!(
        r["dest"],
        "/private/var/kbf-sim/runtimes/iOS_27.exportedBundle"
    );
    let files: Vec<(&str, u64, &str)> = r["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| {
            (
                f["path"].as_str().expect("path"),
                f["bytes"].as_u64().expect("bytes"),
                f["sha256"].as_str().expect("sha"),
            )
        })
        .collect();
    assert_eq!(files.len(), 3);
    assert_eq!(files[0].0, "ExportedMetadata.plist");
    assert_eq!(files[1], ("Restore/090-cryptex.dmg", 7, CRYPTEX));
    assert_eq!(files[2].0, "Restore/BuildManifest.plist");
    assert!(files.iter().all(|f| f.2.len() == 64));
    // The image beside it is reported as before, with its kind.
    assert_eq!(out["runtimes"][1]["kind"], "dmg");
    assert_eq!(out["runtimes"][1]["sha256"], HELLO);
    let text =
        std::fs::read_to_string(volume.join("private/var/kbf-sim/staged.json")).expect("manifest");
    assert!(!text.contains(dir.to_str().expect("p")), "{text}");
    let m: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(m["runtimes"], out["runtimes"]);
    let owned = h.chowned.borrow();
    for p in [
        "iOS_27.exportedBundle",
        "iOS_27.exportedBundle/Restore",
        "iOS_27.exportedBundle/Restore/090-cryptex.dmg",
        "iOS_27.exportedBundle/Restore/latest.dmg",
    ] {
        let built = rt_dir.with_file_name(".runtimes.kbf-staging").join(p);
        assert!(owned.contains(&built), "{p}");
    }
}

/// Catches a bundle's files left out of the free-space check, or the check writing
/// before it refuses.
#[test]
fn a_bundle_that_does_not_fit_is_refused_before_anything_is_written() {
    let dir = tmp("bundle-space");
    let b = fake_bundle(&dir, "iOS.exportedBundle");
    let plan = plan(None, &[b], false).expect("plan");
    let volume = volume_in(&dir);
    let tight = plan.bytes() + HEADROOM_BYTES;
    let e = Harness::new(tight - 1)
        .run(&volume, &plan)
        .expect_err("too small");
    assert!(
        e.contains("not enough room") && e.contains("runtimes"),
        "{e}"
    );
    assert!(!volume.join("private/var/kbf-sim").exists());
    Harness::new(tight).run(&volume, &plan).expect("fits");
}

/// Catches a file of a bundle that changed after the plan being installed (its size no longer
/// the planned one), and a file swapped for a link being read as root.
#[test]
fn a_bundle_file_that_changed_since_the_plan_is_an_error() {
    let dir = tmp("bundle-changed");
    let b = fake_bundle(&dir, "iOS.exportedBundle");
    let plan = plan(None, std::slice::from_ref(&b), false).expect("plan");
    let victim = b.join("ExportedMetadata.plist");
    write(&victim, "much longer than planned", 0o600);
    let e = Harness::new(100 * GIB)
        .run(&volume_in(&dir), &plan)
        .expect_err("changed");
    assert!(e.contains("changed while it was copied"), "{e}");

    let secret = dir.join("secret");
    write(&secret, "meta", 0o600);
    std::fs::remove_file(&victim).expect("rm");
    link(secret.to_str().expect("p"), &victim);
    let e = Harness::new(100 * GIB)
        .run(&volume_in(&dir), &plan)
        .expect_err("swapped");
    assert!(
        e.starts_with("copy ") && e.contains("ExportedMetadata"),
        "{e}"
    );
}

/// Catches a failed ownership change inside a bundle not stopping the stage, and a
/// bundle whose directory cannot be created passing.
#[test]
fn a_failure_while_building_a_bundle_stops_the_stage() {
    let dir = tmp("bundle-fail");
    let b = fake_bundle(&dir, "iOS.exportedBundle");
    let plan = plan(None, &[b], false).expect("plan");
    // Two directories, the bundle and its five entries, and the manifest are given to root.
    for at in 0..9 {
        let mut h = Harness::new(100 * GIB);
        h.chown_fails_at = Some(at);
        let e = h.run(&volume_in(&tmp(&format!("bundle-fail-{at}"))), &plan);
        assert!(e.is_err(), "{at}");
    }
    let mut h = Harness::new(100 * GIB);
    h.chown_fails_at = Some(9);
    assert!(h.run(&volume_in(&tmp("bundle-fail-ok")), &plan).is_ok());
}
