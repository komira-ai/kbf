//! Everything that is checked before anything on the Mac changes.
//!
//! The release files are untrusted bytes until they pass, and they are copied ONCE, into a
//! directory only root can write (`<work>/stage`, mode 0700), and only that copy is
//! checked, in this order (for a channel whose source is GitHub, the two files are first
//! fetched as `nobody` into a directory it owns inside the stage, [`super::fetch`], and the
//! copy into the stage is the same one):
//!
//! 1. when the channel pins its releases, the sha256 of the copied tarball is the one its
//!    sums list (the extra freeze list; a channel without sums skips this);
//! 2. `gh attestation verify` accepts the copied tarball and its bundle, offline: the
//!    tarball's sha256 is a subject, the signer is this repository's `artifacts.yml` on a
//!    GitHub-hosted runner, and the build was of the full 40-digit commit on the channel's
//!    branch. It runs as `nobody` and never as root. This is the trust anchor;
//! 3. the tarball holds exactly one regular file, `kbf`, a Mach-O file for arm64;
//! 4. no `com.apple.quarantine` attribute is left on it;
//! 5. `codesign --verify --strict` accepts it and its signature has the hardened runtime;
//! 6. the new binary, run as the daemon's user, accepts the daemon's live configuration
//!    (`kbf --mode=daemon --config <file> --check`).
//!
//! The stage directory is opened to traverse-only (0711) for the two steps that run as
//! another user (2 and 6) and sealed (0700) again after each. Any failure removes the stage and
//! changes nothing else.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, chown};
use std::path::Path;

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use super::channel::{MacChannel, Source};
use super::fetch::{AssetKind, FetchError, Fetcher, Limits};
use super::layout::{BINARY, DAEMON_USER, MacHost, UNPRIVILEGED_USER};
use super::release::{self, Origin, PickError};
use super::tools;
use crate::config::refusal;
use crate::host::fail;
use crate::outcome::{FailureCode, Outcome};
use crate::request::Version;
use crate::runner::{Exec, Identity, Runner};
use crate::trust::Trust;
use crate::trust::nofollow;

/// The Mach-O header of a 64-bit arm64 executable: magic, then cputype.
const MACHO_ARM64: [u8; 8] = [0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01];

/// The tarball of `commit` in the releases of `origin`.
fn tarball_of(origin: &Origin, commit: &str) -> String {
    release::named_asset_on(
        origin.prefix(),
        release::PLATFORM,
        release::TARBALL_EXTENSION,
        commit,
    )
}

/// The attestation bundle of [`tarball_of`].
fn bundle_of(origin: &Origin, commit: &str) -> String {
    release::bundle_of(&tarball_of(origin, commit))
}

/// The release a version picked out: its commit, and the sha256 the channel pins for its
/// tarball when it pins any.
struct Pick {
    /// The commit (all 40 digits) of a channel with a directory or sums; for a channel on
    /// GitHub with no sums, the version's 12 digits.
    want: String,
    sha256: Option<String>,
}

/// A release that passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    pub commit: String,
    /// The sha256 of the staged binary: the apply checks it again before it uses the file.
    pub binary_sha256: String,
}

fn verify_failed(detail: impl Into<String>) -> Outcome {
    fail(FailureCode::VerifyFailed, detail)
}

/// Check the release `version` names in `channel` and leave it in the stage.
///
/// # Errors
/// A refusal for a channel's pinned sums that are not usable, a failure for everything
/// else; the stage is removed.
pub fn stage(
    host: &MacHost,
    runner: &dyn Runner,
    channel: &MacChannel,
    version: &Version,
) -> Result<Staged, Outcome> {
    let result = stage_inner(host, runner, channel, version);
    if result.is_err() {
        let _ = fs::remove_dir_all(host.stage());
    }
    result
}

