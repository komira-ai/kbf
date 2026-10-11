//! `--driver native-whole-machine`: the bare-metal whole-machine runtime
//! (`kbf_driver_native::whole_machine`), which runs each `whole_machine` lease as its
//! own user in that user's GUI session through `kbf-mac-session`. macOS only, and only
//! where the helper serves its socket (`--mac-session-socket`): a node without the
//! helper does not start with this driver, so it never lists `native-whole-machine`
//! and is never sent a `whole_machine` lease. The runtime keeps its lease record and
//! lease directories in `<--scratch>/whole-machine`, and its phase goes into every
//! `Hello`.

use super::{Cli, Error, Node};

#[cfg(target_os = "macos")]
pub(super) fn add(cli: &Cli, node: &mut Node) -> Result<(), Error> {
    use std::sync::Arc;

    use kbf_driver_native::whole_machine::{
        HelperSession, WholeMachineConfig, WholeMachineRuntime,
    };

    let socket = &cli.mac_session_socket;
    if !socket.exists() {
        return Err(format!(
            "--driver native-whole-machine needs kbf-mac-session, which serves {}: nothing \
             is there",
            socket.display()
        )
        .into());
    }
    let mut config = WholeMachineConfig::new(super::scratch(cli)?.join("whole-machine"));
    config.outputs = cli.outputs;
    let session = HelperSession::new(socket, cli.daemon_user.clone());
    let runtime = WholeMachineRuntime::new(config, session, Arc::new(super::cas_client(cli)?))?;
    node.phase = Some(runtime.phase());
    node.add(Arc::new(runtime))
}

#[cfg(not(target_os = "macos"))]
pub(super) fn add(_cli: &Cli, _node: &mut Node) -> Result<(), Error> {
    Err("the whole-machine driver runs on macOS only".into())
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::super::{Cli, Driver, node};

    fn parse(extra: &[&str]) -> Cli {
        let base = [
            "kbf-daemon",
            "--server=https://front:7070",
            "--ca-cert=ca.pem",
            "--cert=c.pem",
            "--key=c.key",
            "--node-id=mac-1",
        ];
        Cli::try_parse_from(base.iter().chain(extra)).expect("flags")
    }

    /// Catches the whole-machine driver's flags not reaching the command line, and its
    /// defaults moved off the helper's (`kbf-mac-session --socket`, the `_kbf` user the
    /// provisioning makes).
    #[test]
    fn the_whole_machine_flags_parse() {
        let cli = parse(&["--driver=native-whole-machine"]);
        assert_eq!(cli.drivers, [Driver::NativeWholeMachine]);
        assert_eq!(
            cli.mac_session_socket,
            std::path::PathBuf::from("/var/run/kbf-mac-session/socket")
        );
        assert_eq!(cli.daemon_user, "_kbf");
        let set = parse(&[
            "--driver=native-whole-machine",
            "--mac-session-socket=/tmp/s",
            "--daemon-user=builder",
        ]);
        assert_eq!(set.mac_session_socket, std::path::PathBuf::from("/tmp/s"));
        assert_eq!(set.daemon_user, "builder");
    }

    /// Catches a node that starts with the whole-machine driver where there is no Mac
    /// session to switch (it would list `native-whole-machine` and be sent leases it
    /// cannot run).
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_whole_machine_driver_is_refused_off_macos() {
        let cli = parse(&["--driver=native-whole-machine", "--scratch=/s"]);
        let error = node(&cli, || Err("unused".into()), std::path::Path::new("/x"))
            .err()
            .expect("started");
        assert!(error.to_string().contains("macOS only"), "{error}");
    }

    /// Catches a node that starts with the whole-machine driver while the helper is not
    /// serving: it would list `native-whole-machine` with no one to make lease users.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_whole_machine_driver_needs_the_helper() {
        let cli = parse(&[
            "--driver=native-whole-machine",
            "--scratch=/s",
            "--mac-session-socket=/nonexistent/kbf-mac-session/socket",
        ]);
        let error = node(&cli, || Err("unused".into()), std::path::Path::new("/x"))
            .err()
            .expect("started");
        assert!(
            error.to_string().contains("needs kbf-mac-session"),
            "{error}"
        );
    }
}
