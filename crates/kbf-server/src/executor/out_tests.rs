use js_proto::v1::{Layer, Mount, MountMode, MountPropagation, layer, mount};

use super::{MAX_ROOT_BYTES, OUT_TARGET, PutDirRoots, RootError, TaskOut};

/// Asserts that the trees are the directory beside the put root, under one mount point or `/`.
#[test]
fn the_trees_are_beside_the_put_root() {
    let roots = PutDirRoots::new("/mnt/tierstore/put").expect("roots");
    assert_eq!(roots.put(), "/mnt/tierstore/put");
    assert_eq!(roots.trees(), "/mnt/tierstore/trees");
    let top = PutDirRoots::new("/put").expect("roots");
    assert_eq!(top.trees(), "/trees");
    assert_eq!(TaskOut::default(), TaskOut::Presigned);
}

/// Asserts that a root that is not an absolute path in normal form below `/`, or is too long,
/// is refused by name: the job service would refuse every job that names a path under it.
#[test]
fn a_root_that_is_no_host_path_is_refused() {
    for relative in ["", "mnt/put", "./put"] {
        assert_eq!(
            PutDirRoots::new(relative),
            Err(RootError::NotAbsolute(relative.to_owned()))
        );
    }
    for bad in [
        "/",
        "/mnt/",
        "/mnt//put",
        "/mnt/./put",
        "/mnt/../put",
        "/mnt/p\nut",
    ] {
        assert_eq!(
            PutDirRoots::new(bad),
            Err(RootError::NotNormal(bad.to_owned())),
            "{bad:?}"
        );
    }
    let longest = format!("/{}", "p".repeat(MAX_ROOT_BYTES - 1));
    assert!(PutDirRoots::new(&longest).is_ok());
    let over = format!("{longest}p");
    assert_eq!(
        PutDirRoots::new(&over),
        Err(RootError::TooLong(over.clone()))
    );
}

/// Asserts the layer and the mount of a job: the action tree under the trees, and the put
/// directory under the put root at `/out`, read-write, `no_exec`, private.
#[test]
fn a_job_gets_the_tree_as_its_layer_and_the_dir_at_out() {
    let roots = PutDirRoots::new("/mnt/ts/put").expect("roots");
    let tree = "ab".repeat(32);
    assert_eq!(
        roots.layer(&tree),
        Layer {
            source: Some(layer::Source::HostPath(format!("/mnt/ts/trees/{tree}"))),
        }
    );
    let dir = "cas/b/7-8/0123";
    assert_eq!(
        roots.mount(dir),
        Mount {
            source: Some(mount::Source::HostPath(
                "/mnt/ts/put/cas/b/7-8/0123".to_owned()
            )),
            target: OUT_TARGET.to_owned(),
            mode: MountMode::ReadWrite.into(),
            sub_path: String::new(),
            no_exec: true,
            propagation: MountPropagation::Private.into(),
        }
    );
    assert_eq!(OUT_TARGET, "/out");
}
