//! The image verbs over a fake host: real files in a scratch images directory, the
//! program runs recorded and answered by the fake.

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::*;
use crate::autologin::CommandOutput;
use crate::helper::NewUser;
use crate::helper::tests::{Rig, me, my_gid, rig_vm_with};
use crate::lease::UidRange;

const VM: &str = "kbf-vm";
const FILES: [&str; 4] = [
    "aux.img",
    "disk.img",
    "hardware-model",
    "machine-identifier",
];

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o7777
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A helper whose VM user is this test's own uid (the one account a test can give a file
/// to), over an images directory holding the installed programs' stand-ins.
struct Img {
    rig: Rig,
    images: PathBuf,
    vmm: String,
    guest: String,
}

fn img(name: &str) -> Img {
    let range = UidRange::new(me() + 100, me() + 103).unwrap();
    let rig = rig_vm_with(&format!("img-{name}"), range, None, Some(VM));
    rig.helper.boot_reset(&[]).unwrap();
    rig.host.state().users.insert(
        VM.to_owned(),
        NewUser {
            name: VM.to_owned(),
            uid: me(),
            gid: my_gid(),
            home: rig.dir.join("Users").join(VM),
            admin: false,
        },
    );
    let settings = rig.helper.settings.image.clone();
    std::fs::create_dir(&settings.dir).unwrap();
    set_mode(&settings.dir, 0o755);
    for program in [&settings.vmm, &settings.guest] {
        std::fs::write(program, "program").unwrap();
        set_mode(program, 0o755);
    }
    rig.host.state().log.clear();
    Img {
        vmm: settings.vmm.display().to_string(),
        guest: settings.guest.display().to_string(),
        images: settings.dir,
        rig,
    }
}

impl Img {
    fn path(&self, name: &str) -> String {
        self.images.join(name).display().to_string()
    }

    /// A bundle: the four files, mode 0644, in a directory of mode 0755.
    fn bundle(&self, name: &str) -> PathBuf {
        let dir = self.images.join(name);
        std::fs::create_dir(&dir).unwrap();
        for file in FILES {
            std::fs::write(dir.join(file), file).unwrap();
            set_mode(&dir.join(file), 0o644);
        }
        set_mode(&dir, 0o755);
        dir
    }

    /// Every program run so far, as `program arg ...`.
    fn commands(&self) -> Vec<String> {
        self.rig
            .host
            .state()
            .log
            .iter()
            .filter_map(|line| line.strip_prefix("command "))
            .map(str::to_owned)
            .collect()
    }

    fn answer(&self, command: String, success: bool, stdout: &str, stderr: &str) {
        self.rig.host.state().answers.insert(
            command,
            CommandOutput {
                success,
                stdout: stdout.to_owned(),
                stderr: stderr.to_owned(),
            },
        );
    }

    /// `kbf-vmm image setup ...` as the helper must run it for `name`.
    fn setup_command(&self, name: &str) -> String {
        format!(
            "{} image setup --base {} --guest-binary {}",
            self.vmm,
            self.path(name),
            self.guest
        )
    }

    fn harvest_command(&self, bundle: &str, out: &str) -> String {
        format!(
            "{} image harvest --bundle {} --out {}/files",
            self.vmm,
            self.path(bundle),
            self.path(out)
        )
    }

    /// Notes `look` of the filesystem at each program run, by the command's text.
    fn watch(&self, look: impl Fn() -> String + Send + 'static) -> Arc<Mutex<Vec<String>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let into = Arc::clone(&seen);
        self.rig.host.state().on_command = Some(Box::new(move |text| {
            into.lock().unwrap().push(format!("{text} => {}", look()));
        }));
        seen
    }

    fn untouched(&self, bundle: &Path) {
        assert_eq!(mode(bundle), 0o755, "{}", bundle.display());
        for file in FILES {
            assert_eq!(mode(&bundle.join(file)), 0o644);
        }
    }
}

fn refused(result: Result<String, String>, says: &str) {
    let why = result.unwrap_err();
    assert!(why.contains(says), "{why}");
}

