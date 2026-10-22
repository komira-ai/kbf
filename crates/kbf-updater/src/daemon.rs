//! The binary's wiring: flags, the ports, the listener and the thread that drives rolls.
//!
//! Every setting is a flag (configuration is flags, not environment). Required:
//!
//! - `--state-dir`: where `roll.json` lives (`RollStore` takes an exclusive lock on it);
//! - `--inventory`: the root-owned host file;
//! - `--proxy-uid`: the one uid whose connections are served;
//! - `--operator-api`: `http://<loopback ip>:<port>`, the frontend's operator API, for
//!   safe-to-stop and the health of storage and daemon hosts; a host that is not a loopback
//!   address is refused;
//! - `--token-file`: the file holding the operator bearer token (the proxy takes the same
//!   token on its swap status);
//! - `--proxy-api`: `https://<loopback ip>:<port>`, kbf-proxy's public port on this host,
//!   which serves `GET /v1/proxy/swap` for the health of the frontend; the swap status is
//!   not on the operator API, because the proxy answers it itself;
//! - `--proxy-ca-file`: the PEM file of the farm CA that vouches for the proxy's certificate.
//!
//! Optional: `--operator-proxy-auth-file`, the file holding the proxy's per-boot secret,
//! for a frontend that runs behind kbf-proxy (`--proxy-auth-file` there): its operator API
//! refuses a call without it. It is read on every call, like the token. And
//! `--proxy-server-name`, the DNS name the proxy's certificate carries (the IP of
//! `--proxy-api` when absent).
//!
//! And the socket, one of two (never both):
//!
//! - **production**: socket activation. systemd binds the socket (`SocketUser=<proxy user>`,
//!   `SocketMode=0600`) and passes it as fd 3 with `LISTEN_PID` and `LISTEN_FDS=1`
//!   ([`crate::listener::Activation`]); the updater serves it as it is;
//! - **tests and development**: `--socket`, a path the updater binds itself, mode 0600, its
//!   own user.
//!
//! This build has no host installer ([`crate::not_built`]): a roll is accepted and fails at
//! the first install, with no host touched. The health checks are real ([`crate::health`]).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread::JoinHandle;

use serde_json::json;

use crate::api::RollState;
use crate::frontend_health::FrontendHealth;
use crate::health::HostHealth;
use crate::https::{ProxyApi, ProxyTls};
use crate::inventory::Inventory;
use crate::listener::{Activation, Listen, open};
use crate::node_health::NodeHealth;
use crate::not_built::NotBuiltInstaller;
use crate::ports::SystemClock;
use crate::record::RollStore;
use crate::safe_http::HttpSafeToStop;
use crate::server::{AuditSink, Recent, Server, Settings};
use crate::{Ports, Timing, Updater};

pub const USAGE: &str = "usage: kbf-updater --version\n       kbf-updater --state-dir DIR \
--inventory FILE --proxy-uid UID --operator-api http://IP:PORT --token-file FILE \
--proxy-api https://IP:PORT --proxy-ca-file FILE [--operator-proxy-auth-file FILE] \
[--proxy-server-name NAME] [--socket PATH]\n\
the socket comes from systemd socket activation (LISTEN_PID, LISTEN_FDS=1) or from --socket, \
which is for tests and development\n";

const FLAGS: [&str; 10] = [
    "--state-dir",
    "--inventory",
    "--socket",
    "--proxy-uid",
    "--operator-api",
    "--token-file",
    "--operator-proxy-auth-file",
    "--proxy-api",
    "--proxy-ca-file",
    "--proxy-server-name",
];

/// The settings of a run.
#[derive(Clone, Debug)]
pub struct Config {
    pub state_dir: PathBuf,
    pub inventory: PathBuf,
    /// The uid that must own the inventory: root, for the binary. A test says otherwise.
    pub inventory_owner: u32,
    pub listen: Listen,
    pub proxy_uid: u32,
    pub operator_api: SocketAddr,
    pub token_file: PathBuf,
    /// The proxy's per-boot secret, if the frontend is behind kbf-proxy.
    pub operator_proxy_auth_file: Option<PathBuf>,
    /// Where kbf-proxy is and how to trust it, for the frontend's health.
    pub proxy: ProxyTls,
}

