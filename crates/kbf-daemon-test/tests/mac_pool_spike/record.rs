//! How the spike writes down what it saw: `SPIKE` lines on stdout (`--nocapture` sends
//! them to the beta log; `grep '^SPIKE'` reads them back), and programs run with a time
//! limit, their status, time and output kept.

use std::fmt::Display;
use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a program the spike runs may take before it is killed and the run goes on.
pub const LIMIT: Duration = Duration::from_secs(60);

/// One line: `SPIKE <key>: <text>`.
pub fn say(key: &str, text: impl Display) {
    println!("SPIKE {key}: {text}");
}

/// The first `max` lines of `text`, each as `SPIKE <key> | <label>: <line>`, and a line
/// saying how many more there were.
pub fn lines(key: &str, label: &str, text: &str, max: usize) {
    let mut all = text.lines();
    for line in all.by_ref().take(max) {
        println!("SPIKE {key} | {label}: {line}");
    }
    let more = all.count();
    if more > 0 {
        println!("SPIKE {key} | {label}: ... {more} more lines");
    }
}

/// Runs `f` and says how long it took: `SPIKE time <what>: 1.234s`.
pub fn timed<T>(what: &str, f: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let value = f();
    say(
        &format!("time {what}"),
        format!("{:.3?}", started.elapsed()),
    );
    value
}

/// What a program did.
pub struct Ran {
    /// `exit 0`, `exit 1`, `signal 9`, `timed out after 60s` or `not started: why`.
    pub status: String,
    pub stdout: String,
    pub stderr: String,
    pub took: Duration,
}

/// Runs `program` with `args` and no stdin, killing it after [`LIMIT`], and writes down
/// the command, its status and time (`SPIKE <key>: ...`) and up to `max` lines of its
/// output.
pub fn run(key: &str, program: &str, args: &[&str], max: usize) -> Ran {
    let ran = run_quiet(program, args);
    say(
        key,
        format!(
            "{} in {:.3?}: {program} {}",
            ran.status,
            ran.took,
            args.join(" ")
        ),
    );
    lines(key, "out", &ran.stdout, max);
    lines(key, "err", &ran.stderr, max);
    ran
}

/// [`run`], writing nothing down.
pub fn run_quiet(program: &str, args: &[&str]) -> Ran {
    let started = Instant::now();
    let spawned = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(why) => {
            return Ran {
                status: format!("not started: {why}"),
                stdout: String::new(),
                stderr: String::new(),
                took: started.elapsed(),
            };
        }
    };
    // Read on threads of their own, so a full pipe never stalls the program.
    let reader = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let out = reader(child.stdout.take().map(|p| Box::new(p) as _));
    let err = reader(child.stderr.take().map(|p| Box::new(p) as _));
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let text = match (
                    status.code(),
                    std::os::unix::process::ExitStatusExt::signal(&status),
                ) {
                    (Some(code), _) => format!("exit {code}"),
                    (None, Some(signal)) => format!("signal {signal}"),
                    (None, None) => status.to_string(),
                };
                break (text, false);
            }
            Ok(None) if started.elapsed() < LIMIT => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break (format!("timed out after {LIMIT:?}"), true);
            }
            Err(why) => break (format!("wait failed: {why}"), false),
        }
    };
    // A program killed at the limit may have left a child holding the pipes open: the
    // threads are then not waited for.
    let (stdout, stderr) = if timed_out {
        (String::new(), String::new())
    } else {
        (
            out.join().unwrap_or_default(),
            err.join().unwrap_or_default(),
        )
    };
    Ran {
        status,
        stdout,
        stderr,
        took: started.elapsed(),
    }
}
