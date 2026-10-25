//! `pkg-upgrade-test --old-installer <deb> --old-kbf <deb> --package <deb> --listing <file>
//! --image-dir <dir> [--docker <program>]`: a host that has the `kbf` deb of the release before
//! (and, but for one leg, its `kbf-installer` deb) takes the `kbf` deb of this one, in a fresh
//! container with systemd as PID 1 and the package managers that install it (apt and dpkg, not a
//! model of them).
//!
//! The old debs are published assets of an earlier release, so the files they own are the
//! files a real host has. The upgrade is made three ways, each in a container of its own:
//!
//! - **apt** (what an operator does with the file): `apt-get install` of the new deb on a host
//!   with both old debs. There is no `Breaks`, so apt leaves the installer installed;
//! - **dpkg**: a plain `dpkg --install` of the new deb (no force flag, no resolution: how
//!   `kbf-host-install` updates a native-package host and `--migrate-to-package` moves a
//!   hand-placed one) on a host with both old debs. Nothing but `Replaces` lets dpkg unpack the
//!   new files over the ones the installer owns;
//! - **dpkg without the installer**: the same on a host that has only the old `kbf` deb, which
//!   had no dependencies, so it has neither `crun` nor `uidmap`: the new deb must configure
//!   without them (a `Depends` on them would leave it unpacked, unconfigured and undone).
//!
//! In each container:
//!
//! 1. the old debs are installed, and the checks that make the rest mean something hold: with the
//!    installer, it owns the trust directory and `kbf.slice` (so the new package has files to
//!    take over) and, after `systemd-sysusers` and `systemd-tmpfiles --create`, the users exist
//!    (so there are users to keep); without it, `crun` and `uidmap` are not installed;
//! 2. the new `kbf` is installed over them: afterwards `kbf` is `install ok installed` at the new
//!    version, `dpkg --audit` is clean, every entry of `--listing` is at its path, `root:root`,
//!    with its mode and bytes, and dpkg says `kbf` owns each file of it (the trust files, the
//!    sysusers and tmpfiles files, the slice, the needrestart drop-in), not `kbf-installer`;
//! 3. with the installer: it is still installed (its binary is there), and purging it deletes
//!    none of the entries: removing `kbf-installer` takes nothing `kbf` owns;
//! 4. the users have the ids they had (or, without the installer, exist now that the package's
//!    sysusers file has been read), and the directories `tmpfiles.d` makes exist.
//!
//! Every command is one `docker` argv run through a closure ([`Docker`]), as in
//! [`crate::pkg_install`], so the checks are tested against a model of the container.

use std::path::Path;

use crate::Error;
use crate::package::read;
use crate::pkg_install::{Container, Docker, entry, eq, inside, must, up};
use crate::pkg_linux::{self, Entry, TMPFILES_DIRS, USERS};

/// What `pkg-upgrade-test` is given.
pub struct UpgradeTest<'a> {
    /// The `kbf-installer` deb of the release before.
    pub old_installer: &'a Path,
    /// The `kbf` deb of the release before.
    pub old_kbf: &'a Path,
    /// The `kbf` deb of this release.
    pub package: &'a Path,
    /// The `listing.txt` of `pkg-config` for `package`.
    pub listing: &'a Path,
    pub image_dir: &'a Path,
    /// A name no other container of the host has; each container adds its leg's name.
    pub container: &'a str,
}

/// How the new deb is installed, and over what.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// `apt-get install`, on a host with both old debs.
    Apt,
    /// A plain `dpkg --install`, on a host with both old debs.
    Dpkg,
    /// A plain `dpkg --install`, on a host with only the old `kbf` deb.
    DpkgWithoutInstaller,
}

impl Via {
    /// All of them, in the order they run.
    pub const ALL: [Via; 3] = [Via::Apt, Via::Dpkg, Via::DpkgWithoutInstaller];

    /// The word in the container's name and in the problems.
    pub fn name(self) -> &'static str {
        match self {
            Via::Apt => "apt",
            Via::Dpkg => "dpkg",
            Via::DpkgWithoutInstaller => "dpkg-without-installer",
        }
    }

    /// Whether the host has the old `kbf-installer` deb.
    fn has_installer(self) -> bool {
        self != Via::DpkgWithoutInstaller
    }

    /// The command that installs `deb` (a path in the container) over the old packages.
    fn install(self, deb: &str) -> Vec<&str> {
        match self {
            Via::Apt => vec!["apt-get", "-y", "--allow-downgrades", "install", deb],
            Via::Dpkg | Via::DpkgWithoutInstaller => vec!["dpkg", "--install", deb],
        }
    }
}

