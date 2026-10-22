//! The binary's wiring: flags, the ports, the listener and the thread that drives rolls.
//!
//! Every setting is a flag (configuration is flags, not environment), all required:
//!
//! - `--state-dir`: where `roll.json` lives (`RollStore` takes an exclusive lock on it);
//! - `--inventory`: the root-owned host file;
//! - `--socket`: where to bind the API (mode 0600);
//! - `--proxy-uid`: the one uid whose connections are served;
//! - `--operator-api`: `http://<loopback ip>:<port>`, the frontend's operator API, for
//!   safe-to-stop; a host that is not a loopback address is refused;
//! - `--token-file`: the file holding the operator bearer token.
//!
//! This build has no host installer and no health check ([`crate::not_built`]): a roll is
//! accepted and fails at the first install, with no host touched.

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
use crate::inventory::Inventory;
use crate::not_built::{NotBuiltHealth, NotBuiltInstaller};
use crate::ports::SystemClock;
use crate::record::RollStore;
use crate::safe_http::HttpSafeToStop;
use crate::server::{AuditSink, Recent, Server, Settings, bind_socket};
use crate::{Ports, Timing, Updater};

pub const USAGE: &str = "usage: kbf-updater --version\n       kbf-updater --state-dir DIR \
--inventory FILE --socket PATH --proxy-uid UID --operator-api http://IP:PORT --token-file FILE\n";

const FLAGS: [&str; 6] = [
    "--state-dir",
    "--inventory",
    "--socket",
    "--proxy-uid",
    "--operator-api",
    "--token-file",
];

/// The settings of a run.
#[derive(Clone, Debug)]
pub struct Config {
    pub state_dir: PathBuf,
    pub inventory: PathBuf,
    /// The uid that must own the inventory: root, for the binary. A test says otherwise.
    pub inventory_owner: u32,
    pub socket: PathBuf,
    pub proxy_uid: u32,
    pub operator_api: SocketAddr,
    pub token_file: PathBuf,
}

/// The settings from the command line (without the program name). All six flags are
/// required, each once, as `--flag value` or `--flag=value`.
pub fn parse_flags(args: &[String]) -> Result<Config, String> {
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
    let socket = PathBuf::from(take("--socket")?);
    let proxy_uid = take("--proxy-uid")?
        .parse::<u32>()
        .map_err(|_| "--proxy-uid must be a number".to_owned())?;
    let operator_api = parse_operator_url(&take("--operator-api")?)?;
    let token_file = PathBuf::from(take("--token-file")?);
    Ok(Config {
        state_dir,
        inventory,
        inventory_owner: 0,
        socket,
        proxy_uid,
        operator_api,
        token_file,
    })
}

/// `http://<ip>:<port>` with a loopback ip. The operator token goes over this connection in
/// clear, so any other host, a name, or `https` is refused.
pub fn parse_operator_url(url: &str) -> Result<SocketAddr, String> {
    let rest = url.strip_prefix("http://").ok_or(
        "--operator-api must start with http:// (the token travels in clear, on loopback)",
    )?;
    let addr: SocketAddr = rest
        .trim_end_matches('/')
        .parse()
        .map_err(|_| "--operator-api must be http://<ip>:<port>, with an IP address".to_owned())?;
    if !addr.ip().to_canonical().is_loopback() {
        return Err("--operator-api must be a loopback address".to_owned());
    }
    Ok(addr)
}

/// Stops a running [`Daemon`] from another thread.
#[derive(Clone)]
pub struct Stopper {
    flag: Arc<AtomicBool>,
    socket: PathBuf,
}

impl Stopper {
    /// Ask the accept loop to end: set the flag and make one connection to wake it.
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
    /// remove the socket.
    pub fn wait(self) {
        let _ = self.accept.join();
        let _ = self.worker.join();
        let _ = std::fs::remove_file(&self.stopper.socket);
    }
}

/// An audit sink that writes each line to stderr, where the journal takes it.
pub fn stderr_sink() -> AuditSink {
    Arc::new(|line| eprintln!("{line}"))
}

/// The ports of this build: the real safe-to-stop and clock, and the refusing installer and
/// health check.
pub fn build_ports(cfg: &Config) -> Result<Ports, String> {
    Ok(Ports {
        safe: Box::new(HttpSafeToStop::new(
            cfg.operator_api,
            cfg.token_file.clone(),
        )?),
        installer: Box::new(NotBuiltInstaller),
        health: Box::new(NotBuiltHealth),
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
    let listener =
        bind_socket(&cfg.socket).map_err(|e| format!("socket {}: {e}", cfg.socket.display()))?;
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
        socket: cfg.socket.clone(),
    };
    let flag = Arc::clone(&stopper.flag);
    let accept = std::thread::spawn(move || {
        let listener: UnixListener = listener;
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
