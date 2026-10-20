//! What the proxy does with its children when it starts (Linux): adopt the frontends a
//! previous proxy left running, start the installed release where none runs, or leave the
//! startup hold in force and say why.
//!
//! A starting proxy holds every new stream (`kbf-proxy`'s startup hold, bounded by
//! `hold_max_s`); this ends that hold into the child that serves, or does not end it. With
//! no update in flight:
//!
//! - **One slot has a live recorded child** (still the recorded process): its `role` is
//!   read. An active child is adopted. A candidate (the proxy stopped between starting it
//!   and its takeover) is waited for, made to take the generation, and then routed to.
//! - **No slot has one**: the root half's `installed_commit` (`status.json`) is started in
//!   slot A as a candidate, made to take the generation (a cold start with an epoch bump,
//!   FP sections 6 and 8: [`super::cold`], the same steps as a restart in the middle of an
//!   update) and routed to. If `status.json` names no release, or its binary is missing or
//!   not executable, nothing is started: no other release is picked.
//! - **Both slots have one**: left held; which one serves is the child's role to say.
//!
//! Anything else is left held: the proxy then answers `UNAVAILABLE` after the bound and
//! `/readyz` 503, which is visible, instead of routing to a child whose role nobody has
//! read. The reason is returned and the verb writes it to the standard error. Not done
//! here: resuming an update that was in flight (the root half's orders, U8b), and choosing
//! between two live children.

use std::ffi::OsString;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use super::child_real::{Argv, ProcChild};
use kbf_proxy::config::UpstreamTable;
use kbf_proxy::route::Target;

use super::admin::{Admin, Deadlines};
use super::boot::{Booted, ColdBoot, Reason, Report, Retry, Source};
use super::child_http::Headers;
use super::cold::{Cold, ReadyRule};
use super::gate_real::{ProxyGate, gate_slot};
use super::model::{PortError, ProcIdent, RoleState, Slot, Start, Timing};
use super::ports::{Child as _, ChildAdmin, Clock, Migrator};
use super::record::FileRecorder;
use super::root_orders::installed_commit;
use crate::CommitId;

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

/// The admin and operator calls on the frontend children, at the loopback ports `table` gives
/// each slot, carrying the proxy's secret. `token_file` is the operator token, read on every
/// `post_desired` (the boot calls none).
///
/// # Errors
/// The proxy's secret is not a legal header value (never, for the 64 hex digits it is).
pub fn child_admin(
    handle: &kbf_proxy::ProxyHandle,
    table: UpstreamTable,
    token_file: PathBuf,
) -> Result<Admin, String> {
    let loopback = |port: u16| SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let admin_headers = handle.admin_headers();
    let pairs: Vec<(&str, &[u8])> = admin_headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_bytes()))
        .collect();
    secret_headers(&pairs).map(|secret| {
        Admin::new(
            move |slot| loopback(table.admin_port(gate_slot(slot))),
            move |slot| loopback(table.port(gate_slot(slot), Target::Operator)),
            secret,
            move || std::fs::read(&token_file),
            Deadlines::frontend(),
        )
    })
}

/// The proxy's secret headers as the child client sends them.
///
/// # Errors
/// A header value the client refuses to send (never, for the 64 hex digits the proxy's secret is).
fn secret_headers(pairs: &[(&str, &[u8])]) -> Result<Headers, String> {
    pairs
        .iter()
        .try_fold(Headers::new(), |headers, &(name, value)| {
            headers.with(name, value)
        })
        .map_err(|e| format!("the proxy secret is not a header value: {e}"))
}

/// What the start needs beyond the children and the gate: the child's admin calls, the
/// clock, the root half's `status.json`, the migration hook and the limits of a cold start.
pub struct Boot<'a> {
    pub admin: &'a dyn ChildAdmin,
    pub clock: &'a dyn Clock,
    /// Runs the release's forward migration before its child starts. The verb passes
    /// [`NoMigration`] until the installer's migrator is wired in.
    pub migrator: &'a dyn Migrator,
    pub report: &'a dyn Report,
    pub status: &'a Path,
    pub timing: Timing,
    pub retry: Retry,
}

/// The migration hook of a proxy whose installer migrator is not wired in: it migrates
/// nothing, and says so in the log. The child still opens the store itself, as a candidate,
/// and a store it cannot open stops it before it takes the generation.
pub struct NoMigration;

impl Migrator for NoMigration {
    fn migrate_forward(&self, _release: &CommitId) -> Result<(), PortError> {
        Ok(())
    }
}

