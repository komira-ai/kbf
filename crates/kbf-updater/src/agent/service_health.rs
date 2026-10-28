//! The health of a service that declares a readiness URL: it is healthy when
//! `GET <path>` on its loopback address answers 2xx.
//!
//! The answer says nothing of which version answered, so it is the service's word that it
//! serves, not that the target is the one running; a service whose readiness hides the old
//! release during a switch should not rely on this alone.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use crate::inventory::Host;
use crate::ports::{Health, HealthCheck, PortError};
use crate::version::Version;

/// How long the connection and the answer each may take.
const WAIT: Duration = Duration::from_secs(5);

/// The most bytes of the answer that are read: the status line is all that is used.
const HEAD_MAX: u64 = 1024;

/// Health by a readiness URL.
pub struct ReadyUrl {
    addr: SocketAddr,
    path: String,
}

impl ReadyUrl {
    /// `path` is checked by the flags: it starts with `/` and holds no space or control
    /// character.
    pub fn new(addr: SocketAddr, path: String) -> Self {
        Self { addr, path }
    }

    /// The status the service answers `GET path` with.
    fn status(&self) -> Result<u16, PortError> {
        let down = |e: &dyn std::fmt::Display| PortError::unreachable(format!("readiness: {e}"));
        let mut stream = TcpStream::connect_timeout(&self.addr, WAIT).map_err(|e| down(&e))?;
        stream.set_read_timeout(Some(WAIT)).map_err(|e| down(&e))?;
        stream.set_write_timeout(Some(WAIT)).map_err(|e| down(&e))?;
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.path, self.addr
        );
        stream.write_all(request.as_bytes()).map_err(|e| down(&e))?;
        let mut head = Vec::new();
        stream
            .take(HEAD_MAX)
            .read_to_end(&mut head)
            .map_err(|e| down(&e))?;
        let line = head.split(|b| *b == b'\n').next().unwrap_or_default();
        let line = String::from_utf8_lossy(line);
        let mut words = line.split_whitespace();
        match (words.next(), words.next().map(str::parse::<u16>)) {
            (Some(version), Some(Ok(code))) if version.starts_with("HTTP/1.") => Ok(code),
            _ => Err(PortError::other(
                "readiness: the answer is not an HTTP status line",
            )),
        }
    }
}

impl HealthCheck for ReadyUrl {
    fn refusal(&self, _host: &Host) -> Option<String> {
        None
    }

    fn check(&self, _host: &Host, _target: &Version) -> Result<Health, PortError> {
        let status = self.status()?;
        Ok(if (200..300).contains(&status) {
            Health::Healthy
        } else {
            Health::Waiting(format!("the readiness URL answers HTTP {status}"))
        })
    }

    /// A service with a readiness URL is not held by anything: the same as [`Self::check`].
    fn check_held(&self, host: &Host, target: &Version) -> Result<Health, PortError> {
        self.check(host, target)
    }
}
