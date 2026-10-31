use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Mutex;

use kbf_proto::reapi::command::EnvironmentVariable;
use kbf_proto::reapi::{Digest, Directory, DirectoryNode, FileNode, SymlinkNode};
use kbf_segments::{SegmentReader, sha256};
use kbf_task_pack::{EMPTY_DIGEST, read_manifest};
use kbf_testkit::dir::TestDir;
use prost::Message;

use kbf_task_pack::ManifestError;

use super::recording_sink::Fake;
use super::*;
use crate::fetch::label;
use crate::put::FixedJitter;

/// The REAPI digest of `bytes`.
fn reapi_of(bytes: &[u8]) -> Digest {
    digest::to_reapi(&sha256(bytes))
}

/// Blobs held in memory by digest.
#[derive(Default)]
struct Mem(HashMap<String, Vec<u8>>);

impl Mem {
    fn add(&mut self, bytes: Vec<u8>) -> Digest {
        let digest = reapi_of(&bytes);
        self.0.insert(label(&digest), bytes);
        digest
    }
}

impl BlobSource for Mem {
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
        self.0
            .get(&label(digest))
            .cloned()
            .ok_or_else(|| FetchError::Source {
                digest: label(digest),
                why: "absent".to_owned(),
            })
    }
}

/// A fresh scratch directory for one test, beside the test binary.
fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-task-{name}"))
}

fn cli(work_dir: &Path, action: Digest) -> Cli {
    let id = digest::from_reapi(&action).expect("digest");
    Cli::try_parse_from([
        "kbf-task",
        "--pack-url-file",
        "/nonexistent-url",
        "--grant-url",
        "https://grant.example",
        "--grant-token-file",
        "/nonexistent",
        "--lease",
        "lease-1",
        "--action",
        &id.to_string(),
        "--work-dir",
        work_dir.to_str().expect("utf-8"),
    ])
    .expect("flags")
}

/// An Action whose Command is `argv` with `outputs`, over an input root holding `data` and
/// `src`, a directory of `d/f` and a symlink `link` to `d/f`.
fn action(mem: &mut Mem, argv: &[&str], outputs: &[&str]) -> Digest {
    let data = mem.add(b"hi\n".to_vec());
    let x = mem.add(b"x\n".to_vec());
    let file = |name: &str, digest: Digest| FileNode {
        name: name.to_owned(),
        digest: Some(digest),
        is_executable: false,
        ..Default::default()
    };
    let sub = |name: &str, directory: Directory, mem: &mut Mem| DirectoryNode {
        name: name.to_owned(),
        digest: Some(mem.add(directory.encode_to_vec())),
    };
    let d = sub(
        "d",
        Directory {
            files: vec![file("f", x)],
            ..Default::default()
        },
        mem,
    );
    let src = sub(
        "src",
        Directory {
            directories: vec![d],
            symlinks: vec![SymlinkNode {
                name: "link".to_owned(),
                target: "d/f".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        },
        mem,
    );
    let root = mem.add(
        Directory {
            files: vec![file("data", data)],
            directories: vec![src],
            ..Default::default()
        }
        .encode_to_vec(),
    );
    let command = mem.add(
        Command {
            arguments: argv.iter().map(|a| (*a).to_owned()).collect(),
            environment_variables: vec![EnvironmentVariable {
                name: "PATH".to_owned(),
                value: "/usr/bin:/bin".to_owned(),
            }],
            output_paths: outputs.iter().map(|o| (*o).to_owned()).collect(),
            ..Default::default()
        }
        .encode_to_vec(),
    );
    mem.add(
        Action {
            command_digest: Some(command),
            input_root_digest: Some(root),
            ..Default::default()
        }
        .encode_to_vec(),
    )
}

/// Copies `data` to `out` and reports its record counts on standard error.
const COPY: &[&str] = &["dd", "if=data", "of=out", "status=noxfer"];

#[tokio::test]
async fn a_run_stores_a_pack_whose_first_record_is_the_manifest_naming_every_output() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, COPY, &["out"]);
    let dir = scratch("e2e");
    let cli = cli(&dir, digest);
    let sink = Fake::new(false);
    let manifest = execute(&cli, &mem, &sink).await.expect("task");
    let stored = sink.stored.lock().unwrap();
    assert_eq!(stored.len(), 1);
    let body = &stored[0];

    // The frontend's check: the manifest is the record at offset 0 and every digest it names
    // is a record of the pack.
    assert_eq!(read_manifest(body).expect("read"), manifest);
    assert_eq!(manifest.lease, "lease-1");
    assert_eq!(manifest.action, cli.action);
    assert_eq!(manifest.exit_code, 0);
    assert_eq!(manifest.output_files.len(), 1);
    assert_eq!(manifest.output_files[0].path, "out");
    assert_eq!(manifest.output_files[0].digest, sha256(b"hi\n"));
    assert!(!manifest.output_files[0].is_executable);
    // Stdout is empty: it is left out of the manifest and has no record. What dd reports on
    // stderr depends on its locale, so only that it is recorded is asserted; `read_manifest` above
    // found its record in the pack.
    assert_eq!(manifest.stdout, None);
    assert_ne!(manifest.stderr.map(|e| e.size_bytes), Some(0));
    assert!(manifest.stderr.is_some());
    // The manifest, stderr and the one output file: three records, none of them empty.
    let reader = SegmentReader::open(body).expect("segment");
    assert_eq!(reader.footer().entries().len(), 3);
    assert!(
        reader
            .footer()
            .entries()
            .iter()
            .all(|e| e.digest.size_bytes > 0)
    );
    let json_len = manifest.to_json().expect("json").len();
    let json = String::from_utf8_lossy(&body[..json_len]).into_owned();
    assert!(!json.contains("stdout"), "{json}");
    assert!(!json.contains("put_ms"), "{json}");
}

