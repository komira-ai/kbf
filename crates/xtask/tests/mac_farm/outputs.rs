//! `mac-farm --output` against the fake farm: what a passed command left at its declared paths
//! is written below `--out-dir` at the same paths, as files and directories with their bytes and
//! their executable bit; and each answer that is not exactly the declared outputs, a symlink,
//! bytes that are not their digest, a tree that is broken, or a place already taken fails the
//! run with its own message.

use std::os::unix::fs::PermissionsExt as _;

use kbf_proto::reapi::{
    DirectoryNode, FileNode, OutputDirectory, OutputFile, OutputSymlink, SymlinkNode, Tree,
};

use super::*;

const KBF: &str = "target/aarch64-apple-darwin/release/kbf";

fn file(path: &str, digest: &Digest, executable: bool) -> OutputFile {
    OutputFile {
        path: path.to_owned(),
        digest: Some(digest.clone()),
        is_executable: executable,
        ..OutputFile::default()
    }
}

fn node(name: &str, digest: &Digest, executable: bool) -> FileNode {
    FileNode {
        name: name.to_owned(),
        digest: Some(digest.clone()),
        is_executable: executable,
        ..FileNode::default()
    }
}

fn directory(path: &str, tree: &Digest) -> OutputDirectory {
    OutputDirectory {
        path: path.to_owned(),
        tree_digest: Some(tree.clone()),
        ..OutputDirectory::default()
    }
}

/// A passed command's response with these outputs.
fn passed(files: Vec<OutputFile>, dirs: Vec<OutputDirectory>) -> ExecuteResponse {
    let mut response = exited(0, None, None);
    if let Some(result) = response.result.as_mut() {
        result.output_files = files;
        result.output_directories = dirs;
    }
    response
}

impl Rig {
    /// Puts the tree of `root` and `children` in the fake's CAS; its digest.
    fn put_tree(
        &self,
        root: kbf_proto::reapi::Directory,
        children: Vec<kbf_proto::reapi::Directory>,
    ) -> Digest {
        self.put(
            &Tree {
                root: Some(root),
                children,
            }
            .encode_to_vec(),
        )
    }

    /// A run that declares `outputs` and writes them below `<dir>/out`, answered `response`.
    fn run_out(
        &self,
        dir: &TestDir,
        outputs: &[&str],
        response: &ExecuteResponse,
    ) -> (Result<Vec<String>, Error>, String) {
        self.state
            .queue(Answer::Updates(vec![answered("op/out", response)]));
        let out_dir = dir.path().join("out");
        let (result, _, stderr) = self.run_in(dir, |a| Args {
            outputs: outputs.iter().map(|o| (*o).to_owned()).collect(),
            out_dir: Some(out_dir),
            ..a
        });
        (result, stderr)
    }
}

/// Catches an output file or directory not written, written with other bytes or at another
/// path, the executable bit lost or given to a file that has none, an empty file skipped, a
/// directory below an output directory left out, and an `--output` missing from the Command.
/// With no `--out-dir` the outputs land in the checkout, where a local build leaves them.
#[test]
fn the_outputs_of_a_passed_command_are_written_at_their_paths() {
    let rig = Rig::new();
    let kbf = rig.put(b"mach-o kbf");
    let tarball = rig.put(b"tarball");
    let empty = rig.put(b"");
    let sub = kbf_proto::reapi::Directory {
        files: vec![node("empty", &empty, false)],
        ..Default::default()
    };
    let sub_digest = digest_of(&sub.encode_to_vec());
    let root = kbf_proto::reapi::Directory {
        files: vec![node("kbf.tar.gz", &tarball, false)],
        directories: vec![DirectoryNode {
            name: "sub".to_owned(),
            digest: Some(sub_digest),
        }],
        ..Default::default()
    };
    let tree = rig.put_tree(root, vec![sub]);
    rig.state.queue(Answer::Updates(vec![answered(
        "op/outputs",
        &passed(vec![file(KBF, &kbf, true)], vec![directory("dist", &tree)]),
    )]));
    let dir = scratch("outputs");
    let (result, _, stderr) = rig.run_in(&dir, |a| Args {
        outputs: vec![KBF.to_owned(), "dist".to_owned()],
        ..a
    });
    assert_eq!(
        result.expect("ok"),
        ["ok: `cargo test -p kbf-update` passed on a farm Mac"]
    );
    let (_, command) = rig.executed(0);
    assert_eq!(command.output_paths, ["dist", KBF]);
    let checkout = dir.path().join("checkout");
    assert!(stderr.ends_with(&format!(
        "mac-farm: 3 output files (17 bytes) written below {}\n",
        checkout.display()
    )));
    let mode = |p: &str| {
        std::fs::metadata(checkout.join(p))
            .expect("written")
            .permissions()
            .mode()
            & 0o111
    };
    assert_eq!(
        std::fs::read(checkout.join(KBF)).expect("kbf"),
        b"mach-o kbf"
    );
    assert_ne!(mode(KBF), 0);
    assert_eq!(
        std::fs::read(checkout.join("dist/kbf.tar.gz")).expect("tarball"),
        b"tarball"
    );
    assert_eq!(mode("dist/kbf.tar.gz"), 0);
    assert_eq!(
        std::fs::read(checkout.join("dist/sub/empty")).expect("empty"),
        b""
    );
}

