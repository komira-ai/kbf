//! The rules for the two units of the frontend proxy, `kbf-proxy.socket` and
//! `kbf-proxy.service` (docs/design/frontend-proxy.md, section 2; kbf-proxy-api.md, section
//! 11). The generic rules of the parent module (placeholder, install, network, exec, reload,
//! gate-file) apply to the service too; these are the ones that belong to a socket-activated
//! service that balances over frontends that run as units of their own and supervises none.

use kbf_unitfile::UnitFile;

use super::{INSTALLER, INSTALLER_CONFIG, all, read, rules_of, trim, violations};

/// The rules of this module, by the tag a violation starts with.
pub(super) const RULES: [&str; 13] = [
    "proxy-socket-pair",
    "proxy-socket-accept",
    "proxy-socket-port",
    "proxy-exec",
    "proxy-kill",
    "proxy-user",
    "proxy-env",
    "proxy-harden",
    "proxy-reload",
    "proxy-restart",
    "proxy-state",
    "proxy-stop",
    "proxy-type",
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
    // Catches a public port that is not the shipped 8980 (a port in the privileged range is
    // out too: the proxy is not root; a drop-in `listen.conf` is how a host differs), a
    // second listener (the verb takes exactly one descriptor) and anything that is not a
    // TCP stream.
    let listens: Vec<&str> = all(unit, "Socket", "ListenStream");
    if listens != ["8980"] {
        v(
            "proxy-socket-port",
            format!("ListenStream= is {listens:?}, not 8980 alone"),
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

/// `kbf-proxy.service`: the proxy, the load balancer over the frontends of its pool.
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
    // Catches a stop that leaves part of the unit running: the proxy starts no process of its
    // own, so the default KillMode (the control group) ends the unit whole, and `process` or
    // `none` would leave whatever is left in the group behind after the proxy is gone.
    if !matches!(get("KillMode"), None | Some("control-group" | "mixed")) {
        v(
            "proxy-kill",
            format!(
                "KillMode= is {:?}, not control-group, mixed or unset",
                get("KillMode")
            ),
        );
    }
    // Catches a certificate renewal that cannot reach the proxy without ending its streams:
    // no reload at all (the renew hook would restart), a reload that restarts (the old
    // proxy's streams are cut), or one that signals anything but the main process with HUP
    // (TERM drains and stops it; without $MAINPID the command names no process).
    let reloads = all(unit, "Service", "ExecReload");
    if reloads != ["/bin/kill -HUP $MAINPID"] {
        v(
            "proxy-reload",
            format!("ExecReload= is {reloads:?}, not /bin/kill -HUP $MAINPID alone"),
        );
    }
    // Catches a notify unit: the verb never sends READY=1, so systemd would wait out the
    // start timeout and kill a proxy that is serving, and the socket's backlog would drain
    // into a restart loop. Also `exec`/`forking`, which would change what systemd waits for.
    if get("Type") != Some("simple") {
        v(
            "proxy-type",
            format!("Type= is {:?}, not simple", get("Type")),
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
    // Catches the proxy's secret lost at a restart: the runtime directory must outlive the
    // service (RuntimeDirectoryPreserve=yes) or the frontends, which keep running, would read
    // every call as anonymous.
    if get("RuntimeDirectory") != Some("kbf-proxy")
        || get("RuntimeDirectoryPreserve") != Some("yes")
    {
        v(
            "proxy-state",
            "no RuntimeDirectory=kbf-proxy with RuntimeDirectoryPreserve=yes".into(),
        );
    }
    // Catches a stop that never ends (the proxy bounds itself to 10 s) and one that is not
    // SIGTERM, which is the signal the verb drains on.
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
const BROKEN_UNITS: &[(&str, &str, &str, &str)] = &[
    (
        "kbf-proxy.socket",
        "[Install]\n",
        "[Socket]\nService=kbf-frontend.service\n[Install]\n",
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
        "ListenStream=8980\n",
        "ListenStream=443\n",
        "proxy-socket-port",
    ),
    (
        "kbf-proxy.socket",
        "ListenStream=8980\n",
        "ListenStream=8980\nListenStream=8981\n",
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
        "\nKillSignal=SIGTERM\n",
        "\nKillSignal=SIGTERM\nKillMode=process\n",
        "proxy-kill",
    ),
    (
        "kbf-proxy.socket",
        "ListenStream=8980\n",
        "ListenStream=8443\n",
        "proxy-socket-port",
    ),
    (
        "kbf-proxy.service",
        "\nType=simple\n",
        "\nType=notify\n",
        "proxy-type",
    ),
    (
        "kbf-proxy.service",
        "RuntimeDirectory=kbf-proxy\n",
        "RuntimeDirectory=kbf\n",
        "proxy-state",
    ),
    (
        "kbf-proxy.service",
        "\nUser=kbf-server\n",
        "\nUser=root\n",
        "proxy-user",
    ),
    (
        "kbf-proxy.service",
        "\nUser=kbf-server\n",
        "\nUser=kbf-server\nEnvironment=LISTEN_FDS=1\n",
        "proxy-env",
    ),
    (
        "kbf-proxy.service",
        "ExecReload=/bin/kill -HUP $MAINPID\n",
        "",
        "proxy-reload",
    ),
    (
        "kbf-proxy.service",
        "ExecReload=/bin/kill -HUP $MAINPID\n",
        "ExecReload=/bin/systemctl restart kbf-proxy\n",
        "proxy-reload",
    ),
    (
        "kbf-proxy.service",
        "ExecReload=/bin/kill -HUP $MAINPID\n",
        "ExecReload=/bin/kill -TERM $MAINPID\n",
        "proxy-reload",
    ),
    (
        "kbf-proxy.service",
        "ExecReload=/bin/kill -HUP $MAINPID\n",
        "ExecReload=/bin/kill -HUP\n",
        "proxy-reload",
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

/// Catches a proxy rule that would pass a unit with its defect: each broken unit is reported by
/// the rule it names.
#[test]
fn each_broken_proxy_unit_is_reported_by_its_rule() {
    for (file, from, to, rule) in BROKEN_UNITS {
        let text = read(file);
        assert!(
            text.contains(from),
            "{file}: the broken unit's `{from}` is not in the file"
        );
        let broken = text.replacen(from, to, 1);
        assert_ne!(broken, text);
        let found = violations(file, &broken);
        assert!(
            rules_of(&found).contains(rule),
            "{file}: broken unit `{from}` -> `{to}` was not reported by rule {rule}: {found:?}"
        );
    }
}

/// Catches a proxy rule with no broken unit, and a broken unit for a rule that does not exist.
#[test]
fn every_proxy_rule_has_a_broken_unit() {
    for rule in RULES {
        assert!(
            BROKEN_UNITS.iter().any(|m| m.3 == rule),
            "rule {rule} has no broken unit"
        );
    }
    for m in BROKEN_UNITS {
        assert!(RULES.contains(&m.3), "broken unit names no rule: {}", m.3);
    }
}
