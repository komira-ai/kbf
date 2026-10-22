//! A client that can only read.
//!
//! The exporter touches a running farm, so it is built so that it cannot write: every
//! request goes through [`ReadOnly`], which sends a `GET` and refuses any other method
//! before a byte leaves the process, and [`OperatorClient`]'s adapter refuses again.

use std::net::SocketAddr;

use kbf_server::api_client::OperatorClient;

/// An answer: the status and the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    /// The HTTP status code.
    pub status: u16,
    /// The body.
    pub body: String,
}

/// Why a call did not get an answer.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CallError {
    /// The method is not `GET`.
    #[error("{method} {path}: the exporter reads only (GET)")]
    NotGet {
        /// The refused method.
        method: String,
        /// The path it was meant for.
        path: String,
    },
    /// The server could not be reached or its answer was not read.
    #[error("{0}")]
    Transport(String),
}

/// Something that sends a request and returns the answer.
pub trait Transport {
    /// Sends `method` `path`.
    ///
    /// # Errors
    /// See [`CallError`].
    fn call(&self, method: &str, path: &str) -> Result<Reply, CallError>;
}

/// The only way the exporter reaches the farm.
pub struct ReadOnly<T> {
    inner: T,
}

impl<T: Transport> ReadOnly<T> {
    /// A read-only view of `inner`.
    pub const fn new(inner: T) -> Self {
        Self { inner }
    }

    /// `method` `path`, if the method is `GET`.
    ///
    /// # Errors
    /// [`CallError::NotGet`] for any other method (nothing is sent), or the transport's.
    pub fn request(&self, method: &str, path: &str) -> Result<Reply, CallError> {
        if method != "GET" {
            return Err(CallError::NotGet {
                method: method.to_owned(),
                path: path.to_owned(),
            });
        }
        self.inner.call(method, path)
    }

    /// `GET path`.
    ///
    /// # Errors
    /// See [`CallError`].
    pub fn get(&self, path: &str) -> Result<Reply, CallError> {
        self.request("GET", path)
    }
}

/// The operator API at an address, with the token of a read-only credential.
pub struct Api {
    client: OperatorClient,
}

impl Api {
    /// The API at `addr`; `token` is sent as the bearer.
    pub const fn new(addr: SocketAddr, token: Option<String>) -> Self {
        Self {
            client: OperatorClient::new(addr, token),
        }
    }
}

impl Transport for Api {
    fn call(&self, method: &str, path: &str) -> Result<Reply, CallError> {
        if method != "GET" {
            return Err(CallError::NotGet {
                method: method.to_owned(),
                path: path.to_owned(),
            });
        }
        let reply = self
            .client
            .get(path)
            .map_err(|e| CallError::Transport(e.to_string()))?;
        Ok(Reply {
            status: reply.status,
            body: reply.body,
        })
    }
}
