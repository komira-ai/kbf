//! The real ports of an agent, and the process around it.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_types::HostRole;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

use super::coordinator::{HttpsCoordinator, TlsFiles};
use super::flags::{Config, NodeApi};
use super::local::{Limits, LocalInstaller, SocketLocal};
use super::run::{Agent, Stop, World};
use super::status::{Status, host_platform};
use crate::identity::{Identity, identity_of};
use crate::inventory::{Host, Role};
use crate::node_drain::NodeDrain;
use crate::node_health::NodeHealth;
use crate::operator::OperatorApi;
use crate::ports::{Drain, Health, HealthCheck, PortError, SystemClock};
use crate::version::Version;

/// A host whose node is never drained (a storage host): the drain and the undrain are done
/// already, and it holds no leases.
pub struct NoDrain;

impl Drain for NoDrain {
    fn drain(&self, _host: &Host, _deadline: Duration) -> Result<(), PortError> {
        Ok(())
    }

    fn leases(&self, _host: &Host) -> Result<usize, PortError> {
        Ok(0)
    }

    fn undrain(&self, _host: &Host) -> Result<(), PortError> {
        Ok(())
    }
}

/// [`NoDrain`], as a port.
pub fn no_drain() -> Arc<dyn Drain> {
    Arc::new(NoDrain)
}

/// The ports of an agent that was given no credential for the frontend's operator API: it
/// refuses every update before it fetches anything, and drains nothing.
pub struct NoCredential;

const NO_CREDENTIAL: &str = "this agent was given no operator API credential, so it cannot drain \
     its node and updates nothing";

impl HealthCheck for NoCredential {
    fn refusal(&self, _host: &Host) -> Option<String> {
        Some(NO_CREDENTIAL.to_owned())
    }

    fn check(&self, _host: &Host, _target: &Version) -> Result<Health, PortError> {
        Err(PortError::other(NO_CREDENTIAL))
    }

    fn check_held(&self, _host: &Host, _target: &Version) -> Result<Health, PortError> {
        Err(PortError::other(NO_CREDENTIAL))
    }
}

impl Drain for NoCredential {
    fn drain(&self, _host: &Host, _deadline: Duration) -> Result<(), PortError> {
        Err(PortError::other(NO_CREDENTIAL))
    }

    fn leases(&self, _host: &Host) -> Result<usize, PortError> {
        Err(PortError::other(NO_CREDENTIAL))
    }

    fn undrain(&self, _host: &Host) -> Result<(), PortError> {
        Err(PortError::other(NO_CREDENTIAL))
    }
}

/// The drain and the health of a host.
type NodePorts = (Arc<dyn Drain>, Arc<dyn HealthCheck>);

/// The drain and the health of a host through `api`, or the refusals of [`NoCredential`].
fn node_ports(
    api: Option<&NodeApi>,
    role: Role,
    local: &Arc<SocketLocal>,
) -> Result<NodePorts, String> {
    let Some(api) = api else {
        return Ok((Arc::new(NoCredential), Arc::new(NoCredential)));
    };
    let api = OperatorApi::new(
        api.addr,
        api.token_file.clone(),
        api.proxy_auth_file.clone(),
    )?;
    let drain = drain_for(role, &api);
    let health = Arc::new(NodeHealth::with_api(
        api,
        Arc::new(LocalInstaller(local.clone())),
    ));
    Ok((drain, health))
}

/// Who the updater certificate in `files` says this host is.
///
/// # Errors
/// The file cannot be read, holds no certificate, or the certificate is no updater's.
pub fn identity_of_files(files: &TlsFiles) -> Result<Identity, String> {
    let pem = std::fs::read(&files.cert).map_err(|e| format!("{}: {e}", files.cert.display()))?;
    let leaf = CertificateDer::pem_slice_iter(&pem)
        .next()
        .ok_or_else(|| format!("{} holds no certificate", files.cert.display()))?
        .map_err(|e| format!("{}: {e}", files.cert.display()))?;
    identity_of(leaf.as_ref()).map_err(|e| format!("{}: {e}", files.cert.display()))
}

/// The drain of a host of `role`: none for a storage host (the roll never drains one, a daemon
/// on it included), the frontend's cordon, drain and uncordon for the rest.
pub fn drain_for(role: Role, api: &OperatorApi) -> Arc<dyn Drain> {
    if role == Role::Storage {
        no_drain()
    } else {
        Arc::new(NodeDrain::with_api(api.clone()))
    }
}

/// Unix seconds now.
pub(super) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The agent of this host, with the real ports.
///
/// # Errors
/// Why not: no releases for this platform, a certificate that is no updater's or is a
/// frontend's (a frontend is updated by the roll, not yet by the agent), files that do not make
/// a TLS configuration, or an operator API client whose token files are not usable.
pub fn build(cfg: &Config) -> Result<Agent, String> {
    let platform = host_platform().ok_or("kbf has no release for this platform")?;
    let identity = identity_of_files(&cfg.files)?;
    let role = match identity.role {
        HostRole::Frontend => {
            return Err(
                "this certificate is a frontend's: a frontend is updated by the roll, \
                        not by the agent"
                    .to_owned(),
            );
        }
        HostRole::Storage => Role::Storage,
        HostRole::Daemon => Role::Daemon,
        HostRole::MacDaemon => Role::MacDaemon,
    };
    super::coordinator::client_config(&cfg.files).map_err(|e| e.to_string())?;
    let node_id = identity.node_id.to_string();
    let host = Host {
        alias: node_id.clone(),
        address: "localhost".to_owned(),
        role,
        host_key: String::new(),
        node_id: Some(node_id),
        member: None,
    };
    let local = Arc::new(SocketLocal::new(&cfg.helper_socket, Limits::default()));
    let (drain, health) = node_ports(cfg.node_api.as_ref(), host.role, &local)?;
    let coordinator = Arc::new(
        HttpsCoordinator::new(cfg.coordinator.clone(), cfg.files.clone()).bound_to(identity),
    );
    let world = World {
        coordinator,
        local,
        drain,
        health,
        clock: Arc::new(SystemClock::new()),
        unix_now: Arc::new(unix_now),
        log: Arc::new(|line| eprintln!("kbf-updater: {line}")),
    };
    let unknown = Version::parse("0").map_err(|e| e.to_string())?;
    Ok(Agent::new(
        host,
        cfg.channel.clone(),
        cfg.timing,
        world,
        Status::unknown(platform, unknown),
    ))
}

/// Run `agent` until the handle given to `ready` stops it. The exit code and text of the
/// process.
pub fn run_agent(agent: &Agent, ready: impl FnOnce(Stop)) -> (u8, String) {
    let stop = Stop::default();
    ready(stop.clone());
    agent.run(&stop);
    (0, String::new())
}

/// [`build`], then [`run_agent`].
pub fn run_config(cfg: &Config, ready: impl FnOnce(Stop)) -> (u8, String) {
    match build(cfg) {
        Err(why) => (1, format!("kbf-updater: {why}\n")),
        Ok(agent) => run_agent(&agent, ready),
    }
}