/// Catches: a bundle or output path outside the images directory (another directory, a
/// sibling whose name starts like it, a relative name, the directory itself) reaching
/// `kbf-vmm`.
#[test]
fn a_bundle_outside_the_images_directory_is_refused() {
    let img = img("outside");
    let inside = img.bundle("b");
    let other = img.rig.dir.join("other");
    std::fs::create_dir(&other).unwrap();
    let lookalike = img.rig.dir.join("images-evil");
    std::fs::create_dir(&lookalike).unwrap();
    for outside in [
        other.display().to_string(),
        lookalike.join("b").display().to_string(),
        "b".to_owned(),
        "images/b".to_owned(),
        img.images.display().to_string(),
        "/".to_owned(),
        img.path("b/disk.img"),
    ] {
        let helper = &img.rig.helper;
        let says = if outside.ends_with("disk.img") || outside == img.images.display().to_string() {
            "not a name directly inside"
        } else {
            "not inside the images directory"
        };
        refused(helper.image_setup(&outside), says);
        refused(helper.image_harvest(&outside, &img.path("out")), says);
        refused(helper.image_harvest(&img.path("b"), &outside), says);
    }
    refused(img.rig.helper.image_setup("b\0"), "NUL");
    assert!(img.commands().is_empty());
    assert!(!img.images.join("out").exists());
    img.untouched(&inside);
}

/// Catches: a bundle that is a symlink being followed (the helper would attach a disk
/// image elsewhere as root), or a link's target being taken (chowned, locked).
#[test]
fn a_symlinked_bundle_is_refused() {
    let img = img("link");
    let real = img.rig.dir.join("real");
    std::fs::create_dir(&real).unwrap();
    for file in FILES {
        std::fs::write(real.join(file), file).unwrap();
        set_mode(&real.join(file), 0o644);
    }
    set_mode(&real, 0o755);
    symlink(&real, img.images.join("link")).unwrap();
    symlink(img.rig.dir.join("nowhere"), img.images.join("dangling")).unwrap();
    for name in ["link", "dangling"] {
        refused(
            img.rig.helper.image_setup(&img.path(name)),
            "not a plain directory",
        );
        refused(
            img.rig
                .helper
                .image_harvest(&img.path(name), &img.path("out")),
            "not a plain directory",
        );
    }
    assert!(img.commands().is_empty());
    img.untouched(&real);
    assert!(
        !img.images.join("out").exists(),
        "an output directory was left"
    );
}

/// Catches: a path that reaches a bundle through a symlinked parent being accepted: a
/// link to the images directory (outside it by name), and a link inside it (a name
/// below the images directory is one level deep or refused).
#[test]
fn a_symlink_in_a_parent_of_the_bundle_is_refused() {
    let img = img("parent");
    let real = img.bundle("b");
    let alias = img.rig.dir.join("alias");
    symlink(&img.images, &alias).unwrap();
    refused(
        img.rig
            .helper
            .image_setup(&alias.join("b").display().to_string()),
        "not inside the images directory",
    );
    let sub = img.images.join("sub");
    std::fs::create_dir(&sub).unwrap();
    symlink(&real, sub.join("b")).unwrap();
    symlink(&img.images, img.images.join("self")).unwrap();
    for deeper in ["sub/b", "self/b"] {
        refused(
            img.rig.helper.image_setup(&img.path(deeper)),
            "not a name directly inside",
        );
        refused(
            img.rig
                .helper
                .image_harvest(&img.path("b"), &img.path(deeper)),
            "not a name directly inside",
        );
    }
    assert!(img.commands().is_empty());
    img.untouched(&real);
}

