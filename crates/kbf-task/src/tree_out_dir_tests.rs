//! A tree job with `--stage-and-commit`: the flags, the whole run into a plain directory as the
//! put directory, and the exit code.

use super::*;
use crate::out_dir::OutDir;
use crate::pack::MAX_LARGE_BYTES;
use crate::task::EXIT_PUT;

/// `--stage-and-commit` with `--out-dir` set to `out`.
fn out_flags(out: &Path) -> [String; 2] {
    [
        "--stage-and-commit".to_owned(),
        format!("--out-dir={}", out.display()),
    ]
}

/// The flags of a tree job over `root` running `action` into the put directory `out`.
fn out_cli(root: &Path, action: &Digest, out: &Path) -> Cli {
    let extra = out_flags(out);
    flags(root, action, &[&extra[0], &extra[1]]).expect("flags")
}

/// A put directory beside the tree under `dir`.
fn put_dir(dir: &Path) -> PathBuf {
    let out = dir.join("put");
    std::fs::create_dir(&out).expect("put");
    out
}

/// The names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name().into_string().expect("utf-8"))
        .collect();
    names.sort();
    names
}

/// Asserts the whole run through the binary's entry point: the Command runs over the tree, the
/// task exits 0, and the put directory holds `pack` alone, one object whose manifest names the
/// Command's output and its exit code, says the inputs came from the tree, and carries the
/// walk time (the hash, write and commit times are not measured: one pass has no phases).
#[tokio::test]
async fn a_tree_job_commits_pack_into_the_put_directory_and_exits_0() {
    let (dir, digest) = copy_tree("out-dir-e2e");
    let out = put_dir(&dir);
    assert_eq!(
        main_with(&out_cli(&dir, &digest, &out)).await,
        ExitCode::SUCCESS
    );
    assert_eq!(names(&out), ["pack"]);
    let object = std::fs::read(out.join("pack")).expect("pack");
    let (manifest, entries) = read_one_pass(&object);
    assert_eq!(manifest.action, digest);
    assert_eq!(manifest.exit_code, 0);
    assert_eq!(manifest.output_files[0].path, "out");
    assert_eq!(manifest.output_files[0].digest, sha256(b"hi\n"));
    assert_eq!(manifest.inputs, Some(input_stats()));
    let timing = manifest.timing;
    assert_eq!(timing.walk_ms, Some(timing.pack_ms), "{timing:?}");
    assert_eq!(
        (timing.hash_ms, timing.write_ms, timing.commit_ms),
        (None, None, None)
    );
    let entry = (entries.iter())
        .find(|e| e.digest == sha256(b"hi\n"))
        .expect("the output's record");
    assert_eq!(
        &object[entry.range().start as usize..entry.range().end as usize],
        b"hi\n"
    );
}

/// Asserts the exit-code rule: a Command that fails (here `dd` of a missing file, exit 1) is
/// still a run whose pack was committed, so the task exits 0 and the Command's code and its
/// standard error are in the manifest.
#[tokio::test]
async fn a_failing_command_whose_object_is_committed_exits_0_with_its_code_in_the_manifest() {
    let dir = scratch("out-dir-failing");
    let command = command(&["dd", "if=missing", "of=out", "status=none"], "", &["out"]);
    let digest = lay_out(&dir, &action_of(&command), &command);
    let out = put_dir(&dir);
    assert_eq!(
        main_with(&out_cli(&dir, &digest, &out)).await,
        ExitCode::SUCCESS
    );
    let object = std::fs::read(out.join("pack")).expect("pack");
    let (manifest, _) = read_one_pass(&object);
    assert_eq!(manifest.exit_code, 1);
    assert!(manifest.stderr.is_some());
    assert!(manifest.output_files.is_empty());
}