/// The files the old installer deb owns and the new `kbf` deb takes: what must change hands.
const HANDED_OVER: [&str; 2] = [
    "/usr/share/kbf-installer/trust/gh",
    "/usr/lib/systemd/system/kbf.slice",
];

/// `dpkg -S <path>`: the package that owns `path`, or what dpkg said instead.
fn owner(c: &mut Container, path: &str) -> Result<String, Error> {
    let said = c.say(&["dpkg", "-S", path])?;
    Ok(said.split(':').next().unwrap_or("").trim().to_owned())
}

/// The numeric id of each of [`USERS`], and the group the signer shares, as `name=id` lines.
fn ids(c: &mut Container) -> Result<String, Error> {
    let mut lines = Vec::new();
    for user in USERS {
        lines.push(format!("{user}={}", c.say(&["id", "-u", user])?));
    }
    lines.push(format!(
        "kbf-signer-clients={}",
        c.say(&["getent", "group", "kbf-signer-clients"])?
    ));
    Ok(lines.join("\n"))
}

/// The old `deb`, copied into the container, installed with apt. A downgrade is allowed: the
/// versions are `0.1.0+<commit>`, which apt orders by the commit as text, and the test is of the
/// files, not of the version order.
fn install_old(
    c: &mut Container,
    what: &str,
    deb: &Path,
    problems: &mut Vec<String>,
) -> Result<(), Error> {
    let deb = inside(deb);
    c.must(what, &Via::Apt.install(&deb), problems)
}

/// Whether the package `name` is installed.
fn installed(c: &mut Container, name: &str) -> Result<bool, Error> {
    let status = c.say(&["dpkg-query", "-W", "-f=${Status}", name])?;
    Ok(status == "install ok installed")
}

/// The checks after the container is up and the debs are in it.
fn checks(
    c: &mut Container,
    t: &UpgradeTest,
    entries: &[Entry],
    via: Via,
) -> Result<Vec<String>, Error> {
    let mut problems = Vec::new();
    let with_installer = via.has_installer();
    if with_installer {
        install_old(
            c,
            "installing the old kbf-installer",
            t.old_installer,
            &mut problems,
        )?;
    }
    install_old(c, "installing the old kbf", t.old_kbf, &mut problems)?;
    let mut before_names = vec![pkg_linux::KBF_NAME];
    if with_installer {
        before_names.push(pkg_linux::NAME);
    }
    for name in before_names {
        if !installed(c, name)? {
            problems.push(format!("{name} is not installed before the upgrade"));
        }
    }
    let before = if with_installer {
        for path in HANDED_OVER {
            eq(
                &mut problems,
                &format!("the owner of {path} before the upgrade"),
                &owner(c, path)?,
                pkg_linux::NAME,
            );
        }
        c.must("systemd-sysusers", &["systemd-sysusers"], &mut problems)?;
        c.must(
            "systemd-tmpfiles",
            &["systemd-tmpfiles", "--create"],
            &mut problems,
        )?;
        Some(ids(c)?)
    } else {
        // The host the old `kbf` deb alone made: it had no dependency, so `crun` and `uidmap`
        // are not there for a dependency of the new one to be met by.
        for name in ["crun", "uidmap"] {
            if installed(c, name)? {
                problems.push(format!(
                    "{name} is installed before the upgrade, so the test cannot show the new package needs nothing"
                ));
            }
        }
        None
    };
    if !problems.is_empty() {
        // The rest would not mean what it says.
        return Ok(problems);
    }

    let new = inside(t.package);
    c.must("installing the new kbf", &via.install(&new), &mut problems)?;
    if !installed(c, pkg_linux::KBF_NAME)? {
        let status = c.say(&["dpkg-query", "-W", "-f=${Status}", pkg_linux::KBF_NAME])?;
        problems.push(format!(
            "kbf is {status:?} after the upgrade, not \"install ok installed\""
        ));
    }
    eq(
        &mut problems,
        "dpkg --audit after the upgrade",
        &c.say(&["dpkg", "--audit"])?,
        "",
    );
    let want = c.say(&["dpkg-deb", "-f", &new, "Version"])?;
    let got = c.say(&["dpkg-query", "-W", "-f=${Version}", pkg_linux::KBF_NAME])?;
    eq(
        &mut problems,
        "the version of kbf after the upgrade",
        &got,
        &want,
    );
    for e in entries {
        entry(c, e, &mut problems)?;
        if e.sha256.is_some() {
            eq(
                &mut problems,
                &format!("the owner of {}", e.dst),
                &owner(c, &e.dst)?,
                pkg_linux::KBF_NAME,
            );
        }
    }
    if with_installer {
        // Nothing removed the installer: it stays, with the rest of its files, until purged.
        if !installed(c, pkg_linux::NAME)? {
            problems.push(
                "kbf-installer is gone after the upgrade, though nothing removed it".to_owned(),
            );
        }
        if !c.ok(&["test", "-e", pkg_linux::BINARY_PATH])? {
            problems.push(format!(
                "{} is gone after the upgrade",
                pkg_linux::BINARY_PATH
            ));
        }
        c.must(
            "purging the old kbf-installer",
            &["dpkg", "--purge", pkg_linux::NAME],
            &mut problems,
        )?;
        for e in entries {
            if !c.ok(&["test", "-e", &e.dst])? {
                problems.push(format!("{} is gone after kbf-installer is purged", e.dst));
            }
        }
        if c.ok(&["test", "-e", pkg_linux::BINARY_PATH])? {
            problems.push(format!(
                "{} is still there after kbf-installer is purged",
                pkg_linux::BINARY_PATH
            ));
        }
        eq(
            &mut problems,
            "dpkg --audit after the purge",
            &c.say(&["dpkg", "--audit"])?,
            "",
        );
    }

    c.must("systemd-sysusers", &["systemd-sysusers"], &mut problems)?;
    c.must(
        "systemd-tmpfiles",
        &["systemd-tmpfiles", "--create"],
        &mut problems,
    )?;
    let after = ids(c)?;
    match before {
        Some(before) => eq(&mut problems, "the users and their ids", &after, &before),
        None => {
            for user in USERS {
                if !c.ok(&["getent", "passwd", user])? {
                    problems.push(format!("the user {user} does not exist after the upgrade"));
                }
            }
        }
    }
    for (path, mode) in TMPFILES_DIRS {
        let got = c.say(&["stat", "-c", "%U:%G:%a:%F", path])?;
        eq(
            &mut problems,
            path,
            &got,
            &format!("root:root:{mode}:directory"),
        );
    }
    Ok(problems)
}