fn stage_inner(
    host: &MacHost,
    runner: &dyn Runner,
    channel: &MacChannel,
    version: &Version,
) -> Result<Staged, Outcome> {
    let prefix = release::commit_prefix(version.as_str()).ok_or_else(|| {
        verify_failed("the version carries no commit (+g or +, then 12 hex digits)")
    })?;
    let origin = &channel.origin;
    let pick = pick_release(host, channel, prefix)?;
    let token = match &channel.source {
        Source::Github {
            token_file: Some(file),
        } => Some(read_token(&host.trust, file).map_err(Outcome::Refused)?),
        _ => None,
    };
    // The pinned tools are root's, or nothing from them is believed.
    let lister = matches!(channel.source, Source::Github { .. }).then_some(&host.curl);
    for tool in [&host.gh, &host.trusted_root].into_iter().chain(lister) {
        host.trust.check_file(tool).map_err(|e| {
            Outcome::Refused(match refusal(e) {
                crate::outcome::Refusal::ConfigUnsafe => crate::outcome::Refusal::KeyUntrusted,
                other => other,
            })
        })?;
    }
    let nobody = lookup(host, runner, UNPRIVILEGED_USER)?;
    let daemon = lookup(host, runner, DAEMON_USER)?;

    let stage = host.stage();
    make_stage(&stage).map_err(|e| fail(FailureCode::InstallFailed, format!("stage: {e}")))?;
    let home = stage.join("gh-home");
    let tuning = &host.tuning;
    let commit = match &channel.source {
        Source::Dir(dir) => {
            let tarball = stage.join(tarball_of(origin, &pick.want));
            let bundle = stage.join(bundle_of(origin, &pick.want));
            copy_in(
                &dir.join(tarball_of(origin, &pick.want)),
                &tarball,
                tuning.tarball_max,
            )
            .map_err(|e| verify_failed(format!("tarball: {e}")))?;
            copy_in(
                &dir.join(bundle_of(origin, &pick.want)),
                &bundle,
                tuning.bundle_max,
            )
            .map_err(|e| verify_failed(format!("attestation bundle: {e}")))?;
            pick.want.clone()
        }
        Source::Github { .. } => {
            make_home(&home, nobody)?;
            fetch_release(
                (host, runner, origin),
                &pick.want,
                token.as_deref(),
                nobody,
                &home,
            )?
        }
    };
    let tarball = stage.join(tarball_of(origin, &commit));
    let bundle = stage.join(bundle_of(origin, &commit));

    // 1. The copy is the pinned one, when the channel pins.
    if let Some(pinned) = &pick.sha256 {
        let digest = sha256_of(&tarball).map_err(|e| verify_failed(format!("tarball: {e}")))?;
        if digest != *pinned {
            return Err(verify_failed("the tarball's sha256 is not the pinned one"));
        }
    }

    // 2. The copy was built by the workflow, for the commit.
    if !home.exists() {
        make_home(&home, nobody)?;
    }
    set_mode(&stage, 0o711)?;
    attest(
        host, runner, channel, &commit, &tarball, &bundle, nobody, &home,
    )?;
    set_mode(&stage, 0o700)?;

    // 3. The binary.
    let binary = stage.join(BINARY);
    unpack(&tarball, &binary, tuning.binary_max)
        .map_err(|e| verify_failed(format!("tarball: {e}")))?;
    is_macho_arm64(&binary).map_err(|e| verify_failed(format!("{BINARY}: {e}")))?;
    let binary_sha256 = sha256_of(&binary).map_err(|e| verify_failed(format!("{BINARY}: {e}")))?;

    // 4 and 5. Not quarantined, and signed with the hardened runtime.
    strip_quarantine(host, runner, &binary).map_err(verify_failed)?;
    codesign(host, runner, &binary).map_err(verify_failed)?;

    // 6. The daemon's user accepts the live configuration with the new binary.
    set_mode(&stage, 0o711)?;
    let check = tools::config_check(
        &binary,
        &host.daemon_config,
        &host.daemon_home,
        daemon,
        tuning.tool,
    );
    run_ok(runner, &check, "kbf --check").map_err(verify_failed)?;

    set_mode(&stage, 0o700)?;
    // The file that will run is root's, in a tree only root can write.
    host.trust
        .check_file(&binary)
        .map_err(|e| verify_failed(format!("{BINARY} in the stage: {e}")))?;
    Ok(Staged {
        commit,
        binary_sha256,
    })
}

