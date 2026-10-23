//! The probe steps of `kbf-act` (see [`crate::act`]): each makes one attempt that a
//! sandbox or the kernel may refuse, and reports what happened in one line on stdout,
//! `PROBE <name> ok` or `PROBE <name> errno=<n>` (`<name>` is the step's name, `<n>`
//! the OS error number), then exits 0 whatever the attempt came to. A test reads the
//! line; it never reads an exit status, so a refusal and a harness that failed to
//! start differ (a missing line, or a step that exited 1 or 2).
//!
//! - `try-read PATH`: opens `PATH` for reading and reads a byte of it;
//! - `try-write PATH`: opens `PATH` for writing; never creates it, never truncates it,
//!   writes nothing;
//! - `try-list DIR`: opens `DIR` and reads its first entry;
//! - `tcp-listen ADDR PORTFILE SECS`: binds `ADDR` (port 0 for any), writes the bound
//!   port and a newline to `PORTFILE` (made whole, by rename), prints its line, and
//!   holds the listener for `SECS` seconds; connections wait in its backlog;
//! - `tcp-connect ADDR`: connects to `ADDR`, within 5 s;
//! - `unix-listen PATH SECS`: binds a Unix socket at `PATH`, makes it mode 0777 (so
//!   the socket file's own mode is never what refuses a connection), prints its line,
//!   and holds the listener for `SECS` seconds;
//! - `unix-connect PATH`: connects to the Unix socket at `PATH`;
//! - `procargs PID`: asks for the arguments of process `PID` with sysctl
//!   `KERN_PROCARGS2` (macOS; elsewhere `errno=ENOSYS`);
//! - `proc-sees PID`: `ok` when `proc_listpids(PROC_ALL_PIDS)` lists `PID`, else
//!   `errno=ESRCH`, or the error of the call (macOS; elsewhere `errno=ENOSYS`);
//! - `ps-sees MARKER`: runs `/bin/ps -axww -o pid,uid,args` with an empty environment,
//!   and says `ok` when the arguments of some process of another user, not root, hold
//!   `MARKER`; `errno=ESRCH` when none does; the error of starting `ps`, or `EIO` when
//!   it fails and lists no such process. This step's own arguments, and the programs
//!   that started it (`launchctl asuser` runs as root), hold the marker too, so
//!   neither counts: only another user's process can be seen.
//!
//! A listen step prints its line when the socket is bound, before the hold, so a test
//! that waits for `PORTFILE` (or the socket) and for the line has both.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::{Error, write_line};

/// How long `tcp-connect` waits before it gives up with `ETIMEDOUT`.
const CONNECT_WITHIN: Duration = Duration::from_secs(5);

/// One probe step, checked.
#[derive(Debug, PartialEq)]
pub(crate) enum Probe<'a> {
    TryRead(&'a str),
    TryWrite(&'a str),
    TryList(&'a str),
    TcpListen {
        addr: SocketAddr,
        portfile: &'a str,
        hold: Duration,
    },
    TcpConnect(SocketAddr),
    UnixListen {
        path: &'a str,
        hold: Duration,
    },
    UnixConnect(&'a str),
    Procargs(i32),
    ProcSees(i32),
    PsSees(&'a str),
}

/// What an attempt came to.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Outcome {
    Ok,
    Errno(i32),
}

/// The probe step `words` spell, `None` when they spell no probe's name. A probe's name
/// with arguments that do not fit is a usage error.
pub(crate) fn parse<'a>(words: &[&'a str]) -> Result<Option<Probe<'a>>, Error> {
    Ok(Some(match words {
        ["try-read", path] => Probe::TryRead(path),
        ["try-write", path] => Probe::TryWrite(path),
        ["try-list", dir] => Probe::TryList(dir),
        ["tcp-listen", addr, portfile, secs] => Probe::TcpListen {
            addr: socket_addr(addr)?,
            portfile,
            hold: crate::act::seconds(secs)?,
        },
        ["tcp-connect", addr] => Probe::TcpConnect(socket_addr(addr)?),
        ["unix-listen", path, secs] => Probe::UnixListen {
            path,
            hold: crate::act::seconds(secs)?,
        },
        ["unix-connect", path] => Probe::UnixConnect(path),
        ["procargs", pid] => Probe::Procargs(pid_of(pid)?),
        ["proc-sees", pid] => Probe::ProcSees(pid_of(pid)?),
        ["ps-sees", marker] if !marker.is_empty() => Probe::PsSees(marker),
        _ => return Ok(None),
    }))
}

