//! The real [`FrontendControl`]: the operator API of one frontend pool member, which is on
//! loopback at the member's operator port (its `base` of the inventory plus two), through the
//! client of [`crate::operator`] with the same token file as every other call.
//!
//! - **commit**: the `commit` of `GET /healthz`, the commit the member's binary was built from
//!   (12 hex digits, or `unknown` for a build without a git checkout).
//! - **drain**: `POST /v1/frontend/drain` begins the member's own drain, which ends in its exit.
//!   A member that is not running refuses the connection, and the error says so
//!   ([`PortError::is_refused`]).
//! - **drain status**: `GET /v1/frontend/drain`, the same route read.
//! - **ready**: `GET /readyz`, 200 for ready and 503 for not.
//!
//! Error texts carry no address and no token: they reach `GET /rolls/{id}`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use serde::Deserialize;

use crate::inventory::Host;
use crate::operator::{CHECK, OperatorApi};
use crate::ports::{DrainStatus, FrontendControl, PortError};

/// The path of the member's own drain.
const DRAIN: &str = "/v1/frontend/drain";

/// The path of the member's readiness.
const READY: &str = "/readyz";

#[derive(Deserialize)]
struct Alive {
    commit: String,
}

/// The error for an answer of 200 whose body is not the `of` it should be.
fn garbled(what: &str, of: &str) -> PortError {
    PortError::other(format!("{what} sent a garbled {of}"))
}

/// A frontend member's own API over HTTP.
#[derive(Clone)]
pub struct HttpFrontend {
    api: OperatorApi,
}

impl HttpFrontend {
    /// A client over `api`, whose address is replaced by each member's.
    pub fn with_api(api: OperatorApi) -> HttpFrontend {
        HttpFrontend { api }
    }

    /// The client for the member `host` stands for.
    fn member_api(&self, host: &Host) -> Result<OperatorApi, PortError> {
        let alias = &host.alias;
        let Some(member) = &host.member else {
            return Err(PortError::other(format!("{alias} is not a pool member")));
        };
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let addr = SocketAddr::new(loopback, member.operator_port());
        Ok(self.api.at(addr))
    }
}

impl FrontendControl for HttpFrontend {
    fn commit(&self, host: &Host) -> Result<String, PortError> {
        let api = self.member_api(host)?;
        let what = api.what();
        let reply = api.get("/healthz")?;
        if reply.status != 200 {
            return Err(reply.failure(what, CHECK));
        }
        let parsed = serde_json::from_slice::<Alive>(&reply.body);
        let alive = parsed.map_err(|_| garbled(what, "health report"))?;
        Ok(alive.commit)
    }

    fn drain(&self, host: &Host) -> Result<(), PortError> {
        let api = self.member_api(host)?;
        let what = api.what();
        let reply = api.post(DRAIN, "")?;
        match reply.status {
            200 => Ok(()),
            _ => Err(reply.failure(what, CHECK)),
        }
    }

    fn drain_status(&self, host: &Host) -> Result<DrainStatus, PortError> {
        let api = self.member_api(host)?;
        let what = api.what();
        let reply = api.get(DRAIN)?;
        if reply.status != 200 {
            return Err(reply.failure(what, CHECK));
        }
        let parsed = serde_json::from_slice(&reply.body);
        parsed.map_err(|_| garbled(what, "drain status"))
    }

    fn ready(&self, host: &Host) -> Result<bool, PortError> {
        let api = self.member_api(host)?;
        let what = api.what();
        let reply = api.get(READY)?;
        match reply.status {
            200 => Ok(true),
            503 => Ok(false),
            _ => Err(reply.failure(what, CHECK)),
        }
    }
}
