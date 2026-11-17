use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser as _;
use clap::error::ErrorKind as ClapKind;
use kbf_proto::reapi;
use kbf_segments::sha256;
use kbf_testkit::dir::TestDir;
use kbf_types::Digest;
use prost::Message as _;

use super::*;
use crate::digest;
use crate::task::{EXIT_FETCH, EXIT_RUN, EXIT_SETUP};
use crate::tree::tests::{command, out_dir::read_one_pass, scratch};
use crate::tree::{ACTION_FILE, COMMAND_FILE, RUN_DIR};

/// A mounted tree, a job root and a put directory, with the Action's digest.
struct Job {
    tree: TestDir,
    root: TestDir,
    put: TestDir,
    action: Digest,
}

/// Lays out the tree `.kbf/action`, `.kbf/command` and an empty `kbf-work` for `command`, beside an
/// empty job root and an empty put directory.
fn job(name: &str, command: &reapi::Command) -> Job {
    let tree = scratch(&format!("mat-{name}-tree"));
    std::fs::create_dir_all(tree.join(".kbf")).expect(".kbf");
    std::fs::create_dir(tree.join("kbf-work")).expect("kbf-work");
    let action = reapi::Action {
        command_digest: Some(digest::to_reapi(&sha256(&command.encode_to_vec()))),
        input_root_digest: Some(digest::to_reapi(&sha256(b"input root"))),
        ..Default::default()
    }
    .encode_to_vec();
    std::fs::write(tree.join(ACTION_FILE), &action).expect("action");
    std::fs::write(tree.join(COMMAND_FILE), command.encode_to_vec()).expect("command");
    Job {
        tree,
        root: scratch(&format!("mat-{name}-root")),
        put: scratch(&format!("mat-{name}-put")),
        action: sha256(&action),
    }
}

impl Job {
    /// The flags of the job, as the Mac job passes them (the tree's path in place of `.kbf-tree`).
    fn cli(&self) -> Cli {
        Cli::try_parse_from([
            "kbf-task".to_owned(),
            format!("--materialize-work={}", self.tree.display()),
            "--stage-and-commit".to_owned(),
            format!("--out-dir={}", self.put.display()),
            "--lease=lease-1".to_owned(),
            format!("--action={}", self.action),
            "--object-format=one-pass".to_owned(),
        ])
        .expect("flags")
    }

    /// Runs the job into the put directory.
    async fn run(&self) -> Result<TaskManifest, Failure> {
        let out = OutDir::open(&self.put).expect("put directory");
        execute(&self.cli(), &self.tree, &self.root, &out).await
    }

    /// Runs the job and expects it to be refused with exit code `code`.
    async fn refused(&self, code: u8) -> Failure {
        let failure = self.run().await.expect_err("refused");
        assert_eq!(failure.exit_code(), code, "{failure}");
        failure
    }

    /// Asserts that nothing was made in the job root and nothing was committed.
    fn assert_untouched(&self) {
        assert!(!self.root.join("kbf-work").exists());
        assert!(!self.root.join(RUN_DIR).exists());
        assert_eq!(std::fs::read_dir(&*self.put).expect("put").count(), 0);
    }
}

/// The permission bits of `path`, not following a symlink.
fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

/// The put directory's one committed object: its manifest.
fn committed(put: &Path) -> TaskManifest {
    let object = std::fs::read(put.join("pack")).expect("pack");
    read_one_pass(&object).0
}

