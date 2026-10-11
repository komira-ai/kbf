//! `kbf-act STEP [; STEP]...`: runs the steps in order and exits 0 after the last,
//! unless a step exits first. Steps are separated by an argument that is exactly `;`.
//! Every step is checked before the first runs, so a malformed one exits 2 having done
//! nothing.
//!
//! The steps:
//! - `exit CODE`: exits with status `CODE` (0 to 255) at once;
//! - `sleep SECS`: sleeps `SECS` seconds, fractions allowed;
//! - `echo [--stderr] WORDS...`: prints the words, joined by spaces, and a newline on
//!   stdout (on stderr with `--stderr`);
//! - `append FILE WORDS...`: appends the words, joined by spaces, and a newline to
//!   `FILE`, creating it;
//! - `env NAME [DEFAULT]`: prints the value of the environment variable `NAME`; when it
//!   is unset, prints `DEFAULT`, or fails without one;
//! - `pwd`: prints the working directory;
//! - `id`: prints the real user id;
//! - `kill-self SIGNAL`: sends itself signal number `SIGNAL`, and fails if it is still
//!   running afterwards (the signal was ignored or caught);
//! - `spawn-child STEP`: starts this program running `STEP` with the same stdin,
//!   stdout and stderr, and goes on without waiting for it;
//! - `setsid`: makes this process the leader of a new session (fails if it leads a
//!   process group already);
//! - `alloc MIB`: touches `MIB` MiB (rounded up to 16 MiB) of distinct, incompressible
//!   memory and holds it until the program exits;
//! - `upper OUT IN`: writes `IN`, ASCII upper-cased, to `OUT`.
//!
//! [`main`] first looks at argv0: run through a link named for a tool of
//! [`crate::fake`], it answers as that tool instead.

use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitCode};
use std::time::Duration;

use rustix::process::Signal;

use crate::{Error, create, failed, number, read, write_line};

/// Runs `kbf-act` on the process's arguments, or the fake its argv0 names.
#[must_use]
pub fn main() -> ExitCode {
    let (argv0, args) = crate::arguments();
    match crate::fake::Tool::named(&argv0) {
        Some(tool) => crate::exit_with(
            tool.name(),
            args.and_then(|args| crate::fake::run(tool, &argv0, &args)),
        ),
        None => crate::exit_with("kbf-act", args.and_then(|args| run(&args))),
    }
}

/// Checks and runs the steps in `args`, returning the exit status.
fn run(args: &[String]) -> Result<u8, Error> {
    Ok(perform(&parse(args)?)?.unwrap_or(0))
}

/// One step, checked.
#[derive(Debug, PartialEq)]
pub(crate) enum Step<'a> {
    Exit(u8),
    Sleep(Duration),
    Echo {
        stderr: bool,
        text: String,
    },
    Append {
        path: &'a str,
        text: String,
    },
    Env {
        name: &'a str,
        default: Option<&'a str>,
    },
    Pwd,
    Id,
    KillSelf(Signal),
    SpawnChild(&'a [String]),
    Setsid,
    Alloc(u64),
    Upper {
        out: &'a str,
        input: &'a str,
    },
}

/// The steps of `args`, split at each `;`, every one checked.
pub(crate) fn parse(args: &[String]) -> Result<Vec<Step<'_>>, Error> {
    if args.is_empty() {
        return Err(Error::Usage("usage: kbf-act STEP [; STEP]...".to_owned()));
    }
    args.split(|a| a == ";").map(step).collect()
}

fn step(words: &[String]) -> Result<Step<'_>, Error> {
    let strs: Vec<&str> = words.iter().map(String::as_str).collect();
    Ok(match strs.as_slice() {
        ["exit", code] => Step::Exit(
            code.parse()
                .map_err(|_| Error::Usage(format!("{code:?} is not an exit status")))?,
        ),
        ["sleep", secs] => Step::Sleep(seconds(secs)?),
        ["echo", "--stderr", text @ ..] => Step::Echo {
            stderr: true,
            text: text.join(" "),
        },
        ["echo", text @ ..] => Step::Echo {
            stderr: false,
            text: text.join(" "),
        },
        ["append", path, text @ ..] => Step::Append {
            path,
            text: text.join(" "),
        },
        ["env", name] => Step::Env {
            name,
            default: None,
        },
        ["env", name, default] => Step::Env {
            name,
            default: Some(*default),
        },
        ["pwd"] => Step::Pwd,
        ["id"] => Step::Id,
        ["kill-self", signal] => Step::KillSelf(
            signal
                .parse::<i32>()
                .ok()
                .and_then(Signal::from_named_raw)
                .ok_or_else(|| Error::Usage(format!("{signal:?} is not a signal number")))?,
        ),
        ["spawn-child", ..] if words.len() > 1 => {
            step(&words[1..])?;
            Step::SpawnChild(&words[1..])
        }
        ["setsid"] => Step::Setsid,
        ["alloc", mib] => Step::Alloc(number(mib)?),
        ["upper", out, input] => Step::Upper { out, input },
        _ => return Err(Error::Usage(format!("bad step: {strs:?}"))),
    })
}

