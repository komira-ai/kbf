//! `image-harvest` over the fake host.

use super::*;

/// Catches: a harvest output directory that is not new, not root's or not mode 0700
/// (readable by the VM user whose guest it was copied from), or outside the images
/// directory; and the harvest not being what the helper builds. The happy path.
#[test]
fn harvest_makes_its_output_directory_0700_root_owned_inside_the_images_directory() {
    let img = img("harvest");
    let bundle = img.bundle("clone");
    let out = img.images.join("out");
    let watched = out.clone();
    let seen = img.watch(move || {
        let m = std::fs::metadata(&watched).unwrap();
        format!("out {:o} {}", m.mode() & 0o7777, m.uid())
    });
    let command = img.harvest_command(&img.spath("clone"), "out");
    img.answer(command.clone(), true, "{\"harvest\":1}", "");
    let report = img
        .rig
        .helper
        .image_harvest(&img.spath("clone"), &img.path("out"))
        .unwrap();
    assert_eq!(report, "{\"harvest\":1}");
    let euid = rustix::process::geteuid().as_raw();
    assert_eq!(
        *seen.lock().unwrap(),
        [format!("{command} => out 700 {euid}")]
    );
    let made = std::fs::metadata(&out).unwrap();
    assert!(made.is_dir());
    assert_eq!(made.mode() & 0o7777, 0o700);
    assert_eq!(made.uid(), euid);
    assert_eq!(made.gid(), rustix::process::getegid().as_raw());
    assert_eq!(out.parent().unwrap(), img.images);
    img.as_built(&bundle);
    assert_eq!(img.commands().len(), 1);
}

/// Catches: a harvest into an existing name (a directory, a file, a link that would lead
/// anywhere), into the sealed base it reads, or one that removes what was there.
#[test]
fn harvest_makes_only_a_new_output_directory() {
    let img = img("harvest-new");
    let bundle = img.bundle("clone");
    let base = img.sealed("base");
    std::fs::create_dir(img.images.join("taken")).unwrap();
    std::fs::write(img.images.join("taken/keep"), "x").unwrap();
    std::fs::write(img.images.join("file"), "x").unwrap();
    symlink(img.rig.dir.join("elsewhere"), img.images.join("link")).unwrap();
    for name in ["taken", "file", "link"] {
        refused(
            img.rig
                .helper
                .image_harvest(&img.spath("clone"), &img.path(name)),
            "making",
        );
    }
    refused(
        img.rig
            .helper
            .image_harvest(&img.path("base"), &img.path("base")),
        "must not be the bundle",
    );
    assert!(img.images.join("taken/keep").exists());
    assert!(!img.rig.dir.join("elsewhere").exists());
    assert!(img.commands().is_empty());
    img.as_built(&bundle);
    img.as_sealed(&base);
}

/// Catches: a failed harvest leaving an empty output directory behind (a retry would be
/// refused), removing a partial copy a person may want, or leaving the bundle root's.
#[test]
fn a_failed_harvest_removes_its_empty_output_directory_and_returns_the_bundle() {
    let img = img("harvest-fail");
    let bundle = img.bundle("clone");
    img.answer(
        img.harvest_command(&img.spath("clone"), "out"),
        false,
        "",
        "no volume",
    );
    refused(
        img.rig
            .helper
            .image_harvest(&img.spath("clone"), &img.path("out")),
        "kbf-vmm failed: no volume",
    );
    assert!(!img.images.join("out").exists());
    img.as_built(&bundle);
    // A partial copy stays.
    let partial = img.images.join("out");
    img.rig.host.state().on_command = Some(Box::new(move |_| {
        std::fs::create_dir_all(partial.join("files")).unwrap();
    }));
    refused(
        img.rig
            .helper
            .image_harvest(&img.spath("clone"), &img.path("out")),
        "kbf-vmm failed",
    );
    assert!(img.images.join("out/files").is_dir());
    img.as_built(&bundle);
}

/// Catches: a harvest of a kept clone giving it back as root's, or with another mode
/// than it had.
#[test]
fn harvest_gives_a_clone_back_with_its_owner_and_modes() {
    let img = img("harvest-owner");
    let bundle = img.bundle("clone");
    set_mode(&bundle, 0o770);
    set_mode(&bundle.join("disk.img"), 0o660);
    img.rig
        .helper
        .image_harvest(&img.spath("clone"), &img.path("out"))
        .unwrap();
    assert_eq!(mode(&bundle), 0o770);
    assert_eq!(mode(&bundle.join("disk.img")), 0o660);
    assert_eq!(mode(&bundle.join("aux.img")), 0o664);
    assert_eq!(std::fs::metadata(&bundle).unwrap().uid(), me());
}

/// Catches: a harvest of a sealed base taking it (locking it 0700, changing its group
/// or modes) or failing on its `uchg` files: it is root's and closed already, so
/// nothing is changed.
#[test]
fn harvest_of_a_sealed_base_changes_nothing() {
    let img = img("harvest-sealed");
    let base = img.sealed("base");
    let watched = base.clone();
    let seen = img.watch(move || format!("base {:o}", mode(&watched)));
    img.rig
        .helper
        .image_harvest(&img.path("base"), &img.path("out"))
        .unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        [format!(
            "{} => base 750",
            img.harvest_command(&img.path("base"), "out")
        )]
    );
    img.as_sealed(&base);
}
