use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser as _;
use clap::error::ErrorKind as ClapKind;
use kbf_proto::reapi::command::EnvironmentVariable;
use kbf_proto::reapi::{self, Directory, FileNode, Platform, platform::Property};
use kbf_segments::sha256;
use kbf_task_pack::{MANIFEST_VERSION, MANIFEST_VERSION_2, read_manifest};
use kbf_testkit::dir::TestDir;
use kbf_types::Digest;
use prost::Message as _;

use super::*;
use crate::fetch::{BlobSource, label};
use crate::task::recording_sink::Fake;
use crate::task::{EXIT_FETCH, EXIT_RUN, EXIT_SETUP, main_with};

/// A fresh scratch directory standing in for a job's root.
fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-task-tree-{name}"))
}

/// A Command of `argv` in `working_directory`, declaring `outputs`, with a `PATH`.
fn command(argv: &[&str], working_directory: &str, outputs: &[&str]) -> reapi::Command {
    reapi::Command {
        arguments: argv.iter().map(|a| (*a).to_owned()).collect(),
        environment_variables: vec![EnvironmentVariable {
            name: "PATH".to_owned(),
            value: "/usr/bin:/bin".to_owned(),
        }],
        output_paths: outputs.iter().map(|o| (*o).to_owned()).collect(),
        working_directory: working_directory.to_owned(),
        ..Default::default()
    }
}

