//! `--driver native-whole-machine`: the bare-metal whole-machine runtime
//! (`kbf_driver_native::whole_machine`), which runs each `whole_machine` lease as its
//! own user in that user's GUI session through `kbf-mac-session`. macOS only, and only
//! where the helper serves its socket (`--mac-session-socket`): a node without the
//! helper does not start with this driver, so it never lists `native-whole-machine`
//! and is never sent a `whole_machine` lease. The runtime keeps its lease record and
//! lease directories in `<--data-dir>/leases/whole-machine`, and its phase goes into every
//! `Hello`.

use super::{Cli, Error, Node};

#[cfg(target_os = "macos")]
pub(super) fn add<C: kbf_transfer::Cas>(cli: &Cli, node: &mut Node<C>) -> Result<(), Error> {
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
    config.upload_deadline = super::upload_deadline(cli);
    let session = HelperSession::new(socket, cli.daemon_user.clone());
    let runtime = WholeMachineRuntime::new(config, session, node.plane(cli)?)?;
    node.phase = Some(runtime.phase());
    node.add(Arc::new(runtime))
}

#[cfg(not(target_os = "macos"))]
pub(super) fn add<C>(_cli: &Cli, _node: &mut Node<C>) -> Result<(), Error> {
    Err("the whole-machine driver runs on macOS only".into())
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::super::{Cli, Driver};

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
    /// cannot run). The command line is a Mac's: the native driver beside it.
    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn the_whole_machine_driver_is_refused_off_macos() {
        use super::super::tests::{scratch, tls_files};
        use super::super::{native_config, native_with};

        let dir = scratch("whole-machine-off-macos");
        tls_files(&dir);
        let flag = |name: &str, path: &std::path::Path| format!("--{name}={}", path.display());
        let flags = [
            "kbf-daemon".to_owned(),
            "--server=https://127.0.0.1:1".to_owned(),
            flag("ca-cert", &dir.join("ca.pem")),
            flag("cert", &dir.join("node.pem")),
            flag("key", &dir.join("node.key")),
            "--node-id=mac-1".to_owned(),
            "--cas=https://127.0.0.1:1".to_owned(),
            flag("data-dir", &dir),
            flag("xcode-apps", &dir.join("Applications")),
            "--driver=native".to_owned(),
            "--driver=native-whole-machine".to_owned(),
        ];
        let cli = Cli::try_parse_from(flags).expect("flags");
        let config = native_config(&cli).expect("the native configuration");
        let error = native_with(&cli, config, &dir.join("no-xcrun"))
            .err()
            .expect("started");
        assert!(error.to_string().contains("macOS only"), "{error}");
        // The native runtime's Xcode survey may still be writing under it: not removed
        // here (the next run of this test binary's pid would).
        drop(dir);
    }

    /// Catches a node that starts with the whole-machine driver while the helper is not
    /// serving: it would list `native-whole-machine` with no one to make lease users.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_whole_machine_driver_needs_the_helper() {
        use super::super::node;
        use super::super::tests::scratch;

        let data = scratch("whole-machine-needs-helper");
        let cli = parse(&[
            "--driver=native-whole-machine",
            &format!("--data-dir={}", data.display()),
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
