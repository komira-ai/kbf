//! `pkg-kbf-asset --package <deb> --platform <platform> --commit <sha> --kbf-tarball <file>
//! --expect-ref refs/heads/<branch> [--gh <program>] (--out <dir> | --verify-only)`: the `kbf`
//! deb as a release asset, `kbf-<commit>-<platform>.deb`, and the checks that tie it to the
//! bytes the attestation covers.
//!
//! The deb is opened ([`crate::pkg_asset::open`]) and refused unless:
//!
//! - its control file says the architecture of `--platform`, the package `kbf`, the
//!   recommendations and suggestions of [`crate::pkg_linux`], no `Depends`, `Pre-Depends`,
//!   `Breaks` or `Conflicts` at all (a host updates it with a plain `dpkg --install`, which
//!   resolves nothing: a dependency the host lacks leaves it unconfigured, and a `Breaks` of the
//!   installer makes dpkg refuse on a host that has it), and that it `Replaces` the
//!   `kbf-installer` packages older than the first version that has none
//!   ([`KBF_DEB_REPLACES`]): the units, `kbf.slice`, the sysusers and tmpfiles files, the
//!   needrestart drop-in and the trust files were files of the installer's deb, and dpkg refuses
//!   to unpack a package over files another package owns unless the control file says so;
//! - `usr/bin/kbf` is byte for byte the `kbf` inside `--kbf-tarball`, the tarball
//!   `build-commit check-asset` has already shown names the commit: the deb carries the
//!   bytes the build identified, not others. It is mode 0755;
//! - it holds the unit of every mode ([`crate::pkg_linux::KBF_UNITS`]) and `kbf.slice` in
//!   `usr/lib/systemd/system`, the sysusers.d and tmpfiles.d files and the needrestart
//!   drop-in, all mode 0644;
//! - it holds the host agent: the helper's and the agent's units
//!   ([`crate::pkg_linux::AGENT_UNITS`]) beside the others and the agent's sysusers.d file, mode
//!   0644, and the link that enables each, `<WANTS_DIR>/<unit> -> ../<unit>`
//!   ([`crate::pkg_linux::WANTS_DIR`]): a host that installs the published deb runs its agent
//!   from its next boot;
//! - its trust directory `usr/share/kbf/trust` holds the pinned `gh` (mode 0755, and byte for
//!   byte `--gh`, the program the job downloaded and checked by SHA-256, when given), the
//!   Sigstore trusted root and an `installer.conf` that is exactly `SOURCE_REFS=` and
//!   `--expect-ref` ([`crate::pkg_asset::trust_problems`]): a deb built on `main` trusts
//!   `main` and a deb built on a feature branch trusts that branch, never the reverse;
//! - it holds no other file and no other link.
//!
//! With `--native` it is the native package instead, `kbf-native-<commit>-<platform>.deb`: the same
//! checks without the host agent's units, account and links (a deb that holds them is refused), and
//! a `Version` of exactly `<major>.<minor>.<seq>`, which with `--native-version-of <kbf>` must be
//! the native version that `kbf --version --json` names ([`crate::pkg_asset::native_version`]).
//!
//! Then, without `--verify-only`, it copies the deb to `<out>/kbf-<commit>-<platform>.deb`
//! and appends its line to `<out>/SHA256SUMS.<platform>` ([`crate::sums`]), refusing a deb
//! already listed. With `--verify-only` it makes the checks on `--package`, which must
//! already be named as the asset (the file the attest job signed, downloaded again by the
//! verify job) and writes nothing.

use std::path::{Path, PathBuf};

use crate::Error;
use crate::package::Platform;
use crate::pkg_asset::{self, Mode};
use crate::pkg_linux::{
    AGENT_SYSUSERS_PATH, AGENT_UNITS, DEB_RECOMMENDS, DEB_SUGGESTS, KBF_DEB_REPLACES, KBF_NAME,
    KBF_PATH, KBF_UNITS, NEEDRESTART_PATH, SLICE, SYSUSERS_PATH, TMPFILES_PATH, TRUST_DIR,
    TRUST_FILES, UNIT_DIR, WANTS_DIR,
};

