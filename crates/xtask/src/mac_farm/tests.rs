//! Unit tests of `mac-farm`: its flags, the Command and Action it sends, the inputs it reads
//! from the checkout and the Cargo home, and the text of a farm's failure.

use std::collections::BTreeMap;

use kbf_testkit::dir::TestDir;
use prost::Message as _;

use super::*;

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("xtask-mac-farm-{name}"))
}

fn args() -> Args {
    Args {
        address: "grpc://farm.example:8980".to_owned(),
        instance: "main".to_owned(),
        tls: "true".to_owned(),
        properties: "os=macos,arch=arm64".to_owned(),
        scratch: PathBuf::from("scratch"),
        timeout_secs: 600,
        git: PathBuf::from("git"),
        cargo_program: PathBuf::from("cargo"),
        cargo: vec!["test".to_owned(), "-p".to_owned(), "kbf-update".to_owned()],
    }
}

/// Catches the scheme not following `--tls`, the properties left in the order given (REAPI
/// wants them sorted by name), and a flag value dropped on the way to the settings.
#[test]
fn the_flags_make_the_settings() {
    let s = Settings::of(&args()).expect("settings");
    assert_eq!(
        s,
        Settings {
            endpoint: "https://farm.example:8980".to_owned(),
            tls: true,
            instance: "main".to_owned(),
            properties: vec![
                ("arch".to_owned(), "arm64".to_owned()),
                ("os".to_owned(), "macos".to_owned()),
            ],
            timeout_secs: 600,
            cargo: vec!["test".to_owned(), "-p".to_owned(), "kbf-update".to_owned()],
        }
    );
    let plain = Args {
        tls: "false".to_owned(),
        ..args()
    };
    assert_eq!(
        Settings::of(&plain).expect("settings").endpoint,
        "http://farm.example:8980"
    );
}

/// Catches each flag that is wrong taken anyway: an address that is not `grpc://host:port` (no
/// port, or one that is not a number),
/// a TLS value that is neither word, no properties, a zero timeout, and a Cargo command that is
/// empty or starts with an option (which `cargo` would read as its own, not the subcommand's)
/// or a `+toolchain` (the Mac has no rustup to pick one).
#[test]
fn a_wrong_flag_is_named() {
    for address in [
        "farm:1",
        "grpc://",
        "grpc://farm:1/x",
        "grpc://fa rm:1",
        "grpc://farm",
        "grpc://farm:",
        "grpc://farm:80a",
    ] {
        let a = Args {
            address: address.to_owned(),
            ..args()
        };
        assert_eq!(Settings::of(&a), Err(Problem::Address(address.to_owned())));
    }
    let a = Args {
        tls: "yes".to_owned(),
        ..args()
    };
    assert_eq!(Settings::of(&a), Err(Problem::Tls("yes".to_owned())));
    let a = Args {
        properties: String::new(),
        ..args()
    };
    assert_eq!(
        Settings::of(&a),
        Err(Problem::Properties(
            crate::prove_farm::Problem::NoProperties
        ))
    );
    let a = Args {
        timeout_secs: 0,
        ..args()
    };
    assert_eq!(Settings::of(&a), Err(Problem::NoTimeout));
    let a = Args {
        cargo: vec!["--locked".to_owned(), "test".to_owned()],
        ..args()
    };
    assert_eq!(Settings::of(&a), Err(Problem::Cargo("--locked".to_owned())));
    let a = Args {
        cargo: vec!["+nightly".to_owned(), "test".to_owned()],
        ..args()
    };
    assert_eq!(Settings::of(&a), Err(Problem::Cargo("+nightly".to_owned())));
    let a = Args {
        cargo: Vec::new(),
        ..args()
    };
    assert_eq!(Settings::of(&a), Err(Problem::Cargo(String::new())));
}

