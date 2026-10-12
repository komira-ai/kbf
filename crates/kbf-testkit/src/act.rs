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
//! - `spawn-child [--pid FILE] STEP [;; STEP]...`: starts this program running the
//!   steps (`;;` separates the child's steps, which may not hold a `spawn-child` with
//!   steps of its own) with the same stdin, stdout and stderr, and goes on without
//!   waiting for it; with `--pid`, appends the child's process id and a newline to
//!   `FILE`, or prints them when `FILE` is `-`;
//! - `run PROGRAM [ARGS]...`: runs `PROGRAM` (looked up on `PATH` when it has no `/`)
//!   with the same stdin, stdout, stderr and environment, waits for it, and fails
//!   unless it exits 0;
//! - `setsid`: makes this process the leader of a new session (fails if it leads a
//!   process group already);
//! - `alloc MIB`: touches `MIB` MiB (rounded up to 16 MiB) of distinct, incompressible
//!   memory and holds it until the program exits;
//! - `upper OUT IN`: writes `IN`, ASCII upper-cased, to `OUT`;
//! - `rename FROM TO`: renames `FROM` to `TO`, as rename(2) does;
//! - `symlink TARGET LINK`: makes `LINK` a symbolic link to `TARGET`;
//! - `chmod MODE PATH`: sets the permission bits of `PATH` to `MODE`, in octal (at most
//!   `7777`);
//! - `wait-for PATH SECS`: waits until `PATH` exists, looking every 10 ms, and fails
//!   if it does not within `SECS` seconds (`0` looks once);
//! - `mkdir PATH`: makes the directory `PATH` and any of its parents that are missing;
//! - `cat FILE`: writes the bytes of `FILE` to stdout;
//! - `mtime PATH SECS`: sets the modification time of `PATH` (a file or a directory)
//!   to `SECS` whole seconds after the Unix epoch;
//! - `pgid`: prints the id of its process group.
//!
//! [`main`] first looks at argv0: run through a link named for a tool of
//! [`crate::fake`], it answers as that tool instead.
//!
//! A test binary has no `main` of its own. One that expands
//! [`act_test_hook!`](crate::act_test_hook) answers in the same way when it is run
//! through a link named [`NAME`] or for a fake's tool, which [`link_self`] makes: so a
//! crate's tests, unit tests included, run the act steps without a bin of their own.

use std::ffi::OsStr;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use rustix::process::Signal;

use crate::{Error, create, failed, number, read, write_line};

/// The base name a link to a test binary that expands
/// [`act_test_hook!`](crate::act_test_hook) has, to run the act steps.
pub const NAME: &str = "kbf-act";

/// Runs `kbf-act` on the process's arguments, or the fake its argv0 names.
#[must_use]
pub fn main() -> ExitCode {
    let (argv0, args) = crate::arguments();
    ExitCode::from(answer(&argv0, args))
}

/// The exit status of the fake `argv0` names, or else of the steps in `args`.
fn answer(argv0: &str, args: Result<Vec<String>, Error>) -> u8 {
    match crate::fake::Tool::named(argv0) {
        Some(tool) => crate::status(
            tool.name(),
            args.and_then(|args| crate::fake::run(tool, argv0, &args)),
        ),
        None => crate::status(NAME, args.and_then(|args| run(&args))),
    }
}

/// [`main`] for a test binary, which has no `main` of its own: when argv0's base name
/// is [`NAME`] or a fake's tool, answers as [`main`] does and exits with the status;
/// otherwise returns, and the test harness starts. Called by
/// [`act_test_hook!`](crate::act_test_hook) before the harness reads its arguments.
#[doc(hidden)]
pub fn hook() {
    let (argv0, args) = crate::arguments();
    if Path::new(&argv0).file_name() == Some(OsStr::new(NAME))
        || crate::fake::Tool::named(&argv0).is_some()
    {
        std::process::exit(i32::from(answer(&argv0, args)));
    }
}

