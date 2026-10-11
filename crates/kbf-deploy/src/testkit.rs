//! Test helpers: [`FakeExec`], an in-memory host that records every step and query, and
//! scratch directories.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{ExitStatus, Output};

use crate::exec::{Entry, Exec, HostExec, Kind, Meta};
use crate::path::{HostPath, Root};
use crate::step::{Cmd, HttpUrl, Owner, Perms, Step};
use crate::trust::Untrusted;

/// How the fake answers a command: a query, a [`Step::Run`] or a [`Step::Capture`].
pub type Answer = Box<dyn Fn(&Cmd) -> io::Result<Output>>;

/// A host made of a map from path to entry. Every step `apply` is given is pushed to
/// `log` first, and every query to `queries`; file, directory, removal and capture
/// steps then change the map, so a plan made after an apply sees the result. Commands
/// get their output from `answer` (an error there is a tool that cannot start); a
/// [`Step::Run`] or [`Step::Capture`] whose answer is a non-zero exit fails.
pub struct FakeExec {
    pub euid: u32,
    pub entries: BTreeMap<HostPath, Entry>,
    pub log: Vec<Step>,
    pub queries: RefCell<Vec<Cmd>>,
    pub answer: Answer,
    /// What every `http_status` answers.
    pub http: Result<u16, io::ErrorKind>,
    /// Fail the step at this index of `log` (counting every step applied).
    pub fail_at: Option<usize>,
    /// Make `check_binary` refuse.
    pub untrusted: bool,
}

/// The owner a step with `owner: None` leaves: the fake's user.
pub const RUNNER: Owner = Owner { uid: 501, gid: 20 };

