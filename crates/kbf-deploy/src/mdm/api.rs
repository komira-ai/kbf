//! The MDM services' HTTP APIs on loopback: NanoHUB's (the push certificate) and
//! NanoDEP's depserver (the Apple Business Manager token and the ADE profile).
//!
//! A call is printed as one line, `DRY-RUN: <call>` or `+ <call>`, showing its method,
//! URL and the size of its body, never the body (a private key or a token travels in
//! it). It is made only with `--apply`, and only from a binary root owns alone (the
//! check [`kbf_host::execute`] makes before a root plan). Each call carries HTTP Basic
//! auth with the service's API key, read from its secret file, in a header: never on
//! an argv, never printed.

use std::fmt;
use std::io::{BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::MdmError;
use super::config::MdmConfig;
use super::secret::{self, Secret};
use crate::https;
use crate::linux::host::{note, under};
use kbf_host::exec::Exec;
use kbf_host::plan::Mode;

/// How long a connect, a read or a write may take: depserver's `/proxy/` calls wait on
/// Apple.
pub const TIMEOUT: Duration = Duration::from_secs(60);
/// The most of a refusal's body an error quotes.
const QUOTED: usize = 300;

/// One service's API on `127.0.0.1`.
pub struct Api {
    /// What the service is called in messages: `NanoHUB`, `NanoDEP`.
    pub name: &'static str,
    pub port: u16,
    /// The Basic auth user, and the key from the service's secret file.
    pub user: &'static str,
    pub key: Secret,
}

impl Api {
    /// NanoHUB's API, with `NANOHUB_API_KEY` from `MDM_ETC/nanohub.env`.
    pub fn nanohub<E: Exec>(exec: &E, c: &MdmConfig) -> Result<Api, MdmError> {
        let key = secret::read(exec, &under(&c.etc, "nanohub.env")?, "NANOHUB_API_KEY")?;
        Ok(Api {
            name: "NanoHUB",
            port: c.nanohub_port,
            user: "nanohub",
            key,
        })
    }

    /// depserver's API, with `NANODEP_API` from `MDM_ETC/nanodep.env`.
    pub fn nanodep<E: Exec>(exec: &E, c: &MdmConfig) -> Result<Api, MdmError> {
        let key = secret::read(exec, &under(&c.etc, "nanodep.env")?, "NANODEP_API")?;
        Ok(Api {
            name: "NanoDEP",
            port: c.dep_port,
            user: "depserver",
            key,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

/// An HTTP method the kit calls with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Put,
    Post,
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Method::Get => "GET",
            Method::Put => "PUT",
            Method::Post => "POST",
        })
    }
}

/// One request: its method, path and query, and body (JSON when `json`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub method: Method,
    pub path: String,
    pub body: Vec<u8>,
    pub json: bool,
}

impl Call {
    pub fn get(path: impl Into<String>) -> Call {
        Call {
            method: Method::Get,
            path: path.into(),
            body: Vec::new(),
            json: false,
        }
    }

    pub fn put(path: impl Into<String>, body: Vec<u8>) -> Call {
        Call {
            method: Method::Put,
            ..Call::get(path)
        }
        .with_body(body)
    }

    pub fn post_json(path: impl Into<String>, body: Vec<u8>) -> Call {
        Call {
            method: Method::Post,
            json: true,
            ..Call::get(path)
        }
        .with_body(body)
    }

    fn with_body(mut self, body: Vec<u8>) -> Call {
        self.body = body;
        self
    }
}

/// `<method> <url> (<service> API[, <n> bytes])`.
struct Shown<'a>(&'a Api, &'a Call);

impl fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Shown(api, call) = self;
        write!(
            f,
            "{} {} ({} API",
            call.method,
            api.url(&call.path),
            api.name
        )?;
        if !call.body.is_empty() {
            write!(f, ", {} bytes", call.body.len())?;
        }
        f.write_str(")")
    }
}

