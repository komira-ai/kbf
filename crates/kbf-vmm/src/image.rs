//! `image base`: builds the base bundle the probes clone, from a macOS restore image.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde_json::json;
use sha2::{Digest, Sha256};

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
    /// After the install, write the probe launch daemons and the Setup Assistant marker
    /// into the image as root.
    pub offline_setup: bool,
}

/// `image base`. Nothing is created when the digest is bad or the file's digest differs.
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
    let (ipsw, downloaded) = match parse_source(&args.ipsw) {
        Source::Path(p) => (p, false),
        Source::Url(url) => {
            if let Err(e) = std::fs::create_dir_all(&args.out) {
                return Report::error(probe, format!("create {}: {e}", args.out.display()));
            }
            let dest = args.out.join("restore.ipsw");
            if let Err(e) = sys.fetch(&url, &dest) {
                let _ = std::fs::remove_dir_all(&args.out);
                return Report::error(probe, format!("download {url}: {e}"));
            }
            (dest, true)
        }
    };
    let actual = match verify(&ipsw, &expected) {
        Ok(a) => a,
        Err(e) => {
            if downloaded {
                let _ = std::fs::remove_dir_all(&args.out);
            }
            return Report::error(probe, e.to_string());
        }
    };
    let installed = match sys.install_image(&ipsw, &args.out, args.disk_gib) {
        Ok(v) => v,
        Err(e) => return Report::error(probe, format!("install: {e}")),
    };
    if downloaded {
        let _ = std::fs::remove_file(&ipsw);
    }
    let offline = args.offline_setup.then(|| {
        sys.offline_write(
            &args.out.join(crate::bundle::DISK),
            &guest::offline_payload(),
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
                "the bundle installed, but the offline setup (P0) failed: {e}; finish the image by hand at a console"
            ),
            details,
        ),
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
            offline_setup: false,
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
        assert_eq!((installs.len(), installs[0].2), (1, 64));
        assert!(sys.offline_calls.borrow().is_empty());
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
    }

    /// Catches an offline write that goes to the wrong disk or whose failure hides the
    /// installed bundle: success is reported in the details, failure is needs-manual.
    #[test]
    fn the_offline_setup_runs_on_the_new_disk_and_its_failure_is_needs_manual() {
        let dir = tmp("image-offline");
        let sys = FakeSystem::new();
        let mut a = args(&dir, &ipsw_file(&dir), HELLO);
        a.offline_setup = true;
        let r = base(&sys, &a);
        assert_eq!(r.status, Status::Ok);
        assert_eq!(r.details["offline_setup"]["written"], 5);
        assert_eq!(
            sys.offline_calls.borrow()[0].0,
            dir.join("out").join("disk.img")
        );
        let dir2 = tmp("image-offline2");
        *sys.offline.borrow_mut() = Err("hdiutil said no".into());
        let mut a = args(&dir2, &ipsw_file(&dir2), HELLO);
        a.offline_setup = true;
        let r = base(&sys, &a);
        assert_eq!(r.status, Status::NeedsManual);
        assert!(r.reason.expect("reason").contains("hdiutil said no"));
        assert_eq!(r.details["offline_setup"]["failure"], "hdiutil said no");
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
}
