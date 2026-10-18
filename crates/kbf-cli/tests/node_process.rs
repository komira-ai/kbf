//! `kbf enroll` and `kbf node` as processes, against the in-process frontend of
//! `kbf-enroll`'s tests: the words reach their entry points (not the mode parser), the
//! exit status is what a unit's `Restart=` can act on, and a refusal's reason is on
//! stderr.

#[path = "../../kbf-enroll/tests/support/mod.rs"]
mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_types::RefusalReason;
use support::fake;
use support::scratch;

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

/// A `kbf.toml` in a scratch root naming a frontend at `port`, and the path to it.
fn config(root: &Path, port: u16) -> PathBuf {
    let path = root.join("kbf.toml");
    let text = format!(
        "[frontend]\nurl = \"https://localhost:{port}\"\n[node]\nid = \"node-02\"\n[daemon]\n\
         [enroll]\ntoken_file = \"{}\"\n[dirs]\nstate = \"{}\"\n",
        root.join("enroll.token").display(),
        root.join("state").display(),
    );
    std::fs::write(&path, text).expect("kbf.toml");
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

/// Catches `kbf enroll` routed to the mode parser (which would say it is not a mode), an
/// enrollment that does not happen or is not reported, and a second run that is not a
/// no-op: exit 0 both times, the second saying the node was already enrolled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_enrolls_and_a_second_run_changes_nothing() {
    let run = fake::start();
    let root = scratch("process-enroll");
    let path = config(&root, run.enroll_addr.port());
    token(&root, &run.fake.mint(1, 1).to_line());

    let first = kbf(args(&["enroll"], &path)).await;
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    assert!(
        first.stdout.contains("enrolled as node-02 (daemon)"),
        "{}",
        first.stdout
    );
    assert!(root.join("state/pki/node.json").exists());

    let second = kbf(args(&["enroll"], &path)).await;
    assert_eq!(second.code, Some(0), "{}", second.stderr);
    assert!(
        second.stdout.contains("already enrolled as node-02"),
        "{}",
        second.stdout
    );
    assert_eq!(run.fake.enrolls().len(), 1);
}

/// Catches a refusal that exits 0, exits with the generic 1 (a unit would restart it
/// forever), or hides the reason: both words exit 2 with the frontend's reason on stderr,
/// and `kbf node` starts no mode (it returns instead of serving).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_token_exits_2_with_the_reason_on_stderr() {
    for word in ["enroll", "node"] {
        let run = fake::start();
        let root = scratch(&format!("process-refused-{word}"));
        let path = config(&root, run.enroll_addr.port());
        let minted = run.fake.mint(1, 1);
        token(&root, &minted.to_line());
        run.fake.expire(&minted);

        let out = kbf(args(&[word], &path)).await;

        assert_eq!(out.code, Some(2), "{word}: {}", out.stderr);
        assert!(
            out.stderr.contains(&format!(
                "kbf {word}: refused: {}",
                RefusalReason::TokenExpired.message()
            )),
            "{word}: {}",
            out.stderr
        );
        assert!(out.stdout.is_empty(), "{}", out.stdout);
        assert!(root.join("enroll.token").exists());
        assert!(!root.join("state/pki/node.json").exists());
    }
}

/// Catches a frontend that is down being a permanent failure: exit 1 (a unit's
/// `Restart=on-failure` tries again), not 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_that_is_down_exits_1() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    for word in ["enroll", "node"] {
        let root = scratch(&format!("process-down-{word}"));
        let path = config(&root, closed);
        token(&root, &fake::Fake::new().mint(1, 1).to_line());
        let out = kbf(args(&[word], &path)).await;
        assert_eq!(out.code, Some(1), "{word}: {}", out.stderr);
        assert!(
            out.stderr.contains("cannot reach the frontend"),
            "{}",
            out.stderr
        );
    }
}

/// Catches a missing file or an unknown flag reaching a start: exit 2 and a message that
/// names the file or the flag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_config_that_is_not_there_and_a_flag_that_is_not_known_exit_2() {
    let root = scratch("process-usage");
    for word in ["enroll", "node"] {
        let out = kbf(args(&[word], &root.join("none.toml"))).await;
        assert_eq!(out.code, Some(2), "{word}: {}", out.stderr);
        assert!(out.stderr.contains("none.toml"), "{}", out.stderr);

        let out = kbf(vec![word.to_owned(), "--bogus".to_owned()]).await;
        assert_eq!(out.code, Some(2), "{word}: {}", out.stderr);
        assert!(out.stderr.contains("--bogus"), "{}", out.stderr);

        let out = kbf(vec![word.to_owned(), "--help".to_owned()]).await;
        assert_eq!(out.code, Some(0), "{word}: {}", out.stderr);
        assert!(out.stdout.contains("--config"), "{}", out.stdout);
    }
}

/// Catches the mode parser's refusal of what `kbf node` built being printed as something
/// else, or exiting other than 2: an enrolled node given a label the daemon refuses prints
/// clap's message and exits 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flag_the_mode_refuses_exits_2_with_its_message() {
    let run = fake::start();
    let root = scratch("process-mode-refusal");
    let path = config(&root, run.enroll_addr.port());
    token(&root, &run.fake.mint(1, 1).to_line());
    let enrolled = kbf(args(&["enroll"], &path)).await;
    assert_eq!(enrolled.code, Some(0), "{}", enrolled.stderr);

    let mut words = args(&["node"], &path);
    words.push("--label=no-equals-sign".to_owned());
    let out = kbf(words).await;

    assert_eq!(out.code, Some(2), "{}", out.stderr);
    assert!(out.stderr.contains("--label"), "{}", out.stderr);
}
