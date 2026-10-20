//! The caller of `kbf-mac-session` the root tests run (`tests/as_root.rs`): the test
//! binary answers as this client when it runs through a signed copy named
//! [`super::CLIENT`] or [`super::IMPOSTOR`]. It stands in for the installed `kbf`: the helper
//! is started with the client copy's cdhash as the daemon requirement.
//!
//! The verbs, by the copy's name (`client` below):
//!
//! ```text
//! client create <socket> <lease> [grant token]   prints the uid
//! client run <socket> <lease> <dir> <argv>...   prints "exit <code>" or "signal <n>"
//! client run-gui <socket> <lease> <dir> <argv>...   the same, in the GUI session
//! client run-quiet <socket> <lease> <dir> <argv>...   as run, with the action's
//!                                                      stdout and stderr /dev/null
//! client session-ready <socket> <lease>    prints "true" or "false"
//! client kill <socket> <lease>
//! client delete <socket> <lease>           prints "existed" or "absent"
//! client reset <socket> [held lease]...     prints "kept <lease|none> deleted <leases>"
//! client scan <socket>                     prints "clean" or "dirty", then "+ <added>",
//!                                                      "- <removed>" and "! <error>" lines
//! client baseline <socket>                 prints "<class> <items>" lines
//! client reboot-dirty <socket>             reboots the Mac if the last scan was dirty
//! client grant <seed> <serial> <lease> <issued>   prints a grant token
//! client pubkey <seed>                     prints the grant key file line
//! client connect-then-exec <socket> <program> <args>...
//! client write-then-exec <socket> <lease> <program> <args>...
//! client replies                           prints the replies on KBF_CI_FD
//! client exec-probe                        prints audit tokens across an exec
//! client spawn <gui|background> <uid> <gid> <user> <dir> <argv>...
//!                                                       prints "exit <code>"
//! client groups                             prints this process's groups
//! ```
//!
//! `spawn` starts `<argv>` as `kbf-mac-session`'s `run` does (`spawn::spawn`), with
//! stdout and stderr this process's, without the helper's socket or ledger: run as root,
//! it shows where a process lands in each session, for a uid that has a GUI session
//! (the runner's own) where no lease user has one. In the GUI session it executes this
//! binary in the `as-user` mode, as the helper executes itself, so this binary also
//! answers `as-user` (`kbf_mac_session::macos::gui::as_user`).
//!
//! The attack S4.3 names, connecting as one binary and then becoming the genuine one,
//! in two forms:
//! - `connect-then-exec` connects, waits (up to 3 seconds) until the helper has
//!   answered the connection, then executes `<program>` with the connection open as
//!   `KBF_CI_FD`; a `kill` run that way carries out the whole exchange over that
//!   connection, as the genuine binary.
//! - `write-then-exec` connects, writes a `kill-uid <lease>` request at once (with a
//!   made-up nonce), then executes `<program>`, for example `replies`, which only reads
//!   and prints what the helper answers on `KBF_CI_FD`.
//!
//! `exec-probe` measures, without the helper, what `LOCAL_PEERTOKEN` reports for a
//! process that executes another program: the pid and pid version of the token before
//! the exec, after it, and once the process has exited.
//!
//! A refusal prints the helper's reason and exits 3; any other failure exits 4.

pub use macos::main;