/// The release a version names in `channel`: from the pinned sums when it has some, else
/// from the file names in its directory, or, for a channel on GitHub with no sums, as the
/// 12 digits alone (the listing of the releases decides, [`fetch`]).
fn pick_release(host: &MacHost, channel: &MacChannel, prefix: &str) -> Result<Pick, Outcome> {
    let github = matches!(channel.source, Source::Github { .. });
    let Some(sums_file) = &channel.sums else {
        let Source::Dir(dir) = &channel.source else {
            return Ok(Pick {
                want: prefix.to_owned(),
                sha256: None,
            });
        };
        let listing =
            list_names(dir).map_err(|e| verify_failed(format!("the channel's directory: {e}")))?;
        return release::pick_named(listing.iter().map(String::as_str), prefix)
            .map(|commit| Pick {
                want: commit,
                sha256: None,
            })
            .map_err(|e| pick_failed(e, "the channel's directory", false));
    };
    let sums = host
        .trust
        .read(sums_file)
        .map_err(|e| Outcome::Refused(refusal(e)))?;
    release::pick(&sums, prefix)
        .map(|pick| Pick {
            want: pick.commit,
            sha256: Some(pick.sha256),
        })
        .map_err(|e| pick_failed(e, "the channel's sums", github))
}

/// Why no release was picked, as an outcome. A channel on GitHub refuses with the closed
/// codes of an unknown and an ambiguous release.
fn pick_failed(e: PickError, whence: &str, github: bool) -> Outcome {
    use crate::outcome::Refusal;
    match (e, github) {
        (PickError::Malformed, _) => Outcome::Refused(Refusal::ConfigInvalid),
        (PickError::Unknown, true) => Outcome::Refused(Refusal::UnknownRelease),
        (PickError::Ambiguous, true) => Outcome::Refused(Refusal::AmbiguousRelease),
        (PickError::Unknown, false) => verify_failed(format!(
            "unknown release: {whence} hold none of that commit"
        )),
        (PickError::Ambiguous, false) => verify_failed(format!(
            "ambiguous release: {whence} hold two of that commit"
        )),
    }
}

/// The longest token a channel's file may hold. GitHub's workflow tokens are about 520 bytes
/// (`ghs_`, an app id, and a signed token in base64url with dots), and it asks that a token be
/// treated as opaque and its length not assumed.
const MAX_TOKEN_BYTES: usize = 1024;

/// A mark in the binary that its channel token file takes GitHub's Actions token. The update
/// rehearsal, which runs the helper of the release before this one, reads the mark to know
/// whether to give that helper the run's token.
#[used]
static LONG_TOKEN_MARKER: &[u8] = crate::LONG_TOKEN_MARKER;

/// The token for the API in `file`, which is root's alone to write: one line of letters,
/// digits, `_`, `-` and `.`, at most [`MAX_TOKEN_BYTES`] bytes.
pub(crate) fn read_token(trust: &Trust, file: &Path) -> Result<String, crate::outcome::Refusal> {
    let text = trust.read(file).map_err(refusal)?;
    let token = text.trim();
    let plain = !token.is_empty()
        && token.len() <= MAX_TOKEN_BYTES
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    if plain {
        Ok(token.to_owned())
    } else {
        Err(crate::outcome::Refusal::ConfigInvalid)
    }
}

/// A home for `gh` that `who` owns, inside the stage.
fn make_home(home: &Path, who: Identity) -> Result<(), Outcome> {
    fs::create_dir(home)
        .and_then(|()| fs::set_permissions(home, fs::Permissions::from_mode(0o700)))
        .and_then(|()| chown(home, Some(who.uid), Some(who.gid)))
        .map_err(|e| fail(FailureCode::InstallFailed, format!("stage: {e}")))
}

