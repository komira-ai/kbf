//! The image verbs over a fake host: real files in scratch staging and images
//! directories, the program runs recorded and answered by the fake.
//!
//! The tests run as an ordinary user, so "the VM user's" bundle is one the helper does
//! not count as closed: group-writable (0775 and 0664). A root's closed object is 0750 and
//! 0640.

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
/// to), over a staging and an images directory and the installed programs' stand-ins.
struct Img {
    rig: Rig,
    images: PathBuf,
    staging: PathBuf,
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
    for (dir, open) in [(&settings.dir, 0o755), (&settings.staging, 0o775)] {
        std::fs::create_dir(dir).unwrap();
        set_mode(dir, open);
    }
    for program in [&settings.vmm, &settings.guest] {
        std::fs::write(program, "program").unwrap();
        set_mode(program, 0o755);
    }
    rig.host.state().log.clear();
    Img {
        vmm: settings.vmm.display().to_string(),
        guest: settings.guest.display().to_string(),
        images: settings.dir,
        staging: settings.staging,
        rig,
    }
}

impl Img {
    /// A name in the images directory, as text.
    fn path(&self, name: &str) -> String {
        self.images.join(name).display().to_string()
    }

    /// A name in the staging directory, as text.
    fn spath(&self, name: &str) -> String {
        self.staging.join(name).display().to_string()
    }

    /// A bundle in `dir`: the four files, in a directory; `dir_mode` and `file_mode`.
    fn make(dir: &Path, name: &str, dir_mode: u32, file_mode: u32) -> PathBuf {
        let bundle = dir.join(name);
        std::fs::create_dir(&bundle).unwrap();
        for file in FILES {
            std::fs::write(bundle.join(file), file).unwrap();
            set_mode(&bundle.join(file), file_mode);
        }
        set_mode(&bundle, dir_mode);
        bundle
    }

    /// A bundle the VM user built, in the staging directory.
    fn bundle(&self, name: &str) -> PathBuf {
        Self::make(&self.staging, name, 0o775, 0o664)
    }

