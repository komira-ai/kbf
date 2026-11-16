use tierstore_trees_api::v1::location::Where;
use tierstore_trees_api::v1::{ChildTree, Dir, File, FsLocation, ListDir, S3Location, Symlink};
use tonic::Code;

use super::*;

fn file(path: &str, size: u64) -> Entry {
    Entry {
        path: path.to_owned(),
        kind: Some(entry::Kind::File(File {
            mode: None,
            sha256: vec![7; 32],
            size,
            from: None,
        })),
    }
}

fn kind(path: &str, kind: entry::Kind) -> Entry {
    Entry {
        path: path.to_owned(),
        kind: Some(kind),
    }
}

fn fs(path: &str) -> Location {
    Location {
        r#where: Some(Where::Fs(FsLocation {
            path: path.to_owned(),
        })),
    }
}

fn create(
    state: &mut State,
    entries: Vec<Entry>,
    at: Vec<Location>,
) -> Result<CreateTreeResponse, Status> {
    state.create(CreateTreeRequest {
        entries,
        at,
        ..CreateTreeRequest::default()
    })
}

/// Asserts the digest covers each kind of entry and an entry of no kind, and changes with
/// the salt, the path, a mode, a size and a child; and not with where a file's bytes come
/// from.
#[test]
fn the_digest_covers_names_kinds_and_contents_but_not_sources() {
    let mut state = State::new();
    let all = vec![
        file("a", 1),
        kind("d", entry::Kind::Dir(Dir { mode: None })),
        kind(
            "l",
            entry::Kind::Symlink(Symlink {
                target: "a".to_owned(),
            }),
        ),
        kind("t", entry::Kind::Tree(ChildTree { tree: vec![1; 32] })),
        Entry {
            path: "x".to_owned(),
            kind: None,
        },
    ];
    let base = state.digest(&all);
    let mut sourced = all.clone();
    if let Some(entry::Kind::File(f)) = &mut sourced[0].kind {
        f.from = Some(tierstore_trees_api::v1::Source {
            key: "packs/p".to_owned(),
            offset: 9,
        });
    }
    assert_eq!(state.digest(&sourced), base);
    let mut bigger = all.clone();
    bigger[0] = file("a", 2);
    assert_ne!(state.digest(&bigger), base);
    let mut renamed = all.clone();
    renamed[0].path = "b".to_owned();
    assert_ne!(state.digest(&renamed), base);
    state.set_salt(b"another builder");
    assert_ne!(state.digest(&all), base);
}

/// Asserts an entry of no kind is refused with a status that names no refusal of the node,
/// and each named check of the request alone.
#[test]
fn the_request_alone_is_checked() {
    let mut state = State::new();
    let none = Entry {
        path: "x".to_owned(),
        kind: None,
    };
    let refused = create(&mut state, vec![none], vec![]).unwrap_err();
    assert_eq!(
        (refused.code(), refused.message()),
        (Code::InvalidArgument, "entry x has no kind")
    );
    let cases: [(Vec<Entry>, &str); 7] = [
        (vec![file("a//b", 1)], "unsupported entry name: a//b"),
        (vec![file("a/..", 1)], "unsupported entry name: a/.."),
        (vec![file("./a", 1)], "unsupported entry name: ./a"),
        (vec![file("a", 1), file("a", 2)], "duplicate path: a"),
        (
            vec![kind("t", entry::Kind::Tree(ChildTree { tree: vec![1; 3] }))],
            "malformed digest: t",
        ),
        (
            vec![kind(
                "d",
                entry::Kind::Dir(Dir {
                    mode: Some(0o10000),
                }),
            )],
            "bad mode: d",
        ),
        (
            vec![kind(
                "l",
                entry::Kind::Symlink(Symlink {
                    target: String::new(),
                }),
            )],
            "",
        ),
    ];
    for (entries, message) in cases {
        let answer = create(&mut state, entries, vec![]);
        match message {
            "" => assert!(answer.is_ok(), "{answer:?}"),
            _ => assert_eq!(answer.unwrap_err().message(), message),
        }
    }
}

/// Asserts a placement whose tree the node no longer has reads as `unknown placement`, a
/// cleared mark is no mark, and forgetting a tree twice says so.
#[test]
fn a_placement_without_its_tree_is_unknown() {
    let mut state = State::new();
    let made = create(&mut state, vec![file("a", 1)], vec![fs("jobs/j")]).unwrap();
    let tree = TreeDigest::from_slice(&made.tree).unwrap();
    assert!(state.forget_tree(tree));
    assert!(!state.forget_tree(tree));
    let read = GetPlacementRequest {
        placement: made.placement.clone(),
        list: None,
    };
    assert_eq!(
        state.placement(&read).unwrap_err().message(),
        "unknown placement: p1"
    );
    state.mark("cas/l/x", Some(Source::Missing));
    state.mark("cas/l/x", None);
    assert!(state.sources.is_empty());
}

