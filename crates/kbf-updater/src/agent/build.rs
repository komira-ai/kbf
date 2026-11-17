//! The real ports of an agent, and the process around it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_enroll::store::Store;
use kbf_types::HostRole;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

use super::container_health::ContainerHealth;
use super::coordinator::{Endpoint, HttpsCoordinator, TlsFiles};
use super::enroll::frontend_url;
use super::enrolled::{self, LOOK_EVERY, Waited, Waiting};
use super::flags::{Config, NodeApi, ServiceConfig, ServiceHealth};
use super::local::{Limits, LocalInstaller, SocketLocal};
use super::pin::PinFile;
use super::renew::{FrontendRenewer, Renewer};
use super::run::{Agent, ServicePorts, Stop, World};
use super::service_health::ReadyUrl;
use super::status::{Status, host_platform};
use crate::identity::{Identity, identity_of};
use crate::inventory::{Host, Role};
use crate::node_health::NodeHealth;
use crate::operator::OperatorApi;
use crate::ports::{Drain, Health, HealthCheck, Placed, PortError, SystemClock};
use crate::version::Version;

/// The drain of every host: nothing. No kbf node runs a daemon any more, so there is no node to
/// take out of placement and none to give back (a frontend's own drain is the helper's).
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
/// update before it fetches anything. The text says why.
pub struct NoCredential(pub &'static str);

/// Why an agent given no way to the frontend refuses.
pub const NO_CREDENTIAL: &str = "this agent was given neither --frontend (to read its node's health \
     with its own certificate) nor an operator API credential, so it cannot judge its node's \
     health and updates nothing";

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

/// What a process needs to reach its node at the frontend: the settings of the agent (or of
/// the update hook, which is given the same flags) that say how the health is asked.
#[derive(Clone, Debug)]
pub struct Access {
    /// This host's updater certificate, its key and the farm CA.
    pub files: TlsFiles,
    /// The frontend's worker listener.
    pub frontend: Option<Endpoint>,
    /// The opt-in operator API and its token.
    pub node_api: Option<NodeApi>,
}

impl Access {
    /// The access an agent's settings give.
    pub fn of(cfg: &Config) -> Access {
        Access {
            files: cfg.files.clone(),
            frontend: cfg.frontend.clone(),
            node_api: cfg.node_api.clone(),
        }
    }
}

/// The health of the host `identity` names, by the way the agent was given to the frontend:
///
/// - the operator token (`--operator-api`, `--token-file`), the opt-in fallback, if it was given;
/// - else the host's own certificate on the frontend's worker listener (`--frontend`): the
///   storage host's own storage view ([`NodeHealth::for_updater`]);
/// - else the refusals of [`NoCredential`].
pub(super) fn node_health(
    cfg: &Config,
    identity: &Identity,
) -> Result<Arc<dyn HealthCheck>, String> {
    node_health_for(&Access::of(cfg), identity)
}

/// [`node_health`], from the access alone.
pub(super) fn node_health_for(
    access: &Access,
    identity: &Identity,
) -> Result<Arc<dyn HealthCheck>, String> {
    if let Some(api) = &access.node_api {
        let api = OperatorApi::new(api.addr, api.token_file.clone())?;
        return Ok(Arc::new(NodeHealth::with_api(api)));
    }
    let Some(frontend) = &access.frontend else {
        return Ok(Arc::new(NoCredential(NO_CREDENTIAL)));
    };
    let api = OperatorApi::as_updater(frontend.clone(), access.files.clone(), identity.clone());
    Ok(Arc::new(NodeHealth::for_updater(api)))
}

/// Where the agent keeps its deferral and halt records: `--cordon-dir`, else the state
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

/// The health of a frontend host, which the helper runs as a container
/// (`docs/deploy/host-install.md`, "A frontend container"): what the helper says runs. Its switch
/// returned only after the container answered `/readyz` with 200 and `/healthz` named the
/// release's commit. The frontend itself is drained by the helper, inside the A/B, through the
/// frontend's own operator API.
pub(super) fn frontend_health(local: &Arc<SocketLocal>) -> Arc<dyn HealthCheck> {
    Arc::new(ContainerHealth::new(Arc::new(LocalInstaller(
        local.clone(),
    ))))
}

/// The kind of host a role names: what the agent has duties for, and nothing else. The roles
/// `daemon` and `mac-daemon` are read (a certificate of an earlier release names one) and have
/// no duties: no kbf daemon runs on the host.
pub(super) fn kind_of(role: &HostRole) -> Result<Role, String> {
    match role.name() {
        "frontend" => Ok(Role::Frontend),
        "storage" => Ok(Role::Storage),
        other => Err(format!(
            "{other} is a host role this agent has no duties for"
        )),
    }
}

/// The host a certificate names, as the frontend knows its node: the certificate's own node id.
///
/// # Errors
/// A role this updater has no duties for.
pub(super) fn this_host(identity: &Identity) -> Result<Host, String> {
    let role = kind_of(&identity.role)?;
    let node_id = identity.node_id.to_string();
    Ok(Host {
        alias: node_id.clone(),
        address: "localhost".to_owned(),
        role,
        host_key: String::new(),
        node_id: Some(node_id),
        member: None,
    })
}

/// The agent of this host, with the real ports.
///
/// # Errors
/// Why not: no releases for this platform, a certificate that is no updater's, files that do
/// not make a TLS configuration, or an operator API client whose token files are not usable.
pub fn build(cfg: &Config) -> Result<Agent, String> {
    let platform = host_platform().ok_or("kbf has no release for this platform")?;
    let identity = identity_of_files(&cfg.files)?;
    let host = this_host(&identity)?;
    super::coordinator::client_config(&cfg.files).map_err(|e| e.to_string())?;
    let local = Arc::new(SocketLocal::new(&cfg.helper_socket, Limits::default()));
    let health = if host.role == Role::Frontend {
        frontend_health(&local)
    } else {
        node_health(cfg, &identity)?
    };
    let renewer = renewer_for(cfg)?;
    let coordinator = Arc::new(
        HttpsCoordinator::new(cfg.coordinator.clone(), cfg.files.clone())
            .bound_to(identity.clone()),
    );
    let world = World {
        coordinator,
        local,
        drain: no_drain(),
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
        .map(|service| service_ports(cfg, &identity, service))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Agent::new(
        host,
        cfg.channel.clone(),
        cfg.timing,
        world,
        Status::unknown(platform, unknown),
    )
    .with_services(services)
    .with_host_updates(cordon_dir(cfg))
    .with_pin(PinFile::helper()))
}

/// The ports of `service`: its own helper socket, and the node's health unless it declared a
/// readiness URL. The frontend is needed only for what the service did not replace.
pub(super) fn service_ports(
    cfg: &Config,
    identity: &Identity,
    service: &ServiceConfig,
) -> Result<ServicePorts, String> {
    let local = Arc::new(SocketLocal::new(&service.helper_socket, Limits::default()));
    let health: Arc<dyn HealthCheck> = match &service.health {
        ServiceHealth::Ready { addr, path } => Arc::new(ReadyUrl::new(*addr, path.clone())),
        ServiceHealth::Node => node_health(cfg, identity)?,
    };
    Ok(ServicePorts {
        name: service.name.clone(),
        channel: service.channel.clone(),
        local,
        drain: no_drain(),
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

/// [`run_config_waiting`] with the real ports ([`build`]), napping [`LOOK_EVERY`] between two
/// looks at an enrollment that is not there yet and saying so on the error stream.
pub fn run_config(cfg: &Config, ready: impl FnOnce(Stop)) -> (u8, String) {
    let log = |line: &str| eprintln!("kbf-updater: {line}");
    let nap = |stop: &Stop| super::run::sleep(LOOK_EVERY, stop);
    run_config_waiting(
        cfg,
        ready,
        &Waiting {
            nap: &nap,
            log: &log,
        },
        build,
    )
}

/// The agent of `cfg`, run until the handle given to `ready` stops it. An agent given
/// `--state-dir` first waits there for the host's enrollment ([`enrolled::wait`]); then `make`
/// makes the agent from `cfg` ([`build`] in the process: the ports are made only now, so nothing
/// is dialled before the host is enrolled), and the agent runs. The exit code and text of the
/// process: 0 once stopped, 1 with the reason when the enrollment cannot be read or the agent
/// cannot be made.
pub(crate) fn run_config_waiting(
    cfg: &Config,
    ready: impl FnOnce(Stop),
    waiting: &Waiting,
    make: impl FnOnce(&Config) -> Result<Agent, String>,
) -> (u8, String) {
    let stop = Stop::default();
    ready(stop.clone());
    if let Some(dir) = &cfg.state_dir {
        match enrolled::wait(dir, &stop, waiting) {
            Err(why) => return (1, format!("kbf-updater: {why}\n")),
            Ok(Waited::Stopped) => return (0, String::new()),
            Ok(Waited::Enrolled) => {}
        }
    }
    match make(cfg) {
        Err(why) => (1, format!("kbf-updater: {why}\n")),
        Ok(agent) => {
            agent.run(&stop);
            (0, String::new())
        }
    }
}
