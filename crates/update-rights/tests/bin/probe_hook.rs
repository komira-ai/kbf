//! A hook for the root tests: it does one thing its first argument names, with the arguments
//! up to a lone `--`, and reports what happened in the contract's last line. Every operation
//! writes a line of noise first, so the last line is the report and nothing else.
//!
//! The command line is the one the updater builds: `<this program> <op> [args] -- <step> ...`,
//! the operation and its arguments being the declaration's `hook_args`.

use std::io::Write;
use std::process::ExitCode;

use update_hook::Report;

fn say(code: u8, detail: &str) -> ExitCode {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "a line of noise before the report");
    let report = Report {
        detail: Some(detail.to_owned()),
        ..Report::default()
    };
    let _ = report.write_to(&mut out);
    ExitCode::from(code)
}

fn outcome<T, E: std::fmt::Display>(
    result: Result<T, E>,
    ok: impl FnOnce(T) -> String,
) -> ExitCode {
    match result {
        Ok(value) => say(0, &ok(value)),
        Err(e) => say(1, &e.to_string()),
    }
}

/// The value of a field of `/proc/self/status`, trimmed.
fn status_field(name: &str) -> String {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|line| {
            line.strip_prefix(name)
                .map(|v| v.trim_start_matches(':').trim().to_owned())
        })
        .unwrap_or_else(|| "?".to_owned())
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let op = args.next().unwrap_or_default();
    let op_args: Vec<String> = args.by_ref().take_while(|arg| arg != "--").collect();
    let rest: Vec<String> = args.collect();
    let arg = |n: usize| op_args.get(n).cloned().unwrap_or_default();
    match op.as_str() {
        // Write a file: fails where the filesystem is read-only or the account may not write.
        "write" => outcome(std::fs::write(arg(0), "written by the probe\n"), |()| {
            "written".into()
        }),
        "read" => outcome(std::fs::read_to_string(arg(0)), |text| {
            text.trim().to_owned()
        }),
        // The capability sets of this process, as the kernel prints them.
        "caps" => say(
            0,
            &format!(
                "eff={} amb={} bnd={}",
                status_field("CapEff"),
                status_field("CapAmb"),
                status_field("CapBnd")
            ),
        ),
        "ids" => say(
            0,
            &format!(
                "uid={} gid={} groups={}",
                status_field("Uid"),
                status_field("Gid"),
                status_field("Groups")
            ),
        ),
        // Open a socket of one family.
        "socket" => match arg(0).as_str() {
            "unix" => outcome(std::os::unix::net::UnixDatagram::unbound(), |_| {
                "unix opened".into()
            }),
            "inet" => outcome(std::net::UdpSocket::bind("127.0.0.1:0"), |_| {
                "inet opened".into()
            }),
            "inet6" => outcome(std::net::UdpSocket::bind("[::1]:0"), |_| {
                "inet6 opened".into()
            }),
            other => say(2, &format!("unknown family {other}")),
        },
        "env" => say(
            0,
            &std::env::var(arg(0)).unwrap_or_else(|_| "unset".to_owned()),
        ),
        // Run a program with these arguments and take its status; its output is the log's.
        "run" => {
            let Some((program, program_args)) = op_args.split_first() else {
                return say(2, "run needs a program");
            };
            match std::process::Command::new(program)
                .args(program_args)
                .output()
            {
                Ok(done) => {
                    let text = format!(
                        "{} {}",
                        String::from_utf8_lossy(&done.stdout).trim(),
                        String::from_utf8_lossy(&done.stderr).trim()
                    );
                    say(u8::from(!done.status.success()), text.trim())
                }
                Err(e) => say(1, &format!("cannot run {program}: {e}")),
            }
        }
        // The kernel's table of file locks, whole.
        "locks" => {
            let table = std::fs::read_to_string("/proc/locks");
            if let Ok(text) = &table {
                let _ = std::io::stdout().write_all(text.as_bytes());
            }
            outcome(table, |text| format!("{} lines", text.lines().count()))
        }
        // Exit with a code the test names, to see it come back.
        "exit" => say(arg(0).parse().unwrap_or(1), &format!("exit {}", arg(0))),
        // The arguments after the lone `--`, as the step gave them.
        "args" => say(0, &rest.join(" ")),
        // Do nothing and succeed.
        "ok" => say(0, "ran"),
        // Stay for ever: a unit for the tests to stop.
        "park" => loop {
            std::thread::park();
        },
        other => say(2, &format!("unknown operation {other:?}")),
    }
}