/// The input root the trees of these tests hold under `work`: one file `data`, `hi\n`.
fn input_root() -> Directory {
    Directory {
        files: vec![FileNode {
            name: "data".to_owned(),
            digest: Some(digest::to_reapi(&sha256(b"hi\n"))),
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// The Action of `command` over [`input_root`].
fn action_of(command: &reapi::Command) -> reapi::Action {
    reapi::Action {
        command_digest: Some(digest::to_reapi(&sha256(&command.encode_to_vec()))),
        input_root_digest: Some(digest::to_reapi(&sha256(&input_root().encode_to_vec()))),
        ..Default::default()
    }
}

/// Lays out a job's root under `root` as the frontend's tree holds it: `.kbf/action`,
/// `.kbf/command` and `work/data`. Returns the Action's digest.
fn lay_out(root: &Path, action: &reapi::Action, command: &reapi::Command) -> Digest {
    std::fs::create_dir_all(root.join(".kbf")).expect(".kbf");
    std::fs::create_dir_all(root.join("work")).expect("work");
    std::fs::write(root.join("work/data"), b"hi\n").expect("data");
    let action = action.encode_to_vec();
    std::fs::write(root.join(ACTION_FILE), &action).expect("action");
    std::fs::write(root.join(COMMAND_FILE), command.encode_to_vec()).expect("command");
    sha256(&action)
}

/// Copies `data` to `out`.
const COPY: &[&str] = &["dd", "if=data", "of=out", "status=none"];

/// A tree under `name` whose Command copies `data` to `out`; its root and its Action's digest.
fn copy_tree(name: &str) -> (TestDir, Digest) {
    let dir = scratch(name);
    let command = command(COPY, "", &["out"]);
    let digest = lay_out(&dir, &action_of(&command), &command);
    (dir, digest)
}

/// The flags of a tree job over `root` running `action`, with `extra` flags appended (a pack
/// URL among them).
fn flags(root: &Path, action: &Digest, extra: &[&str]) -> Result<Cli, clap::Error> {
    let action = format!("--action={action}");
    let root = format!("--tree={}", root.display());
    let mut args = vec!["kbf-task", &root, "--lease=lease-1", &action];
    args.extend_from_slice(extra);
    Cli::try_parse_from(args)
}

/// A pack URL file that is not there: the sinks of these tests are fakes.
const NO_URL: &str = "--pack-url-file=/nonexistent-url";

fn tree_cli(root: &Path, action: &Digest) -> Cli {
    flags(root, action, &[NO_URL]).expect("flags")
}

/// The root `--tree` names in `cli`.
fn root_of(cli: &Cli) -> &Path {
    cli.tree.as_deref().expect("a tree job")
}

/// Runs the tree job `cli` names into a sink of one pack slot; the outcome and the sink.
async fn run_tree(cli: &Cli) -> (Result<TaskManifest, Failure>, Fake) {
    let sink = Fake::new(false);
    let outcome = execute(cli, root_of(cli), &sink).await;
    (outcome, sink)
}

/// The failure of a tree job that must not have run its Command or stored anything: its run
/// directory was never made and the sink holds nothing.
async fn refused(cli: &Cli) -> Failure {
    let (outcome, sink) = run_tree(cli).await;
    let failure = outcome.expect_err("refused");
    assert!(!root_of(cli).join(RUN_DIR).exists(), "{failure}");
    assert!(sink.tried.lock().unwrap().is_empty(), "{failure}");
    failure
}

/// The canonical JSON of the advisory `inputs` of a tree job, as it ends a manifest.
const TREE_INPUTS: &str = concat!(
    r#","packs":1,"large":[],"inputs":{"blobs":0,"files":0,"directories":0,"requests":0,"#,
    r#""retries":0,"max_in_flight":0,"bytes_written_scratch":0,"walk_ms":0,"fetch_ms":0,"#,
    r#""hash_ms":0,"write_ms":0,"request_p50_ms":0,"request_p99_ms":0,"#,
    r#""bytes_by_source":{"listener_get":0,"listener_batch":0,"cache_hit":0,"tierstore_direct":0},"#,
    r#""from":"tree"}}"#,
);

/// Asserts the whole tree job: the Action and the Command are read from `.kbf`, the Command runs
/// in `work` over its input, its output is collected from there, the streams go to `.kbf-run`,
/// and the stored manifest says the inputs came from the tree. Nothing is laid out under a
/// work directory (`--work-dir` is refused with `--tree`, and its default is not touched).
#[tokio::test]
async fn a_tree_run_reads_the_action_and_command_from_kbf_and_fetches_nothing() {
    let (dir, digest) = copy_tree("e2e");
    let cli = tree_cli(&dir, &digest);
    let (outcome, sink) = run_tree(&cli).await;
    let manifest = outcome.expect("task");

    assert_eq!(manifest.action, digest);
    assert_eq!(manifest.exit_code, 0);
    assert_eq!(manifest.output_files.len(), 1);
    assert_eq!(manifest.output_files[0].path, "out");
    assert_eq!(manifest.output_files[0].digest, sha256(b"hi\n"));
    assert_eq!(manifest.inputs, Some(input_stats()));
    assert_eq!(manifest.version(), MANIFEST_VERSION_2);
    assert_eq!(std::fs::read(dir.join("work/out")).expect("out"), b"hi\n");
    assert!(dir.join(".kbf-run/stdout").is_file());
    assert!(dir.join(".kbf-run/stderr").is_file());
    assert!(dir.join(".kbf-run/private/home").is_dir());
    let stored = sink.stored.lock().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(read_manifest(&stored[0]).expect("read"), manifest);
}

/// Asserts the new data against the reader of the release before this one (kbf-task-pack's
/// manifest reader, which learnt `from` and to skip unknown members one release earlier and is
/// not changed here): the manifest a tree job stores ends with exactly the `inputs` it means,
/// `"from":"tree"` included, and that reader reads it back with `from` as `tree`.
#[tokio::test]
async fn a_manifest_with_from_is_read_by_the_item_8_reader() {
    let (dir, digest) = copy_tree("from-tree");
    let (outcome, sink) = run_tree(&tree_cli(&dir, &digest)).await;
    let manifest = outcome.expect("task");
    let json = String::from_utf8(manifest.to_json().expect("json")).expect("utf-8");
    assert!(json.ends_with(TREE_INPUTS), "{json}");
    assert!(json.starts_with(r#"{"version":2,"#), "{json}");

    let read = TaskManifest::parse(json.as_bytes()).expect("parse");
    assert_eq!(read.inputs.and_then(|i| i.from), Some(InputsFrom::Tree));
    let stored = sink.stored.lock().unwrap();
    let from_pack = read_manifest(&stored[0]).expect("read the pack");
    assert_eq!(
        from_pack.inputs.and_then(|i| i.from),
        Some(InputsFrom::Tree)
    );
}

/// Asserts the old data against this release: a manifest with no `inputs` (what today's job
/// writes, version 1) and one whose `inputs` has no `from` (version 2, as the writers before tree
/// jobs spell it) are each read, with no `from`.
#[tokio::test]
async fn a_manifest_without_from_is_read_by_this_release() {
    let (dir, digest) = copy_tree("no-from");
    let (outcome, _sink) = run_tree(&tree_cli(&dir, &digest)).await;
    let mut manifest = outcome.expect("task");

    manifest.inputs = None;
    let v1 = manifest.to_json().expect("v1");
    assert!(v1.starts_with(br#"{"version":1,"#));
    let read = TaskManifest::parse(&v1).expect("parse v1");
    assert_eq!((read.version(), read.inputs), (MANIFEST_VERSION, None));

    manifest.inputs = Some(InputStats {
        blobs: 3,
        ..InputStats::default()
    });
    let v2 = String::from_utf8(manifest.to_json().expect("v2")).expect("utf-8");
    assert!(!v2.contains("\"from\""), "{v2}");
    let read = TaskManifest::parse(v2.as_bytes()).expect("parse v2");
    assert_eq!(read.inputs.map(|i| (i.blobs, i.from)), Some((3, None)));
}

/// Blobs held in memory by digest: a read grant for today's job.
#[derive(Default)]
struct Mem(HashMap<String, Vec<u8>>);

impl Mem {
    fn add(&mut self, bytes: Vec<u8>) {
        self.0
            .insert(label(&digest::to_reapi(&sha256(&bytes))), bytes);
    }
}

impl BlobSource for Mem {
    fn get(&self, digest: &reapi::Digest) -> Result<Vec<u8>, FetchError> {
        // Every blob the test's fetch asks for is held.
        Ok(self.0[&label(digest)].clone())
    }
}

/// Asserts that today's job is unchanged by tree jobs: the same action fetched through a grant
/// writes no `inputs` and its manifest stays version 1, the bytes the frontend read before.
#[tokio::test]
async fn todays_job_writes_no_inputs_and_stays_version_1() {
    let command = command(COPY, "", &["out"]);
    let action = action_of(&command);
    let mut mem = Mem::default();
    mem.add(b"hi\n".to_vec());
    mem.add(input_root().encode_to_vec());
    mem.add(command.encode_to_vec());
    mem.add(action.encode_to_vec());
    let dir = scratch("todays");
    let action = sha256(&action.encode_to_vec()).to_string();
    let cli = Cli::try_parse_from([
        "kbf-task",
        "--pack-url-file=/nonexistent-url",
        "--grant-url=https://grant.example",
        "--grant-token-file=/nonexistent",
        "--lease=lease-1",
        &format!("--action={action}"),
        &format!("--work-dir={}", dir.display()),
    ])
    .expect("flags");
    let manifest = crate::task::execute(&cli, &mem, &Fake::new(false))
        .await
        .expect("task");
    assert_eq!(manifest.inputs, None);
    assert_eq!(manifest.version(), MANIFEST_VERSION);
    assert_eq!(manifest.output_files[0].digest, sha256(b"hi\n"));
}

/// Asserts that a job never runs another action's tree: an `.kbf/action` that is not the Action
/// `--action` names is exit 10, with a line naming the digest, what was read and where, and
/// nothing runs. Each way it can differ is its own line: the same size and another hash, one
/// byte longer, one byte shorter.
#[tokio::test]
async fn an_action_whose_digest_is_not_the_flags_is_exit_10_with_its_line() {
    let command = command(COPY, "", &["out"]);
    let ran = action_of(&command);
    let salted = |salt: &[u8]| {
        reapi::Action {
            salt: salt.to_vec(),
            ..ran.clone()
        }
        .encode_to_vec()
    };
    // `--action` names this one; the file holds another.
    let named_bytes = salted(b"named");
    let named = sha256(&named_bytes);
    let len = named_bytes.len();
    let same_length = salted(b"other");
    let cases = [
        (
            same_length.clone(),
            format!("its SHA-256 is {}", hex::encode(sha256(&same_length).hash)),
        ),
        (
            [named_bytes.as_slice(), b"\0"].concat(),
            format!("it is longer than {len} bytes"),
        ),
        (
            named_bytes[..len - 1].to_vec(),
            format!("it has {} bytes", len - 1),
        ),
    ];
    for (i, (bytes, why)) in cases.into_iter().enumerate() {
        let dir = scratch(&format!("wrong-action-{i}"));
        lay_out(&dir, &ran, &command);
        std::fs::write(dir.join(ACTION_FILE), &bytes).expect("action");
        let failure = refused(&tree_cli(&dir, &named)).await;
        assert_eq!(failure.exit_code(), EXIT_FETCH);
        assert_eq!(
            failure.to_string(),
            format!(
                "fetch: blob {} does not match its digest: {why}, as read from {}",
                label(&digest::to_reapi(&named)),
                dir.join(ACTION_FILE).display()
            )
        );
        assert!(!dir.join("work/out").exists());
    }
}

/// Asserts that a `.kbf/command` that is not the Command the Action names is exit 10 with its
/// line, and nothing runs.
#[tokio::test]
async fn a_command_that_is_not_the_actions_is_exit_10_with_its_line() {
    let dir = scratch("wrong-command");
    let command = command(COPY, "", &["out"]);
    let action = action_of(&command);
    let digest = lay_out(&dir, &action, &command);
    // Another Command of the same length: `status=nonf` for `status=none`.
    let swapped =
        self::command(&["dd", "if=data", "of=out", "status=nonf"], "", &["out"]).encode_to_vec();
    std::fs::write(dir.join(COMMAND_FILE), &swapped).expect("command");
    let failure = refused(&tree_cli(&dir, &digest)).await;
    assert_eq!(failure.exit_code(), EXIT_FETCH);
    let named = action.command_digest.expect("command digest");
    assert_eq!(
        failure.to_string(),
        format!(
            "fetch: blob {} does not match its digest: its SHA-256 is {}, as read from {}",
            label(&named),
            hex::encode(sha256(&swapped).hash),
            dir.join(COMMAND_FILE).display()
        )
    );
}

/// Asserts that a missing `.kbf/action` or `.kbf/command` is exit 10 naming the file, and
/// nothing runs.
#[tokio::test]
async fn a_missing_action_or_command_file_is_exit_10_naming_it() {
    for file in [ACTION_FILE, COMMAND_FILE] {
        let (dir, digest) = copy_tree(&format!("missing-{}", file.replace('/', "-")));
        std::fs::remove_file(dir.join(file)).expect("remove");
        let failure = refused(&tree_cli(&dir, &digest)).await;
        assert_eq!(failure.exit_code(), EXIT_FETCH);
        assert!(
            matches!(&failure, Failure::Fetch(FetchError::Io { path, .. }) if *path == dir.join(file)),
            "{failure}"
        );
    }
}

/// Asserts that an `.kbf/action` or `.kbf/command` that opens but cannot be read (a directory)
/// is exit 10 naming the file, and nothing runs.
#[tokio::test]
async fn an_action_or_command_that_is_a_directory_is_exit_10_naming_it() {
    for file in [ACTION_FILE, COMMAND_FILE] {
        let (dir, digest) = copy_tree(&format!("dir-{}", file.replace('/', "-")));
        std::fs::remove_file(dir.join(file)).expect("remove");
        std::fs::create_dir(dir.join(file)).expect("a directory in its place");
        let failure = refused(&tree_cli(&dir, &digest)).await;
        assert_eq!(failure.exit_code(), EXIT_FETCH);
        assert!(
            matches!(&failure, Failure::Fetch(FetchError::Io { path, .. }) if *path == dir.join(file)),
            "{failure}"
        );
    }
}

/// Asserts that an `.kbf/action` whose bytes are the digest's but do not decode as an Action is
/// exit 10, and nothing runs.
#[tokio::test]
async fn an_action_that_does_not_decode_is_exit_10() {
    let (dir, _) = copy_tree("undecodable");
    // Field 1 with wire type 7, which no message has.
    let garbage = b"\x0f".to_vec();
    std::fs::write(dir.join(ACTION_FILE), &garbage).expect("action");
    let failure = refused(&tree_cli(&dir, &sha256(&garbage))).await;
    assert_eq!(failure.exit_code(), EXIT_FETCH);
    assert!(failure.to_string().contains("does not decode"), "{failure}");
}

/// Asserts that an `--action` naming more than a message may be is refused before the file is
/// opened (there is none), and that one at the bound is not refused for its size.
#[tokio::test]
async fn an_action_over_the_message_bound_is_refused_before_it_is_read() {
    let (dir, digest) = copy_tree("too-large");
    std::fs::remove_file(dir.join(ACTION_FILE)).expect("remove");
    let bound = u64::try_from(crate::fetch::MAX_MESSAGE_BYTES).expect("bound");
    let over = Digest::new(digest.function, digest.hash, bound + 1);
    let failure = refused(&tree_cli(&dir, &over)).await;
    assert!(
        matches!(&failure, Failure::Fetch(FetchError::Invalid(why)) if why.contains("over the")),
        "{failure}"
    );
    let at = Digest::new(digest.function, digest.hash, bound);
    let failure = refused(&tree_cli(&dir, &at)).await;
    assert!(
        matches!(&failure, Failure::Fetch(FetchError::Io { .. })),
        "{failure}"
    );
}

/// Asserts that an Action naming no Command or no input root is exit 10, and nothing runs.
#[tokio::test]
async fn an_action_without_a_command_or_an_input_root_is_exit_10() {
    let command = command(COPY, "", &["out"]);
    for (i, action) in [
        reapi::Action {
            command_digest: None,
            ..action_of(&command)
        },
        reapi::Action {
            input_root_digest: None,
            ..action_of(&command)
        },
    ]
    .into_iter()
    .enumerate()
    {
        let dir = scratch(&format!("unnamed-{i}"));
        let digest = lay_out(&dir, &action, &command);
        let failure = refused(&tree_cli(&dir, &digest)).await;
        assert_eq!(failure.exit_code(), EXIT_FETCH);
        assert!(
            matches!(&failure, Failure::Fetch(FetchError::Invalid(why)) if why.starts_with("the Action names no")),
            "{failure}"
        );
    }
}

/// A tree under `name` whose `work` is removed; its root, its Action's digest and where `work`
/// was.
fn without_work(name: &str) -> (TestDir, Digest, PathBuf) {
    let (dir, digest) = copy_tree(name);
    let work = dir.join("work");
    std::fs::remove_dir_all(&work).expect("remove");
    (dir, digest, work)
}

/// Asserts that a tree with no `work` is exit 10 naming it, and nothing runs.
#[tokio::test]
async fn a_missing_input_root_is_exit_10_naming_it() {
    let (dir, digest, work) = without_work("work-missing");
    let failure = refused(&tree_cli(&dir, &digest)).await;
    assert_eq!(failure.exit_code(), EXIT_FETCH);
    assert!(
        matches!(&failure, Failure::Fetch(FetchError::Io { path, .. }) if *path == work),
        "{failure}"
    );
}

/// Asserts that a `work` that is a file, or a symlink to a directory (not followed), is exit 10
/// with its line, and nothing runs.
#[tokio::test]
async fn an_input_root_that_is_a_file_or_a_symlink_is_exit_10() {
    let (file_dir, file_digest, file_work) = without_work("work-file");
    std::fs::write(&file_work, b"not a directory").expect("file");
    let (link_dir, link_digest, link_work) = without_work("work-symlink");
    std::fs::create_dir(link_dir.join("elsewhere")).expect("elsewhere");
    std::os::unix::fs::symlink("elsewhere", &link_work).expect("symlink");
    for (dir, digest, work) in [
        (&file_dir, file_digest, file_work),
        (&link_dir, link_digest, link_work),
    ] {
        let failure = refused(&tree_cli(dir, &digest)).await;
        assert_eq!(failure.exit_code(), EXIT_FETCH);
        assert_eq!(
            failure.to_string(),
            format!(
                "fetch: {}: the tree's input root is not a directory",
                work.display()
            )
        );
    }
}

/// Asserts that a run directory that already exists is refused as a run start (exit 11) and the
/// Command does not run: a run never reuses or follows what is there.
#[tokio::test]
async fn an_existing_run_directory_is_refused_as_a_run_start() {
    let dir = scratch("run-dir-exists");
    let command = command(&["touch", "ran"], "", &[]);
    let digest = lay_out(&dir, &action_of(&command), &command);
    std::fs::create_dir(dir.join(RUN_DIR)).expect("run dir");
    let sink = Fake::new(false);
    let failure = execute(&tree_cli(&dir, &digest), &dir, &sink)
        .await
        .expect_err("refused");
    assert_eq!(failure.exit_code(), EXIT_RUN);
    assert!(
        failure
            .to_string()
            .contains("cannot make the run directory (an existing one is refused)"),
        "{failure}"
    );
    assert!(!dir.join("work/ran").exists());
    assert!(sink.tried.lock().unwrap().is_empty());
}

/// Asserts that a refused network property fails the run start (exit 11) before the run
/// directory is made, as it fails today's job before the inputs are fetched.
#[tokio::test]
async fn a_refused_network_property_is_a_run_start_before_the_run_directory() {
    let dir = scratch("network-refused");
    let command = command(&["touch", "ran"], "", &[]);
    let action = reapi::Action {
        platform: Some(Platform {
            properties: vec![Property {
                name: crate::network::PROPERTY.to_owned(),
                value: "sometimes".to_owned(),
            }],
        }),
        ..action_of(&command)
    };
    let digest = lay_out(&dir, &action, &command);
    let failure = refused(&tree_cli(&dir, &digest)).await;
    assert_eq!(failure.exit_code(), EXIT_RUN);
    assert!(!dir.join("work/ran").exists());
}

/// Asserts that the working directory and the parent of every output path are made under `work`
/// before the Command starts: `touch` makes no directory, so it succeeds only if they exist, and
/// its output is collected relative to the working directory.
#[tokio::test]
async fn the_working_directory_and_output_parents_exist_before_the_command_starts() {
    let dir = scratch("parents");
    let command = command(&["touch", "o/p/f"], "w/x", &["o/p/f"]);
    let digest = lay_out(&dir, &action_of(&command), &command);
    let (outcome, _sink) = run_tree(&tree_cli(&dir, &digest)).await;
    let manifest = outcome.expect("task");
    assert_eq!(manifest.exit_code, 0);
    assert_eq!(manifest.output_files.len(), 1);
    assert_eq!(manifest.output_files[0].path, "o/p/f");
    assert!(dir.join("work/w/x/o/p/f").is_file());
}

/// Asserts that the run's streams and its private directories are in `.kbf-run`, beside `work`
/// and not in it: the Command's `HOME` and `TMPDIR` name directories there, its standard output
/// is there, and `work` holds nothing new.
#[tokio::test]
async fn the_streams_and_home_are_in_the_run_directory_and_not_outputs() {
    let dir = scratch("run-dir");
    let command = command(&["env"], "", &[]);
    let digest = lay_out(&dir, &action_of(&command), &command);
    let (outcome, _sink) = run_tree(&tree_cli(&dir, &digest)).await;
    let manifest = outcome.expect("task");
    assert_eq!(manifest.exit_code, 0);
    let stdout = std::fs::read(dir.join(".kbf-run/stdout")).expect("stdout");
    assert_eq!(manifest.stdout, Some(sha256(&stdout)));
    let printed = String::from_utf8(stdout).expect("utf-8");
    let private = dir.join(".kbf-run/private");
    for line in [
        format!("HOME={}", private.join("home").display()),
        format!("TMPDIR={}", private.join("tmp").display()),
    ] {
        assert!(printed.lines().any(|l| l == line), "{line} in {printed}");
    }
    assert!(private.join("home").is_dir());
    let work: Vec<_> = std::fs::read_dir(dir.join("work"))
        .expect("work")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(work, ["data"]);
    assert!(manifest.output_files.is_empty());
}

/// Asserts that a sink with more pack slots than a manifest can name is a setup failure before
/// the tree is read.
#[tokio::test]
async fn too_many_slots_are_a_setup_failure_before_the_tree_is_read() {
    let (dir, digest) = copy_tree("slots");
    let sink = Fake::with_slots(usize::from(kbf_task_pack::MAX_PACKS) + 1, 0);
    let failure = execute(&tree_cli(&dir, &digest), &dir, &sink)
        .await
        .expect_err("slots");
    assert_eq!(failure.exit_code(), EXIT_SETUP);
    assert!(!dir.join(RUN_DIR).exists());
}

/// Asserts the flags of a tree job: `--tree` takes its root after `=` and is `/` alone; no read
/// grant is needed; every flag of the grant and of fetching, and `--work-dir`, is refused with
/// `--tree` (their defaults are not); without `--tree` the grant URL and its token are still
/// required.
#[test]
fn a_tree_job_needs_no_grant_and_refuses_the_fetch_flags() {
    let action = sha256(b"a");
    let tree = flags(Path::new("/t"), &action, &[NO_URL]).expect("tree flags");
    assert_eq!(tree.tree, Some(PathBuf::from("/t")));
    assert_eq!(tree.grant_url, None);
    let action = format!("--action={action}");
    let bare =
        Cli::try_parse_from(["kbf-task", "--tree", NO_URL, "--lease=l", &action]).expect("bare");
    assert_eq!(bare.tree, Some(PathBuf::from("/")));

    for flag in [
        "--grant-url=https://g",
        "--grant-token-file=t",
        "--grant-token=t",
        "--fetch-concurrency=4",
        "--fetch-batch",
        "--allow-plain-http-grant",
        "--locate",
        "--work-dir=/w",
    ] {
        let error = flags(Path::new("/t"), &sha256(b"a"), &[NO_URL, flag])
            .err()
            .expect(flag);
        assert_eq!(error.kind(), ClapKind::ArgumentConflict, "{flag}");
    }

    for (args, kind) in [
        (
            vec![NO_URL, "--lease=l", &action],
            ClapKind::MissingRequiredArgument,
        ),
        (
            vec![NO_URL, "--lease=l", &action, "--grant-url=g"],
            ClapKind::MissingRequiredArgument,
        ),
        // The root must follow `=`: a separate word is not taken for it.
        (
            vec![NO_URL, "--lease=l", &action, "--tree", "/t"],
            ClapKind::UnknownArgument,
        ),
    ] {
        let error = Cli::try_parse_from(std::iter::once("kbf-task").chain(args.iter().copied()))
            .err()
            .expect("refused");
        assert_eq!(error.kind(), kind, "{args:?}");
    }
    let today = Cli::try_parse_from([
        "kbf-task",
        NO_URL,
        "--lease=l",
        &action,
        "--grant-url=g",
        "--grant-token-file=t",
    ])
    .expect("today's job");
    assert_eq!(today.tree, None);
}

/// A store on loopback that takes one PUT, answers 201 and hands back its body.
fn store() -> (String, std::thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/pack", listener.local_addr().expect("addr"));
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut got = Vec::new();
        let mut chunk = [0_u8; 4096];
        while !got.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut chunk).expect("read");
            assert_ne!(n, 0, "the request ended early");
            got.extend_from_slice(&chunk[..n]);
        }
        let end = got
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("the end of the head");
        let len = String::from_utf8_lossy(&got[..end])
            .to_ascii_lowercase()
            .lines()
            .find_map(|l| l.strip_prefix("content-length:").map(str::to_owned))
            .expect("a content length")
            .trim()
            .parse::<usize>()
            .expect("a length");
        let head = end + 4;
        while got.len() < head + len {
            let n = stream.read(&mut chunk).expect("read");
            assert_ne!(n, 0, "the body ended early");
            got.extend_from_slice(&chunk[..n]);
        }
        stream
            .write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("reply");
        got.split_off(head)
    });
    (url, server)
}

/// Asserts that the real entry point runs a tree job with no grant: it PUTs the pack to the
/// presigned URL and exits 0, and the pack's manifest says the inputs came from the tree.
#[tokio::test(flavor = "multi_thread")]
async fn main_with_runs_a_tree_job_and_puts_its_pack() {
    let (dir, digest) = copy_tree("main");
    let (url, server) = store();
    let url_file = dir.join("pack-url");
    std::fs::write(&url_file, &url).expect("url file");
    let url_flag = format!("--pack-url-file={}", url_file.display());
    let cli = flags(&dir, &digest, &[&url_flag, "--allow-plain-http-put"]).expect("flags");
    assert_eq!(main_with(&cli).await, ExitCode::SUCCESS);
    let body = server.join().expect("store");
    let manifest = read_manifest(&body).expect("the stored pack");
    assert_eq!(manifest.inputs, Some(input_stats()));
    assert_eq!(manifest.output_files[0].digest, sha256(b"hi\n"));
}

/// Asserts that the setup of a tree job fails before the tree is read, each as exit 14: a lease
/// the frontend would refuse, a credential in argv without the opt-in, an unreadable root
/// certificate, a pack URL file that is not there.
#[tokio::test]
async fn a_tree_job_whose_setup_fails_exits_14_before_the_tree_is_read() {
    let (dir, digest) = copy_tree("setup");
    let root = format!("--tree={}", dir.display());
    let action = format!("--action={digest}");
    // Paths in the test's own directory that are not there, read as the setup reads them.
    let no_url = format!("--pack-url-file={}", dir.join("absent-url").display());
    let no_der = format!("--root-der={}", dir.join("absent.der").display());
    let url_file = dir.join("pack-url");
    std::fs::write(&url_file, "https://store.example/p").expect("url file");
    let url = format!("--pack-url-file={}", url_file.display());
    for case in [
        vec![url.as_str(), "--lease=not a build id"],
        vec!["--pack-url=https://store.example/p", "--lease=lease-1"],
        vec![url.as_str(), "--lease=lease-1", no_der.as_str()],
        vec![no_url.as_str(), "--lease=lease-1"],
    ] {
        let mut args = vec!["kbf-task", &root, &action];
        args.extend_from_slice(&case);
        let cli = Cli::try_parse_from(args).expect("flags");
        assert_eq!(
            main_with(&cli).await,
            ExitCode::from(EXIT_SETUP),
            "{case:?}"
        );
        assert!(!dir.join(RUN_DIR).exists(), "{case:?}");
    }
    let failed = Err(Failure::Setup("no".to_owned()));
    assert_eq!(
        finish(&tree_cli(&dir, &digest), &dir, failed).await,
        ExitCode::from(EXIT_SETUP)
    );
}

/// Asserts what a tree job hands the sandbox to hide from its Command: the job's grants and the
/// task's run directory `.kbf-run` (its streams; `private/` is bound again), and not `work`, the
/// input root the Command runs in; nothing of today's `--work-dir`.
#[test]
fn a_tree_job_hides_the_grants_and_its_run_directory() {
    let (dir, digest) = copy_tree("hidden");
    let inputs = read(&tree_cli(&dir, &digest), &dir).expect("read");
    assert_eq!(
        inputs.hidden,
        [PathBuf::from(GRANTS_DIR), dir.join(RUN_DIR)]
    );
    assert_eq!(inputs.root, dir.join("work"));
    assert_eq!(inputs.run_dir, dir.join(RUN_DIR));
}
