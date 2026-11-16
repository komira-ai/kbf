use super::*;

/// Asserts a digest is 32 bytes and no other length, and prints in hex.
#[test]
fn a_tree_digest_is_32_bytes_and_prints_in_hex() {
    assert_eq!(TreeDigest::from_slice(&[0; 31]), None);
    assert_eq!(TreeDigest::from_slice(&[0; 33]), None);
    let digest = TreeDigest::from_slice(&[0xab; 32]).unwrap();
    assert_eq!(digest, TreeDigest::from_array([0xab; 32]));
    assert_eq!(digest.as_bytes(), &[0xab; 32]);
    assert_eq!(digest.to_string(), "ab".repeat(32));
    assert_eq!(
        format!("{digest:?}"),
        format!("TreeDigest({})", "ab".repeat(32))
    );
}

/// Asserts a placement id is any text but the empty one, which names nothing.
#[test]
fn a_placement_id_is_not_empty() {
    assert_eq!(PlacementId::new(""), None);
    let id = PlacementId::new("p-7").unwrap();
    assert_eq!((id.as_str(), id.to_string()), ("p-7", "p-7".to_owned()));
}

/// Asserts a create's answer is a value only with a 32-byte digest and a placement, and keeps
/// the whole answer. Catches an answer without either reaching a caller that would then
/// name a child or release a placement by nothing.
#[test]
fn a_create_answer_needs_a_digest_and_a_placement() {
    let good = CreateTreeResponse {
        tree: vec![1; 32],
        placement: "p1".to_owned(),
        created: true,
        files: 4,
        ..CreateTreeResponse::default()
    };
    let created = Created::of(good.clone()).unwrap();
    assert_eq!(created.tree(), TreeDigest::from_array([1; 32]));
    assert_eq!(created.placement().as_str(), "p1");
    assert!(created.is_new());
    assert_eq!(created.answer(), &good);
    let short = CreateTreeResponse {
        tree: vec![1; 3],
        ..good.clone()
    };
    assert!(matches!(Created::of(short), Err(Undecodable::Digest(3))));
    let unplaced = CreateTreeResponse {
        placement: String::new(),
        ..good
    };
    assert!(matches!(
        Created::of(unplaced),
        Err(Undecodable::NoPlacement)
    ));
}