/// The name of the native package's asset: `kbf-native-<commit>-<platform>.deb`.
pub const NATIVE_ASSET: &str = "kbf-native";

/// The file name of the asset: `kbf-<commit>-<platform>.deb`, or with `native`
/// `kbf-native-<commit>-<platform>.deb`.
pub fn file_name(commit: &str, platform: Platform, native: bool) -> String {
    let name = if native { NATIVE_ASSET } else { KBF_NAME };
    format!("{name}-{commit}-{}.deb", platform.name())
}

/// The inputs of the checks.
pub struct Check<'a> {
    pub platform: Platform,
    /// The `kbf-<commit>-<platform>.tar.gz` the deb's `kbf` must equal.
    pub kbf_tarball: &'a Path,
    /// The branch the deb must trust: `refs/heads/<branch>`.
    pub expect_ref: &'a str,
    /// The pinned `gh` the deb's must equal, when the job has it.
    pub gh: Option<&'a Path>,
    /// The native package (`--native`): its control file says a `Version` of exactly three
    /// numbers, it holds no host agent, and its asset is `kbf-native-<commit>-<platform>.deb`.
    pub native: bool,
    /// With `native`: the version its control file must say, `<major>.<minor>.<seq>` of the
    /// `kbf` it carries (`--native-version-of`).
    pub native_version: Option<&'a str>,
}

/// The value of the control field that starts a line with `name` (such as `Depends: `).
fn field<'a>(control: &'a str, name: &str) -> Option<&'a str> {
    control.lines().find_map(|l| l.strip_prefix(name))
}

/// `text` without its white space: two spellings of a dependency differ in spaces only.
fn squashed(text: &str) -> String {
    text.split_whitespace().collect()
}

/// Whether the comma-separated control field value `list` holds the entry `want`.
fn holds(list: &str, want: &str) -> bool {
    list.split(',').any(|d| squashed(d) == squashed(want))
}

/// The path of the file `dst` of the package in a data tarball (no leading slash).
fn member_path(dst: &str) -> String {
    dst.trim_start_matches('/').to_owned()
}

/// The paths of the trust files, as a data tarball names them.
fn trust_paths() -> Vec<String> {
    TRUST_FILES
        .map(|(n, _)| member_path(&format!("{TRUST_DIR}/{n}")))
        .to_vec()
}

/// The regular files of the package other than `usr/bin/kbf` and the trust files (which
/// [`pkg_asset::trust_problems`] holds to their modes and contents), as a data tarball names
/// them, each with the mode it has: the units (the host agent's among them), the slice, and the
/// sysusers (the agent's too), tmpfiles and needrestart files.
fn other_files(native: bool) -> Vec<(String, u32)> {
    let agent: &[&str] = if native { &[] } else { &AGENT_UNITS };
    let units = KBF_UNITS
        .into_iter()
        .chain([SLICE])
        .chain(agent.iter().copied());
    let units = units.map(|u| (member_path(&format!("{UNIT_DIR}/{u}")), 0o644));
    let agent_account: &[&str] = if native { &[] } else { &[AGENT_SYSUSERS_PATH] };
    let single = [SYSUSERS_PATH, TMPFILES_PATH, NEEDRESTART_PATH]
        .into_iter()
        .chain(agent_account.iter().copied())
        .map(|p| (member_path(p), 0o644));
    units.chain(single).collect()
}

/// What is wrong with the `Version` of the native package's `control`: not exactly
/// `<major>.<minor>.<seq>` (the shared apt repository takes no `+`, `~` or revision), or not
/// `want`, the native version of the `kbf` it carries, when given.
fn native_version_problems(control: &str, want: Option<&str>) -> Vec<String> {
    let got = field(control, "Version: ").unwrap_or("(none)");
    let numbers = got.split('.').count() == 3
        && got
            .split('.')
            .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    let mut problems = Vec::new();
    if !numbers {
        problems.push(format!(
            "control says Version {got:?}, not <major>.<minor>.<seq> (three numbers, no `+`, `~` or revision)"
        ));
    }
    if let Some(want) = want
        && got != want
    {
        problems.push(format!(
            "control says Version {got:?}, not {want:?}, the native version of the kbf it carries"
        ));
    }
    problems
}

