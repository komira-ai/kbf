//! `pkg-config` and `pkg-build`: the `kbf-installer` Linux packages (packaging/linux/README.md).
//!
//! The packages are built by nfpm, pinned in CI by SHA-256. This module is the one place
//! that says what goes in them:
//!
//! - [`plan`] lists the files from the inputs and refuses what the contract
//!   (docs/design/kbf-installer.md, section 3) does not ship: a unit that is not
//!   `kbf-*.service` or `kbf-*.timer` (or the slice), a trust file other than the three, an empty unit directory;
//! - [`config`] renders the list as the nfpm configuration, with the dependencies as
//!   constants here (never Podman, never Envoy), no maintainer script, every file
//!   `root:root`, and no time (nfpm takes it from `SOURCE_DATE_EPOCH`);
//! - [`listing`] renders the same list as the file the install test checks the
//!   installed package against ([`parse_listing`]);
//! - [`build`] runs nfpm for the deb and the rpm, then again, and refuses a package
//!   whose second build differs in a byte.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;
use crate::fetch::Sha256;
use crate::package::{Platform, read};

/// The package's name, and the name of its binary.
pub const NAME: &str = "kbf-installer";
/// Where the binary goes.
pub const BINARY_PATH: &str = "/usr/bin/kbf-installer";
/// Where the unit files go.
pub const UNIT_DIR: &str = "/usr/lib/systemd/system";
/// The one packaged unit that is neither a `kbf-*.service` nor a `kbf-*.timer`: the slice the
/// units run in.
pub const SLICE: &str = "kbf.slice";
/// The directory the package owns, holding [`TRUST_DIR`].
pub const SHARE_DIR: &str = "/usr/share/kbf-installer";
/// Where the trust files go.
pub const TRUST_DIR: &str = "/usr/share/kbf-installer/trust";
/// Where the users file goes (`systemd-sysusers`).
pub const SYSUSERS_PATH: &str = "/usr/lib/sysusers.d/kbf-installer.conf";
/// Where the directories file goes (`systemd-tmpfiles`).
pub const TMPFILES_PATH: &str = "/usr/lib/tmpfiles.d/kbf-installer.conf";
/// The directory `setup` fills and the package never removes.
pub const STATE_DIR: &str = "/var/lib/kbf-installer";
/// The directories `tmpfiles.d` makes, with their modes (all `root:root`). The file is
/// the unit author's, packaged unchanged; `STATE_DIR` is made by `kbf-installer setup`.
pub const TMPFILES_DIRS: [(&str, &str); 2] = [
    ("/run/kbf-installer", "755"),
    ("/var/cache/kbf-fetch", "711"),
];
/// The users `sysusers.d` makes.
pub const USERS: [&str; 4] = ["kbf", "kbf-server", "kbf-fetch", "kbf-verify"];
/// The `User=` and `Group=` names a unit's text sets, as `("User" | "Group", name)`, in
/// order. Comments and empty values (which reset the setting) are skipped.
pub fn unit_accounts(text: &str) -> Vec<(&'static str, String)> {
    let mut found = Vec::new();
    for line in text.lines() {
        for key in ["User", "Group"] {
            if let Some(name) = line.strip_prefix(key).and_then(|r| r.strip_prefix('=')) {
                let name = name.trim();
                if !name.is_empty() {
                    found.push((key, name.to_owned()));
                }
            }
        }
    }
    found
}
/// The `Depends` of the deb.
pub const DEB_DEPENDS: [&str; 4] = ["crun", "uidmap", "systemd", "ca-certificates"];
/// The `Requires` of the rpm: Fedora's `uidmap` is `shadow-utils`.
pub const RPM_DEPENDS: [&str; 4] = ["crun", "shadow-utils", "systemd", "ca-certificates"];
/// The trust files and their modes: the pinned `gh` and the two data files.
pub const TRUST_FILES: [(&str, u32); 3] = [
    ("gh", 0o755),
    ("installer.conf", 0o644),
    ("trusted_root.jsonl", 0o644),
];
/// The time every file and the package carry unless `--source-date-epoch` says
/// otherwise: a constant, so the same inputs are the same bytes whoever builds them.
pub const SOURCE_DATE_EPOCH: u64 = 1_790_000_000;
/// The host name an rpm records as its build host; without it, the builder's.
const BUILD_HOST: &str = "kbf-build";

/// One thing the package installs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Where it goes.
    pub dst: String,
    /// Its mode.
    pub mode: u32,
    /// Its bytes' digest; `None` for a directory.
    pub sha256: Option<Sha256>,
    /// Where its bytes are (empty for a directory, and in a parsed [`listing`]).
    pub src: PathBuf,
}