/// Makes `link` a symbolic link to the running program. A test binary that expands
/// [`act_test_hook!`](crate::act_test_hook) runs the act steps through a link named
/// [`NAME`], and answers as a fake through one named for its tool.
///
/// # Errors
///
/// When the running program's path cannot be read or the link cannot be made.
pub fn link_self(link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(std::env::current_exe()?, link)
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
    SpawnChild {
        /// Where the child's process id goes: a file, `-` for stdout, or nowhere.
        pid: Option<&'a str>,
        /// The child's arguments: its steps, separated by `;`.
        args: Vec<&'a str>,
    },
    Run(&'a [String]),
    Setsid,
    Alloc(u64),
    Upper {
        out: &'a str,
        input: &'a str,
    },
    Rename {
        from: &'a str,
        to: &'a str,
    },
    Symlink {
        target: &'a str,
        link: &'a str,
    },
    Chmod {
        mode: u32,
        path: &'a str,
    },
    WaitFor {
        path: &'a str,
        within: Duration,
    },
    Mkdir(&'a str),
    Cat(&'a str),
    Mtime {
        path: &'a str,
        secs: u64,
    },
    Pgid,
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
        ["spawn-child", "--pid", pid, ..] => Step::SpawnChild {
            pid: Some(*pid),
            args: child(&strs[3..])?,
        },
        ["spawn-child", ..] => Step::SpawnChild {
            pid: None,
            args: child(&strs[1..])?,
        },
        ["run", ..] if words.len() > 1 => Step::Run(&words[1..]),
        ["setsid"] => Step::Setsid,
        ["alloc", mib] => Step::Alloc(number(mib)?),
        ["upper", out, input] => Step::Upper { out, input },
        ["rename", from, to] => Step::Rename { from, to },
        ["symlink", target, link] => Step::Symlink { target, link },
        ["chmod", mode, path] => Step::Chmod {
            mode: u32::from_str_radix(mode, 8)
                .ok()
                .filter(|m| *m <= 0o7777)
                .ok_or_else(|| Error::Usage(format!("{mode:?} is not an octal mode")))?,
            path,
        },
        ["wait-for", path, secs] => Step::WaitFor {
            path,
            within: seconds(secs)?,
        },
        ["mkdir", path] => Step::Mkdir(path),
        ["cat", path] => Step::Cat(path),
        ["mtime", path, secs] => Step::Mtime {
            path,
            secs: number(secs)?,
        },
        ["pgid"] => Step::Pgid,
        _ => return Err(Error::Usage(format!("bad step: {strs:?}"))),
    })
}

/// The arguments of a `spawn-child`'s child: `words`, its steps separated by `;;`,
/// with `;` in place of each `;;`, every step checked. (A `spawn-child` among them
/// sees no `;;`, as they are all taken here: its child runs one step.)
fn child<'a>(words: &[&'a str]) -> Result<Vec<&'a str>, Error> {
    let steps: Vec<&[&str]> = words.split(|w| *w == ";;").collect();
    for words in &steps {
        let owned: Vec<String> = words.iter().map(|&w| w.to_owned()).collect();
        step(&owned)?;
    }
    Ok(steps.join(&";"))
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
            Step::SpawnChild { pid, args } => {
                let child = std::env::current_exe()
                    .and_then(|exe| Command::new(exe).arg0(NAME).args(args).spawn())
                    .map_err(failed(format!("spawn {args:?}")))?;
                match pid {
                    Some("-") => write_line(std::io::stdout(), &child.id().to_string())?,
                    Some(file) => append(file, &child.id().to_string())?,
                    None => {}
                }
            }
            Step::Run(argv) => {
                let status = Command::new(&argv[0])
                    .args(&argv[1..])
                    .status()
                    .map_err(failed(format!("run {}", argv[0])))?;
                if !status.success() {
                    return Err(Error::Failed(format!("{} ended: {status}", argv[0])));
                }
            }
            Step::Setsid => {
                rustix::process::setsid().map_err(failed("setsid"))?;
            }
            Step::Alloc(mib) => held.extend(crate::touch(*mib, Duration::ZERO)),
            Step::Upper { out, input } => create(out, &read(input)?.to_ascii_uppercase())?,
            Step::Rename { from, to } => {
                std::fs::rename(from, to).map_err(failed(format!("rename {from} to {to}")))?;
            }
            Step::Symlink { target, link } => std::os::unix::fs::symlink(target, link)
                .map_err(failed(format!("link {link} to {target}")))?,
            Step::Chmod { mode, path } => {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(*mode))
                    .map_err(failed(format!("chmod {path}")))?;
            }
            Step::WaitFor { path, within } => wait_for(path, *within)?,
            Step::Mkdir(path) => {
                std::fs::create_dir_all(path).map_err(failed(format!("mkdir {path}")))?;
            }
            Step::Cat(path) => std::io::stdout()
                .write_all(&read(path)?)
                .and_then(|()| std::io::stdout().flush())
                .map_err(failed("write"))?,
            Step::Mtime { path, secs } => std::fs::File::open(path)
                .and_then(|file| {
                    file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(*secs))
                })
                .map_err(failed(format!("mtime {path}")))?,
            Step::Pgid => write_line(
                std::io::stdout(),
                &rustix::process::getpgrp().as_raw_nonzero().to_string(),
            )?,
        }
    }
    drop(held);
    Ok(None)
}