/// Asserts the other half of the rule: a Command that succeeded but whose commit the store
/// refuses is exit 13, and nothing is left in the put directory.
#[tokio::test]
async fn a_refused_commit_is_exit_13_though_the_command_succeeded() {
    let (dir, digest) = copy_tree("out-dir-refused");
    let out = put_dir(&dir);
    let sink = OutDir::open(&out)
        .expect("open")
        .with_rename(|from, to| match to.extension() {
            Some(ext) if ext == "part" => std::fs::rename(from, to),
            _ => Err(std::io::Error::from_raw_os_error(libc::EEXIST)),
        });
    let failure = execute_out_dir(&out_cli(&dir, &digest, &out), &dir, &sink, &Limits::real())
        .await
        .expect_err("refused");
    assert_eq!(failure.exit_code(), EXIT_PUT);
    assert_eq!(std::fs::read(dir.join("work/out")).expect("ran"), b"hi\n");
    assert!(names(&out).is_empty(), "{:?}", names(&out));
}

/// Asserts that the task refuses to start, exit 14, before anything runs (no run directory is
/// made) when `--out-dir` is not a directory or the lease is not a build id.
#[tokio::test]
async fn a_put_directory_that_is_not_one_or_a_bad_lease_is_exit_14_and_nothing_runs() {
    let (dir, digest) = copy_tree("out-dir-setup");
    let file = dir.join("not-a-dir");
    std::fs::write(&file, b"").expect("file");
    assert_eq!(
        main_with(&out_cli(&dir, &digest, &file)).await,
        ExitCode::from(EXIT_SETUP)
    );
    assert!(!dir.join(RUN_DIR).exists());

    let out = put_dir(&dir);
    let mut cli = out_cli(&dir, &digest, &out);
    cli.lease = "not/a/build".to_owned();
    assert_eq!(main_with(&cli).await, ExitCode::from(EXIT_SETUP));
    assert!(!dir.join(RUN_DIR).exists());
    assert!(names(&out).is_empty());
}

/// Asserts that with `--stage-and-commit` a tree job hides nothing either, the put directory
/// included: it is the job's own output path.
#[test]
fn a_tree_job_with_stage_and_commit_hides_nothing_either() {
    let (dir, digest) = copy_tree("out-dir-hidden");
    let out = put_dir(&dir);
    let inputs = read(&out_cli(&dir, &digest, &out), &dir).expect("read");
    assert!(inputs.hidden.is_empty());
}

/// Asserts the flags: `--stage-and-commit` stands in for a pack URL, `--out-dir` defaults to
/// `/out`; it needs `--tree`, and it is refused with a pack URL, a key set or
/// `--allow-plain-http-put`; `--out-dir` needs `--stage-and-commit`.
#[test]
fn stage_and_commit_needs_a_tree_and_no_url_and_out_dir_needs_it() {
    let (dir, digest) = copy_tree("out-dir-flags");
    let cli = flags(&dir, &digest, &["--stage-and-commit"]).expect("flags");
    assert!(cli.stage_and_commit);
    assert_eq!(cli.out_dir(), Path::new("/out"));

    let conflicts = [
        NO_URL,
        "--put-urls-file=/nonexistent",
        "--allow-plain-http-put",
    ];
    for flag in conflicts {
        let error = flags(&dir, &digest, &["--stage-and-commit", flag])
            .err()
            .expect(flag);
        assert_eq!(error.kind(), ClapKind::ArgumentConflict, "{flag}");
    }
    for flag in [NO_URL, "--put-urls-file=/nonexistent"] {
        let error = flags(&dir, &digest, &[flag, "--out-dir=/elsewhere"])
            .err()
            .expect(flag);
        assert_eq!(error.kind(), ClapKind::ArgumentConflict, "{flag}");
    }
    let action = format!("--action={digest}");
    let without_tree = |extra: &[&str]| {
        let mut args = vec!["kbf-task", "--stage-and-commit", "--lease=lease-1", &action];
        args.extend_from_slice(extra);
        Cli::try_parse_from(args).err().expect("no --tree")
    };
    assert_eq!(without_tree(&[]).kind(), ClapKind::MissingRequiredArgument);
    let grant = [
        "--grant-url=https://grant.example",
        "--grant-token-file=/nonexistent",
    ];
    assert_eq!(without_tree(&grant).kind(), ClapKind::ArgumentConflict);
}