/// What `pkg-config` is given.
pub struct Inputs<'a> {
    /// The `kbf-installer` binary.
    pub binary: &'a Path,
    /// The directory of `kbf-*.service`, `kbf-*.timer` files and the slice.
    pub units: &'a Path,
    /// The directory of the three trust files.
    pub trust: &'a Path,
    /// The sysusers.d file.
    pub sysusers: &'a Path,
    /// The tmpfiles.d file.
    pub tmpfiles: &'a Path,
    /// The platform the binary was built for.
    pub platform: Platform,
    /// The package version.
    pub version: &'a str,
}

/// The names of the regular files in `dir`, sorted, with what is wrong with `dir`.
fn files_in(dir: &Path, problems: &mut Vec<String>) -> Vec<(String, PathBuf)> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        problems.push(format!("{} cannot be read as a directory", dir.display()));
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().is_file() {
            files.push((name, entry.path()));
        } else {
            problems.push(format!("{} is not a regular file", entry.path().display()));
        }
    }
    files.sort();
    files
}

/// The file at `path` as an entry, or the reason it is not one.
fn file(dst: String, mode: u32, src: &Path, problems: &mut Vec<String>) -> Entry {
    let sha256 = match read(src) {
        Ok(bytes) => Some(Sha256::of(&bytes)),
        Err(e) => {
            problems.push(e.to_string());
            None
        }
    };
    Entry {
        dst,
        mode,
        sha256,
        src: src.to_owned(),
    }
}

/// A directory the package owns.
fn dir(dst: &str) -> Entry {
    Entry {
        dst: dst.to_owned(),
        mode: 0o755,
        sha256: None,
        src: PathBuf::new(),
    }
}

/// Whether `version` is one a deb and an rpm both take: digits first, then letters,
/// digits, `.` and `+`.
fn version_ok(version: &str) -> bool {
    version.starts_with(|c: char| c.is_ascii_digit())
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '+')
}

/// The files a package or a payload is made of: [`plan`] without the version and the
/// platform, which only the nfpm configuration needs.
pub struct Sources<'a> {
    /// The `kbf-installer` binary.
    pub binary: &'a Path,
    /// The directory of `kbf-*.service`, `kbf-*.timer` files and the slice.
    pub units: &'a Path,
    /// The directory of the three trust files.
    pub trust: &'a Path,
    /// The sysusers.d file.
    pub sysusers: &'a Path,
    /// The tmpfiles.d file.
    pub tmpfiles: &'a Path,
}

/// Everything the package installs, sorted by path.
///
/// # Errors
/// [`Error::Refused`] with every problem with the inputs.
pub fn plan(i: &Inputs) -> Result<Vec<Entry>, Error> {
    let mut problems = Vec::new();
    if !version_ok(i.version) {
        problems.push(format!(
            "version {:?} is not digits first, then letters, digits, '.' and '+'",
            i.version
        ));
    }
    if !matches!(i.platform, Platform::LinuxX86_64 | Platform::LinuxArm64) {
        problems.push(format!("{} is not a Linux platform", i.platform.name()));
    }
    let sources = Sources {
        binary: i.binary,
        units: i.units,
        trust: i.trust,
        sysusers: i.sysusers,
        tmpfiles: i.tmpfiles,
    };
    entries(&sources, problems)
}

/// Everything the package installs, sorted by path: the one list the deb, the rpm and
/// the `install.sh` payload ([`crate::installer_tarball`]) are all made from.
///
/// # Errors
/// [`Error::Refused`] with every problem with the sources.
pub fn files(i: &Sources) -> Result<Vec<Entry>, Error> {
    entries(i, Vec::new())
}