/// Returns once `path` exists, looking every 10 ms; fails when it does not within
/// `within`.
fn wait_for(path: &str, within: Duration) -> Result<(), Error> {
    let until = Instant::now() + within;
    while !Path::new(path).exists() {
        if Instant::now() >= until {
            return Err(Error::Failed(format!("{path} not there after {within:?}")));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Appends `text` and a newline to the file at `path`, creating it, in one write: the
/// lines of processes appending to one file at once stay whole.
pub(crate) fn append(path: &str, text: &str) -> Result<(), Error> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(format!("{text}\n").as_bytes()))
        .map_err(failed(format!("append to {path}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split(' ').map(str::to_owned).collect()
    }

    /// Catches a step taken for the wrong verb, its arguments misread (a `chmod` mode
    /// read as decimal, or above 7777; a `spawn-child`'s `;;` not turned into the `;`
    /// its child splits at, or its `--pid` taken for a step), and a malformed step
    /// accepted (it would run with a default the test did not ask for), a child's step
    /// included.
    #[test]
    fn steps_parse_or_are_refused() {
        let run = args("/bin/x a b");
        let all = args(
            "exit 7 ; sleep 0.25 ; echo a b ; echo --stderr c ; append f x y ; env A ; \
             env B dflt ; pwd ; id ; kill-self 9 ; spawn-child sleep 1 ; \
             spawn-child --pid - setsid ;; sleep 1 ;; exit 0 ; spawn-child --pid f spawn-child id ; \
             run /bin/x a b ; setsid ; alloc 32 ; upper o i ; rename a b ; symlink t l ; chmod 750 p ; \
             chmod 7777 q ; wait-for w 1.5 ; mkdir m/n ; cat c ; mtime t 946684800 ; pgid",
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
            Step::SpawnChild {
                pid: None,
                args: vec!["sleep", "1"],
            },
            Step::SpawnChild {
                pid: Some("-"),
                args: vec!["setsid", ";", "sleep", "1", ";", "exit", "0"],
            },
            Step::SpawnChild {
                pid: Some("f"),
                args: vec!["spawn-child", "id"],
            },
            Step::Run(&run),
            Step::Setsid,
            Step::Alloc(32),
            Step::Upper {
                out: "o",
                input: "i",
            },
            Step::Rename { from: "a", to: "b" },
            Step::Symlink {
                target: "t",
                link: "l",
            },
            Step::Chmod {
                mode: 0o750,
                path: "p",
            },
            Step::Chmod {
                mode: 0o7777,
                path: "q",
            },
            Step::WaitFor {
                path: "w",
                within: Duration::from_millis(1500),
            },
            Step::Mkdir("m/n"),
            Step::Cat("c"),
            Step::Mtime {
                path: "t",
                secs: 946_684_800,
            },
            Step::Pgid,
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
            "rename a",
            "symlink t",
            "chmod 750",
            "chmod 10000 p",
            "chmod 8 p",
            "chmod rwx p",
            "wait-for w",
            "wait-for w soon",
            "spawn-child --pid",
            "spawn-child --pid f",
            "spawn-child echo a ;; ;; exit 0",
            "spawn-child exit 0 ;; nope",
            "run",
            "mkdir",
            "mkdir a b",
            "cat",
            "mtime t",
            "mtime t -1",
            "mtime t 1.5",
            "pgid x",
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