/// Asserts that the command line `kbf-server`'s executor gives a put-directory job (its
/// `put_dir_argv`: `--tree` with no value, then `--stage-and-commit`, `--out-dir /out`, the
/// action, the lease and the deadline, as separate words) parses to a tree run at `/` that
/// commits into `/out`.
#[test]
fn the_executors_put_dir_command_line_parses() {
    let action = format!("{}/12", "ab".repeat(32));
    let argv = [
        "kbf-task",
        "--tree",
        "--stage-and-commit",
        "--out-dir",
        "/out",
        "--action",
        &action,
        "--lease",
        "7-3",
        "--deadline-ms",
        "90000",
    ];
    let cli = Cli::try_parse_from(argv).expect("flags");
    assert_eq!(cli.tree.as_deref(), Some(Path::new("/")));
    assert!(cli.stage_and_commit);
    assert_eq!(cli.out_dir(), Path::new("/out"));
    assert_eq!(cli.lease, "7-3");
    assert_eq!(cli.deadline_ms, 90_000);
}

/// The flags of a tree job over `root` running `action` into `out` as one object.
fn one_pass_cli(root: &Path, action: &Digest, out: &Path) -> Cli {
    let extra = out_flags(out);
    flags(
        root,
        action,
        &[&extra[0], &extra[1], "--object-format=one-pass"],
    )
    .expect("flags")
}

/// The manifest and the entries of the one-pass object `object`, each record checked against
/// its entry's CRC and digest.
fn read_one_pass(object: &[u8]) -> (TaskManifest, Vec<kbf_task_pack::one_pass::Entry>) {
    let (manifest, entries, _) = read_one_pass_files(object);
    (manifest, entries)
}

/// [`read_one_pass`], and the neutral file index parsed.
fn read_one_pass_files(
    object: &[u8],
) -> (
    TaskManifest,
    Vec<kbf_task_pack::one_pass::Entry>,
    serde_json::Value,
) {
    use kbf_task_pack::one_pass::{TAIL_LEN, Tail, Trailer};
    let len = object.len() as u64;
    let trailer = Trailer::parse(&object[object.len() - TAIL_LEN..], len).expect("trailer");
    let tail = &object[object.len() - usize::try_from(trailer.tail_len()).expect("fits")..];
    let tail = Tail::parse(&trailer, tail).expect("tail");
    for entry in &tail.entries {
        let range = entry.range();
        assert!(entry.verify(&object[range.start as usize..range.end as usize]));
    }
    let manifest = TaskManifest::parse_with(&tail.manifest, MAX_LARGE_BYTES).expect("manifest");
    let files = serde_json::from_slice(&tail.files).expect("the file index is JSON");
    (manifest, tail.entries, files)
}

/// Asserts that `--object-format one-pass` commits `pack` alone, as one object whose trailer
/// leads to the manifest and the index, whose index has the output file, stdout and stderr, and
/// whose manifest says one pack, no large object, and one pass in its timing.
#[tokio::test]
async fn a_one_pass_run_commits_one_object_with_the_manifest_and_index_at_the_end() {
    let (dir, digest) = copy_tree("one-pass-e2e");
    let out = put_dir(&dir);
    assert_eq!(
        main_with(&one_pass_cli(&dir, &digest, &out)).await,
        ExitCode::SUCCESS
    );
    assert_eq!(names(&out), ["pack"]);
    let object = std::fs::read(out.join("pack")).expect("pack");
    let (manifest, entries) = read_one_pass(&object);
    assert_eq!(manifest.action, digest);
    assert_eq!(manifest.exit_code, 0);
    assert_eq!(manifest.packs, 1);
    assert!(manifest.large.is_empty());
    assert_eq!(manifest.output_files[0].digest, sha256(b"hi\n"));
    assert_eq!(manifest.inputs, Some(input_stats()));
    let held: Vec<_> = entries.iter().map(|e| e.digest).collect();
    assert!(held.contains(&sha256(b"hi\n")));
    for digest in manifest.output_digests() {
        assert!(held.contains(&digest), "{digest}");
    }
    let timing = manifest.timing;
    assert_eq!(timing.walk_ms, Some(timing.pack_ms), "{timing:?}");
    assert_eq!(
        (timing.hash_ms, timing.write_ms, timing.commit_ms),
        (None, None, None)
    );
}

