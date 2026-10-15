//! The per-node image store of the OCI backend: each image digest unpacked once into
//! `<images>/<hex>/rootfs`, read-only afterwards, shared by every action that names it.
//!
//! **Acquisition is off the action path, and uses Podman once per digest.** The node's
//! Podman store is still where an image is pulled (`podman pull <repo>@sha256:...`,
//! as the error for a missing image says) and where the manifest is checked
//! (`PodmanBackend::check_store`: the digest must name one image, not an index). Then
//! `podman export` writes the container filesystem as a tar, and a helper container
//! ([`helper::unpack_jobs`]) extracts it with `tar` inside a user namespace that has
//! the same mapping as an action's, so every file ends up owned by the host id the
//! action sees as the image's own id (root is the first subordinate id), with no
//! shifting pass. After that an action needs one `stat` of `<hex>/rootfs`.
//!
//! The extraction runs in a user namespace with the daemon user unmapped and the stage
//! directory the only writable path, so a hostile tar can write nothing the daemon user
//! owns. The tar is of an image pinned by digest.
//!
//! Single flight per digest: concurrent first uses of one image wait on one unpack; a
//! use cancelled mid-unpack leaves another to retry (a stage directory with a different
//! random name each time; the leftovers are removed by the sweep at the next start).

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::OnceCell;

use super::{JobError, Oci, at};
use crate::image::{ImageCheckError, ImageRef};

/// The directory of the rootfs under an image's directory.
pub(crate) const ROOTFS: &str = "rootfs";

/// The prefix of an unpack in progress.
pub(crate) const STAGE_PREFIX: &str = ".stage-";

/// The per-node store.
#[derive(Debug)]
pub(crate) struct Images {
    dir: PathBuf,
    flights: Mutex<HashMap<String, Arc<OnceCell<()>>>>,
}

/// The hex of an image's `sha256:<hex>` digest, the name of its directory.
fn hex_of(image: &ImageRef) -> String {
    image
        .digest()
        .strip_prefix("sha256:")
        .unwrap_or(image.digest())
        .to_owned()
}

impl Images {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            flights: Mutex::new(HashMap::new()),
        }
    }

    /// The root filesystem of the image whose digest is `digest` (`sha256:<hex>`).
    pub(crate) fn rootfs_of(&self, digest: &str) -> PathBuf {
        self.dir
            .join(digest.strip_prefix("sha256:").unwrap_or(digest))
            .join(ROOTFS)
    }

    /// Whether `image` is unpacked.
    pub(crate) fn has(&self, image: &ImageRef) -> bool {
        self.rootfs_of(image.digest()).is_dir()
    }

    /// Unpacks `image` unless it is, waiting for an unpack already under way.
    ///
    /// # Errors
    /// The image is not in the node's Podman store, names an index, or cannot be
    /// exported or unpacked.
    pub(crate) async fn ensure(&self, oci: &Oci, image: &ImageRef) -> Result<(), ImageCheckError> {
        if self.has(image) {
            return Ok(());
        }
        let flight = {
            let mut flights = self.flights.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(flights.entry(hex_of(image)).or_default())
        };
        let unpacked = flight.get_or_try_init(|| async {
            oci.podman_backend().check_store(image).await?;
            self.unpack(oci, image)
                .await
                .map_err(|e| ImageCheckError::Failed(e.to_string()))
        });
        unpacked.await?;
        Ok(())
    }

    /// Exports and unpacks `image` into a stage directory, then renames the stage to the
    /// image's directory: presence of that name means complete. The stage is removed
    /// whatever happens to it.
    async fn unpack(&self, oci: &Oci, image: &ImageRef) -> Result<(), JobError> {
        let hex = hex_of(image);
        let stage = self.dir.join(format!("{STAGE_PREFIX}{hex}-{}", unique()));
        let tar = stage.with_extension("tar");
        at(&self.dir, tokio::fs::create_dir_all(&self.dir).await)?;
        let unpacked = self.unpack_into(oci, image, &stage, &tar).await;
        let _ = tokio::fs::remove_file(&tar).await;
        if unpacked.is_err() {
            // The stage holds files only the container's ids own.
            let _ = oci.remove_dir_async(&stage).await;
        }
        unpacked?;
        // The stage was open to the container's root; the image directory is not.
        let open = std::fs::Permissions::from_mode(0o755);
        at(&stage, tokio::fs::set_permissions(&stage, open).await)?;
        let finished = self.dir.join(&hex);
        at(&finished, tokio::fs::rename(&stage, &finished).await)
    }

    async fn unpack_into(
        &self,
        oci: &Oci,
        image: &ImageRef,
        stage: &Path,
        tar: &Path,
    ) -> Result<(), JobError> {
        // Open to the container's root, which is not the daemon user: it makes the
        // image's root inside, owned by itself.
        at(stage, tokio::fs::create_dir(stage).await)?;
        let open = std::fs::Permissions::from_mode(0o777);
        at(stage, tokio::fs::set_permissions(stage, open).await)?;
        let name = format!("kbf-image-{}", hex_of(image));
        oci.podman_export(&image.to_string(), &name, tar)
            .await
            .map_err(JobError::Crun)?;
        oci.unpack_tar(stage, tar).await
    }

    /// Removes what an unpack that was killed left: the stage directories and tars.
    /// Blocking; the files in a stage are owned by subordinate ids, so it needs `oci`.
    ///
    /// # Errors
    /// What could not be removed, in words that name the sweep.
    pub(crate) fn sweep(&self, oci: &Oci) -> Result<(), String> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Ok(());
        };
        let mut errors = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with(STAGE_PREFIX)
            {
                continue;
            }
            let removed = if path.is_dir() {
                oci.remove_dir_blocking(&path)
            } else {
                at(&path, std::fs::remove_file(&path))
            };
            errors.extend(removed.err().map(|e| e.to_string()));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "removing unpacks a previous daemon left: {}",
                errors.join("; ")
            ))
        }
    }
}

/// A name part unique to this process and call.
fn unique() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
