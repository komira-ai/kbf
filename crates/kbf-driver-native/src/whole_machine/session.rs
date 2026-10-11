//! What the whole-machine runtime asks of `kbf-mac-session`: [`Session`], and
//! [`HelperSession`], which asks the helper over its socket.

use std::os::fd::{AsFd as _, OwnedFd};
use std::path::Path;

use kbf_mac_session::client::{Client, ClientError, Running};
use kbf_mac_session::proto::Session as Where;
use kbf_types::LeaseId;

/// How a process the helper started ended: its exit code, or the signal that ended it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl Exit {
    /// The exit code an action reports: its own, or 128 plus the signal that ended it
    /// (as a shell reports one), or 255 when neither is known.
    #[must_use]
    pub fn action_code(self) -> i32 {
        match (self.code, self.signal) {
            (Some(code), _) => code,
            (None, Some(signal)) => 128 + signal,
            (None, None) => 255,
        }
    }
}

/// The root helper's verbs the runtime uses (`docs/design/fleet-updates-security.md`
/// S4.2). Each call blocks; the runtime makes them off its async threads. An error is
/// the helper's reason, or why it could not be asked.
pub trait Session: Send + Sync + 'static {
    /// A process the helper started, until it is waited for.
    type Running: Send + 'static;

    /// `boot-reset`, naming the lease the runtime still holds (if any): returns the
    /// leases whose users it deleted. A reset that ran before since the helper started
    /// (another start of the daemon) is no error: it deleted nothing now.
    ///
    /// # Errors
    /// The reset failed.
    fn boot_reset(&self, held: Option<LeaseId>) -> Result<Vec<LeaseId>, String>;
    /// `user-create`: the lease's fresh, non-admin user; its uid.
    ///
    /// # Errors
    /// The user could not be made.
    fn user_create(&self, lease: LeaseId) -> Result<u32, String>;
    /// Lets the lease user work in `dir`, which the daemon made and owns, and keeps the
    /// daemon able to read and remove what the user writes there (on macOS an
    /// inherited access control entry for each).
    ///
    /// # Errors
    /// The directory's access could not be set.
    fn share(&self, lease: LeaseId, dir: &Path) -> Result<(), String>;
    /// `session-login`: auto-login into the lease user, then a userspace restart, which
    /// may end this process before it returns.
    ///
    /// # Errors
    /// The switch was refused.
    fn session_login(&self, lease: LeaseId) -> Result<(), String>;
    /// `session-ready`: whether the lease user's GUI session is up.
    ///
    /// # Errors
    /// The helper could not tell.
    fn session_ready(&self, lease: LeaseId) -> Result<bool, String>;
    /// `run` in the lease user's GUI session, with stdin, stdout, stderr and the
    /// working directory.
    ///
    /// # Errors
    /// The process did not start.
    fn run(
        &self,
        lease: LeaseId,
        argv: &[String],
        env: &[(String, String)],
        fds: [OwnedFd; 4],
    ) -> Result<Self::Running, String>;
    /// Waits for a process [`Self::run`] started.
    ///
    /// # Errors
    /// It could not be waited for.
    fn wait(&self, running: Self::Running) -> Result<Exit, String>;
    /// `kill-uid`: no process of the lease user is left when it returns.
    ///
    /// # Errors
    /// Processes survived, or the kill was refused.
    fn kill_uid(&self, lease: LeaseId) -> Result<(), String>;
    /// `session-idle`: auto-login cleared, then the same restart.
    ///
    /// # Errors
    /// The switch back was refused.
    fn session_idle(&self) -> Result<(), String>;
    /// `user-delete`: the lease user gone and its state swept.
    ///
    /// # Errors
    /// The user could not be deleted.
    fn user_delete(&self, lease: LeaseId) -> Result<(), String>;
}

/// The helper over its socket.
#[derive(Clone, Debug)]
pub struct HelperSession {
    client: Client,
    /// The daemon's own user, which keeps access to what the lease user writes.
    daemon_user: String,
}