/// An empty output file is listed with the empty digest and the pack holds no record for it.
#[tokio::test]
async fn an_empty_output_file_is_listed_without_a_record() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, &["touch", "out"], &["out"]);
    let dir = scratch("empty-out");
    let sink = Fake::new(false);
    let manifest = execute(&cli(&dir, digest), &mem, &sink)
        .await
        .expect("task");
    assert_eq!(manifest.output_files[0].digest, EMPTY_DIGEST);
    assert_eq!((manifest.stdout, manifest.stderr), (None, None));
    let stored = sink.stored.lock().unwrap();
    assert_eq!(&read_manifest(&stored[0]).expect("read"), &manifest);
    let reader = SegmentReader::open(&stored[0]).expect("segment");
    assert_eq!(reader.footer().entries().len(), 1);
}

#[tokio::test]
async fn the_actions_exit_code_is_in_the_manifest_and_the_tool_still_succeeds() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, &["false"], &[]);
    let dir = scratch("exit3");
    let sink = Fake::new(false);
    let manifest = execute(&cli(&dir, digest), &mem, &sink)
        .await
        .expect("task");
    assert_eq!(manifest.exit_code, 1);
    assert!(manifest.output_files.is_empty());
    assert_eq!(sink.stored.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn output_directories_and_symlinks_reach_the_manifest() {
    let mut mem = Mem::default();
    let digest = action(
        &mut mem,
        &["cp", "-a", "src/d", "src/link", "."],
        &["d", "link"],
    );
    let dir = scratch("tree");
    let sink = Fake::new(false);
    let manifest = execute(&cli(&dir, digest), &mem, &sink)
        .await
        .expect("task");
    assert_eq!(manifest.output_directories.len(), 1);
    assert_eq!(manifest.output_directories[0].path, "d");
    assert_eq!(manifest.output_symlinks.len(), 1);
    assert_eq!(manifest.output_symlinks[0].target, "d/f");
    let stored = sink.stored.lock().unwrap();
    assert_eq!(&read_manifest(&stored[0]).expect("read"), &manifest);
}

#[tokio::test]
async fn an_action_that_cannot_be_read_fails_as_a_fetch_and_stores_nothing() {
    let mem = Mem::default();
    let dir = scratch("nofetch");
    let sink = Fake::new(false);
    let failure = execute(&cli(&dir, reapi_of(b"x")), &mem, &sink)
        .await
        .expect_err("fetch");
    assert!(matches!(failure, Failure::Fetch(_)), "{failure}");
    assert_eq!(failure.exit_code(), EXIT_FETCH);
    assert!(sink.stored.lock().unwrap().is_empty());
}

/// Catches an input blob that is gone, or is not the bytes its digest names, ending a task any way
/// but a fetch failure (exit 10) that stores nothing, and a bad `--fetch-concurrency` being taken.
#[tokio::test]
async fn an_input_blob_that_is_missing_or_corrupt_fails_as_a_fetch() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, COPY, &["out"]);
    let data = label(&reapi_of(b"hi\n"));
    for (name, replacement) in [("missing", None), ("corrupt", Some(b"HI\n".to_vec()))] {
        let mut broken = Mem(mem.0.clone());
        broken.0.remove(&data);
        if let Some(bytes) = replacement {
            broken.0.insert(data.clone(), bytes);
        }
        let sink = Fake::new(false);
        let dir = scratch(name);
        let mut flags = cli(&dir, digest.clone());
        flags.fetch_concurrency = 2;
        let failure = execute(&flags, &broken, &sink).await.expect_err(name);
        assert!(matches!(failure, Failure::Fetch(_)), "{name}: {failure}");
        assert_eq!(failure.exit_code(), EXIT_FETCH);
        assert!(sink.stored.lock().unwrap().is_empty());
    }
    let dir = scratch("default");
    assert_eq!(cli(&dir, digest).fetch_concurrency, 32);
}

#[test]
fn the_fetch_concurrency_flag_takes_one_to_256() {
    for (value, accepted) in [("0", false), ("1", true), ("256", true), ("257", false)] {
        let parsed = Cli::try_parse_from([
            "kbf-task",
            "--pack-url-file=u",
            "--grant-url=https://g",
            "--grant-token-file=t",
            "--lease=l",
            "--action=0000000000000000000000000000000000000000000000000000000000000000/0",
            "--fetch-concurrency",
            value,
        ]);
        assert_eq!(parsed.is_ok(), accepted, "{value}");
    }
}