/// Catches a Command that a Mac with no Rust and no network could not run: not the toolchain's
/// cargo, the arguments changed, the Cargo home or the compiler not the input root's, the
/// network not off, no path to the Mac's C compiler, or the environment out of name order.
#[test]
fn the_command_is_the_sent_cargo_offline_with_the_sent_compiler() {
    let s = Settings::of(&args()).expect("settings");
    let c = command(&s);
    assert_eq!(
        c.arguments,
        [".kbf-mac/toolchain/bin/cargo", "test", "-p", "kbf-update"]
    );
    let env: Vec<(&str, &str)> = c
        .environment_variables
        .iter()
        .map(|v| (v.name.as_str(), v.value.as_str()))
        .collect();
    assert_eq!(
        env,
        [
            ("CARGO_HOME", ".kbf-mac/cargo-home"),
            ("CARGO_INCREMENTAL", "0"),
            ("CARGO_NET_OFFLINE", "true"),
            ("CARGO_TERM_COLOR", "always"),
            ("PATH", "/usr/bin:/bin:/usr/sbin:/sbin"),
            ("RUSTC", ".kbf-mac/toolchain/bin/rustc"),
            ("RUSTDOC", ".kbf-mac/toolchain/bin/rustdoc"),
            ("RUSTFLAGS", "-D warnings"),
        ]
    );
    assert_eq!(c.working_directory, "");
    assert!(c.output_paths.is_empty());
}

/// Catches an Action that points at another Command or root, has no timeout or the wrong one,
/// or lacks the platform that sends it to a Mac; and its blobs not added for the upload.
#[test]
fn the_action_names_its_command_root_timeout_and_platform() {
    let s = Settings::of(&args()).expect("settings");
    let mut t = tree::build(Vec::new()).expect("tree");
    let root = t.root.clone();
    let digest = action(&s, &root, &mut t);
    let action = Action::decode(t.blobs[&digest.hash].as_slice()).expect("an Action");
    assert_eq!(action.input_root_digest, Some(root));
    let command_digest = action.command_digest.expect("command");
    let sent = Command::decode(t.blobs[&command_digest.hash].as_slice()).expect("a Command");
    assert_eq!(sent, command(&s));
    assert_eq!(action.timeout.map(|d| d.seconds), Some(600));
    let platform: Vec<(String, String)> = action
        .platform
        .expect("platform")
        .properties
        .into_iter()
        .map(|p| (p.name, p.value))
        .collect();
    assert_eq!(platform, s.properties);
    assert!(!action.do_not_cache);
}

