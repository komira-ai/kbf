//! The frontend proxy's port layout is a contract with the installer: `kbf-installer deploy
//! --observe-base-port` computes a child's operator port from the proxy's `base_port` with
//! its own copy of the stride and the offset (`deploy::observe`), because the installer does
//! not link the proxy. This test links both and fails when they drift.
//!
//! Catches a proxy whose slot stride or operator offset changed while the installer's copy
//! did not (the deploy would read a port nothing listens on, or another process's).

use kbf_installer::deploy::observe::{Slot as Installer, operator_port};
use kbf_proxy::config::UpstreamTable;
use kbf_proxy::route::Target;
use kbf_proxy::slot::Slot as Proxy;

#[test]
fn the_installers_operator_ports_are_the_proxys() {
    assert_eq!(
        kbf_installer::deploy::observe::MAX_BASE_PORT,
        kbf_proxy::config::MAX_BASE_PORT
    );
    for base_port in [1024u16, 18000, 40000, kbf_proxy::config::MAX_BASE_PORT] {
        let table = UpstreamTable { base_port };
        for (ours, theirs) in [(Installer::A, Proxy::A), (Installer::B, Proxy::B)] {
            assert_eq!(
                operator_port(base_port, ours),
                table.port(theirs, Target::Operator),
                "base {base_port}, {ours}"
            );
        }
    }
}
