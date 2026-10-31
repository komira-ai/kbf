//! The real ports of an agent, and the process around it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_enroll::store::Store;
use kbf_types::HostRole;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

use super::container_health::ContainerHealth;
use super::coordinator::{HttpsCoordinator, TlsFiles};
use super::cordon::OwnCordon;
use super::enroll::frontend_url;
use super::flags::{Config, ServiceConfig, ServiceDrain, ServiceHealth};
use super::local::{Limits, LocalInstaller, SocketLocal};
use super::renew::{FrontendRenewer, Renewer};
use super::run::{Agent, ServicePorts, Stop, World};
use super::service_health::ReadyUrl;
use super::status::{Status, host_platform};
use crate::identity::{Identity, identity_of};
use crate::inventory::{Host, Role};
use crate::node_drain::NodeDrain;
use crate::node_health::NodeHealth;
use crate::operator::OperatorApi;
use crate::ports::{Drain, Health, HealthCheck, Placed, PortError, SystemClock};
use crate::version::Version;
use crate::wire::KBF_SERVICE;

/// A host whose node is never drained (a frontend host that runs no daemon): the frontend's own
/// drain is the helper's, and there is no node to give back.
pub struct NoDrain;

impl Drain for NoDrain {
    fn placed(&self, _host: &Host) -> Result<Placed, PortError> {
        Ok(Placed::In)
    }

