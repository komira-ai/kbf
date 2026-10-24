//! The real [`Pool`]: the admin API of a kbf-proxy in pool mode (`docs/design/kbf-proxy-api.md`),
//! plain HTTP/1.1 on the loopback address of the proxy's `admin_listen`, through
//! [`OperatorApi::anonymous`].
//!
//! The API asks for no credential, so none is sent: not the operator token, not the proxy's
//! secret.
//!
//! - **view**: `GET /v1/proxy/upstreams`, a list of `{"name", "state", "ready", "open_streams"}`
//!   where `state` is `up` or `draining` and `open_streams` counts the streams open on the
//!   member by listener. Any state but `up` is read as not taking streams.
//! - **drain** and **undrain**: `POST /v1/proxy/upstreams/{name}:drain` and `:undrain`. Both are
//!   idempotent on the proxy's side, and a name that is no member is a 404 that changes
//!   nothing.
//!
//! The name stands in a path as it is, so a name the host would not take for a unit instance
//! is refused here without a request. Error texts carry no address: they reach
//! `GET /rolls/{id}`.

use std::net::SocketAddr;

use serde::Deserialize;

use crate::operator::OperatorApi;
use crate::ports::{Pool, PoolMember, PortError};
use crate::ssh_installer::frontend_name_ok;

/// What an operator checks when the admin API refuses a call or does not answer.
pub const CHECK: &str = "--proxy-admin";

/// What a call says when the daemon was started without `--proxy-admin`.
pub const NOT_CONFIGURED: &str = "there is no proxy admin API: --proxy-admin was not given";

/// The path of the list, and of every member under it.
const LIST: &str = "/v1/proxy/upstreams";

#[derive(Deserialize)]
struct Row {
    name: String,
    state: String,
    ready: bool,
    open_streams: Streams,
}

#[derive(Deserialize)]
struct Streams {
    reapi: u64,
    operator: u64,
}

impl From<Row> for PoolMember {
    fn from(row: Row) -> PoolMember {
        PoolMember {
            name: row.name,
            draining: row.state != "up",
            ready: row.ready,
            reapi_streams: row.open_streams.reapi,
            operator_streams: row.open_streams.operator,
        }
    }
}

/// The pool over the proxy's admin API. Without `--proxy-admin` there is no pool
/// ([`Pool::configured`] is false, and the roll does not ask it), and each call says so.
#[derive(Clone)]
pub struct HttpPool {
    api: Option<OperatorApi>,
}

impl HttpPool {
    /// The pool of the proxy whose admin API listens at `admin`.
    pub fn new(admin: SocketAddr) -> HttpPool {
        HttpPool::with_api(OperatorApi::anonymous(admin))
    }

    /// The pool over a client that was made already.
    pub fn with_api(api: OperatorApi) -> HttpPool {
        HttpPool { api: Some(api) }
    }

    /// No pool: the daemon was started without `--proxy-admin`.
    pub fn unconfigured() -> HttpPool {
        HttpPool { api: None }
    }

    fn api(&self) -> Result<&OperatorApi, PortError> {
        match &self.api {
            Some(api) => Ok(api),
            None => Err(PortError::other(NOT_CONFIGURED)),
        }
    }

    /// `POST` the verb `drain` or `undrain` for the member `name`.
    fn post(&self, name: &str, verb: &str) -> Result<(), PortError> {
        let api = self.api()?;
        let what = api.what();
        if !frontend_name_ok(name) {
            return Err(PortError::other(format!("{name:?} is not a member name")));
        }
        let reply = api.post(&format!("{LIST}/{name}:{verb}"), "")?;
        match reply.status {
            200 => Ok(()),
            404 => Err(PortError::other(format!("{what} has no member {name}"))),
            _ => Err(reply.failure(what, CHECK)),
        }
    }
}

impl Pool for HttpPool {
    fn configured(&self) -> bool {
        self.api.is_some()
    }

    fn view(&self) -> Result<Vec<PoolMember>, PortError> {
        let api = self.api()?;
        let what = api.what();
        let reply = api.get(LIST)?;
        if reply.status != 200 {
            return Err(reply.failure(what, CHECK));
        }
        let parsed = serde_json::from_slice::<Vec<Row>>(&reply.body);
        let rows = parsed.map_err(|_| PortError::other(format!("{what}'s list is garbled")))?;
        Ok(rows.into_iter().map(PoolMember::from).collect())
    }

    fn drain(&self, name: &str) -> Result<(), PortError> {
        self.post(name, "drain")
    }

    fn undrain(&self, name: &str) -> Result<(), PortError> {
        self.post(name, "undrain")
    }
}
