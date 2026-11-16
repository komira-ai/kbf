use kbf_segments::sha256;
use kbf_task_pack::ManifestError;

use super::*;

/// Asserts that a manifest over its size cap is the size refusal and any other manifest
/// refusal is the invalid-manifest one, each with the manifest error's own text after its detail.
#[test]
fn a_manifest_refusal_is_a_size_cap_or_an_invalid_manifest() {
    let too_large = PackError::from(ManifestError::TooLarge { len: 9 });
    assert_eq!(too_large.detail, OUTPUT_TOO_LARGE);
    assert_eq!(
        too_large.to_string(),
        format!("output-too-large: {}", ManifestError::TooLarge { len: 9 })
    );

    let other = PackError::from(ManifestError::Version(9));
    assert_eq!(other.detail, INVALID_MANIFEST);
    assert_eq!(
        other.to_string(),
        format!("invalid-manifest: {}", ManifestError::Version(9))
    );
}

/// Asserts that a manifest naming a blob the object lacks is refused as an invalid object, with
/// the digest in the line.
#[test]
fn a_blob_the_object_lacks_is_an_invalid_object_naming_it() {
    let digest = sha256(b"absent");
    let error = PackError::missing(&digest);
    assert_eq!(error.detail, INVALID_PACK);
    assert_eq!(
        error.to_string(),
        format!("invalid-pack: the manifest names {digest}, which the object does not hold")
    );
}
