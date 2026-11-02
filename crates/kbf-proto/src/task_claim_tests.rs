use super::*;

/// Catches a claim not marked, one marked twice, and a mark read off metadata that has none:
/// metadata a daemon wrote (its own usage entry in `auxiliary_metadata`, or nothing) is not a
/// claim, a marked one is, and marking again changes nothing.
#[test]
fn a_marked_metadata_reads_as_a_claim_and_an_unmarked_one_does_not() {
    let mut metadata = ExecutedActionMetadata::default();
    assert!(!is_marked(&metadata));
    metadata.auxiliary_metadata.push(Any {
        type_url: crate::worker::ResourceUsage::TYPE_URL.to_owned(),
        value: Vec::new(),
    });
    assert!(!is_marked(&metadata));

    mark(&mut metadata);
    assert!(is_marked(&metadata));
    mark(&mut metadata);
    let marks = metadata
        .auxiliary_metadata
        .iter()
        .filter(|any| any.type_url == TYPE_URL)
        .count();
    assert_eq!(marks, 1);
    assert_eq!(
        metadata.auxiliary_metadata.len(),
        2,
        "the usage entry stays"
    );
}
