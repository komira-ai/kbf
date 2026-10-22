//! The real [`SafeToStop`]: a call to the frontend's loopback operator API,
//! `GET /v1/members/{id}/safe-to-stop` (`docs/api.md`), with the operator bearer token read
//! from a file.
//!
//! The host's alias is the id (a node's id; the API accepts a member's number as well). The
//! token is secret material: it is read from the file on every call (so a rotated token needs
//! no restart), held only in the request buffer, and never in an error text, a `Debug` or a
//! log. The file must be a regular file owned by the updater's user with mode `0600` or
//! `0400`, as the server demands of its own copy, and holds one token of 32 to 4096 visible
//! ASCII characters; a byte that could end a header line is therefore never sent.
//!
//! What the answer means:
//!
//! - 200: the [`Verdict`]; an answer that is no verdict is an error, never "safe".
//! - 404: [`Stop::Unknown`].
//! - 401, 403, 415: the API refused the call (a wrong token, or a server behind the proxy's
//!   secret); an error that says so, which the roll retries until its bound and then fails at.
//! - 409: the server cannot tell whether the node runs a storage member; an error.
//! - 5xx, a refused or timed-out connection: [`PortError::unreachable`].
//!
//! Error texts carry no address and no token: they reach `GET /rolls/{id}`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::inventory::Host;
use crate::ports::{PortError, SafeToStop, Stop, Verdict};

/// How long a connect, a write or a read may take.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// The shortest and longest token, as the server's `--api-token-file` takes it.
const TOKEN_MIN: usize = 32;
const TOKEN_MAX: u64 = 4096;

/// The most bytes of an answer that are read.
const REPLY_MAX: u64 = 1024 * 1024;

/// Read the token in the file at `path`: a regular file (not followed if it is a symlink,
/// not opened if it is a FIFO) owned by this process's user with mode exactly `0600` or
/// `0400`, one token of visible ASCII, 32 bytes or more, surrounding whitespace dropped. The
/// error text names the path and the rule, never the content.
pub fn read_token(path: &Path) -> Result<String, String> {
    let fail = |why: &str| format!("token file {}: {why}", path.display());
    let io = |e: std::io::Error| fail(&e.kind().to_string());
    let (file, meta) = open_checked(path).map_err(io)?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    file_rule(meta.is_file(), meta.uid(), meta.mode(), me).map_err(fail)?;
    let mut text = String::new();
    file.take(TOKEN_MAX + 1)
        .read_to_string(&mut text)
        .map_err(io)?;
    let token = text.trim();
    let visible = token.bytes().all(|b| b.is_ascii_graphic());
    if text.len() as u64 > TOKEN_MAX || token.len() < TOKEN_MIN || !visible {
        return Err(fail(
            "it must hold one token of 32 to 4096 visible ASCII characters",
        ));
    }
    Ok(token.to_owned())
}

/// Open without following a symlink and without blocking on a FIFO; the metadata is of the
/// open file, so what is checked is what is read.
fn open_checked(path: &Path) -> std::io::Result<(std::fs::File, std::fs::Metadata)> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    Ok((file, meta))
}

/// The rule for the file itself: regular, owned by `me`, mode exactly `0600` or `0400`.
pub(crate) fn file_rule(is_file: bool, uid: u32, mode: u32, me: u32) -> Result<(), &'static str> {
    if !is_file {
        return Err("it is not a regular file");
    }
    if uid != me {
        return Err("it is not owned by the updater's user");
    }
    if !matches!(mode & 0o7777, 0o600 | 0o400) {
        return Err("its mode must be 0600 or 0400");
    }
    Ok(())
}

/// The safe-to-stop port over HTTP. It has no `Debug`: nothing about it should be printed.
pub struct HttpSafeToStop {
    addr: SocketAddr,
    token_file: PathBuf,
    timeout: Duration,
}

impl HttpSafeToStop {
    /// An adapter for the API at `addr`. The token file is read once now, so that a start-up
    /// with a bad file fails at once and not at the first roll.
    pub fn new(addr: SocketAddr, token_file: PathBuf) -> Result<HttpSafeToStop, String> {
        read_token(&token_file)?;
        Ok(HttpSafeToStop {
            addr,
            token_file,
            timeout: TIMEOUT,
        })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> HttpSafeToStop {
        self.timeout = timeout;
        self
    }

    /// One request, one answer: the raw bytes, up to [`REPLY_MAX`].
    fn exchange(&self, alias: &str, token: &str) -> std::io::Result<Vec<u8>> {
        let mut stream = TcpStream::connect_timeout(&self.addr, self.timeout)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        let request = format!(
            "GET /v1/members/{alias}/safe-to-stop HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
             Accept: application/json\r\nAuthorization: Bearer {token}\r\n\
             Content-Type: application/json\r\nContent-Length: 0\r\n\r\n",
            self.addr
        );
        stream.write_all(request.as_bytes())?;
        let mut raw = Vec::new();
        stream.take(REPLY_MAX).read_to_end(&mut raw)?;
        Ok(raw)
    }
}

impl SafeToStop for HttpSafeToStop {
    fn check(&self, host: &Host) -> Result<Stop, PortError> {
        let token = read_token(&self.token_file).map_err(PortError::other)?;
        let raw = self
            .exchange(&host.alias, &token)
            .map_err(|e| PortError::unreachable(format!("the operator API: {}", e.kind())))?;
        interpret(&raw)
    }
}

/// The meaning of an answer's bytes.
fn interpret(raw: &[u8]) -> Result<Stop, PortError> {
    let mut slots = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut slots);
    let Ok(httparse::Status::Complete(head)) = response.parse(raw) else {
        return Err(PortError::other(
            "the operator API did not answer with HTTP/1.1",
        ));
    };
    let status = response.code.unwrap_or_default();
    let mut length = None;
    for h in response.headers.iter() {
        if h.name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(PortError::other(
                "the operator API sent a chunked answer, which it does not do",
            ));
        }
        if h.name.eq_ignore_ascii_case("content-length") {
            length = std::str::from_utf8(h.value)
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok());
        }
    }
    let mut body = &raw[head..];
    if let Some(length) = length {
        if body.len() < length {
            return Err(PortError::unreachable(
                "the operator API closed the answer short",
            ));
        }
        body = &body[..length];
    }
    match status {
        200 => serde_json::from_slice::<Verdict>(body)
            .map(Stop::Answer)
            .map_err(|_| {
                PortError::other("the operator API's answer is not a safe-to-stop verdict")
            }),
        404 => Ok(Stop::Unknown),
        401 | 403 | 415 => Err(PortError::other(format!(
            "the operator API refused the call ({status}); check the token file and the \
             API's gates"
        ))),
        409 => Err(PortError::other(
            "the operator API cannot tell whether the host runs a storage member (409)",
        )),
        500..=599 => Err(PortError::unreachable(format!(
            "the operator API answered {status}"
        ))),
        _ => Err(PortError::other(format!(
            "the operator API answered {status}"
        ))),
    }
}
