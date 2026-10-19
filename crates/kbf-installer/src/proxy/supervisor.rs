//! What the proxy does with its children when it starts (Linux): adopt the frontends a
//! previous proxy left running, or leave the startup hold in force and say why.
//!
//! A starting proxy holds every new stream (`kbf-proxy`'s startup hold, bounded by
//! `hold_max_s`); this ends that hold into the child that serves, or does not end it. It
//! ends it only on evidence it can check without the child's admin port: exactly one slot
//! has a recorded child that is still the recorded process, and no update is in flight. Any
//! other state is left held: the proxy then answers `UNAVAILABLE` after the bound and
//! `/readyz` 503, which is visible, instead of routing to a child whose role nobody has
//! read. Not done here, because each needs a port that is not built yet (the child's admin
//! port, U2; the root half's orders, U8b): resuming an update that was in flight, choosing
//! between two live children, and starting a first child on a host that has none.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::child_real::{Argv, ProcChild};
use kbf_proxy::config::UpstreamTable;
use kbf_proxy::route::Target;

use super::gate_real::{ProxyGate, gate_slot};
use super::model::{Slot, Start};
use super::ports::Child as _;
use super::record::FileRecorder;

/// How the start ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Startup {
    /// `active` serves; the startup hold ended into it.
    Adopted { active: Slot },
    /// The startup hold is still in force, for this reason.
    Held(String),
}

/// Where the children's identities are kept: under the proxy's state directory.
pub fn children_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("children")
}

/// The command line of a frontend child after its program: its mode, the four loopback
/// listeners at the slot's ports in `table` (`kbf-proxy`'s upstream table: REAPI, worker,
/// operator and admin), the proxy's secret file, and `--await-takeover` for a candidate. The
/// child's own configuration (stores, TLS) is its `kbf.toml`. Nothing here names an
/// instance: `--instance` needs a handover socket, and the two slots do not hand over.
pub fn frontend_argv(proxy_auth_file: &Path, table: UpstreamTable) -> Argv {
    let secret = proxy_auth_file.to_owned();
    Box::new(move |slot, start| {
        let slot = gate_slot(slot);
        let mut proxy_auth = OsString::from("--proxy-auth-file=");
        proxy_auth.push(&secret);
        let listen = |flag: &str, port: u16| OsString::from(format!("--{flag}=127.0.0.1:{port}"));
        let mut argv = vec![
            OsString::from("--mode=frontend"),
            listen("listen", table.port(slot, Target::Reapi)),
            listen("worker-listen", table.port(slot, Target::Worker)),
            listen("api-listen", table.port(slot, Target::Operator)),
            listen("admin-listen", table.admin_port(slot)),
            proxy_auth,
        ];
        if start == Start::Candidate {
            argv.push(OsString::from("--await-takeover"));
        }
        argv
    })
}

/// Adopt what a previous proxy left. `child` reads the identities `spawn` recorded; the
/// update record decides whether anything is in flight.
pub fn adopt_children(gate: &ProxyGate, child: &ProcChild, state_dir: &Path) -> Startup {
    match FileRecorder::load(state_dir) {
        Err(e) => return Startup::Held(format!("{e}; both slots are left as they are")),
        Ok(Some(intent)) if !intent.phase.is_terminal() => {
            return Startup::Held(format!(
                "an update is in flight (phase {:?}); resuming it needs the child's admin port",
                intent.phase
            ));
        }
        Ok(_) => {}
    }
    let live: Vec<Slot> = [Slot::A, Slot::B]
        .into_iter()
        .filter(|slot| {
            child
                .recorded(*slot)
                .is_some_and(|recorded| child.adopt(*slot, &recorded.ident).is_ok())
        })
        .collect();
    match live.as_slice() {
        [] => Startup::Held(
            "no frontend child is running; starting one needs the root half's status.json".into(),
        ),
        [active] => match gate.adopt(*active, None) {
            Ok(()) => Startup::Adopted { active: *active },
            Err(e) => Startup::Held(e.to_string()),
        },
        _ => Startup::Held(
            "both slots have a live child and no update is in flight; which one serves is the \
             child's role, which needs its admin port"
                .into(),
        ),
    }
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;