impl HelperSession {
    /// The helper listening on `socket`, for a daemon running as `daemon_user`.
    #[must_use]
    pub fn new(socket: &Path, daemon_user: impl Into<String>) -> Self {
        Self {
            client: Client::new(socket),
            daemon_user: daemon_user.into(),
        }
    }
}

/// The helper's reason, or why it could not be asked.
fn said(error: ClientError) -> String {
    error.to_string()
}

impl Session for HelperSession {
    type Running = Running;

    fn boot_reset(&self, held: Option<LeaseId>) -> Result<Vec<LeaseId>, String> {
        let held: Vec<String> = held.iter().map(ToString::to_string).collect();
        match self.client.boot_reset(&held) {
            Ok((_, deleted)) => Ok(deleted.iter().filter_map(|l| parse(l)).collect()),
            Err(ClientError::Refused(why)) if why == kbf_mac_session::helper::RESET_ALREADY => {
                Ok(Vec::new())
            }
            Err(error) => Err(said(error)),
        }
    }

    fn user_create(&self, lease: LeaseId) -> Result<u32, String> {
        self.client
            .user_create(&lease.to_string(), None)
            .map_err(said)
    }

    fn share(&self, lease: LeaseId, dir: &Path) -> Result<(), String> {
        share_with(
            &kbf_mac_session::lease::user_name(lease),
            &self.daemon_user,
            dir,
        )
    }

    fn session_login(&self, lease: LeaseId) -> Result<(), String> {
        restarting(self.client.session_login(&lease.to_string()))
    }

    fn session_ready(&self, lease: LeaseId) -> Result<bool, String> {
        self.client.session_ready(&lease.to_string()).map_err(said)
    }

    fn run(
        &self,
        lease: LeaseId,
        argv: &[String],
        env: &[(String, String)],
        fds: [OwnedFd; 4],
    ) -> Result<Running, String> {
        let [stdin, stdout, stderr, cwd] = &fds;
        let borrowed = [stdin.as_fd(), stdout.as_fd(), stderr.as_fd(), cwd.as_fd()];
        self.client
            .run_in(&lease.to_string(), argv, env, borrowed, Where::Gui)
            .map_err(said)
    }

    fn wait(&self, running: Running) -> Result<Exit, String> {
        let exit = running.wait().map_err(said)?;
        Ok(Exit {
            code: exit.code,
            signal: exit.signal,
        })
    }

    fn kill_uid(&self, lease: LeaseId) -> Result<(), String> {
        self.client.kill_uid(&lease.to_string()).map_err(said)
    }

    fn session_idle(&self) -> Result<(), String> {
        restarting(self.client.session_idle())
    }

    fn user_delete(&self, lease: LeaseId) -> Result<(), String> {
        self.client
            .user_delete(&lease.to_string())
            .map(drop)
            .map_err(said)
    }
}

