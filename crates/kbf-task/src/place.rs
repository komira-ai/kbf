//! Placing a run's recorded blobs into the slots it was issued, the step both sinks share: the
//! presigned PUTs of [`crate::task`] and the put directory of [`crate::out_dir`].
//!
//! [`manifest_reserve`] says how many bytes pack 0 keeps for the manifest, [`place`] runs the
//! plan ([`crate::plan::plan`]) over the blobs' digests and writes its shape (`packs`, `large`)
//! into the manifest, and [`output_cap`] bounds what the recorder may hold.

use kbf_task_pack::{LargeEntry, TaskManifest};

use crate::outputs::Blobs;
use crate::pack::{MAX_LARGE_BYTES, PackError};
use crate::plan::{Limits, Plan, plan};
use crate::task::{Failure, digest_of};

/// What the manifest of a run with several large objects can take beyond a manifest without:
/// the `packs` field and up to [`MAX_LARGE`](kbf_task_pack::MAX_LARGE) entries of about 110 bytes each.
pub(crate) const MANIFEST_SLACK: u64 = 2048;

/// The bytes pack 0 keeps for the manifest. A run of one pack and no large object writes the
/// manifest it already has, so the reserve is exactly its JSON length and a v1 run that fills
/// the pack to the byte still fits. With more packs or a large object the manifest gains the
/// `packs` field and entries, so [`MANIFEST_SLACK`] is added.
pub(crate) fn manifest_reserve(manifest: &TaskManifest, limits: &Limits) -> Result<u64, PackError> {
    let json = manifest.to_json_with(MAX_LARGE_BYTES)?.len() as u64;
    Ok(if limits.packs == 1 && limits.large == 0 {
        json
    } else {
        json + MANIFEST_SLACK
    })
}

/// The bytes of outputs the recorder may hold: what the run's packs and large objects can.
pub(crate) fn output_cap(limits: &Limits) -> u64 {
    let packs = (limits.packs as u64).saturating_mul(limits.pack_bytes);
    let large = (limits.large as u64).saturating_mul(limits.large_bytes);
    packs.saturating_add(large)
}

/// Plans `blobs` into the slots `limits` allows, pack 0 keeping `reserve` bytes for the manifest
/// as it will be written, and sets `manifest.packs` and `manifest.large` to the plan. Nothing is
/// read but the digests.
///
/// # Errors
/// [`Failure::Pack`] when the blobs do not fit (`output-too-large`).
pub(crate) fn place(
    manifest: &mut TaskManifest,
    blobs: &Blobs,
    reserve: u64,
    limits: &Limits,
) -> Result<Plan, Failure> {
    let sizes = blobs
        .digests()
        .map(|d| digest_of(Some(&d)).map(|d| (d, d.size_bytes)))
        .collect::<Result<Vec<_>, _>>()?;
    let planned = plan(&sizes, reserve, limits).map_err(PackError::from)?;
    // At most `limits.packs`, which the callers keep within MAX_PACKS; a count that did not fit
    // a byte would be refused by the manifest's own check.
    manifest.packs = u8::try_from(planned.packs.len()).unwrap_or(u8::MAX);
    manifest.large = (planned.large.iter().zip(0..))
        .map(|(digest, slot)| LargeEntry {
            slot,
            digest: *digest,
        })
        .collect();
    Ok(planned)
}