/// Proves the whole job: the tree's nested directories, files and symlinks are at the right
/// paths in the job root for the Command, an executable file is 0755 and any other 0644, a
/// symlink keeps its target (a dangling one too, not followed), the object is committed, and the
/// manifest says `fetch` with the files, directories and bytes copied.
#[tokio::test]
async fn the_tree_is_materialized_and_its_run_committed_with_the_copy_counted() {
    let job = job(
        "happy",
        &command(&["cp", "link", "out.txt"], "", &["out.txt"]),
    );
    let work = job.tree.join("kbf-work");
    std::fs::create_dir_all(work.join("a/c")).expect("dirs");
    std::fs::write(work.join("a/b.txt"), b"B\n").expect("file");
    std::fs::set_permissions(work.join("a/b.txt"), std::fs::Permissions::from_mode(0o600))
        .expect("mode");
    std::fs::write(work.join("tool"), b"0123456789").expect("script");
    std::fs::set_permissions(work.join("tool"), std::fs::Permissions::from_mode(0o510))
        .expect("mode");
    std::os::unix::fs::symlink("a/b.txt", work.join("link")).expect("link");
    std::os::unix::fs::symlink("/nonexistent/target", work.join("dangling")).expect("link");

    let manifest = job.run().await.expect("task");

    assert_eq!(manifest.exit_code, 0);
    assert_eq!(manifest.output_files[0].path, "out.txt");
    assert_eq!(manifest.output_files[0].digest, sha256(b"B\n"));
    assert_eq!(
        manifest.inputs,
        Some(InputStats {
            from: Some(InputsFrom::Fetch),
            files: 2,
            directories: 2,
            bytes_written_scratch: 12,
            ..InputStats::default()
        })
    );
    let copy = job.root.join("kbf-work");
    assert_eq!(std::fs::read(copy.join("a/b.txt")).expect("b"), b"B\n");
    assert!(copy.join("a/c").is_dir());
    assert_eq!(mode_of(&copy.join("a/b.txt")), 0o644);
    assert_eq!(mode_of(&copy.join("tool")), 0o755);
    assert_eq!(mode_of(&copy.join("a")), 0o755);
    assert_eq!(mode_of(&copy), 0o755);
    assert_eq!(
        std::fs::read_link(copy.join("link")).expect("link"),
        Path::new("a/b.txt")
    );
    assert_eq!(
        std::fs::read_link(copy.join("dangling")).expect("dangling"),
        Path::new("/nonexistent/target")
    );
    assert_eq!(mode_of(&job.root.join(RUN_DIR)), 0o700);
    assert!(job.root.join(".kbf-run/stdout").is_file());
    assert_eq!(std::fs::read_dir(&*job.put).expect("put").count(), 1);
    assert_eq!(committed(&job.put).output_files, manifest.output_files);
}

/// Proves that every copy is private: a Command that overwrites a read-only input, which is also
/// its declared output, changes the copy and leaves the tree's file and its mode as they were.
#[tokio::test]
async fn a_command_may_overwrite_an_input_without_touching_the_tree() {
    let job = job("private", &command(&["cp", "other", "data"], "", &["data"]));
    let source = job.tree.join("kbf-work/data");
    std::fs::write(&source, b"original\n").expect("data");
    std::fs::write(job.tree.join("kbf-work/other"), b"changed\n").expect("other");
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o444)).expect("mode");

    let manifest = job.run().await.expect("task");

    assert_eq!(manifest.output_files[0].digest, sha256(b"changed\n"));
    assert_eq!(std::fs::read(&source).expect("source"), b"original\n");
    assert_eq!(mode_of(&source), 0o444);
    assert_eq!(
        std::fs::read(job.root.join("kbf-work/data")).expect("copy"),
        b"changed\n"
    );
}

/// Proves that a tree whose mount is itself a symlink is followed, and that a symlink inside it
/// is recreated, not followed.
#[tokio::test]
async fn a_tree_that_is_a_symlink_is_followed_but_nothing_under_it_is() {
    let job = job("mount", &command(&["true"], "", &[]));
    std::os::unix::fs::symlink("/nonexistent", job.tree.join("kbf-work/dangling")).expect("link");
    let mount = scratch("mat-mount-link");
    std::os::unix::fs::symlink(&*job.tree, mount.join("tree")).expect("mount");

    let out = OutDir::open(&job.put).expect("put");
    execute(&job.cli(), &mount.join("tree"), &job.root, &out)
        .await
        .expect("task");

    assert!(
        std::fs::symlink_metadata(job.root.join("kbf-work/dangling"))
            .expect("link")
            .is_symlink()
    );
}

