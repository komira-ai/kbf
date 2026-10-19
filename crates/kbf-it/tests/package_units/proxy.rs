//! The rules for the two units of the frontend proxy, `kbf-proxy.socket` and
//! `kbf-proxy.service` (docs/design/frontend-proxy.md, section 2; kbf-proxy-api.md, section
//! 11). The generic rules of the parent module (placeholder, install, network, exec, reload,
//! gate-file) apply to the service too; these are the ones that belong to a socket-activated
//! service that supervises children it leaves running. Each rule has a planted defect in
//! `MUTANTS`, and `every_proxy_rule_has_a_mutant` fails when one has none.

use kbf_deploy::unit::UnitFile;

use super::{INSTALLER, INSTALLER_CONFIG, all, read, rules_of, trim, violations};

/// The rules of this module, by the tag a violation starts with.
pub(super) const RULES: [&str; 11] = [
    "proxy-socket-pair",
    "proxy-socket-accept",
    "proxy-socket-port",
    "proxy-exec",
    "proxy-kill",
    "proxy-user",
    "proxy-env",
    "proxy-harden",
    "proxy-restart",
    "proxy-state",
    "proxy-stop",
];

/// `kbf-proxy.socket`: the unit that holds the public port.
pub(super) fn socket_rules(unit: &UnitFile, v: &mut impl FnMut(&str, String)) {
    let get = |s: &str, k: &str| unit.get_in(s, k).map(trim);
    // Catches a socket that starts another service than the proxy (a connection would
    // reach a process that does not speak the proxy's activation), and one that is not
    // wanted by sockets.target (it would not listen after a reboot).
    if let Some(other) = get("Socket", "Service")
        && other != "kbf-proxy.service"
    {
        v("proxy-socket-pair", format!("Service={other}"));
    }
    if !all(unit, "Install", "WantedBy").contains(&"sockets.target") {
        v("proxy-socket-pair", "no WantedBy=sockets.target".into());
    }
    // Catches a per-connection service: with Accept=yes systemd starts an instance for
    // every connection and passes the accepted socket, which the proxy cannot adopt (it
    // wants a listening socket and LISTEN_FDS=1).
    if get("Socket", "Accept") != Some("no") {
        v(
            "proxy-socket-accept",
            format!("Accept= is {:?}, not no", get("Socket", "Accept")),
        );
    }
    // Catches a public port in the privileged range (the proxy is not root and the design
    // says above 1024), a second listener (the verb takes exactly one descriptor) and
    // anything that is not a TCP stream.
    let listens: Vec<&str> = all(unit, "Socket", "ListenStream");
    let port_ok = |l: &str| {
        l.rsplit(':')
            .next()
            .and_then(|p| p.parse::<u32>().ok())
            .is_some_and(|p| (1025..=65_535).contains(&p))
    };
    if listens.len() != 1 || !port_ok(listens[0]) {
        v(
            "proxy-socket-port",
            format!("ListenStream= is {listens:?}, not one port above 1024"),
        );
    }
    for other in [
        "ListenDatagram",
        "ListenSequentialPacket",
        "ListenFIFO",
        "ListenSpecial",
        "ListenNetlink",
        "ListenMessageQueue",
    ] {
        if get("Socket", other).is_some() {
            v("proxy-socket-port", format!("{other}= is set"));
        }
    }
}

