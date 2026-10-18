//! The start that reads `kbf.toml` as a process, against the in-process frontend of
//! `kbf-enroll`'s tests: the flags reach their entry point (not a mode's parser), the exit
//! status is what a supervisor can act on (2 when a restart cannot fix it), a refusal's
//! reason is on stderr, and `--check` answers for a host that is not enrolled yet. The
//! words `kbf node` and `kbf enroll` are gone.

#[path = "../../kbf-enroll/tests/support/mod.rs"]
mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_types::RefusalReason;
use support::fake;
use support::scratch;

/// Writes a `kbf.toml` that a start accepts whatever the umask of the test run is: the
/// file may not be writable by group or other.
fn write_config(path: &Path, text: &str) {
    std::fs::write(path, text).expect("kbf.toml");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).expect("mode");
}

const BIN: &str = env!("CARGO_BIN_EXE_kbf");

struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn kbf(args: Vec<String>) -> Out {
    tokio::task::spawn_blocking(move || {
        let out = Command::new(BIN).args(&args).output().expect("run kbf");
        Out {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    })
    .await
    .expect("no panic")
}

/// A `kbf.toml` in a scratch root naming a frontend at `port`, with `top` lines before the
/// first table (the mode), and the path to it.
fn config(root: &Path, port: u16, top: &str) -> PathBuf {
    let path = root.join("kbf.toml");
    let text = format!(
        "{top}[frontend]\nurl = \"https://localhost:{port}\"\n[node]\nid = \"node-02\"\n\
         [enroll]\ntoken_file = \"{}\"\n[dirs]\nstate = \"{}\"\n",
        root.join("enroll.token").display(),
        root.join("state").display(),
    );
    write_config(&path, &text);
    path
}

fn token(root: &Path, line: &str) {
    let path = root.join("enroll.token");
    std::fs::write(&path, line).expect("token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
}

fn args(words: &[&str], config: &Path) -> Vec<String> {
    words
        .iter()
        .map(|w| (*w).to_owned())
        .chain(["--config".to_owned(), config.display().to_string()])
        .collect()
}

/// Catches `--check` that fails a host that is configured and not enrolled yet, that
/// enrolls, or that does not take the mode from the file: for the flag, the file, and
/// the old alias word with `--check`, exit 0 with the answer on stdout, the token file
/// where it was, no `node.json`, and the frontend not called.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_passes_for_a_host_that_is_not_enrolled_yet() {
    let run = fake::start();
    let root = scratch("process-check");
    let path = config(&root, run.enroll_addr.port(), "mode = \"storage\"\n");
    token(&root, &run.fake.mint(1, 1).to_line());

    for words in [
        &["--mode=daemon", "--check"][..],
        &["--check"],
        &["storage-and-daemon", "--check"],
    ] {
        let out = kbf(args(words, &path)).await;
        assert_eq!(out.code, Some(0), "{words:?}: {}", out.stderr);
        assert!(
            out.stdout.contains("not enrolled yet") && out.stdout.contains("enroll for"),
            "{words:?}: {}",
            out.stdout
        );
        assert!(root.join("enroll.token").exists());
        assert!(!root.join("state/pki/node.json").exists());
    }
    assert!(run.fake.enrolls().is_empty(), "--check calls no frontend");

    // The flag beats the file and the start says so on stderr.
    let out = kbf(args(&["--mode=daemon", "--check"], &path)).await;
    assert!(
        out.stderr
            .contains("--mode=daemon overrides mode = \"storage\""),
        "{}",
        out.stderr
    );
    let out = kbf(args(&["--check"], &path)).await;
    assert!(out.stdout.contains("mode storage"), "{}", out.stdout);
    assert!(!out.stderr.contains("overrides"), "{}", out.stderr);
}

/// Catches `--check` passing a host a start would refuse, with a code other than 2 or a
/// message that does not name the cause: no token file, a config that is not there, and
/// no mode from either source (the message names the flag and the file).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_exits_2_for_a_missing_token_a_missing_file_and_no_mode() {
    let run = fake::start();
    let root = scratch("process-check-bad");
    let path = config(&root, run.enroll_addr.port(), "");

    let out = kbf(args(&["--mode=daemon", "--check"], &path)).await;
    assert_eq!(out.code, Some(2), "{}", out.stderr);
    assert!(out.stderr.contains("enroll.token"), "{}", out.stderr);
    assert!(out.stdout.is_empty(), "{}", out.stdout);

    token(&root, &run.fake.mint(1, 1).to_line());
    let out = kbf(args(&["--check"], &path)).await;
    assert_eq!(out.code, Some(2), "{}", out.stderr);
    assert!(
        out.stderr.contains("no mode:")
            && out.stderr.contains("--mode=<")
            && out.stderr.contains("mode = ")
            && out.stderr.contains("kbf.toml"),
        "{}",
        out.stderr
    );

    let out = kbf(args(&["--mode=daemon", "--check"], &root.join("none.toml"))).await;
    assert_eq!(out.code, Some(2), "{}", out.stderr);
    assert!(out.stderr.contains("none.toml"), "{}", out.stderr);
}