/// Fetch the release that starts with `want` from GitHub as `nobody`, into a directory
/// `nobody` owns, then copy its two files once, as root, into the stage. Nothing but
/// the closed refusals comes out: the fetch changes nothing outside the stage.
fn fetch_release(
    (host, runner, origin): (&MacHost, &dyn Runner, &Origin),
    want: &str,
    token: Option<&str>,
    nobody: Identity,
    home: &Path,
) -> Result<String, Outcome> {
    use crate::outcome::Refusal;
    let stage = host.stage();
    let scratch = stage.join("fetch");
    make_home(&scratch, nobody)?;
    let tuning = &host.tuning;
    let fetcher = Fetcher {
        origin,
        platform: release::PLATFORM,
        asset: AssetKind::Tarball,
        runner,
        curl: &host.curl,
        gh: &host.gh,
        who: nobody,
        home,
        token,
        limits: Limits {
            tool: tuning.tool,
            download: tuning.fetch,
            tarball_max: tuning.tarball_max,
            bundle_max: tuning.bundle_max,
        },
    };
    // The account `nobody` can pass through the stage while it fetches, and not after.
    set_mode(&stage, 0o711)?;
    let fetched = fetcher.fetch(want, &scratch);
    set_mode(&stage, 0o700)?;
    let commit = fetched.map_err(|e| match e {
        FetchError::Unknown => Outcome::Refused(Refusal::UnknownRelease),
        FetchError::Ambiguous => Outcome::Refused(Refusal::AmbiguousRelease),
        FetchError::Failed(why) | FetchError::RateLimited(why) => {
            Outcome::RefusedBecause(Refusal::FetchFailed, why)
        }
    })?;
    let fetch_failed = |_: String| Outcome::Refused(Refusal::FetchFailed);
    copy_in(
        &scratch.join(tarball_of(origin, &commit)),
        &stage.join(tarball_of(origin, &commit)),
        tuning.tarball_max,
    )
    .map_err(fetch_failed)?;
    copy_in(
        &scratch.join(bundle_of(origin, &commit)),
        &stage.join(bundle_of(origin, &commit)),
        tuning.bundle_max,
    )
    .map_err(fetch_failed)?;
    // What `nobody` wrote is no longer needed. The stage is sealed and root's, `nobody` cannot
    // reach what stays, and the apply job removes the whole stage.
    let _ = fs::remove_dir_all(&scratch);
    Ok(commit)
}

/// The names (UTF-8 ones) in the untrusted directory `dir`. Only names are read here.
fn list_names(dir: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        if let Ok(name) = entry?.file_name().into_string() {
            names.push(name);
        }
    }
    Ok(names)
}

fn set_mode(dir: &Path, mode: u32) -> Result<(), Outcome> {
    fs::set_permissions(dir, fs::Permissions::from_mode(mode))
        .map_err(|e| fail(FailureCode::InstallFailed, format!("stage: {e}")))
}

/// A fresh stage: root's, 0700, whatever an earlier run left removed.
fn make_stage(stage: &Path) -> io::Result<()> {
    match fs::remove_dir_all(stage) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    fs::create_dir(stage)?;
    fs::set_permissions(stage, fs::Permissions::from_mode(0o700))
}

/// The ids of the account `name`.
fn lookup(host: &MacHost, runner: &dyn Runner, name: &str) -> Result<Identity, Outcome> {
    let exec =
        run_ok(runner, &tools::dscl_ids(name, host.tuning.tool), "dscl").map_err(verify_failed)?;
    tools::parse_ids(&exec.stdout_text())
        .filter(|id| id.uid != 0 && id.gid != 0)
        .ok_or_else(|| verify_failed(format!("the account {name} has no usable ids")))
}

/// Run `cmd`; its output when it ran and exited 0, else why not.
pub fn run_ok(runner: &dyn Runner, cmd: &crate::runner::Cmd, what: &str) -> Result<Exec, String> {
    let exec = runner.run(cmd).map_err(|e| format!("{what}: {e}"))?;
    if exec.success() {
        Ok(exec)
    } else {
        Err(format!(
            "{what}: exit status {:?}: {}",
            exec.code,
            exec.stderr_text()
        ))
    }
}

/// Copy the untrusted file `src` to the new file `dst` through the open descriptor: a
/// regular file with one link, at most `cap` bytes, not followed through a symbolic link.
/// (A file that grew since it was looked at is cut at `cap`; the hash then fails.)
fn copy_in(src: &Path, dst: &Path, cap: u64) -> Result<(), String> {
    copy_file(src, dst, cap).map_err(|e| e.to_string())
}

fn copy_file(src: &Path, dst: &Path, cap: u64) -> io::Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nofollow())
        .open(src)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(io::Error::other("not a regular file with one link"));
    }
    if meta.len() > cap {
        return Err(io::Error::other(format!("larger than {cap} bytes")));
    }
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(dst)?;
    io::copy(&mut file.take(cap), &mut out).map(drop)
}