fn socket_addr(text: &str) -> Result<SocketAddr, Error> {
    text.parse()
        .map_err(|_| Error::Usage(format!("{text:?} is not an address and port")))
}

/// `text` as the id of a process: a positive number.
fn pid_of(text: &str) -> Result<i32, Error> {
    text.parse::<i32>()
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| Error::Usage(format!("{text:?} is not a process id")))
}

/// The line a probe prints.
pub(crate) fn line(name: &str, outcome: Outcome) -> String {
    match outcome {
        Outcome::Ok => format!("PROBE {name} ok"),
        Outcome::Errno(n) => format!("PROBE {name} errno={n}"),
    }
}

/// What `result` came to: the OS error number of a failure; `ETIMEDOUT` for a timeout
/// that carries none, `EIO` for any other failure that carries none.
pub(crate) fn outcome<T>(result: &std::io::Result<T>) -> Outcome {
    match result {
        Ok(_) => Outcome::Ok,
        Err(e) => Outcome::Errno(e.raw_os_error().unwrap_or(
            if e.kind() == std::io::ErrorKind::TimedOut {
                libc::ETIMEDOUT
            } else {
                libc::EIO
            },
        )),
    }
}

fn report(name: &str, outcome: Outcome) -> Result<(), Error> {
    write_line(std::io::stdout(), &line(name, outcome))
}

/// Makes the attempt `probe` names and prints its line.
pub(crate) fn perform(probe: &Probe<'_>) -> Result<(), Error> {
    match probe {
        Probe::TryRead(path) => report("try-read", outcome(&try_read(path))),
        Probe::TryWrite(path) => report(
            "try-write",
            outcome(&std::fs::OpenOptions::new().write(true).open(path)),
        ),
        Probe::TryList(dir) => report(
            "try-list",
            outcome(&std::fs::read_dir(dir).and_then(|mut entries| entries.next().transpose())),
        ),
        Probe::TcpListen {
            addr,
            portfile,
            hold,
        } => {
            let listener = tcp_listen(*addr, portfile);
            report("tcp-listen", outcome(&listener))?;
            hold_for(listener.ok(), *hold);
            Ok(())
        }
        Probe::TcpConnect(addr) => report(
            "tcp-connect",
            outcome(&TcpStream::connect_timeout(addr, CONNECT_WITHIN)),
        ),
        Probe::UnixListen { path, hold } => {
            let listener = unix_listen(path);
            report("unix-listen", outcome(&listener))?;
            hold_for(listener.ok(), *hold);
            Ok(())
        }
        Probe::UnixConnect(path) => report("unix-connect", outcome(&UnixStream::connect(path))),
        Probe::Procargs(pid) => report("procargs", outcome(&os::procargs(*pid))),
        Probe::ProcSees(pid) => report("proc-sees", os::proc_sees(*pid)),
        Probe::PsSees(marker) => report("ps-sees", ps_sees(marker)),
    }
}

fn try_read(path: &str) -> std::io::Result<usize> {
    std::fs::File::open(path)?.read(&mut [0u8; 1])
}

/// Holds `listener` (if any) for `hold`.
fn hold_for<T>(listener: Option<T>, hold: Duration) {
    if listener.is_some() {
        std::thread::sleep(hold);
    }
}

/// Binds `addr` and publishes the port in `portfile`, which appears whole.
fn tcp_listen(addr: SocketAddr, portfile: &str) -> std::io::Result<TcpListener> {
    let listener = TcpListener::bind(addr)?;
    let port = listener.local_addr()?.port();
    let part = format!("{portfile}.part");
    std::fs::File::create(&part)?.write_all(format!("{port}\n").as_bytes())?;
    std::fs::rename(&part, portfile)?;
    Ok(listener)
}

/// Binds a Unix socket at `path` that anyone may connect to.
fn unix_listen(path: &str) -> std::io::Result<UnixListener> {
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777))?;
    Ok(listener)
}

