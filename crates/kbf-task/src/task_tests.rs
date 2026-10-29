use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Mutex;

use kbf_proto::reapi::command::EnvironmentVariable;
use kbf_proto::reapi::{Directory, DirectoryNode, FileNode, SymlinkNode};
use kbf_segments::SegmentReader;
use kbf_testkit::dir::TestDir;
use prost::Message;

use super::*;
use crate::fetch::label;
use crate::pack::verify;

/// Blobs held in memory by digest.
#[derive(Default)]
struct Mem(HashMap<String, Vec<u8>>);

impl Mem {
    fn add(&mut self, bytes: Vec<u8>) -> Digest {
        let id = BlobId::of(&bytes);
        let digest = Digest::from(id);
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

/// A sink that keeps what it was given, or refuses.
struct Fake {
    stored: Mutex<Vec<Vec<u8>>>,
    refuse: bool,
}

impl Fake {
    fn new(refuse: bool) -> Self {
        Self {
            stored: Mutex::new(Vec::new()),
            refuse,
        }
    }
}

impl PackSink for Fake {
    fn store(&self, body: &[u8]) -> Result<(), PutError> {
        if self.refuse {
            return Err(PutError::Exists {
                url: "https://store.example/pack".to_owned(),
            });
        }
        self.stored.lock().unwrap().push(body.to_vec());
        Ok(())
    }
}

/// A fresh scratch directory for one test, beside the test binary.
fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-task-{name}"))
}

fn cli(work_dir: &Path, action: Digest) -> Cli {
    let id = BlobId::try_from(&action).expect("digest");
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
    assert_eq!(verify(body).expect("verify"), manifest);
    assert_eq!(manifest.lease, "lease-1");
    assert_eq!(manifest.action, cli.action);
    assert_eq!(manifest.exit_code, 0);
    assert_eq!(manifest.output_files.len(), 1);
    assert_eq!(manifest.output_files[0].path, "out");
    assert_eq!(manifest.output_files[0].digest, BlobId::of(b"hi\n"));
    assert!(!manifest.output_files[0].is_executable);
    // Stdout is empty: it is left out of the manifest and has no record. What dd reports on
    // stderr depends on its locale, so only that it is recorded is asserted; `verify` above
    // found its record in the pack.
    assert_eq!(manifest.stdout, None);
    assert!(
        manifest
            .stderr
            .as_ref()
            .is_some_and(|e| *e != BlobId::of(b""))
    );
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
    let json = String::from_utf8_lossy(&body[..manifest.encode().len()]).into_owned();
    assert!(
        !json.contains("stdout") && !json.contains("put_ms"),
        "{json}"
    );
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
    assert_eq!(manifest.output_files[0].digest, BlobId::of(b""));
    assert_eq!((manifest.stdout, manifest.stderr), (None, None));
    let stored = sink.stored.lock().unwrap();
    assert_eq!(&verify(&stored[0]).expect("verify"), &manifest);
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
    assert_eq!(&verify(&stored[0]).expect("verify"), &manifest);
}

#[tokio::test]
async fn an_action_that_cannot_be_read_fails_as_a_fetch_and_stores_nothing() {
    let mem = Mem::default();
    let dir = scratch("nofetch");
    let sink = Fake::new(false);
    let failure = execute(&cli(&dir, Digest::from(BlobId::of(b"x"))), &mem, &sink)
        .await
        .expect_err("fetch");
    assert!(matches!(failure, Failure::Fetch(_)), "{failure}");
    assert_eq!(failure.exit_code(), EXIT_FETCH);
    assert!(sink.stored.lock().unwrap().is_empty());
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
        Failure::Pack(PackError::TooLarge { limit: 1 }).exit_code(),
        Failure::Put(PutError::Exists { url: String::new() }).exit_code(),
    ];
    assert_eq!(codes, [14, 10, 11, 12, 12, 12, 13]);
}