/// `text` as a non-negative, finite number of seconds.
fn seconds(text: &str) -> Result<Duration, Error> {
    text.parse::<f64>()
        .ok()
        .and_then(|s| Duration::try_from_secs_f64(s).ok())
        .ok_or_else(|| Error::Usage(format!("{text:?} is not a number of seconds")))
}

/// Runs `steps` in order: `Some` status when one exits, `None` when all ran.
pub(crate) fn perform(steps: &[Step<'_>]) -> Result<Option<u8>, Error> {
    let mut held = Vec::new();
    for step in steps {
        match step {
            Step::Exit(code) => return Ok(Some(*code)),
            Step::Sleep(time) => std::thread::sleep(*time),
            Step::Echo { stderr: true, text } => write_line(std::io::stderr(), text)?,
            Step::Echo {
                stderr: false,
                text,
            } => write_line(std::io::stdout(), text)?,
            Step::Append { path, text } => append(path, text)?,
            Step::Env { name, default } => {
                let value = match (std::env::var_os(name), default) {
                    (Some(value), _) => value.to_string_lossy().into_owned(),
                    (None, Some(default)) => (*default).to_owned(),
                    (None, None) => return Err(Error::Failed(format!("{name} is not set"))),
                };
                write_line(std::io::stdout(), &value)?;
            }
            Step::Pwd => {
                let dir = std::env::current_dir().map_err(failed("working directory"))?;
                write_line(std::io::stdout(), &dir.display().to_string())?;
            }
            Step::Id => write_line(
                std::io::stdout(),
                &rustix::process::getuid().as_raw().to_string(),
            )?,
            Step::KillSelf(signal) => {
                rustix::process::kill_process(rustix::process::getpid(), *signal)
                    .map_err(failed("kill"))?;
                return Err(Error::Failed(format!(
                    "still running after signal {}",
                    signal.as_raw()
                )));
            }
            Step::SpawnChild(child) => {
                std::env::current_exe()
                    .and_then(|exe| Command::new(exe).arg0("kbf-act").args(*child).spawn())
                    .map_err(failed(format!("spawn {child:?}")))?;
            }
            Step::Setsid => {
                rustix::process::setsid().map_err(failed("setsid"))?;
            }
            Step::Alloc(mib) => held.extend(crate::touch(*mib, Duration::ZERO)),
            Step::Upper { out, input } => create(out, &read(input)?.to_ascii_uppercase())?,
        }
    }
    drop(held);
    Ok(None)
}

/// Appends `text` and a newline to the file at `path`, creating it.
pub(crate) fn append(path: &str, text: &str) -> Result<(), Error> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{text}"))
        .map_err(failed(format!("append to {path}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split(' ').map(str::to_owned).collect()
    }

    /// Catches a step taken for the wrong verb, its arguments misread, and a malformed
    /// step accepted (it would run with a default the test did not ask for).
    #[test]
    fn steps_parse_or_are_refused() {
        let child = args("sleep 1");
        let all = args(
            "exit 7 ; sleep 0.25 ; echo a b ; echo --stderr c ; append f x y ; env A ; \
             env B dflt ; pwd ; id ; kill-self 9 ; spawn-child sleep 1 ; setsid ; alloc 32 ; \
             upper o i",
        );
        let want = [
            Step::Exit(7),
            Step::Sleep(Duration::from_millis(250)),
            Step::Echo {
                stderr: false,
                text: "a b".to_owned(),
            },
            Step::Echo {
                stderr: true,
                text: "c".to_owned(),
            },
            Step::Append {
                path: "f",
                text: "x y".to_owned(),
            },
            Step::Env {
                name: "A",
                default: None,
            },
            Step::Env {
                name: "B",
                default: Some("dflt"),
            },
            Step::Pwd,
            Step::Id,
            Step::KillSelf(Signal::KILL),
            Step::SpawnChild(&child),
            Step::Setsid,
            Step::Alloc(32),
            Step::Upper {
                out: "o",
                input: "i",
            },
        ];
        assert_eq!(parse(&all).expect("parses"), want);
        for bad in [
            "",
            "exit",
            "exit 256",
            "exit -1",
            "sleep x",
            "sleep -1",
            "sleep inf",
            "env",
            "env a b c",
            "pwd x",
            "kill-self 0",
            "kill-self TERM",
            "spawn-child",
            "spawn-child nope",
            "alloc x",
            "upper o",
            "echo a ; ; exit 0",
            "nope",
        ] {
            let words = args(bad);
            let got = parse(&words);
            assert!(
                matches!(got, Err(Error::Usage(_))),
                "{bad:?} was not refused: {got:?}"
            );
        }
        assert!(matches!(parse(&[]), Err(Error::Usage(_))));
    }
}
