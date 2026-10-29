//! Where the client dials, what it presents, and which CA it trusts.

use std::io::Read as _;
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use http::HeaderValue;
use http::uri::Uri;

use crate::error::ConfigError;

/// The most a token file may hold, in bytes. The server wants 32 or more printable bytes;
/// this only keeps a wrong file from being read whole.
const MAX_TOKEN_FILE: u64 = 4096;

/// The lease server's address: a scheme, a host and a port, and nothing else. The token is
/// sent only here, and a redirect is never followed, so no other host sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    tls: bool,
    /// The host as dialled and as TLS names it: a DNS name or an IP, without brackets.
    host: String,
    port: u16,
    /// The `Host` header value.
    authority: String,
}

impl Origin {
    /// Whether the origin is `https`.
    #[must_use]
    pub fn is_tls(&self) -> bool {
        self.tls
    }

    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    pub(crate) fn authority(&self) -> &str {
        &self.authority
    }
}

impl FromStr for Origin {
    type Err = ConfigError;

    /// Reads `http://host[:port]` or `https://host[:port]`, with an optional trailing `/`.
    /// A path, query, user name or any other scheme is refused, and so is `http` for a host
    /// that is not loopback: the token would cross the network in clear.
    fn from_str(text: &str) -> Result<Self, ConfigError> {
        let refuse = |reason| ConfigError::Origin {
            origin: text.to_owned(),
            reason,
        };
        let uri: Uri = text.parse().map_err(|_| refuse("not a URI"))?;
        let tls = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => return Err(refuse("the scheme must be http or https")),
        };
        let authority = uri.authority().ok_or_else(|| refuse("no host"))?;
        if authority.as_str().contains('@') {
            return Err(refuse("a user name or password has no place in an origin"));
        }
        if uri.query().is_some() || uri.path() != "/" {
            return Err(refuse("an origin has no path or query"));
        }
        let host = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']');
        let loopback =
            host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
        if !tls && !loopback {
            return Err(ConfigError::PlainHttp(text.to_owned()));
        }
        // `Authority::port` reads a port that does not fit a u16 as no port at all.
        let after_host = authority.as_str().strip_prefix(authority.host());
        let port = match after_host.and_then(|rest| rest.strip_prefix(':')) {
            Some(digits) => digits
                .parse()
                .map_err(|_| refuse("the port is not a number below 65536"))?,
            None if tls => 443,
            None => 80,
        };
        Ok(Self {
            tls,
            host: host.to_owned(),
            port,
            authority: authority.as_str().to_owned(),
        })
    }
}

/// The bearer token. It prints as `Token(..)` and goes into the one header it is for,
/// marked sensitive so the HTTP stack does not log it.
#[derive(Clone)]
pub struct Token(HeaderValue);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

impl Token {
    /// A token from its text: one or more printable ASCII characters, no space.
    ///
    /// # Errors
    /// The text is empty or holds a byte outside `!` to `~`.
    pub fn new(secret: &str) -> Result<Self, ConfigError> {
        let printable =
            !secret.is_empty() && secret.bytes().all(|byte| (0x21..=0x7e).contains(&byte));
        let value = printable.then(|| HeaderValue::from_str(&format!("Bearer {secret}")).ok());
        let mut value = value.flatten().ok_or(ConfigError::BadToken(
            "empty, or not printable ASCII without spaces",
        ))?;
        value.set_sensitive(true);
        Ok(Self(value))
    }

    /// Reads the token from a file only its owner can use (mode 0600 or 0400, like the
    /// server's own token file). One trailing newline is dropped.
    ///
    /// # Errors
    /// The file cannot be read, is not a regular file, has a mode other than 0600 or 0400
    /// (the file's owner is not checked), is over 4 KiB, or holds something that is not a token.
    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let file_error = |source| ConfigError::TokenFile {
            path: path.to_path_buf(),
            source,
        };
        let mode_error = |problem| ConfigError::TokenFileMode {
            path: path.to_path_buf(),
            problem,
        };
        // One handle for the checks and the read, so the file cannot change in between.
        let file = std::fs::File::open(path).map_err(file_error)?;
        let meta = file.metadata().map_err(file_error)?;
        if !meta.is_file() {
            return Err(mode_error("not a regular file"));
        }
        if !matches!(meta.permissions().mode() & 0o777, 0o600 | 0o400) {
            return Err(mode_error("mode is not 0600 or 0400"));
        }
        let mut text = String::new();
        let read = file.take(MAX_TOKEN_FILE + 1).read_to_string(&mut text);
        read.map_err(file_error)?;
        if text.len() as u64 > MAX_TOKEN_FILE {
            return Err(mode_error("over 4 KiB"));
        }
        let secret = text.strip_suffix('\n').unwrap_or(&text);
        Self::new(secret)
    }

    pub(crate) fn header(&self) -> &HeaderValue {
        &self.0
    }
}

/// How to reach one lease server.
#[derive(Debug, Clone)]
pub struct Config {
    pub(crate) origin: Origin,
    pub(crate) token: Token,
    pub(crate) ca_pem: Option<Vec<u8>>,
    pub(crate) connect_timeout: Duration,
    pub(crate) request_timeout: Duration,
}

impl Config {
    /// The connect timeout of [`Config::new`].
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
    /// The request timeout of [`Config::new`].
    pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

    /// A config for `origin` and `token`, with the default timeouts and no CA.
    #[must_use]
    pub fn new(origin: Origin, token: Token) -> Self {
        Self {
            origin,
            token,
            ca_pem: None,
            connect_timeout: Self::CONNECT_TIMEOUT,
            request_timeout: Self::REQUEST_TIMEOUT,
        }
    }

    /// The CA certificate (PEM) that signed the server's certificate. It is the only trust
    /// anchor: no system roots are consulted. An `https` origin needs it; an `http` one
    /// refuses it.
    #[must_use]
    pub fn with_ca_pem(mut self, pem: Vec<u8>) -> Self {
        self.ca_pem = Some(pem);
        self
    }

    /// How long connecting (TCP and the TLS handshake) and then the exchange (the request
    /// sent and the answer read in full) may each take.
    #[must_use]
    pub fn with_timeouts(mut self, connect: Duration, request: Duration) -> Self {
        self.connect_timeout = connect;
        self.request_timeout = request;
        self
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
