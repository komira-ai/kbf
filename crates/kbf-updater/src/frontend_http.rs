//! The real [`FrontendControl`]: the operator API of one frontend pool member, which is on
//! loopback at the member's operator port (its `base` of the inventory plus two), through the
//! client of [`crate::operator`] with the same token and proxy-secret files as every other call.
//!
//! - **retire**: `POST /v1/frontend/retire` (`docs/api.md`). The frontend refuses new work,
//!   hands its streams and sessions to the other frontends and answers its status when that has
//!   ended, with `retired` true; the call is given `RETIRE_TIMEOUT` to be answered. It is
//!   irreversible: the unit is restarted after it. A 409 says the frontend is not one over a
//!   state store, which has no retire.
//! - **retire status**: `GET /v1/frontend/retire`, the same status as it is now.
//! - **commit**: the `commit` of `GET /healthz`, the commit the member's binary was built from
//!   (12 hex digits, or `unknown` for a build without a git checkout).
//!
//! Error texts carry no address and no token: they reach `GET /rolls/{id}`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use serde::Deserialize;

use crate::inventory::Host;
use crate::operator::{CHECK, OperatorApi};
use crate::ports::{FrontendControl, PortError, RetireStatus};

/// The path of the retire.
const RETIRE: &str = "/v1/frontend/retire";

/// How long the `POST` of the retire may take to be answered. The frontend answers it once the
/// retire has ended, which is up to ten seconds of quiesce and the same again for the requeues
/// (more with `--handover-quiesce-secs`), longer than the client's bound for any other call.
const RETIRE_TIMEOUT: Duration = Duration::from_secs(120);

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

    /// The retire status from `POST` (`begin`) or `GET` of the retire route.
    fn retire_call(&self, host: &Host, begin: bool) -> Result<RetireStatus, PortError> {
        let api = self.member_api(host)?;
        let reply = if begin {
            api.clone().with_timeout(RETIRE_TIMEOUT).post(RETIRE, "")?
        } else {
            api.get(RETIRE)?
        };
        let (what, alias) = (api.what(), &host.alias);
        match reply.status {
            200 => {
                let parsed = serde_json::from_slice(&reply.body);
                parsed.map_err(|_| garbled(what, "retire status"))
            }
            409 => Err(PortError::other(format!(
                "{alias} is not a frontend over a state store, which has no retire"
            ))),
            _ => Err(reply.failure(what, CHECK)),
        }
    }
}

impl FrontendControl for HttpFrontend {
    fn retire(&self, host: &Host) -> Result<RetireStatus, PortError> {
        self.retire_call(host, true)
    }

    fn retire_status(&self, host: &Host) -> Result<RetireStatus, PortError> {
        self.retire_call(host, false)
    }

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
}