/// Catches a tracked file read without its executable bit, a tracked symlink followed (its
/// target's bytes sent in its place), and a path read relative to the wrong directory.
#[test]
fn the_checkout_is_its_tracked_files_with_their_modes_and_symlinks() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("checkout");
    let root = dir.path();
    std::fs::create_dir_all(root.join("tools")).expect("mkdir");
    std::fs::write(root.join("README.md"), "hi").expect("write");
    std::fs::write(root.join("tools/run"), "#!").expect("write");
    std::fs::set_permissions(
        root.join("tools/run"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    std::os::unix::fs::symlink("../README.md", root.join("tools/readme")).expect("symlink");
    let got = checkout(
        root,
        &[
            "README.md".to_owned(),
            "tools/run".to_owned(),
            "tools/readme".to_owned(),
        ],
    )
    .expect("checkout");
    assert_eq!(
        got,
        vec![
            (
                "README.md".to_owned(),
                Entry::File {
                    bytes: b"hi".to_vec(),
                    executable: false
                }
            ),
            (
                "tools/run".to_owned(),
                Entry::File {
                    bytes: b"#!".to_vec(),
                    executable: true
                }
            ),
            (
                "tools/readme".to_owned(),
                Entry::Symlink("../README.md".to_owned())
            ),
        ]
    );
}

/// Catches a checkout that tracks the directory the run's own inputs go in (its files would
/// be replaced on the Mac), a tracked path that is gone, and a directory listed as a file.
#[test]
fn a_checkout_the_run_cannot_send_is_refused() {
    let dir = scratch("refused");
    let root = dir.path();
    std::fs::create_dir_all(root.join("sub")).expect("mkdir");
    for ours in [".kbf-mac", ".kbf-mac/toolchain/bin/cargo"] {
        let refused = checkout(root, &[ours.to_owned()]).expect_err("refused");
        assert_eq!(
            refused.to_string(),
            format!("the checkout tracks {ours}, where the run puts its toolchain and crates")
        );
    }
    let gone = checkout(root, &["gone".to_owned()]).expect_err("gone");
    assert!(gone.to_string().starts_with("read "), "{gone}");
    let not_file = checkout(root, &["sub".to_owned()]).expect_err("a directory");
    assert_eq!(
        not_file.to_string(),
        format!(
            "{} is neither a file nor a symlink",
            root.join("sub").display()
        )
    );
    assert!(checkout(root, &[".kbf-macintosh".to_owned()]).is_err());
}

/// Catches Cargo's locks, its cache tracker or installed programs sent (they change every run,
/// so no two runs would share an input root), a nested directory of the same name skipped,
/// and a file put anywhere but below the Cargo home of the input root.
#[test]
fn the_cargo_home_is_sent_without_its_locks_and_programs() {
    let dir = scratch("home");
    let home = dir.path();
    for path in [
        "bin/cargo-x",
        ".package-cache",
        ".package-cache-mutate",
        ".global-cache",
        "registry/cache/index/a-1.0.0.crate",
        "registry/index/index/.cache/a",
        "git/db/bin/HEAD",
    ] {
        let path = home.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, path.display().to_string()).expect("write");
    }
    let got: Vec<String> = cargo_home(home)
        .expect("home")
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert_eq!(
        got,
        [
            ".kbf-mac/cargo-home/git/db/bin/HEAD",
            ".kbf-mac/cargo-home/registry/cache/index/a-1.0.0.crate",
            ".kbf-mac/cargo-home/registry/index/index/.cache/a",
        ]
    );
    let missing = cargo_home(&home.join("gone")).expect_err("missing");
    assert!(missing.to_string().starts_with("list "), "{missing}");
}

/// Catches a farm failure printed without the job's output, an output of another reason
/// printed as the job's, and a detail that does not decode stopping the others.
#[test]
fn the_output_tail_of_a_failed_job_is_printed() {
    let info = |reason: &str, pairs: &[(&str, &str)]| prost_types::Any {
        type_url: "type.googleapis.com/google.rpc.ErrorInfo".to_owned(),
        value: rpc::ErrorInfo {
            reason: reason.to_owned(),
            metadata: pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..rpc::ErrorInfo::default()
        }
        .encode_to_vec(),
    };
    let status = rpc::Status {
        code: 14,
        message: "the job was lost".to_owned(),
        details: vec![
            prost_types::Any {
                type_url: "junk".to_owned(),
                value: vec![0xff],
            },
            info("OTHER", &[("stdout", "not this")]),
            info(
                "JOB_OUTPUT_TAIL",
                &[("stdout", "out end"), ("stderr", "err end")],
            ),
            info("JOB_OUTPUT_TAIL", &[("truncated", "true")]),
        ],
    };
    assert_eq!(
        tail(&status),
        "--- the job's stdout (its end) ---\nout end\n--- the job's stderr (its end) ---\nerr end\n"
    );
    assert_eq!(tail(&rpc::Status::default()), "");
}