/// A command's output: exit `code`, with `stdout` and `stderr`.
pub fn reply(code: i32, stdout: &str, stderr: &str) -> Output {
    Output {
        status: ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

impl FakeExec {
    /// Every command exits 0 with no output, and every URL answers 200.
    pub fn new(euid: u32) -> FakeExec {
        FakeExec {
            euid,
            entries: BTreeMap::new(),
            log: Vec::new(),
            queries: RefCell::new(Vec::new()),
            answer: Box::new(|_| Ok(reply(0, "", ""))),
            http: Ok(200),
            fail_at: None,
            untrusted: false,
        }
    }
}

impl Exec for FakeExec {
    fn euid(&self) -> u32 {
        self.euid
    }

    fn inspect(&self, path: &HostPath) -> io::Result<Option<Entry>> {
        if path.as_str() == "/unreadable" {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        Ok(self.entries.get(path).cloned())
    }

    /// The entry's kind, permissions and owner; a file's length is its bytes'.
    fn stat(&self, path: &HostPath) -> io::Result<Option<Meta>> {
        Ok(self.inspect(path)?.map(|entry| match entry {
            Entry::File {
                perms,
                owner,
                bytes,
            } => Meta {
                kind: Kind::File,
                perms,
                owner,
                len: bytes.len() as u64,
            },
            Entry::Dir { perms, owner } => Meta {
                kind: Kind::Dir,
                perms,
                owner,
                len: 0,
            },
            Entry::Other => Meta {
                kind: Kind::Other,
                perms: Perms::new(0o777),
                owner: RUNNER,
                len: 0,
            },
        }))
    }

    fn query(&self, cmd: &Cmd) -> io::Result<Output> {
        self.queries.borrow_mut().push(cmd.clone());
        (self.answer)(cmd)
    }

    fn http_status(&self, _url: &HttpUrl) -> io::Result<u16> {
        self.http.map_err(io::Error::from)
    }

    fn apply(&mut self, step: &Step) -> io::Result<()> {
        self.log.push(step.clone());
        if self.fail_at == Some(self.log.len() - 1) {
            return Err(io::Error::other("planted failure"));
        }
        match step {
            Step::WriteFile {
                path,
                perms,
                owner,
                bytes,
            } => {
                let entry = Entry::File {
                    perms: *perms,
                    owner: owner.unwrap_or(RUNNER),
                    bytes: bytes.clone(),
                };
                self.entries.insert(path.clone(), entry);
            }
            Step::Mkdir { path, perms, owner } => {
                let entry = Entry::Dir {
                    perms: *perms,
                    owner: owner.unwrap_or(RUNNER),
                };
                self.entries.insert(path.clone(), entry);
            }
            Step::Remove { path } => {
                self.entries.remove(path);
            }
            Step::Run(cmd) => {
                crate::exec::succeeded(cmd, (self.answer)(cmd)?)?;
            }
            Step::Capture {
                cmd,
                filter,
                path,
                perms,
                owner,
            } => {
                let out = crate::exec::succeeded(cmd, (self.answer)(cmd)?)?;
                let entry = Entry::File {
                    perms: *perms,
                    owner: owner.unwrap_or(RUNNER),
                    bytes: (filter.apply)(&out.stdout).map_err(io::Error::other)?,
                };
                self.entries.insert(path.clone(), entry);
            }
            Step::WaitHttp { .. } => {}
        }
        Ok(())
    }

    fn check_binary(&self) -> Result<(), Untrusted> {
        if self.untrusted {
            return Err(Untrusted::Writable {
                path: PathBuf::from("/opt/downloads/kbf-deploy"),
                mode: 0o775,
            });
        }
        Ok(())
    }
}

/// A fresh, empty directory for one test: `<target>/tmp/kbf-deploy/<name>`.
pub fn scratch(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary has a path");
    let target = exe
        .ancestors()
        .nth(3)
        .expect("<target>/<profile>/deps/<test binary>");
    let dir = target.join("tmp").join("kbf-deploy").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("make the scratch directory");
    dir
}

/// A real file system under a scratch root, with commands played by the test: `inspect`,
/// `stat` and the file, directory and removal steps go to a [`HostExec`] on `root`;
/// every query and command step is recorded and answered by `answer`, which may change
/// files under the root as the tool would. It reports `euid` and trusts the running
/// binary, so a root plan applies as the test's own user. A capture's output is not
/// written (no plan here has one).
pub struct ScratchHost {
    pub host: HostExec,
    pub root: PathBuf,
    pub euid: u32,
    pub log: Vec<Step>,
    pub queries: RefCell<Vec<Cmd>>,
    pub answer: Answer,
}

impl ScratchHost {
    /// A fresh scratch root named `name`; every command exits 0 with no output.
    pub fn new(name: &str) -> ScratchHost {
        let root = scratch(name);
        ScratchHost {
            host: HostExec::new(Root::new(&root).expect("absolute")),
            root,
            euid: 0,
            log: Vec::new(),
            queries: RefCell::new(Vec::new()),
            answer: Box::new(|_| Ok(reply(0, "", ""))),
        }
    }

    /// The path `host` names under the root.
    pub fn at(&self, host: &str) -> PathBuf {
        self.root.join(host.trim_start_matches('/'))
    }
}

/// The scratch root goes with the host, after a failed test too: it may hold planted
/// executables (a fake `kbf-daemon`), and the coverage run loads every executable it
/// finds under the target directory as an object file.
impl Drop for ScratchHost {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Exec for ScratchHost {
    fn euid(&self) -> u32 {
        self.euid
    }

    fn inspect(&self, path: &HostPath) -> io::Result<Option<Entry>> {
        self.host.inspect(path)
    }

    fn stat(&self, path: &HostPath) -> io::Result<Option<Meta>> {
        self.host.stat(path)
    }

    fn query(&self, cmd: &Cmd) -> io::Result<Output> {
        self.queries.borrow_mut().push(cmd.clone());
        (self.answer)(cmd)
    }

    fn http_status(&self, _url: &HttpUrl) -> io::Result<u16> {
        Ok(200)
    }

    fn apply(&mut self, step: &Step) -> io::Result<()> {
        self.log.push(step.clone());
        match step {
            Step::Run(cmd) | Step::Capture { cmd, .. } => {
                crate::exec::succeeded(cmd, (self.answer)(cmd)?).map(drop)
            }
            Step::WaitHttp { .. } => Ok(()),
            _ => self.host.apply(step),
        }
    }

    fn check_binary(&self) -> Result<(), Untrusted> {
        Ok(())
    }
}

/// An HTTPS server on a loopback port, with a certificate for its names under a fresh
/// CA; each connection gets one answer.
pub struct TlsServer {
    pub addr: std::net::SocketAddr,
    /// The CA's certificate, for a client's roots.
    pub ca: rustls::pki_types::CertificateDer<'static>,
    /// The request paths served, in order.
    pub paths: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl TlsServer {
    /// A client trusting only this server's CA.
    pub fn client(&self) -> crate::https::Client {
        let roots = crate::https::roots(vec![self.ca.clone()]).expect("the CA parses");
        crate::https::Client::with_roots(roots, std::time::Duration::from_secs(5))
    }

    /// `https://<name>:<port><path>`.
    pub fn url(&self, name: &str, path: &str) -> crate::https::HttpsUrl {
        let url = format!("https://{name}:{}{path}", self.addr.port());
        crate::https::HttpsUrl::new(&url).expect("a test URL")
    }
}

/// Serve on `127.0.0.1:0` as `names` (DNS names or IP addresses), answering each
/// request with the raw bytes `answer` makes of its path, then a TLS close.
pub fn tls_server(names: &[&str], answer: impl Fn(&str) -> Vec<u8> + Send + 'static) -> TlsServer {
    use std::sync::{Arc, Mutex};

    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = rcgen::CertifiedIssuer::self_signed(ca, rcgen::KeyPair::generate().expect("key"))
        .expect("CA");
    let key = rcgen::KeyPair::generate().expect("key");
    let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
    let cert = rcgen::CertificateParams::new(names)
        .expect("server params")
        .signed_by(&key, &ca)
        .expect("server certificate");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).expect("key der"),
        )
        .expect("server config");
    let config = Arc::new(config);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let paths = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&paths);
    std::thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            let _ = serve_one(&config, tcp, &answer, &seen);
        }
    });
    TlsServer {
        addr,
        ca: ca.der().clone(),
        paths,
    }
}

fn serve_one(
    config: &std::sync::Arc<rustls::ServerConfig>,
    tcp: std::net::TcpStream,
    answer: &dyn Fn(&str) -> Vec<u8>,
    seen: &std::sync::Mutex<Vec<String>>,
) -> io::Result<()> {
    use std::io::{Read, Write};

    let conn =
        rustls::ServerConnection::new(std::sync::Arc::clone(config)).map_err(io::Error::other)?;
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    let mut head = Vec::new();
    let mut byte = [0u8];
    while !head.ends_with(b"\r\n\r\n") {
        tls.read_exact(&mut byte)?;
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let path = text.split(' ').nth(1).unwrap_or_default().to_owned();
    seen.lock().expect("lock").push(path.clone());
    tls.write_all(&answer(&path))?;
    tls.conn.send_close_notify();
    tls.flush()
}

/// An HTTP/1.1 answer with `status` (`200 OK`) and `body`, by `Content-Length`.
pub fn http(status: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}
