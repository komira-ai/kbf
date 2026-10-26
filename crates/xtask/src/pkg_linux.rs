//! `pkg-config` and `pkg-build`: the Linux package `kbf` (packaging/linux/README.md).
//!
//! The package is built by nfpm, pinned in CI by SHA-256. This module is the one place
//! that says what goes in it:
//!
//! - [`kbf_plan`] lists the files of the `kbf` package from the inputs: the `kbf` binary at
//!   [`KBF_PATH`], the mode units ([`KBF_UNITS`]) and `kbf.slice`, the users and directories
//!   they run with (sysusers and tmpfiles), the needrestart drop-in, and the trust directory
//!   ([`TRUST_DIR`]) the pinned `gh` and the Sigstore trusted root verify attestations from.
//!   It refuses a trust file other than the three and a unit directory that lacks one of the
//!   units. Everything else in the unit directory (the retired installer's and proxy's units)
//!   is not packaged;
//! - [`config`] renders a list as the nfpm configuration, with the dependencies as
//!   constants here (never Podman, never Envoy), no maintainer script, every file
//!   `root:root`, and no time (nfpm takes it from `SOURCE_DATE_EPOCH`). The package `Depends` on
//!   nothing: a host that updates it with a plain `dpkg --install` (no apt, no resolution) must be
//!   able to configure it. The deb replaces every `kbf-installer` older than
//!   [`INSTALLER_RETIRED_FROM`] ([`KBF_DEB_REPLACES`]): the files above were its files. It does not
//!   `Break` it, which would make that same plain `dpkg --install` refuse on a host that has it;
//! - [`files`] lists the files of the retired installer's package for `installer-tarball`
//!   (the payload of `packaging/install.sh`), which no release publishes any more;
//! - [`listing`] renders the same list as the file the install test checks the
//!   installed package against ([`parse_listing`]);
//! - [`build`] runs nfpm for the deb and the rpm, then again, and refuses a package
//!   whose second build differs in a byte.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;
use crate::fetch::Sha256;
use crate::package::{Platform, read};

/// The name of the retired installer package, and of its binary.
pub const NAME: &str = "kbf-installer";
/// Where the binary goes.
pub const BINARY_PATH: &str = "/usr/bin/kbf-installer";
/// The name of the second package, which carries the `kbf` runtime and the units of its modes.
pub const KBF_NAME: &str = "kbf";
/// Where the `kbf` runtime goes: its own package's file, which the package manager replaces.
pub const KBF_PATH: &str = "/usr/bin/kbf";
/// The units of the modes `kbf` runs in, which the `kbf` package carries. (`kbf.slice`, which
/// they run in, is the `kbf` package's too: [`SLICE`].)
pub const KBF_UNITS: [&str; 5] = [
    "kbf-all-in-one.service",
    "kbf-daemon.service",
    "kbf-frontend.service",
    "kbf-storage-and-daemon.service",
    "kbf-storage.service",
];
/// The first version no `kbf-installer` package has: every deb that was published is
/// `0.1.0+<commit>`. The deb of `kbf` replaces the installer packages below it, so that a later
/// package of that name (there is none) would not be.
pub const INSTALLER_RETIRED_FROM: &str = "0.1.1";
/// What the deb of the `kbf` package `Replaces`: the `kbf-installer` package of every
/// published version. The units of the modes, `kbf.slice`, the trust files and the needrestart
/// drop-in were files of its deb, and dpkg refuses to unpack `kbf` over files another package
/// owns unless `kbf` says it replaces that package; with it, a plain `dpkg --install` of `kbf`
/// unpacks over the installer's files, which change hands, and the installer stays installed (with
/// the rest of its files) until an operator purges it, taking none of them. The rpm names nothing: its only form,
/// `Obsoletes`, would remove the installer, and no rpm is published.
pub const KBF_DEB_REPLACES: [&str; 1] = ["kbf-installer (<< 0.1.1)"];
/// Where the unit files go.
pub const UNIT_DIR: &str = "/usr/lib/systemd/system";
/// The one packaged unit that is not a `kbf-*.service` or `kbf-*.socket`: the slice
/// the units run in.
pub const SLICE: &str = "kbf.slice";
/// The directory the `kbf` package owns, holding [`TRUST_DIR`].
pub const SHARE_DIR: &str = "/usr/share/kbf";
/// Where the trust files go: `kbf_setup::layout`'s trust directory.
pub const TRUST_DIR: &str = "/usr/share/kbf/trust";
/// Where the users file goes (`systemd-sysusers`).
pub const SYSUSERS_PATH: &str = "/usr/lib/sysusers.d/kbf.conf";
/// Where the directories file goes (`systemd-tmpfiles`).
pub const TMPFILES_PATH: &str = "/usr/lib/tmpfiles.d/kbf.conf";
/// The same four, where the retired installer's package (and the payload of `install.sh`)
/// put them.
pub const INSTALLER_SHARE_DIR: &str = "/usr/share/kbf-installer";
pub const INSTALLER_TRUST_DIR: &str = "/usr/share/kbf-installer/trust";
pub const INSTALLER_SYSUSERS_PATH: &str = "/usr/lib/sysusers.d/kbf-installer.conf";
pub const INSTALLER_TMPFILES_PATH: &str = "/usr/lib/tmpfiles.d/kbf-installer.conf";
/// Where the needrestart drop-in goes: a host without needrestart ignores it.
pub const NEEDRESTART_PATH: &str = "/etc/needrestart/conf.d/kbf.conf";
/// The directory `setup` fills and the package never removes.
pub const STATE_DIR: &str = "/var/lib/kbf-installer";
/// The directories `tmpfiles.d` makes, with their modes (all `root:root`). The file is
/// the unit author's, packaged unchanged; `STATE_DIR` is made by `kbf-installer setup`.
pub const TMPFILES_DIRS: [(&str, &str); 2] = [
    ("/run/kbf-installer", "755"),
    ("/var/cache/kbf-fetch", "711"),
];
/// The users `sysusers.d` makes.
pub const USERS: [&str; 4] = ["kbf", "kbf-server", "kbf-fetch", "kbf-signer"];
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
/// The `Recommends` of the deb of `kbf` (they were the installer's `Depends`). Not `Depends`: a
/// host updates this package with a plain `dpkg --install`, which resolves nothing, so a
/// dependency a host lacks would leave `kbf` unpacked and unconfigured and then undone. `crun`
/// and `uidmap` are the container driver's, and its preflight names `crun` when it is missing;
/// `systemd` runs the units and `ca-certificates` is what the host's downloads verify with, and
/// any host that runs these units has them, but a recommendation cannot fail an install where a
/// dependency can.
pub const DEB_RECOMMENDS: [&str; 4] = ["crun", "uidmap", "systemd", "ca-certificates"];
/// The `Suggests` of the deb: a Postgres server, for a frontend or all-in-one host that keeps
/// its state on the host. A suggestion, not a `Depends` or `Recommends`: node hosts keep no
/// state in Postgres, and apt installs recommended packages by default. `setup` refuses a
/// frontend host with no reachable server, saying what to install.
pub const DEB_SUGGESTS: [&str; 1] = ["postgresql"];
/// The `Suggests` of the rpm.
pub const RPM_SUGGESTS: [&str; 1] = ["postgresql-server"];
/// The `Recommends` of the rpm: Fedora's `uidmap` is `shadow-utils`.
pub const RPM_RECOMMENDS: [&str; 4] = ["crun", "shadow-utils", "systemd", "ca-certificates"];