/// Catches: `..` accepted, at the front, in the middle or at the end, where it would
/// lead out of the images directory or to another bundle.
#[test]
fn dotdot_is_refused() {
    let img = img("dotdot");
    let real = img.bundle("b");
    let images = img.images.display().to_string();
    for path in [
        format!("{images}/../images/b"),
        format!("{images}/b/.."),
        format!("{images}/.."),
        format!("{images}/b/../b"),
        format!("{images}/../../etc"),
    ] {
        refused(img.rig.helper.image_setup(&path), "`..`");
        refused(
            img.rig.helper.image_harvest(&path, &img.path("out")),
            "`..`",
        );
        refused(img.rig.helper.image_harvest(&img.path("b"), &path), "`..`");
    }
    assert!(img.commands().is_empty());
    img.untouched(&real);
}

/// Catches: the helper running any program but the installed `kbf-vmm` (and `chflags`
/// on the bundle's own paths), or with any argument but the ones it builds: the
/// complete list of what runs for each verb.
#[test]
fn the_helper_runs_only_kbf_vmm_and_chflags_with_its_own_arguments() {
    let img = img("argv");
    img.bundle("base");
    img.rig.helper.image_setup(&img.path("base")).unwrap();
    let flags = |names: &[&str]| {
        let paths: Vec<String> = names
            .iter()
            .map(|n| format!(" {}", img.path(&format!("base/{n}"))))
            .collect();
        format!("{CHFLAGS} -h uchg{}", paths.concat())
    };
    assert_eq!(
        img.commands(),
        [
            img.setup_command("base"),
            flags(&FILES),
            format!("{CHFLAGS} -h uchg {}", img.path("base")),
        ]
    );
    // The caller's text never reaches the argv: a trailing slash and a repeated
    // separator name the same bundle, and the argument is the configured path.
    img.rig.host.state().log.clear();
    let sloppy = format!("{}//base/", img.images.display());
    img.rig.helper.image_setup(&sloppy).unwrap();
    assert_eq!(img.commands()[0], img.setup_command("base"));
    let clone = img.bundle("clone");
    img.rig.host.state().log.clear();
    img.rig
        .helper
        .image_harvest(&img.path("clone"), &img.path("out"))
        .unwrap();
    assert_eq!(img.commands(), [img.harvest_command("clone", "out")]);
    img.untouched(&clone);
}

/// Catches: `kbf-vmm` running on a bundle the VM user can still swap or write (the
/// directory and its files must be root's and closed while it runs), the flags set
/// before the files went back (which would refuse the chown), a bundle left root's
/// afterwards, and a base image left unflagged.
#[test]
fn setup_takes_the_bundle_while_kbf_vmm_runs_and_returns_it_flagged() {
    let img = img("setup");
    let bundle = img.bundle("base");
    let (dir, disk) = (bundle.clone(), bundle.join("disk.img"));
    let seen = img.watch(move || {
        let (d, f) = (
            std::fs::metadata(&dir).unwrap(),
            std::fs::metadata(&disk).unwrap(),
        );
        format!("dir {:o} {} file {}", d.mode() & 0o7777, d.uid(), f.uid())
    });
    img.answer(img.setup_command("base"), true, "{\"status\":\"ok\"}\n", "");
    let report = img.rig.helper.image_setup(&img.path("base")).unwrap();
    assert_eq!(report, "{\"status\":\"ok\"}\n");
    let euid = rustix::process::geteuid().as_raw();
    let seen = seen.lock().unwrap().clone();
    // The bundle is closed (0700) and root's for the program, and for the flags on its
    // files; the flag on the directory comes after it is the VM user's again.
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert_eq!(
        seen[0],
        format!(
            "{} => dir 700 {euid} file {euid}",
            img.setup_command("base")
        )
    );
    assert!(
        seen[1].ends_with(&format!("=> dir 700 {euid} file {euid}")),
        "{seen:?}"
    );
    assert!(
        seen[2].ends_with(&format!("=> dir 755 {me} file {me}", me = me())),
        "{seen:?}"
    );
    img.untouched(&bundle);
    assert_eq!(std::fs::metadata(&bundle).unwrap().uid(), me());
}