/// Whether `ps -axww -o pid,uid,args` says a process of another user, not root, has
/// `marker` in its arguments.
fn ps_sees(marker: &str) -> Outcome {
    let ran = Command::new("/bin/ps")
        .args(["-axww", "-o", "pid,uid,args"])
        .env_clear()
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let out = match ran {
        Ok(out) => out,
        Err(e) => return outcome::<()>(&Err(e)),
    };
    let listing = String::from_utf8_lossy(&out.stdout);
    let (pid, uid) = (std::process::id(), rustix::process::getuid().as_raw());
    ps_verdict(
        marker_seen(&listing, marker, pid, uid),
        out.status.success(),
    )
}

/// What `ps-sees` reports: `ok` when the marker was seen, `ESRCH` when `ps` listed the
/// processes and the marker was not among them, `EIO` when `ps` itself failed (so a
/// listing that could not be had never reads as "not seen").
pub(crate) fn ps_verdict(seen: bool, ps_ran: bool) -> Outcome {
    if seen {
        Outcome::Ok
    } else if ps_ran {
        Outcome::Errno(libc::ESRCH)
    } else {
        Outcome::Errno(libc::EIO)
    }
}

/// Whether `listing` (`ps -o pid,uid,args` output) holds a process other than `pid`,
/// of a user other than `uid` and root, whose arguments hold `marker`. A line that is
/// not a number, a number and arguments (the header) is skipped.
pub(crate) fn marker_seen(listing: &str, marker: &str, pid: u32, uid: u32) -> bool {
    listing.lines().any(|line| {
        let Some((p, rest)) = line.trim_start().split_once(char::is_whitespace) else {
            return false;
        };
        let Some((u, args)) = rest.trim_start().split_once(char::is_whitespace) else {
            return false;
        };
        match (p.parse::<u32>(), u.parse::<u32>()) {
            (Ok(p), Ok(u)) => p != pid && u != uid && u != 0 && args.contains(marker),
            _ => false,
        }
    })
}

#[cfg(target_os = "macos")]
mod os {
    use super::Outcome;

    /// `proc_listpids` type: every process.
    const PROC_ALL_PIDS: u32 = 1;

    /// Asks `sysctl(KERN_PROCARGS2)` for the arguments of `pid`, into a buffer of
    /// `KERN_ARGMAX` bytes (what `ps` does); `Ok` when the kernel answered.
    pub(super) fn procargs(pid: i32) -> std::io::Result<usize> {
        let mut max: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        // SAFETY: `mib` holds the 2 names it is told to; the kernel writes at most
        // `size` bytes, the size of `max`, at `max`; nothing is set (null, 0).
        let asked = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                std::ptr::from_mut(&mut max).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if asked != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buffer = vec![0u8; usize::try_from(max).unwrap_or(0).max(4096)];
        let mut size = buffer.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        // SAFETY: `mib` holds the 3 names it is told to; the kernel writes at most
        // `size` bytes, the length of `buffer`, into it and the count into `size`;
        // nothing is set (null, 0).
        let asked = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buffer.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if asked != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(size)
    }

    /// Whether `proc_listpids(PROC_ALL_PIDS)` lists `pid`.
    pub(super) fn proc_sees(pid: i32) -> Outcome {
        match list_pids() {
            Ok(pids) if pids.contains(&pid) => Outcome::Ok,
            Ok(_) => Outcome::Errno(libc::ESRCH),
            Err(e) => super::outcome::<()>(&Err(e)),
        }
    }

    fn list_pids() -> std::io::Result<Vec<libc::pid_t>> {
        // SAFETY: a null buffer asks only for the size needed.
        let bytes = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
        if bytes < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let size = std::mem::size_of::<libc::pid_t>();
        // Room for some more pids than the kernel reported, should some start meanwhile.
        let mut pids = vec![0 as libc::pid_t; bytes.unsigned_abs() as usize / size + 64];
        let room = i32::try_from(pids.len() * size).map_err(std::io::Error::other)?;
        // SAFETY: the buffer holds `room` bytes of pids and outlives the call.
        let wrote =
            unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr().cast(), room) };
        if wrote < 0 {
            return Err(std::io::Error::last_os_error());
        }
        pids.truncate(wrote.unsigned_abs() as usize / size);
        Ok(pids)
    }
}

