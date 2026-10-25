use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use super::*;
use crate::pkg_linux::Out;
use kbf_testkit::dir::TestDir;

/// A defect of the package or of the system, planted in the model of the container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Defect {
    None,
    BuildFails,
    /// The old `kbf-installer` deb does not own the trust directory, so there is nothing to
    /// hand over.
    OldInstallerOwnsNoTrust,
    InstallOfNewFails,
    /// The new deb does not say it replaces the installer: the files stay the installer's.
    FilesStayWithInstaller,
    /// The new deb `Breaks` the installer: a plain `dpkg --install` refuses on a host that has it
    /// (apt removes the installer instead).
    BreaksTheInstaller,
    /// The new deb `Depends` on `crun`: a plain `dpkg --install` leaves it unpacked and
    /// unconfigured on a host without it (apt installs it).
    DependsOnCrun,
    /// The image already has `crun`, so the host without the installer does not lack it.
    CrunPreinstalled,
    VersionStale,
    PurgeTakesKbfFiles,
    UsersRenumbered,
    TmpfilesDirMissing,
    WrongOwnerOrMode,
}

const OLD_INSTALLER: &str = "/p/kbf-installer-old.deb";
const OLD_KBF: &str = "/p/kbf-old.deb";
const NEW_KBF: &str = "/p/kbf-new.deb";
const LISTING: &str = "file 0755 /usr/bin/kbf 1111111111111111111111111111111111111111111111111111111111111111\n\
dir 0755 /usr/share/kbf -\n\
dir 0755 /usr/share/kbf/trust -\n\
file 0755 /usr/share/kbf/trust/gh 2222222222222222222222222222222222222222222222222222222222222222\n\
file 0644 /usr/share/kbf/trust/installer.conf 3333333333333333333333333333333333333333333333333333333333333333\n\
file 0644 /usr/lib/systemd/system/kbf.slice 4444444444444444444444444444444444444444444444444444444444444444\n\
file 0644 /usr/lib/sysusers.d/kbf.conf 5555555555555555555555555555555555555555555555555555555555555555\n";

fn ok(stdout: &str) -> Out {
    Out {
        ok: true,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    }
}

fn fail(stderr: &str) -> Out {
    Out {
        ok: false,
        stdout: String::new(),
        stderr: stderr.to_owned(),
    }
}

/// A model of a container with apt and dpkg: which packages are installed, who owns which
/// path, the users, and every argv of `docker`.
struct World {
    defect: Defect,
    entries: Vec<Entry>,
    installed: BTreeSet<&'static str>,
    owner: BTreeMap<String, &'static str>,
    present: BTreeSet<String>,
    uid_base: u32,
    kbf_version: &'static str,
    /// The new `kbf` is unpacked and not configured.
    unconfigured: bool,
    installer_purged: bool,
    log: Vec<String>,
}

/// The files the old installer deb owns, besides the ones the listing names.
const OLD_INSTALLER_ONLY: [&str; 2] = [
    "/usr/bin/kbf-installer",
    "/usr/share/kbf-installer/trust/gh",
];

impl World {
    fn new(defect: Defect, entries: Vec<Entry>) -> World {
        World {
            defect,
            entries,
            installed: BTreeSet::new(),
            owner: BTreeMap::new(),
            present: BTreeSet::new(),
            uid_base: 990,
            kbf_version: "0.1.0+old",
            unconfigured: false,
            installer_purged: false,
            log: Vec::new(),
        }
    }

    fn is(&self, d: Defect) -> bool {
        self.defect == d
    }

    fn own(&mut self, path: &str, package: &'static str) {
        self.owner.insert(path.to_owned(), package);
        self.present.insert(path.to_owned());
    }

    /// The model of a fresh container: nothing installed, no file, the users not made.
    fn reset(&mut self) {
        self.installed.clear();
        self.owner.clear();
        self.present.clear();
        self.kbf_version = "0.1.0+old";
        self.unconfigured = false;
        self.installer_purged = false;
        if self.is(Defect::CrunPreinstalled) {
            self.installed.insert("crun");
        }
    }