/// The links of the package, as a data tarball names them, each with its target: the one that
/// enables each of the host agent's units.
fn links(native: bool) -> Vec<(String, String)> {
    if native {
        return Vec::new();
    }
    AGENT_UNITS
        .map(|u| (member_path(&format!("{WANTS_DIR}/{u}")), format!("../{u}")))
        .to_vec()
}

/// What is wrong with `deb` (the bytes of the package) as the `kbf` package for `c`: one
/// line per problem, none when it meets every check above.
///
/// # Errors
/// A file `c` names cannot be read.
pub fn problems(deb: &[u8], c: &Check) -> Result<Vec<String>, Error> {
    let mut problems = Vec::new();
    let Some(opened) = pkg_asset::open(deb, c.platform, &mut problems) else {
        return Ok(problems);
    };
    let package = field(&opened.control, "Package: ");
    if package != Some(KBF_NAME) {
        problems.push(format!(
            "control says Package {:?}, not {KBF_NAME:?}",
            package.unwrap_or("(none)")
        ));
    }
    for name in ["Depends", "Pre-Depends", "Breaks", "Conflicts"] {
        let got = field(&opened.control, &format!("{name}: ")).unwrap_or_default();
        if !got.trim().is_empty() {
            problems.push(format!(
                "control says {name} {got:?}, but the kbf package has none: a plain `dpkg --install` resolves nothing"
            ));
        }
    }
    for (name, wanted) in [
        ("Recommends", &DEB_RECOMMENDS[..]),
        ("Suggests", &DEB_SUGGESTS[..]),
        ("Replaces", &KBF_DEB_REPLACES[..]),
    ] {
        let got = field(&opened.control, &format!("{name}: ")).unwrap_or_default();
        for want in wanted {
            if !holds(got, want) {
                problems.push(format!(
                    "control says {name} {got:?}, which does not hold {want:?}"
                ));
            }
        }
    }
    let path = KBF_PATH.trim_start_matches('/');
    if c.native {
        problems.extend(native_version_problems(&opened.control, c.native_version));
    }
    let binary = pkg_asset::binary_problems(opened.file(path), path, KBF_NAME, c.kbf_tarball)?;
    problems.extend(binary);
    let others = other_files(c.native);
    for (path, mode) in &others {
        match opened.file(path) {
            None => problems.push(format!("holds no {path}")),
            Some(m) if m.mode != *mode => {
                problems.push(format!("{path} has mode {:04o}, not {mode:04o}", m.mode));
            }
            Some(_) => {}
        }
    }
    problems.extend(pkg_asset::trust_problems(&opened, c.expect_ref, c.gh)?);
    let trust = trust_paths();
    for m in &opened.files {
        if m.path != path && !trust.contains(&m.path) && !others.iter().any(|(p, _)| *p == m.path) {
            problems.push(format!(
                "holds {}, which the kbf package does not ship",
                m.path
            ));
        }
    }
    problems.extend(link_problems(&opened, c.native));
    Ok(problems)
}

/// What is wrong with the links of `opened`: a path the data tarball holds more than once (two
/// entries, links or a file and a link, which dpkg would unpack one over the other), a link the
/// package ships that is missing, every link that points elsewhere than [`links`] says, and a link
/// it does not ship.
fn link_problems(opened: &pkg_asset::Opened, native: bool) -> Vec<String> {
    let mut problems = Vec::new();
    let mut paths: Vec<&str> = opened
        .files
        .iter()
        .map(|m| m.path.as_str())
        .chain(opened.links.iter().map(|(p, _)| p.as_str()))
        .collect();
    paths.sort_unstable();
    let mut repeated: Vec<&str> = paths
        .windows(2)
        .filter(|w| w[0] == w[1])
        .map(|w| w[0])
        .collect();
    repeated.dedup();
    for path in repeated {
        problems.push(format!("holds {path} more than once"));
    }
    let wanted = links(native);
    for (link, _) in &wanted {
        if !opened.links.iter().any(|(p, _)| p == link) {
            problems.push(format!(
                "holds no link {link}, which enables its unit at boot"
            ));
        }
    }
    for (link, got) in &opened.links {
        match wanted.iter().find(|(p, _)| p == link) {
            Some((_, target)) if got != target => {
                problems.push(format!("{link} points to {got:?}, not {target:?}"));
            }
            Some(_) => {}
            None => problems.push(format!(
                "holds the link {link} -> {got}, which the kbf package does not ship"
            )),
        }
    }
    problems
}

