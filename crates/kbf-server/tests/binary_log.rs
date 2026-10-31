//! The `kbf-server` binary's durable log: where `--log-dir` and `--hot-store` put it,
//! that it gets what stderr gets, that a restart appends, and what the server says when
//! it has no file.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-server");
const ANY_PORT: [&str; 4] = ["--listen", "127.0.0.1:0", "--worker-listen", "127.0.0.1:0"];
/// What the server logs when the file is on, and when it is not.
const ON: &str = "durable log file on, besides stderr";
const OFF: &str = "no durable log file, stderr only";

/// A path in a directory of its own, removed with it when this drops.
#[derive(Debug)]
struct Fresh {
    path: PathBuf,
    _dir: kbf_testkit::dir::TestDir,
}

impl std::ops::Deref for Fresh {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for Fresh {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

/// A fresh, absent directory for `name`, under the target directory, removed (with what
/// the test made in it) when it drops.
fn fresh_dir(name: &str) -> Fresh {
    let dir = kbf_testkit::dir::TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("kbf-server-binary-log-{name}"),
    );
    Fresh {
        path: dir.join("d"),
        _dir: dir,
    }
}

/// Runs the server over a cold store in `base/cold` and a metadata log in `base/meta`
/// (unless `extra` puts the log in a hot store) with `extra` flags until its start line,
/// stops it with SIGINT and returns its stderr.
fn run(base: &Path, extra: &[&str]) -> String {
    let cold = format!("file://{}", base.join("cold").display());
    let mut c = Command::new(BIN);
    c.args(["--cold-store", &cold, "--cold-store-floor-bytes=1"])
        .args(ANY_PORT)
        .args(extra);
    if !extra.contains(&"--hot-store") {
        c.arg("--meta-dir").arg(base.join("meta"));
    }
    let mut child = c
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kbf-server");
    let mut start = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut start)
        .expect("a start line");
    assert!(start.starts_with("kbf-server "), "{start:?}");
    let sent = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("run kill");
    assert!(sent.success());
    let status = child.wait().expect("exit");
    assert!(status.success(), "{status}");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("UTF-8 stderr");
    stderr
}

/// Catches: `--log-dir` parsed and ignored (no file), the file missing the events
/// stderr gets (the line saying the file is on is in both), a start that truncates the
/// file (the second run's entries replace the first's), and the capture flags not
/// reaching the file (`--log-files` and `--log-max-bytes` are in the line).
#[test]
fn log_dir_gets_the_events_and_a_restart_appends() {
    let base = fresh_dir("log-dir");
    let log = base.join("logs");
    let dir = log.to_str().expect("UTF-8 path");
    let flags = [
        "--log-dir",
        dir,
        "--log-max-bytes",
        "1048576",
        "--log-files",
        "3",
    ];
    let stderr = run(&base, &flags);
    assert!(stderr.contains(ON), "{stderr}");
    let file = std::fs::read_to_string(log.join("kbf-server.log")).expect("the log file");
    assert!(file.contains(ON), "{file}");
    assert!(file.contains("max_bytes=1048576"), "{file}");
    assert!(file.contains("files=3"), "{file}");
    assert!(!file.contains('\u{1b}'), "colour codes in the file: {file}");

    run(&base, &flags);
    let again = std::fs::read_to_string(log.join("kbf-server.log")).expect("the log file");
    assert_eq!(again.matches(ON).count(), 2, "{again}");
    assert!(
        again.starts_with(&file),
        "the first run's entries were not kept"
    );
}

/// Catches: no default log under the hot store's directory (the dev-node case: the
/// operator gave a hot store and expected a log), the log put beside `meta` somewhere
/// other than `<hot>/log`, and a server with neither flag that does not say it has no
/// file.
#[test]
fn the_default_is_log_in_the_hot_store_and_no_file_without_either() {
    let base = fresh_dir("hot");
    let hot = base.join("hot");
    let uri = format!("file://{}", hot.display());
    let stderr = run(&base, &["--hot-store", &uri]);
    assert!(stderr.contains(ON), "{stderr}");
    let file = std::fs::read_to_string(hot.join("log").join("kbf-server.log")).expect("<hot>/log");
    assert!(file.contains(ON), "{file}");
    assert!(file.contains("write tier attached"), "{file}");

    let bare = fresh_dir("bare");
    let stderr = run(&bare, &[]);
    assert!(stderr.contains(OFF), "{stderr}");
    assert!(!bare.join("log").exists());
}