    /// `deb` installed by apt (`by_apt`, which resolves a `Break` by removing the installer and a
    /// `Depends` by installing it) or by a plain `dpkg --install` (which does neither).
    fn install(&mut self, deb: &str, by_apt: bool) -> Out {
        if deb.ends_with("kbf-installer-old.deb") {
            self.installed.insert("kbf-installer");
            for path in OLD_INSTALLER_ONLY {
                self.own(path, "kbf-installer");
            }
            if !self.is(Defect::OldInstallerOwnsNoTrust) {
                self.own("/usr/lib/systemd/system/kbf.slice", "kbf-installer");
            }
        } else if deb.ends_with("kbf-old.deb") {
            self.installed.insert("kbf");
            self.kbf_version = "0.1.0+old";
            self.own("/usr/bin/kbf", "kbf");
        } else {
            if self.is(Defect::InstallOfNewFails) {
                return fail("E: Unable to correct problems, you have held broken packages.");
            }
            if self.is(Defect::BreaksTheInstaller) && self.installed.contains("kbf-installer") {
                if !by_apt {
                    return fail(
                        "dpkg: regarding /var/tmp/kbf-new.deb containing kbf:\n kbf breaks kbf-installer (<< 0.1.1) and is installed.",
                    );
                }
                // apt removes what the new package breaks.
                self.installed.remove("kbf-installer");
                for path in OLD_INSTALLER_ONLY {
                    self.present.remove(path);
                }
            }
            if self.is(Defect::DependsOnCrun) && !self.installed.contains("crun") {
                if by_apt {
                    self.installed.insert("crun");
                } else {
                    self.unconfigured = true;
                }
            }
            self.installed.insert("kbf");
            if !self.is(Defect::VersionStale) {
                self.kbf_version = "0.1.0+new";
            }
            for e in self.entries.clone() {
                let held = self.owner.get(&e.dst).copied();
                if held == Some("kbf-installer") && self.is(Defect::FilesStayWithInstaller) {
                    continue;
                }
                self.own(&e.dst, "kbf");
            }
        }
        if self.unconfigured {
            return fail(
                "dpkg: error processing package kbf (--install):\n dependency problems - leaving unconfigured",
            );
        }
        ok("")
    }

    /// The model's answer to `docker <args>`.
    fn docker(&mut self, args: &[&str]) -> Out {
        self.log.push(args.join(" "));
        match args {
            ["build", ..] if self.is(Defect::BuildFails) => fail("no such image"),
            ["run", ..] => {
                self.reset();
                ok("")
            }
            ["exec", _, rest @ ..] => self.exec(rest),
            _ => ok(""),
        }
    }

    fn exec(&mut self, args: &[&str]) -> Out {
        match args {
            ["systemctl", "is-system-running", "--wait"] => ok("running"),
            ["apt-get", "-y", "--allow-downgrades", "install", deb] => self.install(deb, true),
            ["dpkg", "--install", deb] => self.install(deb, false),
            ["dpkg-query", "-W", "-f=${Status}", package] => {
                if self.unconfigured && *package == "kbf" {
                    ok("install ok unpacked")
                } else if self.installed.contains(package) {
                    ok("install ok installed")
                } else {
                    fail("")
                }
            }
            ["dpkg-query", "-W", "-f=${Version}", "kbf"] => ok(self.kbf_version),
            ["dpkg-deb", "-f", _, "Version"] => ok("0.1.0+new"),
            ["dpkg", "-S", path] => match self.owner.get(*path) {
                Some(owner) if self.present.contains(*path) => ok(&format!("{owner}: {path}\n")),
                _ => fail("dpkg-query: no path found matching pattern"),
            },
            ["dpkg", "--purge", "kbf-installer"] => {
                let held: Vec<String> = self
                    .owner
                    .iter()
                    .filter(|(_, o)| **o == "kbf-installer")
                    .map(|(p, _)| p.clone())
                    .collect();
                for path in held {
                    self.present.remove(&path);
                }
                self.installed.remove("kbf-installer");
                self.installer_purged = true;
                if self.is(Defect::PurgeTakesKbfFiles) {
                    self.present.clear();
                }
                ok("")
            }
            ["dpkg", "--audit"] => ok(if self.unconfigured {
                "The following packages have been unpacked but not yet configured:\n kbf"
            } else {
                ""
            }),
            ["systemd-sysusers"] | ["systemd-tmpfiles", "--create"] => ok(""),
            ["id", "-u", user] => {
                let at = USERS.iter().position(|u| u == user).expect("a known user");
                let renumbered = self.is(Defect::UsersRenumbered) && self.installer_purged;
                let base = self.uid_base + if renumbered { 100 } else { 0 };
                ok(&format!("{}\n", base + at as u32))
            }
            ["getent", "group", "kbf-signer-clients"] => {
                ok("kbf-signer-clients:x:991:kbf-signer,kbf-server\n")
            }
            ["stat", "-c", "%U:%G:%a:%F", path] => self.stat(path),
            ["sha256sum", path] => {
                let e = self
                    .entries
                    .iter()
                    .find(|e| e.dst == *path)
                    .expect("a listed file");
                ok(&format!("{}  {path}\n", e.sha256.expect("a file")))
            }
            ["test", "-e", path] => {
                if self.present.contains(*path) {
                    ok("")
                } else {
                    fail("")
                }
            }
            _ => ok(""),
        }
    }