/// Proves that an existing `<root>/work` is refused with exit 10, naming it, and that nothing
/// ran or was committed.
#[tokio::test]
async fn an_existing_work_directory_is_refused_exit_10_and_nothing_runs() {
    let job = job("exists", &command(&["true"], "", &[]));
    std::fs::create_dir(job.root.join("kbf-work")).expect("work");
    let failure = job.refused(EXIT_FETCH).await;
    assert!(
        failure
            .to_string()
            .contains(&job.root.join("kbf-work").display().to_string()),
        "{failure}"
    );
    assert!(!job.root.join(RUN_DIR).exists());
    assert_eq!(std::fs::read_dir(&*job.put).expect("put").count(), 0);
}

/// Proves that a socket in the tree, which is none of file, directory or symlink, is refused
/// with exit 10 naming its path under `work` (exact message), and that no run directory is made.
#[tokio::test]
async fn a_socket_in_the_tree_is_refused_exit_10_naming_its_path() {
    let job = job("socket", &command(&["true"], "", &[]));
    std::fs::create_dir(job.tree.join("kbf-work/d")).expect("d");
    let _socket =
        std::os::unix::net::UnixListener::bind(job.tree.join("kbf-work/d/s")).expect("bind");
    let failure = job.refused(EXIT_FETCH).await;
    assert_eq!(
        failure.to_string(),
        "fetch: d/s: not a regular file, directory or symlink"
    );
    assert!(!job.root.join(RUN_DIR).exists());
    assert_eq!(std::fs::read_dir(&*job.put).expect("put").count(), 0);
}

