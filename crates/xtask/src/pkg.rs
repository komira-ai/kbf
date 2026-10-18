//! `pkg build` and `pkg cask`: kbf-installer for a Mac (contract section 7, P11).
//!
//! `pkg build --binary <path> --launchd-plist <path> [--trust <path>]... --version
//! <version> --out <dir>` writes `<dir>/kbf-installer-<version>-macos-arm64.pkg`, a flat
//! component package of exactly these entries, all root:wheel:
//!
//! - `/usr/local/kbf-installer/bin/kbf-installer` (0755): `--binary`, signed ad hoc by
//!   CI. The tree `/usr/local/kbf-installer` is root-owned and closed to group and
//!   other writes (0755 all the way down): `kbf-installer check` refuses to run from a
//!   user-owned prefix, which is why there is no formula (a Homebrew prefix is the
//!   user's) and why the tool is not under `/usr/local/bin`;
//! - `/usr/local/kbf-installer/trust/<name>` (0644) for each `--trust` file: the
//!   release's pinned `gh`, trusted root and `installer.conf`;
//! - `/Library/LaunchDaemons/com.komira.kbf-installer.plist` (0644): `--launchd-plist`;
//! - `/var/db/kbf-installer` (0700), the installer's state directory, empty;
//! - `/Library/Logs/kbf-installer` (0755), where the job writes.
//!
//! The package has no scripts and ships no kbf release: `kbf-installer setup` fetches,
//! verifies and installs the first one under `/usr/local/kbf-installer/releases`, and
//! writes the configuration and creates the users. Installing the package places files
//! and nothing else: no job is loaded, no process starts. The job is gated twice:
//! Installer does not load a LaunchDaemon, and the property list, which [`gate_problems`]
//! refuses the package without, keeps the job from running until the file [`GATE`]
//! exists: `RunAtLoad` false, `KeepAlive` on that path and no other way to start. launchd
//! has no `ConditionPathExists`; the `PathState` keep-alive is its form of one.
//!
//! The package is written here, not by `pkgbuild`/`productbuild` (nfpm, which builds the
//! Linux packages, does not write a `.pkg`), because `pkgbuild` is not deterministic:
//! it stamps the archive and the bill of materials with the time and the host's owners,
//! and this repository's builders give the same bytes for the same inputs (a test builds
//! twice, from different files and directories). The pieces are a xar archive ([`xar`])
//! of `Bom` ([`bom`]), `Payload` ([`cpio`]) and `PackageInfo`. They are checked here by
//! reading them back with a second implementation (`tests/read`) and by Apple's tools:
//! CI's macOS job expands the package with `pkgutil`, lists its bill with `lsbom` and
//! installs it (`crates/xtask/tests/pkg_macos.rs`). Signing the package
//! (`productsign`) wraps this file without changing it and waits on the signing
//! identity (contract decision 7).
//!
//! `pkg cask --pkg <path> --version <version> --url <url> --out <file>` ([`brew`])
//! writes the Homebrew cask that installs the package.

use std::path::{Path, PathBuf};

use crate::Error;
use crate::fetch::Sha256;
use crate::package::{read, write_new};

pub mod bom;
pub mod brew;
pub mod cpio;
pub mod sha1;
pub mod tree;
pub mod xar;

use tree::Tree;

/// The package's identifier and the job's label: what `pkgutil --forget` and `launchctl
/// bootout` take.
pub const IDENTIFIER: &str = "com.komira.kbf-installer";
/// Where the files go, relative to `/`.
pub const PREFIX: &str = "usr/local/kbf-installer";
pub const BINARY: &str = "usr/local/kbf-installer/bin/kbf-installer";
pub const TRUST_DIR: &str = "usr/local/kbf-installer/trust";
pub const PLIST: &str = "Library/LaunchDaemons/com.komira.kbf-installer.plist";
/// The state directory, by its real path: `/var` is a link to `/private/var`.
pub const STATE_DIR: &str = "private/var/db/kbf-installer";
pub const LOG_DIR: &str = "Library/Logs/kbf-installer";
/// The mode of [`STATE_DIR`].
pub const STATE_MODE: u32 = 0o700;
/// The installer's configuration, which `kbf-installer setup` writes and whose existence
/// opens the gate.
pub const GATE: &str = "/usr/local/etc/kbf/installer.toml";
/// What the property list runs: the installed tool in its service mode.
pub const PROGRAM: &str = "/usr/local/kbf-installer/bin/kbf-installer";

/// The package's file name for `version`.
pub fn pkg_name(version: &str) -> String {
    format!("kbf-installer-{version}-macos-arm64.pkg")
}

/// Whether `version` is a name a package, a Ruby string and an XML attribute can hold
/// as it is.
pub fn valid_version(version: &str) -> bool {
    // `&` and `|`: see `brew::valid_url`.
    (!version.is_empty())
        & (version.len() <= 64)
        & version.starts_with(|c: char| c.is_ascii_alphanumeric())
        & version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() | "._+-".contains(c))
}

/// Keys that start a job without a keep-alive condition: a property list with one is
/// not gated by [`GATE`].
const TRIGGERS: [&str; 7] = [
    "Sockets",
    "StartInterval",
    "StartCalendarInterval",
    "StartOnMount",
    "WatchPaths",
    "QueueDirectories",
    "LaunchEvents",
];