    fn stat(&self, path: &str) -> Out {
        if let Some(e) = self.entries.iter().find(|e| e.dst == path) {
            if !self.present.contains(path) {
                return fail("stat: cannot statx: No such file or directory");
            }
            let kind = if e.sha256.is_some() {
                "regular file"
            } else {
                "directory"
            };
            let (owner, mode) = if self.is(Defect::WrongOwnerOrMode) && path.ends_with("/gh") {
                ("kbf", 0o644)
            } else {
                ("root", e.mode)
            };
            return ok(&format!("{owner}:root:{mode:o}:{kind}\n"));
        }
        match path {
            "/run/kbf-installer" => ok("root:root:755:directory\n"),
            "/var/cache/kbf-fetch" if self.is(Defect::TmpfilesDirMissing) => {
                fail("stat: cannot statx: No such file or directory")
            }
            "/var/cache/kbf-fetch" => ok("root:root:711:directory\n"),
            _ => fail("stat: no such path"),
        }
    }
}

/// A scratch directory holding the listing.
fn listing_dir(name: &str) -> (TestDir, PathBuf) {
    let beside = std::env::current_exe().expect("test binary");
    let dir = TestDir::under(
        beside
            .parent()
            .expect("deps")
            .join("xtask-pkg-upgrade-unit"),
        name,
    );
    let listing = dir.join("listing.txt");
    std::fs::write(&listing, LISTING).expect("listing");
    (dir, listing)
}

/// The verb run against a model with `defect`: the result, and the model.
fn upgrade(name: &str, defect: Defect) -> (Result<Vec<String>, Error>, World) {
    let (_dir, listing) = listing_dir(name);
    let entries = pkg_linux::parse_listing(LISTING).expect("listing parses");
    let world = std::cell::RefCell::new(World::new(defect, entries));
    let result = run(
        &mut |args| Ok(world.borrow_mut().docker(args)),
        &UpgradeTest {
            old_installer: Path::new(OLD_INSTALLER),
            old_kbf: Path::new(OLD_KBF),
            package: Path::new(NEW_KBF),
            listing: &listing,
            image_dir: Path::new("/p/image"),
            container: "c1",
        },
    );
    (result, world.into_inner())
}