#[tokio::test]
async fn an_action_without_a_command_or_an_input_root_fails_as_a_fetch() {
    for (command, root, what) in [(false, true, "command"), (true, false, "input root")] {
        let mut mem = Mem::default();
        let present = mem.add(Command::default().encode_to_vec());
        let digest = mem.add(
            Action {
                command_digest: command.then(|| present.clone()),
                input_root_digest: root.then(|| present.clone()),
                ..Default::default()
            }
            .encode_to_vec(),
        );
        let dir = scratch("missing-field");
        let failure = execute(&cli(&dir, digest), &mem, &Fake::new(false))
            .await
            .expect_err("fetch");
        assert!(failure.to_string().contains(what), "{failure}");
        assert_eq!(failure.exit_code(), EXIT_FETCH);
    }
}

#[tokio::test]
async fn a_command_with_no_program_fails_as_a_run_start() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, &[], &[]);
    let dir = scratch("noprog");
    let failure = execute(&cli(&dir, digest), &mem, &Fake::new(false))
        .await
        .expect_err("run");
    assert!(matches!(failure, Failure::Run(_)), "{failure}");
    assert_eq!(failure.exit_code(), EXIT_RUN);
}

/// The home and tmp directories are outside the input root, so files a Command writes there are
/// not in the manifest's outputs and not a record of the pack.
#[tokio::test]
async fn files_written_into_home_and_tmp_are_not_packed_or_listed() {
    let dir = scratch("private-unpacked");
    let in_home = dir.join("private/home/cc-cache");
    let in_tmp = dir.join("private/tmp/cc-scratch");
    let mut mem = Mem::default();
    let digest = action(
        &mut mem,
        &[
            "touch",
            in_home.to_str().unwrap(),
            in_tmp.to_str().unwrap(),
            "out",
        ],
        &["out"],
    );
    let sink = Fake::new(false);
    let manifest = execute(&cli(&dir, digest), &mem, &sink)
        .await
        .expect("task");
    assert!(in_home.exists());
    assert!(in_tmp.exists());
    assert_eq!(manifest.exit_code, 0);
    assert_eq!(manifest.output_files.len(), 1);
    assert_eq!(manifest.output_files[0].path, "out");
    assert!(manifest.output_directories.is_empty());
    assert!(manifest.output_symlinks.is_empty());
    let stored = sink.stored.lock().unwrap();
    let reader = SegmentReader::open(&stored[0]).expect("segment");
    // Only the manifest: `out` is empty, and the files in `private/` have no record.
    assert_eq!(reader.footer().entries().len(), 1);
    let json = manifest.to_json().expect("json");
    assert!(!String::from_utf8_lossy(&json).contains("private"));
}

/// Catches: a task whose home, tmp and cache directories cannot be made (a file where the
/// `private` directory belongs) starting the Command anyway, or failing in a class other than
/// the run start.
#[tokio::test]
async fn private_directories_that_cannot_be_made_fail_as_a_run_start() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, COPY, &["out"]);
    let dir = scratch("private-blocked");
    std::fs::write(dir.join("private"), b"in the way").expect("file");
    let sink = Fake::new(false);
    let failure = execute(&cli(&dir, digest), &mem, &sink)
        .await
        .expect_err("run");
    assert!(
        matches!(failure, Failure::Run(RunError::Io { .. })),
        "{failure}"
    );
    assert_eq!(failure.exit_code(), EXIT_RUN);
    assert!(!dir.join("in/out").exists());
    assert!(sink.stored.lock().unwrap().is_empty());
}

#[tokio::test]
async fn outputs_past_a_limit_fail_as_a_pack_and_store_nothing() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, COPY, &["out"]);
    let dir = scratch("limit");
    let mut flags = cli(&dir, digest);
    flags.limits.max_stdio_bytes = 1;
    let sink = Fake::new(false);
    let failure = execute(&flags, &mem, &sink).await.expect_err("limit");
    assert!(matches!(failure, Failure::Outputs(_)), "{failure}");
    assert_eq!(failure.exit_code(), EXIT_PACK);
    assert!(sink.stored.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_pack_the_store_refuses_fails_as_a_put() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, COPY, &["out"]);
    let dir = scratch("refused");
    let failure = execute(&cli(&dir, digest), &mem, &Fake::new(true))
        .await
        .expect_err("put");
    assert!(matches!(failure, Failure::Put(_)), "{failure}");
    assert_eq!(failure.exit_code(), EXIT_PUT);
}

#[test]
fn each_failure_class_has_its_own_exit_code() {
    let codes = [
        Failure::Setup(String::new()).exit_code(),
        Failure::Fetch(FetchError::Invalid(String::new())).exit_code(),
        Failure::Run(RunError::Invalid(String::new())).exit_code(),
        Failure::Result(String::new()).exit_code(),
        Failure::Read(std::io::Error::other("x")).exit_code(),
        Failure::Pack(PackError::from(ManifestError::Version(2))).exit_code(),
        Failure::Put(PutError::Exists { url: String::new() }).exit_code(),
    ];
    assert_eq!(codes, [14, 10, 11, 12, 12, 12, 13]);
}