/// The sha256 of the file at `path`, as 64 lower-case hex digits.
pub fn sha256_of(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// `gh attestation verify` for each signer the host accepts; one accepting is enough.
#[allow(clippy::too_many_arguments)]
fn attest(
    host: &MacHost,
    runner: &dyn Runner,
    channel: &MacChannel,
    commit: &str,
    tarball: &Path,
    bundle: &Path,
    who: Identity,
    home: &Path,
) -> Result<(), Outcome> {
    let mut why = String::new();
    for (repo, workflow) in channel.origin.signers() {
        let attest = tools::Attest {
            gh: &host.gh,
            tarball,
            bundle,
            trusted_root: &host.trusted_root,
            repo,
            workflow,
            source_ref: &channel.source_ref,
            commit,
            who,
            home,
        };
        match run_ok(
            runner,
            &attest.cmd(host.tuning.tool),
            "gh attestation verify",
        ) {
            Ok(_) => return Ok(()),
            Err(e) => why = e,
        }
    }
    Err(verify_failed(format!("not attested: {why}")))
}

/// Unpack the one member of the gzipped tar at `tarball` to the new file `dst`.
fn unpack(tarball: &Path, dst: &Path, cap: u64) -> Result<(), String> {
    unpack_member(tarball, dst, cap).map_err(|e| e.to_string())
}

fn unpack_member(tarball: &Path, dst: &Path, cap: u64) -> io::Result<()> {
    let bad = |why: String| Err(io::Error::other(why));
    let mut archive = tar::Archive::new(GzDecoder::new(File::open(tarball)?));
    let mut entries = archive.entries()?;
    let Some(entry) = entries.next() else {
        return bad("holds no file".to_owned());
    };
    let mut entry = entry?;
    if entry.header().entry_type() != tar::EntryType::Regular {
        return bad("the member is not a regular file".to_owned());
    }
    if entry.path_bytes().as_ref() != BINARY.as_bytes() {
        return bad(format!("the member is not named {BINARY}"));
    }
    let size = entry.size();
    if size > cap {
        return bad(format!("the member is larger than {cap} bytes"));
    }
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(dst)?;
    io::copy(&mut (&mut entry).take(size), &mut out)?;
    drop(entry);
    if entries.next().is_some() {
        return bad("holds more than one member".to_owned());
    }
    Ok(())
}

/// Whether the file at `path` starts as a 64-bit arm64 Mach-O executable.
fn is_macho_arm64(path: &Path) -> Result<(), String> {
    let mut head = [0_u8; 8];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut head))
        .map_err(|e| e.to_string())?;
    if head == MACHO_ARM64 {
        Ok(())
    } else {
        Err("not an arm64 Mach-O file".to_owned())
    }
}

/// Remove the quarantine attribute of `path` if it has one, and prove it is gone. Used on
/// the staged binary and on the one laid out in `releases/`.
pub fn strip_quarantine(host: &MacHost, runner: &dyn Runner, path: &Path) -> Result<(), String> {
    let limit = host.tuning.tool;
    let listing = run_ok(runner, &tools::xattr_list(path, limit), "xattr")?;
    if !tools::has_quarantine(&listing.stdout_text()) {
        return Ok(());
    }
    run_ok(
        runner,
        &tools::xattr_remove_quarantine(path, limit),
        "xattr -d",
    )?;
    let again = run_ok(runner, &tools::xattr_list(path, limit), "xattr")?;
    if tools::has_quarantine(&again.stdout_text()) {
        return Err(format!("{} stays on {BINARY}", tools::QUARANTINE));
    }
    Ok(())
}

/// The signature is valid and has the hardened runtime. `codesign -d` describes on stderr;
/// both outputs are read whole and matched here.
fn codesign(host: &MacHost, runner: &dyn Runner, path: &Path) -> Result<(), String> {
    let limit = host.tuning.tool;
    run_ok(
        runner,
        &tools::codesign_verify(path, limit),
        "codesign --verify",
    )?;
    let described = run_ok(
        runner,
        &tools::codesign_describe(path, limit),
        "codesign -d",
    )?;
    let text = format!("{}\n{}", described.stdout_text(), described.stderr_text());
    if tools::has_hardened_runtime(&text) {
        Ok(())
    } else {
        Err("the signature has no hardened runtime".to_owned())
    }
}