/// Asserts that with no `--object-format` the object is the one-pass object, the same as with
/// `--object-format=one-pass`: a trailer, a manifest and an index at its end.
#[tokio::test]
async fn without_the_object_format_flag_the_object_is_the_one_pass_object() {
    for (name, with_flag) in [("default", false), ("flag", true)] {
        let (dir, digest) = copy_tree(&format!("one-pass-default-{name}"));
        let out = put_dir(&dir);
        let cli = if with_flag {
            one_pass_cli(&dir, &digest, &out)
        } else {
            out_cli(&dir, &digest, &out)
        };
        assert_eq!(main_with(&cli).await, ExitCode::SUCCESS);
        let object = std::fs::read(out.join("pack")).expect("pack");
        let (manifest, _) = read_one_pass(&object);
        assert_eq!(manifest.output_files[0].digest, sha256(b"hi\n"), "{name}");
    }
}

/// Asserts that `--object-format` is refused without `--stage-and-commit`, and that `one-pass`
/// is its only value: `packs`, which an earlier release took, is refused like any other.
#[test]
fn the_object_format_flag_needs_the_put_directory() {
    let action = format!("--action={}/12", "ab".repeat(32));
    let base = ["kbf-task", "--tree", "--lease=lease-1", &action];
    let mut args = base.to_vec();
    args.extend(["--pack-url-file=/x", "--object-format=one-pass"]);
    let error = Cli::try_parse_from(args).err().expect("refused");
    assert_eq!(error.kind(), ClapKind::ArgumentConflict);
    let mut args = base.to_vec();
    args.extend(["--stage-and-commit", "--object-format=packs"]);
    let error = Cli::try_parse_from(args).err().expect("refused");
    assert_eq!(error.kind(), ClapKind::InvalidValue);
}

/// Asserts that a Command whose outputs are a directory of files (two equal, one larger than a
/// chunk twice over) gives an object holding each distinct blob once: the Tree, its Directory,
/// the files, and that the equal large files are dead bytes, never a second entry.
#[tokio::test]
async fn a_directory_of_equal_files_is_one_entry_per_blob_and_dead_bytes_for_a_repeated_large_one()
{
    let dir = scratch("one-pass-dir");
    let command = command(&["cp", "-R", "src", "d"], "", &["d"]);
    let digest = lay_out(&dir, &action_of(&command), &command);
    // The Command copies this directory: two equal large files and two equal small ones.
    std::fs::create_dir(dir.join("work/src")).expect("src");
    for (name, bytes) in [
        ("big", vec![0u8; 3_145_728]),
        ("copy", vec![0u8; 3_145_728]),
    ] {
        std::fs::write(dir.join("work/src").join(name), bytes).expect("large");
    }
    for name in ["a", "b"] {
        std::fs::write(dir.join("work/src").join(name), b"a\n").expect("small");
    }
    let out = put_dir(&dir);
    assert_eq!(
        main_with(&one_pass_cli(&dir, &digest, &out)).await,
        ExitCode::SUCCESS
    );
    let object = std::fs::read(out.join("pack")).expect("pack");
    let (manifest, entries, index) = read_one_pass_files(&object);
    assert_eq!(manifest.exit_code, 0);
    // The neutral index names each file by its full path with its offset: the two equal large
    // files share the offset of the one record, and the object holds their bytes there.
    let at = |path: &str| {
        let file = index["files"]
            .as_array()
            .expect("files")
            .iter()
            .find(|f| f["path"] == path)
            .unwrap_or_else(|| panic!("{path} is indexed"));
        (
            file["offset"].as_u64().expect("offset"),
            file["size"].as_u64(),
        )
    };
    assert_eq!(at("d/big"), at("d/copy"));
    assert_eq!(at("d/a"), at("d/b"));
    let (offset, _) = at("d/a");
    assert_eq!(&object[offset as usize..offset as usize + 2], b"a\n");
    assert_eq!(index["dirs"], serde_json::json!(["d"]));
    assert_eq!(index["symlinks"], serde_json::json!([]));
    let zeros = sha256(&vec![0u8; 3_145_728]);
    assert_eq!(entries.iter().filter(|e| e.digest == zeros).count(), 1);
    let small = sha256(b"a\n");
    assert_eq!(entries.iter().filter(|e| e.digest == small).count(), 1);
    let tree = entries
        .iter()
        .find(|e| e.digest == manifest.output_directories[0].tree)
        .expect("the Tree is a record");
    let range = tree.range();
    let tree =
        reapi::Tree::decode(&object[range.start as usize..range.end as usize]).expect("tree");
    let root = tree.root.expect("root");
    let named: Vec<_> = root
        .files
        .iter()
        .map(|f| digest::from_reapi(f.digest.as_ref().expect("digest")).expect("digest"))
        .collect();
    assert_eq!(named.len(), 4);
    for digest in named {
        assert!(entries.iter().any(|e| e.digest == digest));
    }
    // The 3 MiB file is stored once as an entry and its copy is dead bytes after it, so the object
    // is longer than the entries' bytes by the copy's size.
    let blobs: u64 = entries.iter().map(|e| e.digest.size_bytes).sum();
    assert!(
        object.len() as u64 >= blobs + 3_145_728,
        "{} bytes holding {blobs} of blobs",
        object.len()
    );
}