/// A Command refused for want of a confine has an exit code of its own, so the farm can tell
/// "this system cannot confine" from a Command that could not be started.
#[test]
fn a_run_refused_for_want_of_a_confine_has_its_own_exit_code() {
    let failure = Failure::Run(RunError::NoConfine);
    assert_eq!(failure.exit_code(), EXIT_NO_CONFINE);
    assert_eq!(EXIT_NO_CONFINE, 15);
    assert_ne!(EXIT_NO_CONFINE, EXIT_RUN);
    assert!(
        failure
            .to_string()
            .contains("cannot deny the Command the network")
    );
}

#[test]
fn a_result_digest_that_is_absent_or_malformed_is_refused() {
    let good = reapi_of(b"x");
    assert_eq!(digest_of(Some(&good)).expect("good"), sha256(b"x"));
    assert!(matches!(digest_of(None), Err(Failure::Result(_))));
    let bad = Digest {
        hash: "xyz".to_owned(),
        size_bytes: 1,
    };
    assert!(matches!(digest_of(Some(&bad)), Err(Failure::Result(_))));
}

#[test]
fn the_actions_positive_timeout_replaces_the_default() {
    let default = Duration::from_secs(60);
    let with = |seconds, nanos| Action {
        timeout: Some(prost_types_duration(seconds, nanos)),
        ..Default::default()
    };
    assert_eq!(deadline(&Action::default(), default), default);
    assert_eq!(deadline(&with(5, 0), default), Duration::from_secs(5));
    assert_eq!(deadline(&with(0, 500), default), Duration::from_nanos(500));
    assert_eq!(deadline(&with(0, 0), default), default);
    assert_eq!(deadline(&with(-1, 0), default), default);
    // An Action cannot outlive the operator's limit.
    assert_eq!(deadline(&with(1_000_000_000, 0), default), default);
}

/// The Action's `timeout`, whatever type the generated code gives it.
fn prost_types_duration(seconds: i64, nanos: i32) -> prost_types::Duration {
    prost_types::Duration { seconds, nanos }
}

#[test]
fn the_flags_parse_with_their_defaults() {
    let digest = reapi_of(b"a");
    let flags = cli(Path::new("/w"), digest);
    assert_eq!(flags.deadline_ms, 3_600_000);
    assert!(flags.root_der.is_empty());
    let default = Cli::try_parse_from([
        "kbf-task",
        "--pack-url-file=u",
        "--grant-url=g",
        "--grant-token-file=t",
        "--lease=l",
        &format!("--action={}", sha256(b"a")),
    ])
    .expect("flags");
    assert_eq!(default.work_dir, PathBuf::from("/scratch/work"));
    assert!(!default.insecure_argv_secrets);
    // The action must be a digest, and each credential needs a source.
    for bad in [
        vec!["kbf-task", "--action=nothex/1"],
        vec![
            "kbf-task",
            "--grant-url=g",
            "--lease=l",
            "--grant-token-file=t",
        ],
    ] {
        assert!(Cli::try_parse_from(bad).is_err());
    }
}

const SECRET_URL: &str = "https://store.example/pack?X-Sig=very-secret";
const SECRET_TOKEN: &str = "very-secret-token";

/// Flags with the credentials as the given options, and `insecure` as asked.
fn with_secrets(args: &[&str]) -> Result<Cli, clap::Error> {
    let action = format!("--action={}", sha256(b"a"));
    let mut all = vec![
        "kbf-task",
        "--grant-url=https://grant.example",
        "--lease=l",
        action.as_str(),
    ];
    all.extend_from_slice(args);
    Cli::try_parse_from(all)
}

/// Both credentials as flag values are refused at startup without the opt-in, and the
/// refusal names the opt-in and neither value.
#[test]
fn credential_flags_need_the_insecure_opt_in() {
    for args in [
        vec![
            format!("--pack-url={SECRET_URL}"),
            "--grant-token-file=t".to_owned(),
        ],
        vec![
            "--pack-url-file=u".to_owned(),
            format!("--grant-token={SECRET_TOKEN}"),
        ],
        vec![
            format!("--pack-url={SECRET_URL}"),
            format!("--grant-token={SECRET_TOKEN}"),
        ],
    ] {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let flags = with_secrets(&refs).expect("clap accepts it; the startup check refuses");
        let Err(failure) = check_argv_secrets(&flags) else {
            panic!("accepted without --insecure-argv-secrets");
        };
        assert_eq!(failure.exit_code(), EXIT_SETUP);
        let text = failure.to_string();
        assert!(text.contains("--insecure-argv-secrets"), "{text}");
        assert!(!text.contains("very-secret"), "{text}");
        assert!(matches!(connect(&flags), Err(Failure::Setup(_))));
    }
}