fn entries(i: &Sources, mut problems: Vec<String>) -> Result<Vec<Entry>, Error> {
    let mut entries = vec![
        file(BINARY_PATH.to_owned(), 0o755, i.binary, &mut problems),
        file(SYSUSERS_PATH.to_owned(), 0o644, i.sysusers, &mut problems),
        file(TMPFILES_PATH.to_owned(), 0o644, i.tmpfiles, &mut problems),
        dir(SHARE_DIR),
        dir(TRUST_DIR),
    ];
    let units = files_in(i.units, &mut problems);
    if units.is_empty() {
        problems.push(format!("no unit files in {}", i.units.display()));
    }
    for (name, src) in units {
        let kbf = name.starts_with("kbf-");
        if !(name == SLICE || (kbf && (name.ends_with(".service") || name.ends_with(".timer")))) {
            problems.push(format!(
                "{name}: a package unit is named kbf-*.service or kbf-*.timer (or is {SLICE})"
            ));
        }
        entries.push(file(
            format!("{UNIT_DIR}/{name}"),
            0o644,
            &src,
            &mut problems,
        ));
    }
    let mut trust = files_in(i.trust, &mut problems);
    for (name, mode) in TRUST_FILES {
        match trust.iter().position(|(n, _)| *n == name) {
            Some(at) => {
                let (_, src) = trust.remove(at);
                entries.push(file(
                    format!("{TRUST_DIR}/{name}"),
                    mode,
                    &src,
                    &mut problems,
                ));
            }
            None => problems.push(format!("trust file {name} is missing")),
        }
    }
    for (name, _) in trust {
        problems.push(format!(
            "{name}: the package ships no trust file of that name"
        ));
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    entries.sort_by(|a, b| a.dst.cmp(&b.dst));
    Ok(entries)
}

/// `s` as a YAML double-quoted scalar (JSON strings are YAML strings).
fn quoted(s: &str) -> String {
    serde_json::Value::String(s.to_owned()).to_string()
}

/// The nfpm configuration for `entries`.
pub fn config(entries: &[Entry], platform: Platform, version: &str) -> String {
    let arch = match platform {
        Platform::LinuxArm64 => "arm64",
        _ => "amd64",
    };
    let list = |names: &[&str], indent: &str| {
        names
            .iter()
            .map(|n| format!("{indent}- {}\n", quoted(n)))
            .collect::<String>()
    };
    let mut out = format!(
        "# Generated by `cargo xtask pkg-config`; do not edit.\n\
         name: {name}\n\
         arch: {arch}\n\
         platform: \"linux\"\n\
         version: {version}\n\
         version_schema: \"none\"\n\
         section: \"admin\"\n\
         priority: \"optional\"\n\
         maintainer: \"Komira <noreply@komira.com>\"\n\
         description: \"Root setup and update tool for the kbf build farm.\"\n\
         homepage: \"https://github.com/komira-ai/kbf\"\n\
         license: \"Apache-2.0\"\n\
         depends:\n{deb}\
         overrides:\n  rpm:\n    depends:\n{rpm}\
         deb:\n  compression: \"gzip\"\n\
         rpm:\n  compression: \"gzip\"\n  buildhost: {host}\n\
         contents:\n",
        name = quoted(NAME),
        arch = quoted(arch),
        version = quoted(version),
        deb = list(&DEB_DEPENDS, "  "),
        rpm = list(&RPM_DEPENDS, "      "),
        host = quoted(BUILD_HOST),
    );
    for e in entries {
        out.push_str(&format!("  - dst: {}\n", quoted(&e.dst)));
        if e.sha256.is_some() {
            out.push_str(&format!("    src: {}\n", quoted(&e.src.to_string_lossy())));
        } else {
            out.push_str("    type: \"dir\"\n");
        }
        out.push_str(&format!(
            "    file_info: {{mode: 0{:o}, owner: root, group: root}}\n",
            e.mode
        ));
    }
    out
}

/// `entries` as the lines of the file the install test reads: `file|dir`, mode, path,
/// SHA-256 (`-` for a directory), separated by spaces.
pub fn listing(entries: &[Entry]) -> String {
    entries
        .iter()
        .map(|e| match e.sha256 {
            Some(sha) => format!("file {:04o} {} {sha}\n", e.mode, e.dst),
            None => format!("dir {:04o} {} -\n", e.mode, e.dst),
        })
        .collect()
}

/// The entries a [`listing`] holds (their `src` is empty).
///
/// # Errors
/// A line that is not one a listing writes.
pub fn parse_listing(text: &str) -> Result<Vec<Entry>, Error> {
    text.lines()
        .map(|line| {
            let bad = || Error::Failed(format!("not a listing line: {line:?}"));
            let f: Vec<&str> = line.split(' ').collect();
            let &[kind, mode, dst, sha] = f.as_slice() else {
                return Err(bad());
            };
            let sha256 = match (kind, sha) {
                ("dir", "-") => None,
                ("file", hex) => Some(hex.parse::<Sha256>().map_err(|_| bad())?),
                _ => return Err(bad()),
            };
            Ok(Entry {
                dst: dst.to_owned(),
                mode: u32::from_str_radix(mode, 8).map_err(|_| bad())?,
                sha256,
                src: PathBuf::new(),
            })
        })
        .collect()
}

/// The `pkg-config` verb: writes `<out>/nfpm.yaml` and `<out>/listing.txt`.
///
/// # Errors
/// The inputs are refused, or a file cannot be written.
pub fn write_config(i: &Inputs, out: &Path) -> Result<Vec<String>, Error> {
    let entries = plan(i)?;
    std::fs::create_dir_all(out).map_err(Error::spawn(format!("create {}", out.display())))?;
    let files = [
        ("nfpm.yaml", config(&entries, i.platform, i.version)),
        ("listing.txt", listing(&entries)),
    ];
    let mut printed = Vec::new();
    for (name, text) in files {
        let path = out.join(name);
        std::fs::write(&path, text).map_err(Error::spawn(format!("write {}", path.display())))?;
        printed.push(format!("wrote {}", path.display()));
    }
    Ok(printed)
}

/// What a program printed and whether it succeeded.
#[derive(Clone, Debug, Default)]
pub struct Out {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `cmd` to its end.
pub fn spawn(cmd: &mut Command) -> Result<Out, Error> {
    let what = format!("run {:?}", cmd.get_program());
    let out = cmd.output().map_err(Error::spawn(what))?;
    Ok(Out {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Runs nfpm with the environment `env` and the arguments `args`.
pub type Nfpm<'a> = dyn FnMut(&[(&str, &str)], &[&str]) -> Result<Out, Error> + 'a;

/// The closure that runs the program `nfpm`, with `env` and nothing else of the
/// environment.
pub fn nfpm_process(nfpm: &Path) -> impl FnMut(&[(&str, &str)], &[&str]) -> Result<Out, Error> {
    let nfpm = nfpm.to_owned();
    move |env, args| {
        let mut cmd = Command::new(&nfpm);
        cmd.env_clear().envs(env.iter().copied()).args(args);
        spawn(&mut cmd)
    }
}

/// What `pkg-build` is given.
pub struct Build<'a> {
    /// The nfpm configuration (`pkg-config`'s `nfpm.yaml`).
    pub config: &'a Path,
    /// The directory the packages go in.
    pub out: &'a Path,
    /// The time the packages carry.
    pub epoch: u64,
}

/// Builds the `format` package (`deb` or `rpm`) into `dir` and says where it is.
fn once(nfpm: &mut Nfpm, b: &Build, format: &str, dir: &Path) -> Result<PathBuf, Error> {
    std::fs::create_dir_all(dir).map_err(Error::spawn(format!("create {}", dir.display())))?;
    let epoch = b.epoch.to_string();
    let argv = ["package", "-f", &b.config.to_string_lossy(), "-p", format];
    let target = dir.to_string_lossy();
    let args: Vec<&str> = argv.iter().copied().chain(["-t", &target]).collect();
    let out = nfpm(&[("SOURCE_DATE_EPOCH", &epoch)], &args)?;
    if !out.ok {
        return Err(Error::Failed(format!(
            "nfpm package -p {format}: {}",
            out.stderr.trim()
        )));
    }
    let wanted = format!(".{format}");
    let mut made: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(Error::spawn(format!("read {}", dir.display())))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(&wanted))
        .collect();
    match (made.pop(), made.is_empty()) {
        (Some(only), true) => Ok(only),
        _ => Err(Error::Failed(format!(
            "expected nfpm to leave one {wanted} in {}",
            dir.display()
        ))),
    }
}

/// The source files a [`config`] names.
fn sources(config: &str) -> Result<Vec<PathBuf>, Error> {
    config
        .lines()
        .filter_map(|l| l.strip_prefix("    src: "))
        .map(|v| {
            serde_json::from_str::<String>(v)
                .map(PathBuf::from)
                .map_err(|e| Error::Failed(format!("not a src line: {v:?}: {e}")))
        })
        .collect()
}

/// The modification times the two builds of a package see on its sources: a package
/// that is the same bytes under both does not depend on them.
const SOURCE_TIMES: [u64; 2] = [1_000_000_000, 1_500_000_000];

/// Sets the modification time of every file in `paths` to `secs` after the epoch.
fn stamp(paths: &[PathBuf], secs: u64) -> Result<(), Error> {
    let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
    for p in paths {
        std::fs::File::open(p)
            .and_then(|f| f.set_modified(time))
            .map_err(Error::spawn(format!("set the time of {}", p.display())))?;
    }
    Ok(())
}

/// The `pkg-build` verb: the deb and the rpm in `b.out`, each built a second time, with
/// the modification times of the sources changed, to prove the same inputs are the same
/// bytes.
///
/// # Errors
/// nfpm fails, or the two builds of a package differ.
pub fn build(nfpm: &mut Nfpm, b: &Build) -> Result<Vec<String>, Error> {
    let again = b.out.join("again");
    let sources = sources(&String::from_utf8_lossy(&read(b.config)?))?;
    let mut printed = Vec::new();
    for format in ["deb", "rpm"] {
        stamp(&sources, SOURCE_TIMES[0])?;
        let first = once(nfpm, b, format, b.out)?;
        stamp(&sources, SOURCE_TIMES[1])?;
        let second = once(nfpm, b, format, &again.join(format))?;
        let bytes = read(&first)?;
        if bytes != read(&second)? {
            return Err(Error::Failed(format!(
                "{} is not reproducible: a second build of the same inputs differs",
                first.display()
            )));
        }
        printed.push(format!("{} {}", Sha256::of(&bytes), first.display()));
    }
    // Nothing in it is needed once the builds are compared.
    let _ = std::fs::remove_dir_all(&again);
    Ok(printed)
}

#[cfg(test)]
mod tests;