/// Catches an output written over what is already at its place (a previous run's file, a
/// directory, or a file where its parent directory goes): the run fails naming the output and
/// keeps what was there.
#[test]
fn nothing_already_at_an_outputs_place_is_overwritten() {
    let rig = Rig::new();
    let kbf = rig.put(b"new");
    let dir = scratch("taken");
    let out = dir.path().join("out");
    std::fs::create_dir_all(out.join("dist")).expect("mkdir");
    std::fs::write(out.join("kbf"), b"old").expect("write");
    std::fs::write(out.join("target"), b"a file").expect("write");
    let exists = "File exists (os error 17)";
    let (result, _) = rig.run_out(
        &dir,
        &["kbf"],
        &passed(vec![file("kbf", &kbf, false)], vec![]),
    );
    assert_eq!(
        failure(result),
        format!("write the output \"kbf\": {exists}")
    );
    assert_eq!(std::fs::read(out.join("kbf")).expect("kept"), b"old");
    let tree = rig.put_tree(kbf_proto::reapi::Directory::default(), vec![]);
    let (result, _) = rig.run_out(
        &dir,
        &["dist"],
        &passed(vec![], vec![directory("dist", &tree)]),
    );
    assert_eq!(
        failure(result),
        format!("write the output \"dist\": {exists}")
    );
    let (result, _) = rig.run_out(
        &dir,
        &["target/kbf"],
        &passed(vec![file("target/kbf", &kbf, false)], vec![]),
    );
    assert_eq!(
        failure(result),
        format!("write the output \"target/kbf\": {exists}")
    );
}

/// Catches a declared output the command did not leave read as a pass (a build whose binary is
/// missing), an output the farm answered that was not declared written anyway, and the outputs
/// of a command that failed written as if it had passed.
#[test]
fn the_answer_must_be_exactly_the_declared_outputs_of_a_passed_command() {
    let rig = Rig::new();
    let kbf = rig.put(b"kbf");
    let dir = scratch("exactly");
    let out = dir.path().join("out");
    let (result, _) = rig.run_out(&dir, &["dist"], &passed(vec![], vec![]));
    assert_eq!(failure(result), "the command left no output \"dist\"");
    let (result, _) = rig.run_out(&dir, &[], &passed(vec![file("stray", &kbf, false)], vec![]));
    assert_eq!(
        failure(result),
        "the farm answered an output \"stray\" the command did not declare"
    );
    let tree = rig.put_tree(kbf_proto::reapi::Directory::default(), vec![]);
    let (result, _) = rig.run_out(
        &dir,
        &["dist"],
        &passed(
            vec![],
            vec![directory("dist", &tree), directory("more", &tree)],
        ),
    );
    assert_eq!(
        failure(result),
        "the farm answered an output \"more\" the command did not declare"
    );
    let mut failed = passed(vec![file("kbf", &kbf, false)], vec![]);
    if let Some(r) = failed.result.as_mut() {
        r.exit_code = 101;
    }
    let (result, _) = rig.run_out(&dir, &["kbf"], &failed);
    assert_eq!(
        failure(result),
        "`cargo test -p kbf-update` exited 101 on a farm Mac"
    );
    assert!(!out.exists(), "nothing is written for a failed command");
}

/// Catches a symlink written (its target could point anywhere on the runner, and a later file
/// written through it would land there): one answered as an output, and one inside an output
/// directory, each fail the run naming its path.
#[test]
fn a_symlink_among_the_outputs_fails_the_run() {
    let rig = Rig::new();
    let dir = scratch("symlinks");
    let mut response = passed(vec![], vec![]);
    if let Some(r) = response.result.as_mut() {
        r.output_symlinks = vec![OutputSymlink {
            path: "dist".to_owned(),
            target: "/etc".to_owned(),
            ..OutputSymlink::default()
        }];
    }
    let (result, _) = rig.run_out(&dir, &["dist"], &response);
    assert_eq!(
        failure(result),
        "the output \"dist\" is a symlink: mac-farm writes files and directories only"
    );
    let tree = rig.put_tree(
        kbf_proto::reapi::Directory {
            symlinks: vec![SymlinkNode {
                name: "link".to_owned(),
                target: "/etc".to_owned(),
                ..SymlinkNode::default()
            }],
            ..Default::default()
        },
        vec![],
    );
    let (result, _) = rig.run_out(
        &dir,
        &["dist"],
        &passed(vec![], vec![directory("dist", &tree)]),
    );
    assert_eq!(
        failure(result),
        "the output \"dist/link\" is a symlink: mac-farm writes files and directories only"
    );
}