#[tokio::test]
async fn main_with_refuses_a_credential_flag_without_the_opt_in() {
    let flags = with_secrets(&[&format!("--pack-url={SECRET_URL}"), "--grant-token-file=t"])
        .expect("flags");
    assert_eq!(main_with(&flags).await, ExitCode::from(EXIT_SETUP));
}

/// With the opt-in both flag values are used, and no file is needed or touched.
#[test]
fn credential_flags_work_with_the_insecure_opt_in() {
    let flags = with_secrets(&[
        "--insecure-argv-secrets",
        &format!("--pack-url={SECRET_URL}"),
        &format!("--grant-token={SECRET_TOKEN}"),
    ])
    .expect("flags");
    assert!(flags.insecure_argv_secrets);
    check_argv_secrets(&flags).expect("allowed");
    assert!(connect(&flags).is_ok());
    // The opt-in alone, with the file forms, changes nothing.
    let files = with_secrets(&[
        "--insecure-argv-secrets",
        "--pack-url-file=u",
        "--grant-token-file=t",
    ])
    .expect("flags");
    check_argv_secrets(&files).expect("nothing to refuse");
}

/// An empty or absent flag value is a setup failure that names the flag, not the value.
#[test]
fn empty_credential_flag_values_are_refused() {
    let mut flags = with_secrets(&[
        "--insecure-argv-secrets",
        &format!("--pack-url={SECRET_URL}"),
        "--grant-token=",
    ])
    .expect("flags");
    let Err(failure) = connect(&flags) else {
        panic!("an empty token was accepted");
    };
    assert!(
        failure.to_string().contains("grant token flag is empty"),
        "{failure}"
    );
    flags.grant_token = Some(SECRET_TOKEN.to_owned());
    flags.pack_url = Some("\n".to_owned());
    assert!(matches!(connect(&flags), Err(Failure::Setup(_))));
    flags.pack_url = None;
    let Err(failure) = connect(&flags) else {
        panic!("no URL was accepted");
    };
    assert!(failure.to_string().contains("no pack URL"), "{failure}");
}

/// Each credential has exactly one source: both or neither is a usage error, and the
/// error text does not echo a value.
#[test]
fn each_credential_has_exactly_one_source() {
    let both_url = [
        "--insecure-argv-secrets",
        "--pack-url-file=u",
        "--pack-url=https://store.example/pack?X-Sig=very-secret",
        "--grant-token-file=t",
    ];
    let both_token = [
        "--insecure-argv-secrets",
        "--pack-url-file=u",
        "--grant-token-file=t",
        "--grant-token=very-secret-token",
    ];
    let no_url = ["--grant-token-file=t"];
    let no_token = ["--pack-url-file=u"];
    for args in [&both_url[..], &both_token[..], &no_url[..], &no_token[..]] {
        let Err(err) = with_secrets(args) else {
            panic!("accepted {args:?}");
        };
        assert_eq!(err.exit_code(), 2);
        assert!(!err.to_string().contains("very-secret"), "{err}");
    }
    with_secrets(&["--pack-url-file=u", "--grant-token-file=t"]).expect("one each");
}

/// The values do not appear in a setup error raised after they were read.
#[test]
fn credential_values_are_not_in_a_setup_error() {
    let dir = scratch("secret-text");
    let flags = with_secrets(&[
        "--insecure-argv-secrets",
        &format!("--pack-url={SECRET_URL}"),
        &format!("--grant-token={SECRET_TOKEN}"),
        &format!("--root-der={}", dir.join("absent.der").display()),
    ])
    .expect("flags");
    let Err(failure) = connect(&flags) else {
        panic!("a missing root was accepted");
    };
    let text = failure.to_string();
    assert!(!text.contains("very-secret"), "{text}");
}

#[test]
fn connect_reads_the_token_file_and_the_extra_roots() {
    let dir = scratch("connect");
    let digest = reapi_of(b"a");
    let mut flags = cli(&dir, digest);
    // No token file.
    assert!(matches!(connect(&flags), Err(Failure::Setup(_))));
    // An empty one.
    flags.grant_token_file = Some(dir.join("token"));
    std::fs::write(dir.join("token"), "\n").expect("write");
    assert!(matches!(connect(&flags), Err(Failure::Setup(_))));
    // A token, and a root file that is missing.
    std::fs::write(dir.join("token"), "tok\n").expect("write");
    flags.root_der = vec![dir.join("absent.der")];
    assert!(matches!(connect(&flags), Err(Failure::Setup(_))));
    // Both usable, but the URL file is missing.
    std::fs::write(dir.join("root.der"), [1, 2, 3]).expect("write");
    flags.root_der = vec![dir.join("root.der")];
    let url_file = dir.join("url");
    flags.pack_url_file = Some(url_file.clone());
    assert!(matches!(connect(&flags), Err(Failure::Setup(_))));
    // An empty one.
    std::fs::write(&url_file, "\n").expect("write");
    assert!(matches!(connect(&flags), Err(Failure::Setup(_))));
    // A URL: it is read, and the file is left in place for the executor that delivered it.
    std::fs::write(&url_file, "https://store.example/pack?sig=s\n").expect("write");
    assert!(connect(&flags).is_ok());
    assert!(url_file.exists());
}