/// A session switch's answer: the restart it asks for may end the connection before
/// the answer comes, so an end of stream is success; any other failure is not.
fn restarting(answer: Result<(), ClientError>) -> Result<(), String> {
    match answer {
        Err(ClientError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(()),
        other => other.map_err(said),
    }
}

/// `<term>.<seq>`, as the helper names leases.
fn parse(text: &str) -> Option<LeaseId> {
    let (term, seq) = text.split_once('.')?;
    Some(LeaseId::new(term.parse().ok()?, seq.parse().ok()?))
}

/// The access control entries a lease directory gets on macOS: the lease user may
/// work in it, and the daemon's user may read and remove what the lease user makes;
/// both are inherited by everything made in it from then on.
#[cfg(target_os = "macos")]
fn share_with(user: &str, daemon: &str, dir: &Path) -> Result<(), String> {
    const INHERIT: &str = "file_inherit,directory_inherit";
    let entries = [
        format!(
            "user:{user} allow list,add_file,search,add_subdirectory,delete_child,readattr,\
             writeattr,readextattr,writeextattr,readsecurity,read,write,append,execute,\
             {INHERIT}"
        ),
        format!(
            "user:{daemon} allow list,search,delete,delete_child,readattr,readextattr,\
             readsecurity,read,{INHERIT}"
        ),
    ];
    for entry in entries {
        let output = std::process::Command::new("/bin/chmod")
            .arg("+a")
            .arg(&entry)
            .arg(dir)
            .env_clear()
            .output()
            .map_err(|why| format!("chmod +a: {why}"))?;
        if !output.status.success() {
            return Err(format!(
                "chmod +a {entry:?} {}: {}",
                dir.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    Ok(())
}

/// Lease users exist only on macOS.
#[cfg(not(target_os = "macos"))]
fn share_with(_: &str, _: &str, _: &Path) -> Result<(), String> {
    Err("lease users exist only on macOS".to_owned())
}

#[cfg(test)]
mod tests {
    use kbf_mac_session::proto::{self, Call, Reply, Request};

    use super::*;

    /// Catches an action's exit taken from the wrong field, or a killed action
    /// reported as a success.
    #[test]
    fn an_exit_is_the_code_or_128_plus_the_signal() {
        let exit = |code, signal| Exit { code, signal }.action_code();
        assert_eq!(exit(Some(3), None), 3);
        assert_eq!(exit(None, Some(9)), 137);
        assert_eq!(exit(None, None), 255);
    }

    /// Catches a deleted lease the boot reset names that is read as another, or not
    /// read at all (the runtime would then try to clean up a user that is gone).
    #[test]
    fn the_helper_s_lease_names_parse() {
        assert_eq!(parse("7.3"), Some(LeaseId::new(7, 3)));
        assert_eq!(parse("7"), None);
        assert_eq!(parse("x.3"), None);
        assert_eq!(parse("7.y"), None);
    }

    /// A helper that answers each connection, in turn, with `replies` (a `Started`
    /// is followed on its connection by the next reply, as `run` expects), then hangs
    /// up on one more connection before the answer. Returns the requests it got.
    fn scripted(socket: &Path, replies: Vec<Reply>) -> std::thread::JoinHandle<Vec<Request>> {
        let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            let mut asked = Vec::new();
            let mut replies = replies.into_iter();
            while let Some(reply) = replies.next() {
                let (stream, _) = listener.accept().unwrap();
                let hello = Reply::Hello {
                    nonce: "n".to_owned(),
                };
                proto::send(stream.as_fd(), &hello, &[]).unwrap();
                let (call, _) = proto::recv::<Call>(stream.as_fd(), proto::RUN_FDS)
                    .unwrap()
                    .unwrap();
                asked.push(call.request);
                let started = matches!(reply, Reply::Started { .. });
                proto::send(stream.as_fd(), &reply, &[]).unwrap();
                if started {
                    proto::send(stream.as_fd(), &replies.next().unwrap(), &[]).unwrap();
                }
            }
            // One more caller, hung up on after its request: a restart mid-call.
            let (stream, _) = listener.accept().unwrap();
            let hello = Reply::Hello {
                nonce: "n".to_owned(),
            };
            proto::send(stream.as_fd(), &hello, &[]).unwrap();
            let _ = proto::recv::<Call>(stream.as_fd(), proto::RUN_FDS);
            asked
        })
    }

    /// Catches a verb sent to the helper as another, or with another lease; a GUI run
    /// sent as a background one (the action would start outside the session); a second
    /// boot reset (a daemon restarted while the helper ran on) taken as a failure; a
    /// session switch whose restart ends the connection taken as refused; and a
    /// refusal or a reply the verb does not expect passed off as success.
    #[test]
    fn each_verb_reaches_the_helper_as_itself() {
        let dir = std::env::temp_dir().join(format!("kbf-wm-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("socket");
        let refused = |reason: &str| Reply::Refused {
            reason: reason.to_owned(),
        };
        let helper = scripted(
            &socket,
            vec![
                Reply::Reset {
                    kept: Some("7.3".to_owned()),
                    deleted: vec!["7.1".to_owned(), "bad".to_owned()],
                },
                refused(kbf_mac_session::helper::RESET_ALREADY),
                refused("the ledger is gone"),
                Reply::Created { uid: 650 },
                Reply::Restarting,
                Reply::SessionReady { ready: true },
                Reply::Started { pid: 9 },
                Reply::Exited {
                    code: None,
                    signal: Some(9),
                },
                Reply::Killed,
                Reply::Restarting,
                Reply::Deleted { existed: true },
                refused("no"),
                refused("no"),
                Reply::Killed,
            ],
        );
        let lease = LeaseId::new(7, 3);
        let session = HelperSession::new(&socket, "_kbf");
        assert_eq!(
            session.boot_reset(Some(lease)),
            Ok(vec![LeaseId::new(7, 1)])
        );
        assert_eq!(session.boot_reset(None), Ok(Vec::new()));
        assert!(
            session
                .boot_reset(None)
                .unwrap_err()
                .contains("ledger is gone")
        );
        assert_eq!(session.user_create(lease), Ok(650));
        assert_eq!(session.session_login(lease), Ok(()));
        assert_eq!(session.session_ready(lease), Ok(true));
        let null = || -> OwnedFd { std::fs::File::open("/dev/null").unwrap().into() };
        let argv = ["/usr/bin/true".to_owned()];
        let running = session
            .run(lease, &argv, &[], [null(), null(), null(), null()])
            .unwrap();
        assert_eq!(
            session.wait(running),
            Ok(Exit {
                code: None,
                signal: Some(9)
            })
        );
        assert_eq!(session.kill_uid(lease), Ok(()));
        assert_eq!(session.session_idle(), Ok(()));
        assert_eq!(session.user_delete(lease), Ok(()));
        assert!(
            session
                .session_login(lease)
                .unwrap_err()
                .contains("refused: no")
        );
        assert!(session.session_idle().unwrap_err().contains("refused: no"));
        assert!(session.user_create(lease).unwrap_err().contains("Killed"));
        // The restart ends the connection before the answer.
        assert_eq!(session.session_idle(), Ok(()));
        let asked = helper.join().unwrap();
        // No helper at all is no restart.
        let error = session.session_login(lease).unwrap_err();
        assert!(error.contains("kbf-mac-session: "), "{error}");
        let lease = || "7.3".to_owned();
        assert_eq!(
            asked[0],
            Request::BootReset {
                held: vec![lease()]
            }
        );
        assert_eq!(asked[1], Request::BootReset { held: Vec::new() });
        assert_eq!(
            asked[3],
            Request::UserCreate {
                lease: lease(),
                grant: None
            }
        );
        assert_eq!(asked[4], Request::SessionLogin { lease: lease() });
        assert_eq!(asked[5], Request::SessionReady { lease: lease() });
        assert_eq!(
            asked[6],
            Request::Run {
                lease: lease(),
                argv: argv.to_vec(),
                env: Vec::new(),
                session: Where::Gui,
            }
        );
        assert_eq!(asked[7], Request::KillUid { lease: lease() });
        assert_eq!(asked[8], Request::SessionIdle);
        assert_eq!(asked[9], Request::UserDelete { lease: lease() });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Catches a lease directory shared off macOS as if it had worked: there is no
    /// lease user to share it with.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn sharing_is_refused_off_macos() {
        let session = HelperSession::new(Path::new("/nonexistent/socket"), "_kbf");
        let error = session
            .share(LeaseId::new(1, 1), Path::new("/tmp"))
            .unwrap_err();
        assert!(error.contains("only on macOS"), "{error}");
    }
}
