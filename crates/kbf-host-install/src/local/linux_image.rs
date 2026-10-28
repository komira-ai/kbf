//! The frontend image of a release, fetched and checked on a Linux host: the other half of
//! [`super::linux`]'s stage, for the asset `kbf-frontend-image-<commit>-linux-<arch>.oci.tar`.
//!
//! The listing, the channel, the token and the unprivileged fetch are the tarball's. What is
//! different is the check, which runs before the image is anywhere near the container runtime:
//!
//! 1. root copies the image file, its attestation bundle and the release's `SHA256SUMS` out of
//!    the fetch's directory once, by descriptor, into a directory of its own, hashing the image
//!    as it copies (the fetch's directory belongs to an unprivileged user, so nothing is read
//!    from it twice);
//! 2. the copy's sha256 must be the one `SHA256SUMS` lists for the image's name, and the one the
//!    channel's pinned sums list, if the channel has some;
//! 3. `gh attestation verify` over the copy and its bundle, offline, for the release's commit,
//!    on the signer, runner and branch the host's settings allow ([`GhCheck`]).
//!
//! A release whose image fails any of this is refused, and nothing was loaded.

use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt};
use std::path::{Path, PathBuf};

use kbf_host::exec::Exec;
use kbf_installer::attest::{GhCheck, Tools as AttestTools};
use kbf_installer::stage::BUNDLE_CAP;

use super::answer::Code;
use super::backend::Fault;
use super::frontend::{FetchedImage, ImageSource};
use super::linux::{Linux, Prepared, commit_id, copy_hashing_mode, fetch_fault, read_capped};
use crate::mac::fetch::{AssetKind, Fetcher, Limits};
use crate::mac::release::{PickError, SUMS_NAME, bundle_of, listed_hash};
use crate::request::{Channel, Version};
use crate::runner::Identity;

/// The most of `SHA256SUMS` that is read.
const SUMS_CAP: u64 = 4 * 1024 * 1024;

impl<E: Exec> ImageSource for Linux<E> {
    fn fetch(&self, version: &Version, channel: &Channel) -> Result<FetchedImage, Fault> {
        let prepared = self.prepare(version, channel)?;
        let scratch = self.fetch_scratch(&prepared.prefix, prepared.who)?;
        let checked = self
            .download_image(&prepared, &scratch)
            .and_then(|(commit, files)| {
                self.check_image(&prepared, &commit, &files)
                    .inspect_err(|_| {
                        let _ = fs::remove_dir_all(self.check_dir(&commit));
                    })
            });
        // What the unprivileged user fetched is read once, by `check_image`, and goes.
        let _ = fs::remove_dir_all(&scratch);
        checked
    }
}

/// What the fetch left in its directory, by name.
struct Files {
    image: PathBuf,
    bundle: PathBuf,
    sums: PathBuf,
}

/// Write `text` to the new file `path` (root's, 0644 under the usual umask).
fn write_public(path: &Path, text: &str) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)?;
    file.write_all(text.as_bytes())
}

impl<E: Exec> Linux<E> {
    fn platform(prepared: &Prepared) -> String {
        format!("linux-{}", prepared.settings.arch)
    }

    /// Download the image, its bundle and the sums into `scratch`; the commit and the files.
    fn download_image(
        &self,
        prepared: &Prepared,
        scratch: &Path,
    ) -> Result<(String, Files), Fault> {
        let platform = Self::platform(prepared);
        let gh = self.layout.join(&self.layout.gh());
        let home = scratch.join("home");
        let origin = &prepared.origin;
        let fetcher = Fetcher {
            origin,
            asset: AssetKind::FrontendImage,
            platform: &platform,
            runner: &*self.runner,
            curl: &self.tools.curl,
            gh: &gh,
            who: prepared.who,
            home: &home,
            token: prepared.token.as_deref(),
            limits: Limits {
                tool: self.tools.tool,
                download: self.tools.download,
                tarball_max: self.tools.image_max,
                bundle_max: BUNDLE_CAP.max(SUMS_CAP),
            },
        };
        let commit = fetcher
            .fetch(&prepared.prefix, scratch)
            .map_err(fetch_fault)?;
        let files = Files {
            image: scratch.join(AssetKind::FrontendImage.name_on(
                origin.prefix(),
                &platform,
                &commit,
            )),
            bundle: scratch.join(bundle_of(&AssetKind::FrontendImage.name_on(
                origin.prefix(),
                &platform,
                &commit,
            ))),
            sums: scratch.join(SUMS_NAME),
        };
        Ok((commit, files))
    }

