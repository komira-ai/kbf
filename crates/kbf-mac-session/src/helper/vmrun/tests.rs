//! The `role-run` allowlist: which argument vectors start a process.

use std::os::unix::fs::symlink;

use crate::helper::ImageSettings;
use crate::helper::tests::me;
use crate::helper::tests::{Rig, rig_vm_with};
use crate::lease::UidRange;

fn rig(name: &str) -> Rig {
    rig_vm_with(
        name,
        UidRange::new(me(), me() + 3).unwrap(),
        None,
        Some("kbf-vm"),
    )
}

fn settings(rig: &Rig) -> ImageSettings {
    ImageSettings::under(&rig.dir)
}

fn refused(rig: &Rig, argv: &[String]) -> String {
    rig.helper.check_vm_run(argv).unwrap_err()
}

fn with_spec(argv: &[String], spec: &str) -> Vec<String> {
    let mut argv = argv.to_vec();
    argv[3] = spec.to_owned();
    argv
}

/// Catches: the one vector the daemon sends being refused (the VM would never start), or
/// a spec in a subdirectory of the images directory (where the clones live) being refused.
#[test]
fn the_exact_vector_is_accepted() {
    let rig = rig("vmrun-ok");
    let argv = settings(&rig).vm_run_argv();
    assert_eq!(rig.helper.check_vm_run(&argv), Ok(()));
    let deep = settings(&rig).dir.join("clones/a/b");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("spec.json"), "{}").unwrap();
    let deep = with_spec(&argv, &deep.join("spec.json").display().to_string());
    assert_eq!(rig.helper.check_vm_run(&deep), Ok(()));
}

/// Catches: `role-run` starting any program as the VM user (the daemon's account could
/// run anything as `kbf-vm`), another verb of the right program, another flag, a spec
/// given as `--spec=path`, or a vector with something added or left off.
#[test]
fn nothing_but_vm_run_with_a_spec_starts() {
    let rig = rig("vmrun-argv");
    let ok = settings(&rig).vm_run_argv();
    let set = |at: usize, text: &str| {
        let mut argv = ok.clone();
        argv[at] = text.to_owned();
        argv
    };
    let other = vec!["/usr/bin/id".to_owned(), "-u".to_owned()];
    let mut longer = ok.clone();
    longer.push("--extra".to_owned());
    let cases = [
        Vec::new(),
        other,
        ok[..1].to_vec(),
        ok[..3].to_vec(),
        longer,
        set(0, "/usr/bin/true"),
        set(0, "kbf-vmm"),
        set(0, &format!("{}/../kbf-vmm", settings(&rig).dir.display())),
        set(1, "image"),
        set(1, "probe"),
        set(2, "--bundle"),
        set(2, "-s"),
        vec![ok[0].clone(), ok[1].clone(), format!("--spec={}", ok[3])],
    ];
    for argv in cases {
        let error = refused(&rig, &argv);
        assert!(error.contains("role-run starts only"), "{argv:?}: {error}");
    }
}

/// Catches: a spec outside the images directory (the VM user would run a spec of its
/// choosing), a path that only starts like it, one that climbs out with `..`, the directory itself, a file that does not exist and one that is a directory.
#[test]
fn a_spec_must_be_a_file_inside_the_images_directory() {
    let rig = rig("vmrun-where");
    let ok = settings(&rig).vm_run_argv();
    let images = settings(&rig).dir;
    let sibling = format!("{}-evil", images.display());
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(format!("{sibling}/spec.json"), "{}").unwrap();
    let outside = rig.dir.join("outside.json");
    std::fs::write(&outside, "{}").unwrap();
    std::fs::create_dir_all(images.join("clones/dir")).unwrap();
    let cases = [
        (
            "not inside the images directory",
            outside.display().to_string(),
        ),
        (
            "not inside the images directory",
            format!("{sibling}/spec.json"),
        ),
        ("not inside the images directory", "spec.json".to_owned()),
        ("not inside the images directory", "/etc/passwd".to_owned()),
        (
            "goes up",
            format!("{}/clones/../../outside.json", images.display()),
        ),
        (
            "is the images directory itself",
            images.display().to_string(),
        ),
        (
            "No such file",
            format!("{}/clones/missing.json", images.display()),
        ),
        (
            "not a regular file",
            format!("{}/clones/dir", images.display()),
        ),
    ];
    for (says, spec) in cases {
        let error = refused(&rig, &with_spec(&ok, &spec));
        assert!(error.contains(says), "{spec}: {error}");
    }
}

/// Catches: a link making a spec outside the images directory (or another file in it)
/// pass: the file itself a link, a directory on the way a link, a second hard link to a
/// file elsewhere.
#[test]
fn a_spec_reached_through_a_link_is_refused() {
    let rig = rig("vmrun-links");
    let ok = settings(&rig).vm_run_argv();
    let images = settings(&rig).dir;
    let elsewhere = rig.dir.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("spec.json"), "{}").unwrap();
    // A plain file with one link, reached only through a link to its directory.
    std::fs::create_dir_all(elsewhere.join("sub")).unwrap();
    std::fs::write(elsewhere.join("sub/spec.json"), "{}").unwrap();

    symlink(elsewhere.join("spec.json"), images.join("clones/link.json")).unwrap();
    symlink(elsewhere.join("sub"), images.join("clones/dirlink")).unwrap();
    std::fs::hard_link(elsewhere.join("spec.json"), images.join("clones/hard.json")).unwrap();

    for spec in [
        "clones/link.json",
        "clones/dirlink/spec.json",
        "clones/hard.json",
    ] {
        let spec = format!("{}/{spec}", images.display());
        let error = refused(&rig, &with_spec(&ok, &spec));
        assert!(error.starts_with(&format!("the spec {spec}: ")), "{error}");
    }
    // The same files, plain, pass: the refusals are for the links.
    std::fs::copy(
        elsewhere.join("spec.json"),
        images.join("clones/plain.json"),
    )
    .unwrap();
    let plain = format!("{}/clones/plain.json", images.display());
    assert_eq!(rig.helper.check_vm_run(&with_spec(&ok, &plain)), Ok(()));
}

/// Catches: an images directory that cannot be opened being taken as a pass.
#[test]
fn a_missing_images_directory_refuses() {
    let rig = rig("vmrun-no-images");
    let ok = settings(&rig).vm_run_argv();
    std::fs::remove_dir_all(settings(&rig).dir).unwrap();
    let error = refused(&rig, &ok);
    assert!(error.contains("the images directory"), "{error}");
}