/// Catches a program that failed read as an empty success, and its stderr not in the error.
#[test]
fn a_program_that_failed_is_named_with_its_stderr() {
    let ran = |ok: bool| Out {
        ok,
        stdout: "a\0b\0".to_owned(),
        stderr: "no such ref\n".to_owned(),
    };
    assert_eq!(succeeded("git ls-files", ran(true)).expect("ok"), "a\0b\0");
    assert_eq!(
        succeeded("git ls-files", ran(false))
            .expect_err("failed")
            .to_string(),
        "git ls-files failed: no such ref"
    );
    assert_eq!(split_nul("a\0b c\0\0"), ["a", "b c"]);
    assert!(split_nul("").is_empty());
}

/// Catches a stage printed as the other one: the log must say when the run waits for a Mac.
#[test]
fn each_stage_prints_its_own_line() {
    assert_eq!(
        progress_line(Progress::Queued),
        "mac-farm: queued: waiting for a free Mac"
    );
    assert_eq!(
        progress_line(Progress::Executing),
        "mac-farm: running on a farm Mac"
    );
}

/// Catches a run with no farm configured (a fork, a repository with no farm) that fails or
/// tries to connect instead of saying so and passing.
#[test]
fn no_address_runs_nothing_and_says_so() {
    let a = Args {
        address: "  ".to_owned(),
        ..args()
    };
    assert_eq!(
        run(&a).expect("a notice"),
        ["::notice::mac-farm runs nothing: the repository variable FARM_ADDRESS is not set"]
    );
}

/// Catches the blobs of the inputs and the action losing one another: every digest the action
/// names, through the root, is a blob of the run.
#[test]
fn the_inputs_hold_every_blob_the_action_names() {
    let dir = scratch("inputs");
    let root = dir.path().join("checkout");
    std::fs::create_dir_all(&root).expect("mkdir");
    std::fs::write(root.join("Cargo.toml"), "[workspace]").expect("write");
    let file = tar::EntryType::Regular;
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(file);
    header.set_mode(0o755);
    header.set_size(1);
    builder
        .append_data(&mut header, "c-1/c/bin/cargo", &b"c"[..])
        .expect("append");
    let mut xz = Vec::new();
    lzma_rs::xz_compress(&mut builder.into_inner().expect("tar").as_slice(), &mut xz).expect("xz");
    let pins = toolchain::Pins {
        rust: "1".to_owned(),
        component: vec![toolchain::Pin {
            name: "c".to_owned(),
            url: "https://example.invalid/c".to_owned(),
            sha256: crate::fetch::Sha256::of(&xz).to_string(),
            dir: "c".to_owned(),
        }],
    };
    let mut tracked = || Ok(vec!["Cargo.toml".to_owned()]);
    let mut fetch = |home: &Path| {
        std::fs::write(home.join("registry"), "r").map_err(Error::spawn("write".to_owned()))
    };
    let mut download = |_: &str| Ok(xz.clone());
    let mut host = Host {
        tracked: &mut tracked,
        fetch: &mut fetch,
        pins,
        download: &mut download,
    };
    let s = Settings::of(&args()).expect("settings");
    let (t, action) = inputs(&s, &root, &dir.path().join("scratch"), &mut host).expect("inputs");
    let mut names = BTreeMap::new();
    let mut todo = vec![("".to_owned(), t.root.clone())];
    while let Some((at, digest)) = todo.pop() {
        let d = kbf_proto::reapi::Directory::decode(t.blobs[&digest.hash].as_slice())
            .expect("a Directory");
        for f in d.files {
            let hash = f.digest.expect("digest").hash;
            assert!(t.blobs.contains_key(&hash), "{at}/{}", f.name);
            names.insert(format!("{at}/{}", f.name), hash);
        }
        for sub in d.directories {
            todo.push((format!("{at}/{}", sub.name), sub.digest.expect("digest")));
        }
    }
    let paths: Vec<&str> = names.keys().map(String::as_str).collect();
    assert_eq!(
        paths,
        [
            "/.kbf-mac/cargo-home/registry",
            "/.kbf-mac/toolchain/bin/cargo",
            "/Cargo.toml"
        ]
    );
    assert!(t.blobs.contains_key(&action.hash));
}