    /// `work/<commit>.check` (`<commit>.<tag>.check` for a helper with a tag), where the installer's own checks sweep after a crash.
    pub(super) fn check_dir(&self, commit: &str) -> PathBuf {
        self.layout
            .join(&self.layout.work_dir())
            .join(format!("{}.check", self.tagged(commit)))
    }

    /// A root-owned directory for the checks, with the unprivileged user's home for `gh`.
    fn make_check_dir(&self, commit: &str, who: Identity) -> Result<PathBuf, Fault> {
        let stage_failed = |e: io::Error| Fault::failed(Code::StageFailed, format!("scratch: {e}"));
        let dir = self.check_dir(commit);
        let work = self.layout.join(&self.layout.work_dir());
        fs::create_dir_all(&work).map_err(stage_failed)?;
        let _ = fs::remove_dir_all(&dir);
        for (path, mode, owner) in [
            (dir.clone(), 0o755, (self.owner.uid, self.owner.gid)),
            (dir.join("gh"), 0o700, (who.uid, who.gid)),
        ] {
            fs::create_dir(&path)
                .and_then(|()| fs::set_permissions(&path, fs::Permissions::from_mode(mode)))
                .and_then(|()| std::os::unix::fs::chown(&path, Some(owner.0), Some(owner.1)))
                .map_err(stage_failed)?;
        }
        Ok(dir)
    }

    /// Copy what was fetched into a directory of root's, and check it. The image, kept in
    /// the directory that is removed with it.
    fn check_image(
        &self,
        prepared: &Prepared,
        commit: &str,
        files: &Files,
    ) -> Result<FetchedImage, Fault> {
        let id = commit_id(commit)?;
        let platform = Self::platform(prepared);
        let name = AssetKind::FrontendImage.name_on(prepared.origin.prefix(), &platform, commit);
        let dir = self.make_check_dir(commit, prepared.who)?;
        let fetch_failed =
            |what: &str, e: io::Error| Fault::refused(Code::FetchFailed, format!("{what}: {e}"));
        let image = dir.join(&name);
        // Public files, which the unprivileged `gh` reads: 0644 (under the usual umask).
        let copied = copy_hashing_mode(&files.image, &image, self.tools.image_max, 0o644)
            .map_err(|e| fetch_failed(&name, e))?;
        let bundle = dir.join("bundle.json");
        let text = read_capped(&files.bundle, BUNDLE_CAP).map_err(|e| fetch_failed("bundle", e))?;
        let sums = read_capped(&files.sums, SUMS_CAP).map_err(|e| fetch_failed(SUMS_NAME, e))?;
        write_public(&bundle, &text).map_err(|e| fetch_failed("bundle", e))?;
        let mismatch = |from: &str, e: PickError| {
            let why = match e {
                PickError::Unknown => format!("{from} does not list {name}"),
                PickError::Malformed | PickError::Ambiguous => format!("{from} is malformed"),
            };
            Fault::refused(Code::VerifyFailed, why)
        };
        let lists = |from: &str, sums: &str| {
            let listed = listed_hash(sums, &name).map_err(|e| mismatch(from, e))?;
            if listed == copied.sha256 {
                Ok(())
            } else {
                Err(Fault::refused(
                    Code::VerifyFailed,
                    format!("the image's sha256 is not the one {from} lists for {name}"),
                ))
            }
        };
        lists("the release's SHA256SUMS", &sums)?;
        if let Some(pinned) = &prepared.sums {
            let pinned_text = self
                .host
                .trust
                .read(pinned)
                .map_err(|e| Fault::refused(Code::ConfigUnsafe, e.to_string()))?;
            lists("the channel's sums", &pinned_text)?;
        }
        let tools = AttestTools {
            trust: &*self.trust,
            runner: &*self.installer_runner,
        };
        GhCheck::pinned(&self.layout, &prepared.settings)
            .verify(&tools, &dir.join("gh"), &image, &bundle, &id)
            .map_err(|e| Fault::refused(Code::VerifyFailed, e.to_string()))?;
        Ok(FetchedImage::new(commit.to_owned(), image, Some(dir)))
    }
}