mod macos {
    use std::io::{Read as _, Write as _};
    use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _};
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt as _;
    use std::process::Command;

    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use ed25519_dalek::{Signer as _, SigningKey};
    use kbf_mac_session::client::{self, Client, ClientError};
    use kbf_mac_session::proto::{self, Reply, Request, Session};
    use kbf_mac_session::spawn::Spawn;
    use time::OffsetDateTime;
    use time::macros::format_description;

    /// Answers as the client, and returns the exit status.
    pub fn main() -> i32 {
        let raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        if let Some((mode, rest)) = raw.split_first()
            && mode == kbf_mac_session::macos::gui::AS_USER
        {
            // It returns only on failure, with the reason on stderr (exit 126 for a
            // refusal, 127 for a failed exec); an `ExitCode` has no raw value to pass
            // on, so both exit 126 here.
            let _failed = kbf_mac_session::macos::gui::as_user(rest);
            return 126;
        }
        let args: Vec<String> = std::env::args().skip(1).collect();
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        match run(&words) {
            Ok(()) => 0,
            Err(ClientError::Refused(why)) => {
                eprintln!("refused: {why}");
                3
            }
            Err(why) => {
                eprintln!("failed: {why}");
                4
            }
        }
    }

    fn key(seed: &str) -> SigningKey {
        let byte = u8::from_str_radix(seed, 16).expect("a seed is one hex byte");
        SigningKey::from_bytes(&[byte; 32])
    }

    fn run(words: &[&str]) -> Result<(), ClientError> {
        match words {
            ["create", socket, lease, rest @ ..] => {
                let uid = Client::new(socket).user_create(lease, rest.first().copied())?;
                println!("{uid}");
            }
            ["session-ready", socket, lease] => {
                println!("{}", Client::new(socket).session_ready(lease)?);
            }
            [
                verb @ ("run" | "run-gui" | "run-quiet"),
                socket,
                lease,
                dir,
                argv @ ..,
            ] => {
                let session = if *verb == "run-gui" {
                    Session::Gui
                } else {
                    Session::Background
                };
                let dir = std::fs::File::open(dir).map_err(ClientError::Io)?;
                let null = std::fs::File::open("/dev/null").map_err(ClientError::Io)?;
                let argv: Vec<String> = argv.iter().map(|&a| a.to_owned()).collect();
                let stdout = std::io::stdout();
                let stderr = std::io::stderr();
                // A process the action leaves running holds no pipe of the caller's.
                let fds = if *verb == "run-quiet" {
                    [null.as_fd(), null.as_fd(), null.as_fd(), dir.as_fd()]
                } else {
                    [null.as_fd(), stdout.as_fd(), stderr.as_fd(), dir.as_fd()]
                };
                let exit = Client::new(socket)
                    .run_in(lease, &argv, &[], fds, session)?
                    .wait()?;
                match (exit.code, exit.signal) {
                    (Some(code), _) => println!("exit {code}"),
                    (_, Some(signal)) => println!("signal {signal}"),
                    _ => println!("unknown"),
                }
            }
            ["reset", socket, held @ ..] => {
                let held: Vec<String> = held.iter().map(|&a| a.to_owned()).collect();
                let (kept, deleted) = Client::new(socket).boot_reset(&held)?;
                let kept = kept.as_deref().unwrap_or("none");
                println!("kept {kept} deleted {}", deleted.join(","));
            }
            ["scan", socket] => {
                let scanned = Client::new(socket).scan()?;
                println!("{}", if scanned.clean { "clean" } else { "dirty" });
                for item in &scanned.added {
                    println!("+ {item}");
                }
                for item in &scanned.removed {
                    println!("- {item}");
                }
                for error in &scanned.errors {
                    println!("! {error}");
                }
            }
            ["baseline", socket] => {
                for (class, items) in Client::new(socket).baseline()? {
                    println!("{class} {items}");
                }
            }
            ["reboot-dirty", socket] => Client::new(socket).reboot_dirty()?,
            ["kill", socket, lease] => {
                let stream = match inherited() {
                    Some(stream) => stream,
                    None => UnixStream::connect(socket).map_err(ClientError::Io)?,
                };
                let request = Request::KillUid {
                    lease: (*lease).to_owned(),
                };
                match client::call_on(stream, &request, &[])?.1 {
                    Reply::Killed => {}
                    Reply::Refused { reason } => return Err(ClientError::Refused(reason)),
                    other => return Err(ClientError::Unexpected(other)),
                }
            }
            ["replies"] => {
                let stream = inherited().expect("KBF_CI_FD names the connection");
                let mut last = None;
                while let Some((reply, _)) =
                    proto::recv::<Reply>(stream.as_fd(), 0).map_err(ClientError::Io)?
                {
                    println!("{reply:?}");
                    last = Some(reply);
                }
                match last {
                    Some(Reply::Refused { reason }) => return Err(ClientError::Refused(reason)),
                    Some(Reply::Killed) => {}
                    other => return Err(ClientError::Unexpected(other.unwrap_or(Reply::Killed))),
                }
            }
            ["exec-probe"] => exec_probe().map_err(ClientError::Io)?,
            ["groups"] => {
                // The kernel's list (getgroups(2) without _DARWIN_UNLIMITED_GETGROUPS),
                // which setgroups changes; `id -G` asks the directory instead.
                let mut groups = [0 as libc::gid_t; 64];
                // SAFETY: `groups` has room for 64 ids, the count passed.
                let n = unsafe { libc::getgroups(64, groups.as_mut_ptr()) };
                let n = usize::try_from(n)
                    .map_err(|_| ClientError::Io(std::io::Error::last_os_error()))?;
                let named: Vec<String> = groups[..n].iter().map(ToString::to_string).collect();
                println!("{}", named.join(" "));
            }
            ["spawn", session, uid, gid, user, dir, argv @ ..] => {
                let session = match *session {
                    "gui" => Session::Gui,
                    _ => Session::Background,
                };
                let open = |path: &str| std::fs::File::open(path).map_err(ClientError::Io);
                let fd = |file: std::fs::File| -> std::os::fd::OwnedFd { file.into() };
                let dup = |fd: std::os::fd::BorrowedFd<'_>| {
                    fd.try_clone_to_owned().map_err(ClientError::Io)
                };
                let spawn = Spawn {
                    uid: uid.parse().expect("a uid"),
                    gid: gid.parse().expect("a gid"),
                    user: (*user).to_owned(),
                    home: std::path::PathBuf::from("/Users").join(user),
                    argv: argv.iter().map(|&a| a.to_owned()).collect(),
                    env: Vec::new(),
                    stdin: fd(open("/dev/null")?),
                    stdout: dup(std::io::stdout().as_fd())?,
                    stderr: dup(std::io::stderr().as_fd())?,
                    cwd: fd(open(dir)?),
                    session,
                };
                let status = kbf_mac_session::spawn::spawn(spawn)
                    .and_then(|mut child| child.wait())
                    .map_err(ClientError::Io)?;
                println!("exit {}", status.code().unwrap_or(-1));
            }
            ["probe-child"] => {
                // exec-probe's child: says it is there, waits for the go, becomes sleep.
                let stream = inherited().expect("KBF_CI_FD names the probe's socket");
                (&stream).write_all(b"x").map_err(ClientError::Io)?;
                let mut go = [0u8; 1];
                (&stream).read_exact(&mut go).map_err(ClientError::Io)?;
                let error = Command::new("/bin/sleep").arg("30").exec();
                return Err(ClientError::Io(error));
            }
            ["delete", socket, lease] => {
                let existed = Client::new(socket).user_delete(lease)?;
                println!("{}", if existed { "existed" } else { "absent" });
            }
            ["grant", seed, serial, lease, issued] => {
                // The `token` of the gate's `grant-admin` answer (kbf-mdm's
                // `GrantKey::sign`): valid for an hour from `issued`.
                let issued: i64 = issued.parse().expect("issued is unix seconds");
                let time = |secs: i64| {
                    let utc = format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
                    OffsetDateTime::from_unix_timestamp(secs)
                        .expect("a time")
                        .format(utc)
                        .expect("a UTC time")
                };
                let grant = format!(
                    "kbf-grant-v1\nserial {serial}\nlease {lease}\nissued {}\nnot-after {}\n",
                    time(issued),
                    time(issued + 3600)
                );
                let signature = key(seed).sign(grant.as_bytes());
                println!(
                    "{}.{}",
                    URL_SAFE_NO_PAD.encode(&grant),
                    URL_SAFE_NO_PAD.encode(signature.to_bytes())
                );
            }
            ["pubkey", seed] => {
                println!("{}", STANDARD.encode(key(seed).verifying_key().to_bytes()))
            }
            ["connect-then-exec", socket, program, args @ ..] => {
                let stream = UnixStream::connect(socket).map_err(ClientError::Io)?;
                // Wait for the helper's first frame without reading it, so that the
                // helper has seen this process, not the program it becomes.
                let mut poll = [libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                }];
                // SAFETY: one valid pollfd, which the call may write.
                if unsafe { libc::poll(poll.as_mut_ptr(), 1, 3000) } < 0 {
                    return Err(ClientError::Io(std::io::Error::last_os_error()));
                }
                exec_with(&stream, program, args)?;
            }
            ["write-then-exec", socket, lease, program, args @ ..] => {
                let stream = UnixStream::connect(socket).map_err(ClientError::Io)?;
                let call = proto::Call {
                    nonce: "0".repeat(32),
                    request: Request::KillUid {
                        lease: (*lease).to_owned(),
                    },
                };
                // A helper that refuses this caller at accept answers and closes at once,
                // so the write can fail with a broken pipe or a reset before the program
                // reads anything. That is the refusal arriving first: the answer is
                // still queued on this end, and the program reads it, so the refusal is
                // still checked by what it says. Any other failure is a failure.
                match proto::send(stream.as_fd(), &call, &[]) {
                    Err(why)
                        if !matches!(
                            why.kind(),
                            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                        ) =>
                    {
                        return Err(ClientError::Io(why));
                    }
                    _ => {}
                }
                exec_with(&stream, program, args)?;
            }
            _ => {
                eprintln!("usage: see the source of tests/as_root/client.rs");
                return Err(ClientError::Io(std::io::ErrorKind::InvalidInput.into()));
            }
        }
        Ok(())
    }

    /// The connection a parent left open as `KBF_CI_FD`, if any.
    fn inherited() -> Option<UnixStream> {
        let fd = std::env::var("KBF_CI_FD").ok()?;
        let fd = fd.parse().expect("KBF_CI_FD is a descriptor");
        // SAFETY: the descriptor was left open across exec for this, and nothing else
        // owns it.
        Some(unsafe { UnixStream::from_raw_fd(fd) })
    }

    /// Executes `program` with `stream` left open as `KBF_CI_FD`; returns only on
    /// failure.
    fn exec_with(stream: &UnixStream, program: &str, args: &[&str]) -> Result<(), ClientError> {
        let fd = stream.as_raw_fd();
        rustix::io::fcntl_setfd(stream.as_fd(), rustix::io::FdFlags::empty())
            .map_err(|e| ClientError::Io(e.into()))?;
        let error = Command::new(program)
            .args(args.iter())
            .env("KBF_CI_FD", fd.to_string())
            .exec();
        Err(ClientError::Io(error))
    }

    /// The pid and pid version `LOCAL_PEERTOKEN` reports for the other end of `socket`
    /// (words 5 and 7 of the audit token), or the error.
    fn peer(socket: &UnixStream) -> String {
        let mut token = [0u32; 8];
        let mut len = std::mem::size_of_val(&token) as libc::socklen_t;
        // SAFETY: `token` is a writable buffer of `len` bytes and `len` a writable
        // socklen_t; the call writes at most `len` bytes.
        let got = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERTOKEN,
                token.as_mut_ptr().cast(),
                &raw mut len,
            )
        };
        if got != 0 {
            return format!("no token ({})", std::io::Error::last_os_error());
        }
        format!("pid {} version {}", token[5], token[7])
    }

    /// See the module's documentation.
    fn exec_probe() -> std::io::Result<()> {
        let (mut mine, theirs) = UnixStream::pair()?;
        rustix::io::fcntl_setfd(theirs.as_fd(), rustix::io::FdFlags::empty())?;
        let mut child = Command::new(std::env::current_exe()?)
            .arg("probe-child")
            .env("KBF_CI_FD", theirs.as_raw_fd().to_string())
            .spawn()?;
        drop(theirs);
        let mut byte = [0u8; 1];
        mine.read_exact(&mut byte)?;
        let before = peer(&mine);
        mine.write_all(b"g")?;
        std::thread::sleep(std::time::Duration::from_secs(1));
        let after = peer(&mine);
        child.kill()?;
        child.wait()?;
        let exited = peer(&mine);
        println!(
            "exec-probe: child {}; before exec {before}; after exec {after}; after exit {exited}",
            child.id()
        );
        Ok(())
    }
}