/// The lines of a YAML list of the strings `names`, each at `indent`.
fn list(names: &[&str], indent: &str) -> String {
    names
        .iter()
        .map(|n| format!("{indent}- {}\n", quoted(n)))
        .collect()
}

/// The description the package carries.
const DESCRIPTION: &str =
    "The kbf build farm runtime, the systemd units of its modes, and what they run with.";

/// The `recommends`, `suggests` and `overrides` keys of the configuration, as lines (there is no
/// `depends`).
fn relations() -> String {
    format!(
        "recommends:\n{}suggests:\n{}overrides:\n  rpm:\n    recommends:\n{}    suggests:\n{}  deb:\n    replaces:\n{}",
        list(&DEB_RECOMMENDS, "  "),
        list(&DEB_SUGGESTS, "  "),
        list(&RPM_RECOMMENDS, "      "),
        list(&RPM_SUGGESTS, "      "),
        list(&KBF_DEB_REPLACES, "      "),
    )
}

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
    /// The `kbf` binary, which the package carries at [`KBF_PATH`].
    pub kbf: &'a Path,
    /// The directory of unit files: the package takes the units of the modes
    /// ([`KBF_UNITS`]) and the slice ([`SLICE`]) from it, and no other.
    pub units: &'a Path,
    /// The directory of the three trust files.
    pub trust: &'a Path,
    /// The sysusers.d file.
    pub sysusers: &'a Path,
    /// The tmpfiles.d file.
    pub tmpfiles: &'a Path,
    /// The needrestart drop-in.
    pub needrestart: &'a Path,
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

/// The files of the retired installer's package, which `installer-tarball` places: no
/// version and no platform, which only the nfpm configuration needs.
pub struct Sources<'a> {
    /// The `kbf-installer` binary.
    pub binary: &'a Path,
    /// The directory of `kbf-*.service` and `kbf-*.socket` files and the slice.
    pub units: &'a Path,
    /// The directory of the three trust files.
    pub trust: &'a Path,
    /// The sysusers.d file.
    pub sysusers: &'a Path,
    /// The tmpfiles.d file.
    pub tmpfiles: &'a Path,
    /// The needrestart drop-in.
    pub needrestart: &'a Path,
}