/// The `pkg-kbf-asset` verb: the lines to print.
///
/// # Errors
/// The commit is not one, the deb is refused, or a file cannot be read or written.
pub fn asset(deb: &Path, commit: &str, mode: &Mode, c: &Check) -> Result<Vec<String>, Error> {
    let said = format!(
        "a {}{} package of the kbf in {}, trusting {}",
        if c.native { "native " } else { "" },
        c.platform.name(),
        c.kbf_tarball.display(),
        c.expect_ref
    );
    let name = file_name(commit, c.platform, c.native);
    let found = |bytes: &[u8]| problems(bytes, c);
    pkg_asset::publish(deb, (commit, c.platform), &name, mode, found, &said)
}

#[cfg(test)]
mod tests;

/// The flags of `pkg-kbf-asset`: the kbf deb (or, with `--native`, the native package) as a
/// release asset, refused unless its `/usr/bin/kbf` is the attested `kbf`, it replaces the old
/// kbf-installer, and it holds the units of the modes, the slice, the sysusers and tmpfiles files
/// and a trust directory that trusts the branch it is attested for, and nothing else.
#[derive(clap::Args, Debug)]
pub struct AssetArgs {
    /// The deb nfpm built, or the directory holding the one (with `--verify-only`: the
    /// asset itself).
    #[arg(long)]
    pub package: PathBuf,
    /// The platform it was built for (`linux-x86_64`, `linux-arm64`).
    #[arg(long, value_enum)]
    pub platform: Platform,
    /// The commit it was built from (`$GITHUB_SHA`).
    #[arg(long)]
    pub commit: String,
    /// The `kbf-<commit>-<platform>.tar.gz` whose `kbf` the deb must hold.
    #[arg(long)]
    pub kbf_tarball: PathBuf,
    /// The branch the deb must trust, and is attested for (`$GITHUB_REF`).
    #[arg(long)]
    pub expect_ref: String,
    /// The pinned `gh` the deb's must be byte for byte (optional with --verify-only).
    #[arg(long)]
    pub gh: Option<PathBuf>,
    /// The native package's asset, `kbf-native-<commit>-<platform>.deb`: the deb built with
    /// `pkg-config --no-host-agent --native-version-of <kbf>`. It holds no host agent, and
    /// its `Version` is `<major>.<minor>.<seq>`.
    #[arg(long)]
    pub native: bool,
    /// With `--native`: the `kbf` (an executable of this machine) whose `--version --json`
    /// gives the native version the deb's `Version` must be.
    #[arg(long, requires = "native")]
    pub native_version_of: Option<PathBuf>,
    /// The directory the asset and its line of `SHA256SUMS.<platform>` go in.
    #[arg(
        long,
        required_unless_present = "verify_only",
        conflicts_with = "verify_only"
    )]
    pub out: Option<PathBuf>,
    /// Only check `--package`, which must be named as the asset; write nothing.
    #[arg(long)]
    pub verify_only: bool,
}

/// The `pkg-kbf-asset` verb.
///
/// # Errors
/// What [`asset`] refuses, a package that is not one deb, and a `--native-version-of` binary
/// whose version cannot be read.
pub fn run(args: &AssetArgs) -> Result<Vec<String>, Error> {
    let native_version = args
        .native_version_of
        .as_deref()
        .map(pkg_asset::binary_native_version)
        .transpose()?;
    asset(
        &pkg_asset::sole(&args.package, "deb")?,
        &args.commit,
        &match (&args.out, args.verify_only) {
            (Some(out), false) => Mode::Add(out),
            _ => Mode::VerifyOnly,
        },
        &Check {
            platform: args.platform,
            kbf_tarball: &args.kbf_tarball,
            expect_ref: &args.expect_ref,
            gh: args.gh.as_deref(),
            native: args.native,
            native_version: native_version.as_deref(),
        },
    )
}