/// Print `call`; with [`Mode::Apply`], make it and return the body of its 2xx answer.
/// Any other answer fails with its status and the start of its body.
pub fn call<E: Exec>(
    exec: &E,
    api: &Api,
    call: &Call,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<Option<Vec<u8>>, MdmError> {
    let shown = Shown(api, call).to_string();
    if mode == Mode::DryRun {
        note(out, format_args!("DRY-RUN: {shown}"))?;
        return Ok(None);
    }
    // The verbs refuse before planning unless the effective uid is 0; this is the
    // other half of what `execute` checks before a root plan.
    exec.check_binary().map_err(kbf_host::HostError::from)?;
    note(out, format_args!("+ {shown}"))?;
    let (status, body) = send(api, call).map_err(|e| MdmError::Api {
        call: shown.clone(),
        why: e.to_string(),
    })?;
    if !(200..300).contains(&status) {
        let said = String::from_utf8_lossy(&body);
        let said: String = said.trim().chars().take(QUOTED).collect();
        return Err(MdmError::Api {
            call: shown,
            why: format!("answered {status}: {said}"),
        });
    }
    Ok(Some(body))
}

fn send(api: &Api, call: &Call) -> Result<(u16, Vec<u8>), https::HttpsError> {
    let url = api.url(&call.path);
    let io = |source| https::HttpsError::Io {
        url: url.clone(),
        source,
    };
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, api.port));
    let mut tcp = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(io)?;
    tcp.set_read_timeout(Some(TIMEOUT)).map_err(io)?;
    tcp.set_write_timeout(Some(TIMEOUT)).map_err(io)?;
    let auth = STANDARD.encode(format!("{}:{}", api.user, api.key.expose()));
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUser-Agent: kbf-deploy\r\nAuthorization: Basic {auth}\r\nContent-Length: {}\r\nConnection: close\r\n",
        call.method,
        call.path,
        api.port,
        call.body.len()
    );
    if call.json {
        head.push_str("Content-Type: application/json;charset=UTF8\r\n");
    }
    head.push_str("\r\n");
    let mut request = head.into_bytes();
    request.extend_from_slice(&call.body);
    tcp.write_all(&request).map_err(io)?;
    https::read_reply(&url, BufReader::new(tcp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{http, http_server};
    use kbf_host::testkit::FakeExec;

    fn api(port: u16) -> Api {
        Api {
            name: "NanoHUB",
            port,
            user: "nanohub",
            key: Secret::new("k3y"),
        }
    }

    /// Catches a dry run that calls the API, a call line that shows the body (a private
    /// key travels in it), a key sent anywhere but the Basic auth header, a body or
    /// content type that does not reach the server, and a refusal reported as done.
    #[test]
    fn a_call_is_printed_and_made_only_with_apply() {
        let server = http_server(|r| {
            if r.path == "/refuse" {
                http("403 Forbidden", b"no such topic")
            } else {
                http("200 OK", b"{\"topic\":\"t\"}")
            }
        });
        let a = api(server.addr.port());
        let exec = FakeExec::new(0);
        let put = Call::put("/api/v1/nanomdm/pushcert", b"CERT+KEY".to_vec());
        let mut out = Vec::new();
        let got = call(&exec, &a, &put, Mode::DryRun, &mut out).expect("dry run");
        assert_eq!(got, None);
        assert!(server.requests().is_empty());
        let line = format!(
            "DRY-RUN: PUT http://127.0.0.1:{}/api/v1/nanomdm/pushcert (NanoHUB API, 8 bytes)\n",
            a.port
        );
        assert_eq!(String::from_utf8_lossy(&out), line);

        let mut out = Vec::new();
        let got = call(&exec, &a, &put, Mode::Apply, &mut out).expect("applied");
        assert_eq!(got, Some(b"{\"topic\":\"t\"}".to_vec()));
        let shown = String::from_utf8_lossy(&out).into_owned();
        assert!(
            shown.starts_with("+ PUT ") && !shown.contains("CERT"),
            "{shown}"
        );
        let r = &server.requests()[0];
        assert_eq!(
            (r.method.as_str(), r.body.as_slice()),
            ("PUT", &b"CERT+KEY"[..])
        );
        // base64("nanohub:k3y")
        assert_eq!(r.header("authorization"), Some("Basic bmFub2h1YjprM3k="));
        assert_eq!(r.header("content-type"), None);
        assert!(!r.path.contains("k3y"));

        let post = Call::post_json("/refuse", b"{}".to_vec());
        let err = call(&exec, &a, &post, Mode::Apply, &mut Vec::new()).map_err(|e| e.to_string());
        let err = err.err().unwrap_or_default();
        assert!(err.contains("answered 403: no such topic"), "{err}");
        let r = &server.requests()[1];
        assert_eq!(
            r.header("content-type"),
            Some("application/json;charset=UTF8")
        );
        let get = Call::get("/version");
        assert!(call(&exec, &a, &get, Mode::Apply, &mut Vec::new()).is_ok());
        assert_eq!(server.requests()[2].method, "GET");
    }

    /// Catches a call made from a binary others can replace (it would carry the API key
    /// and the push certificate's private key for whoever replaced it), and a service
    /// that is down reported as anything but a failed call.
    #[test]
    fn no_call_is_made_from_an_untrusted_binary_or_to_a_closed_port() {
        let server = http_server(|_| http("200 OK", b""));
        let mut exec = FakeExec::new(0);
        exec.untrusted = true;
        let a = api(server.addr.port());
        let err = call(&exec, &a, &Call::get("/x"), Mode::Apply, &mut Vec::new());
        assert!(matches!(
            err,
            Err(MdmError::Deploy(kbf_host::HostError::Untrusted(_)))
        ));
        assert!(server.requests().is_empty());

        let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = closed.local_addr().expect("addr").port();
        drop(closed);
        let exec = FakeExec::new(0);
        let err = call(
            &exec,
            &api(port),
            &Call::get("/x"),
            Mode::Apply,
            &mut Vec::new(),
        );
        assert!(matches!(&err, Err(MdmError::Api { .. })), "{err:?}");
        assert_eq!(err.map_err(|e| e.exit_code()).err(), Some(1));
    }
}