/// Catches: a failed `kbf-vmm` (exit status, or not started) leaving the bundle root's
/// and closed so the VM user cannot remove or retry it, or being flagged immutable.
#[test]
fn a_failed_setup_gives_the_bundle_back_unflagged() {
    let img = img("setup-fail");
    let bundle = img.bundle("base");
    img.answer(
        img.setup_command("base"),
        false,
        "{\"status\":\"error\"}",
        "no volume",
    );
    let why = img.rig.helper.image_setup(&img.path("base")).unwrap_err();
    assert!(
        why.contains("kbf-vmm failed: no volume {\"status\":\"error\"}"),
        "{why}"
    );
    img.untouched(&bundle);
    assert_eq!(img.commands(), [img.setup_command("base")]);

    img.rig.host.state().fail.insert("command");
    refused(
        img.rig.helper.image_setup(&img.path("base")),
        "running kbf-vmm",
    );
    img.untouched(&bundle);
    assert_eq!(img.commands().len(), 2);
    img.rig.host.state().fail.clear();

    // The flag step failing is a failure too, and the bundle still goes back.
    img.rig.host.state().log.clear();
    img.answer(img.setup_command("base"), true, "", "");
    let files = FILES
        .map(|f| format!(" {}", img.path(&format!("base/{f}"))))
        .concat();
    img.answer(
        format!("{CHFLAGS} -h uchg{files}"),
        false,
        "",
        "Operation not permitted",
    );
    refused(
        img.rig.helper.image_setup(&img.path("base")),
        "chflags failed: Operation not permitted",
    );
    img.untouched(&bundle);
}

/// Catches: a bundle holding a subdirectory, a symlink or a hard link to a file
/// elsewhere being taken: the chown would reach the target, or the return to the VM
/// user would hand it a file it must not own.
#[test]
fn a_bundle_holding_anything_but_plain_files_is_refused_and_given_back() {
    let img = img("odd");
    let bundle = img.bundle("b");
    let elsewhere = img.rig.dir.join("elsewhere");
    std::fs::write(&elsewhere, "x").unwrap();
    set_mode(&elsewhere, 0o600);
    for odd in ["sub", "link", "hard"] {
        let at = bundle.join(odd);
        match odd {
            "sub" => std::fs::create_dir(&at).unwrap(),
            "link" => symlink(&elsewhere, &at).unwrap(),
            _ => std::fs::hard_link(&elsewhere, &at).unwrap(),
        }
        let says = "not a plain file with one link";
        refused(img.rig.helper.image_setup(&img.path("b")), says);
        refused(
            img.rig
                .helper
                .image_harvest(&img.path("b"), &img.path("out")),
            says,
        );
        img.untouched(&bundle);
        assert_eq!(mode(&elsewhere), 0o600);
        assert!(!img.images.join("out").exists());
        if odd == "sub" {
            std::fs::remove_dir(&at).unwrap();
        } else {
            std::fs::remove_file(&at).unwrap();
        }
    }
    assert!(img.commands().is_empty());
    // And a plain bundle still works after those.
    img.rig.helper.image_setup(&img.path("b")).unwrap();
}

/// Catches: a setup on a Mac with no VM user, or whose VM user has no account, going
/// ahead (there is nobody to hand the bundle to), or taking the bundle first.
#[test]
fn setup_needs_the_vm_user_before_it_touches_the_bundle() {
    let img = img("no-vm");
    let bundle = img.bundle("b");
    img.rig.host.state().users.clear();
    refused(img.rig.helper.image_setup(&img.path("b")), "has no account");
    img.untouched(&bundle);
    let plain = rig_vm_with(
        "img-no-vm-user",
        UidRange::new(me() + 100, me() + 103).unwrap(),
        None,
        None,
    );
    plain.helper.boot_reset(&[]).unwrap();
    plain.host.state().log.clear();
    let path = plain
        .helper
        .settings
        .image
        .dir
        .join("b")
        .display()
        .to_string();
    refused(plain.helper.image_setup(&path), "no VM user is configured");
    assert!(plain.host.state().log.is_empty());
}

