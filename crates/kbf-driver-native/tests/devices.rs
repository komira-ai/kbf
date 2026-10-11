//! On macOS, an action reaches none of the services that talk to an iOS device on the
//! node: CoreDevice (`devicectl`), `remoted` (RemoteXPC) and `usbmuxd`, with or without
//! the network (`kbf_driver_native::network::NO_DEVICE_RULES`). No lease has a device
//! yet, so any action that could reach them could install, launch or reboot apps on a
//! device another lease is to book (`docs/design/ios-devices.md`, section 6.1).
//!
//! Each refusal is checked against a control on the same host: the same action run by
//! the same driver without a sandbox must reach the service, or the test proves nothing.

#![cfg(target_os = "macos")]

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::Runtime;
use kbf_driver_native::NativeRuntime;
use kbf_driver_native::network::Isolation;
use kbf_proto::reapi::ActionResult;
use support::{MemoryCas, Spec, config, scratch, stderr, stdout, work};

/// The actions' `PATH`.
const PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// A sandboxed runtime and an unsandboxed one (the control), each with its own
/// scratch root under `name`.
fn runtimes(
    name: &str,
    cas: &Arc<MemoryCas>,
) -> (NativeRuntime<MemoryCas>, NativeRuntime<MemoryCas>) {
    let dir = scratch(name);
    let sandboxed = config(&dir);
    assert!(matches!(sandboxed.isolation, Isolation::Sandbox(_)));
    let mut open = config(&dir.join("unsandboxed"));
    open.isolation = Isolation::None;
    (
        support::runtime(sandboxed, cas),
        support::runtime(open, cas),
    )
}

/// Runs `spec` as lease `1.seq` with up to two minutes: `devicectl` refused its
/// service waits over 15 seconds for it before it gives up.
async fn run_long(
    rt: &NativeRuntime<MemoryCas>,
    cas: &MemoryCas,
    seq: u64,
    spec: &Spec,
) -> ActionResult {
    let action = spec.clone().timeout(Duration::from_secs(120)).store(cas);
    tokio::time::timeout(Duration::from_secs(150), rt.run(work(seq, action, 0)))
        .await
        .expect("the run ends")
        .expect("ran")
}

/// Catches a profile that lets an action reach `CoreDeviceService` (the CoreDevice
/// mach rule dropped, or naming another service): `devicectl list devices` must
/// fail, saying its connection to the service was invalidated (what the sandbox's
/// refused lookup looks like to it), with or without the network. Control: the same
/// action without a sandbox lists the devices (none on the runner) and exits 0.
#[tokio::test]
async fn no_action_reaches_coredevice() {
    let cas = Arc::new(MemoryCas::default());
    let (sandboxed, open) = runtimes("coredevice", &cas);
    let script = "xcrun devicectl list devices";
    let service = "com.apple.CoreDevice.CoreDeviceService";
    for (seq, network) in [(1, "off"), (2, "on")] {
        let spec = Spec::sh(script)
            .env("PATH", PATH)
            .property("network", network);
        let result = run_long(&sandboxed, &cas, seq, &spec).await;
        let (out, err) = (stdout(&cas, &result), stderr(&cas, &result));
        assert_ne!(
            result.exit_code, 0,
            "network={network}: devicectl ran: {out}{err}"
        );
        assert!(
            err.contains(service) && err.contains("The connection was invalidated"),
            "network={network}: devicectl failed for another reason: {out}{err}"
        );
    }

    let spec = Spec::sh(script).env("PATH", PATH);
    let result = run_long(&open, &cas, 10, &spec).await;
    assert_eq!(
        result.exit_code,
        0,
        "control: devicectl fails even unsandboxed: {}{}",
        stdout(&cas, &result),
        stderr(&cas, &result)
    );
}

/// Catches a profile that lets an action reach `remoted` (the `remoted` rule dropped,
/// or naming another service): `remotectl dumpstate` must print no device, the
/// local one included, with or without the network. Control: the same action
/// without a sandbox prints the local device. The sandbox without the rule does not
/// hide it: a sandboxed action then prints it too.
#[tokio::test]
async fn no_action_reaches_remoted() {
    let cas = Arc::new(MemoryCas::default());
    let (sandboxed, open) = runtimes("remoted", &cas);
    let script = "/usr/libexec/remotectl dumpstate";
    let local = "Local device";
    for (seq, network) in [(1, "off"), (2, "on")] {
        let spec = Spec::sh(script)
            .env("PATH", PATH)
            .property("network", network);
        let result = run_long(&sandboxed, &cas, seq, &spec).await;
        let (out, err) = (stdout(&cas, &result), stderr(&cas, &result));
        assert!(
            !out.contains(local),
            "network={network}: remotectl reached remoted: {out}{err}"
        );
    }

    let spec = Spec::sh(script).env("PATH", PATH);
    let result = run_long(&open, &cas, 10, &spec).await;
    let out = stdout(&cas, &result);
    assert!(
        out.contains(local),
        "control: remotectl does not reach remoted even unsandboxed: {out}{}",
        stderr(&cas, &result)
    );
}

/// Perl that connects to `usbmuxd`'s socket by the path clients use (a link into
/// `/private/var/run`) and prints `connected` or the error.
const CONNECT: &str = r#"
use Socket;
socket(my $s, PF_UNIX, SOCK_STREAM, 0) or do { print "socket: $!\n"; exit 0 };
if (connect($s, sockaddr_un("/var/run/usbmuxd"))) { print "connected\n" } else { print "$!\n" }
"#;

/// Catches a profile that lets an action connect to `usbmuxd` (the socket rule
/// dropped, or naming the socket by `/var/run`, the link's path, which the sandbox
/// never sees): the connect must be refused with EPERM, with or without the network
/// (the no-network profile allows every other Unix socket).
/// Control: the same action without a sandbox connects.
#[tokio::test]
async fn no_action_reaches_usbmuxd() {
    let cas = Arc::new(MemoryCas::default());
    let (sandboxed, open) = runtimes("usbmuxd", &cas);
    let spec = Spec::argv(&["/usr/bin/perl", "-e", CONNECT]);
    for (seq, network) in [(1, "off"), (2, "on")] {
        let spec = spec.clone().property("network", network);
        let result = run_long(&sandboxed, &cas, seq, &spec).await;
        assert_eq!(
            stdout(&cas, &result).trim(),
            "Operation not permitted",
            "network={network}: {}",
            stderr(&cas, &result)
        );
    }

    let result = run_long(&open, &cas, 10, &spec).await;
    assert_eq!(
        stdout(&cas, &result).trim(),
        "connected",
        "control: {}",
        stderr(&cas, &result)
    );
}