/// The settings from the command line (without the program name) and from what the service
/// manager passed. Each flag is given once, as `--flag value` or `--flag=value`; all but
/// `--socket`, `--operator-proxy-auth-file` and `--proxy-server-name` are required, and the socket is either
/// `--socket` or the activated descriptor.
pub fn parse_flags(args: &[String], activation: &Activation) -> Result<Config, String> {
    let mut found: BTreeMap<&str, String> = BTreeMap::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (arg.as_str(), None),
        };
        let Some(&flag) = FLAGS.iter().find(|f| **f == name) else {
            return Err(format!("unknown argument {name}"));
        };
        let value = match inline {
            Some(value) => value,
            None => rest
                .next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))?,
        };
        if found.insert(flag, value).is_some() {
            return Err(format!("{flag} is given twice"));
        }
    }
    let mut take = |flag: &str| {
        found
            .remove(flag)
            .ok_or_else(|| format!("{flag} is required"))
    };
    let state_dir = PathBuf::from(take("--state-dir")?);
    let inventory = PathBuf::from(take("--inventory")?);
    let proxy_uid = take("--proxy-uid")?
        .parse::<u32>()
        .map_err(|_| "--proxy-uid must be a number".to_owned())?;
    let operator_api = parse_operator_url(&take("--operator-api")?)?;
    let token_file = PathBuf::from(take("--token-file")?);
    let proxy_addr = parse_proxy_url(&take("--proxy-api")?)?;
    let proxy_ca_file = PathBuf::from(take("--proxy-ca-file")?);
    let proxy_server_name = found.remove("--proxy-server-name");
    let operator_proxy_auth_file = found
        .remove("--operator-proxy-auth-file")
        .map(PathBuf::from);
    let listen = match (
        found.remove("--socket"),
        activation.passed_to(std::process::id())?,
    ) {
        (Some(path), None) => Listen::Bind(PathBuf::from(path)),
        (None, Some(fd)) => Listen::Activated(fd),
        (Some(_), Some(_)) => {
            return Err(
                "--socket is given and systemd passed a socket (LISTEN_FDS): use one".to_owned(),
            );
        }
        (None, None) => {
            return Err(
                "--socket is required unless systemd passes the socket (LISTEN_PID, LISTEN_FDS)"
                    .to_owned(),
            );
        }
    };
    Ok(Config {
        state_dir,
        inventory,
        inventory_owner: 0,
        listen,
        proxy_uid,
        operator_api,
        token_file,
        operator_proxy_auth_file,
        proxy: ProxyTls {
            addr: proxy_addr,
            ca_file: proxy_ca_file,
            server_name: proxy_server_name,
        },
    })
}

/// `http://<ip>:<port>` with a loopback ip. The operator token goes over this connection in
/// clear, so any other host, a name, or `https` is refused.
pub fn parse_operator_url(url: &str) -> Result<SocketAddr, String> {
    loopback(
        url,
        "http://",
        "--operator-api",
        " (the token travels in clear, on loopback)",
    )
}

/// `https://<ip>:<port>` with a loopback ip: kbf-proxy's public port on this host. The
/// updater runs beside the proxy, and a config naming another host is a mistake to stop.
pub fn parse_proxy_url(url: &str) -> Result<SocketAddr, String> {
    loopback(url, "https://", "--proxy-api", "")
}

fn loopback(url: &str, scheme: &str, flag: &str, why: &str) -> Result<SocketAddr, String> {
    let rest = url
        .strip_prefix(scheme)
        .ok_or_else(|| format!("{flag} must start with {scheme}{why}"))?;
    let addr: SocketAddr = rest
        .trim_end_matches('/')
        .parse()
        .map_err(|_| format!("{flag} must be {scheme}<ip>:<port>, with an IP address"))?;
    if !addr.ip().to_canonical().is_loopback() {
        return Err(format!("{flag} must be a loopback address"));
    }
    Ok(addr)
}

/// Stops a running [`Daemon`] from another thread.
#[derive(Clone)]
pub struct Stopper {
    flag: Arc<AtomicBool>,
    socket: PathBuf,
    /// Whether the updater made the socket file, and so removes it.
    owned: bool,
}

impl Stopper {
    /// Ask the accept loop to end: set the flag and make one connection to wake it. (An
    /// activated socket that only the proxy's user may connect to cannot be woken by the
    /// updater's user; the process then ends by signal, as a service does.)
    pub fn stop(&self) {
        self.flag.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket);
    }
}

/// A running updater: the listener thread and the thread that drives rolls.
pub struct Daemon {
    stopper: Stopper,
    accept: JoinHandle<()>,
    worker: JoinHandle<()>,
}

impl Daemon {
    pub fn stopper(&self) -> Stopper {
        self.stopper.clone()
    }