/// What is wrong with the version and the platform of `i`.
fn input_problems(i: &Inputs) -> Vec<String> {
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
    problems
}

/// Everything the `kbf` package installs, sorted by path: the `kbf` binary, the units of the
/// modes ([`KBF_UNITS`]) and the slice, which the unit directory must hold, the sysusers,
/// tmpfiles and needrestart files, and the trust directory with its three files.
///
/// # Errors
/// [`Error::Refused`] with every problem with the inputs.
pub fn kbf_plan(i: &Inputs) -> Result<Vec<Entry>, Error> {
    let mut problems = input_problems(i);
    let mut entries = vec![
        file(KBF_PATH.to_owned(), 0o755, i.kbf, &mut problems),
        file(SYSUSERS_PATH.to_owned(), 0o644, i.sysusers, &mut problems),
        file(TMPFILES_PATH.to_owned(), 0o644, i.tmpfiles, &mut problems),
        file(
            NEEDRESTART_PATH.to_owned(),
            0o644,
            i.needrestart,
            &mut problems,
        ),
        dir(SHARE_DIR),
        dir(TRUST_DIR),
    ];
    let units = files_in(i.units, &mut problems);
    for name in KBF_UNITS.into_iter().chain([SLICE]) {
        let Some((_, src)) = units.iter().find(|(n, _)| *n == name) else {
            problems.push(format!("unit file {name} is missing"));
            continue;
        };
        entries.push(file(
            format!("{UNIT_DIR}/{name}"),
            0o644,
            src,
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

/// Everything the retired installer's package installed, sorted by path: the one list the
/// `install.sh` payload ([`crate::installer_tarball`]) is made from.
///
/// # Errors
/// [`Error::Refused`] with every problem with the sources.
pub fn files(i: &Sources) -> Result<Vec<Entry>, Error> {
    entries(i, Vec::new())
}

fn entries(i: &Sources, mut problems: Vec<String>) -> Result<Vec<Entry>, Error> {
    let mut entries = vec![
        file(BINARY_PATH.to_owned(), 0o755, i.binary, &mut problems),
        file(
            INSTALLER_SYSUSERS_PATH.to_owned(),
            0o644,
            i.sysusers,
            &mut problems,
        ),
        file(
            INSTALLER_TMPFILES_PATH.to_owned(),
            0o644,
            i.tmpfiles,
            &mut problems,
        ),
        file(
            NEEDRESTART_PATH.to_owned(),
            0o644,
            i.needrestart,
            &mut problems,
        ),
        dir(INSTALLER_SHARE_DIR),
        dir(INSTALLER_TRUST_DIR),
    ];
    // The units of the modes are the `kbf` package's.
    let units: Vec<(String, PathBuf)> = files_in(i.units, &mut problems)
        .into_iter()
        .filter(|(name, _)| !KBF_UNITS.contains(&name.as_str()))
        .collect();
    if units.is_empty() {
        problems.push(format!("no unit files in {}", i.units.display()));
    }
    for (name, src) in units {
        let kbf = name.starts_with("kbf-");
        if !(name == SLICE || (kbf && (name.ends_with(".service") || name.ends_with(".socket")))) {
            problems.push(format!(
                "{name}: a package unit is named kbf-*.service or kbf-*.socket (or is {SLICE})"
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
                    format!("{INSTALLER_TRUST_DIR}/{name}"),
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

/// The nfpm configuration of the package for `entries`.
pub fn config(entries: &[Entry], platform: Platform, version: &str) -> String {
    let arch = match platform {
        Platform::LinuxArm64 => "arm64",
        _ => "amd64",
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
         description: {description}\n\
         homepage: \"https://github.com/komira-ai/kbf\"\n\
         license: \"Apache-2.0\"\n\
         {relations}\
         deb:\n  compression: \"gzip\"\n\
         rpm:\n  compression: \"gzip\"\n  buildhost: {host}\n\
         contents:\n",
        name = quoted(KBF_NAME),
        arch = quoted(arch),
        version = quoted(version),
        description = quoted(DESCRIPTION),
        relations = relations(),
        host = quoted(BUILD_HOST),
    );
    for e in entries {
        out.push_str(&format!("  - dst: {}\n", quoted(&e.dst)));
        if e.sha256.is_some() {
            out.push_str(&format!("    src: {}\n", quoted(&e.src.to_string_lossy())));
        } else {
            out.push_str("    type: \"dir\"\n");
        }
        // Under /etc a file is configuration: an upgrade keeps the host's edits.
        if e.dst.starts_with("/etc/") {
            out.push_str("    type: \"config|noreplace\"\n");
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

/// The `pkg-config` verb: writes `<out>/nfpm.yaml` and `<out>/listing.txt` for the `kbf`
/// package.
///
/// # Errors
/// The inputs are refused, or a file cannot be written.
pub fn write_config(i: &Inputs, out: &Path) -> Result<Vec<String>, Error> {
    let entries = kbf_plan(i)?;
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
