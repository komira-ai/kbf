//! The small pieces of the image verbs, and the lock.

use super::*;

/// Catches: an object another user owns, or one group or others can write, counted as
/// closed (the helper would leave it as it is and attach a disk image the user can write).
#[test]
fn only_the_helpers_own_unwritable_objects_are_closed() {
    assert!(closed_by(0, 0, 0o750));
    assert!(closed_by(0, 0, 0o640));
    assert!(!closed_by(501, 0, 0o750));
    assert!(!closed_by(0, 0, 0o770));
    assert!(!closed_by(0, 0, 0o702));
}

/// Catches: an images directory owned by another user being trusted.
#[test]
fn an_images_directory_another_user_owns_is_refused() {
    let mut stat = fstat(std::fs::File::open(".").unwrap()).unwrap();
    stat.st_uid = rustix::process::geteuid().as_raw() + 1;
    stat.st_mode = 0o755;
    assert!(
        closed_dir(&stat)
            .unwrap_err()
            .contains("not the helper's uid")
    );
}

/// Catches: a claim that is satisfied by a name that now leads to another directory, or
/// to one the helper does not own.
#[test]
fn a_claim_holds_only_for_the_same_directory_the_helper_owns() {
    let mut mine = fstat(std::fs::File::open(".").unwrap()).unwrap();
    mine.st_uid = rustix::process::geteuid().as_raw();
    assert_eq!(check_claim("b", &mine, &mine), Ok(()));
    let mut other = mine;
    other.st_ino = mine.st_ino + 1;
    let why = check_claim("b", &mine, &other).unwrap_err();
    assert!(why.contains("b changed while it was being taken"), "{why}");
    let mut theirs = mine;
    theirs.st_uid = mine.st_uid + 1;
    assert!(check_claim("b", &mine, &theirs).is_err());
}

/// Catches: an error that loses what was being done, or the OS's reason; a path that is
/// not text being passed on as an argument; an undo that runs on success or hides the
/// original error.
#[test]
fn errors_name_the_step_and_a_path_must_be_text() {
    let why = at("locking b")(rustix::io::Errno::ACCESS);
    assert!(why.starts_with("locking b: "), "{why}");
    warn_if_failed("a step", Ok(()));
    warn_if_failed("a step", Err("failed".to_owned()));
    let mut undone = 0;
    let ok = or_undo("x", Ok(1), || {
        undone += 1;
        Ok(())
    });
    assert_eq!(ok, Ok(1));
    let bad: Result<u8, String> = Err("first".to_owned());
    let kept = or_undo("x", bad, || {
        undone += 10;
        Err("second".to_owned())
    });
    assert_eq!(kept, Err("first".to_owned()));
    assert_eq!(undone, 10);
    use std::os::unix::ffi::OsStrExt as _;
    let bad = Path::new(std::ffi::OsStr::from_bytes(b"/images/\xff"));
    assert!(ImageSettings::text(bad).unwrap_err().contains("not UTF-8"));
    assert_eq!(ImageSettings::text(Path::new("/images/b")), Ok("/images/b"));
}

/// Catches: a report that overruns the reply frame, or is cut inside a character.
#[test]
fn the_report_is_cut_at_the_frame_limit_on_a_character_boundary() {
    assert_eq!(report("ok"), "ok");
    let long = format!("a{}", "é".repeat(IMAGE_REPORT_BYTES));
    let cut = report(&long);
    assert_eq!(cut.len(), IMAGE_REPORT_BYTES - 1);
    assert!(long.starts_with(&cut));
}

/// Catches: one panicked image verb stopping every later one.
#[test]
fn a_panic_in_one_image_verb_does_not_stop_the_next() {
    let img = img("poison");
    img.bundle("b");
    let helper = &img.rig.helper;
    std::thread::scope(|scope| {
        let panicked = scope.spawn(|| {
            let _one = helper.images.lock().unwrap();
            panic!("a verb panicked holding the image lock");
        });
        assert!(panicked.join().is_err());
    });
    helper.image_setup(&img.spath("b")).unwrap();
}

/// Catches: two image verbs at once. A verb that does not hold the image lock while it
/// works would let a second one run beside it; the lock is the only thing that makes
/// the second wait. At each command a verb runs, a second thread tries the lock and
/// must find it taken, so every command ran under the lock (no sleep, no race).
#[test]
fn image_verbs_run_one_at_a_time() {
    let img = img("serial");
    img.bundle("b");
    let helper = &img.rig.helper;
    let path = img.spath("b");
    let (ask, asked) = std::sync::mpsc::channel::<()>();
    let (answer, answers) = std::sync::mpsc::channel::<()>();
    // The verb waits in the hook until the other thread has looked at the lock.
    img.rig.host.state().on_command = Some(Box::new(move |_| {
        ask.send(()).unwrap();
        answers.recv().unwrap();
    }));
    let free = std::thread::scope(|scope| {
        let checker = scope.spawn(|| {
            let mut free = Vec::new();
            for () in asked {
                free.push(helper.images.try_lock().is_ok());
                answer.send(()).unwrap();
            }
            free
        });
        helper.image_setup(&path).unwrap();
        // Drops the hook, and with it the sender the checker is reading.
        img.rig.host.state().on_command = None;
        checker.join().unwrap()
    });
    assert_eq!(img.commands().len(), 3);
    assert_eq!(
        free, [false; 3],
        "the lock was taken at every command the verb ran"
    );
    assert!(
        helper.images.try_lock().is_ok(),
        "the verb released the lock"
    );
}
