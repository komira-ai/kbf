//! The `kbf-task` binary as the release checks it (`cargo xtask build-commit check`).

use std::process::Command;

/// Catches: `--version` not naming the commit the binary was built from, which is what the
/// release checks the binary, and the image carrying it, against: the line is `kbf-task
/// <package version>+<12 hex digits of the commit>` (or `unknown` for a source archive).
#[test]
fn the_version_names_the_commit_it_was_built_from() {
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-task"))
        .arg("--version")
        .output()
        .expect("kbf-task runs");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).expect("utf-8");
    let line = text.trim_end();
    let rest = line
        .strip_prefix("kbf-task ")
        .unwrap_or_else(|| panic!("not `kbf-task <version>`: {line:?}"));
    let (package, stamp) = rest
        .split_once('+')
        .unwrap_or_else(|| panic!("no `+<commit>` after the package version: {line:?}"));
    assert_eq!(package, env!("CARGO_PKG_VERSION"), "{line:?}");
    let hex = stamp.len() == 12
        && stamp
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    assert!(hex || stamp == "unknown", "the stamp is {stamp:?}");
}

/// Catches: the binary's `main` not handing the parsed flags to the task and returning the
/// task's exit code: a put directory that does not exist is a setup failure, exit code 14, before
/// anything is read or run.
#[test]
fn a_missing_put_directory_is_a_setup_failure_exit_code() {
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-task"))
        .args([
            "--tree=/nonexistent-root",
            "--stage-and-commit",
            "--out-dir=/nonexistent-out",
            "--lease",
            "lease-1",
            "--action",
            &format!("{}/0", "0".repeat(64)),
        ])
        .output()
        .expect("kbf-task runs");
    assert_eq!(out.status.code(), Some(14));
    let text = String::from_utf8(out.stderr).expect("utf-8");
    assert!(text.starts_with("kbf-task: "), "{text}");
}

/// Catches: the flags of the presigned mode of the releases before being taken for flags of this
/// one: a frontend that still sends them is refused by the flag parser (exit code 2), and the
/// Command never starts.
#[test]
fn the_flags_of_the_presigned_mode_are_refused_with_exit_code_2() {
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-task"))
        .args([
            "--pack-url-file",
            "/nonexistent-url",
            "--grant-url",
            "https://grant.example",
            "--grant-token-file",
            "/nonexistent-token",
            "--lease",
            "lease-1",
            "--action",
            &format!("{}/0", "0".repeat(64)),
        ])
        .output()
        .expect("kbf-task runs");
    assert_eq!(out.status.code(), Some(2));
}

/// Catches: `main` not handing its two hidden modes to the relay, or a mode reporting nothing:
/// `--sandbox-init` starts its Command and writes the start report on its standard input, the
/// relay with no PID namespace pending executes the Command itself, and a malformed mode exits 2.
#[cfg(target_os = "linux")]
#[test]
fn the_binary_runs_the_relay_and_the_first_process() {
    use std::io::Read as _;
    let run = |args: &[&str]| {
        let (read, write) = rustix::pipe::pipe().expect("pipe");
        let status = Command::new(env!("CARGO_BIN_EXE_kbf-task"))
            .args(args)
            .stdin(std::process::Stdio::from(write))
            .status()
            .expect("kbf-task runs");
        let mut said = Vec::new();
        std::fs::File::from(read)
            .read_to_end(&mut said)
            .expect("the report");
        (status.code(), said)
    };
    assert_eq!(
        run(&["--sandbox-init", "--", "/usr/bin/false"]),
        (Some(1), vec![0, 0])
    );
    assert_eq!(
        run(&["--sandbox-relay", "--group", "1", "--", "/usr/bin/true"]),
        (Some(0), Vec::new())
    );
    assert_eq!(run(&["--sandbox-init"]), (Some(2), Vec::new()));
}

/// Catches: the binary not running `--materialize-work` against the current directory with relative
/// paths: started with its working directory a scratch `exec/` whose `.kbf-tree` and `.kbf-out` are
/// symlinks to the tree and the put directory, it copies the tree's `work` into `exec/work`, runs the
/// Command there, exits 0 and commits `pack` into the put directory. Linux only, like the other
/// tests that run a Command through the binary.
#[cfg(target_os = "linux")]
#[test]
fn the_binary_materializes_the_tree_into_its_working_directory_and_commits_pack() {
    use kbf_proto::reapi::{Action, Command as Cmd};
    use prost::Message as _;
    use sha2::{Digest as _, Sha256};

    let base =
        kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-task-binary-materialize");
    let (tree, put, exec) = (base.join("tree"), base.join("put"), base.join("exec"));
    for dir in [&tree, &put, &exec] {
        std::fs::create_dir(dir).expect("dir");
    }
    std::fs::create_dir_all(tree.join(".kbf")).expect(".kbf");
    std::fs::create_dir(tree.join("work")).expect("work");
    std::fs::write(tree.join("work/data"), b"hi\n").expect("data");
    let digest = |bytes: &[u8]| kbf_proto::reapi::Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        size_bytes: i64::try_from(bytes.len()).expect("size"),
    };
    let command = Cmd {
        arguments: ["cp", "data", "out"].map(str::to_owned).to_vec(),
        output_paths: vec!["out".to_owned()],
        ..Default::default()
    }
    .encode_to_vec();
    let action = Action {
        command_digest: Some(digest(&command)),
        input_root_digest: Some(digest(b"input root")),
        ..Default::default()
    }
    .encode_to_vec();
    std::fs::write(tree.join(".kbf/command"), &command).expect("command");
    std::fs::write(tree.join(".kbf/action"), &action).expect("action");
    std::os::unix::fs::symlink(&tree, exec.join(".kbf-tree")).expect("tree mount");
    std::os::unix::fs::symlink(&put, exec.join(".kbf-out")).expect("put mount");
    let id = digest(&action);

    let out = Command::new(env!("CARGO_BIN_EXE_kbf-task"))
        .current_dir(&exec)
        .args([
            "--materialize-work=.kbf-tree",
            "--stage-and-commit",
            "--out-dir",
            ".kbf-out",
            "--action",
            &format!("{}/{}", id.hash, id.size_bytes),
            "--lease",
            "lease-1",
            "--deadline-ms",
            "60000",
            "--object-format",
            "one-pass",
        ])
        .output()
        .expect("kbf-task runs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert_eq!(std::fs::read(exec.join("work/out")).expect("out"), b"hi\n");
    assert!(put.join("pack").is_file());
    assert_eq!(
        std::fs::read(tree.join("work/data")).expect("data"),
        b"hi\n"
    );
    assert!(!tree.join("work/out").exists());
}