/// Catches the plain-HTTP opt-in being ignored: without `--allow-plain-http-put` the sink refuses
/// an `http` URL before connecting; with it, the sink reaches an `http` store.
#[test]
fn the_plain_http_opt_in_lets_the_sink_reach_an_http_store() {
    let dir = scratch("plain-http");
    let digest = reapi_of(b"a");
    let mut flags = cli(&dir, digest);
    std::fs::write(dir.join("token"), "tok\n").expect("write");
    flags.grant_token_file = Some(dir.join("token"));
    let url_file = dir.join("url");
    flags.pack_url_file = Some(url_file.clone());
    // Nothing listens at port 9: a refusal before connecting is a different error from a failed
    // connection, which is what the opt-in leads to.
    let http = "http://127.0.0.1:9/pack?sig=s\n";
    std::fs::write(&url_file, http).expect("write");
    let (_, sink) = connect(&flags).expect("connect");
    let refused = sink.store_pack(0, b"x").expect_err("https only");
    flags.allow_plain_http_put = true;
    std::fs::write(&url_file, http).expect("write");
    let (_, sink) = connect(&flags).expect("connect");
    let failed = sink.store_pack(0, b"x").expect_err("nothing listens");
    assert_ne!(refused.to_string(), failed.to_string());
    assert!(!failed.to_string().contains("sig=s"), "{failed}");
}

/// Catches `--fetch-batch` not reaching the source: it is off unless the flag is given.
#[test]
fn the_fetch_batch_flag_turns_batching_on_in_the_source() {
    let dir = scratch("batch-flag");
    let mut flags = cli(&dir, reapi_of(b"a"));
    std::fs::write(dir.join("token"), "tok\n").expect("write");
    flags.grant_token_file = Some(dir.join("token"));
    let url_file = dir.join("url");
    std::fs::write(&url_file, "https://store.example/pack?sig=s\n").expect("write");
    flags.pack_url_file = Some(url_file.clone());
    assert!(!flags.fetch_batch);
    let (source, _) = connect(&flags).expect("connect");
    assert!(!source.batching());
    flags.fetch_batch = true;
    std::fs::write(&url_file, "https://store.example/pack?sig=s\n").expect("write");
    let (source, _) = connect(&flags).expect("connect");
    assert!(source.batching());
}

/// Catches: the URL file being removed, or needing a writable directory: an executor delivers it
/// as a read-only run file, so the tool reads it and leaves it in place.
#[test]
fn connect_reads_a_url_file_in_a_read_only_directory() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("connect-read-only");
    let held = dir.join("held");
    std::fs::create_dir(&held).expect("dir");
    let mut flags = cli(&dir, reapi_of(b"a"));
    flags.grant_token_file = Some(dir.join("token"));
    std::fs::write(dir.join("token"), "tok\n").expect("write");
    flags.pack_url_file = Some(held.join("url"));
    std::fs::write(held.join("url"), "https://store.example/pack?sig=s").expect("write");
    std::fs::set_permissions(&held, std::fs::Permissions::from_mode(0o500)).expect("mode");
    let outcome = connect(&flags);
    let left = held.join("url").exists();
    std::fs::set_permissions(&held, std::fs::Permissions::from_mode(0o700)).expect("mode");
    assert!(outcome.is_ok());
    assert!(left);
}

#[tokio::test]
async fn main_with_reports_a_setup_failure_by_exit_code() {
    let dir = scratch("main-setup");
    let flags = cli(&dir, reapi_of(b"a"));
    assert_eq!(main_with(&flags).await, ExitCode::from(EXIT_SETUP));
}

/// The source and the sink as `finish` takes them.
fn boxed(mem: Mem, sink: Fake) -> Connected {
    (Box::new(mem), Box::new(sink))
}

/// Catches: a task that ran and stored its pack not exiting 0.
#[tokio::test]
async fn a_task_that_stores_its_pack_exits_zero() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, COPY, &["out"]);
    let dir = scratch("finish-ok");
    let flags = cli(&dir, digest);
    let connected = Ok::<_, Failure>(boxed(mem, Fake::new(false)));
    assert_eq!(finish(&flags, connected).await, ExitCode::SUCCESS);
}

/// Catches: a failed connect not ending as its own exit code, or a sink that refuses the pack
/// not ending as the put code.
#[tokio::test]
async fn finish_exits_with_the_code_of_the_failure() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, COPY, &["out"]);
    let dir = scratch("finish-fail");
    let flags = cli(&dir, digest);
    let refused = Ok::<_, Failure>(boxed(mem, Fake::new(true)));
    assert_eq!(finish(&flags, refused).await, ExitCode::from(EXIT_PUT));
    let not_connected = Err(Failure::Setup("no".to_owned()));
    assert_eq!(
        finish(&flags, not_connected).await,
        ExitCode::from(EXIT_SETUP)
    );
}

