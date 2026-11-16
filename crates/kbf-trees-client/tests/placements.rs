//! `GetPlacement` and `DeleteTree` against the fake.

mod support;

use kbf_trees_client::fake::Call;
use kbf_trees_client::v1::{
    DeleteTreeRequest, EntryKind, GetPlacementRequest, ListDir, PlacementState,
};
use kbf_trees_client::{PlacementId, Reason};
use support::{create, fake_and_client, job, whole};
use tonic::Code;

/// Asserts a read of a placement answers its tree, state, location, counts and lease as the
/// create made them, and with `list`, a page of a directory's children.
#[tokio::test]
async fn a_placement_reads_as_the_create_made_it() {
    let (server, client) = fake_and_client().await;
    let entries = vec![
        whole("work/a", 1, 5),
        whole("work/b", 2, 7),
        whole("top", 3, 1),
    ];
    let created = client
        .create_tree(create(entries, vec![job("j")]))
        .await
        .unwrap();
    let read = client
        .get_placement(created.placement(), None)
        .await
        .unwrap();
    assert_eq!(read.placement, created.placement().as_str());
    assert_eq!(read.tree, created.tree().as_bytes());
    assert_eq!(read.state(), PlacementState::Ready);
    assert_eq!(read.at, [job("j")]);
    assert_eq!(
        (read.files, read.total_bytes, read.filled_bytes),
        (3, 13, 13)
    );
    assert_eq!(read.created_ms, 1_800_000_000_000);
    assert!(read.children.is_empty());
    let list = ListDir {
        path: "work".to_owned(),
        after: String::new(),
        limit: 1,
    };
    let page = client
        .get_placement(created.placement(), Some(list.clone()))
        .await
        .unwrap();
    let names: Vec<(&str, i32, u64)> = page
        .children
        .iter()
        .map(|c| (c.name.as_str(), c.kind, c.size))
        .collect();
    assert_eq!(names, [("a", EntryKind::File as i32, 5)]);
    assert_eq!(page.next, "a");
    let rest = ListDir {
        after: page.next.clone(),
        limit: 0,
        ..list.clone()
    };
    let last = client
        .get_placement(created.placement(), Some(rest))
        .await
        .unwrap();
    let names: Vec<&str> = last.children.iter().map(|c| c.name.as_str()).collect();
    assert_eq!((names, last.next.as_str()), (vec!["b"], ""));
    let read_call = GetPlacementRequest {
        placement: created.placement().as_str().to_owned(),
        list: Some(list),
    };
    assert_eq!(server.calls()[2], Call::GetPlacement(read_call));
}

/// Asserts a release removes the placement, after which a read or a second release of it is
/// `unknown placement` naming the id, a definite refusal; the tree stays on the node.
#[tokio::test]
async fn a_released_placement_is_unknown() {
    let (server, client) = fake_and_client().await;
    let created = client
        .create_tree(create(vec![whole("a", 1, 1)], vec![job("j")]))
        .await
        .unwrap();
    client.delete_tree(created.placement()).await.unwrap();
    assert!(server.placements().is_empty());
    assert!(server.has_tree(created.tree()));
    let id = created.placement().as_str();
    let read = client
        .get_placement(created.placement(), None)
        .await
        .unwrap_err();
    let again = client.delete_tree(created.placement()).await.unwrap_err();
    for error in [read, again] {
        let refusal = error.refusal().expect("a refusal");
        assert_eq!(
            (refusal.reason(), refusal.about(), refusal.code()),
            (Reason::UnknownPlacement, Some(id), Code::NotFound)
        );
        assert!(!error.may_have_applied());
    }
    let release = DeleteTreeRequest {
        placement: id.to_owned(),
    };
    assert_eq!(server.calls()[1], Call::DeleteTree(release));
}

/// Asserts a placement never made is unknown to a read.
#[tokio::test]
async fn a_placement_never_made_is_unknown() {
    let (_server, client) = fake_and_client().await;
    let none = PlacementId::new("p404").unwrap();
    let error = client.get_placement(&none, None).await.unwrap_err();
    assert!(error.is(Reason::UnknownPlacement), "{error:?}");
}
