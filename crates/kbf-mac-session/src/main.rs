//! `kbf-mac-session`: see the library's documentation.

use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser as _;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if let Some((mode, rest)) = args.split_first() {
        // The verbs a node's administrator runs through sudo; never a flag of the
        // helper itself.
        if mode == kbf_mac_session::node_admin::MODE {
            return node_admin(rest);
        }
        // The mode `run` in a GUI session executes this binary in (`macos::gui`), as
        // root under `launchctl asuser`.
        #[cfg(target_os = "macos")]
        if mode == kbf_mac_session::macos::gui::AS_USER {
            return kbf_mac_session::macos::gui::as_user(rest);
        }
    }
    kbf_mac_session::start::main(&kbf_mac_session::start::Args::parse())
}

#[cfg(target_os = "macos")]
fn node_admin(args: &[OsString]) -> ExitCode {
    kbf_mac_session::macos::admin::main(args)
}

#[cfg(not(target_os = "macos"))]
fn node_admin(_args: &[OsString]) -> ExitCode {
    eprintln!("REFUSED: kbf-mac-session node-admin runs on a Mac");
    ExitCode::from(2)
}
