//! `base-tree` as the verb runs it: the bundle written, listed, read back, and what a refused
//! or repeated run leaves in the output directory.

use std::collections::BTreeMap;
use std::path::PathBuf;

use kbf_testkit::dir::TestDir;

use super::super::*;
use super::support::{E, counted, image, layer};
use super::{COMMIT, built, norm, scratch};

/// The files of one `base-tree` run: the image and the binary, and the arguments for `platform`.
struct World {
    dir: TestDir,
    out: PathBuf,
    base: Base,
    args: Args,
}

/// A run of `layers` as the base and `kbf-task` bytes `binary`, for `platform`.
fn world(name: &str, layers: &[Vec<u8>], binary: &[u8], platform: Platform) -> World {
    let dir = scratch(name);
    let img = image(
        layers,
        &[layer(&[E::file("usr/bin/kbf-task", 0o755, b"top")])],
        true,
    );
    std::fs::write(dir.join("image.oci.tar"), &img.bytes).expect("image");
    std::fs::write(dir.join("kbf-task"), binary).expect("binary");
    let out = dir.join("dist");
    let args = Args {
        image: dir.join("image.oci.tar"),
        platform,
        commit: COMMIT.to_owned(),
        binary: dir.join("kbf-task"),
        out: out.clone(),
    };
    World {
        dir,
        out,
        base: img.base,
        args,
    }
}

/// A small base with every kind of entry the tree keeps.
fn small_base() -> Vec<Vec<u8>> {
    vec![
        layer(&[
            E::file("etc/shadow", 0o640, b"shadow"),
            E::file("usr/bin/tool", 0o755, b"tool"),
            E::file("usr/bin/same", 0o755, b"tool"),
            E::symlink("bin", "usr/bin"),
            E::dir("tmp/", 0o1777),
        ]),
        layer(&[E::file("usr/lib/deep/er/libc", 0o644, b"libc")]),
    ]
}

/// The number of names on the path of `e` from the root of the flattened tree `flat`.
fn depth_of<'a>(flat: &'a [tierstore::TreeEntry], mut e: &'a tierstore::TreeEntry) -> u64 {
    let mut depth = 0;
    while let Some(parent) = e.parent {
        depth += 1;
        e = &flat[parent as usize];
    }
    depth
}

/// Asserts the tree's entries and depth are the numbers tierstore's flattener finds in its
/// manifests: every entry below the root counted once, the deepest path in names.
/// Catches: a count that is not tierstore's, which every action tree's limits are then counted
/// from.
#[test]
fn the_entries_and_depth_of_the_tree_are_tierstores() {
    let tree = built(&small_base());
    assert_eq!(
        counted(&tree),
        (tree.built.entries, tree.built.depth as u64)
    );
    assert_eq!(counted(&tree), (14, 5));
}