/// Proves that an unreadable file in the tree is exit 10 naming it, and likewise an unreadable
/// directory (where this user can read neither, as root can, the copy succeeds instead).
#[tokio::test]
async fn an_unreadable_file_or_directory_in_the_tree_is_exit_10_naming_it() {
    for (name, is_dir) in [("file", false), ("dir", true)] {
        let job = job(&format!("unreadable-{name}"), &command(&["true"], "", &[]));
        let path = job.tree.join("kbf-work/locked");
        if is_dir {
            std::fs::create_dir(&path).expect("dir");
        } else {
            std::fs::write(&path, b"x").expect("file");
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("mode");
        let readable = if is_dir {
            std::fs::read_dir(&path).is_ok()
        } else {
            std::fs::File::open(&path).is_ok()
        };
        let outcome = job.run().await;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("mode");
        if readable {
            outcome.expect("a reader of everything copies it");
        } else {
            let failure = outcome.expect_err("refused");
            assert_eq!(failure.exit_code(), EXIT_FETCH);
            assert!(failure.to_string().contains("locked"), "{failure}");
        }
    }
}

/// Proves that a run directory that already exists is exit 11, as in a tree job.
#[tokio::test]
async fn an_existing_run_directory_is_refused_exit_11() {
    let job = job("run-dir", &command(&["true"], "", &[]));
    std::fs::create_dir(job.root.join(RUN_DIR)).expect("run dir");
    job.refused(EXIT_RUN).await;
    assert_eq!(std::fs::read_dir(&*job.put).expect("put").count(), 0);
}

/// Proves that an Action that is not `--action`'s, a missing `.kbf/command`, a `work` that is not
/// a directory (a file, or a symlink to one) and an Action naming no input root are each exit 10
/// with the job root untouched: the refusal comes before `work` is made.
#[tokio::test]
async fn a_refused_tree_is_exit_10_and_leaves_the_job_root_untouched() {
    let sh = command(&["true"], "", &[]);

    let wrong = job("wrong-action", &sh);
    let mut cli = wrong.cli();
    cli.action = sha256(b"another action");
    let out = OutDir::open(&wrong.put).expect("put");
    let failure = execute(&cli, &wrong.tree, &wrong.root, &out)
        .await
        .expect_err("refused");
    assert_eq!(failure.exit_code(), EXIT_FETCH);
    wrong.assert_untouched();

    let missing = job("no-command", &sh);
    std::fs::remove_file(missing.tree.join(COMMAND_FILE)).expect("remove");
    missing.refused(EXIT_FETCH).await;
    missing.assert_untouched();

    let file = job("work-file", &sh);
    std::fs::remove_dir(file.tree.join("kbf-work")).expect("remove");
    std::fs::write(file.tree.join("kbf-work"), b"").expect("file");
    file.refused(EXIT_FETCH).await;
    file.assert_untouched();

    let link = job("work-link", &sh);
    std::fs::remove_dir(link.tree.join("kbf-work")).expect("remove");
    std::os::unix::fs::symlink(".kbf", link.tree.join("kbf-work")).expect("link");
    link.refused(EXIT_FETCH).await;
    link.assert_untouched();

    let bare = job("no-input-root", &sh);
    let action = reapi::Action {
        command_digest: Some(digest::to_reapi(&sha256(&sh.encode_to_vec()))),
        ..Default::default()
    }
    .encode_to_vec();
    std::fs::write(bare.tree.join(ACTION_FILE), &action).expect("action");
    let mut cli = bare.cli();
    cli.action = sha256(&action);
    let out = OutDir::open(&bare.put).expect("put");
    let failure = execute(&cli, &bare.tree, &bare.root, &out)
        .await
        .expect_err("refused");
    assert_eq!(failure.exit_code(), EXIT_FETCH);
    bare.assert_untouched();
}

/// Proves that a put directory that is not a directory, or a bad lease, is exit 14 before
/// anything is read: the tree here does not exist, which would be exit 10 if it were read.
#[tokio::test]
async fn a_bad_put_directory_or_lease_is_exit_14_before_anything_is_read() {
    let root = scratch("mat-setup-root");
    let file = root.join("not-a-dir");
    std::fs::write(&file, b"").expect("file");
    let action = sha256(b"a");
    let cli = Cli::try_parse_from([
        "kbf-task".to_owned(),
        "--materialize-work=/nonexistent-tree".to_owned(),
        "--stage-and-commit".to_owned(),
        format!("--out-dir={}", file.display()),
        "--lease=lease-1".to_owned(),
        format!("--action={action}"),
    ])
    .expect("flags");
    let tree = Path::new("/nonexistent-tree");
    assert_eq!(finish(&cli, tree, &root).await, ExitCode::from(EXIT_SETUP));

    let mut cli = cli;
    cli.out_dir = Some(root.to_path_buf());
    cli.lease = "not/a/build".to_owned();
    assert_eq!(finish(&cli, tree, &root).await, ExitCode::from(EXIT_SETUP));
    assert!(!root.join("kbf-work").exists());
}

/// Proves the exit code of a run through `finish`: 0 when the object is committed, and 10 for a
/// tree that cannot be read.
#[tokio::test]
async fn finish_exits_0_for_a_committed_run_and_10_for_an_unreadable_tree() {
    let job = job("finish", &command(&["true"], "", &[]));
    assert_eq!(
        finish(&job.cli(), &job.tree, &job.root).await,
        ExitCode::SUCCESS
    );
    assert!(job.put.join("pack").is_file());

    let other = scratch("mat-finish-other");
    let out = scratch("mat-finish-out");
    let mut cli = job.cli();
    cli.out_dir = Some(out.to_path_buf());
    assert_eq!(
        finish(&cli, &job.tree, &other).await,
        ExitCode::SUCCESS,
        "a second run in a fresh job root"
    );
    std::fs::remove_dir_all(job.tree.join("kbf-work")).expect("remove");
    let again = scratch("mat-finish-again");
    assert_eq!(
        finish(&cli, &job.tree, &again).await,
        ExitCode::from(EXIT_FETCH)
    );
}

/// The flags of a job with `extra` appended to the action, lease and put directory.
fn parse(extra: &[&str]) -> Result<Cli, clap::Error> {
    let action = format!("--action={}/12", "ab".repeat(32));
    let mut argv = vec![
        "kbf-task",
        "--stage-and-commit",
        "--lease=l",
        "--out-dir=o",
        &action,
    ];
    argv.extend_from_slice(extra);
    Cli::try_parse_from(argv)
}

/// Proves that exactly one of `--tree` and `--materialize-work` is accepted (clap's exit 2), and
/// that the tree must follow `=`.
#[test]
fn exactly_one_of_tree_and_materialize_work_is_accepted() {
    let both = parse(&["--tree=/", "--materialize-work=t"])
        .err()
        .expect("both");
    assert_eq!(both.kind(), ClapKind::ArgumentConflict);
    assert_eq!(both.exit_code(), 2);
    let neither = parse(&[]).err().expect("neither");
    assert_eq!(neither.kind(), ClapKind::MissingRequiredArgument);
    assert_eq!(neither.exit_code(), 2);
    assert!(parse(&["--materialize-work", "t"]).is_err());
    assert!(parse(&["--materialize-work"]).is_err());
    let only = parse(&["--materialize-work=t"]).expect("flags");
    assert_eq!(only.materialize_work.as_deref(), Some(Path::new("t")));
    assert_eq!(only.tree, None);
}

/// Proves that the command line the Mac job is started with parses to a materialized job over
/// the relative `.kbf-tree`, committing into `.kbf-out`.
#[test]
fn the_mac_jobs_command_line_parses() {
    let action = format!("{}/12", "ab".repeat(32));
    let cli = Cli::try_parse_from([
        "kbf-task",
        "--materialize-work=.kbf-tree",
        "--stage-and-commit",
        "--out-dir",
        ".kbf-out",
        "--action",
        &action,
        "--lease",
        "7-3",
        "--deadline-ms",
        "90000",
        "--object-format",
        "one-pass",
    ])
    .expect("flags");
    assert_eq!(cli.materialize_work, Some(PathBuf::from(".kbf-tree")));
    assert_eq!(cli.tree, None);
    assert_eq!(cli.out_dir(), Path::new(".kbf-out"));
    assert_eq!(cli.deadline_ms, 90_000);
}

/// Proves that a tree an older frontend wrote, its input root at `work` and no `kbf-work`, is
/// copied into `<root>/work` and run there, and that nothing named `kbf-work` is made; and that a
/// tree holding both names is copied from `kbf-work` alone.
/// Catches: a Mac task that refuses the trees older frontends wrote, copies them under another
/// name, or prefers the older name when both are there.
#[tokio::test]
async fn an_older_trees_work_is_copied_under_its_own_name_and_kbf_work_wins_over_it() {
    let older = job("older-work", &command(&["cp", "data", "out"], "", &["out"]));
    std::fs::remove_dir(older.tree.join("kbf-work")).expect("remove");
    std::fs::create_dir(older.tree.join("work")).expect("work");
    std::fs::write(older.tree.join("work/data"), b"older\n").expect("data");
    let manifest = older.run().await.expect("task");
    assert_eq!(manifest.output_files[0].digest, sha256(b"older\n"));
    assert_eq!(
        std::fs::read(older.root.join("work/out")).expect("out"),
        b"older\n"
    );
    assert!(!older.root.join("kbf-work").exists());

    let both = job("both-names", &command(&["cp", "data", "out"], "", &["out"]));
    std::fs::write(both.tree.join("kbf-work/data"), b"new\n").expect("data");
    std::fs::create_dir(both.tree.join("work")).expect("work");
    std::fs::write(both.tree.join("work/data"), b"older\n").expect("older data");
    let manifest = both.run().await.expect("task");
    assert_eq!(manifest.output_files[0].digest, sha256(b"new\n"));
    assert!(both.root.join("kbf-work/out").is_file());
    assert!(!both.root.join("work").exists());
}
