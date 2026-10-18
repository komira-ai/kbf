//! A stand-in for Buck2, for the tests of scenario B0 that have no Buck2 (the beta workflow
//! runs the real one): it answers `--version`, `kill`, and `build`, which prints the console
//! summary a Buck2 build ends with and writes an event log. What a build does is the word in
//! `.fake-buck2` in the directory it runs in (`ok` when there is none).
//!
//! - `ok`: three actions run on the farm, none fails; exit 0.
//! - `failed-action`: as `ok`, but one action in the log failed; exit 0.
//! - `exit-1`: the summary and a clean log, exit 1.
//! - `local`: one of the actions ran locally; exit 0.
//! - `no-summary`: exit 0 and no summary line.
//! - `bad-log`: the event log is not an event log; exit 0.
//! - `hang`: never ends.
//!
//! A second word, `kill-hangs`, makes `kill` slow (as a wedged daemon makes the real one):
//! it sleeps for twenty seconds and then leaves `.fake-buck2-kill-survived` in the directory
//! it runs in, which a `kill` that is given up on never does. Every `kill` adds a line to
//! `.fake-buck2-killed-<isolation dir>` there first, so a test can tell which daemons were
//! killed, and how many times. The word `version-hangs` makes `--version` sleep for thirty seconds.

use std::io::Write as _;
use std::time::Duration;

/// One action end of an event log, with the fields of Buck2's own that the harness reads:
/// `failed`, `error`, and the `status` of its command.
fn action_line(n: usize, failed: bool) -> String {
    let (error, status) = if failed {
        (
            "{\"CommandExecutionError\":{}}",
            "{\"Error\":{\"stage\":\"stand-in\",\"error\":\"failed\"}}",
        )
    } else {
        ("null", "{\"Success\":{}}")
    };
    format!(
        "{{\"Event\":{{\"data\":{{\"SpanEnd\":{{\"data\":{{\"ActionExecution\":{{\"name\":{{\"category\":\"fill\",\"identifier\":\"a{n}\"}},\"failed\":{failed},\"error\":{error},\"commands\":[{{\"status\":{status}}}]}}}}}}}}}}}}"
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let words = std::fs::read_to_string(".fake-buck2").unwrap_or_default();
    let words: Vec<&str> = words.split_whitespace().collect();
    if args.first().map(String::as_str) == Some("--version") {
        if words.contains(&"version-hangs") {
            std::thread::sleep(Duration::from_secs(30));
        }
        println!("buck2 fake");
        return;
    }
    // `--isolation-dir <dir>` precedes the subcommand.
    let rest: Vec<&String> = args.iter().skip(2).collect();
    if rest.first().map(|s| s.as_str()) == Some("kill") {
        let marker = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!(".fake-buck2-killed-{}", args[1]));
        let _ = marker.and_then(|mut file| writeln!(file, "kill"));
        if words.contains(&"kill-hangs") {
            std::thread::sleep(Duration::from_secs(20));
            let _ = std::fs::write(".fake-buck2-kill-survived", "");
        }
        return;
    }
    if rest.first().map(|s| s.as_str()) != Some("build") {
        return;
    }
    let mode = words.first().copied().unwrap_or_default();
    if mode == "hang" {
        // Not for ever: a test that gives up on this build leaves the process behind.
        std::thread::sleep(Duration::from_secs(30));
    }
    // A build takes a moment, so the loop does not spin.
    std::thread::sleep(Duration::from_millis(300));
    let at = args
        .iter()
        .position(|a| a == "--event-log")
        .expect("--event-log");
    let mut log = std::fs::File::create(&args[at + 1]).expect("the event log");
    if mode == "bad-log" {
        let _ = log.write_all(b"this is not an event log\n");
    } else {
        for n in 0..3 {
            let failed = mode == "failed-action" && n == 0;
            let _ = writeln!(log, "{}", action_line(n, failed));
        }
    }
    match mode {
        "no-summary" => {}
        "local" => println!("Commands: 3 (cached: 0, remote: 2, local: 1)"),
        _ => println!("Commands: 3 (cached: 0, remote: 3, local: 0)"),
    }
    if mode == "exit-1" {
        std::process::exit(1);
    }
}