/// Catches: a `kbf-vmm` or `kbf-guest` that is not root's, or another user can write,
/// being run (or copied into the guest as root); a missing or non-regular one.
#[test]
fn the_installed_programs_must_be_the_helpers_own_and_closed_to_others() {
    let img = img("programs");
    let bundle = img.bundle("b");
    let settings = img.rig.helper.settings.image.clone();
    set_mode(&settings.vmm, 0o775);
    refused(
        img.rig.helper.image_setup(&img.path("b")),
        "writable by its group or others",
    );
    refused(
        img.rig
            .helper
            .image_harvest(&img.path("b"), &img.path("out")),
        "writable by its group or others",
    );
    set_mode(&settings.vmm, 0o755);
    set_mode(&settings.guest, 0o646);
    refused(
        img.rig.helper.image_setup(&img.path("b")),
        "writable by its group or others",
    );
    // `image-harvest` does not use the guest binary.
    img.rig
        .helper
        .image_harvest(&img.path("b"), &img.path("out"))
        .unwrap();
    set_mode(&settings.guest, 0o755);
    std::fs::remove_file(&settings.guest).unwrap();
    refused(img.rig.helper.image_setup(&img.path("b")), "opening");
    std::fs::create_dir(&settings.guest).unwrap();
    refused(
        img.rig.helper.image_setup(&img.path("b")),
        "is not a regular file",
    );
    img.untouched(&bundle);
}

/// Catches: an images directory other users can write (they could swap a bundle's name
/// for a link), or a group-writable one without the sticky bit; and the sticky,
/// group-writable layout the VM user needs being refused.
#[test]
fn the_images_directory_must_be_closed_to_others() {
    let img = img("dir");
    let bundle = img.bundle("b");
    for open in [0o777, 0o1777, 0o775, 0o757] {
        set_mode(&img.images, open);
        refused(
            img.rig.helper.image_setup(&img.path("b")),
            "the images directory",
        );
    }
    set_mode(&img.images, 0o1775);
    img.rig.helper.image_setup(&img.path("b")).unwrap();
    set_mode(&img.images, 0o755);
    std::fs::remove_dir_all(&bundle).unwrap();
    std::fs::remove_dir(&img.images).unwrap();
    refused(
        img.rig.helper.image_setup(&img.path("b")),
        "opening the images directory",
    );
    assert_eq!(img.commands().len(), 3);
}

/// Catches: an images directory owned by another user being trusted.
#[test]
fn an_images_directory_another_user_owns_is_refused() {
    let mut stat = fstat(std::fs::File::open(".").unwrap()).unwrap();
    stat.st_uid = rustix::process::geteuid().as_raw() + 1;
    stat.st_mode = 0o755;
    assert!(
        shared_safely(&stat)
            .unwrap_err()
            .contains("not the helper's uid")
    );
}

/// Catches: a harvest output directory that is not new, not root's or not mode 0700
/// (readable by the VM user whose guest it was copied from), or outside the images
/// directory; and the harvest not being what the helper builds.
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
    img.answer(
        img.harvest_command("clone", "out"),
        true,
        "{\"harvest\":1}",
        "",
    );
    let report = img
        .rig
        .helper
        .image_harvest(&img.path("clone"), &img.path("out"))
        .unwrap();
    assert_eq!(report, "{\"harvest\":1}");
    let euid = rustix::process::geteuid().as_raw();
    assert_eq!(
        seen.lock().unwrap().clone(),
        [format!(
            "{} => out 700 {euid}",
            img.harvest_command("clone", "out")
        )]
    );
    let made = std::fs::metadata(&out).unwrap();
    assert!(made.is_dir());
    assert_eq!(made.mode() & 0o7777, 0o700);
    assert_eq!(made.uid(), euid);
    assert_eq!(made.gid(), rustix::process::getegid().as_raw());
    assert_eq!(out.parent().unwrap(), img.images);
    img.untouched(&bundle);
    assert_eq!(std::fs::metadata(&bundle).unwrap().uid(), me());
    assert_eq!(img.commands().len(), 1);
}