/// `kbf-proxy.service`: the proxy and the supervisor of its children.
pub(super) fn service_rules(
    unit: &UnitFile,
    start: &[&str],
    pre: &[&str],
    v: &mut impl FnMut(&str, String),
) {
    let get = |k: &str| unit.get_in("Service", k).map(trim);
    let unit_words = |k: &str| -> Vec<&str> {
        all(unit, "Unit", k)
            .into_iter()
            .flat_map(str::split_whitespace)
            .collect()
    };
    // Catches a service that does not need its socket: started alone it would have no
    // descriptor 3 (the verb refuses), and started before it, the port would not be held.
    for key in ["Requires", "After"] {
        if !unit_words(key).contains(&"kbf-proxy.socket") {
            v("proxy-socket-pair", format!("no {key}=kbf-proxy.socket"));
        }
    }
    // Catches a command line that is not the verb with the installer's file: another
    // verb, a second config, a flag the contract does not give it, a `--check` that is
    // not the gate in front of the start.
    let verb = [INSTALLER, "proxy", "--config", INSTALLER_CONFIG];
    if start != verb {
        v(
            "proxy-exec",
            format!("ExecStart= is {start:?}, not {verb:?}"),
        );
    }
    let check: Vec<&str> = verb.iter().copied().chain(["--check"]).collect();
    if pre != [check.join(" ")] {
        v(
            "proxy-exec",
            format!("ExecStartPre= is {pre:?}, not {check:?}"),
        );
    }
    // Catches a stop or restart that kills the children: with KillMode=control-group (the
    // default) or mixed, every frontend dies with the proxy and every build in flight
    // fails; the restart would find nothing to adopt.
    if get("KillMode") != Some("process") {
        v(
            "proxy-kill",
            format!("KillMode= is {:?}, not process", get("KillMode")),
        );
    }
    // Catches a proxy that runs as root (the front door parses bytes from the internet) or
    // as the user builds run as (which must not read what a frontend holds).
    if get("User") != Some("kbf-server") {
        v(
            "proxy-user",
            format!("User= is {:?}, not kbf-server", get("User")),
        );
    }
    // Catches configuration by environment: the table is `installer.toml`, passed by flag.
    // The service manager's own activation variables are not set in the unit.
    for d in unit.directives() {
        if d.section == "Service"
            && ["Environment", "EnvironmentFile", "PassEnvironment"].contains(&d.key.as_str())
        {
            v("proxy-env", format!("{}={}", d.key, d.value));
        }
    }
    // Catches a lost hardening or capacity key: new privileges, the OOM score that keeps
    // the front door alive longest, the descriptor limit (a connection and an upstream
    // stream each hold one), swap (it would slow every stream), the shared slice.
    for (k, want) in [
        ("NoNewPrivileges", "yes"),
        ("OOMScoreAdjust", "-900"),
        ("LimitNOFILE", "1048576"),
        ("MemorySwapMax", "0"),
        ("Slice", "kbf.slice"),
    ] {
        if get(k) != Some(want) {
            v("proxy-harden", format!("{k}= is {:?}, not {want}", get(k)));
        }
    }
    // Catches a proxy that stays down after a crash, one restarted on a table it refused
    // (exit 2), and a start limit that would give up on a crash loop.
    if get("Restart") != Some("on-failure") {
        v("proxy-restart", format!("Restart= is {:?}", get("Restart")));
    }
    if get("RestartPreventExitStatus") != Some("2") {
        v("proxy-restart", "no RestartPreventExitStatus=2".into());
    }
    if unit.get_in("Unit", "StartLimitIntervalSec").map(trim) != Some("0") {
        v(
            "proxy-restart",
            "no StartLimitIntervalSec=0 in [Unit]".into(),
        );
    }
    // Catches the proxy's directories missing, or its secret lost at a restart: the
    // runtime directory must outlive the service (RuntimeDirectoryPreserve=yes) or the
    // adopted children would read every call as anonymous.
    let dirs: Vec<&str> = all(unit, "Service", "StateDirectory")
        .into_iter()
        .flat_map(str::split_whitespace)
        .collect();
    if !dirs.contains(&"kbf-proxy") || !dirs.contains(&"kbf-server") {
        v(
            "proxy-state",
            format!("StateDirectory= is {dirs:?}, not kbf-proxy and kbf-server"),
        );
    }
    if get("RuntimeDirectory") != Some("kbf-proxy")
        || get("RuntimeDirectoryPreserve") != Some("yes")
    {
        v(
            "proxy-state",
            "no RuntimeDirectory=kbf-proxy with RuntimeDirectoryPreserve=yes".into(),
        );
    }
    // Catches a stop that never ends (the proxy bounds itself to 10 s; with KillMode=process
    // the SIGKILL after the timeout reaches only the proxy) and one that is not SIGTERM,
    // which is the signal the verb drains on.
    let timeout = get("TimeoutStopSec").and_then(|t| t.trim_end_matches('s').parse::<u64>().ok());
    if timeout.is_none_or(|t| !(10..=60).contains(&t)) {
        v(
            "proxy-stop",
            format!(
                "TimeoutStopSec= is {:?}, not 10 to 60 s",
                get("TimeoutStopSec")
            ),
        );
    }
    if !matches!(get("KillSignal"), Some("SIGTERM" | "TERM" | "15")) {
        v(
            "proxy-stop",
            format!("KillSignal= is {:?}", get("KillSignal")),
        );
    }
}