/// The container up, the debs in it, the checks run.
fn started(
    docker: &mut Docker,
    t: &UpgradeTest,
    (container, image): (&str, &str),
    via: Via,
) -> Result<(), Error> {
    up(docker, t.image_dir, image, container)?;
    let installer = via.has_installer().then_some(t.old_installer);
    for deb in installer.into_iter().chain([t.old_kbf, t.package]) {
        let target = format!("{container}:{}", inside(deb));
        must(docker, &["cp", &deb.to_string_lossy(), &target])?;
    }
    Ok(())
}

/// One container, the upgrade made `via`, and the container removed whatever the result.
fn session(
    docker: &mut Docker,
    t: &UpgradeTest,
    entries: &[Entry],
    via: Via,
) -> Result<Vec<String>, Error> {
    let container = format!("{}-{}", t.container, via.name());
    let image = format!("{}-upgrade-test-{container}", pkg_linux::KBF_NAME);
    let result = match started(docker, t, (&container, &image), via) {
        Ok(()) => {
            Container::booted(docker, &container).and_then(|mut c| checks(&mut c, t, entries, via))
        }
        Err(e) => Err(e),
    };
    let _ = docker(&["rm", "-f", &container]);
    let _ = docker(&["rmi", &image]);
    result
}

/// The `pkg-upgrade-test` verb: the lines to print.
///
/// # Errors
/// The listing cannot be read, docker fails, or a check finds a problem.
pub fn run(docker: &mut Docker, t: &UpgradeTest) -> Result<Vec<String>, Error> {
    let text = String::from_utf8_lossy(&read(t.listing)?).into_owned();
    let entries = pkg_linux::parse_listing(&text)?;
    let mut problems = Vec::new();
    for via in Via::ALL {
        let found = session(docker, t, &entries, via)?;
        problems.extend(found.into_iter().map(|p| format!("{}: {p}", via.name())));
    }
    if problems.is_empty() {
        return Ok(vec![format!(
            "{} takes over from kbf-installer and from the old kbf alone, by apt and by a plain dpkg --install: it configures with nothing else installed, its files change hands, the users keep their ids, and purging the installer takes none of them",
            t.package.display()
        )]);
    }
    Err(Error::Refused(problems))
}

#[cfg(test)]
mod tests;