#[cfg(not(target_os = "macos"))]
mod os {
    use super::Outcome;

    /// There is no `KERN_PROCARGS2` here.
    pub(super) fn procargs(_pid: i32) -> std::io::Result<usize> {
        Err(std::io::Error::from_raw_os_error(libc::ENOSYS))
    }

    /// There is no `proc_listpids` here.
    pub(super) fn proc_sees(_pid: i32) -> Outcome {
        Outcome::Errno(libc::ENOSYS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a line other than `PROBE <name> ok` / `PROBE <name> errno=<n>` (the
    /// tests of every other crate read these lines), an error with no OS number
    /// mapped to a success or to 0, and a timeout reported as a plain I/O failure.
    #[test]
    fn lines_and_outcomes() {
        assert_eq!(line("try-read", Outcome::Ok), "PROBE try-read ok");
        assert_eq!(
            line("tcp-connect", Outcome::Errno(61)),
            "PROBE tcp-connect errno=61"
        );
        assert_eq!(outcome(&Ok(7)), Outcome::Ok);
        let eperm: std::io::Result<()> = Err(std::io::Error::from_raw_os_error(libc::EPERM));
        assert_eq!(outcome(&eperm), Outcome::Errno(libc::EPERM));
        let timed: std::io::Result<()> = Err(std::io::ErrorKind::TimedOut.into());
        assert_eq!(outcome(&timed), Outcome::Errno(libc::ETIMEDOUT));
        let other: std::io::Result<()> = Err(std::io::Error::other("no number"));
        assert_eq!(outcome(&other), Outcome::Errno(libc::EIO));
    }

    /// Catches: a `ps` that failed read as "not seen" (a listing that could not be had
    /// must not pass for a refusal), and a marker seen read as anything but `ok`.
    ///
    /// Catches: the `ps` failure mapped to `ESRCH`; `seen` ignored.
    #[test]
    fn ps_sees_reports_seen_not_seen_and_a_failed_ps_apart() {
        assert_eq!(ps_verdict(true, true), Outcome::Ok);
        assert_eq!(ps_verdict(true, false), Outcome::Ok);
        assert_eq!(ps_verdict(false, true), Outcome::Errno(libc::ESRCH));
        assert_eq!(ps_verdict(false, false), Outcome::Errno(libc::EIO));
    }

    /// Catches: `ps-sees` with an empty marker taken as a probe (every line would hold
    /// it); it is not a probe word, so the act step refuses it as an unknown step.
    #[test]
    fn an_empty_ps_sees_marker_is_not_a_probe() {
        assert!(matches!(parse(&["ps-sees", ""]), Ok(None)));
        assert!(matches!(
            parse(&["ps-sees", "x"]),
            Ok(Some(Probe::PsSees("x")))
        ));
    }

    /// Catches: a marker found in the process that asks (its own arguments name it), in
    /// the asking user's other processes, in root's (the `launchctl asuser` that
    /// started the asker holds the marker), in the header, or on a line that is the
    /// marker alone (no pid and uid before it); and a marker missed in another user's
    /// process whatever its arguments' spacing.
    #[test]
    fn the_marker_counts_only_in_another_users_process() {
        let (me, uid) = (500, 600);
        let listing = "  PID   UID ARGS\n\
                       1     0 /sbin/launchd\n\
                       500   600 kbf-act ps-sees needle-7391\n\
                       501   600 kbf-act sleep needle-7391\n\
                       502   0   launchctl asuser 600 kbf-act ps-sees needle-7391\n\
                       503   601 kbf-act   sleep   needle-7392\n\
                       junk line\n\
                       needle-7391\n";
        assert!(!marker_seen(listing, "needle-7391", me, uid));
        let with_other = format!("{listing}504   601 kbf-act sleep needle-7391\n");
        assert!(marker_seen(&with_other, "needle-7391", me, uid));
        assert!(marker_seen(&with_other, "needle-7392", me, uid));
        assert!(marker_seen(&with_other, "sleep   needle-7392", me, uid));
        assert!(!marker_seen(&with_other, "needle-7393", me, uid));
        // The asker's pid is excluded by number, whoever's it is.
        assert!(!marker_seen("503 601 x needle\n", "needle", 503, uid));
        assert!(!marker_seen("", "needle", me, uid));
    }
}