/// Catches: `run_cli` not building a runtime and running the task on it.
#[test]
fn run_cli_runs_the_task_and_returns_its_exit_code() {
    let dir = scratch("run-cli");
    let flags = cli(&dir, reapi_of(b"a"));
    assert_eq!(run_cli(&flags), ExitCode::from(EXIT_SETUP));
}

/// Catches: a runtime that cannot start not being a setup failure.
#[test]
fn a_runtime_that_cannot_start_is_a_setup_failure() {
    let dir = scratch("no-runtime");
    let flags = cli(&dir, reapi_of(b"a"));
    let failed = Err(std::io::Error::other("no threads"));
    assert_eq!(start(failed, &flags), ExitCode::from(EXIT_SETUP));
}

#[test]
fn the_http_sink_puts_the_pack_to_the_url() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/pack", listener.local_addr().expect("addr"));
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut got = Vec::new();
        let mut chunk = [0_u8; 512];
        while !got.ends_with(b"ok") {
            let n = stream.read(&mut chunk).expect("read");
            got.extend_from_slice(&chunk[..n]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .expect("reply");
        String::from_utf8_lossy(&got).into_owned()
    });
    let sink = HttpSink {
        agent: agent(None),
        urls: PutUrls {
            packs: vec![url],
            large: Vec::new(),
        },
        sleeper: Arc::new(ThreadSleeper),
        jitter: Arc::new(RandomJitter),
    };
    sink.store_pack(0, b"ok").expect("stored");
    let request = server.join().expect("server");
    assert!(request.starts_with("PUT /pack "), "{request}");
}

/// Catches a lease the frontend would refuse (empty, a `/`, a space, non-ASCII, over 128 bytes)
/// starting the Command: it is a setup failure (exit 14) that names `--lease`, raised before any
/// credential file is read or anything is fetched.
#[test]
fn a_lease_ingest_would_refuse_is_a_setup_failure() {
    let long = "a".repeat(129);
    for lease in ["", "a/b", "a b", "caf\u{e9}", long.as_str()] {
        let mut flags = cli(Path::new("/w"), reapi_of(b"a"));
        flags.lease = lease.to_owned();
        let Err(failure) = connect(&flags) else {
            panic!("accepted the lease {lease:?}");
        };
        assert_eq!(failure.exit_code(), EXIT_SETUP);
        assert!(failure.to_string().contains("--lease"), "{failure}");
    }
}

/// Catches a valid lease being refused: 1 byte and 128 bytes of `0-9A-Za-z_-` pass.
#[test]
fn a_build_id_is_a_lease() {
    let longest = format!("{}aZ", "aZ09_-".repeat(21));
    assert_eq!(longest.len(), 128);
    for lease in ["0", "Lease_1-b", longest.as_str()] {
        let mut flags = cli(Path::new("/w"), reapi_of(b"a"));
        flags.lease = lease.to_owned();
        check_lease(&flags).expect(lease);
    }
}

/// Catches a bad lease reaching the run: the process ends with the setup code and the Command
/// never starts.
#[tokio::test]
async fn main_with_refuses_a_bad_lease_before_fetching() {
    let dir = scratch("bad-lease");
    let mut flags = cli(&dir, reapi_of(b"a"));
    flags.lease = "a/b".to_owned();
    assert_eq!(main_with(&flags).await, ExitCode::from(EXIT_SETUP));
    assert!(!dir.join("in").exists());
}

/// Catches a manifest ingest would refuse being stored: two declared outputs with one path make a
/// manifest that names the path twice, so the task fails as a pack refusal (exit 12, detail
/// `invalid-manifest`) after the Command ran, and nothing is stored.
#[tokio::test]
async fn a_manifest_ingest_would_refuse_fails_as_a_pack_and_stores_nothing() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, COPY, &["out", "out"]);
    let dir = scratch("duplicate-path");
    let sink = Fake::new(false);
    let failure = execute(&cli(&dir, digest), &mem, &sink)
        .await
        .expect_err("duplicate path");
    assert!(matches!(failure, Failure::Pack(_)), "{failure}");
    assert_eq!(failure.exit_code(), EXIT_PACK);
    let text = failure.to_string();
    assert!(text.contains("invalid-manifest"), "{text}");
    assert!(!text.contains("output-too-large"), "{text}");
    assert!(sink.stored.lock().unwrap().is_empty());
}

/// Catches a size cap losing its detail on the way to stderr: the failure says `output-too-large`
/// and exits 12.
#[test]
fn a_size_cap_is_exit_12_with_output_too_large() {
    let failure = Failure::Pack(PackError::from(
        kbf_task_pack::WritePackError::RecordTooLarge { size: 1 },
    ));
    assert_eq!(failure.exit_code(), EXIT_PACK);
    assert!(
        failure.to_string().contains("output-too-large"),
        "{failure}"
    );
}