fn refused(result: Result<Vec<String>, Error>) -> Vec<String> {
    match result {
        Err(Error::Refused(problems)) => problems,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Asserts that the upgrade is made three ways, in a container of its own each (apt and a plain
/// dpkg over the old installer and kbf, a plain dpkg over the old kbf alone); that in each the old
/// debs are installed first and the new kbf over them, with no force flag, then the old installer
/// purged where there is one; and that a host where every step holds is accepted with one line
/// saying what was shown.
#[test]
fn a_host_that_hands_its_files_to_the_new_kbf_and_keeps_its_users_passes() {
    let (result, world) = upgrade("good", Defect::None);
    let said = result.expect("accepted");
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(
        said[0].contains("takes over from kbf-installer"),
        "{said:?}"
    );
    let steps: Vec<&String> = world
        .log
        .iter()
        .filter(|l| {
            l.contains("apt-get") || l.contains("dpkg --install") || l.contains("dpkg --purge")
        })
        .collect();
    let apt = |deb: &str| format!("apt-get -y --allow-downgrades install /var/tmp/{deb}");
    let want = [
        ("c1-apt", apt("kbf-installer-old.deb")),
        ("c1-apt", apt("kbf-old.deb")),
        ("c1-apt", apt("kbf-new.deb")),
        ("c1-apt", "dpkg --purge kbf-installer".to_owned()),
        ("c1-dpkg", apt("kbf-installer-old.deb")),
        ("c1-dpkg", apt("kbf-old.deb")),
        ("c1-dpkg", "dpkg --install /var/tmp/kbf-new.deb".to_owned()),
        ("c1-dpkg", "dpkg --purge kbf-installer".to_owned()),
        ("c1-dpkg-without-installer", apt("kbf-old.deb")),
        (
            "c1-dpkg-without-installer",
            "dpkg --install /var/tmp/kbf-new.deb".to_owned(),
        ),
    ];
    assert_eq!(steps.len(), want.len(), "{steps:?}");
    for (step, (container, command)) in steps.iter().zip(want) {
        assert_eq!(**step, format!("exec {container} {command}"));
    }
    // The debs are copied into each container before anything runs in it: all three where the
    // host has the old installer, and two where it has not.
    for (container, debs) in [
        ("c1-apt", 3),
        ("c1-dpkg", 3),
        ("c1-dpkg-without-installer", 2),
    ] {
        let first_exec = world
            .log
            .iter()
            .position(|l| l.starts_with(&format!("exec {container} ")))
            .expect("exec");
        let copies = world.log[..first_exec]
            .iter()
            .filter(|l| l.starts_with("cp ") && l.contains(&format!(" {container}:")))
            .count();
        assert_eq!(copies, debs, "{container}: {:?}", world.log);
        // Each container and its image are removed after its run.
        assert!(
            world.log.contains(&format!("rm -f {container}")),
            "{:?}",
            world.log
        );
    }
}

/// Asserts that the files the new package lists staying the old installer's after the upgrade
/// is refused, once per file, naming the owner it found: dpkg would not have unpacked them
/// without a `Replaces`, and a host would be left with two packages claiming one file.
#[test]
fn files_that_stay_with_the_old_installer_are_refused() {
    let (result, _) = upgrade("stay", Defect::FilesStayWithInstaller);
    let problems = refused(result);
    assert!(
        problems.iter().any(|p| p
            == "apt: the owner of /usr/lib/systemd/system/kbf.slice: \"kbf-installer\", not \"kbf\""),
        "{problems:?}"
    );
    // Seen by dpkg too, where `Replaces` is what lets the files be unpacked at all.
    assert!(
        problems.iter().any(|p| p
            == "dpkg: the owner of /usr/lib/systemd/system/kbf.slice: \"kbf-installer\", not \"kbf\""),
        "{problems:?}"
    );
}

/// Asserts that a new deb that `Breaks` the installer is refused where a plain `dpkg --install`
/// meets it (a host that has the installer, which is where `--migrate-to-package` starts), and
/// that apt, which removes the installer instead, is not the leg that says so.
#[test]
fn a_deb_that_breaks_the_installer_is_refused_by_a_plain_dpkg() {
    let (result, _) = upgrade("breaks", Defect::BreaksTheInstaller);
    let problems = refused(result);
    assert!(
        problems.iter().any(|p| p.starts_with(
            "dpkg: installing the new kbf failed: dpkg: regarding /var/tmp/kbf-new.deb containing kbf"
        )),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .all(|p| !p.starts_with("apt: installing the new kbf failed")),
        "{problems:?}"
    );
}

/// Asserts that a new deb that `Depends` on `crun` is refused on the host that has only the old
/// `kbf` deb (and so no `crun`), where a plain `dpkg --install` leaves the package unpacked and
/// unconfigured, and that the host that has the installer's dependencies is not the leg that
/// says so.
#[test]
fn a_deb_that_depends_on_crun_is_refused_where_the_host_lacks_it() {
    let (result, _) = upgrade("depends", Defect::DependsOnCrun);
    let problems = refused(result);
    assert!(
        problems.iter().any(|p| p.starts_with(
            "dpkg-without-installer: installing the new kbf failed: dpkg: error processing package kbf"
        )),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("dpkg-without-installer: kbf is \"install ok unpacked\"")),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("dpkg-without-installer: dpkg --audit after the upgrade: ")),
        "{problems:?}"
    );
}

/// Asserts that the host that is meant to lack `crun` and `uidmap` having `crun` is refused before
/// the upgrade: the leg would show nothing.
#[test]
fn a_host_that_already_has_crun_cannot_show_the_package_needs_nothing() {
    let (result, world) = upgrade("crun-there", Defect::CrunPreinstalled);
    let problems = refused(result);
    assert!(
        problems.iter().any(|p| p
            == "dpkg-without-installer: crun is installed before the upgrade, so the test cannot show the new package needs nothing"),
        "{problems:?}"
    );
    let _ = world;
}

/// Asserts that purging the old installer's record taking a file the new kbf owns is refused,
/// naming the file: that is the hand-over failing, seen from the removal.
#[test]
fn a_purge_of_the_installer_that_takes_a_kbf_file_is_refused() {
    let (result, _) = upgrade("purge", Defect::PurgeTakesKbfFiles);
    let problems = refused(result);
    assert!(
        problems
            .iter()
            .any(|p| p == "apt: /usr/share/kbf/trust/gh is gone after kbf-installer is purged"),
        "{problems:?}"
    );
}

/// Asserts that users with other ids after the upgrade are refused: a host's files would then
/// be owned by someone else.
#[test]
fn users_that_get_new_ids_are_refused() {
    let (result, _) = upgrade("users", Defect::UsersRenumbered);
    let problems = refused(result);
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("apt: the users and their ids: ")),
        "{problems:?}"
    );
}

