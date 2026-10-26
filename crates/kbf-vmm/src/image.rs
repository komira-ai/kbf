//! `image base`: builds the base bundle the probes clone, from a macOS restore image.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::bundle::{self, Recorded, Sizing};
use crate::guest;
use crate::report::Report;
use crate::system::System;

/// Where a restore image comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Path(PathBuf),
    Url(String),
}

/// `https://...` is a download; anything else is a path.
#[must_use]
pub fn parse_source(s: &str) -> Source {
    if s.starts_with("https://") || s.starts_with("http://") {
        Source::Url(s.to_owned())
    } else {
        Source::Path(PathBuf::from(s))
    }
}

/// Why an image could not be built.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ImageError {
    #[error("--sha256 must be 64 hexadecimal digits, got {0:?}")]
    BadDigest(String),
    #[error("refusing {path}: its SHA-256 is {actual}, not the {expected} that was pinned")]
    Mismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("{0}")]
    Io(String),
}

/// The lower-case hex SHA-256 of the file at `path`.
///
/// # Errors
/// The file could not be read.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Normalizes a pinned digest (64 hex digits, any case).
///
/// # Errors
/// It is not 64 hexadecimal digits.
pub fn parse_digest(s: &str) -> Result<String, ImageError> {
    if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(s.to_ascii_lowercase())
    } else {
        Err(ImageError::BadDigest(s.to_owned()))
    }
}

/// Checks the file at `path` against the pinned digest.
///
/// # Errors
/// The file cannot be read or its digest differs.
pub fn verify(path: &Path, expected: &str) -> Result<String, ImageError> {
    let actual =
        sha256_file(path).map_err(|e| ImageError::Io(format!("read {}: {e}", path.display())))?;
    if actual == expected {
        Ok(actual)
    } else {
        Err(ImageError::Mismatch {
            path: path.to_owned(),
            expected: expected.to_owned(),
            actual,
        })
    }
}

/// What `image base` was asked.
#[derive(Debug, Clone)]
pub struct BaseArgs {
    pub ipsw: String,
    pub sha256: String,
    pub out: PathBuf,
    pub disk_gib: u64,
    /// Guest CPUs; `None` keeps what the restore image needs.
    pub cpus: Option<u32>,
    /// Guest memory in MiB; `None` keeps what the restore image needs.
    pub memory_mib: Option<u64>,
    /// After the install, write the probe launch daemons and the Setup Assistant marker
    /// into the image as root.
    pub offline_setup: bool,
    /// The `kbf-guest` the offline setup copies onto the guest's disk (required with
    /// `offline_setup`).
    pub guest_binary: Option<PathBuf>,
}

/// The curl flags of a restore image download. `-q` first, so a `.curlrc` of the invoking
/// user (root, usually) is not read; https only, also across redirects; a size and a time
/// cap, so a hostile or broken server cannot fill the disk or hang the run before the
/// digest check.
pub const FETCH_FLAGS: [&str; 15] = [
    "-q",
    "--fail",
    "--location",
    "--silent",
    "--show-error",
    "--proto",
    "=https",
    "--proto-redir",
    "=https",
    "--connect-timeout",
    "30",
    "--max-time",
    "7200",
    "--max-filesize",
    "40G",
];

/// The restore image's name inside the new bundle directory.
pub const STAGED: &str = "restore.ipsw";

/// Copies `src` to the new file `dst` (`O_EXCL`, mode 0600) and returns the SHA-256 of
/// the bytes copied. The installer is handed `dst`, never `src`, so the file that was
/// hashed is the file that is installed even if `src` changes after this returns. `src` is
/// opened without following a link and without blocking on a pipe, and the open file must be
/// a regular file.
///
/// # Errors
/// `src` cannot be read or is not a regular file, or `dst` cannot be written.
pub fn stage(src: &Path, dst: &Path) -> Result<String, ImageError> {
    copy_hashed(src, dst, None)
}

/// [`stage`] for a file of a known size: at most `bytes` are copied (one more is read, to
/// see that the file grew), and a file that grew or shrank is an error.
///
/// # Errors
/// As [`stage`], or the file is not `bytes` long.
pub(crate) fn stage_exact(src: &Path, dst: &Path, bytes: u64) -> Result<String, ImageError> {
    copy_hashed(src, dst, Some(bytes))
}