/// Asserts the exit codes of a one-pass run that cannot be written: a file that cannot be
/// created, a write the file refuses while the outputs are read (a full disk), and a commit the
/// store refuses are all exit 13, and nothing is left in the put directory.
#[tokio::test]
async fn a_one_pass_object_that_cannot_be_written_or_committed_is_exit_13_and_leaves_nothing() {
    struct Full;
    impl std::io::Write for Full {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from_raw_os_error(libc::ENOSPC))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    for what in ["create", "write", "commit"] {
        let dir = scratch(&format!("one-pass-fail-{what}"));
        let command = command(
            &[
                "dd",
                "if=/dev/zero",
                "of=big",
                "bs=1048576",
                "count=2",
                "status=none",
            ],
            "",
            &["big"],
        );
        let digest = lay_out(&dir, &action_of(&command), &command);
        let out = put_dir(&dir);
        let mut sink = OutDir::open(&out).expect("open");
        sink = match what {
            "create" => sink.with_create(|_| Err(std::io::Error::from_raw_os_error(libc::EACCES))),
            "write" => sink.with_create(|_| Ok(Box::new(Full))),
            _ => sink.with_rename(|from, to| match to.extension() {
                Some(ext) if ext == "part" => std::fs::rename(from, to),
                _ => Err(std::io::Error::from_raw_os_error(libc::EEXIST)),
            }),
        };
        let failure = execute_out_dir(
            &one_pass_cli(&dir, &digest, &out),
            &dir,
            &sink,
            &Limits::real(),
        )
        .await
        .expect_err(what);
        assert_eq!(failure.exit_code(), EXIT_PUT, "{what}: {failure}");
        assert!(names(&out).is_empty(), "{what}: {:?}", names(&out));
    }
}

/// Asserts that a manifest naming a blob the object does not hold is refused as an invalid pack
/// (exit 12) before anything is committed, and the `.part` file is removed.
#[tokio::test]
async fn a_manifest_naming_a_blob_the_object_lacks_is_refused() {
    let dir = scratch("one-pass-missing");
    let out = put_dir(&dir);
    let sink = OutDir::open(&out).expect("open");
    let run = sink.object().expect("object");
    let mut manifest = TaskManifest {
        lease: "lease-1".to_owned(),
        action: sha256(b"action"),
        exit_code: 0,
        stdout: Some(sha256(b"never written")),
        stderr: None,
        output_files: Vec::new(),
        output_directories: Vec::new(),
        output_symlinks: Vec::new(),
        timing: kbf_task_pack::Timing::default(),
        usage: kbf_task_pack::Usage::default(),
        packs: 1,
        large: Vec::new(),
        inputs: None,
        objects: None,
    };
    let failure = run.finish(&mut manifest).await.expect_err("refused");
    assert_eq!(failure.exit_code(), crate::task::EXIT_PACK);
    assert!(
        matches!(&failure, Failure::Pack(e) if e.detail == crate::pack::INVALID_PACK),
        "{failure}"
    );
    assert!(names(&out).is_empty());
}