    fn drain(&self, _host: &Host, _deadline: Option<Duration>) -> Result<(), PortError> {
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

/// The ports of an agent that cannot reach what it needs of the frontend: it refuses every
/// update before it fetches anything, and drains nothing. The text says why.
pub struct NoCredential(pub &'static str);

/// Why an agent given no way to the frontend refuses.
pub const NO_CREDENTIAL: &str = "this agent was given neither --frontend (to drain its node with \
     its own certificate) nor an operator API credential, so it cannot drain its node and \
     updates nothing";

impl HealthCheck for NoCredential {
    fn refusal(&self, _host: &Host) -> Option<String> {
        Some(self.0.to_owned())
    }

    fn check(&self, _host: &Host, _target: &Version) -> Result<Health, PortError> {
        Err(PortError::other(self.0))
    }

    fn check_held(&self, _host: &Host, _target: &Version) -> Result<Health, PortError> {
        Err(PortError::other(self.0))
    }
}

impl Drain for NoCredential {
    fn placed(&self, _host: &Host) -> Result<Placed, PortError> {
        Err(PortError::other(self.0))
    }

    fn drain(&self, _host: &Host, _deadline: Option<Duration>) -> Result<(), PortError> {
        Err(PortError::other(self.0))
    }

    fn leases(&self, _host: &Host) -> Result<usize, PortError> {
        Err(PortError::other(self.0))
    }

    fn undrain(&self, _host: &Host) -> Result<(), PortError> {
        Err(PortError::other(self.0))
    }
}

/// The drain and the health of a host.
type NodePorts = (Arc<dyn Drain>, Arc<dyn HealthCheck>);

/// The drain and the health of the host `identity` names, by the way the agent was given to the
/// frontend:
///
/// - the operator token (`--operator-api`, `--token-file`), the opt-in fallback, if it was given;
/// - else the host's own certificate on the frontend's worker listener (`--frontend`): the drain of
///   the node, and the node's status for a daemon's health or its own storage view for a
///   storage host's ([`NodeHealth::for_updater`]);
/// - else the refusals of [`NoCredential`].
pub(super) fn node_ports(
    cfg: &Config,
    identity: &Identity,
    role: Role,
    unit: &str,
    local: &Arc<SocketLocal>,
) -> Result<NodePorts, String> {
    let installer = || Arc::new(LocalInstaller(local.clone()));
    // A drain that could not record its cordon would refuse every update: found out here.
    let own = |drain: Arc<dyn Drain>| -> Result<Arc<dyn Drain>, String> {
        let own = OwnCordon::new(drain, cordon_dir(cfg), unit);
        own.check_writable()?;
        Ok(Arc::new(own))
    };
    if let Some(api) = &cfg.node_api {
        let api = OperatorApi::new(api.addr, api.token_file.clone())?;
        let drain = own(drain_for(role, &api))?;
        let health = Arc::new(NodeHealth::with_api(api, installer()));
        return Ok((drain, health));
    }
    let Some(frontend) = &cfg.frontend else {
        return Ok((
            Arc::new(NoCredential(NO_CREDENTIAL)),
            Arc::new(NoCredential(NO_CREDENTIAL)),
        ));
    };
    let api = OperatorApi::as_updater(frontend.clone(), cfg.files.clone(), identity.clone());
    let drain = own(drain_for(role, &api))?;
    let health = Arc::new(NodeHealth::for_updater(api, installer()));
    Ok((drain, health))
}

/// Where the marks of the cordons the agent made are kept: `--cordon-dir`, else the state
/// directory, else the directory of the key file (the one an operator placed the files in).
pub(super) fn cordon_dir(cfg: &Config) -> PathBuf {
    cfg.cordon_dir
        .clone()
        .or_else(|| cfg.state_dir.clone())
        .or_else(|| cfg.files.key.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
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

/// The drain of a host of `role`: the frontend's cordon, drain and uncordon of its node. A
/// storage host may run a daemon too (`storage-and-daemon`), which its restart would cut off
/// from its leases, so it drains that if the frontend knows the node, and has nothing to drain
/// if it does not ([`NodeDrain::if_present`]).
pub fn drain_for(role: Role, api: &OperatorApi) -> Arc<dyn Drain> {
    if role == Role::Storage {
        Arc::new(NodeDrain::if_present(api.clone()))
    } else {
        Arc::new(NodeDrain::with_api(api.clone()))
    }
}

/// The renewer of the certificate in the state directory, if the agent was given one
/// (`--state-dir`, which comes with `--enroll-frontend`).
fn renewer_for(cfg: &Config) -> Result<Option<Arc<dyn Renewer>>, String> {
    let (Some(dir), Some(frontend)) = (&cfg.state_dir, &cfg.enroll_frontend) else {
        return Ok(None);
    };
    let store = Store::open(dir).map_err(|e| e.to_string())?;
    let renewer = FrontendRenewer::new(
        frontend_url(frontend),
        store,
        Arc::new(|| i64::try_from(unix_now()).unwrap_or(i64::MAX)),
        Arc::new(|line| eprintln!("kbf-updater: {line}")),
        jitter(),
    )?;
    Ok(Some(Arc::new(renewer)))
}

/// A random permille from -100 to 100: the spread of the renewal schedule, so that hosts
/// enrolled together do not renew together.
fn jitter() -> i32 {
    use std::hash::BuildHasher as _;
    let draw = std::collections::hash_map::RandomState::new().hash_one(0_u8) % 201;
    i32::try_from(draw).map_or(0, |d| d - 100)
}

/// Unix seconds now.
pub(super) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The drain and the health of a frontend host, which the helper runs as a container
/// (`docs/deploy/host-install.md`, "A frontend container"):
///
/// - the frontend itself is drained by the helper, inside the A/B, through the frontend's own
///   operator API, so the agent drains nothing on its behalf;
/// - the node of a daemon the host also runs is drained and given back only when the agent was
///   told which node it is (`--daemon-node`), by the way [`node_ports`] gives the daemon of a
///   host;
/// - the health is what the helper says runs: its switch returned only after the container
///   answered `/readyz` with 200 and `/healthz` named the release's commit.
pub(super) fn frontend_ports(
    cfg: &Config,
    identity: &Identity,
    local: &Arc<SocketLocal>,
) -> Result<NodePorts, String> {
    let drain = match &cfg.daemon_node {
        Some(_) => node_ports(cfg, identity, Role::Daemon, KBF_SERVICE, local)?.0,
        None => no_drain(),
    };
    let health = Arc::new(ContainerHealth::new(Arc::new(LocalInstaller(
        local.clone(),
    ))));
    Ok((drain, health))
}

/// The kind of host a role names: what the agent has duties for, and nothing else.
pub(super) fn kind_of(role: &HostRole) -> Result<Role, String> {
    match role.name() {
        "frontend" => Ok(Role::Frontend),
        "storage" => Ok(Role::Storage),
        "daemon" => Ok(Role::Daemon),
        "mac-daemon" => Ok(Role::MacDaemon),
        other => Err(format!(
            "{other} is a host role this agent has no duties for"
        )),
    }
}

/// The agent of this host, with the real ports.
///
/// # Errors
/// Why not: no releases for this platform, a certificate that is no updater's, files that do
/// not make a TLS configuration, an operator API client whose token files are not usable, or
/// `--daemon-node` given to an agent whose certificate is not a frontend's.
pub fn build(cfg: &Config) -> Result<Agent, String> {
    let platform = host_platform().ok_or("kbf has no release for this platform")?;
    let identity = identity_of_files(&cfg.files)?;
    let role = kind_of(&identity.role)?;
    if cfg.daemon_node.is_some() && role != Role::Frontend {
        return Err(
            "--daemon-node is for the agent of a frontend host that also runs a \
                    daemon: another host's node is its certificate's"
                .to_owned(),
        );
    }
    super::coordinator::client_config(&cfg.files).map_err(|e| e.to_string())?;
    let node_id = cfg
        .daemon_node
        .clone()
        .unwrap_or_else(|| identity.node_id.to_string());
    let host = Host {
        alias: node_id.clone(),
        address: "localhost".to_owned(),
        role,
        host_key: String::new(),
        node_id: Some(node_id),
        member: None,
    };
    let local = Arc::new(SocketLocal::new(&cfg.helper_socket, Limits::default()));
    let (drain, health) = if role == Role::Frontend {
        frontend_ports(cfg, &identity, &local)?
    } else {
        node_ports(cfg, &identity, host.role, KBF_SERVICE, &local)?
    };
    let renewer = renewer_for(cfg)?;
    let coordinator = Arc::new(
        HttpsCoordinator::new(cfg.coordinator.clone(), cfg.files.clone())
            .bound_to(identity.clone()),
    );
    let world = World {
        coordinator,
        local,
        drain,
        health,
        clock: Arc::new(SystemClock::new()),
        unix_now: Arc::new(unix_now),
        renewer,
        log: Arc::new(|line| eprintln!("kbf-updater: {line}")),
    };
    let unknown = Version::parse("0").map_err(|e| e.to_string())?;
    let services = cfg
        .services
        .iter()
        .map(|service| service_ports(cfg, &identity, &host, service))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Agent::new(
        host,
        cfg.channel.clone(),
        cfg.timing,
        world,
        Status::unknown(platform, unknown),
    )
    .with_services(services))
}

/// The ports of `service`: its own helper socket, the node's drain
/// unless it declared none, and the node's health unless it declared a readiness URL. The
/// frontend is needed only for what the service did not replace.
pub(super) fn service_ports(
    cfg: &Config,
    identity: &Identity,
    host: &Host,
    service: &ServiceConfig,
) -> Result<ServicePorts, String> {
    let local = Arc::new(SocketLocal::new(&service.helper_socket, Limits::default()));
    let node = || node_ports(cfg, identity, host.role, &service.name, &local);
    let drain = match service.drain {
        ServiceDrain::None => no_drain(),
        ServiceDrain::Node => node()?.0,
    };
    let health: Arc<dyn HealthCheck> = match &service.health {
        ServiceHealth::Ready { addr, path } => Arc::new(ReadyUrl::new(*addr, path.clone())),
        ServiceHealth::Node => node()?.1,
    };
    Ok(ServicePorts {
        name: service.name.clone(),
        channel: service.channel.clone(),
        local,
        drain,
        health,
    })
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
