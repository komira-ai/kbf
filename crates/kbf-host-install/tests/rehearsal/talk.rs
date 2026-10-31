//! Speaking to the helper's local socket as the updater's account, which the helper's socket
//! (mode 0600, owned by that account) requires: the rehearsal runs a copy of its own test
//! binary as that account, and the copy sends one request and prints the one line the helper
//! answers.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use super::{SOCKET, UPDATER, ids_of, run};

/// The environment variable that carries a request to [`speak_to_the_helper`].
const REQUEST_ENV: &str = "KBF_REHEARSAL_REQUEST";
/// How long one request may take: an install downloads and verifies a release, a switch of a
/// frontend container drains it and waits for the new one.
const REQUEST_LIMIT: Duration = Duration::from_secs(900);
/// The name of [`speak_to_the_helper`] among the tests of a rehearsal's binary.
const SPEAKER: &str = "rehearsal::talk::speak_to_the_helper";

/// The helper's answer line, which [`speak_to_the_helper`] prints, parsed.
fn answer(text: &str) -> Value {
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("answer: "))
        .unwrap_or_else(|| panic!("no answer line: {text}"));
    serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
}

/// Not a test of its own: the process that speaks to the helper as the updater's account. It
/// sends the request in [`REQUEST_ENV`] and prints the one line the helper answers.
#[test]
#[ignore = "run by an update rehearsal, as the updater's account"]
fn speak_to_the_helper() {
    let request = std::env::var(REQUEST_ENV).expect("a request to send");
    let mut stream = UnixStream::connect(SOCKET).expect("the helper's socket");
    stream.set_read_timeout(Some(REQUEST_LIMIT)).unwrap();
    stream.write_all(format!("{request}\n").as_bytes()).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    println!("answer: {}", line.trim_end());
}

/// Sends `request` to the helper as the updater's account (through `probe`, a copy of the
/// rehearsal's binary) and returns the answer, kept as `name`.
pub fn ask(probe: &Path, name: &str, request: &str) -> Value {
    let (uid, gid) = ids_of(UPDATER);
    let text = run(
        name,
        Command::new(probe)
            .args(["--ignored", "--exact", SPEAKER, "--nocapture"])
            .env_clear()
            .env(REQUEST_ENV, request)
            .current_dir("/")
            .uid(uid)
            .gid(gid),
    );
    let value = answer(&text);
    println!("{request} -> {value}");
    value
}