/// What is wrong with the LaunchDaemon property list `plist` for this package: one line
/// per problem, none when it is the job [`IDENTIFIER`] that is not started at load, is
/// kept alive only while [`GATE`] exists, and has no other way to start.
pub fn gate_problems(plist: &str) -> Vec<String> {
    let flat: String = plist.chars().filter(|c| !c.is_whitespace()).collect();
    let mut problems = Vec::new();
    let required = [
        (
            "runs the installed tool in its service mode",
            format!(
                "<key>ProgramArguments</key><array><string>{PROGRAM}</string>\
                 <string>serve</string></array>"
            ),
        ),
        (
            "is labelled",
            format!("<key>Label</key><string>{IDENTIFIER}</string>"),
        ),
        (
            "is not started at load",
            "<key>RunAtLoad</key><false/>".to_owned(),
        ),
        (
            "is kept alive only while the gate file exists",
            format!(
                "<key>KeepAlive</key><dict><key>PathState</key><dict><key>{GATE}</key><true/>\
                 </dict></dict>"
            ),
        ),
    ];
    for (what, xml) in required {
        if !flat.contains(&xml) {
            problems.push(format!(
                "the property list does not say the job {what}: {xml}"
            ));
        }
    }
    for key in TRIGGERS {
        if flat.contains(&format!("<key>{key}</key>")) {
            problems.push(format!(
                "the property list has {key}, which starts the job without {GATE}"
            ));
        }
    }
    problems
}

/// The `PackageInfo` of a package of `tree`.
fn package_info(tree: &Tree, version: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <pkg-info format-version=\"2\" identifier=\"{IDENTIFIER}\" version=\"{version}\" \
         install-location=\"/\" auth=\"root\">\n\
         <payload numberOfFiles=\"{}\" installKBytes=\"{}\"/>\n\
         </pkg-info>\n",
        tree.count(),
        tree.file_bytes().div_ceil(1024)
    )
}

/// The package of `tree` at `version`.
///
/// # Errors
/// The bill of materials cannot hold `tree`.
pub fn package(tree: &Tree, version: &str) -> Result<Vec<u8>, String> {
    let bom = bom::bom(tree)?;
    let payload = cpio::payload(tree);
    let info = package_info(tree, version);
    Ok(xar::xar(&[
        ("Bom", &bom),
        ("Payload", &payload),
        ("PackageInfo", info.as_bytes()),
    ]))
}

/// What `pkg build` is given.
pub struct Build<'a> {
    pub binary: &'a Path,
    pub plist: &'a Path,
    pub trust: &'a [PathBuf],
    pub version: &'a str,
    pub out: &'a Path,
}

/// The tree `pkg build` packages.
fn installed(b: &Build) -> Result<Tree, Error> {
    let binary = read(b.binary)?;
    if binary.is_empty() {
        return Err(Error::Failed(format!("{} is empty", b.binary.display())));
    }
    let plist = read(b.plist)?;
    let text = std::str::from_utf8(&plist)
        .map_err(|_| Error::Failed(format!("{} is not UTF-8 text", b.plist.display())))?;
    let problems = gate_problems(text);
    if !problems.is_empty() {
        return Err(Error::Refused(
            problems
                .into_iter()
                .map(|p| format!("{}: {p}", b.plist.display()))
                .collect(),
        ));
    }
    let fail = |e: String| Error::Failed(e);
    let mut tree = Tree::rooted();
    tree.dir(LOG_DIR).map_err(fail)?;
    tree.dir_mode(STATE_DIR, STATE_MODE).map_err(fail)?;
    tree.file(BINARY, 0o755, binary).map_err(fail)?;
    tree.file(PLIST, 0o644, plist).map_err(fail)?;
    for path in b.trust {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return Err(Error::Failed(format!(
                "{} has no UTF-8 file name to install it as",
                path.display()
            )));
        };
        tree.file(&format!("{TRUST_DIR}/{name}"), 0o644, read(path)?)
            .map_err(fail)?;
    }
    Ok(tree)
}

/// The `pkg build` verb: the lines to print.
pub fn build(b: &Build) -> Result<Vec<String>, Error> {
    if !valid_version(b.version) {
        return Err(Error::Usage(format!(
            "--version {:?} is not a version (letters, digits and . _ + -)",
            b.version
        )));
    }
    let tree = installed(b)?;
    let bytes = package(&tree, b.version).map_err(Error::Failed)?;
    std::fs::create_dir_all(b.out).map_err(Error::spawn(format!("create {}", b.out.display())))?;
    let name = pkg_name(b.version);
    write_new(&b.out.join(&name), &bytes)?;
    Ok(vec![
        format!("packaged {name}:"),
        format!("{}  {name}", Sha256::of(&bytes)),
    ])
}

/// Writes `text` to the new file `out`, for the lines `pkg cask` prints.
fn write_rendered(
    out: &Path,
    what: &str,
    text: Result<String, Vec<String>>,
) -> Result<Vec<String>, Error> {
    let text = text.map_err(|p| Error::Usage(p.join("; ")))?;
    write_new(out, text.as_bytes())?;
    Ok(vec![format!("wrote the {what} {}", out.display())])
}

/// The `pkg cask` verb: the lines to print.
pub fn cask(pkg: &Path, version: &str, url: &str, out: &Path) -> Result<Vec<String>, Error> {
    let name = pkg_name(version);
    if pkg.file_name().and_then(|n| n.to_str()) != Some(&name) {
        return Err(Error::Failed(format!(
            "{} is not named {name}, the package of version {version}",
            pkg.display()
        )));
    }
    let sha256 = Sha256::of(&read(pkg)?);
    write_rendered(out, "cask", brew::cask(version, &sha256, url))
}

#[cfg(test)]
mod tests;
