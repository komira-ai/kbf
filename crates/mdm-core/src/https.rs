//! An `https://` URL, parsed for its parts and nothing else (no network here).

use std::fmt;

/// An `https://` URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpsUrl {
    url: String,
    host: String,
    port: u16,
    path: String,
}

/// Why a string is not an [`HttpsUrl`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HttpsUrlError {
    /// Another scheme, or none.
    #[error("{0:?} is not an https:// URL")]
    Scheme(String),
    /// No host, or a user part.
    #[error("{0:?} names no host, or names a user")]
    Host(String),
    /// A port that is not a number from 1 to 65535.
    #[error("{0:?} has a port that is not a number from 1 to 65535")]
    Port(String),
}

impl HttpsUrl {
    /// `url` as an [`HttpsUrl`], or why it is not one.
    pub fn new(url: &str) -> Result<HttpsUrl, HttpsUrlError> {
        let Some(rest) = url.strip_prefix("https://") else {
            return Err(HttpsUrlError::Scheme(url.to_owned()));
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        // No user part (`https://user:key@host/`): it would be printed with the URL.
        if authority.contains(['@', '[', ']']) {
            return Err(HttpsUrlError::Host(url.to_owned()));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => {
                let port = port.parse::<u16>().ok().filter(|p| *p != 0);
                let Some(port) = port else {
                    return Err(HttpsUrlError::Port(url.to_owned()));
                };
                (host, port)
            }
            None => (authority, 443),
        };
        if host.is_empty() {
            return Err(HttpsUrlError::Host(url.to_owned()));
        }
        Ok(HttpsUrl {
            url: url.to_owned(),
            host: host.to_owned(),
            port,
            path: path.to_owned(),
        })
    }

    /// The host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port, 443 when the URL names none.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The path and query, `/` when the URL has none.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The last part of the path, without the query.
    pub fn file_name(&self) -> &str {
        let path = self.path.split('?').next().unwrap_or_default();
        path.rsplit('/').next().unwrap_or_default()
    }
}

impl fmt::Display for HttpsUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.url)
    }
}