/// Adopt what a previous proxy left, or start the installed release. `child` reads the
/// identities `spawn` recorded; the update record decides whether anything is in flight.
pub fn adopt_children(
    gate: &ProxyGate,
    child: &ProcChild,
    state_dir: &Path,
    boot: &Boot<'_>,
) -> Startup {
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
    let live: Vec<(Slot, ProcIdent)> = [Slot::A, Slot::B]
        .into_iter()
        .filter_map(|slot| {
            let recorded = child.recorded(slot)?;
            child.adopt(slot, &recorded.ident).ok()?;
            Some((slot, recorded.ident))
        })
        .collect();
    let cold = Cold {
        child,
        admin: boot.admin,
        gate,
        clock: boot.clock,
        timing: boot.timing,
        ready: ReadyRule::Readyz,
    };
    match live.as_slice() {
        [] => start_installed(gate, child, &cold, boot),
        [(slot, ident)] => adopt_one(gate, &cold, *slot, ident),
        _ => Startup::Held(
            "both slots have a live child and no update is in flight; which one serves is the \
             child's role, which needs its admin port"
                .into(),
        ),
    }
}

/// The one live child: an active one serves, so the startup hold ends into it; a candidate
/// was started and never took the generation, so it does that first.
fn adopt_one(gate: &ProxyGate, cold: &Cold<'_>, slot: Slot, ident: &ProcIdent) -> Startup {
    let role = match cold.await_role(slot, ident) {
        Ok(role) => role,
        Err(e) => {
            return Startup::Held(format!(
                "the live child in slot {slot:?} cannot be adopted: {e}"
            ));
        }
    };
    if role.state != RoleState::Candidate {
        return match gate.adopt(slot, None) {
            Ok(()) => Startup::Adopted { active: slot },
            Err(e) => Startup::Held(e.to_string()),
        };
    }
    if let Err(e) = gate.keep_startup_hold() {
        return Startup::Held(e.to_string());
    }
    take_generation(cold, slot, ident, "the recorded candidate")
}

/// No child runs: start the release `status.json` calls installed, with the retries of
/// [`ColdBoot`].
fn start_installed(
    gate: &ProxyGate,
    child: &ProcChild,
    cold: &Cold<'_>,
    boot: &Boot<'_>,
) -> Startup {
    let source = HostSource {
        status: boot.status,
        child,
    };
    let keep = || gate.keep_startup_hold().is_ok();
    let run = ColdBoot {
        cold: *cold,
        source: &source,
        migrator: boot.migrator,
        report: boot.report,
        retry: boot.retry,
        hold: &keep,
    };
    match run.run() {
        Booted::Serving(slot) => Startup::Adopted { active: slot },
        Booted::GaveUp(reason) => Startup::Held(format!(
            "no frontend child is running and the installed release did not start: {}",
            reason.word()
        )),
        Booted::Stopped => Startup::Held("the proxy is shutting down".into()),
    }
}

/// [`Source`] on the host: `status.json` and the release directories.
struct HostSource<'a> {
    status: &'a Path,
    child: &'a ProcChild,
}

impl Source for HostSource<'_> {
    fn installed(&self) -> Result<Option<CommitId>, PortError> {
        installed_commit(self.status)
    }

    fn runnable(&self, release: &CommitId) -> Result<(), Reason> {
        let program = self.child.program(release);
        let file = |path: &Path| std::fs::symlink_metadata(path).ok().filter(|m| m.is_file());
        // The binary is what exec runs, so a link is followed; the marker is a plain file.
        if std::fs::metadata(&program)
            .ok()
            .filter(|m| m.is_file())
            .is_none()
        {
            return Err(Reason::BinaryMissing);
        }
        // Executable by the user the child runs as (the proxy's own): the kernel's answer for
        // this very path, which is what the spawn needs, not whether some execute bit is set.
        if rustix::fs::accessat(
            rustix::fs::CWD,
            &program,
            rustix::fs::Access::EXEC_OK,
            rustix::fs::AtFlags::EACCESS,
        )
        .is_err()
        {
            return Err(Reason::BinaryNotExecutable);
        }
        let marker = program
            .parent()
            .map(|dir| dir.join(crate::stage::VERIFIED))
            .and_then(|path| file(&path));
        marker.map(drop).ok_or(Reason::NotVerified)
    }
}

/// Wait for the candidate in `slot`, make it take the generation and route to it.
fn take_generation(cold: &Cold<'_>, slot: Slot, ident: &ProcIdent, what: &str) -> Startup {
    match cold
        .await_ready(slot, ident)
        .and_then(|()| cold.take_over(slot))
    {
        Ok(_) => Startup::Adopted { active: slot },
        Err(e) => Startup::Held(format!(
            "{what} in slot {slot:?} did not come up: {e}; the process is left as it is"
        )),
    }
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;