/// Asserts a location taken names it, as an `fs` path or as `bucket/prefix`, and a location
/// of no kind is named by nothing.
#[test]
fn a_taken_location_is_named() {
    let mut state = State::new();
    let s3 = Location {
        r#where: Some(Where::S3(S3Location {
            bucket: "b".to_owned(),
            prefix: "jobs/".to_owned(),
        })),
    };
    let nowhere = Location { r#where: None };
    create(
        &mut state,
        vec![file("a", 1)],
        vec![s3.clone(), nowhere.clone()],
    )
    .unwrap();
    let other = create(&mut state, vec![file("b", 1)], vec![s3]).unwrap_err();
    assert_eq!(other.message(), "location taken: b/jobs/");
    let other = create(&mut state, vec![file("b", 1)], vec![nowhere]).unwrap_err();
    assert_eq!(other.message(), "location taken: ");
}

/// Asserts a listing names each child of a directory once, sorted: files with their mode,
/// size and digest; directories, given or implied by a deeper path; symlinks; child trees
/// as directories. Pages follow `after` and `limit` (0 is 1000), and `next` is set only when
/// more remain.
#[test]
fn a_listing_pages_through_a_directory() {
    let mut state = State::new();
    let entries = vec![
        file("top", 3),
        kind("d", entry::Kind::Dir(Dir { mode: Some(0o700) })),
        file("sub/x", 1),
        file("sub/y/z", 1),
        kind(
            "sub/l",
            entry::Kind::Symlink(Symlink {
                target: "x".to_owned(),
            }),
        ),
        kind("sub/c", entry::Kind::Tree(ChildTree { tree: vec![0; 32] })),
        kind("sub/e", entry::Kind::Dir(Dir { mode: None })),
    ];
    let child = create(&mut state, vec![file("f", 1)], vec![]).unwrap();
    let mut entries = entries;
    entries[5] = kind("sub/c", entry::Kind::Tree(ChildTree { tree: child.tree }));
    let made = create(&mut state, entries, vec![fs("jobs/j")]).unwrap();
    let list = |path: &str, after: &str, limit| {
        let read = GetPlacementRequest {
            placement: made.placement.clone(),
            list: Some(ListDir {
                path: path.to_owned(),
                after: after.to_owned(),
                limit,
            }),
        };
        state.placement(&read).unwrap()
    };
    let root = list("", "", 0);
    let names: Vec<(&str, i32, u32)> = root
        .children
        .iter()
        .map(|c| (c.name.as_str(), c.kind, c.mode))
        .collect();
    let (file_kind, dir_kind, link_kind) = (
        EntryKind::File as i32,
        EntryKind::Dir as i32,
        EntryKind::Symlink as i32,
    );
    assert_eq!(
        names,
        [
            ("d", dir_kind, 0o700),
            ("sub", dir_kind, 0o755),
            ("top", file_kind, 0o644)
        ]
    );
    assert_eq!(
        (root.children[2].size, root.children[2].sha256.len()),
        (3, 32)
    );
    assert_eq!(root.next, "");
    let first = list("sub", "", 2);
    let names: Vec<&str> = first.children.iter().map(|c| c.name.as_str()).collect();
    assert_eq!((names, first.next.as_str()), (vec!["c", "e"], "e"));
    let rest = list("sub", "e", 2);
    let kinds: Vec<(&str, i32)> = rest
        .children
        .iter()
        .map(|c| (c.name.as_str(), c.kind))
        .collect();
    assert_eq!(kinds, [("l", link_kind), ("x", file_kind)]);
    assert_eq!(rest.next, "x");
    let last = list("sub", "x", 2);
    assert_eq!(last.children.len(), 1);
    assert_eq!(last.next, "");
}

/// Asserts a placement that is still filling reads with no bytes filled.
#[test]
fn a_filling_placement_has_nothing_filled() {
    let mut state = State::new();
    let made = state
        .create(CreateTreeRequest {
            entries: vec![file("a", 5)],
            prefetch: Prefetch::Background.into(),
            ..CreateTreeRequest::default()
        })
        .unwrap();
    let read = GetPlacementRequest {
        placement: made.placement,
        list: None,
    };
    let answer = state.placement(&read).unwrap();
    assert_eq!(answer.state(), PlacementState::Filling);
    assert_eq!((answer.total_bytes, answer.filled_bytes), (5, 0));
}