/// Catches a refusal that exits 0, exits with the generic 1 (a supervisor would restart it
/// forever), or hides the reason: an expired token and a token that lacks the role the
/// mode needs exit 2 with the frontend's reason on stderr, nothing on stdout, the token
/// file where it was and no `node.json`; the start runs no mode (it returns instead of
/// serving).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_token_exits_2_with_the_reason_on_stderr() {
    let run = fake::start();
    let root = scratch("process-expired");
    let path = config(&root, run.enroll_addr.port(), "");
    let minted = run.fake.mint(1, 1);
    token(&root, &minted.to_line());
    run.fake.expire(&minted);
    let out = kbf(args(&["--mode=daemon"], &path)).await;
    assert_eq!(out.code, Some(2), "{}", out.stderr);
    assert!(
        out.stderr.contains(&format!(
            "kbf: refused: {}",
            RefusalReason::TokenExpired.message()
        )),
        "{}",
        out.stderr
    );
    assert!(out.stdout.is_empty(), "{}", out.stdout);
    assert!(root.join("enroll.token").exists());
    assert!(!root.join("state/pki/node.json").exists());

    let run = fake::start();
    let root = scratch("process-lacking");
    let path = config(&root, run.enroll_addr.port(), "mode = \"daemon\"\n");
    token(&root, &run.fake.mint_roles(1, 1, &["storage"]).to_line());
    let out = kbf(args(&[], &path)).await;
    assert_eq!(out.code, Some(2), "{}", out.stderr);
    assert!(
        out.stderr.contains(RefusalReason::RoleNotGranted.message()),
        "{}",
        out.stderr
    );
    assert!(root.join("enroll.token").exists());
    assert_eq!(run.fake.enrolls().len(), 1);
    assert_eq!(
        run.fake.enrolls()[0]
            .csrs
            .iter()
            .map(|c| c.role.as_str())
            .collect::<Vec<_>>(),
        ["daemon"],
        "the request asked for the mode's role only"
    );
}

/// Catches an unknown flag, a flag of another mode, or a word reaching a start that reads
/// the file: exit 2 and a message that names it; `--help` is the top level's, exit 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flag_that_is_not_known_exits_2() {
    let root = scratch("process-usage");
    // A file that exists and is the mode's: the flags are parsed with it.
    let none = config(&root, 1, "");
    let out = kbf(args(&["--mode=daemon", "--bogus"], &none)).await;
    assert_eq!(out.code, Some(2), "{}", out.stderr);
    assert!(out.stderr.contains("--bogus"), "{}", out.stderr);

    let out = kbf(args(&["--mode=daemon", "--cold-store=s3://b"], &none)).await;
    assert_eq!(out.code, Some(2), "{}", out.stderr);
    assert!(
        out.stderr.contains("kbf daemon refuses --cold-store"),
        "{}",
        out.stderr
    );

    for word in ["node", "enroll"] {
        let out = kbf(vec![word.to_owned(), "--config".to_owned(), "x".to_owned()]).await;
        assert_eq!(out.code, Some(2), "{word}: {}", out.stderr);
        assert!(
            out.stderr
                .contains(&format!("unrecognized subcommand '{word}'")),
            "{word}: {}",
            out.stderr
        );
    }

    let out = kbf(vec!["--help".to_owned()]).await;
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    for flag in ["--mode", "--config", "--check"] {
        assert!(out.stdout.contains(flag), "{flag}: {}", out.stdout);
    }
    assert!(!out.stdout.contains("kbf node") && !out.stdout.contains("kbf enroll"));
}

/// Catches the mode parser's refusal of what the start built being printed as something
/// else, or exiting other than 2: an enrolled node given a label the daemon refuses prints
/// clap's message and exits 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flag_the_mode_refuses_exits_2_with_its_message() {
    let run = fake::start();
    let root = scratch("process-mode-refusal");
    let path = config(&root, run.enroll_addr.port(), "");
    token(&root, &run.fake.mint(1, 1).to_line());
    // Enrolled by a start that stops at its first refusal: the label has no `=`.
    let mut words = args(&["--mode=daemon"], &path);
    words.push("--label=no-equals-sign".to_owned());
    let out = kbf(words).await;

    assert_eq!(out.code, Some(2), "{}", out.stderr);
    assert!(out.stderr.contains("--label"), "{}", out.stderr);
    assert!(
        root.join("state/pki/node.json").exists(),
        "enrolled all the same"
    );
}