/// Asserts that a directory `tmpfiles.d` is to make that it does not, an installed kbf of the
/// old version, an owner or mode other than the listing's, and a failing install are each
/// reported.
#[test]
fn a_missing_tmpfiles_directory_a_stale_version_a_wrong_mode_and_a_failed_install_are_refused() {
    let (result, _) = upgrade("tmpfiles", Defect::TmpfilesDirMissing);
    let problems = refused(result);
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("apt: /var/cache/kbf-fetch: ")),
        "{problems:?}"
    );
    let (result, _) = upgrade("stale", Defect::VersionStale);
    let problems = refused(result);
    assert!(
        problems
            .iter()
            .any(|p| p
                == "apt: the version of kbf after the upgrade: \"0.1.0+old\", not \"0.1.0+new\""),
        "{problems:?}"
    );
    let (result, _) = upgrade("mode", Defect::WrongOwnerOrMode);
    let problems = refused(result);
    assert!(
        problems
            .iter()
            .any(|p| p == "apt: /usr/share/kbf/trust/gh: \"kbf:root:644:regular file\", not \"root:root:755:regular file\""),
        "{problems:?}"
    );
    let (result, _) = upgrade("install-fails", Defect::InstallOfNewFails);
    let problems = refused(result);
    assert!(
        problems
            .iter()
            .any(|p| p
                .starts_with("apt: installing the new kbf failed: E: Unable to correct problems")),
        "{problems:?}"
    );
}

/// Asserts that when the old installer does not own the trust directory and the slice, the
/// test says so and goes no further: it would pass for any package, having nothing to hand
/// over, so it must not install the new one.
#[test]
fn a_starting_point_with_nothing_to_hand_over_is_refused_before_the_upgrade() {
    let (result, world) = upgrade("nothing", Defect::OldInstallerOwnsNoTrust);
    let problems = refused(result);
    assert!(
        problems.iter().any(|p| p
            .starts_with("apt: the owner of /usr/lib/systemd/system/kbf.slice before the upgrade")),
        "{problems:?}"
    );
    assert!(
        !world
            .log
            .iter()
            .any(|l| l.contains("kbf-new.deb") && l.contains("apt-get")),
        "{:?}",
        world.log
    );
}

/// Asserts that a docker build that fails is an error of the job, not a finding about the
/// package, and that the container and the image are still removed.
#[test]
fn a_build_that_fails_is_an_error_and_cleans_up() {
    let (result, world) = upgrade("build", Defect::BuildFails);
    let err = result.expect_err("build fails");
    assert!(!matches!(err, Error::Refused(_)), "{err:?}");
    assert!(
        world.log.iter().any(|l| l == "rm -f c1-apt"),
        "{:?}",
        world.log
    );
}

/// Asserts that a listing that cannot be read is an error of the job.
#[test]
fn a_listing_that_cannot_be_read_is_an_error() {
    let world = std::cell::RefCell::new(World::new(Defect::None, Vec::new()));
    let err = run(
        &mut |args| Ok(world.borrow_mut().docker(args)),
        &UpgradeTest {
            old_installer: Path::new(OLD_INSTALLER),
            old_kbf: Path::new(OLD_KBF),
            package: Path::new(NEW_KBF),
            listing: Path::new("/nonexistent/listing.txt"),
            image_dir: Path::new("/p/image"),
            container: "c1",
        },
    )
    .expect_err("no listing");
    assert!(!matches!(err, Error::Refused(_)), "{err:?}");
    assert!(world.borrow().log.is_empty());
}
