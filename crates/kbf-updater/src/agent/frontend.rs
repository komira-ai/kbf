//! The drain and the health of a frontend host's agent: the host runs one member of the
//! kbf-proxy pool, and the agent takes that member out of the pool, swaps, checks it and puts
//! it back, with the same steps as for any other host (`update.rs`).
//!
//! - **drain**: the member is drained at the proxy (its admin API, on loopback), and only if
//!   some other member is up and ready. The streams open on it are what the update waits for
//!   (`leases`);
//! - **health**: the proxy lists the member ready, and the member's own `healthz` says it was
//!   built from the target's commit. A member that is still drained is judged held, as the
//!   node of another host is;
//! - **undrain**: the member is put back, and the proxy must list it taking streams.
//!
//! The coordinator never orders the last frontend that is up (`decide.rs`); this is the
//! agent's own refusal for the case the coordinator cannot see, a pool whose other members
//! the proxy finds not ready: the member is left in the pool and the update fails with the
//! reason, before anything is fetched when it can be known then.

use std::sync::Arc;

use crate::health::{commit_of, names};
use crate::inventory::Host;
use crate::ports::{Drain, FrontendControl, Health, HealthCheck, Pool, PoolMember, PortError};
use crate::version::Version;

/// Why a member that is the only one up and ready is not drained, and what the operator does.
pub const ONLY_FRONTEND: &str = "this frontend is the only member of the kbf-proxy pool that is \
     up and ready, so draining it would leave the proxy nothing to send to; the agent does not \
     update it. Add a second frontend to the pool, or update this host by hand in a window in \
     which the farm may be down";

/// Why an agent whose host is no pool member refuses.
pub const NOT_A_MEMBER: &str = "this host's agent was given no pool member, so it cannot drain \
     its frontend; give --proxy-admin and --pool-member";

/// A frontend host's drain and health, over the proxy's pool and the member's own API.
pub struct FrontendNode {
    pool: Arc<dyn Pool>,
    control: Arc<dyn FrontendControl>,
}

impl FrontendNode {
    pub fn new(pool: Arc<dyn Pool>, control: Arc<dyn FrontendControl>) -> Self {
        Self { pool, control }
    }

    /// The name of the pool member `host` stands for.
    fn name(host: &Host) -> Result<&str, PortError> {
        host.member
            .as_ref()
            .map(|member| member.instance.as_str())
            .ok_or_else(|| PortError::other(NOT_A_MEMBER))
    }

    /// The proxy's view of the host's member.
    fn look<'h>(&self, host: &'h Host) -> Result<Look<'h>, PortError> {
        let name = Self::name(host)?;
        let all = self.pool.view()?;
        let others = all.iter().any(|m| m.name != name && m.serves());
        let row = all.into_iter().find(|m| m.name == name);
        Ok(Look { name, row, others })
    }

    fn judge(&self, host: &Host, target: &Version, held: bool) -> Result<Health, PortError> {
        let commit = match commit_of(target) {
            Ok(commit) => commit,
            Err(health) => return Ok(health),
        };
        let look = self.look(host)?;
        let Some(row) = look.row else {
            return Ok(Health::Abandoned(unlisted(look.name).to_string()));
        };
        let name = look.name;
        if !held && row.draining {
            return Ok(Health::Waiting(format!("member {name} is still drained")));
        }
        if !row.ready {
            return Ok(Health::Waiting(format!("member {name} is not ready")));
        }
        let built = self.control.commit(host)?;
        if names(&built, commit) {
            return Ok(Health::Healthy);
        }
        Ok(Health::Waiting(format!(
            "member {name} runs build {built}, not the target's"
        )))
    }
}

/// What the proxy shows of a host's member.
struct Look<'h> {
    name: &'h str,
    /// The member's row; none if the proxy does not list it.
    row: Option<PoolMember>,
    /// Some other member is up and ready.
    others: bool,
}

impl Look<'_> {
    /// The row, or the error that the proxy does not list the member.
    fn listed(self) -> Result<PoolMember, PortError> {
        self.row.ok_or_else(|| unlisted(self.name))
    }
}

fn unlisted(name: &str) -> PortError {
    PortError::other(format!("the proxy has no member {name}"))
}

impl Drain for FrontendNode {
    fn drain(&self, host: &Host, _deadline: std::time::Duration) -> Result<(), PortError> {
        let look = self.look(host)?;
        let others = look.others;
        let name = look.listed()?.name;
        if !others {
            return Err(PortError::other(ONLY_FRONTEND));
        }
        self.pool.drain(&name)
    }

    fn leases(&self, host: &Host) -> Result<usize, PortError> {
        let row = self.look(host)?.listed()?;
        if !row.draining {
            let name = &row.name;
            return Err(PortError::other(format!(
                "member {name} takes streams again; it is no longer drained"
            )));
        }
        Ok(usize::try_from(row.streams()).unwrap_or(usize::MAX))
    }

    fn undrain(&self, host: &Host) -> Result<(), PortError> {
        let name = Self::name(host)?;
        self.pool.undrain(name)?;
        if self.look(host)?.listed()?.draining {
            return Err(PortError::other(format!(
                "the proxy still lists member {name} as drained"
            )));
        }
        Ok(())
    }
}

impl HealthCheck for FrontendNode {
    /// A member that has nothing to send its traffic to cannot be updated: said before the
    /// release is fetched. A pool that cannot be read is a refusal too.
    fn refusal(&self, host: &Host) -> Option<String> {
        let look = match self.look(host) {
            Ok(look) => look,
            Err(e) => return Some(e.to_string()),
        };
        match (&look.row, look.others) {
            (None, _) => Some(unlisted(look.name).to_string()),
            (Some(_), false) => Some(ONLY_FRONTEND.to_owned()),
            (Some(_), true) => None,
        }
    }

    fn check(&self, host: &Host, target: &Version) -> Result<Health, PortError> {
        self.judge(host, target, false)
    }

    fn check_held(&self, host: &Host, target: &Version) -> Result<Health, PortError> {
        self.judge(host, target, true)
    }
}
