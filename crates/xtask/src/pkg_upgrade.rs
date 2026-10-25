//! `pkg-upgrade-test --old-installer <deb> --old-kbf <deb> --package <deb> --listing <file>
//! --image-dir <dir> [--docker <program>]`: a host that has the `kbf-installer` deb and the
//! `kbf` deb of the release before takes the `kbf` deb of this one, in a fresh container with
//! systemd as PID 1 and the package managers that install it (apt and dpkg, not a model of
//! them).
//!
//! The old debs are published assets of an earlier release, so the files they own are the
//! files a real host has. The upgrade is made two ways, each in a container of its own:
//!
//! - **apt** (what an operator does): `apt-get install` of the new deb. It `Breaks` the old
//!   installer, so apt removes it; afterwards the old installer is not installed and its
//!   binary and old trust directory are gone;
//! - **dpkg** (what the `Replaces` is for): `dpkg --install --force-breaks` of the new deb, with
//!   the old installer still installed, so that dpkg unpacks the new files over the ones the
//!   installer owns, which nothing but `Replaces` allows.
//!
//! In each container:
//!
//! 1. the old `kbf-installer` and the old `kbf` are installed, and the checks that make the
//!    rest mean something hold: the installer owns the trust directory and `kbf.slice`
//!    (so the new package has files to take over), and after `systemd-sysusers` and
//!    `systemd-tmpfiles --create` the users exist (so there are users to keep);
//! 2. the new `kbf` is installed over them: afterwards `kbf` is at the new version, every entry
//!    of `--listing` is at its path, `root:root`, with its mode and bytes, and dpkg says `kbf`
//!    owns each file of it (the trust files, the sysusers and tmpfiles files, the slice, the
//!    needrestart drop-in), not `kbf-installer`;
//! 3. purging the old installer deletes none of the entries: removing `kbf-installer` takes
//!    nothing `kbf` owns, and nothing is left half-installed (`dpkg --audit`);
//! 4. the users have the ids they had, and the directories `tmpfiles.d` makes exist.
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
    /// A name no other container of the host has; the two containers add `-apt` and `-dpkg`.
    pub container: &'a str,
}

/// How the new deb is installed over the old packages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// `apt-get install`: the package manager resolves the `Breaks` by removing the old
    /// installer.
    Apt,
    /// `dpkg --install --force-breaks`: the old installer stays, and `Replaces` is what lets
    /// the new files be unpacked over its own.
    Dpkg,
}

impl Via {
    /// Both, in the order they run.
    pub const ALL: [Via; 2] = [Via::Apt, Via::Dpkg];

    /// The word in the container's name and in the problems.
    pub fn name(self) -> &'static str {
        match self {
            Via::Apt => "apt",
            Via::Dpkg => "dpkg",
        }
    }

    /// The command that installs `deb` (a path in the container) over the old packages.
    fn install(self, deb: &str) -> Vec<&str> {
        match self {
            Via::Apt => vec!["apt-get", "-y", "--allow-downgrades", "install", deb],
            Via::Dpkg => vec!["dpkg", "--install", "--force-breaks", deb],
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

/// The checks after the container is up and the three debs are in it.
fn checks(
    c: &mut Container,
    t: &UpgradeTest,
    entries: &[Entry],
    via: Via,
) -> Result<Vec<String>, Error> {
    let mut problems = Vec::new();
    install_old(
        c,
        "installing the old kbf-installer",
        t.old_installer,
        &mut problems,
    )?;
    install_old(c, "installing the old kbf", t.old_kbf, &mut problems)?;
    for name in [pkg_linux::NAME, pkg_linux::KBF_NAME] {
        let status = c.say(&["dpkg-query", "-W", "-f=${Status}", name])?;
        eq(
            &mut problems,
            &format!("{name} before the upgrade"),
            &status,
            "install ok installed",
        );
    }
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
    let before = ids(c)?;
    if !problems.is_empty() {
        // The rest would not mean what it says.
        return Ok(problems);
    }

    let new = inside(t.package);
    c.must("installing the new kbf", &via.install(&new), &mut problems)?;
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
    if via == Via::Apt {
        // The package manager resolved the `Breaks`: the old installer is gone, and its own files.
        let status = c.say(&["dpkg-query", "-W", "-f=${Status}", pkg_linux::NAME])?;
        if status == "install ok installed" {
            problems.push("kbf-installer is still installed beside the new kbf".to_owned());
        }
        for path in [pkg_linux::BINARY_PATH, "/usr/share/kbf-installer/trust/gh"] {
            if c.ok(&["test", "-e", path])? {
                problems.push(format!("{path} is still there after the upgrade"));
            }
        }
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
    let audit = c.say(&["dpkg", "--audit"])?;
    eq(&mut problems, "dpkg --audit", &audit, "");

    c.must("systemd-sysusers", &["systemd-sysusers"], &mut problems)?;
    c.must(
        "systemd-tmpfiles",
        &["systemd-tmpfiles", "--create"],
        &mut problems,
    )?;
    eq(&mut problems, "the users and their ids", &ids(c)?, &before);
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
) -> Result<(), Error> {
    up(docker, t.image_dir, image, container)?;
    for deb in [t.old_installer, t.old_kbf, t.package] {
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
    let result = match started(docker, t, (&container, &image)) {
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
            "{} takes over from kbf-installer by apt and by dpkg: its files change hands, the users keep their ids, and removing the installer takes none of them",
            t.package.display()
        )]);
    }
    Err(Error::Refused(problems))
}

#[cfg(test)]
mod tests;