/// Each cap is a pack failure (exit 12) whose message on stderr says `output-too-large`.
#[test]
fn every_size_cap_is_exit_12_with_output_too_large() {
    for err in [
        PackError::TooLarge { limit: 1 },
        PackError::TooManyEntries { limit: 1 },
        PackError::RecordTooLarge { limit: 1 },
        PackError::ManifestTooLarge { limit: 1 },
    ] {
        let failure = Failure::Pack(err);
        assert_eq!(failure.exit_code(), EXIT_PACK);
        assert!(
            failure.to_string().contains("output-too-large"),
            "{failure}"
        );
    }
    for err in [
        PackError::PathTooLong { len: 2, limit: 1 },
        PackError::DuplicatePath("p".to_owned()),
    ] {
        assert_eq!(Failure::Pack(err).exit_code(), EXIT_PACK);
    }
}

#[test]
fn a_result_digest_that_is_absent_or_malformed_is_refused() {
    let good = Digest::from(BlobId::of(b"x"));
    assert_eq!(blob_id(Some(&good)).expect("good"), BlobId::of(b"x"));
    assert!(matches!(blob_id(None), Err(Failure::Result(_))));
    let bad = Digest {
        hash: "xyz".to_owned(),
        size_bytes: 1,
    };
    assert!(matches!(blob_id(Some(&bad)), Err(Failure::Result(_))));
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
    let digest = Digest::from(BlobId::of(b"a"));
    let flags = cli(Path::new("/w"), digest);
    assert_eq!(flags.deadline_ms, 3_600_000);
    assert!(flags.root_der.is_empty());
    let default = Cli::try_parse_from([
        "kbf-task",
        "--pack-url-file=u",
        "--grant-url=g",
        "--grant-token-file=t",
        "--lease=l",
        &format!("--action={}", BlobId::of(b"a")),
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
    let action = format!("--action={}", BlobId::of(b"a"));
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
    let digest = Digest::from(BlobId::of(b"a"));
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
    // A URL: it is read, and the file is gone before the Command could read it.
    std::fs::write(&url_file, "https://store.example/pack?sig=s\n").expect("write");
    assert!(connect(&flags).is_ok());
    assert!(!url_file.exists());
}

/// Catches the plain-HTTP opt-in being ignored: without `--allow-plain-http-put` the sink refuses
/// an `http` URL before connecting; with it, the sink reaches an `http` store.
#[test]
fn the_plain_http_opt_in_lets_the_sink_reach_an_http_store() {
    let dir = scratch("plain-http");
    let digest = Digest::from(BlobId::of(b"a"));
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
    let refused = sink.store(b"x").expect_err("https only");
    flags.allow_plain_http_put = true;
    std::fs::write(&url_file, http).expect("write");
    let (_, sink) = connect(&flags).expect("connect");
    let failed = sink.store(b"x").expect_err("nothing listens");
    assert_ne!(refused.to_string(), failed.to_string());
    assert!(!failed.to_string().contains("sig=s"), "{failed}");
}

#[test]
fn connect_refuses_a_url_file_it_cannot_remove() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("connect-unremovable");
    let held = dir.join("held");
    std::fs::create_dir(&held).expect("dir");
    let mut flags = cli(&dir, Digest::from(BlobId::of(b"a")));
    flags.grant_token_file = Some(dir.join("token"));
    std::fs::write(dir.join("token"), "tok\n").expect("write");
    flags.pack_url_file = Some(held.join("url"));
    std::fs::write(held.join("url"), "https://store.example/pack?sig=s").expect("write");
    std::fs::set_permissions(&held, std::fs::Permissions::from_mode(0o500)).expect("mode");
    // A superuser removes it anyway; the refusal is checked where the mode is enforced.
    let enforced = std::fs::write(held.join("probe"), b"").is_err();
    let outcome = connect(&flags);
    std::fs::set_permissions(&held, std::fs::Permissions::from_mode(0o700)).expect("mode");
    if enforced {
        assert!(matches!(outcome, Err(Failure::Setup(_))));
    }
}

#[tokio::test]
async fn main_with_reports_a_setup_failure_by_exit_code() {
    let dir = scratch("main-setup");
    let flags = cli(&dir, Digest::from(BlobId::of(b"a")));
    assert_eq!(main_with(&flags).await, ExitCode::from(EXIT_SETUP));
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
        url,
    };
    sink.store(b"ok").expect("stored");
    let request = server.join().expect("server");
    assert!(request.starts_with("PUT /pack "), "{request}");
}