    /// A sealed base in the images directory.
    fn sealed(&self, name: &str) -> PathBuf {
        Self::make(&self.images, name, 0o750, 0o640)
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

    /// `kbf-vmm image harvest ...` for the bundle at `bundle` (a path as text).
    fn harvest_command(&self, bundle: &str, out: &str) -> String {
        format!(
            "{} image harvest --bundle {bundle} --out {}/files",
            self.vmm,
            self.path(out)
        )
    }

    /// `chflags -h uchg` on the four files of the base `name`, then on the base.
    fn flag_commands(&self, name: &str) -> [String; 2] {
        let files = FILES.map(|f| format!(" {}", self.path(&format!("{name}/{f}"))));
        [
            format!("{CHFLAGS} -h uchg{}", files.concat()),
            format!("{CHFLAGS} -h uchg {}", self.path(name)),
        ]
    }

    /// `chflags -h nouchg` on the files of the base `name`, then on the base: the undo.
    fn unflag_command(&self, name: &str) -> String {
        let files = FILES.map(|f| format!(" {}", self.path(&format!("{name}/{f}"))));
        format!("{CHFLAGS} -h nouchg{} {}", files.concat(), self.path(name))
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

    /// A bundle the VM user built is as it was: its modes, in `dir`.
    fn as_built(&self, bundle: &Path) {
        assert_eq!(mode(bundle), 0o775, "{}", bundle.display());
        for file in FILES {
            assert_eq!(mode(&bundle.join(file)), 0o664, "{file}");
        }
    }

    /// A sealed base is as sealed.
    fn as_sealed(&self, bundle: &Path) {
        assert_eq!(mode(bundle), 0o750, "{}", bundle.display());
        for file in FILES {
            assert_eq!(mode(&bundle.join(file)), 0o640, "{file}");
        }
    }
}

fn refused(result: Result<String, String>, says: &str) {
    let why = result.unwrap_err();
    assert!(why.contains(says), "{why}");
}

/// Catches: a bundle or output path outside the directory it must be in (another
/// directory, a sibling whose name starts like it, a relative name, the directory itself,
/// a file inside a bundle, the images directory for a setup) reaching `kbf-vmm`.
#[test]
fn a_bundle_outside_the_staging_directory_is_refused() {
    let img = img("outside");
    let inside = img.bundle("b");
    let other = img.rig.dir.join("other");
    std::fs::create_dir(&other).unwrap();
    let lookalike = img.rig.dir.join("staging-evil");
    std::fs::create_dir(&lookalike).unwrap();
    let staging = img.staging.display().to_string();
    let images = img.images.display().to_string();
    let not_a_name = "not a name directly inside";
    let neither = [
        other.display().to_string(),
        lookalike.join("b").display().to_string(),
        "b".to_owned(),
        "/".to_owned(),
    ];
    for outside in &neither {
        let helper = &img.rig.helper;
        refused(
            helper.image_setup(outside),
            "not inside the staging directory",
        );
        refused(
            helper.image_harvest(outside, &img.path("out")),
            "inside neither",
        );
    }
    // A sealed base's name is not a setup bundle: setup takes from staging only.
    refused(
        img.rig.helper.image_setup(&img.path("b")),
        "not inside the staging directory",
    );
    for deeper in [staging.clone(), img.spath("b/disk.img")] {
        refused(img.rig.helper.image_setup(&deeper), not_a_name);
        refused(
            img.rig.helper.image_harvest(&deeper, &img.path("out")),
            not_a_name,
        );
    }
    refused(
        img.rig.helper.image_harvest(&images, &img.path("out")),
        not_a_name,
    );
    // The output goes to the images directory only.
    let in_staging = img.spath("o");
    for out in neither.iter().chain([&in_staging]) {
        refused(
            img.rig.helper.image_harvest(&img.spath("b"), out),
            "not inside the images directory",
        );
    }
    refused(img.rig.helper.image_setup("b\0"), "NUL");
    assert!(img.commands().is_empty());
    assert!(!img.images.join("out").exists());
    img.as_built(&inside);
}

/// Catches: a bundle that is a symlink being followed (the helper would attach a disk
/// image elsewhere as root), or a link's target being taken (chowned, locked), in either
/// directory.
#[test]
fn a_symlinked_bundle_is_refused() {
    let img = img("link");
    let real = img.rig.dir.join("real");
    std::fs::create_dir(&real).unwrap();
    for file in FILES {
        std::fs::write(real.join(file), file).unwrap();
        set_mode(&real.join(file), 0o664);
    }
    set_mode(&real, 0o775);
    for (dir, prefix) in [(&img.staging, ""), (&img.images, "i")] {
        symlink(&real, dir.join(format!("{prefix}link"))).unwrap();
        let nowhere = img.rig.dir.join("nowhere");
        symlink(nowhere, dir.join(format!("{prefix}dangling"))).unwrap();
    }
    for name in ["link", "dangling"] {
        refused(
            img.rig.helper.image_setup(&img.spath(name)),
            "not a plain directory",
        );
        for bundle in [img.spath(name), img.path(&format!("i{name}"))] {
            refused(
                img.rig.helper.image_harvest(&bundle, &img.path("out")),
                "not a plain directory",
            );
        }
    }
    assert!(img.commands().is_empty());
    img.as_built(&real);
    assert!(
        !img.images.join("out").exists(),
        "an output directory was left"
    );
}

/// Catches: a path that reaches a bundle through a symlinked parent being accepted: a
/// link to the staging directory (outside it by name), and a link inside it (a name below
/// the directory is one level deep or refused).
#[test]
fn a_symlink_in_a_parent_of_the_bundle_is_refused() {
    let img = img("parent");
    let real = img.bundle("b");
    let alias = img.rig.dir.join("alias");
    symlink(&img.staging, &alias).unwrap();
    refused(
        img.rig
            .helper
            .image_setup(&alias.join("b").display().to_string()),
        "not inside the staging directory",
    );
    let sub = img.staging.join("sub");
    std::fs::create_dir(&sub).unwrap();
    symlink(&real, sub.join("b")).unwrap();
    symlink(&img.staging, img.staging.join("self")).unwrap();
    for deeper in ["sub/b", "self/b"] {
        refused(
            img.rig.helper.image_setup(&img.spath(deeper)),
            "not a name directly inside",
        );
        refused(
            img.rig
                .helper
                .image_harvest(&img.spath("b"), &img.path(deeper)),
            "not a name directly inside",
        );
    }
    assert!(img.commands().is_empty());
    img.as_built(&real);
}

/// Catches: `..` accepted, at the front, in the middle or at the end, where it would
/// lead out of the directory or to another bundle.
#[test]
fn dotdot_is_refused() {
    let img = img("dotdot");
    let real = img.bundle("b");
    let staging = img.staging.display().to_string();
    let images = img.images.display().to_string();
    for path in [
        format!("{staging}/../staging/b"),
        format!("{staging}/b/.."),
        format!("{staging}/.."),
        format!("{staging}/b/../b"),
        format!("{staging}/../../etc"),
    ] {
        refused(img.rig.helper.image_setup(&path), "`..`");
        refused(
            img.rig.helper.image_harvest(&path, &img.path("out")),
            "`..`",
        );
    }
    for path in [
        format!("{images}/../images/o"),
        format!("{images}/o/.."),
        format!("{images}/.."),
    ] {
        refused(img.rig.helper.image_harvest(&img.spath("b"), &path), "`..`");
        refused(
            img.rig.helper.image_harvest(&path, &img.path("out")),
            "`..`",
        );
    }
    assert!(img.commands().is_empty());
    img.as_built(&real);
}

/// Catches: the helper running any program but the installed `kbf-vmm` (and `chflags` on
/// the base's own paths), or with any argument but the ones it builds: the complete list
/// of what runs for each verb and each place a harvest bundle can be.
#[test]
fn the_helper_runs_only_kbf_vmm_and_chflags_with_its_own_arguments() {
    let img = img("argv");
    img.bundle("base");
    img.rig.helper.image_setup(&img.spath("base")).unwrap();
    let [files, dir] = img.flag_commands("base");
    assert_eq!(img.commands(), [img.setup_command("base"), files, dir]);
    // The caller's text never reaches the argv: a repeated separator and a trailing slash
    // name the same bundle, and the argument is the configured path.
    img.bundle("second");
    img.rig.host.state().log.clear();
    let sloppy = format!("{}//second/", img.staging.display());
    img.rig.helper.image_setup(&sloppy).unwrap();
    assert_eq!(img.commands()[0], img.setup_command("second"));
    let clone = img.bundle("clone");
    img.rig.host.state().log.clear();
    let helper = &img.rig.helper;
    helper
        .image_harvest(&img.spath("clone"), &img.path("out"))
        .unwrap();
    helper
        .image_harvest(&img.path("base"), &img.path("out2"))
        .unwrap();
    assert_eq!(
        img.commands(),
        [
            img.harvest_command(&img.spath("clone"), "out"),
            img.harvest_command(&img.path("base"), "out2"),
        ]
    );
    img.as_built(&clone);
}

/// Catches: `kbf-vmm` running on the user's own inodes (a copy must be what it attaches:
/// new files in a directory that is root's and 0700 while it runs), the original in staging
/// touched (it must keep its owner, modes and place), the flags set before the files were
/// sealed, or a base left readable by others: the base is root's, 0750 and 0640, group the
/// VM user's. The happy path for `image-setup`.
#[test]
fn setup_copies_the_bundle_seals_the_copy_and_leaves_the_original() {
    let img = img("setup");
    let built = img.bundle("base");
    let (dir, disk, original) = (
        img.images.join("base"),
        img.images.join("base/disk.img"),
        built.join("disk.img"),
    );
    let seen = img.watch(move || {
        format!(
            "dir {:o} disk {:o} original {:o}",
            mode(&dir),
            mode(&disk),
            mode(&original)
        )
    });
    img.answer(img.setup_command("base"), true, "{\"status\":\"ok\"}\n", "");
    let report = img.rig.helper.image_setup(&img.spath("base")).unwrap();
    assert_eq!(report, "{\"status\":\"ok\"}\n");
    let [files, dir] = img.flag_commands("base");
    // Closed (0700) with the helper's own 0600 files for the program; both are sealed
    // before either is flagged.
    assert_eq!(
        *seen.lock().unwrap(),
        [
            format!(
                "{} => dir 700 disk 600 original 664",
                img.setup_command("base")
            ),
            format!("{files} => dir 750 disk 640 original 664"),
            format!("{dir} => dir 750 disk 640 original 664"),
        ]
    );
    let base = img.images.join("base");
    img.as_sealed(&base);
    img.as_built(&built);
    let euid = rustix::process::geteuid().as_raw();
    for path in std::iter::once(base.clone()).chain(FILES.map(|f| base.join(f))) {
        let owner = std::fs::metadata(&path).unwrap();
        assert_eq!(
            (owner.uid(), owner.gid()),
            (euid, my_gid()),
            "{}",
            path.display()
        );
    }
    for file in FILES {
        let (copy, from) = (base.join(file), built.join(file));
        assert_eq!(std::fs::read(&copy).unwrap(), std::fs::read(&from).unwrap());
        let (copy, from) = (
            std::fs::metadata(copy).unwrap(),
            std::fs::metadata(from).unwrap(),
        );
        assert_ne!(copy.ino(), from.ino(), "{file} is the user's inode");
    }
    assert_eq!(
        img.rig
            .host
            .state()
            .log
            .iter()
            .filter(|l| l.starts_with("clone_file "))
            .count(),
        4
    );
}

/// Catches: the main hole of taking the user's inodes in place: a write descriptor the user
/// opened before the helper looked reaching the base, during `kbf-vmm` or after the seal.
/// The base is a copy; the writes land in the user's original only.
#[test]
fn a_write_through_a_descriptor_the_user_held_does_not_reach_the_sealed_base() {
    use std::io::{Seek as _, Write as _};
    let img = img("held");
    let built = img.bundle("base");
    let held = std::fs::OpenOptions::new()
        .write(true)
        .open(built.join("disk.img"))
        .unwrap();
    let held = Arc::new(Mutex::new(held));
    let during = Arc::clone(&held);
    img.rig.host.state().on_command = Some(Box::new(move |text| {
        if text.contains("image setup") {
            during.lock().unwrap().write_all(b"DURING").unwrap();
        }
    }));
    img.rig.helper.image_setup(&img.spath("base")).unwrap();
    let mut late = held.lock().unwrap();
    late.rewind().unwrap();
    late.write_all(b"AFTER!").unwrap();
    drop(late);
    let base = img.images.join("base");
    assert_eq!(std::fs::read(base.join("disk.img")).unwrap(), b"disk.img");
    assert_eq!(
        &std::fs::read(built.join("disk.img")).unwrap()[..6],
        b"AFTER!"
    );
    img.as_sealed(&base);
}

/// Catches: a setup over a base of the same name (the rename would put the new bundle
/// in a sealed base, or fail half way), or one that leaves the bundle moved.
#[test]
fn setup_refuses_a_name_the_images_directory_has() {
    let img = img("exists");
    let built = img.bundle("base");
    let sealed = img.sealed("base");
    refused(
        img.rig.helper.image_setup(&img.spath("base")),
        "is in the images directory already",
    );
    img.as_built(&built);
    img.as_sealed(&sealed);
    assert!(img.commands().is_empty());
}

/// Catches: a failed `kbf-vmm` (exit status, or not started), seal or flag leaving a copy in
/// the images directory (an unsealed base the VM user could clone), or touching the user's
/// original. With `uchg` really blocking removal on the fake host, a failure after the
/// flags are set must clear them before it removes the copy.
#[test]
fn a_failed_setup_removes_the_copy_clearing_uchg_first() {
    let img = img("setup-fail");
    let built = img.bundle("base");
    let gone = |img: &Img| {
        assert!(
            !img.images.join("base").exists(),
            "an unsealed base was left"
        );
        img.as_built(&built);
    };
    img.answer(
        img.setup_command("base"),
        false,
        "{\"status\":\"error\"}",
        "no volume",
    );
    let why = img.rig.helper.image_setup(&img.spath("base")).unwrap_err();
    assert!(
        why.contains("kbf-vmm failed: no volume {\"status\":\"error\"}"),
        "{why}"
    );
    gone(&img);
    assert_eq!(
        img.commands(),
        [img.setup_command("base"), img.unflag_command("base")]
    );

    img.rig.host.state().fail.insert("command");
    refused(
        img.rig.helper.image_setup(&img.spath("base")),
        "running kbf-vmm",
    );
    gone(&img);
    assert_eq!(img.commands().len(), 4);
    img.rig.host.state().fail.clear();
    img.rig.host.state().log.clear();

    // The files flagged and the directory not: removal needs the flags cleared first.
    img.rig.host.state().enforce_uchg = true;
    img.answer(img.setup_command("base"), true, "", "");
    let [files, dir] = img.flag_commands("base");
    img.answer(dir.clone(), false, "", "no");
    refused(
        img.rig.helper.image_setup(&img.spath("base")),
        "chflags failed: no",
    );
    gone(&img);
    let unflag: Vec<String> = img
        .commands()
        .into_iter()
        .filter(|c| c.contains("nouchg"))
        .collect();
    assert_eq!(unflag.len(), 1, "{unflag:?}");
    assert!(
        unflag[0].starts_with(&format!("{CHFLAGS} -h nouchg ")),
        "{unflag:?}"
    );
    assert!(img.rig.host.state().immutable.is_empty());

    // The first flag step failing: the flag may be set, so it is cleared too.
    img.answer(files.clone(), false, "", "Operation not permitted");
    refused(
        img.rig.helper.image_setup(&img.spath("base")),
        "chflags failed: Operation not permitted",
    );
    gone(&img);
    img.rig.host.state().enforce_uchg = false;
    img.answer(files, true, "", "");
    img.answer(dir, true, "", "");
    img.rig.helper.image_setup(&img.spath("base")).unwrap();
    img.as_sealed(&img.images.join("base"));
}

/// Catches: a copy that fails half way (the clone, the images directory not writable)
/// leaving files or a directory behind, or touching the user's original.
#[test]
fn a_failed_copy_leaves_nothing_in_the_images_directory() {
    let img = img("copy-fail");
    let built = img.bundle("base");
    img.rig.host.state().fail.insert("clone_file");
    refused(img.rig.helper.image_setup(&img.spath("base")), "copying");
    assert!(!img.images.join("base").exists());
    img.rig.host.state().fail.clear();
    set_mode(&img.images, 0o555);
    refused(
        img.rig.helper.image_setup(&img.spath("base")),
        "making base in the images directory",
    );
    set_mode(&img.images, 0o755);
    assert!(!img.images.join("base").exists());
    img.as_built(&built);
    // Only the first attempt got as far as the copy, which may carry `uchg` from its first
    // file on, so the undo cleared it (on the empty directory).
    assert_eq!(
        img.commands(),
        [format!("{CHFLAGS} -h nouchg {}", img.path("base"))]
    );
}

/// Catches: a bundle holding a subdirectory, a symlink or a hard link to a file
/// elsewhere being taken: the chown would reach the target, or the seal would give the
/// target to the VM user's group.
#[test]
fn a_bundle_holding_anything_but_plain_files_is_refused_by_both_verbs() {
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
        let says = "is not a plain file";
        refused(img.rig.helper.image_setup(&img.spath("b")), says);
        refused(
            img.rig
                .helper
                .image_harvest(&img.spath("b"), &img.path("out")),
            says,
        );
        img.as_built(&bundle);
        assert_eq!(mode(&elsewhere), 0o600);
        assert!(!img.images.join("out").exists());
        assert!(!img.images.join("b").exists());
        if odd == "sub" {
            std::fs::remove_dir(&at).unwrap();
        } else {
            std::fs::remove_file(&at).unwrap();
        }
    }
    // More entries than a bundle can hold are refused before anything is copied.
    for n in 0..MAX_ENTRIES {
        std::fs::write(bundle.join(format!("extra{n}")), "x").unwrap();
    }
    refused(
        img.rig.helper.image_setup(&img.spath("b")),
        "holds more than 16 entries",
    );
    assert!(!img.images.join("b").exists());
    for n in 0..MAX_ENTRIES {
        std::fs::remove_file(bundle.join(format!("extra{n}"))).unwrap();
    }
    assert!(img.commands().is_empty());
    // And a plain bundle still works after those.
    img.rig.helper.image_setup(&img.spath("b")).unwrap();
}

/// Catches: a setup on a Mac with no VM user, or whose VM user has no account, going
/// ahead (there is no group to seal the base for), or taking the bundle first.
#[test]
fn setup_needs_the_vm_user_before_it_touches_the_bundle() {
    let img = img("no-vm");
    let bundle = img.bundle("b");
    img.rig.host.state().users.clear();
    refused(
        img.rig.helper.image_setup(&img.spath("b")),
        "has no account",
    );
    img.as_built(&bundle);
    let range = UidRange::new(me() + 100, me() + 103).unwrap();
    let plain = rig_vm_with("img-no-vm-user", range, None, None);
    plain.helper.boot_reset(&[]).unwrap();
    plain.host.state().log.clear();
    let path = plain
        .helper
        .settings
        .image
        .staging
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
        img.rig.helper.image_setup(&img.spath("b")),
        "writable by its group or others",
    );
    refused(
        img.rig
            .helper
            .image_harvest(&img.spath("b"), &img.path("out")),
        "writable by its group or others",
    );
    set_mode(&settings.vmm, 0o755);
    set_mode(&settings.guest, 0o646);
    refused(
        img.rig.helper.image_setup(&img.spath("b")),
        "writable by its group or others",
    );
    // `image-harvest` does not use the guest binary.
    img.rig
        .helper
        .image_harvest(&img.spath("b"), &img.path("out"))
        .unwrap();
    set_mode(&settings.guest, 0o755);
    std::fs::remove_file(&settings.guest).unwrap();
    refused(img.rig.helper.image_setup(&img.spath("b")), "opening");
    std::fs::create_dir(&settings.guest).unwrap();
    refused(
        img.rig.helper.image_setup(&img.spath("b")),
        "is not a regular file",
    );
    img.as_built(&bundle);
}

/// Catches: an images directory other users can write (they could add a name that the
/// VM user then reads as a base), a group-writable one even with the sticky bit (the
/// images directory is root's alone), or missing; and a missing staging directory.
#[test]
fn the_images_directory_must_be_closed_to_group_and_others() {
    let img = img("dir");
    let bundle = img.bundle("b");
    for open in [0o777, 0o1777, 0o775, 0o1775, 0o757] {
        set_mode(&img.images, open);
        refused(
            img.rig.helper.image_setup(&img.spath("b")),
            "the images directory",
        );
        refused(
            img.rig
                .helper
                .image_harvest(&img.spath("b"), &img.path("out")),
            "writable by its group or others",
        );
    }
    img.as_built(&bundle);
    set_mode(&img.images, 0o755);
    std::fs::remove_dir(&img.images).unwrap();
    refused(img.rig.helper.image_setup(&img.spath("b")), "opening");
    std::fs::create_dir(&img.images).unwrap();
    set_mode(&img.images, 0o755);
    std::fs::remove_dir_all(&img.staging).unwrap();
    refused(img.rig.helper.image_setup(&img.spath("b")), "opening");
    assert!(img.commands().is_empty());
}

mod copy;
mod harvest;
mod units;
