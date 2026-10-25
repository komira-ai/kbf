//! The copy `image-setup` makes: flags a clone carries, what `kbf-vmm` writes into it,
//! and the directory it is made in.

use super::*;

/// Catches: a clone of a file the user flagged `uchg` keeping the flag (root could not take
/// the copy, and the next file could not be made in it): every clone has its flags cleared
/// right after it is made, before `kbf-vmm` runs, and the base is sealed as usual. With
/// `uchg` really blocking on the fake host, the clones after the flagged one could not be
/// made without it.
#[test]
fn a_source_with_uchg_is_cleared_in_the_copy_before_anything_else() {
    let img = img("flagged-source");
    let built = img.bundle("base");
    {
        let mut state = img.rig.host.state();
        state.enforce_uchg = true;
        state.immutable.insert(built.join("disk.img"));
    }
    img.answer(img.setup_command("base"), true, "", "");
    img.rig.helper.image_setup(&img.spath("base")).unwrap();
    let log = img.rig.host.state().log.clone();
    let position = |what: &str| log.iter().position(|l| l.contains(what)).unwrap();
    let cleared: Vec<usize> = (0..log.len())
        .filter(|i| log[*i] == "clear_user_flags")
        .collect();
    assert_eq!(cleared.len(), 4, "{log:?}");
    assert!(
        cleared.iter().all(|i| *i < position("image setup")),
        "{log:?}"
    );
    let base = img.images.join("base");
    for file in FILES {
        assert_eq!(mode(&base.join(file)), 0o640, "{file}");
        assert_eq!(std::fs::read(base.join(file)).unwrap(), file.as_bytes());
    }
    // The user's file is theirs: it keeps its flag.
    assert!(
        img.rig
            .host
            .state()
            .immutable
            .contains(&built.join("disk.img"))
    );
}

/// Catches: the same flagged source failing later leaving a copy (the inherited flag, or
/// the helper's own, would block its removal).
#[test]
fn a_failed_setup_of_a_flagged_source_leaves_no_copy() {
    let img = img("flagged-fail");
    let built = img.bundle("base");
    {
        let mut state = img.rig.host.state();
        state.enforce_uchg = true;
        state.immutable.insert(built.join("disk.img"));
    }
    img.answer(img.setup_command("base"), false, "", "no volume");
    refused(img.rig.helper.image_setup(&img.spath("base")), "no volume");
    assert!(!img.images.join("base").exists());
    img.as_built(&built);
    img.rig.host.state().fail.insert("clear_user_flags");
    refused(
        img.rig.helper.image_setup(&img.spath("base")),
        "clearing the flags",
    );
    assert!(!img.images.join("base").exists());
}

/// Catches: a file `kbf-vmm` writes into the copy being left out of the seal (root's 0600,
/// unreadable by the VM user's group and not flagged), or left behind by a failure.
#[test]
fn a_file_kbf_vmm_writes_is_sealed_and_a_failed_run_still_removes_it() {
    let img = img("extra");
    let built = img.bundle("base");
    let extra = img.images.join("base/extra.log");
    let writer = extra.clone();
    img.rig.host.state().on_command = Some(Box::new(move |text| {
        if text.contains("image setup") {
            std::fs::write(&writer, "log").unwrap();
        }
    }));
    img.rig.helper.image_setup(&img.spath("base")).unwrap();
    assert_eq!(mode(&extra), 0o640);
    let flagged = img
        .commands()
        .into_iter()
        .find(|c| c.contains("uchg") && c.contains("extra.log"));
    assert!(flagged.is_some(), "{:?}", img.commands());
    let base = img.images.join("base");
    for path in std::iter::once(base.clone()).chain(FILES.map(|f| base.join(f))) {
        assert!(mode(&path) & 0o007 == 0, "{}", path.display());
    }
    std::fs::remove_dir_all(&base).unwrap();
    // A failed run removes it with the rest.
    img.answer(img.setup_command("base"), false, "", "no volume");
    refused(img.rig.helper.image_setup(&img.spath("base")), "no volume");
    assert!(!base.exists());
    img.as_built(&built);
}

/// Catches: a copy removal that leaves something it cannot remove silently, or that goes
/// on to remove the directory under it: a non-regular entry is left, named, and the
/// failure that led to the removal is still the first thing the caller reads.
#[test]
fn a_non_regular_entry_in_the_copy_is_left_and_named() {
    let img = img("weird");
    img.bundle("base");
    let writer = img.images.join("base/weird");
    img.rig.host.state().on_command = Some(Box::new(move |text| {
        if text.contains("image setup") {
            std::fs::create_dir(&writer).unwrap();
        }
    }));
    let why = img.rig.helper.image_setup(&img.spath("base")).unwrap_err();
    assert!(why.contains("base/weird is not a regular file"), "{why}");
    assert!(
        why.contains("the copy was not removed: left base/weird"),
        "{why}"
    );
    let base = img.images.join("base");
    assert!(base.join("weird").is_dir());
    for file in FILES {
        assert!(!base.join(file).exists(), "{file} was left");
    }
    assert_eq!(mode(&base), 0o700);
}

/// Catches: a directory made in the images directory whose preparation failed being left
/// there (the next attempt would be refused as existing), and one that is left when it
/// worked.
#[test]
fn a_directory_that_could_not_be_prepared_is_removed_again() {
    let img = img("prepare");
    let images = open_dir(&img.images).unwrap();
    let why = new_dir_with(&images, "x", |_| Err("no".to_owned())).unwrap_err();
    assert_eq!(why, "no");
    assert!(!img.images.join("x").exists());
    assert!(new_dir_with(&images, "x", |_| Ok(())).is_ok());
    assert!(img.images.join("x").is_dir());
    let again = new_dir_with(&images, "x", |_| Ok(())).unwrap_err();
    assert!(
        again.contains("making x in the images directory"),
        "{again}"
    );
    assert!(
        img.images.join("x").is_dir(),
        "an existing directory was removed"
    );
}