/// Catches: a harvest into an existing name (a directory, a file, a link that would lead
/// anywhere), into the bundle itself, or one that removes what was there.
#[test]
fn harvest_makes_only_a_new_output_directory() {
    let img = img("harvest-new");
    let bundle = img.bundle("clone");
    std::fs::create_dir(img.images.join("taken")).unwrap();
    std::fs::write(img.images.join("taken/keep"), "x").unwrap();
    std::fs::write(img.images.join("file"), "x").unwrap();
    symlink(img.rig.dir.join("elsewhere"), img.images.join("link")).unwrap();
    for name in ["taken", "file", "link"] {
        refused(
            img.rig
                .helper
                .image_harvest(&img.path("clone"), &img.path(name)),
            "making",
        );
    }
    refused(
        img.rig
            .helper
            .image_harvest(&img.path("clone"), &img.path("clone")),
        "must not be the bundle",
    );
    assert!(img.images.join("taken/keep").exists());
    assert!(!img.rig.dir.join("elsewhere").exists());
    assert!(img.commands().is_empty());
    img.untouched(&bundle);
}

/// Catches: a failed harvest leaving an empty output directory behind (a retry would be
/// refused), removing a partial copy a person may want, or leaving the bundle root's.
#[test]
fn a_failed_harvest_removes_its_empty_output_directory_and_returns_the_bundle() {
    let img = img("harvest-fail");
    let bundle = img.bundle("clone");
    img.answer(img.harvest_command("clone", "out"), false, "", "no volume");
    refused(
        img.rig
            .helper
            .image_harvest(&img.path("clone"), &img.path("out")),
        "kbf-vmm failed: no volume",
    );
    assert!(!img.images.join("out").exists());
    img.untouched(&bundle);
    // A partial copy stays.
    let partial = img.images.join("out");
    img.rig.host.state().on_command = Some(Box::new(move |_| {
        std::fs::create_dir_all(partial.join("files")).unwrap();
    }));
    refused(
        img.rig
            .helper
            .image_harvest(&img.path("clone"), &img.path("out")),
        "kbf-vmm failed",
    );
    assert!(img.images.join("out/files").is_dir());
    img.untouched(&bundle);
}

/// Catches: a harvest of a bundle the VM user owns giving it back to root (or to the VM
/// user, when another user made the clone): the owners go back to what they were.
#[test]
fn harvest_gives_the_bundle_back_to_its_owner_and_mode() {
    let img = img("harvest-owner");
    let bundle = img.bundle("clone");
    set_mode(&bundle, 0o750);
    set_mode(&bundle.join("disk.img"), 0o600);
    img.rig
        .helper
        .image_harvest(&img.path("clone"), &img.path("out"))
        .unwrap();
    assert_eq!(mode(&bundle), 0o750);
    assert_eq!(mode(&bundle.join("disk.img")), 0o600);
    assert_eq!(mode(&bundle.join("aux.img")), 0o644);
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
/// not text being passed on as an argument.
#[test]
fn errors_name_the_step_and_a_path_must_be_text() {
    let why = at("locking b")(rustix::io::Errno::ACCESS);
    assert!(why.starts_with("locking b: "), "{why}");
    warn_if_failed("a step", Ok(()));
    warn_if_failed("a step", Err("failed".to_owned()));
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
    assert!(cut.len() <= IMAGE_REPORT_BYTES);
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
    helper.image_setup(&img.path("b")).unwrap();
}

/// Catches: the same bundle set up twice at once; the second waits for the first.
#[test]
fn image_verbs_run_one_at_a_time() {
    let img = img("serial");
    img.bundle("b");
    let held = img.rig.helper.one_image_job();
    let helper = &img.rig.helper;
    let path = img.path("b");
    std::thread::scope(|scope| {
        let second = scope.spawn(|| helper.image_setup(&path));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            img.commands().is_empty(),
            "ran while another verb held the lock"
        );
        drop(held);
        second.join().unwrap().unwrap();
    });
    assert_eq!(img.commands().len(), 3);
}