/// A store that answers `statuses` in turn, one connection each; it returns how many PUTs it saw.
fn serve_statuses(statuses: Vec<u16>) -> (String, std::thread::JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/pack", listener.local_addr().expect("addr"));
    let server = std::thread::spawn(move || {
        let mut seen = 0;
        for status in statuses {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut got = Vec::new();
            let mut chunk = [0_u8; 512];
            while !got.ends_with(b"ok") {
                let n = stream.read(&mut chunk).expect("read");
                got.extend_from_slice(&chunk[..n]);
            }
            seen += 1;
            let reply = format!(
                "HTTP/1.1 {status} X\r\nRetry-After: 4\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(reply.as_bytes()).expect("reply");
        }
        seen
    });
    (url, server)
}

/// A sleeper that records the waits it is asked for and does not wait.
#[derive(Default)]
struct Waits(Mutex<Vec<Duration>>);

impl Sleeper for Arc<Waits> {
    fn sleep(&self, wait: Duration) {
        self.0.lock().unwrap().push(wait);
    }
}

fn recording_sink(url: String, waits: &Arc<Waits>) -> HttpSink {
    HttpSink {
        agent: agent(None),
        urls: PutUrls {
            packs: vec![url],
            large: Vec::new(),
        },
        sleeper: Arc::new(Arc::clone(waits)),
        jitter: Arc::new(FixedJitter(0.5)),
    }
}

/// Catches a full hot tier failing the task after the build ran: the sink PUTs again after a 503,
/// waits what the store's `Retry-After` said, and stores on the 201.
#[test]
fn the_http_sink_retries_a_503_after_the_wait_the_store_asked_for() {
    let (url, server) = serve_statuses(vec![503, 201]);
    let waits = Arc::new(Waits::default());
    recording_sink(url, &waits)
        .store_pack(0, b"ok")
        .expect("stored");
    assert_eq!(server.join().expect("server"), 2);
    assert_eq!(*waits.0.lock().unwrap(), [Duration::from_secs(4)]);
}

/// Catches an unbounded retry against a real store: a store that keeps answering 503 sees exactly
/// one PUT plus the retries, and the failure is a put (exit 13).
#[test]
fn a_store_that_keeps_answering_503_fails_the_task_as_a_put_after_a_bounded_number_of_puts() {
    let attempts = crate::put::MAX_RETRIES as usize + 1;
    let (url, server) = serve_statuses(vec![503; attempts]);
    let waits = Arc::new(Waits::default());
    let err = recording_sink(url, &waits)
        .store_pack(0, b"ok")
        .expect_err("still full");
    assert_eq!(server.join().expect("server"), attempts);
    assert_eq!(waits.0.lock().unwrap().len(), attempts - 1);
    assert_eq!(Failure::from(err).exit_code(), EXIT_PUT);
}

/// Catches a 412 being retried: the key exists, so the sink PUTs once, waits for nothing and the
/// failure is a put (exit 13).
#[test]
fn the_http_sink_does_not_retry_a_412() {
    let (url, server) = serve_statuses(vec![412]);
    let waits = Arc::new(Waits::default());
    let err = recording_sink(url, &waits)
        .store_pack(0, b"ok")
        .expect_err("exists");
    assert_eq!(server.join().expect("server"), 1);
    assert!(waits.0.lock().unwrap().is_empty());
    assert!(matches!(err, PutError::Exists { .. }), "{err:?}");
}

/// Catches the two 412s sharing a line or an exit code: both are a put failure with exit 13, and
/// the line a 412 after an interrupted attempt writes names it (greppable), while a plain 412
/// writes the old line.
#[test]
fn a_412_after_an_interrupted_attempt_has_its_own_line_and_the_same_exit_code() {
    let plain = Failure::from(PutError::Exists { url: "u".into() });
    let after = Failure::from(PutError::ExistsAfterInterrupt { url: "u".into() });
    assert_eq!(plain.exit_code(), EXIT_PUT);
    assert_eq!(after.exit_code(), EXIT_PUT);
    assert_eq!(plain.to_string(), "put: the object already exists at u");
    assert!(
        after
            .to_string()
            .starts_with("put: 412 after an interrupted attempt of this put; "),
        "{after}"
    );
}

#[path = "multipack_tests.rs"]
mod multipack;

#[path = "http_sink_tests.rs"]
mod http_sink;

/// A source that says how it read.
struct Reporting {
    mem: Mem,
    reported: std::sync::atomic::AtomicBool,
}

impl BlobSource for Reporting {
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
        self.mem.get(digest)
    }

    fn report(&self) -> Option<String> {
        self.reported
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Some("inputs read: test".to_owned())
    }
}

/// Catches a source's report being dropped: once the inputs are fetched the task asks the source
/// how it read, and the run goes on to succeed.
#[tokio::test]
async fn a_sources_report_is_asked_for_once_the_inputs_are_fetched() {
    let mut mem = Mem::default();
    let digest = action(&mut mem, COPY, &["out"]);
    let source = Reporting {
        mem,
        reported: std::sync::atomic::AtomicBool::new(false),
    };
    let dir = scratch("report");
    let cli = cli(&dir, digest);
    execute(&cli, &source, &Fake::new(false))
        .await
        .expect("task");
    assert!(source.reported.load(std::sync::atomic::Ordering::SeqCst));
}