    /// Block until the listener has been stopped and the roll in hand (if any) is over;
    /// remove the socket if the updater made it.
    pub fn wait(self) {
        let _ = self.accept.join();
        let _ = self.worker.join();
        if self.stopper.owned {
            let _ = std::fs::remove_file(&self.stopper.socket);
        }
    }
}

/// An audit sink that writes each line to stderr, where the journal takes it.
pub fn stderr_sink() -> AuditSink {
    Arc::new(|line| eprintln!("{line}"))
}

/// The ports of this build: the real safe-to-stop, health checks and clock, and the refusing
/// installer.
pub fn build_ports(cfg: &Config) -> Result<Ports, String> {
    let proxy = ProxyApi::new(&cfg.proxy, cfg.token_file.clone()).map_err(|e| e.to_string())?;
    let nodes = NodeHealth::new(
        cfg.operator_api,
        cfg.token_file.clone(),
        cfg.operator_proxy_auth_file.clone(),
    )?;
    Ok(Ports {
        safe: Box::new(HttpSafeToStop::new(
            cfg.operator_api,
            cfg.token_file.clone(),
            cfg.operator_proxy_auth_file.clone(),
        )?),
        installer: Box::new(NotBuiltInstaller),
        health: Box::new(HostHealth::new(FrontendHealth::new(proxy), nodes)),
        clock: Box::new(SystemClock::new()),
    })
}

/// Start with the ports of this build.
pub fn start(cfg: &Config) -> Result<Daemon, String> {
    start_with(
        cfg,
        build_ports(cfg)?,
        Timing::default(),
        Settings::new(cfg.proxy_uid),
        stderr_sink(),
    )
}

/// [`start`], then `ready` with a handle that stops it, then block until it is stopped. The
/// exit code and text of the process.
pub fn run_config(cfg: &Config, ready: impl FnOnce(Stopper)) -> (u8, String) {
    match start(cfg) {
        Err(why) => (1, format!("kbf-updater: {why}\n")),
        Ok(running) => {
            ready(running.stopper());
            running.wait();
            (0, String::new())
        }
    }
}

/// Open the store and the inventory, bind the socket, start the threads. A roll found
/// `running` is resumed.
pub fn start_with(
    cfg: &Config,
    ports: Ports,
    timing: Timing,
    settings: Settings,
    audit: AuditSink,
) -> Result<Daemon, String> {
    let inventory = Inventory::load(&cfg.inventory, cfg.inventory_owner)
        .map_err(|e| format!("inventory {}: {e}", cfg.inventory.display()))?;
    let store = RollStore::open(&cfg.state_dir)
        .map_err(|e| format!("state dir {}: {e}", cfg.state_dir.display()))?;
    let updater = Updater::open(inventory, store, ports, timing)
        .map_err(|e| format!("roll record in {}: {e}", cfg.state_dir.display()))?;
    let updater = Arc::new(updater);
    let opened = open(&cfg.listen)?;
    let (drive, queue) = channel();
    if let Ok(Some(roll)) = updater.current()
        && roll.state == RollState::Running
    {
        let _ = drive.send(roll.id);
    }
    let recent = Arc::new(Recent::default());
    let worker = {
        let (updater, recent, audit) = (Arc::clone(&updater), Arc::clone(&recent), audit.clone());
        std::thread::spawn(move || drive_loop(&updater, queue, &recent, &audit))
    };
    let server = Arc::new(Server::new(updater, settings, audit, drive, recent));
    let stopper = Stopper {
        flag: Arc::new(AtomicBool::new(false)),
        socket: opened.path,
        owned: opened.owned,
    };
    let flag = Arc::clone(&stopper.flag);
    let accept = std::thread::spawn(move || {
        let listener: UnixListener = opened.listener;
        server.serve(&listener, &flag);
    });
    Ok(Daemon {
        stopper,
        accept,
        worker,
    })
}

/// Run each queued roll to its end, one after the other (the updater allows one at a time).
pub(crate) fn drive_loop(
    updater: &Updater,
    queue: Receiver<String>,
    recent: &Recent,
    audit: &AuditSink,
) {
    for id in queue {
        let line = match updater.drive(&id) {
            Ok(roll) => {
                recent.push(&roll);
                json!({"event": "roll_finished", "roll_id": roll.id, "state": roll.state,
                       "failure": roll.failure})
            }
            Err(e) => json!({"event": "roll_not_driven", "roll_id": id, "error": e.to_string()}),
        };
        audit(&line.to_string());
    }
}