fn copy_hashed(src: &Path, dst: &Path, planned: Option<u64>) -> Result<String, ImageError> {
    let io_err =
        |what: &str, p: &Path, e: io::Error| ImageError::Io(format!("{what} {}: {e}", p.display()));
    let from = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(src)
        .map_err(|e| io_err("read", src, e))?;
    if !from
        .metadata()
        .map_err(|e| io_err("read", src, e))?
        .is_file()
    {
        return Err(ImageError::Io(format!(
            "read {}: not a regular file",
            src.display()
        )));
    }
    let mut to = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dst)
        .map_err(|e| io_err("create", dst, e))?;
    let mut from = from.take(planned.map_or(u64::MAX, |b| b.saturating_add(1)));
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = from
            .read(&mut buf)
            .and_then(|n| to.write_all(&buf[..n]).map(|()| n))
            .map_err(|e| io_err("copy", src, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    if let Some(bytes) = planned.filter(|b| *b != total) {
        return Err(ImageError::Io(format!(
            "{} changed while it was copied ({bytes} bytes planned, {})",
            src.display(),
            if total > bytes {
                "it grew past that".to_owned()
            } else {
                format!("{total} copied")
            }
        )));
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Gets the restore image into `staged` and returns its SHA-256: a download is hashed
/// where it landed, a local file is copied while it is hashed.
fn obtain(sys: &dyn System, source: &Source, staged: &Path) -> Result<String, String> {
    match source {
        Source::Path(p) => stage(p, staged).map_err(|e| e.to_string()),
        Source::Url(url) => {
            if !url.starts_with("https://") {
                return Err(format!(
                    "{url}: only https:// downloads are accepted; download the file and pass its path"
                ));
            }
            sys.fetch(url, staged)
                .map_err(|e| format!("download {url}: {e}"))?;
            sha256_file(staged).map_err(|e| format!("read {}: {e}", staged.display()))
        }
    }
}

/// `image base`. The new bundle directory is made first and holds a private copy of the
/// restore image; the copy is checked against the pin before the installer sees it, and
/// the directory is removed on any failure up to and including the install. Nothing the
/// pin refuses is installed.
#[must_use]
pub fn base(sys: &dyn System, args: &BaseArgs) -> Report {
    let probe = "image-base";
    let expected = match parse_digest(&args.sha256) {
        Ok(d) => d,
        Err(e) => return Report::error(probe, e.to_string()),
    };
    if args.out.exists() {
        return Report::error(
            probe,
            format!("{} already exists; pick a new --out", args.out.display()),
        );
    }
    // `create_dir` fails on a directory another process made since the check above, so
    // the removals below only ever remove what this call made.
    let made = args
        .out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::create_dir(&args.out));
    if let Err(e) = made {
        return Report::error(probe, format!("create {}: {e}", args.out.display()));
    }
    let staged = args.out.join(STAGED);
    let fail = |message: String| {
        let _ = std::fs::remove_dir_all(&args.out);
        Report::error(probe, message)
    };
    let actual = match obtain(sys, &parse_source(&args.ipsw), &staged) {
        Ok(a) => a,
        Err(e) => return fail(e),
    };
    if actual != expected {
        return fail(
            ImageError::Mismatch {
                path: PathBuf::from(&args.ipsw),
                expected,
                actual,
            }
            .to_string(),
        );
    }
    let sizing = Sizing {
        disk_gib: args.disk_gib,
        cpus: args.cpus,
        memory_mib: args.memory_mib,
    };
    let installed = match sys.install_image(&staged, &args.out, &sizing) {
        Ok(v) => v,
        Err(e) => return fail(format!("install: {e}")),
    };
    let _ = std::fs::remove_file(&staged);
    // What the installer settled on (the flags, or the restore image's needs), for the
    // runs that clone this base.
    let recorded = installed["cpus"]
        .as_u64()
        .and_then(|c| u32::try_from(c).ok())
        .zip(installed["memory_mib"].as_u64())
        .map(|(cpus, memory_mib)| Recorded {
            cpus,
            memory_mib,
            disk_gib: args.disk_gib,
        });
    // A failed write does not throw away an install that took an hour; it is reported.
    let sizing_error = recorded
        .and_then(|r| bundle::record_sizing(&args.out, &r).err())
        .map(|e| format!("write {}: {e}", bundle::SIZING));
    let offline = args.offline_setup.then(|| {
        let binary = args
            .guest_binary
            .as_deref()
            .ok_or_else(|| guest::NEEDS_GUEST_BINARY.to_owned())?;
        sys.offline_write(
            &args.out.join(crate::bundle::DISK),
            &guest::offline_payload_from(binary)?,
        )
    });
    let (offline_json, failed) = match offline {
        None => (json!(null), None),
        Some(Ok(v)) => (v, None),
        Some(Err(e)) => (json!({"failure": e}), Some(e)),
    };
    let details = json!({
        "bundle": args.out,
        "restore_image_sha256": actual,
        "install": installed,
        "sizing_record_error": sizing_error,
        "offline_setup": offline_json,
    });
    match failed {
        None => Report::ok(
            probe,
            format!("base bundle written to {}", args.out.display()),
            details,
        ),
        Some(e) => Report::needs_manual(
            probe,
            format!(
                "the bundle installed, but the offline setup (P0) failed: {e}; run `kbf-vmm image setup --base {}` as root to try again",
                args.out.display()
            ),
            details,
        ),
    }
}

/// What `image setup` was asked.
#[derive(Debug, Clone, Default)]
pub struct SetupArgs {
    /// The `kbf-guest` for macOS the offline write copies onto the guest's disk.
    pub guest_binary: Option<PathBuf>,
    /// An Xcode.app to clone into the guest's `/Applications`.
    pub xcode: Option<PathBuf>,
    /// Simulator runtime disk images to stage at the fixed guest path.
    pub sim_runtimes: Vec<PathBuf>,
    /// An app bundle (`<name>.app`, built for the simulator) to stage for the simulator
    /// probe to install and launch.
    pub sim_app: Option<PathBuf>,
    /// Copy Xcode symbolic links that leave the app, as links.
    pub allow_external_links: bool,
}

/// `image setup`: the offline write (P0) into an existing base bundle, and the staging of
/// Xcode and simulator runtimes when they are named. The staging plan is made, and its
/// refusals are raised, before anything is written to the disk.
#[must_use]
pub fn setup(sys: &dyn System, base: &Path, args: &SetupArgs) -> Report {
    let probe = "image-setup";
    if sys.uid() != 0 {
        return Report::needs_manual(
            probe,
            "needs root: attaching the guest disk image and writing root-owned files into its Data volume; run with sudo",
            json!({"uid": sys.uid()}),
        );
    }
    if let Err(e) = crate::bundle::check(base) {
        return Report::error(probe, e.to_string());
    }
    let staging = match crate::stage::plan_with_app(
        args.xcode.as_deref(),
        &args.sim_runtimes,
        args.sim_app.as_deref(),
        args.allow_external_links,
    ) {
        Ok(p) => p,
        Err(e) => return Report::error(probe, e.to_string()),
    };
    if args.guest_binary.is_none() && staging.is_empty() {
        return Report::needs_manual(probe, guest::NEEDS_GUEST_BINARY, json!({"bundle": base}));
    }
    let disk = base.join(crate::bundle::DISK);
    // The simulator probe's daemon goes in with whatever else is written, and only when
    // something is staged for it to use.
    let mut files = Vec::new();
    if let Some(binary) = args.guest_binary.as_deref() {
        match guest::offline_payload_from(binary) {
            Ok(p) => files.extend(p),
            Err(e) => return Report::error(probe, e),
        }
    }
    if !staging.is_empty() {
        files.extend(guest::sim_payload());
    }
    // At least one of the two was given: the check above refused the rest.
    let written = match sys.offline_write(&disk, &files) {
        Ok(observed) => observed,
        Err(e) => return Report::error(probe, format!("offline setup: {e}")),
    };
    let staged = if staging.is_empty() {
        None
    } else {
        match sys.offline_stage(&disk, &staging) {
            Ok(observed) => Some(observed),
            Err(e) => return Report::error(probe, format!("staging: {e}")),
        }
    };
    let what = match (args.guest_binary.is_some(), staged.is_some()) {
        (true, true) => "the probe launch daemons and the staged Xcode inputs are",
        (false, true) => "the simulator launch daemon and the staged Xcode inputs are",
        _ => "the probe launch daemons are",
    };
    Report::ok(
        probe,
        format!("{what} in {}", base.display()),
        json!({"bundle": base, "offline_setup": written, "staged": staged}),
    )
}

/// `image harvest`: copies the allowlisted diagnostics out of a kept clone's disk into the
/// new directory `out`, with the disk attached read-only.
#[must_use]
pub fn harvest(sys: &dyn System, bundle: &Path, out: &Path) -> Report {
    let probe = "image-harvest";
    if sys.uid() != 0 {
        return Report::needs_manual(
            probe,
            "needs root: attaching a guest disk image with its ownership honoured; run with sudo",
            json!({"uid": sys.uid()}),
        );
    }
    if let Err(e) = crate::bundle::check(bundle) {
        return Report::error(probe, e.to_string());
    }
    if std::fs::symlink_metadata(out).is_ok() {
        return Report::error(
            probe,
            format!("{} exists; harvest writes a new directory", out.display()),
        );
    }
    match sys.offline_harvest(&bundle.join(crate::bundle::DISK), out) {
        Ok(observed) => Report::ok(
            probe,
            format!(
                "the allowlisted files of {} are in {}",
                bundle.display(),
                out.display()
            ),
            json!({"bundle": bundle, "out": out, "offline_read": observed}),
        ),
        Err(e) => Report::error(probe, format!("harvest: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeSystem, holds, tmp};
    use crate::report::Status;

    /// The SHA-256 of the bytes `hello`.
    const HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn args(dir: &Path, ipsw: &str, sha: &str) -> BaseArgs {
        BaseArgs {
            ipsw: ipsw.to_owned(),
            sha256: sha.to_owned(),
            out: dir.join("out"),
            disk_gib: 64,
            cpus: None,
            memory_mib: None,
            offline_setup: false,
            guest_binary: None,
        }
    }

    fn with_binary(binary: Option<&Path>) -> SetupArgs {
        SetupArgs {
            guest_binary: binary.map(Path::to_owned),
            ..SetupArgs::default()
        }
    }

    fn ipsw_file(dir: &Path) -> String {
        let p = dir.join("r.ipsw");
        std::fs::write(&p, "hello").expect("ipsw");
        p.to_string_lossy().into_owned()
    }

    /// Catches a URL taken for a path, or a path for a URL.
    #[test]
    fn a_source_is_a_url_or_a_path() {
        assert_eq!(
            parse_source("https://x/y.ipsw"),
            Source::Url("https://x/y.ipsw".into())
        );
        assert_eq!(
            parse_source("http://x/y.ipsw"),
            Source::Url("http://x/y.ipsw".into())
        );
        assert_eq!(parse_source("/a/b.ipsw"), Source::Path("/a/b.ipsw".into()));
        assert_eq!(
            parse_source("httpsfoo.ipsw"),
            Source::Path("httpsfoo.ipsw".into())
        );
    }

    /// Catches a digest check that accepts a short string, a non-hex string or one of the
    /// right length with a stray character, and one that does not lower-case.
    #[test]
    fn a_pinned_digest_is_64_hex_digits() {
        assert_eq!(parse_digest(&HELLO.to_uppercase()), Ok(HELLO.to_owned()));
        for bad in ["", "abc", &"g".repeat(64), &format!("{HELLO}0")] {
            assert_eq!(
                parse_digest(bad),
                Err(ImageError::BadDigest(bad.to_owned()))
            );
        }
    }

    /// Catches a hash of the wrong bytes, and a file larger than one read chunk hashed
    /// from its first chunk only.
    #[test]
    fn the_file_digest_covers_every_byte() {
        let dir = tmp("image-sha");
        let p = dir.join("f");
        std::fs::write(&p, "hello").expect("f");
        assert_eq!(sha256_file(&p).expect("sha"), HELLO);
        let big = dir.join("big");
        std::fs::write(&big, vec![7u8; (1 << 20) + 5]).expect("big");
        let mut h = Sha256::new();
        h.update(vec![7u8; (1 << 20) + 5]);
        assert_eq!(sha256_file(&big).expect("sha"), hex::encode(h.finalize()));
        assert!(sha256_file(&dir.join("missing")).is_err());
    }

    /// Catches a verify that accepts a different digest, and a refusal that does not say
    /// both digests.
    #[test]
    fn a_different_digest_is_refused_with_both_digests() {
        let dir = tmp("image-verify");
        let p = dir.join("f");
        std::fs::write(&p, "hello").expect("f");
        assert_eq!(verify(&p, HELLO), Ok(HELLO.to_owned()));
        let wrong = "0".repeat(64);
        let err = verify(&p, &wrong).expect_err("mismatch").to_string();
        assert!(
            holds(&[
                err.contains(HELLO),
                err.contains(&wrong),
                err.contains("refusing")
            ]),
            "{err}"
        );
        assert!(matches!(
            verify(&dir.join("none"), HELLO),
            Err(ImageError::Io(_))
        ));
    }

    /// Catches an install from a file whose digest differs from the pin: nothing may be
    /// created and the installer must not run.
    #[test]
    fn a_sha256_mismatch_is_refused_before_anything_is_created() {
        let dir = tmp("image-mismatch");
        let sys = FakeSystem::new();
        let r = base(&sys, &args(&dir, &ipsw_file(&dir), &"1".repeat(64)));
        assert_eq!(r.status, Status::Error);
        assert!(r.reason.expect("reason").contains("refusing"));
        assert!(sys.installs.borrow().is_empty());
        assert!(!dir.join("out").exists());
    }

    /// Catches a build that skips the digest argument check, or overwrites a directory.
    #[test]
    fn a_bad_digest_or_an_existing_output_is_refused() {
        let dir = tmp("image-args");
        let sys = FakeSystem::new();
        let r = base(&sys, &args(&dir, &ipsw_file(&dir), "xyz"));
        assert!(r.reason.expect("reason").contains("64 hexadecimal"));
        std::fs::create_dir(dir.join("out")).expect("out");
        let r = base(&sys, &args(&dir, &ipsw_file(&dir), HELLO));
        assert!(r.reason.expect("reason").contains("already exists"));
        assert!(sys.installs.borrow().is_empty());
        let r = base(
            &sys,
            &args(&dir, &dir.join("absent.ipsw").to_string_lossy(), HELLO).tap_out(&dir, "o2"),
        );
        assert!(r.reason.expect("reason").contains("read"));
    }

    impl BaseArgs {
        fn tap_out(mut self, dir: &Path, name: &str) -> Self {
            self.out = dir.join(name);
            self
        }
    }

    /// Catches a matching file that is not installed, or one installed with the wrong
    /// size, and an offline write nobody asked for.
    #[test]
    fn a_pinned_file_is_installed() {
        let dir = tmp("image-ok");
        let sys = FakeSystem::new();
        let r = base(&sys, &args(&dir, &ipsw_file(&dir), HELLO));
        assert_eq!(r.status, Status::Ok, "{r:?}");
        assert_eq!(r.details["restore_image_sha256"], HELLO);
        assert_eq!(r.details["install"]["macos_version"], "27.0.0");
        assert!(r.details["offline_setup"].is_null());
        let installs = sys.installs.borrow();
        assert_eq!((installs.len(), installs[0].2.disk_gib), (1, 64));
        assert!(sys.offline_calls.borrow().is_empty());
    }

    /// Catches a failure to write the size record throwing away an install that took an
    /// hour, or going unreported: the bundle stays and the details say what failed.
    #[test]
    fn a_size_record_that_cannot_be_written_is_reported_and_the_bundle_stays() {
        let dir = tmp("image-sizing-fail");
        let sys = FakeSystem::new();
        // A directory where the record goes, so that writing the file fails whoever runs this.
        *sys.after_install.borrow_mut() = Some(Box::new(|out| {
            std::fs::create_dir(out.join("sizing.json")).expect("dir")
        }));
        let r = base(&sys, &args(&dir, &ipsw_file(&dir), HELLO));
        assert_eq!(r.status, Status::Ok, "{r:?}");
        let e = r.details["sizing_record_error"].as_str().expect("error");
        assert!(e.starts_with("write sizing.json: "), "{e}");
        assert!(dir.join("out").exists());
    }

    /// Catches an installer failure reported as success.
    #[test]
    fn an_install_failure_is_an_error() {
        let dir = tmp("image-install-fail");
        let sys = FakeSystem::new();
        *sys.install.borrow_mut() = Err("no space".into());
        let r = base(&sys, &args(&dir, &ipsw_file(&dir), HELLO));
        assert_eq!(r.status, Status::Error);
        assert!(r.reason.expect("reason").contains("install: no space"));
        // The directory this call made, with the staged copy, is gone: a rerun is not
        // refused for "already exists".
        assert!(!dir.join("out").exists());
    }

    /// Catches an offline write that goes to the wrong disk or whose failure hides the
    /// installed bundle: success is reported in the details, failure is needs-manual.
    #[test]
    fn the_offline_setup_runs_on_the_new_disk_and_its_failure_is_needs_manual() {
        let dir = tmp("image-offline");
        let sys = FakeSystem::new();
        let mut a = args(&dir, &ipsw_file(&dir), HELLO);
        a.offline_setup = true;
        a.guest_binary = Some(dir.join("kbf-guest-src"));
        std::fs::write(dir.join("kbf-guest-src"), "bin").expect("bin");
        let r = base(&sys, &a);
        assert_eq!(r.status, Status::Ok);
        assert_eq!(r.details["offline_setup"]["written"], 5);
        assert!(
            sys.offline_calls.borrow()[0]
                .1
                .contains(&"usr/local/libexec/kbf-guest".to_owned())
        );
        assert_eq!(
            sys.offline_calls.borrow()[0].0,
            dir.join("out").join("disk.img")
        );
        let dir2 = tmp("image-offline2");
        *sys.offline.borrow_mut() = Err("hdiutil said no".into());
        let mut a = args(&dir2, &ipsw_file(&dir2), HELLO);
        a.offline_setup = true;
        a.guest_binary = Some(dir.join("kbf-guest-src"));
        let r = base(&sys, &a);
        assert_eq!(r.status, Status::NeedsManual);
        assert!(r.reason.expect("reason").contains("hdiutil said no"));
        assert_eq!(r.details["offline_setup"]["failure"], "hdiutil said no");
    }

    /// Catches an offline setup that writes daemons whose mounter has no binary to run
    /// (no guest binary named, or one that cannot be read): nothing is written and the
    /// report says to give `--guest-binary`.
    #[test]
    fn the_offline_setup_without_a_readable_guest_binary_writes_nothing() {
        let dir = tmp("image-offline-nobin");
        let sys = FakeSystem::new();
        let mut a = args(&dir, &ipsw_file(&dir), HELLO);
        a.offline_setup = true;
        let r = base(&sys, &a);
        assert_eq!(r.status, Status::NeedsManual);
        assert!(r.reason.expect("reason").contains("--guest-binary"));
        assert!(sys.offline_calls.borrow().is_empty());
        let dir2 = tmp("image-offline-nobin2");
        let mut a = args(&dir2, &ipsw_file(&dir2), HELLO);
        a.offline_setup = true;
        a.guest_binary = Some(dir2.join("missing"));
        let r = base(&sys, &a);
        assert_eq!(r.status, Status::NeedsManual);
        assert!(r.reason.expect("reason").contains("missing"));
        assert!(sys.offline_calls.borrow().is_empty());
    }

    /// Catches a download that is installed unverified, kept after a mismatch, or kept
    /// after the install (the restore image is large).
    #[test]
    fn a_downloaded_image_is_verified_and_removed() {
        let dir = tmp("image-url");
        let sys = FakeSystem::new();
        *sys.fetch_body.borrow_mut() = b"hello".to_vec();
        let r = base(&sys, &args(&dir, "https://example/r.ipsw", HELLO));
        assert_eq!(r.status, Status::Ok, "{r:?}");
        assert_eq!(sys.fetched.borrow()[0], "https://example/r.ipsw");
        assert!(!dir.join("out/restore.ipsw").exists());
        // The installer was handed the downloaded file.
        assert_eq!(sys.installs.borrow()[0].0, dir.join("out/restore.ipsw"));

        let dir = tmp("image-url-bad");
        let r = base(&sys, &args(&dir, "https://example/r.ipsw", &"2".repeat(64)));
        assert!(r.reason.expect("reason").contains("refusing"));
        assert!(!dir.join("out").exists());
    }

    /// Catches a failed download that leaves a half-made output directory, and an output
    /// that cannot be created going unreported.
    #[test]
    fn download_problems_leave_nothing_behind() {
        let dir = tmp("image-url-fail");
        let sys = FakeSystem::new();
        *sys.fetch.borrow_mut() = Err("404".into());
        let r = base(&sys, &args(&dir, "https://example/r.ipsw", HELLO));
        assert!(r.reason.expect("reason").contains("404"));
        assert!(!dir.join("out").exists());
        let file = dir.join("afile");
        std::fs::write(&file, "x").expect("file");
        let mut a = args(&dir, "https://example/r.ipsw", HELLO);
        a.out = file.join("sub");
        assert!(base(&sys, &a).reason.expect("reason").contains("create"));
    }

    /// Catches a copy that is not byte-for-byte what was hashed, one that overwrites a
    /// file, and an installer that is handed the caller's path instead of the private
    /// copy (the caller's file could change after the digest check).
    /// Catches a copy of a source that is a link (read as root from wherever it points) or a
    /// pipe (the copy would wait for a writer), and a size-checked copy of a file that grew or
    /// shrank.
    #[test]
    fn a_source_must_be_a_regular_file_and_a_planned_size_is_held() {
        let dir = tmp("image-stage-source");
        let src = dir.join("src.dmg");
        std::fs::write(&src, "hello").expect("src");
        let text = |r: Result<String, ImageError>| r.unwrap_err().to_string();
        let link = dir.join("link.dmg");
        std::os::unix::fs::symlink(&src, &link).expect("link");
        assert!(text(stage(&link, &dir.join("d1"))).contains("read"));
        let fifo = dir.join("fifo.dmg");
        let c = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(fifo.as_os_str()))
            .expect("path");
        // SAFETY: `c` is a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        assert!(text(stage(&fifo, &dir.join("d2"))).contains("not a regular file"));
        assert_eq!(stage_exact(&src, &dir.join("d3"), 5).expect("exact"), HELLO);
        let grew = text(stage_exact(&src, &dir.join("d4"), 3));
        assert!(
            grew.contains("(3 bytes planned, it grew past that)"),
            "{grew}"
        );
        assert_eq!(std::fs::metadata(dir.join("d4")).expect("d4").len(), 4);
        let shrank = text(stage_exact(&src, &dir.join("d5"), 9));
        assert!(shrank.contains("(9 bytes planned, 5 copied)"), "{shrank}");
    }

    #[test]
    fn the_installer_gets_a_private_copy_of_the_checked_bytes() {
        let dir = tmp("image-stage");
        let src = dir.join("src.ipsw");
        std::fs::write(&src, "hello").expect("src");
        let dst = dir.join("dst");
        assert_eq!(stage(&src, &dst).expect("stage"), HELLO);
        assert_eq!(std::fs::read(&dst).expect("dst"), b"hello");
        let mode = std::fs::metadata(&dst).expect("meta").permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
        let text = |r: Result<String, ImageError>| r.unwrap_err().to_string();
        assert!(text(stage(&src, &dst)).contains("create"));
        assert!(text(stage(&dir.join("no"), &dir.join("d2"))).contains("read"));
        // A directory opens but is not a regular file.
        assert!(text(stage(&dir, &dir.join("d3"))).contains("not a regular file"));

        let sys = FakeSystem::new();
        let a = args(&dir, &src.to_string_lossy(), HELLO);
        assert_eq!(base(&sys, &a).status, Status::Ok);
        assert_eq!(sys.installs.borrow()[0].0, dir.join("out").join(STAGED));
        assert!(
            !dir.join("out").join(STAGED).exists(),
            "the copy is removed after the install"
        );
    }

    /// Catches a plain-http download being accepted, which a network position could
    /// swap before the digest check, and a download that leaves its directory behind.
    #[test]
    fn only_https_downloads_are_accepted() {
        let dir = tmp("image-http");
        let sys = FakeSystem::new();
        let r = base(&sys, &args(&dir, "http://example/r.ipsw", HELLO));
        assert_eq!(r.status, Status::Error);
        assert!(r.reason.expect("reason").contains("only https://"));
        assert!(sys.fetched.borrow().is_empty());
        assert!(!dir.join("out").exists());
    }

    /// Catches a download command that follows a redirect off https, has no size or time
    /// cap, or reads the invoking user's curl configuration.
    #[test]
    fn the_download_is_https_only_capped_and_ignores_curlrc() {
        let f = FETCH_FLAGS;
        assert_eq!(f[0], "-q");
        let at = |flag: &str| f.iter().position(|x| *x == flag).expect(flag);
        assert_eq!(f[at("--proto") + 1], "=https");
        assert_eq!(f[at("--proto-redir") + 1], "=https");
        assert!(holds(&[
            f.contains(&"--max-filesize"),
            f.contains(&"--max-time"),
            f.contains(&"--connect-timeout"),
            f.contains(&"--fail"),
        ]));
    }

    /// Catches an `image setup` that writes a file the payload does not list (the root
    /// helper runs this verb for the daemon, so nothing outside `offline_payload()` may
    /// reach the guest's disk), a path that climbs out of the guest volume, or a payload
    /// that differs from the one `image base --offline-setup` writes.
    #[test]
    fn setup_writes_exactly_the_offline_payload_paths() {
        let work = crate::fake::work_with_base("image-setup-paths");
        let base_dir = work.join("base");
        let sys = FakeSystem::new();
        sys.uid.set(0);
        let bin = work.join("guest-src");
        std::fs::write(&bin, "bin").expect("bin");
        let r = setup(&sys, &base_dir, &with_binary(Some(&bin)));
        assert_eq!(r.status, Status::Ok);
        let calls = sys.offline_calls.borrow();
        assert_eq!(calls.len(), 1);
        let want: Vec<String> = guest::offline_payload(Some(b"bin".to_vec()))
            .into_iter()
            .map(|f| f.path)
            .collect();
        assert_eq!(calls[0].1, want);
        for path in &calls[0].1 {
            assert!(!path.starts_with('/'), "{path}");
            assert!(!path.split('/').any(|part| part == ".."), "{path}");
        }
    }

    /// Catches `image setup` writing as a non-root user, writing into a directory that
    /// is not a bundle, writing to the wrong disk, or hiding a write failure.
    #[test]
    fn setup_writes_the_payload_into_an_existing_base_as_root() {
        let work = crate::fake::work_with_base("image-setup");
        let base_dir = work.join("base");
        let sys = FakeSystem::new();
        let bin = work.join("guest-src");
        std::fs::write(&bin, "bin").expect("bin");
        let r = setup(&sys, &base_dir, &with_binary(Some(&bin)));
        assert_eq!(r.status, Status::NeedsManual);
        assert!(r.reason.expect("reason").contains("root"));
        assert!(sys.offline_calls.borrow().is_empty());
        sys.uid.set(0);
        let r = setup(&sys, &work.join("none"), &with_binary(Some(&bin)));
        assert_eq!(r.status, Status::Error);
        assert!(sys.offline_calls.borrow().is_empty());
        let r = setup(&sys, &base_dir, &with_binary(None));
        assert_eq!(r.status, Status::NeedsManual);
        assert!(r.reason.expect("reason").contains("--guest-binary"));
        let r = setup(&sys, &base_dir, &with_binary(Some(&work.join("missing"))));
        assert_eq!(r.status, Status::Error);
        assert!(sys.offline_calls.borrow().is_empty());
        let r = setup(&sys, &base_dir, &with_binary(Some(&bin)));
        assert_eq!(r.status, Status::Ok);
        assert_eq!(sys.offline_calls.borrow()[0].0, base_dir.join("disk.img"));
        *sys.offline.borrow_mut() = Err("no volume".into());
        let r = setup(&sys, &base_dir, &with_binary(Some(&bin)));
        assert_eq!(r.status, Status::Error);
        assert!(r.reason.expect("reason").contains("no volume"));
    }

    /// Catches `image harvest` running as a non-root user, on a directory that is not a
    /// bundle, into a directory that exists, against the wrong disk, or hiding a failure.
    #[test]
    fn harvest_reads_the_disk_of_a_bundle_as_root_into_a_new_directory() {
        let work = crate::fake::work_with_base("image-harvest");
        let bundle = work.join("base");
        let out = work.join("harvested");
        let sys = FakeSystem::new();
        let r = harvest(&sys, &bundle, &out);
        assert_eq!(r.status, Status::NeedsManual);
        assert!(r.reason.expect("reason").contains("root"));
        sys.uid.set(0);
        let r = harvest(&sys, &work.join("none"), &out);
        assert_eq!(r.status, Status::Error);
        std::fs::create_dir(&out).expect("out");
        let r = harvest(&sys, &bundle, &out);
        assert_eq!(r.status, Status::Error);
        assert!(r.reason.expect("reason").contains("exists"));
        assert!(sys.harvest_calls.borrow().is_empty());
        std::fs::remove_dir(&out).expect("rm");
        let r = harvest(&sys, &bundle, &out);
        assert_eq!(r.status, Status::Ok);
        assert_eq!(r.details["offline_read"]["copied"], serde_json::json!([]));
        assert_eq!(
            sys.harvest_calls.borrow()[0],
            (bundle.join("disk.img"), out.clone())
        );
        *sys.harvest.borrow_mut() = Err("no volume".into());
        let r = harvest(&sys, &bundle, &out);
        assert_eq!(r.status, Status::Error);
        assert!(r.reason.expect("reason").contains("no volume"));
    }
}