/// Catches bytes the farm served under a digest that is not theirs written as that output: the
/// file is hashed as it is written, and its hash and size must be the digest's.
#[test]
fn an_output_whose_bytes_are_not_its_digest_fails() {
    let rig = Rig::new();
    let named = digest_of(b"the real kbf");
    let served = digest_of(b"something else");
    rig.state
        .blobs
        .lock()
        .expect("blobs")
        .insert(named.hash.clone(), b"something else".to_vec());
    let dir = scratch("corrupt");
    let (result, _) = rig.run_out(
        &dir,
        &["kbf"],
        &passed(vec![file("kbf", &named, true)], vec![]),
    );
    assert_eq!(
        failure(result),
        format!(
            "the output \"kbf\" is 14 bytes of {}, not the 12 bytes of {} the farm named",
            served.hash, named.hash
        )
    );
}

/// Catches a broken tree of an output directory taken as written: one the CAS does not have,
/// one that does not decode, one naming a directory it does not hold, and an entry whose name
/// is not one plain component (a file or a directory named to climb out of its directory).
#[test]
fn an_output_directory_whose_tree_is_broken_fails() {
    let rig = Rig::new();
    let dir = scratch("trees");
    let gone = digest_of(b"no such tree");
    let (result, _) = rig.run_out(&dir, &["a"], &passed(vec![], vec![directory("a", &gone)]));
    assert_eq!(
        failure(result),
        format!(
            "ByteStream.Read: NotFound: no blobs/{}/{}",
            gone.hash, gone.size_bytes
        )
    );
    let garbage = rig.put(b"\xff\xff\xff");
    let (result, _) = rig.run_out(
        &dir,
        &["b"],
        &passed(vec![], vec![directory("b", &garbage)]),
    );
    assert!(
        failure(result).starts_with("the tree of the output directory \"b\" does not decode: "),
    );
    let orphan = rig.put_tree(
        kbf_proto::reapi::Directory {
            directories: vec![DirectoryNode {
                name: "sub".to_owned(),
                digest: Some(digest_of(b"not a directory of the tree")),
            }],
            ..Default::default()
        },
        vec![],
    );
    let (result, _) = rig.run_out(&dir, &["c"], &passed(vec![], vec![directory("c", &orphan)]));
    assert_eq!(
        failure(result),
        "the tree of the output directory \"c\" lacks a directory it names"
    );
    let empty = rig.put(b"");
    let climbs = rig.put_tree(
        kbf_proto::reapi::Directory {
            files: vec![node("..", &empty, false)],
            ..Default::default()
        },
        vec![],
    );
    let (result, _) = rig.run_out(&dir, &["d"], &passed(vec![], vec![directory("d", &climbs)]));
    assert_eq!(
        failure(result),
        "the output directory \"d\" has an entry \"..\", which is not a plain name"
    );
    let slashed = rig.put_tree(
        kbf_proto::reapi::Directory {
            directories: vec![DirectoryNode {
                name: "x/y".to_owned(),
                digest: None,
            }],
            ..Default::default()
        },
        vec![],
    );
    let (result, _) = rig.run_out(
        &dir,
        &["e"],
        &passed(vec![], vec![directory("e", &slashed)]),
    );
    assert_eq!(
        failure(result),
        "the output directory \"e\" has an entry \"x/y\", which is not a plain name"
    );
}

/// A writer that refuses every write.
struct Full;

impl std::io::Write for Full {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("no space left"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Catches a write that failed taken as a blob read whole: the read ends with the writer's
/// error, naming the blob.
#[test]
fn a_read_into_a_writer_that_fails_ends_with_its_error() {
    let rig = Rig::new();
    let farm = rig.farm("");
    let digest = rig.put(b"bytes");
    let failed = rig
        .runtime
        .block_on(farm.read_into(&digest, &mut Full))
        .expect_err("refused");
    assert_eq!(
        failed.to_string(),
        format!("write the blob {}: no space left", digest.hash)
    );
}