/// One defect per rule: `(file, from, to, rule)`, as in the parent module.
const MUTANTS: &[(&str, &str, &str, &str)] = &[
    (
        "kbf-proxy.socket",
        "[Install]\n",
        "[Socket]\nService=kbf-frontend@a.service\n[Install]\n",
        "proxy-socket-pair",
    ),
    (
        "kbf-proxy.service",
        "Requires=kbf-proxy.socket\n",
        "",
        "proxy-socket-pair",
    ),
    (
        "kbf-proxy.socket",
        "Accept=no\n",
        "Accept=yes\n",
        "proxy-socket-accept",
    ),
    (
        "kbf-proxy.socket",
        "ListenStream=8443\n",
        "ListenStream=443\n",
        "proxy-socket-port",
    ),
    (
        "kbf-proxy.socket",
        "ListenStream=8443\n",
        "ListenStream=8443\nListenStream=8444\n",
        "proxy-socket-port",
    ),
    (
        "kbf-proxy.service",
        "ExecStart=/usr/bin/kbf-installer proxy --config /etc/kbf/installer.toml\n",
        "ExecStart=/usr/bin/kbf-installer proxy --config /etc/kbf/installer.toml --listen 0.0.0.0:443\n",
        "proxy-exec",
    ),
    (
        "kbf-proxy.service",
        "ExecStartPre=/usr/bin/kbf-installer proxy --config /etc/kbf/installer.toml --check\n",
        "",
        "proxy-exec",
    ),
    (
        "kbf-proxy.service",
        "\nKillMode=process\n",
        "\n",
        "proxy-kill",
    ),
    (
        "kbf-proxy.service",
        "\nUser=kbf-server\n",
        "\nUser=root\n",
        "proxy-user",
    ),
    (
        "kbf-proxy.service",
        "\nStateDirectoryMode=0750\n",
        "\nStateDirectoryMode=0750\nEnvironment=LISTEN_FDS=1\n",
        "proxy-env",
    ),
    (
        "kbf-proxy.service",
        "NoNewPrivileges=yes\n",
        "",
        "proxy-harden",
    ),
    (
        "kbf-proxy.service",
        "OOMScoreAdjust=-900\n",
        "OOMScoreAdjust=0\n",
        "proxy-harden",
    ),
    (
        "kbf-proxy.service",
        "RestartPreventExitStatus=2\n",
        "",
        "proxy-restart",
    ),
    (
        "kbf-proxy.service",
        "RuntimeDirectoryPreserve=yes\n",
        "",
        "proxy-state",
    ),
    (
        "kbf-proxy.service",
        "StateDirectory=kbf-proxy kbf-server\n",
        "StateDirectory=kbf-proxy\n",
        "proxy-state",
    ),
    (
        "kbf-proxy.service",
        "TimeoutStopSec=15\n",
        "TimeoutStopSec=infinity\n",
        "proxy-stop",
    ),
    (
        "kbf-proxy.service",
        "KillSignal=SIGTERM\n",
        "KillSignal=SIGINT\n",
        "proxy-stop",
    ),
];

/// Catches a proxy rule that would pass a unit with its defect: each mutant is reported by
/// the rule it names.
#[test]
fn each_proxy_defect_is_reported_by_its_rule() {
    for (file, from, to, rule) in MUTANTS {
        let text = read(file);
        assert!(
            text.contains(from),
            "{file}: the mutant's `{from}` is not in the file"
        );
        let mutant = text.replacen(from, to, 1);
        assert_ne!(mutant, text);
        let found = violations(file, &mutant);
        assert!(
            rules_of(&found).contains(rule),
            "{file}: mutant `{from}` -> `{to}` was not reported by rule {rule}: {found:?}"
        );
    }
}

/// Catches a proxy rule with no mutant, and a mutant for a rule that does not exist.
#[test]
fn every_proxy_rule_has_a_mutant() {
    for rule in RULES {
        assert!(
            MUTANTS.iter().any(|m| m.3 == rule),
            "rule {rule} has no mutant"
        );
    }
    for m in MUTANTS {
        assert!(RULES.contains(&m.3), "mutant names no rule: {}", m.3);
    }
}

/// Catches the proxy leaving the units that must come after the installer's: the root half
/// reverts a release on trial, so it is ordered before every runtime unit, the proxy included.
#[test]
fn the_installer_is_ordered_before_the_proxy() {
    let installer = UnitFile::parse(&read("kbf-installer.service")).expect("parses");
    assert!(
        all(&installer, "Unit", "Before")
            .join(" ")
            .split_whitespace()
            .any(|u| u == "kbf-proxy.service")
    );
}