/// Asserts the bundle is written to `<out>/kbf-base-tree-<commit>-<platform>.tree.tar` for each
/// Linux platform: the facts first (commit, platform, root, entries and depth as tierstore counts
/// them from the manifests, the normalised paths), then every file's bytes in the order of the
/// tree, then the manifests, the root's last, each object once and named by its digest; its line
/// is in `SHA256SUMS.<platform>` with the digest of the file; and the lines printed name the
/// bundle, the root, the entries, the depth and the number of normalised paths.
/// Catches: facts that are not the tree's (a limit counted wrongly in every action tree), objects
/// out of the order the frontend publishes in, a bundle that is not the one the sums list, and
/// output that does not name what was made.
#[test]
fn the_bundle_holds_the_facts_the_files_and_the_manifests_and_is_listed() {
    for platform in [Platform::LinuxX86_64, Platform::LinuxArm64] {
        let w = world("bundle", &small_base(), b"kbf-task bytes", platform);
        let printed = run_with(&w.args, &w.base).expect("run");
        let name = format!("kbf-base-tree-{COMMIT}-{}.tree.tar", platform.name());
        let bundle = std::fs::read(w.out.join(&name)).expect("bundle");

        let mut facts = None;
        let mut objects: Vec<(String, Vec<u8>)> = Vec::new();
        kbf_tree_bundle::read(bundle.as_slice(), |item| {
            match item {
                Item::Facts(f) => facts = Some(f),
                Item::Object { sha256, bytes } => objects.push((sha256, bytes)),
            }
            Ok::<(), String>(())
        })
        .expect("a bundle");
        let facts = facts.expect("facts");
        assert_eq!(facts.commit, COMMIT);
        assert_eq!(facts.platform, platform.name());
        assert_eq!(
            facts.normalised,
            [
                norm("/etc/shadow", "0640", "0644"),
                norm("/tmp", "1777", "0755"),
            ]
        );

        // The tree, counted by tierstore from the manifests the bundle holds.
        let manifests: BTreeMap<String, Vec<u8>> = objects.iter().cloned().collect();
        let root = tierstore::TreeDigest::from_hex(&facts.root).expect("root");
        let flat = tierstore::flatten(&root, |d: &tierstore::TreeDigest| {
            manifests.get(&d.to_string()).cloned().ok_or("missing")
        })
        .expect("flattens");
        assert_eq!(facts.entries, flat.len() as u64 - 1);
        assert_eq!(
            Some(facts.depth),
            flat.iter().map(|e| depth_of(&flat, e)).max()
        );

        // The files first, in the order of the tree (`usr/bin/same` and `usr/bin/tool` are the
        // same bytes: one object), then the manifests, the root's last; each object once.
        let names: Vec<&str> = objects.iter().map(|(n, _)| n.as_str()).collect();
        let sha = |b: &[u8]| Sha256::of(b).to_string();
        let files = [
            sha(b"kbf-task bytes"),
            sha(b"shadow"),
            sha(b"tool"),
            sha(b"libc"),
        ];
        assert_eq!(
            &names[..4],
            files.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert_eq!(names.last().copied(), Some(facts.root.as_str()));
        for (name, bytes) in &objects[4..] {
            let manifest: serde_json::Value = serde_json::from_slice(bytes).expect("manifest");
            assert_eq!(manifest["v"], 1, "{name}");
        }
        let distinct: std::collections::BTreeSet<&&str> = names.iter().collect();
        assert_eq!(distinct.len(), names.len(), "each object once");

        let sha256 = Sha256::of(&bundle);
        let sums = std::fs::read_to_string(w.out.join(format!("SHA256SUMS.{}", platform.name())))
            .expect("sums");
        assert_eq!(sums, format!("{sha256}  {name}\n"));
        assert_eq!(
            printed,
            [
                format!("wrote {name}: the base tree {}", facts.root),
                format!(
                    "{name}: {} entries, depth {}, 2 paths with a mode normalised",
                    facts.entries, facts.depth
                ),
                format!("{sha256}  {name}"),
            ]
        );
        drop(w.dir);
    }
}

/// Asserts a bundle already listed in the sums is refused and neither the bundle nor the sums
/// change, and a file already at the bundle's path (not listed) is refused and left as it is,
/// with nothing listed.
/// Catches: a second run that overwrites a release asset, and one that lists a file it did not
/// write.
#[test]
fn a_bundle_already_listed_or_already_there_is_refused_and_left_alone() {
    let w = world("listed", &small_base(), b"k", Platform::LinuxX86_64);
    run_with(&w.args, &w.base).expect("first run");
    let name = format!("kbf-base-tree-{COMMIT}-linux-x86_64.tree.tar");
    let sums = w.out.join("SHA256SUMS.linux-x86_64");
    let (bundle, listed) = (
        std::fs::read(w.out.join(&name)).expect("bundle"),
        std::fs::read(&sums).expect("sums"),
    );
    let Err(Error::Failed(why)) = run_with(&w.args, &w.base) else {
        panic!("a second run was not refused");
    };
    assert_eq!(why, format!("{} already lists {name}", sums.display()));
    assert_eq!(std::fs::read(w.out.join(&name)).expect("bundle"), bundle);
    assert_eq!(std::fs::read(&sums).expect("sums"), listed);

    let other = world("there", &small_base(), b"k", Platform::LinuxArm64);
    std::fs::create_dir_all(&other.out).expect("out");
    let there = other
        .out
        .join(format!("kbf-base-tree-{COMMIT}-linux-arm64.tree.tar"));
    std::fs::write(&there, b"something else").expect("file");
    let Err(Error::Spawn { what, .. }) = run_with(&other.args, &other.base) else {
        panic!("an existing file was not refused");
    };
    assert_eq!(what, format!("create {}", there.display()));
    assert_eq!(std::fs::read(&there).expect("file"), b"something else");
    assert!(!other.out.join("SHA256SUMS.linux-arm64").exists());
}

/// Asserts an image that is refused writes nothing: the output directory is not even made, and a
/// bad `--commit` and an unreadable image or binary are errors that write nothing either.
/// Catches: a half-written asset or a sums line for a release that was refused.
#[test]
fn a_refused_image_writes_and_lists_nothing() {
    let mut layers = small_base();
    layers.push(layer(&[E::special("dev/null", tar::EntryType::Char)]));
    let w = world("refused", &layers, b"k", Platform::LinuxX86_64);
    let Err(Error::Refused(problems)) = run_with(&w.args, &w.base) else {
        panic!("not refused");
    };
    assert_eq!(problems, ["/dev/null: a character device"]);
    assert!(!w.out.exists());

    let mut short = Args {
        commit: "abc123".to_owned(),
        ..clone_args(&w.args)
    };
    assert!(run_with(&short, &w.base).is_err());
    short.commit = COMMIT.to_owned();
    short.image = w.dir.join("no-image");
    assert!(matches!(
        run_with(&short, &w.base),
        Err(Error::Spawn { .. })
    ));
    short.image = w.args.image.clone();
    short.binary = w.dir.join("no-binary");
    assert!(matches!(
        run_with(&short, &w.base),
        Err(Error::Spawn { .. })
    ));
    assert!(!w.out.exists());
}

fn clone_args(a: &Args) -> Args {
    Args {
        image: a.image.clone(),
        platform: a.platform,
        commit: a.commit.clone(),
        binary: a.binary.clone(),
        out: a.out.clone(),
    }
}

/// Asserts `--platform darwin-arm64` is refused (the base tree is for the Linux platforms) and
/// that, for a Linux platform, an image that is not the pinned base's is refused by the pinned
/// base's layers.
/// Catches: a base tree built for a platform with no base image, and an image taken for the
/// pinned base that is not.
#[test]
fn the_verb_is_for_linux_and_checks_the_image_against_the_pinned_base() {
    let w = world("pinned", &small_base(), b"k", Platform::DarwinArm64);
    let Err(Error::Refused(problems)) = run(&w.args) else {
        panic!("darwin was not refused");
    };
    assert_eq!(
        problems,
        ["darwin-arm64: the kbf base tree is built for the Linux platforms only"]
    );
    for platform in [Platform::LinuxX86_64, Platform::LinuxArm64] {
        let args = Args {
            platform,
            ..clone_args(&w.args)
        };
        let pinned = Image::Task.base(platform).expect("pinned").expect("a base");
        let Err(Error::Refused(problems)) = run(&args) else {
            panic!("an image that is not the pinned base's was accepted");
        };
        assert_eq!(
            problems,
            [format!(
                "layer 1 is {}, the base's is {}",
                w.base.layers[0], pinned.layers[0]
            )]
        );
    }
    assert!(!w.out.exists());
}

/// Asserts a bundle read back that is not what was written is refused, each by what differs:
/// other facts, a missing object, an object that is not in the order written and a file that is
/// no bundle; and that a bundle that cannot be written is removed, not left to be listed.
/// Catches: a write that loses an object or the facts and is listed anyway, and a broken bundle
/// left in `dist` for the sums step to pick up.
#[test]
fn a_bundle_that_is_not_what_was_written_is_refused_and_removed() {
    let w = world("readback", &small_base(), b"k", Platform::LinuxX86_64);
    let img = std::fs::read(&w.args.image).expect("image");
    let tree = tree_of(&img, &w.base, b"k".to_vec()).expect("tree");
    let facts = facts_of(COMMIT, Platform::LinuxX86_64, &tree);
    let path = w.dir.join("bundle.tree.tar");
    let file = std::fs::File::create_new(&path).expect("file");
    commit_bundle(file, &path, &facts, &tree).expect("written");
    let objects = objects_of(&tree);
    read_back(&path, &facts, &objects).expect("read back");

    let why = |r: Result<(), Error>| match r {
        Err(Error::Failed(why)) => why,
        other => panic!("{other:?}"),
    };
    let other = Facts {
        entries: facts.entries + 1,
        ..facts.clone()
    };
    assert_eq!(
        why(read_back(&path, &other, &objects)),
        format!(
            "{}: read back: the facts are not the ones written",
            path.display()
        )
    );
    let fewer = &objects[1..];
    assert_eq!(
        why(read_back(&path, &facts, fewer)),
        format!(
            "{}: read back: it holds {} objects, not the {} written, or in another order",
            path.display(),
            objects.len(),
            fewer.len()
        )
    );
    let mut swapped = objects.clone();
    swapped.swap(0, 1);
    assert!(why(read_back(&path, &facts, &swapped)).contains("or in another order"));

    let junk = w.dir.join("junk.tree.tar");
    std::fs::write(&junk, b"not a tar").expect("junk");
    assert!(why(read_back(&junk, &facts, &objects)).contains("read back"));
    assert!(matches!(
        read_back(&w.dir.join("absent"), &facts, &objects),
        Err(Error::Spawn { .. })
    ));

    // A file that cannot be written to: the failure is the write's, and nothing is left.
    let unwritable = w.dir.join("unwritable.tree.tar");
    std::fs::write(&unwritable, b"").expect("file");
    let read_only = std::fs::File::open(&unwritable).expect("open");
    assert!(commit_bundle(read_only, &unwritable, &facts, &tree).is_err());
    assert!(!unwritable.exists());
}

/// Asserts the release's name test takes `kbf-base-tree-<commit>-<platform>.tree.tar` for a Linux
/// platform and for the commit asked for, and nothing else: not the Mac, a short or non-hex
/// commit, another commit, another suffix or prefix, or the name the writer would not make.
/// Catches: any file ending `.tree.tar` taken as an asset, and a name the writer makes
/// (`file_name`) that the release then refuses.
#[test]
fn only_the_bundle_of_a_linux_platform_for_the_commit_is_a_bundle_name() {
    for platform in ["linux-x86_64", "linux-arm64"] {
        let name = file_name(COMMIT, platform);
        assert!(is_bundle(&name), "{name}");
        assert!(is_bundle_of(&name, COMMIT), "{name}");
        assert!(!is_bundle_of(&name, &COMMIT.replace('0', "1")), "{name}");
    }
    for bad in [
        format!("kbf-base-tree-{COMMIT}-darwin-arm64.tree.tar"),
        format!("kbf-base-tree-{COMMIT}-windows-x86_64.tree.tar"),
        "kbf-base-tree-notacommit-linux-arm64.tree.tar".to_owned(),
        format!("kbf-base-tree-{COMMIT}-linux-arm64.tar"),
        format!("kbf-base-tree-{COMMIT}-linux-arm64.tree.tar.sigstore.json"),
        format!("kbf-tree-{COMMIT}-linux-arm64.tree.tar"),
        format!("kbf-base-tree{COMMIT}-linux-arm64.tree.tar"),
        format!("kbf-base-tree-{COMMIT}.tree.tar"),
    ] {
        assert!(!is_bundle(&bad), "{bad}");
        assert!(!is_bundle_of(&bad, COMMIT), "{bad}");
    }
}
