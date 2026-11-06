//! `kbf-host-install unpin`: undo `activate --pin`, root's, on the host itself.
//!
//! ```text
//! kbf-host-install unpin
//! ```
//!
//! It takes the commit the hold file names out of `retain.json`'s `pinned` and removes the hold
//! file ([`super::core::Ab::unpin`]), so the agent follows the coordinator again from its next
//! heartbeat, and a prune may remove the release again. It takes no argument. The answer is a
//! JSON line on stdout, `{"result":"unpinned","commit":C|null}` (`null`: nothing was pinned, or
//! the hold file named no commit root wrote), or a refusal or failure as `activate` gives them;
//! the exit status is 0 (done), 2 (refused: nothing changed) or 3 (failed). The command is
//! audited (verb `unpin`), and is not run when the audit log cannot be opened.

use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(target_os = "linux")]
use super::activate_cli::LINUX_HOLD;
#[cfg(target_os = "macos")]
use super::activate_cli::MAC_HOLD;
use super::activate_cli::{Asked, Place, answer, new_ab};
use super::answer::Answer;
use super::backend::Backend;
use crate::mac::layout::MacHost;
use crate::runner::Runner;
use crate::trust::Trust;

pub const USAGE: &str = "usage: kbf-host-install unpin\n";

/// The program: `unpin` with the arguments after it, as root, on this platform.
pub fn main(args: &[String]) -> ExitCode {
    main_with(args, rustix::process::geteuid().as_raw(), &production)
}

/// [`main`] with the effective user and the platform's run put in by the caller.
pub(super) fn main_with(args: &[String], euid: u32, run: &dyn Fn() -> u8) -> ExitCode {
    if let Some(arg) = args.first() {
        eprintln!("kbf-host-install: unknown argument {arg:?}\n{USAGE}");
        return ExitCode::from(2);
    }
    if euid != 0 {
        eprintln!("kbf-host-install: unpin is root's: run it as root (sudo)");
        return ExitCode::from(2);
    }
    ExitCode::from(run())
}

/// Answer with `unpin`, audit it as the verb `unpin`, and print the answer: the exit status.
pub(super) fn run(
    unpin: &dyn Fn() -> Answer,
    place: &Place<'_>,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> u8 {
    let asked = Asked {
        verb: "unpin",
        version: None,
    };
    answer(&asked, unpin, place, out, err)
}

/// A Linux host: unpin on the `kbf-installer` layout that `backend` makes, the state in
/// `state_dir` (made private if it is not there), the hold file at `hold`.
// Called by the Linux build only; the Mac build compiles it and its tests run it everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn on_linux<B: Backend>(
    backend: impl FnOnce() -> B,
    env: &super::production::LinuxEnv,
    trust: &Trust,
    hold: PathBuf,
) -> u8 {
    if let Err(why) = super::production::private_dir(trust, &env.state_dir) {
        eprintln!("kbf-host-install: {why}");
        return 3;
    }
    let ab = new_ab(backend(), env.state_dir.join(&env.state_name), trust, hold);
    let place = Place {
        trust,
        audit: &env.audit,
    };
    run(
        &|| ab.unpin(),
        &place,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

/// A Mac: unpin on the kit of `host`, whose tools `runner` runs, the state in its work
/// directory, the hold file at `hold`.
// Called by the Mac build only; the Linux build compiles it and its tests run it everywhere.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) fn on_mac(host: MacHost, runner: Box<dyn Runner>, hold: PathBuf) -> u8 {
    if crate::mac::ensure_work(&host).is_err() {
        eprintln!("kbf-host-install: the work directory is not usable");
        return 3;
    }
    let (trust, state, audit) = (
        host.trust.clone(),
        host.work.join("state.json"),
        host.audit.clone(),
    );
    let backend = super::mac_backend::MacBackend::new(host, runner);
    let ab = new_ab(backend, state, &trust, hold);
    let place = Place {
        trust: &trust,
        audit: &audit,
    };
    run(
        &|| ab.unpin(),
        &place,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

#[cfg(target_os = "linux")]
fn production() -> u8 {
    on_linux(
        || super::linux::production(Vec::new()),
        &super::production::LinuxEnv::production(),
        &Trust::root(),
        PathBuf::from(LINUX_HOLD),
    )
}

#[cfg(target_os = "macos")]
fn production() -> u8 {
    on_mac(
        MacHost::production(),
        Box::new(crate::runner::SystemRunner),
        PathBuf::from(MAC_HOLD),
    )
}
