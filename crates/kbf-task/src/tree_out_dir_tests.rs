//! A tree job with `--stage-and-commit`: the flags, the whole run into a plain directory as the
//! put directory, and the exit code.

use kbf_task_pack::read_manifest_with;

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
/// task exits 0, and the put directory holds `pack` alone, whose manifest names the Command's
/// output and its exit code, says the inputs came from the tree, and carries the four phase
/// timings (present; their values are not asserted).
#[tokio::test]
async fn a_tree_job_commits_pack_into_the_put_directory_and_exits_0() {
    let (dir, digest) = copy_tree("out-dir-e2e");
    let out = put_dir(&dir);
    assert_eq!(
        main_with(&out_cli(&dir, &digest, &out)).await,
        ExitCode::SUCCESS
    );
    assert_eq!(names(&out), ["pack"]);
    let pack = std::fs::read(out.join("pack")).expect("pack");
    let manifest = read_manifest_with(&pack, MAX_LARGE_BYTES).expect("read");
    assert_eq!(manifest.action, digest);
    assert_eq!(manifest.exit_code, 0);
    assert_eq!(manifest.output_files[0].path, "out");
    assert_eq!(manifest.output_files[0].digest, sha256(b"hi\n"));
    assert_eq!(manifest.inputs, Some(input_stats()));
    let timing = manifest.timing;
    let phases = [
        timing.walk_ms,
        timing.hash_ms,
        timing.write_ms,
        timing.commit_ms,
    ];
    assert!(phases.iter().all(Option::is_some), "{timing:?}");
    let reader = kbf_segments::SegmentReader::open(&pack).expect("segment");
    assert_eq!(
        reader.get(&sha256(b"hi\n")).expect("get"),
        Some(&b"hi\n"[..])
    );
}

/// Asserts the exit-code rule: a Command that fails (here `dd` of a missing file, exit 1) is
/// still a run whose pack was committed, so the task exits 0 and the Command's code and its
/// standard error are in the manifest.
#[tokio::test]
async fn a_failing_command_whose_pack_is_committed_exits_0_with_its_code_in_the_manifest() {
    let dir = scratch("out-dir-failing");
    let command = command(&["dd", "if=missing", "of=out", "status=none"], "", &["out"]);
    let digest = lay_out(&dir, &action_of(&command), &command);
    let out = put_dir(&dir);
    assert_eq!(
        main_with(&out_cli(&dir, &digest, &out)).await,
        ExitCode::SUCCESS
    );
    let pack = std::fs::read(out.join("pack")).expect("pack");
    let manifest = read_manifest_with(&pack, MAX_LARGE_BYTES).expect("read");
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

/// Asserts that with `--stage-and-commit` a tree job hides the put directory from its Command,
/// beside the grants and its run directory, and without it does not.
#[test]
fn a_tree_job_with_stage_and_commit_hides_the_put_directory() {
    let (dir, digest) = copy_tree("out-dir-hidden");
    let out = put_dir(&dir);
    let inputs = read(&out_cli(&dir, &digest, &out), &dir).expect("read");
    assert_eq!(
        inputs.hidden,
        [PathBuf::from(GRANTS_DIR), dir.join(RUN_DIR), out]
    );
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
